use crate::backend::{self, Backend, EntryMetadata, RootAnchor};
use crate::types::{Entry, NodeId, NodeKind, ScanEvent, Sizes};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

const EVENT_CAPACITY: usize = 128;
const SEND_INTERVAL: Duration = Duration::from_millis(20);
// Approximate fairness only: bound selection work and amortize file-only reports.
const EFFORT_SAMPLE: usize = 8;
const EFFORT_REPORT_ENTRIES: usize = 1024;
const ACTIVE_EFFORT_RESERVATION: u64 = 128;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    pub batch_size: usize,
    pub buffer_size: usize,
    pub backend: Backend,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            threads: thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1)
                .clamp(1, 16),
            batch_size: 128,
            buffer_size: 256 * 1024,
            backend: Backend::Auto,
        }
    }
}

pub struct ScanHandle {
    pub receiver: Receiver<ScanEvent>,
    pub cancel: Arc<AtomicBool>,
    pub finished: Arc<AtomicBool>,
    pub anchor: Arc<OnceLock<Arc<RootAnchor>>>,
    control: Arc<ScanControl>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.control.cancel_active();
        self.control.wake();
    }

    /// Suspend new scan I/O while foreground deletion owns filesystem priority.
    /// Already started I/O and its bounded name batch may still finish.
    pub fn set_paused(&self, paused: bool) {
        self.control.set_paused(paused);
    }

    /// Stop this path and all its descendants without interrupting other branches.
    /// Exclusions remain in effect for this handle, including tasks not yet started.
    pub fn exclude(&self, relative: &Path) {
        // The original handle stays alive for anchoring after its producer exits.
        // Completed scans cannot admit more work or need new exclusion paths.
        if !self.is_finished() {
            self.control.exclude(relative);
        }
    }

    /// Prefer this discovered directory and its descendants on the next free
    /// worker. The newest focus replaces the previous request.
    pub fn prioritize(&self, relative: &Path) {
        self.control.prioritize(relative);
    }

    /// Restore normal effort-based scheduling after leaving a focused directory.
    pub fn clear_priority(&self) {
        self.control.clear_priority();
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct FinishedGuard {
    finished: Arc<AtomicBool>,
    control: Arc<ScanControl>,
}

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.control.finish();
        self.finished.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct ScanControl {
    paused: AtomicBool,
    pause_lock: Mutex<()>,
    pause_wake: Condvar,
    state: RwLock<ControlState>,
    has_exclusions: AtomicBool,
    focus_request: Mutex<Option<FocusRequest>>,
    wake_sender: OnceLock<Sender<()>>,
}

#[derive(Debug, PartialEq, Eq)]
enum FocusRequest {
    Prioritize(PathBuf),
    Clear,
}

#[derive(Default)]
struct ControlState {
    excluded: ExclusionTrie,
    active: HashMap<NodeId, ActiveDirectory>,
}

struct ActiveDirectory {
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}

struct ActiveTask<'a> {
    control: &'a ScanControl,
    id: NodeId,
    cancel: Arc<AtomicBool>,
}

impl Drop for ActiveTask<'_> {
    fn drop(&mut self) {
        self.control
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active
            .remove(&self.id);
    }
}

#[derive(Default)]
struct ExclusionTrie {
    terminal: bool,
    children: BTreeMap<std::ffi::OsString, ExclusionTrie>,
}

impl Drop for ExclusionTrie {
    fn drop(&mut self) {
        if self.children.is_empty() {
            return;
        }
        // Keep unvisited siblings in their iterators, using space only for depth.
        let mut pending = vec![std::mem::take(&mut self.children).into_values()];
        while let Some(children) = pending.last_mut() {
            if let Some(mut child) = children.next() {
                if !child.children.is_empty() {
                    pending.push(std::mem::take(&mut child.children).into_values());
                }
            } else {
                pending.pop();
            }
        }
    }
}

impl ExclusionTrie {
    fn excludes(&self, path: &Path) -> bool {
        self.excludes_components(path.components())
    }

    fn excludes_components<'a>(&self, components: impl Iterator<Item = Component<'a>>) -> bool {
        let mut node = self;
        for component in components.filter(|component| !matches!(component, Component::CurDir)) {
            if node.terminal {
                return true;
            }
            let Some(child) = node.children.get(component.as_os_str()) else {
                return false;
            };
            node = child;
        }
        node.terminal
    }

    fn insert(&mut self, path: &Path) {
        let mut node = self;
        for component in path.components() {
            if node.terminal {
                return;
            }
            node = node
                .children
                .entry(component.as_os_str().to_owned())
                .or_default();
        }
        // Retain superseded descendants: dropping a wide subtree here would make
        // confirming an ancestor perform an unbounded walk on the input thread.
        node.terminal = true;
    }
}

impl ControlState {
    fn excludes(&self, path: &Path) -> bool {
        self.excluded.excludes(path)
    }
}

impl ScanControl {
    fn finish(&self) {
        let excluded = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // A terminal root closes admission under the same lock used by
            // exclude(), including calls that passed the handle's finished check.
            let excluded = std::mem::replace(
                &mut state.excluded,
                ExclusionTrie {
                    terminal: true,
                    children: BTreeMap::new(),
                },
            );
            self.has_exclusions.store(true, Ordering::Release);
            for active in state.active.values() {
                active.cancel.store(true, Ordering::Release);
            }
            excluded
        };
        self.wake();
        // The original handle stays alive for its anchor. Release its obsolete
        // paths here on the producer thread, outside the shared state lock.
        drop(excluded);
    }

    fn set_paused(&self, paused: bool) {
        if self.paused.swap(paused, Ordering::AcqRel) != paused {
            self.wake();
        }
    }

    fn wait_unpaused(
        &self,
        cancel: &AtomicBool,
        task_cancel: Option<&AtomicBool>,
    ) -> io::Result<()> {
        let cancelled = || {
            cancel.load(Ordering::Acquire)
                || task_cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire))
        };
        if self.paused.load(Ordering::Acquire) {
            let mut guard = self
                .pause_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while self.paused.load(Ordering::Acquire) && !cancelled() {
                // Cancellation flags can also be set by channel disconnects.
                // A timed wait covers those paths and a notification racing sleep.
                guard = self
                    .pause_wake
                    .wait_timeout(guard, SEND_INTERVAL)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
        }
        if cancelled() {
            Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
        } else {
            Ok(())
        }
    }

    fn prioritize(&self, relative: &Path) {
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return;
        }
        let relative = relative
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect();
        *self
            .focus_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(FocusRequest::Prioritize(relative));
        self.wake();
    }

    fn clear_priority(&self) {
        *self
            .focus_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(FocusRequest::Clear);
        self.wake();
    }

    fn take_focus(&self) -> Option<FocusRequest> {
        self.focus_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn wake(&self) {
        self.pause_wake.notify_all();
        if let Some(sender) = self.wake_sender.get() {
            let _ = sender.try_send(());
        }
    }

    fn exclude(&self, relative: &Path) {
        // Scanner task paths contain only root-relative normal components.
        // Reject malformed callers rather than accidentally excluding the root.
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return;
        }
        let relative: PathBuf = relative
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.excludes(&relative) {
            return;
        }
        for active in state.active.values() {
            if active.path.starts_with(&relative) {
                active.cancel.store(true, Ordering::Release);
            }
        }
        state.excluded.insert(&relative);
        self.has_exclusions.store(true, Ordering::Release);
        drop(state);
        self.wake();
    }

    fn excludes(&self, path: &Path) -> bool {
        if !self.has_exclusions.load(Ordering::Acquire) {
            return false;
        }
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .excludes(path)
    }

    fn excludes_child(&self, parent: &Path, name: &std::ffi::OsStr) -> bool {
        if !self.has_exclusions.load(Ordering::Acquire) {
            return false;
        }
        // Enumeration supplies a single non-dot basename, so no joined path is needed.
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .excluded
            .excludes_components(
                parent
                    .components()
                    .chain(std::iter::once(Component::Normal(name))),
            )
    }

    fn activate<'a>(&'a self, task: &DirectoryTask, cancel: &AtomicBool) -> ActiveTask<'a> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let task_cancel = Arc::new(AtomicBool::new(
            cancel.load(Ordering::Acquire) || state.excludes(&task.path),
        ));
        state.active.insert(
            task.id,
            ActiveDirectory {
                path: task.path.clone(),
                cancel: Arc::clone(&task_cancel),
            },
        );
        ActiveTask {
            control: self,
            id: task.id,
            cancel: task_cancel,
        }
    }

    fn cancel_active(&self) {
        let state = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for active in state.active.values() {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

#[derive(Debug)]
struct DirectoryTask {
    id: NodeId,
    path: PathBuf,
}

struct ScheduledDirectory {
    pending_path: Option<PathBuf>,
    parent: Option<NodeId>,
    pending_children: Vec<NodeId>,
    // Index in the parent's pending_children, or usize::MAX when unavailable.
    pending_slot: usize,
    sample_cursor: usize,
    checked: u64,
    active: usize,
}

impl ScheduledDirectory {
    fn has_pending(&self) -> bool {
        self.pending_path.is_some() || !self.pending_children.is_empty()
    }

    fn effort(&self) -> u64 {
        self.checked
            .saturating_add((self.active as u64).saturating_mul(ACTIVE_EFFORT_RESERVATION))
    }
}

#[derive(Default)]
struct Scheduler {
    directories: HashMap<NodeId, ScheduledDirectory>,
    paths: HashMap<PathBuf, NodeId>,
    root: Option<NodeId>,
    focus: Option<PathBuf>,
}

impl Scheduler {
    fn insert(&mut self, task: DirectoryTask, parent: Option<NodeId>) {
        let id = task.id;
        self.paths.insert(task.path.clone(), id);
        self.directories.insert(
            id,
            ScheduledDirectory {
                pending_path: Some(task.path),
                parent,
                pending_children: Vec::new(),
                pending_slot: usize::MAX,
                sample_cursor: 0,
                checked: 0,
                active: 0,
            },
        );
        if parent.is_none() {
            self.root = Some(id);
        }
        // Most insertions stop at their immediate parent. Only a transition from
        // no pending work to some pending work needs to travel up the tree.
        let mut current = id;
        while let Some(parent) = self.directories[&current].parent {
            let directory = self
                .directories
                .get_mut(&parent)
                .expect("registered parent");
            let was_pending = directory.has_pending();
            let slot = directory.pending_children.len();
            directory.pending_children.push(current);
            self.directories
                .get_mut(&current)
                .expect("registered directory")
                .pending_slot = slot;
            if was_pending {
                break;
            }
            current = parent;
        }
    }

    fn pending_from(&mut self, mut current: NodeId) -> Option<NodeId> {
        loop {
            let directory = &self.directories[&current];
            if directory.pending_path.is_some() {
                return Some(current);
            }
            let children = &directory.pending_children;
            if children.is_empty() {
                return None;
            }
            let count = children.len().min(EFFORT_SAMPLE);
            let start = directory.sample_cursor % children.len();
            let mut selected = children[start];
            let mut effort = self.directories[&selected].effort();
            for offset in 1..count {
                let candidate = children[(start + offset) % children.len()];
                let candidate_effort = self.directories[&candidate].effort();
                if candidate_effort < effort {
                    selected = candidate;
                    effort = candidate_effort;
                }
            }
            // Rotate even when every sibling fits in the sample so equal-cost
            // branches share dispatches before their first progress report.
            let step = if count == children.len() { 1 } else { count };
            let cursor = (start + step) % children.len();
            self.directories
                .get_mut(&current)
                .expect("registered directory")
                .sample_cursor = cursor;
            current = selected;
        }
    }

    fn focused(&mut self) -> Option<NodeId> {
        let root = *self.paths.get(self.focus.as_ref()?)?;
        self.pending_from(root)
    }

    fn take(&mut self) -> Option<DirectoryTask> {
        let id = self.focused().or_else(|| self.pending_from(self.root?))?;
        let path = self.directories.get_mut(&id)?.pending_path.take()?;
        let mut current = id;
        loop {
            let directory = self
                .directories
                .get_mut(&current)
                .expect("registered directory");
            directory.active += 1;
            let Some(parent) = directory.parent else {
                break;
            };
            current = parent;
        }
        // Remove empty branches with constant-time swap removal, stopping as
        // soon as an ancestor still contains another pending task.
        current = id;
        while !self.directories[&current].has_pending() {
            let Some(parent) = self.directories[&current].parent else {
                break;
            };
            let slot = self.directories[&current].pending_slot;
            let directory = self
                .directories
                .get_mut(&parent)
                .expect("registered parent");
            debug_assert_eq!(directory.pending_children[slot], current);
            directory.pending_children.swap_remove(slot);
            let moved = directory.pending_children.get(slot).copied();
            self.directories
                .get_mut(&current)
                .expect("registered directory")
                .pending_slot = usize::MAX;
            if let Some(moved) = moved {
                self.directories
                    .get_mut(&moved)
                    .expect("registered sibling")
                    .pending_slot = slot;
            }
            current = parent;
        }
        Some(DirectoryTask { id, path })
    }

    fn checked(&mut self, mut id: NodeId, entries: usize) {
        if entries == 0 {
            return;
        }
        // Charge an entire report at once; never update tree ordering per file.
        loop {
            let directory = self.directories.get_mut(&id).expect("registered directory");
            directory.checked = directory.checked.saturating_add(entries as u64);
            let Some(parent) = directory.parent else {
                break;
            };
            id = parent;
        }
    }

    fn complete(&mut self, mut id: NodeId) {
        loop {
            let directory = self.directories.get_mut(&id).expect("active directory");
            directory.active -= 1;
            let Some(parent) = directory.parent else {
                break;
            };
            id = parent;
        }
    }

    fn active(&self) -> usize {
        self.root.map_or(0, |root| self.directories[&root].active)
    }

    fn is_empty(&self) -> bool {
        self.root.is_none_or(|root| {
            let root = &self.directories[&root];
            root.active == 0 && !root.has_pending()
        })
    }
}

enum WorkerMessage {
    Progress {
        parent: NodeId,
        tasks: Vec<DirectoryTask>,
        checked: usize,
    },
    Complete {
        id: NodeId,
        checked: usize,
    },
}

struct Emitter {
    sender: Sender<ScanEvent>,
    next_id: NodeId,
    // Existing directory metadata suffices even for same-device bind mounts.
    // Keep this per scan: a scoped refresh must be allowed to revisit its root.
    visited_directories: HashSet<(u64, u64)>,
}

impl Emitter {
    fn send(&self, mut event: ScanEvent, cancel: &AtomicBool, terminal: bool) -> io::Result<()> {
        loop {
            if !terminal && cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
            }
            match self.sender.send_timeout(event, SEND_INTERVAL) {
                Ok(()) => return Ok(()),
                Err(SendTimeoutError::Timeout(returned)) => event = returned,
                Err(SendTimeoutError::Disconnected(_)) => {
                    cancel.store(true, Ordering::Release);
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "scan event receiver disconnected",
                    ));
                }
            }
        }
    }

    fn entries(
        &mut self,
        task: &DirectoryTask,
        discovered: Vec<(Arc<OsStr>, io::Result<EntryMetadata>)>,
        cancel: &AtomicBool,
        control: &ScanControl,
    ) -> io::Result<Vec<DirectoryTask>> {
        let mut entries = Vec::with_capacity(discovered.len());
        let mut directories = Vec::new();
        let mut skipped_directories = Vec::new();
        for (name, metadata) in discovered {
            // Filter before allocating an ID. Already allocated IDs must still be
            // delivered, so queued batches cannot create holes in the model.
            if control.excludes_child(&task.path, &name) {
                continue;
            }
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
                io::Error::other("too many entries for the scan's node identifiers")
            })?;
            let (kind, sizes, hardlink_group, error) = match metadata {
                Ok(metadata) => {
                    // No additional I/O or per-file lookup. Device changes alone
                    // miss bind mounts and directory hardlinks on one filesystem.
                    let repeated_directory = metadata.kind == NodeKind::Directory
                        && metadata
                            .identity
                            .is_some_and(|identity| !self.visited_directories.insert(identity));
                    let group = (metadata.kind == NodeKind::File)
                        .then_some(metadata.identity)
                        .flatten()
                        .filter(|_| metadata.links != 0);
                    (
                        metadata.kind,
                        metadata.sizes,
                        group,
                        repeated_directory
                            .then(|| "directory already visited; contents skipped".to_owned()),
                    )
                }
                Err(error) => (
                    NodeKind::Unknown,
                    Sizes::default(),
                    None,
                    Some(error.to_string()),
                ),
            };
            if kind == NodeKind::Directory {
                if error.is_some() {
                    skipped_directories.push(id);
                } else {
                    directories.push(DirectoryTask {
                        id,
                        path: task.path.join(name.as_ref()),
                    });
                }
            }
            entries.push(Entry {
                id,
                name,
                kind,
                sizes,
                hardlink_group,
                error,
            });
        }
        // IDs and delivery share one lock, so the model receives append-only IDs in order.
        self.send(
            ScanEvent::Entries {
                parent: task.id,
                entries,
            },
            cancel,
            false,
        )?;
        // Preserve visible aliases and their partial-read errors, while releasing
        // every skipped directory's pending token after its entry is delivered.
        for id in skipped_directories {
            self.send(ScanEvent::DirectoryDone { id, error: None }, cancel, false)?;
        }
        Ok(directories)
    }
}

enum ScanRoot {
    Open(PathBuf),
    Anchored {
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
    },
}

/// Starts all filesystem work in the background. Dropping the handle never joins workers.
pub fn spawn(root: PathBuf, options: ScanOptions) -> ScanHandle {
    spawn_with_root(ScanRoot::Open(root), options)
}

/// Scan one directory beneath the retained original root. Events use local root
/// ID zero and contiguous local entry IDs; the model remaps each scope on intake.
pub fn spawn_subtree(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    options: ScanOptions,
) -> ScanHandle {
    spawn_with_root(ScanRoot::Anchored { anchor, relative }, options)
}

fn spawn_with_root(root: ScanRoot, mut options: ScanOptions) -> ScanHandle {
    options.threads = options.threads.clamp(1, 128);
    options.batch_size = options.batch_size.clamp(1, 4096);
    options.buffer_size = options.buffer_size.clamp(4096, 16 * 1024 * 1024);

    let (sender, receiver) = bounded(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let anchor = Arc::new(OnceLock::new());
    if let ScanRoot::Anchored {
        anchor: retained, ..
    } = &root
    {
        let _ = anchor.set(Arc::clone(retained));
    }
    let control = Arc::new(ScanControl::default());
    let thread_control = Arc::clone(&control);
    let thread_anchor = Arc::clone(&anchor);
    let thread_cancel = Arc::clone(&cancel);
    let thread_finished = Arc::clone(&finished);
    let failure_sender = sender.clone();
    let spawned = thread::Builder::new()
        .name("ldu-scan".into())
        .spawn(move || {
            let _finished = FinishedGuard {
                finished: thread_finished,
                control: Arc::clone(&thread_control),
            };
            run(
                root,
                options,
                sender,
                thread_cancel,
                thread_anchor,
                thread_control,
            );
        });
    if let Err(error) = spawned {
        let _ = failure_sender.try_send(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Unknown,
            sizes: Sizes::default(),
            error: Some(format!("could not start scanner: {error}")),
        });
        let _ = failure_sender.try_send(ScanEvent::DirectoryDone { id: 0, error: None });
        let _ = failure_sender.try_send(ScanEvent::Finished { cancelled: false });
        finished.store(true, Ordering::Release);
    }
    ScanHandle {
        receiver,
        cancel,
        finished,
        anchor,
        control,
    }
}

fn run(
    root: ScanRoot,
    options: ScanOptions,
    sender: Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
    anchor_slot: Arc<OnceLock<Arc<RootAnchor>>>,
    control: Arc<ScanControl>,
) {
    let emitter = Arc::new(Mutex::new(Emitter {
        sender,
        next_id: 1,
        visited_directories: HashSet::new(),
    }));

    if control.wait_unpaused(&cancel, None).is_ok() {
        let (relative, metadata) = match root {
            ScanRoot::Open(root) => {
                let metadata = RootAnchor::open_cancellable(&root, &cancel).and_then(|anchor| {
                    control.wait_unpaused(&cancel, None)?;
                    let metadata = anchor.metadata()?;
                    let _ = anchor_slot.set(Arc::new(anchor));
                    Ok(metadata)
                });
                (PathBuf::new(), metadata)
            }
            ScanRoot::Anchored { anchor, relative } => {
                let metadata = anchored_metadata(&anchor, &relative, &cancel, &control);
                (relative, metadata)
            }
        };
        let metadata = metadata.map(|metadata| {
            let file_identity = if metadata.kind == NodeKind::File {
                metadata.identity.filter(|_| metadata.links != 0)
            } else {
                None
            };
            (metadata, file_identity)
        });
        let directory_identity = metadata
            .as_ref()
            .ok()
            .filter(|(metadata, _)| metadata.kind == NodeKind::Directory)
            .and_then(|(metadata, _)| metadata.identity);
        let (kind, sizes, file_identity, mut error) = match metadata {
            Ok((metadata, file_identity)) => (metadata.kind, metadata.sizes, file_identity, None),
            Err(error) => (
                NodeKind::Unknown,
                Sizes::default(),
                None,
                Some(error.to_string()),
            ),
        };
        if control.excludes(&relative) {
            error = None;
        }
        let root_sent = {
            let mut emitter = emitter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(identity) = directory_identity {
                emitter.visited_directories.insert(identity);
            }
            emitter
                .send(
                    ScanEvent::RootInfo {
                        file_identity,
                        kind,
                        sizes,
                        error,
                    },
                    &cancel,
                    false,
                )
                .is_ok()
        };

        if root_sent && kind == NodeKind::Directory && !cancel.load(Ordering::Acquire) {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(options.threads)
                .thread_name(|index| format!("ldu-io-{index}"))
                .build();
            match pool {
                Ok(pool) => {
                    let anchor =
                        Arc::clone(anchor_slot.get().expect("a directory root has an anchor"));
                    run_scheduler(
                        pool,
                        anchor,
                        relative,
                        options,
                        Arc::clone(&emitter),
                        Arc::clone(&cancel),
                        Arc::clone(&control),
                    );
                }
                Err(error) => {
                    let _ = emitter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .send(
                            ScanEvent::DirectoryDone {
                                id: 0,
                                error: Some(format!("could not start I/O workers: {error}")),
                            },
                            &cancel,
                            false,
                        );
                }
            }
        } else if root_sent && !cancel.load(Ordering::Acquire) {
            let _ = emitter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(
                    ScanEvent::DirectoryDone { id: 0, error: None },
                    &cancel,
                    false,
                );
        }
    }

    // Terminal events ignore cancellation, but receiver drop still releases backpressure.
    let _ = emitter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(
            ScanEvent::Finished {
                cancelled: cancel.load(Ordering::Acquire),
            },
            &cancel,
            true,
        );
}

fn run_scheduler(
    pool: rayon::ThreadPool,
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    options: ScanOptions,
    emitter: Arc<Mutex<Emitter>>,
    cancel: Arc<AtomicBool>,
    control: Arc<ScanControl>,
) {
    let (messages, receiver) = bounded(options.threads.saturating_mul(2).max(1));
    let (wake_sender, wake_receiver) = bounded(1);
    let _ = control.wake_sender.set(wake_sender);
    let mut scheduler = Scheduler::default();
    scheduler.insert(
        DirectoryTask {
            id: 0,
            path: relative,
        },
        None,
    );
    while !scheduler.is_empty() && !cancel.load(Ordering::Acquire) {
        if let Some(request) = control.take_focus() {
            scheduler.focus = match request {
                FocusRequest::Prioritize(relative) => Some(relative),
                FocusRequest::Clear => None,
            };
        }
        while scheduler.active() < options.threads
            && !cancel.load(Ordering::Acquire)
            && !control.paused.load(Ordering::Acquire)
        {
            let Some(task) = scheduler.take() else { break };
            let worker_anchor = Arc::clone(&anchor);
            let worker_emitter = Arc::clone(&emitter);
            let worker_cancel = Arc::clone(&cancel);
            let worker_control = Arc::clone(&control);
            let worker_messages = messages.clone();
            let worker_options = options.clone();
            pool.spawn(move || {
                let id = task.id;
                let mut pending_checked = 0;
                scan_directory_emit(
                    &task,
                    &worker_anchor,
                    &worker_options,
                    &worker_emitter,
                    &worker_cancel,
                    &worker_control,
                    |tasks, checked| {
                        pending_checked += checked;
                        if tasks.is_empty() && pending_checked < EFFORT_REPORT_ENTRIES {
                            return Ok(());
                        }
                        send_worker(
                            &worker_messages,
                            WorkerMessage::Progress {
                                parent: id,
                                tasks,
                                checked: std::mem::take(&mut pending_checked),
                            },
                            &worker_cancel,
                        )
                    },
                );
                let _ = send_worker(
                    &worker_messages,
                    WorkerMessage::Complete {
                        id,
                        checked: pending_checked,
                    },
                    &worker_cancel,
                );
            });
        }
        if scheduler.is_empty() || cancel.load(Ordering::Acquire) {
            break;
        }
        crossbeam_channel::select! {
            recv(receiver) -> message => match message {
                Ok(WorkerMessage::Progress { parent, tasks, checked }) => {
                    scheduler.checked(parent, checked);
                    for task in tasks {
                        scheduler.insert(task, Some(parent));
                    }
                }
                Ok(WorkerMessage::Complete { id, checked }) => {
                    scheduler.checked(id, checked);
                    scheduler.complete(id);
                }
                Err(_) => break,
            },
            recv(wake_receiver) -> _ => {},
            default(SEND_INTERVAL) => {},
        }
    }
    // Dropping the receiver releases workers blocked on pending-task delivery.
    drop(receiver);
}

fn send_worker(
    sender: &Sender<WorkerMessage>,
    mut message: WorkerMessage,
    cancel: &AtomicBool,
) -> io::Result<()> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
        }
        match sender.send_timeout(message, SEND_INTERVAL) {
            Ok(()) => return Ok(()),
            Err(SendTimeoutError::Timeout(returned)) => message = returned,
            Err(SendTimeoutError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "scan scheduler disconnected",
                ));
            }
        }
    }
}

fn anchored_metadata(
    anchor: &RootAnchor,
    relative: &Path,
    cancel: &AtomicBool,
    control: &ScanControl,
) -> io::Result<EntryMetadata> {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan path must stay beneath its root",
        ));
    }
    control.wait_unpaused(cancel, None)?;
    if relative.as_os_str().is_empty() {
        return anchor.metadata();
    }
    #[cfg(unix)]
    {
        let parent = anchor.open_parent_cancellable(relative, cancel)?;
        control.wait_unpaused(cancel, None)?;
        backend::MetadataReader::Native(parent).metadata(relative.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "scan path has no final entry")
        })?)
    }
    #[cfg(not(unix))]
    {
        backend::root_metadata(&anchor.root_path().join(relative))
    }
}

#[cfg(test)]
fn scan_directory(
    task: &DirectoryTask,
    anchor: &RootAnchor,
    options: &ScanOptions,
    emitter: &Mutex<Emitter>,
    next: &Mutex<Vec<DirectoryTask>>,
    cancel: &AtomicBool,
    control: &ScanControl,
) {
    scan_directory_emit(
        task,
        anchor,
        options,
        emitter,
        cancel,
        control,
        |directories, _checked| {
            next.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend(directories);
            Ok(())
        },
    );
}

fn scan_directory_emit(
    task: &DirectoryTask,
    anchor: &RootAnchor,
    options: &ScanOptions,
    emitter: &Mutex<Emitter>,
    cancel: &AtomicBool,
    control: &ScanControl,
    mut emit_directories: impl FnMut(Vec<DirectoryTask>, usize) -> io::Result<()>,
) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let active = control.activate(task, cancel);
    let result = if active.cancel.load(Ordering::Acquire) {
        Ok(())
    } else {
        control
            .wait_unpaused(cancel, Some(&active.cancel))
            .and_then(|()| {
                backend::enumerate_anchored(
                    anchor,
                    &task.path,
                    options.backend,
                    options.buffer_size,
                    options.batch_size,
                    &active.cancel,
                    |names, reader| {
                        control.wait_unpaused(cancel, Some(&active.cancel))?;
                        // Include files and failed metadata checks in the batch's effort.
                        let checked = names.len();
                        #[cfg(target_os = "linux")]
                        let discovered = if options.backend == Backend::Auto
                            && crate::uring::metadata_available()
                        {
                            let backend::MetadataReader::Native(fd) = reader;
                            ring_metadata_batch(
                                names,
                                reader,
                                &task.path,
                                cancel,
                                &active.cancel,
                                control,
                                |window| {
                                    crate::uring::metadata_batch(
                                        fd,
                                        window,
                                        &active.cancel,
                                        |index| {
                                            !cancel.load(Ordering::Acquire)
                                                && !control.paused.load(Ordering::Acquire)
                                                && !control
                                                    .excludes_child(&task.path, &window[index])
                                        },
                                    )
                                },
                            )?
                        } else {
                            blocking_metadata_batch(
                                names,
                                reader,
                                &task.path,
                                cancel,
                                &active.cancel,
                                control,
                            )
                        };
                        #[cfg(not(target_os = "linux"))]
                        let discovered = blocking_metadata_batch(
                            names,
                            reader,
                            &task.path,
                            cancel,
                            &active.cancel,
                            control,
                        );
                        let directories = emitter
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .entries(task, discovered, cancel, control)?;
                        emit_directories(directories, checked)?;
                        control.wait_unpaused(cancel, Some(&active.cancel))
                    },
                )
            })
    };
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let error = if control.excludes(&task.path) {
        None
    } else {
        result.err().map(|error| error.to_string())
    };
    let _ = emitter
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(
            ScanEvent::DirectoryDone { id: task.id, error },
            cancel,
            false,
        );
}

fn read_metadata(
    name: &OsStr,
    reader: &backend::MetadataReader,
    parent: &Path,
    cancel: &AtomicBool,
    task_cancel: &AtomicBool,
    control: &ScanControl,
) -> io::Result<EntryMetadata> {
    control.wait_unpaused(cancel, Some(task_cancel))?;
    if control.excludes_child(parent, name) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
    } else {
        reader.metadata(name)
    }
}

fn blocking_metadata_batch(
    names: Vec<Arc<OsStr>>,
    reader: &backend::MetadataReader,
    parent: &Path,
    cancel: &AtomicBool,
    task_cancel: &AtomicBool,
    control: &ScanControl,
) -> Vec<(Arc<OsStr>, io::Result<EntryMetadata>)> {
    // Keep the original bounded Rayon batch when the ring is unavailable, including
    // parallel metadata for a single wide directory. No ring result slots are built.
    names
        .into_par_iter()
        .map(|name| {
            let metadata = read_metadata(&name, reader, parent, cancel, task_cancel, control);
            (name, metadata)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn ring_metadata_batch(
    names: Vec<Arc<OsStr>>,
    reader: &backend::MetadataReader,
    parent: &Path,
    cancel: &AtomicBool,
    task_cancel: &AtomicBool,
    control: &ScanControl,
    mut submit: impl FnMut(&[Arc<OsStr>]) -> crate::uring::BatchOutcome<rustix::fs::Stat>,
) -> io::Result<Vec<(Arc<OsStr>, io::Result<EntryMetadata>)>> {
    use crate::uring::{BatchOutcome, EntryOutcome};
    use rustix::io::Errno;

    let mut completed = Vec::with_capacity(names.len());
    let mut needs_blocking = false;
    for window in names.chunks(crate::uring::BATCH_LIMIT) {
        control.wait_unpaused(cancel, Some(task_cancel))?;
        let outcome = submit(window);
        // The driver has drained submitted work or closed its ring before returning.
        // Never pause while it still owns an in-flight metadata operation.
        control.wait_unpaused(cancel, Some(task_cancel))?;
        let (entries, failed) = match outcome {
            BatchOutcome::Unsupported => {
                needs_blocking = true;
                break;
            }
            BatchOutcome::Complete(entries) => (entries, false),
            BatchOutcome::Failed { entries, .. } => (entries, true),
        };
        debug_assert_eq!(entries.len(), window.len());
        for entry in entries {
            let metadata = match entry {
                EntryOutcome::Completed(Ok(stat)) => Some(Ok(backend::native_metadata(stat))),
                EntryOutcome::Completed(Err(error))
                    if matches!(error.raw_os_error(), Some(code) if
                        code == Errno::INTR.raw_os_error()
                            || code == Errno::NOSYS.raw_os_error()
                            || code == Errno::OPNOTSUPP.raw_os_error()
                            || code == Errno::INVAL.raw_os_error()) =>
                {
                    None
                }
                EntryOutcome::Completed(Err(error)) => Some(Err(error)),
                EntryOutcome::NotSubmitted | EntryOutcome::Unknown => None,
            };
            needs_blocking |= metadata.is_none();
            completed.push(metadata);
        }
        // Metadata is read-only: retry unresolved reads after draining, retaining
        // completed results and leaving ordinary per-entry errors unchanged.
        if failed || needs_blocking {
            needs_blocking = true;
            break;
        }
    }
    if needs_blocking {
        completed.resize_with(names.len(), || None);
        Ok(names
            .into_par_iter()
            .zip(completed.into_par_iter())
            .map(|(name, metadata)| {
                let metadata = metadata.unwrap_or_else(|| {
                    read_metadata(&name, reader, parent, cancel, task_cancel, control)
                });
                (name, metadata)
            })
            .collect())
    } else {
        // Fully completed rings need no further Rayon dispatch or Arc clones.
        Ok(names
            .into_iter()
            .zip(completed)
            .map(|(name, metadata)| (name, metadata.expect("completed metadata")))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::AtomicU64;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            fs::create_dir_all(".tmp").unwrap();
            let path = PathBuf::from(".tmp").join(format!(
                "ldu-scan-test-{}-{stamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> ScanOptions {
        ScanOptions {
            threads: 4,
            batch_size: 4,
            buffer_size: 4096,
            backend: Backend::Std,
        }
    }

    fn collect(handle: &ScanHandle) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        loop {
            let event = handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("scanner failed to send its terminal event");
            let terminal = matches!(&event, ScanEvent::Finished { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    fn task(id: NodeId, path: &str) -> DirectoryTask {
        DirectoryTask {
            id,
            path: path.into(),
        }
    }

    #[test]
    fn available_workers_take_deeper_work_while_shallower_directories_are_active() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        assert_eq!(scheduler.take().unwrap().id, 0);
        scheduler.insert(task(1, "first"), Some(0));
        scheduler.insert(task(2, "second"), Some(0));
        assert_eq!(scheduler.take().unwrap().id, 1);
        scheduler.insert(task(3, "first/deep"), Some(1));
        assert_eq!(scheduler.take().unwrap().id, 2);
        assert_eq!(scheduler.take().unwrap().id, 3);
        assert_eq!(scheduler.active(), 4);
        assert!(scheduler.take().is_none());
        for id in [1, 3, 0, 2] {
            scheduler.complete(id);
        }
        assert!(scheduler.is_empty());
    }

    #[test]
    fn checked_effort_follows_subtrees_across_depth_and_child_proliferation() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "wide"), Some(0));
        scheduler.insert(task(2, "small"), Some(0));
        scheduler.take().unwrap();
        scheduler.take().unwrap();
        scheduler.checked(1, 20_000);
        scheduler.checked(2, 3);
        for id in 3..=34 {
            scheduler.insert(task(id, &format!("wide/child-{id}")), Some(1));
        }
        scheduler.insert(task(35, "small/child"), Some(2));
        scheduler.complete(1);
        scheduler.complete(2);
        scheduler.complete(0);
        assert_eq!(scheduler.take().unwrap().id, 35);
        scheduler.checked(35, 2);
        scheduler.insert(task(36, "small/child/deep"), Some(35));
        assert_eq!(scheduler.take().unwrap().id, 36);
        assert_eq!(scheduler.directories[&2].checked, 5);
        assert_eq!(scheduler.directories[&0].checked, 20_005);
        assert_eq!(scheduler.directories[&1].checked, 20_000);
    }

    #[test]
    fn active_reservations_spread_dispatches_before_progress_arrives() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "first"), Some(0));
        scheduler.insert(task(2, "second"), Some(0));
        scheduler.take().unwrap();
        scheduler.take().unwrap();
        scheduler.insert(task(3, "first/a"), Some(1));
        scheduler.insert(task(4, "first/b"), Some(1));
        scheduler.insert(task(5, "second/a"), Some(2));
        scheduler.insert(task(6, "second/b"), Some(2));
        scheduler.checked(2, (ACTIVE_EFFORT_RESERVATION / 2) as usize);
        assert_eq!(scheduler.take().unwrap().id, 3);
        assert_eq!(scheduler.take().unwrap().id, 5);
        scheduler.complete(3);
        assert_eq!(scheduler.take().unwrap().id, 4);
    }

    #[test]
    fn rotating_samples_find_under_served_work_in_a_wide_frontier() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        for id in 1..=32 {
            scheduler.insert(task(id, &format!("child-{id}")), Some(0));
            if id != 21 {
                scheduler.checked(id, 10_000);
            }
        }
        let early: Vec<_> = (0..4).map(|_| scheduler.take().unwrap().id).collect();
        assert!(
            early.contains(&21),
            "rotating samples must reach later siblings"
        );
        let mut seen: std::collections::HashSet<_> = early.into_iter().collect();
        while let Some(task) = scheduler.take() {
            assert!(
                seen.insert(task.id),
                "a pending slot must be removed exactly once"
            );
        }
        assert_eq!(seen.len(), 32);
        for id in 0..=32 {
            scheduler.complete(id);
        }
        assert!(scheduler.is_empty());
    }

    #[test]
    fn emptied_branch_becomes_available_again_when_active_reader_discovers_more() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "streamed"), Some(0));
        scheduler.take().unwrap();
        for id in 2..10 {
            scheduler.insert(task(id, &format!("streamed/child-{id}")), Some(1));
            assert_eq!(scheduler.take().unwrap().id, id);
            scheduler.complete(id);
            assert!(scheduler.take().is_none());
        }
        scheduler.complete(0);
        scheduler.complete(1);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn focused_pending_work_overrides_effort_and_falls_back_when_unavailable() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, "scope"), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "scope/earlier"), Some(0));
        scheduler.insert(task(2, "scope/focused"), Some(0));
        scheduler.checked(2, 10_000);
        scheduler.focus = Some("scope/focused".into());
        assert_eq!(scheduler.take().unwrap().id, 2);
        scheduler.insert(task(3, "scope/focused/deep"), Some(2));
        assert_eq!(scheduler.take().unwrap().id, 3);
        assert_eq!(scheduler.take().unwrap().id, 1);
        scheduler.insert(task(4, "scope/later"), Some(0));
        scheduler.focus = Some("scope/not-discovered".into());
        assert_eq!(scheduler.take().unwrap().id, 4);
    }

    #[test]
    fn newest_focus_replaces_previous_branch_priority() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "old"), Some(0));
        scheduler.insert(task(2, "new"), Some(0));
        scheduler.focus = Some("old".into());
        scheduler.focus = Some("new".into());
        assert_eq!(scheduler.take().unwrap().id, 2);
        scheduler.insert(task(3, "new/deep"), Some(2));
        scheduler.focus = Some("old".into());
        assert_eq!(scheduler.take().unwrap().id, 1);
        scheduler.insert(task(4, "old/deep"), Some(1));
        scheduler.focus = Some("new".into());
        assert_eq!(scheduler.take().unwrap().id, 3);

        let control = ScanControl::default();
        control.prioritize(Path::new("old"));
        control.prioritize(Path::new("./new"));
        control.prioritize(Path::new("../invalid"));
        assert_eq!(
            control.take_focus(),
            Some(FocusRequest::Prioritize(PathBuf::from("new")))
        );
        assert_eq!(control.take_focus(), None);
    }

    #[test]
    fn clearing_latest_focus_restores_effort_scheduling() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "focused"), Some(0));
        scheduler.insert(task(2, "under-served"), Some(0));
        scheduler.checked(1, 10_000);
        scheduler.focus = Some("focused".into());
        assert_eq!(scheduler.focused(), Some(1));

        let control = ScanControl::default();
        let (sender, receiver) = bounded(1);
        control.wake_sender.set(sender).unwrap();
        control.prioritize(Path::new("focused"));
        receiver.try_recv().unwrap();
        control.clear_priority();
        receiver.try_recv().unwrap();
        assert_eq!(control.take_focus(), Some(FocusRequest::Clear));
        scheduler.focus = None;
        assert_eq!(scheduler.take().unwrap().id, 2);
        assert_eq!(scheduler.take().unwrap().id, 1);

        control.clear_priority();
        control.prioritize(Path::new("focused"));
        assert_eq!(
            control.take_focus(),
            Some(FocusRequest::Prioritize(PathBuf::from("focused")))
        );
        assert_eq!(control.take_focus(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_ring_metadata_preserves_raw_names_order_and_nofollow() {
        use std::os::unix::ffi::OsStrExt;

        let fixture = Fixture::new();
        let raw_name = OsStr::from_bytes(b"raw-\xff");
        fs::write(fixture.0.join(raw_name), b"payload").unwrap();
        fs::create_dir(fixture.0.join("directory")).unwrap();
        std::os::unix::fs::symlink("directory", fixture.0.join("link")).unwrap();
        let anchor = RootAnchor::open(&fixture.0).unwrap();
        let reader = backend::MetadataReader::Native(anchor.open_directory(Path::new("")).unwrap());
        let mut names: Vec<Arc<OsStr>> = [raw_name, OsStr::new("link"), OsStr::new("directory")]
            .into_iter()
            .map(Arc::from)
            .collect();
        names.extend((0..130).map(|index| Arc::from(OsStr::new(&format!("missing-{index}")))));
        let expected = names.clone();
        let mut calls = 0;
        let discovered = ring_metadata_batch(
            names,
            &reader,
            Path::new(""),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &ScanControl::default(),
            |window| {
                calls += 1;
                assert_eq!(window.len(), crate::uring::BATCH_LIMIT);
                crate::uring::BatchOutcome::Unsupported
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            discovered.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
        assert_eq!(discovered[0].1.as_ref().unwrap().sizes.apparent, 7);
        assert_eq!(discovered[1].1.as_ref().unwrap().kind, NodeKind::Symlink);
        assert_eq!(discovered[2].1.as_ref().unwrap().kind, NodeKind::Directory);
        assert!(
            discovered[3..]
                .iter()
                .all(|(_, result)| result.as_ref().unwrap_err().kind() == io::ErrorKind::NotFound)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_ring_metadata_keeps_completions_and_retries_only_unresolved_reads() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        use rustix::fs::{AtFlags, statat};
        use rustix::io::Errno;

        let fixture = Fixture::new();
        let names: Vec<Arc<OsStr>> = (0..9)
            .map(|index| {
                let name = format!("file-{index}");
                fs::write(fixture.0.join(&name), b"payload").unwrap();
                Arc::from(OsStr::new(&name))
            })
            .collect();
        let anchor = RootAnchor::open(&fixture.0).unwrap();
        let reader = backend::MetadataReader::Native(anchor.open_directory(Path::new("")).unwrap());
        let backend::MetadataReader::Native(fd) = &reader;
        let control = ScanControl::default();
        let mut calls = 0;
        let discovered = ring_metadata_batch(
            names,
            &reader,
            Path::new(""),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &control,
            |window| {
                calls += 1;
                let first = statat(fd, window[0].as_ref(), AtFlags::SYMLINK_NOFOLLOW).unwrap();
                // A completed read must survive later path changes and driver failure.
                fs::remove_file(fixture.0.join(window[0].as_ref())).unwrap();
                control.exclude(Path::new("file-8"));
                BatchOutcome::Failed {
                    entries: vec![
                        EntryOutcome::Completed(Ok(first)),
                        EntryOutcome::Completed(Err(Errno::ACCESS.into())),
                        EntryOutcome::Unknown,
                        EntryOutcome::NotSubmitted,
                        EntryOutcome::Completed(Err(Errno::INTR.into())),
                        EntryOutcome::Completed(Err(Errno::NOSYS.into())),
                        EntryOutcome::Completed(Err(Errno::OPNOTSUPP.into())),
                        EntryOutcome::Completed(Err(Errno::INVAL.into())),
                        EntryOutcome::NotSubmitted,
                    ],
                    error: io::Error::other("injected driver failure after drain"),
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(discovered[0].1.as_ref().unwrap().sizes.apparent, 7);
        assert_eq!(
            discovered[1].1.as_ref().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            discovered[2..8]
                .iter()
                .all(|(_, result)| result.as_ref().unwrap().sizes.apparent == 7)
        );
        assert_eq!(
            discovered[8].1.as_ref().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ring_metadata_windows_pause_after_drain_and_cancel_before_readmission() {
        use crate::uring::{BatchOutcome, EntryOutcome};
        use rustix::fs::{AtFlags, statat};

        for finish in 0..3 {
            let fixture = Fixture::new();
            fs::write(fixture.0.join("source"), b"payload").unwrap();
            let anchor = RootAnchor::open(&fixture.0).unwrap();
            let reader =
                backend::MetadataReader::Native(anchor.open_directory(Path::new("")).unwrap());
            let control = Arc::new(ScanControl::default());
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_control = Arc::clone(&control);
            let worker_cancel = Arc::clone(&cancel);
            let (paused_sender, paused_receiver) = bounded(1);
            let (done_sender, done_receiver) = bounded(1);
            let worker = thread::spawn(move || {
                let task = task(0, "");
                let active = worker_control.activate(&task, &worker_cancel);
                let backend::MetadataReader::Native(fd) = &reader;
                let names: Vec<Arc<OsStr>> = (0..130)
                    .map(|index| Arc::from(OsStr::new(&format!("entry-{index}"))))
                    .collect();
                let expected = names.clone();
                let mut windows = Vec::new();
                let result = ring_metadata_batch(
                    names,
                    &reader,
                    &task.path,
                    &worker_cancel,
                    &active.cancel,
                    &worker_control,
                    |window| {
                        windows.push(window.len());
                        let entries = window
                            .iter()
                            .map(|_| {
                                EntryOutcome::Completed(
                                    statat(fd, "source", AtFlags::SYMLINK_NOFOLLOW)
                                        .map_err(io::Error::from),
                                )
                            })
                            .collect();
                        if windows.len() == 1 {
                            worker_control.set_paused(true);
                            paused_sender.send(()).unwrap();
                        }
                        BatchOutcome::Complete(entries)
                    },
                );
                if let Ok(discovered) = &result {
                    assert_eq!(
                        discovered.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                        expected.iter().collect::<Vec<_>>()
                    );
                    assert!(
                        discovered.iter().all(|(_, result)| result
                            .as_ref()
                            .unwrap()
                            .sizes
                            .apparent
                            == 7)
                    );
                }
                done_sender.send((result.is_ok(), windows)).unwrap();
            });
            paused_receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert!(matches!(
                done_receiver.recv_timeout(SEND_INTERVAL * 2),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ));
            match finish {
                0 => control.set_paused(false),
                1 => {
                    cancel.store(true, Ordering::Release);
                    control.cancel_active();
                    control.wake();
                }
                _ => control.exclude(Path::new("")),
            }
            let (complete, windows) = done_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            assert_eq!(complete, finish == 0);
            assert_eq!(
                windows,
                if finish == 0 {
                    vec![64, 64, 2]
                } else {
                    vec![64]
                }
            );
        }
    }

    #[test]
    fn file_only_batches_report_checked_entries_without_directory_discoveries() {
        let fixture = Fixture::new();
        for index in 0..13 {
            fs::write(fixture.0.join(format!("file-{index}")), b"payload").unwrap();
        }
        let anchor = RootAnchor::open(&fixture.0).unwrap();
        let (sender, receiver) = bounded(8);
        let emitter = Mutex::new(Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::new(),
        });
        let mut checked = Vec::new();
        scan_directory_emit(
            &task(0, ""),
            &anchor,
            &options(),
            &emitter,
            &AtomicBool::new(false),
            &ScanControl::default(),
            |directories, entries| {
                assert!(directories.is_empty());
                checked.push(entries);
                Ok(())
            },
        );
        assert_eq!(checked, vec![4, 4, 4, 1]);
        assert!(matches!(
            receiver.try_iter().last(),
            Some(ScanEvent::DirectoryDone { id: 0, error: None })
        ));
    }

    #[test]
    fn paused_directory_batches_resume_and_cancellation_releases_waiters() {
        for backend in [Backend::Std, Backend::Auto] {
            for finish in 0..3 {
                let fixture = Fixture::new();
                for index in 0..8 {
                    fs::write(fixture.0.join(format!("file-{index}")), b"payload").unwrap();
                }
                let anchor = RootAnchor::open(&fixture.0).unwrap();
                let control = Arc::new(ScanControl::default());
                let cancel = Arc::new(AtomicBool::new(false));
                let (sender, receiver) = bounded(16);
                let emitter = Mutex::new(Emitter {
                    sender,
                    next_id: 1,
                    visited_directories: HashSet::new(),
                });
                let (paused_sender, paused_receiver) = bounded(1);
                let (done_sender, done_receiver) = bounded(1);
                let worker_control = Arc::clone(&control);
                let worker_cancel = Arc::clone(&cancel);
                let worker = thread::spawn(move || {
                    let mut options = options();
                    options.backend = backend;
                    options.batch_size = 2;
                    let mut first = true;
                    scan_directory_emit(
                        &task(0, ""),
                        &anchor,
                        &options,
                        &emitter,
                        &worker_cancel,
                        &worker_control,
                        |_, _| {
                            if first {
                                first = false;
                                worker_control.set_paused(true);
                                paused_sender.send(()).unwrap();
                            }
                            Ok(())
                        },
                    );
                    done_sender.send(()).unwrap();
                });
                paused_receiver
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                assert!(control.paused.load(Ordering::Acquire));
                assert!(matches!(
                    receiver.try_recv().unwrap(),
                    ScanEvent::Entries { entries, .. } if entries.len() == 2
                ));
                // This checks the paused condition, not a throughput threshold.
                assert!(matches!(
                    done_receiver.recv_timeout(SEND_INTERVAL * 2),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout)
                ));
                assert!(receiver.is_empty());
                match finish {
                    0 => control.set_paused(false),
                    1 => {
                        cancel.store(true, Ordering::Release);
                        control.cancel_active();
                        control.wake();
                    }
                    _ => control.exclude(Path::new("")),
                }
                done_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
                worker.join().unwrap();
                let remaining = receiver
                    .try_iter()
                    .filter_map(|event| match event {
                        ScanEvent::Entries { entries, .. } => Some(entries.len()),
                        _ => None,
                    })
                    .sum::<usize>();
                assert_eq!(remaining, if finish == 0 { 6 } else { 0 });
            }
        }
    }

    #[test]
    fn cancellation_releases_a_full_scheduler_progress_channel() {
        let (sender, _receiver) = bounded(1);
        sender
            .send(WorkerMessage::Complete { id: 0, checked: 0 })
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (result_sender, result_receiver) = bounded(1);
        let worker = thread::spawn(move || {
            let result = send_worker(
                &sender,
                WorkerMessage::Progress {
                    parent: 0,
                    tasks: Vec::new(),
                    checked: 1024,
                },
                &worker_cancel,
            );
            result_sender.send(result).unwrap();
        });
        cancel.store(true, Ordering::Release);
        let error = result_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        worker.join().unwrap();
    }

    #[test]
    fn completed_handles_do_not_accumulate_new_exclusion_paths() {
        let make_handle = |finished| {
            let (_sender, receiver) = bounded(1);
            ScanHandle {
                receiver,
                cancel: Arc::new(AtomicBool::new(false)),
                finished: Arc::new(AtomicBool::new(finished)),
                anchor: Arc::new(OnceLock::new()),
                control: Arc::new(ScanControl::default()),
            }
        };
        let completed = make_handle(true);
        let active = make_handle(false);
        for index in 0..1024 {
            let relative = PathBuf::from(format!("entry-{index}"));
            completed.exclude(&relative);
            active.exclude(&relative);
        }
        assert!(!completed.control.has_exclusions.load(Ordering::Acquire));
        assert!(
            completed
                .control
                .state
                .read()
                .unwrap()
                .excluded
                .children
                .is_empty()
        );
        assert_eq!(
            active.control.state.read().unwrap().excluded.children.len(),
            1024
        );
        assert!(active.control.excludes(Path::new("entry-17/child")));
        assert!(!active.control.excludes(Path::new("entry-1024")));

        // Active exclusion still cancels matching work. Completion then prevents
        // later UI confirmations from growing the retained control state.
        let task_cancel = Arc::new(AtomicBool::new(false));
        active.control.state.write().unwrap().active.insert(
            1,
            ActiveDirectory {
                path: PathBuf::from("running/child"),
                cancel: Arc::clone(&task_cancel),
            },
        );
        active.exclude(Path::new("running"));
        assert!(task_cancel.load(Ordering::Acquire));
        active.finished.store(true, Ordering::Release);
        for index in 0..1024 {
            active.exclude(Path::new(&format!("later-{index}")));
        }
        assert_eq!(
            active.control.state.read().unwrap().excluded.children.len(),
            1025
        );
        assert!(!active.control.excludes(Path::new("later-0")));
    }

    #[test]
    fn producer_completion_releases_exclusions_and_rejects_late_control_calls() {
        let control = Arc::new(ScanControl::default());
        let finished = Arc::new(AtomicBool::new(false));
        let cancel = AtomicBool::new(false);
        let blocked = control.activate(&task(1, "blocked/child"), &cancel);
        let survivor = control.activate(&task(2, "survivor/child"), &cancel);
        control.exclude(Path::new("blocked"));
        assert!(blocked.cancel.load(Ordering::Acquire));
        assert!(!survivor.cancel.load(Ordering::Acquire));
        for index in 0..2048 {
            control.exclude(Path::new(&format!("entry-{index}")));
        }
        assert_eq!(control.state.read().unwrap().excluded.children.len(), 2049);

        drop(FinishedGuard {
            finished: Arc::clone(&finished),
            control: Arc::clone(&control),
        });
        assert!(finished.load(Ordering::Acquire));
        assert!(blocked.cancel.load(Ordering::Acquire));
        assert!(survivor.cancel.load(Ordering::Acquire));
        assert!(!cancel.load(Ordering::Acquire));
        {
            let state = control.state.read().unwrap();
            assert!(state.excluded.children.is_empty());
            assert!(state.excluded.terminal);
        }
        // Model an exclusion call already admitted by the handle's fast check:
        // it reaches the locked control path after producer cleanup finished.
        for index in 0..2048 {
            control.exclude(Path::new(&format!("late-{index}")));
        }
        assert!(control.state.read().unwrap().excluded.children.is_empty());
        assert!(control.excludes(Path::new("late-0/child")));
        assert!(control.excludes_child(Path::new(""), OsStr::new("future")));
        let late = control.activate(&task(3, "future"), &cancel);
        assert!(late.cancel.load(Ordering::Acquire));
        drop((blocked, survivor, late));
        assert!(control.state.read().unwrap().active.is_empty());
    }

    #[test]
    fn indexed_exclusions_cover_wide_batches_and_ancestor_supersession() {
        let control = ScanControl::default();
        for index in 0..2048 {
            control.exclude(Path::new(&format!("batch/item-{index}")));
        }
        assert!(control.excludes(Path::new("batch/item-1024/child")));
        assert!(!control.excludes(Path::new("batch/item-2048")));
        control.exclude(Path::new("batch"));
        assert!(control.excludes(Path::new("batch/item-2048/child")));
        assert!(!control.excludes(Path::new("batch-other/item-1024")));
    }

    #[test]
    fn child_exclusions_preserve_ancestors_exact_names_and_root_paths() {
        let control = ScanControl::default();
        assert!(!control.excludes_child(Path::new("branch"), std::ffi::OsStr::new("blocked")));
        control.exclude(Path::new("branch/blocked"));
        control.exclude(Path::new("ancestor"));
        for (parent, name, expected) in [
            ("branch", "blocked", true),
            ("./branch/.", "blocked", true),
            ("branch/blocked", "child", true),
            ("branch/blocked/deep", "child", true),
            ("branch", "blocked-other", false),
            ("branch", "survivor", false),
            ("branch-other", "blocked", false),
            ("", "branch", false),
            ("", "ancestor", true),
            (".", "ancestor", true),
            ("ancestor/deep", "child", true),
            ("", "ancestor-other", false),
            ("unrelated", "blocked", false),
        ] {
            assert_eq!(
                control.excludes_child(Path::new(parent), std::ffi::OsStr::new(name)),
                expected,
                "{parent:?} / {name:?}"
            );
        }
        control.exclude(Path::new("."));
        assert!(control.excludes_child(Path::new(""), std::ffi::OsStr::new("anything")));
        assert!(control.excludes_child(Path::new("unrelated"), std::ffi::OsStr::new("child")));
    }

    #[test]
    fn deep_exclusions_drop_without_recursing_on_the_thread_stack() {
        thread::Builder::new()
            .name("ldu-deep-exclusion-drop".into())
            .stack_size(64 * 1024)
            .spawn(|| {
                let mut path = PathBuf::new();
                for _ in 0..20_000 {
                    path.push("branch");
                }
                let mut excluded = ExclusionTrie::default();
                excluded.insert(&path);
                excluded.insert(Path::new("a"));
                excluded.insert(Path::new("z"));
                assert!(excluded.excludes(&path));
                // Supersession retains the deep descendants until destruction.
                excluded.insert(Path::new("branch"));
                assert!(excluded.excludes(Path::new("branch/later")));
                let control = Arc::new(ScanControl::default());
                control.state.write().unwrap().excluded = excluded;
                let finished = Arc::new(AtomicBool::new(false));
                drop(FinishedGuard {
                    finished: Arc::clone(&finished),
                    control: Arc::clone(&control),
                });
                assert!(finished.load(Ordering::Acquire));
                let state = control.state.read().unwrap();
                assert!(state.excluded.children.is_empty());
                assert!(state.excluded.terminal);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn subtree_exclusions_interrupt_active_and_future_tasks_without_prefix_collisions() {
        let control = ScanControl::default();
        let cancel = AtomicBool::new(false);
        let child_task = DirectoryTask {
            id: 1,
            path: "branch/child".into(),
        };
        let neighbor_task = DirectoryTask {
            id: 2,
            path: "branch-other".into(),
        };
        let child = control.activate(&child_task, &cancel);
        let neighbor = control.activate(&neighbor_task, &cancel);
        control.exclude(Path::new("branch/child/deep"));
        assert!(!child.cancel.load(Ordering::Acquire));
        control.exclude(Path::new("./branch"));
        assert!(child.cancel.load(Ordering::Acquire));
        assert!(!neighbor.cancel.load(Ordering::Acquire));
        assert!(control.excludes(Path::new("branch/later")));
        assert!(!control.excludes(Path::new("branch-other/later")));
        let future_task = DirectoryTask {
            id: 3,
            path: "branch/later".into(),
        };
        let future = control.activate(&future_task, &cancel);
        assert!(future.cancel.load(Ordering::Acquire));
        control.exclude(Path::new("../branch-other"));
        assert!(!neighbor.cancel.load(Ordering::Acquire));
        assert!(
            control
                .state
                .read()
                .unwrap()
                .excluded
                .excludes(Path::new("branch"))
        );
    }

    #[test]
    fn excluded_metadata_errors_do_not_consume_ids_or_schedule_directories() {
        let (sender, receiver) = bounded(2);
        let mut emitter = Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::new(),
        };
        let control = ScanControl::default();
        control.exclude(Path::new("removed"));
        let cancel = AtomicBool::new(false);
        let task = DirectoryTask {
            id: 0,
            path: PathBuf::new(),
        };
        let metadata = EntryMetadata {
            kind: NodeKind::Directory,
            sizes: Sizes::default(),
            identity: None,
            links: 1,
        };
        let next = emitter
            .entries(
                &task,
                vec![
                    (
                        Arc::from(OsStr::new("removed")),
                        Err(io::Error::from(io::ErrorKind::NotFound)),
                    ),
                    (Arc::from(OsStr::new("survivor")), Ok(metadata)),
                ],
                &cancel,
                &control,
            )
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].path, Path::new("survivor"));
        let ScanEvent::Entries { entries, .. } = receiver.recv().unwrap() else {
            panic!("expected an entry batch");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[0].name.as_ref(), "survivor");
        assert!(entries[0].error.is_none());
    }

    #[test]
    fn excluded_missing_directory_finishes_cleanly_and_unrelated_directory_scans() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("survivor")).unwrap();
        fs::write(fixture.0.join("survivor/payload"), b"still scanning").unwrap();
        let anchor = RootAnchor::open(&fixture.0).unwrap();
        let (sender, receiver) = bounded(8);
        let emitter = Mutex::new(Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::new(),
        });
        let next = Mutex::new(Vec::new());
        let control = ScanControl::default();
        let cancel = AtomicBool::new(false);
        control.exclude(Path::new("deleted"));
        for (id, path) in [(7, "deleted"), (8, "survivor")] {
            scan_directory(
                &DirectoryTask {
                    id,
                    path: path.into(),
                },
                &anchor,
                &options(),
                &emitter,
                &next,
                &cancel,
                &control,
            );
        }
        assert!(matches!(
            receiver.recv().unwrap(),
            ScanEvent::DirectoryDone { id: 7, error: None }
        ));
        let ScanEvent::Entries { parent, entries } = receiver.recv().unwrap() else {
            panic!("expected unrelated entries");
        };
        assert_eq!(parent, 8);
        assert_eq!(entries[0].name.as_ref(), "payload");
        assert!(entries[0].error.is_none());
        assert!(matches!(
            receiver.recv().unwrap(),
            ScanEvent::DirectoryDone { id: 8, error: None }
        ));
        assert!(!cancel.load(Ordering::Acquire));
    }

    #[test]
    fn excluding_backpressured_subtree_keeps_assigned_ids_and_hides_deletion_errors() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("deleted")).unwrap();
        for index in 0..100 {
            fs::write(
                fixture.0.join(format!("deleted/file-{index}")),
                b"queued bytes",
            )
            .unwrap();
        }
        let anchor = RootAnchor::open(&fixture.0).unwrap();
        let (sender, receiver) = bounded(1);
        let emitter = Arc::new(Mutex::new(Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::new(),
        }));
        let control = Arc::new(ScanControl::default());
        let worker_control = Arc::clone(&control);
        let worker_emitter = Arc::clone(&emitter);
        let worker = thread::spawn(move || {
            let mut scan_options = options();
            scan_options.batch_size = 1;
            scan_directory(
                &DirectoryTask {
                    id: 7,
                    path: "deleted".into(),
                },
                &anchor,
                &scan_options,
                &worker_emitter,
                &Mutex::new(Vec::new()),
                &AtomicBool::new(false),
                &worker_control,
            );
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while receiver.is_empty() {
            assert!(
                Instant::now() < deadline,
                "scanner did not begin its subtree"
            );
            thread::sleep(Duration::from_millis(1));
        }
        control.exclude(Path::new("deleted"));
        fs::remove_dir_all(fixture.0.join("deleted")).unwrap();
        let mut next_id = 1;
        loop {
            match receiver.recv_timeout(Duration::from_secs(10)).unwrap() {
                ScanEvent::Entries { parent, entries } => {
                    assert_eq!(parent, 7);
                    for entry in entries {
                        assert_eq!(entry.id, next_id);
                        next_id += 1;
                        assert!(entry.error.is_none());
                    }
                }
                ScanEvent::DirectoryDone { id: 7, error } => {
                    assert!(error.is_none());
                    break;
                }
                event => panic!("unexpected subtree event: {event:?}"),
            }
        }
        worker.join().unwrap();
        assert!(
            (2..=3).contains(&next_id),
            "only the queued and already assigned batches may survive"
        );
    }

    #[test]
    fn scoped_root_refresh_reads_changed_inode_metadata() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::write(&root, b"before").unwrap();
        let initial = spawn(root.clone(), options());
        let events = collect(&initial);
        assert!(matches!(
            events.first(),
            Some(ScanEvent::RootInfo {
                kind: NodeKind::File,
                sizes,
                error: None,
                ..
            }) if sizes.apparent == 6
        ));

        // A file changes size reliably even on filesystems with fixed-size
        // directory metadata. This uses the same empty path as a root refresh.
        fs::write(&root, b"updated root metadata").unwrap();
        let expected = backend::root_metadata(&root).unwrap();
        assert_ne!(expected.sizes.apparent, 6);
        let scoped = spawn_subtree(
            Arc::clone(initial.anchor.get().unwrap()),
            PathBuf::new(),
            options(),
        );
        let events = collect(&scoped);
        assert!(matches!(
            events.as_slice(),
            [
                ScanEvent::RootInfo {
                    kind: NodeKind::File,
                    sizes,
                    error: None,
                    ..
                },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false }
            ] if *sizes == expected.sizes
        ));
    }

    #[cfg(unix)]
    #[test]
    fn scoped_root_refresh_retains_original_directory_after_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("original"), b"held inode").unwrap();
        fs::create_dir(fixture.0.join("outside")).unwrap();
        fs::write(fixture.0.join("outside/payload"), b"must not traverse").unwrap();
        let initial = spawn(root.clone(), options());
        collect(&initial);
        let anchor = Arc::clone(initial.anchor.get().unwrap());
        let identity = anchor.metadata().unwrap().identity;
        fs::rename(&root, fixture.0.join("held-root")).unwrap();
        symlink("outside", &root).unwrap();

        for display_path_exists in [true, false] {
            if !display_path_exists {
                fs::remove_file(&root).unwrap();
            }
            let scoped = spawn_subtree(Arc::clone(&anchor), PathBuf::new(), options());
            let events = collect(&scoped);
            assert!(matches!(
                events.first(),
                Some(ScanEvent::RootInfo {
                    kind: NodeKind::Directory,
                    error: None,
                    ..
                }),
            ));
            let names: Vec<_> = events
                .iter()
                .flat_map(|event| match event {
                    ScanEvent::Entries { entries, .. } => entries.as_slice(),
                    _ => &[],
                })
                .map(|entry| entry.name.as_ref())
                .collect();
            assert_eq!(names, [OsStr::new("original")]);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ScanEvent::DirectoryDone { id: 0, error: None },))
            );
            assert!(matches!(
                events.last(),
                Some(ScanEvent::Finished { cancelled: false })
            ));
            assert_eq!(anchor.metadata().unwrap().identity, identity);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn fallback_root_refresh_observes_file_replacement_and_missing_root() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::create_dir(&root).unwrap();
        let initial = spawn(root.clone(), options());
        collect(&initial);
        let anchor = Arc::clone(initial.anchor.get().unwrap());
        fs::remove_dir(&root).unwrap();
        fs::write(&root, b"replacement").unwrap();
        let scoped = spawn_subtree(Arc::clone(&anchor), PathBuf::new(), options());
        assert!(matches!(
            collect(&scoped).as_slice(),
            [
                ScanEvent::RootInfo {
                    kind: NodeKind::File,
                    error: None,
                    ..
                },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false },
            ],
        ));
        fs::remove_file(&root).unwrap();
        let missing = spawn_subtree(anchor, PathBuf::new(), options());
        assert!(matches!(
            collect(&missing).as_slice(),
            [
                ScanEvent::RootInfo {
                    kind: NodeKind::Unknown,
                    error: Some(_),
                    ..
                },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false },
            ],
        ));
    }

    #[cfg(unix)]
    #[test]
    fn scoped_refresh_shares_groups_for_previously_single_link_files() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("branch")).unwrap();
        fs::write(fixture.0.join("outside"), b"shared identity").unwrap();
        let initial = spawn(fixture.0.clone(), options());
        let group = collect(&initial)
            .into_iter()
            .find_map(|event| match event {
                ScanEvent::Entries { entries, .. } => entries
                    .into_iter()
                    .find(|entry| entry.name.as_ref() == "outside")
                    .and_then(|entry| entry.hardlink_group),
                _ => None,
            })
            .expect("single-link files retain their identity for later refreshes");
        fs::hard_link(fixture.0.join("outside"), fixture.0.join("branch/alias")).unwrap();
        let scoped = spawn_subtree(
            Arc::clone(initial.anchor.get().unwrap()),
            "branch".into(),
            options(),
        );
        let events = collect(&scoped);
        assert!(matches!(
            events.first(),
            Some(ScanEvent::RootInfo {
                kind: NodeKind::Directory,
                error: None,
                ..
            })
        ));
        let alias = events
            .into_iter()
            .find_map(|event| match event {
                ScanEvent::Entries { parent: 0, entries } => entries
                    .into_iter()
                    .find(|entry| entry.name.as_ref() == "alias"),
                _ => None,
            })
            .expect("scoped scan enumerates its local root");
        assert_eq!(alias.id, 1);
        assert_eq!(alias.hardlink_group, Some(group));
    }

    #[cfg(unix)]
    #[test]
    fn scoped_file_root_reuses_group_after_directory_replacement() {
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside");
        let branch = fixture.0.join("branch");
        fs::write(&outside, b"shared identity").unwrap();
        fs::create_dir(&branch).unwrap();
        let initial = spawn(fixture.0.clone(), options());
        let outside_entry = collect(&initial)
            .into_iter()
            .find_map(|event| match event {
                ScanEvent::Entries { entries, .. } => entries
                    .into_iter()
                    .find(|entry| entry.name.as_ref() == "outside"),
                _ => None,
            })
            .unwrap();
        let group = outside_entry.hardlink_group.unwrap();
        fs::remove_dir(&branch).unwrap();
        fs::hard_link(&outside, &branch).unwrap();

        let scoped = spawn_subtree(
            Arc::clone(initial.anchor.get().unwrap()),
            "branch".into(),
            options(),
        );
        let events = collect(&scoped);
        match events.first().unwrap() {
            ScanEvent::RootInfo {
                file_identity,
                kind,
                sizes,
                error,
            } => {
                assert_eq!(*file_identity, Some(group));
                assert_eq!(*kind, NodeKind::File);
                assert_eq!(*sizes, outside_entry.sizes);
                assert!(error.is_none());
            }
            event => panic!("expected file root information: {event:?}"),
        }
        assert!(matches!(
            events.as_slice(),
            [
                ScanEvent::RootInfo { .. },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false }
            ]
        ));

        let file_root = spawn(outside, options());
        let file_events = collect(&file_root);
        assert!(matches!(
            file_events.first(),
            Some(ScanEvent::RootInfo {
                file_identity: Some(_),
                kind: NodeKind::File,
                sizes,
                error: None,
            }) if *sizes == outside_entry.sizes
        ));
        assert!(matches!(
            file_events.as_slice(),
            [
                ScanEvent::RootInfo { .. },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false }
            ]
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scoped_refresh_retains_original_anchor_after_display_root_replacement() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::create_dir_all(root.join("branch")).unwrap();
        fs::write(root.join("branch/original"), b"anchored object").unwrap();
        let anchor = Arc::new(RootAnchor::open(&root).unwrap());
        fs::rename(&root, fixture.0.join("held-root")).unwrap();
        fs::create_dir_all(root.join("branch")).unwrap();
        fs::write(root.join("branch/replacement"), b"different object").unwrap();
        for backend in [Backend::Std, Backend::Native] {
            let mut scan_options = options();
            scan_options.backend = backend;
            let scoped = spawn_subtree(Arc::clone(&anchor), "branch".into(), scan_options);
            let names: Vec<_> = collect(&scoped)
                .into_iter()
                .filter_map(|event| match event {
                    ScanEvent::Entries { entries, .. } => Some(
                        entries
                            .into_iter()
                            .map(|entry| entry.name)
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .flatten()
                .collect();
            assert_eq!(names, vec![Arc::<OsStr>::from(OsStr::new("original"))]);
        }
    }

    #[test]
    fn concurrent_batches_keep_contiguous_ids_and_already_discovered_parents() {
        let fixture = Fixture::new();
        for branch in 0..8 {
            let top = fixture.0.join(format!("branch-{branch}"));
            fs::create_dir(&top).unwrap();
            for child in 0..3 {
                let directory = top.join(format!("child-{child}"));
                fs::create_dir(&directory).unwrap();
                fs::write(directory.join("payload"), b"measured bytes").unwrap();
            }
        }
        let mut deep = fixture.0.join("deep");
        for _ in 0..12 {
            fs::create_dir(&deep).unwrap();
            fs::write(deep.join("payload"), b"deep payload").unwrap();
            deep = deep.join("next");
        }

        let handle = spawn(fixture.0.clone(), options());
        let events = collect(&handle);
        let mut depths = HashMap::from([(0, 0)]);
        let mut expected_id = 1;
        let mut completed = std::collections::HashSet::new();
        let mut directories = 1;
        let mut saw_deep = false;
        for event in events {
            match event {
                ScanEvent::RootInfo { error, .. } => assert!(error.is_none()),
                ScanEvent::Entries { parent, entries } => {
                    let parent_depth = depths[&parent];
                    saw_deep |= parent_depth >= 10;
                    for entry in entries {
                        assert_eq!(entry.id, expected_id);
                        expected_id += 1;
                        assert!(entry.error.is_none());
                        directories += usize::from(entry.kind == NodeKind::Directory);
                        depths.insert(entry.id, parent_depth + 1);
                    }
                }
                ScanEvent::DirectoryDone { id, error } => {
                    assert!(error.is_none());
                    assert!(depths.contains_key(&id));
                    assert!(completed.insert(id), "directories finish exactly once");
                }
                ScanEvent::Finished { cancelled } => assert!(!cancelled),
            }
        }
        assert_eq!(completed.len(), directories);
        assert!(saw_deep);
        assert!(expected_id > 70);
    }

    #[test]
    fn repeated_directories_finish_as_partial_leaves_without_blocking_siblings() {
        let (sender, receiver) = bounded(64);
        let mut emitter = Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::from([(7, 10)]),
        };
        let cancel = AtomicBool::new(false);
        let control = ScanControl::default();
        let metadata = |name: &str, kind, identity| {
            (
                Arc::<OsStr>::from(OsStr::new(name)),
                Ok(EntryMetadata {
                    kind,
                    sizes: Sizes {
                        apparent: 16,
                        allocated: 512,
                    },
                    identity: Some(identity),
                    links: 1,
                }),
            )
        };
        emitter
            .send(
                ScanEvent::RootInfo {
                    file_identity: None,
                    kind: NodeKind::Directory,
                    sizes: Sizes::default(),
                    error: None,
                },
                &cancel,
                false,
            )
            .unwrap();
        let directories = emitter
            .entries(
                &task(0, ""),
                vec![
                    metadata("root-cycle", NodeKind::Directory, (7, 10)),
                    metadata("branch", NodeKind::Directory, (7, 11)),
                    metadata("branch-alias", NodeKind::Directory, (7, 11)),
                    metadata("other-device", NodeKind::Directory, (8, 11)),
                    metadata("file", NodeKind::File, (7, 11)),
                    metadata("link", NodeKind::Symlink, (7, 10)),
                ],
                &cancel,
                &control,
            )
            .unwrap();
        assert_eq!(
            directories.iter().map(|task| task.id).collect::<Vec<_>>(),
            [2, 4],
        );
        let nested = emitter
            .entries(
                &directories[0],
                vec![
                    metadata("self-cycle", NodeKind::Directory, (7, 11)),
                    metadata("ancestor-cycle", NodeKind::Directory, (7, 10)),
                    metadata("ordinary", NodeKind::Directory, (7, 12)),
                    metadata("new-device", NodeKind::Directory, (8, 12)),
                ],
                &cancel,
                &control,
            )
            .unwrap();
        assert_eq!(
            nested.iter().map(|task| task.id).collect::<Vec<_>>(),
            [9, 10]
        );
        assert_eq!(emitter.visited_directories.len(), 5);
        for id in [0, 2, 4, 9, 10] {
            emitter
                .send(ScanEvent::DirectoryDone { id, error: None }, &cancel, false)
                .unwrap();
        }
        emitter
            .send(ScanEvent::Finished { cancelled: false }, &cancel, true)
            .unwrap();
        let mut model = crate::model::Model::new(PathBuf::from("fixture"));
        let mut skipped = Vec::new();
        for event in receiver.try_iter() {
            if let ScanEvent::Entries { entries, .. } = &event {
                for entry in entries.iter().filter(|entry| entry.error.is_some()) {
                    assert_eq!(entry.kind, NodeKind::Directory);
                    assert!(entry.error.as_deref().unwrap().contains("already visited"));
                    skipped.push(entry.id);
                }
            }
            model.apply(event);
        }
        assert_eq!(skipped, [1, 3, 7, 8]);
        for id in skipped {
            assert!(
                model.node(id).complete,
                "a skipped alias must stop spinning"
            );
            assert_eq!(model.node(id).error_count, 1);
        }
        assert!(model.finished);
        assert!(model.node(0).complete);
        assert_eq!(model.node(0).error_count, 4);
        assert_eq!(model.node(5).sizes.apparent, 16);
        assert!(!model.node(5).hardlink_duplicate);
        assert_eq!(model.node(6).kind, NodeKind::Symlink);
    }

    #[test]
    fn directory_visits_do_not_cross_scan_scopes_or_excluded_paths() {
        for excluded in [true, false] {
            let (sender, receiver) = bounded(4);
            let mut emitter = Emitter {
                sender,
                next_id: 1,
                visited_directories: HashSet::from([(1, 1)]),
            };
            let control = ScanControl::default();
            if excluded {
                control.exclude(Path::new("first"));
            }
            let discovered = ["first", "second"]
                .into_iter()
                .map(|name| {
                    (
                        Arc::from(OsStr::new(name)),
                        Ok(EntryMetadata {
                            kind: NodeKind::Directory,
                            sizes: Sizes::default(),
                            identity: Some((1, 2)),
                            links: 2,
                        }),
                    )
                })
                .collect();
            let directories = emitter
                .entries(&task(0, ""), discovered, &AtomicBool::new(false), &control)
                .unwrap();
            assert_eq!(directories.len(), 1);
            assert_eq!(directories[0].id, 1);
            assert_eq!(
                directories[0].path,
                Path::new(if excluded { "second" } else { "first" }),
            );
            let events: Vec<_> = receiver.try_iter().collect();
            assert_eq!(events.len(), if excluded { 1 } else { 2 });
        }
    }

    #[test]
    fn file_identity_batches_preserve_order_without_a_shared_registry() {
        let (sender, receiver) = bounded(0);
        let worker = thread::spawn(move || {
            let mut emitter = Emitter {
                sender,
                next_id: 1,
                visited_directories: HashSet::new(),
            };
            let discovered = [
                ("first", (1, 42)),
                ("alias", (1, 42)),
                ("other-device", (2, 42)),
                ("wide", (u64::MAX, u64::MAX)),
            ]
            .into_iter()
            .map(|(name, identity)| {
                (
                    Arc::from(OsStr::new(name)),
                    Ok(EntryMetadata {
                        kind: NodeKind::File,
                        sizes: Sizes::default(),
                        identity: Some(identity),
                        links: 1,
                    }),
                )
            })
            .collect();
            emitter.entries(
                &task(0, ""),
                discovered,
                &AtomicBool::new(false),
                &ScanControl::default(),
            )
        });
        let event = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(worker.join().unwrap().unwrap().is_empty());
        let ScanEvent::Entries { entries, .. } = event else {
            panic!("expected an entry batch");
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.id, entry.hardlink_group))
                .collect::<Vec<_>>(),
            [
                (1, Some((1, 42))),
                (2, Some((1, 42))),
                (3, Some((2, 42))),
                (4, Some((u64::MAX, u64::MAX)))
            ]
        );
    }

    #[test]
    fn late_hardlink_alias_keeps_group_after_link_count_shrinks() {
        let (sender, receiver) = bounded(2);
        let mut emitter = Emitter {
            sender,
            next_id: 1,
            visited_directories: HashSet::new(),
        };
        let cancel = AtomicBool::new(false);
        let task = DirectoryTask {
            id: 0,
            path: PathBuf::new(),
        };
        let sizes = Sizes {
            apparent: 8192,
            allocated: 8192,
        };
        for (name, links) in [("original", 2), ("survivor", 1)] {
            emitter
                .entries(
                    &task,
                    vec![(
                        Arc::from(OsStr::new(name)),
                        Ok(EntryMetadata {
                            kind: NodeKind::File,
                            sizes,
                            identity: Some((1, 42)),
                            links,
                        }),
                    )],
                    &cancel,
                    &ScanControl::default(),
                )
                .unwrap();
        }
        for expected_id in 1..=2 {
            let ScanEvent::Entries { entries, .. } = receiver.recv().unwrap() else {
                panic!("expected an entry batch");
            };
            assert_eq!(entries.len(), 1);
            let entry = &entries[0];
            assert_eq!(entry.id, expected_id);
            assert_eq!(entry.hardlink_group, Some((1, 42)));
            assert_eq!(entry.sizes, sizes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn regular_hardlinks_are_charged_once_and_symlink_cycles_are_leaves() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let fixture = Fixture::new();
        let original = fixture.0.join("original");
        fs::write(&original, vec![7u8; 8192]).unwrap();
        fs::hard_link(&original, fixture.0.join("linked")).unwrap();
        symlink(".", fixture.0.join("cycle")).unwrap();

        let original_metadata = fs::symlink_metadata(&original).unwrap();
        let cycle_metadata = fs::symlink_metadata(fixture.0.join("cycle")).unwrap();
        let root_metadata = fs::symlink_metadata(&fixture.0).unwrap();
        let expected_apparent =
            root_metadata.len() + original_metadata.len() + cycle_metadata.len();
        let expected_allocated =
            (root_metadata.blocks() + original_metadata.blocks() + cycle_metadata.blocks()) * 512;

        let handle = spawn(fixture.0.clone(), options());
        let mut model = crate::model::Model::new(fixture.0.clone());
        let mut hardlink_group = None;
        let mut linked_entries = 0;
        let mut entry_count = 0;
        let mut directory_done_count = 0;
        for event in collect(&handle) {
            match &event {
                ScanEvent::RootInfo { error, .. } => assert!(error.is_none()),
                ScanEvent::Entries { parent, entries } => {
                    assert_eq!(*parent, 0, "the cycle symlink must not be enumerated");
                    for entry in entries {
                        entry_count += 1;
                        if entry.kind == NodeKind::File {
                            linked_entries += 1;
                            assert_eq!(entry.sizes.apparent, original_metadata.len());
                            assert_eq!(entry.sizes.allocated, original_metadata.blocks() * 512);
                            let group = entry.hardlink_group.expect("linked file has a group");
                            if let Some(first) = hardlink_group {
                                assert_eq!(group, first);
                            } else {
                                hardlink_group = Some(group);
                            }
                        }
                        assert!(entry.error.is_none());
                        if entry.name.as_ref() == "cycle" {
                            assert_eq!(entry.kind, NodeKind::Symlink);
                        }
                    }
                }
                ScanEvent::DirectoryDone { error, .. } => {
                    assert!(error.is_none());
                    directory_done_count += 1;
                }
                ScanEvent::Finished { cancelled } => assert!(!cancelled),
            }
            model.apply(event);
        }
        assert_eq!(entry_count, 3);
        assert_eq!(
            model
                .children(0)
                .filter(|&id| model.node(id).hardlink_duplicate)
                .count(),
            1
        );
        assert_eq!(linked_entries, 2);
        assert_eq!(directory_done_count, 1);
        assert_eq!(model.node(0).sizes.apparent, expected_apparent);
        assert_eq!(model.node(0).sizes.allocated, expected_allocated);
    }

    #[test]
    fn cancellation_releases_a_full_event_channel() {
        let fixture = Fixture::new();
        for index in 0..300 {
            fs::write(fixture.0.join(format!("entry-{index}")), b"x").unwrap();
        }
        let mut scan_options = options();
        scan_options.batch_size = 1;
        let handle = spawn(fixture.0.clone(), scan_options);
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.receiver.len() < EVENT_CAPACITY {
            assert!(
                Instant::now() < deadline,
                "scanner did not fill its bounded channel"
            );
            assert!(!handle.is_finished());
            thread::sleep(Duration::from_millis(2));
        }
        handle.cancel();
        let events = collect(&handle);
        assert!(matches!(
            events.last(),
            Some(ScanEvent::Finished { cancelled: true })
        ));
    }

    #[test]
    fn inaccessible_root_reports_an_error_and_terminal_event() {
        let fixture = Fixture::new();
        let handle = spawn(fixture.0.join("missing"), options());
        let events = collect(&handle);
        assert!(matches!(
            events.first(),
            Some(ScanEvent::RootInfo {
                kind: NodeKind::Unknown,
                error: Some(_),
                ..
            })
        ));
        assert!(matches!(
            events.last(),
            Some(ScanEvent::Finished { cancelled: false })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trailing_symlink_roots_are_reported_as_leaves_by_the_scanner() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("real")).unwrap();
        fs::write(fixture.0.join("real/payload"), b"must not traverse").unwrap();
        symlink("real", fixture.0.join("alias")).unwrap();
        for backend in [Backend::Auto, Backend::Native, Backend::Std] {
            for suffix in ["alias/", "alias/."] {
                let mut scan_options = options();
                scan_options.backend = backend;
                let handle = spawn(fixture.0.join(suffix), scan_options);
                let events = collect(&handle);
                assert!(matches!(
                    events.first(),
                    Some(ScanEvent::RootInfo {
                        kind: NodeKind::Symlink,
                        error: None,
                        ..
                    })
                ));
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ScanEvent::Entries { .. }))
                );
                assert!(handle.anchor.get().is_some());
            }
        }
    }
}
