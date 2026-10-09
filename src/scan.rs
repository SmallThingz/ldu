use crate::backend::{self, Backend, EntryMetadata, RootAnchor};
use crate::types::{Entry, NodeId, NodeKind, ScanEvent, Sizes};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

const EVENT_CAPACITY: usize = 128;
const SEND_INTERVAL: Duration = Duration::from_millis(20);

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
    hardlinks: Arc<HardlinkRegistry>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.control.cancel_active();
        self.control.wake();
    }

    /// Stop this path and all its descendants without interrupting other branches.
    /// Exclusions remain in effect for this handle, including tasks not yet started.
    pub fn exclude(&self, relative: &Path) {
        self.control.exclude(relative);
    }

    /// Prefer this discovered directory and its descendants on the next free
    /// worker. The newest focus replaces the previous request.
    pub fn prioritize(&self, relative: &Path) {
        self.control.prioritize(relative);
    }

    /// Restore breadth-first scheduling after leaving a focused directory.
    pub fn clear_priority(&self) {
        self.control.clear_priority();
    }

    pub fn hardlinks(&self) -> Arc<HardlinkRegistry> {
        Arc::clone(&self.hardlinks)
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

struct FinishedGuard(Arc<AtomicBool>);

impl Drop for FinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct ScanControl {
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
        let mut node = self;
        for component in path
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
        {
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
        self.has_exclusions.load(Ordering::Acquire) && self.excludes(&parent.join(name))
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum TaskState {
    Pending,
    Active,
    Complete,
}

struct ScheduledDirectory {
    task: Option<DirectoryTask>,
    parent: Option<NodeId>,
    depth: usize,
    state: TaskState,
    pending: usize,
    pending_children: BTreeSet<NodeId>,
}

#[derive(Default)]
struct Scheduler {
    directories: HashMap<NodeId, ScheduledDirectory>,
    paths: HashMap<PathBuf, NodeId>,
    pending_depths: BTreeMap<usize, BTreeSet<NodeId>>,
    active_depths: BTreeMap<usize, usize>,
    focus: Option<PathBuf>,
    active: usize,
}

impl Scheduler {
    fn insert(&mut self, task: DirectoryTask, parent: Option<NodeId>) {
        let id = task.id;
        let depth = parent.map_or(0, |parent| self.directories[&parent].depth + 1);
        self.paths.insert(task.path.clone(), id);
        self.directories.insert(
            id,
            ScheduledDirectory {
                task: Some(task),
                parent,
                depth,
                state: TaskState::Pending,
                pending: 0,
                pending_children: BTreeSet::new(),
            },
        );
        self.pending_depths.entry(depth).or_default().insert(id);
        let mut current = id;
        loop {
            let directory = self
                .directories
                .get_mut(&current)
                .expect("registered directory");
            directory.pending += 1;
            let Some(parent) = directory.parent else {
                break;
            };
            self.directories
                .get_mut(&parent)
                .expect("registered parent")
                .pending_children
                .insert(current);
            current = parent;
        }
    }

    fn focused(&self) -> Option<NodeId> {
        let mut current = *self.paths.get(self.focus.as_ref()?)?;
        loop {
            let directory = &self.directories[&current];
            if directory.pending == 0 {
                return None;
            }
            if directory.state == TaskState::Pending {
                return Some(current);
            }
            current = *directory.pending_children.first()?;
        }
    }

    fn take(&mut self) -> Option<DirectoryTask> {
        let id = if let Some(focused) = self.focused() {
            focused
        } else {
            let (&depth, pending) = self.pending_depths.first_key_value()?;
            if self
                .active_depths
                .first_key_value()
                .is_some_and(|(&active_depth, _)| active_depth < depth)
            {
                return None;
            }
            *pending.first()?
        };
        let depth = self.directories[&id].depth;
        let pending = self.pending_depths.get_mut(&depth).expect("pending depth");
        pending.remove(&id);
        if pending.is_empty() {
            self.pending_depths.remove(&depth);
        }
        let mut current = id;
        loop {
            let directory = self
                .directories
                .get_mut(&current)
                .expect("registered directory");
            directory.pending -= 1;
            let remaining = directory.pending;
            let Some(parent) = directory.parent else {
                break;
            };
            if remaining == 0 {
                self.directories
                    .get_mut(&parent)
                    .expect("registered parent")
                    .pending_children
                    .remove(&current);
            }
            current = parent;
        }
        let directory = self.directories.get_mut(&id).expect("registered directory");
        directory.state = TaskState::Active;
        *self.active_depths.entry(depth).or_default() += 1;
        self.active += 1;
        directory.task.take()
    }

    fn complete(&mut self, id: NodeId) {
        let directory = self.directories.get_mut(&id).expect("active directory");
        directory.state = TaskState::Complete;
        let active = self
            .active_depths
            .get_mut(&directory.depth)
            .expect("active depth");
        *active -= 1;
        if *active == 0 {
            self.active_depths.remove(&directory.depth);
        }
        self.active -= 1;
    }

    fn is_empty(&self) -> bool {
        self.active == 0 && self.pending_depths.is_empty()
    }
}

enum WorkerMessage {
    Discovered {
        parent: NodeId,
        tasks: Vec<DirectoryTask>,
    },
    Complete(NodeId),
}

/// File identities are shared across scoped scans. Group IDs are opaque and
/// independent of each handle's local node IDs.
pub struct HardlinkRegistry {
    state: Mutex<HardlinkState>,
}

struct HardlinkState {
    next_group: NodeId,
    groups: HashMap<(u64, u64), NodeId>,
}

impl Default for HardlinkRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(HardlinkState {
                next_group: 1,
                groups: HashMap::new(),
            }),
        }
    }
}

impl HardlinkRegistry {
    fn group(&self, identity: (u64, u64)) -> io::Result<(NodeId, bool)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(&group) = state.groups.get(&identity) {
            return Ok((group, true));
        }
        let group = state.next_group;
        state.next_group = group.checked_add(1).ok_or_else(|| {
            io::Error::other("too many hardlink groups for the scan's identifiers")
        })?;
        // Record single-link files too. A later refresh may discover a new alias
        // of an unchanged file outside the refreshed branch.
        state.groups.insert(identity, group);
        Ok((group, false))
    }
}

struct Emitter {
    sender: Sender<ScanEvent>,
    next_id: NodeId,
    hardlinks: Arc<HardlinkRegistry>,
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
        discovered: Vec<(std::ffi::OsString, io::Result<EntryMetadata>)>,
        cancel: &AtomicBool,
        control: &ScanControl,
    ) -> io::Result<Vec<DirectoryTask>> {
        let mut entries = Vec::with_capacity(discovered.len());
        let mut directories = Vec::new();
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
            let (kind, sizes, hardlink_group, hardlink_duplicate, error) = match metadata {
                Ok(metadata) => {
                    let (group, duplicate) = if metadata.kind == NodeKind::File {
                        match metadata.identity.filter(|_| metadata.links != 0) {
                            Some(identity) => {
                                let (group, duplicate) = self.hardlinks.group(identity)?;
                                (Some(group), duplicate)
                            }
                            None => (None, false),
                        }
                    } else {
                        (None, false)
                    };
                    (metadata.kind, metadata.sizes, group, duplicate, None)
                }
                Err(error) => (
                    NodeKind::Unknown,
                    Sizes::default(),
                    None,
                    false,
                    Some(error.to_string()),
                ),
            };
            if kind == NodeKind::Directory {
                directories.push(DirectoryTask {
                    id,
                    path: task.path.join(&name),
                });
            }
            entries.push(Entry {
                id,
                name,
                kind,
                sizes,
                hardlink_duplicate,
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
    spawn_with_root(
        ScanRoot::Open(root),
        options,
        Arc::new(HardlinkRegistry::default()),
    )
}

/// Scan one directory beneath the retained original root. Events use local root
/// ID zero and contiguous local entry IDs; the model remaps each scope on intake.
pub fn spawn_subtree(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    options: ScanOptions,
    hardlinks: Arc<HardlinkRegistry>,
) -> ScanHandle {
    spawn_with_root(ScanRoot::Anchored { anchor, relative }, options, hardlinks)
}

fn spawn_with_root(
    root: ScanRoot,
    mut options: ScanOptions,
    hardlinks: Arc<HardlinkRegistry>,
) -> ScanHandle {
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
    let thread_hardlinks = Arc::clone(&hardlinks);
    let control = Arc::new(ScanControl::default());
    let thread_control = Arc::clone(&control);
    let thread_anchor = Arc::clone(&anchor);
    let thread_cancel = Arc::clone(&cancel);
    let thread_finished = Arc::clone(&finished);
    let failure_sender = sender.clone();
    let spawned = thread::Builder::new()
        .name("ldu-scan".into())
        .spawn(move || {
            let _finished = FinishedGuard(thread_finished);
            run(
                root,
                options,
                sender,
                thread_cancel,
                thread_anchor,
                thread_control,
                thread_hardlinks,
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
        hardlinks,
    }
}

fn run(
    root: ScanRoot,
    options: ScanOptions,
    sender: Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
    anchor_slot: Arc<OnceLock<Arc<RootAnchor>>>,
    control: Arc<ScanControl>,
    hardlinks: Arc<HardlinkRegistry>,
) {
    let emitter = Arc::new(Mutex::new(Emitter {
        sender,
        next_id: 1,
        hardlinks: Arc::clone(&hardlinks),
    }));

    if !cancel.load(Ordering::Acquire) {
        let (relative, metadata) = match root {
            ScanRoot::Open(root) => {
                let metadata = RootAnchor::open(&root).and_then(|anchor| {
                    let metadata = anchor.metadata()?;
                    let _ = anchor_slot.set(Arc::new(anchor));
                    Ok(metadata)
                });
                (PathBuf::new(), metadata)
            }
            ScanRoot::Anchored { anchor, relative } => {
                let metadata = anchored_metadata(&anchor, &relative);
                (relative, metadata)
            }
        };
        let metadata = metadata.and_then(|metadata| {
            let file_identity = if metadata.kind == NodeKind::File {
                metadata
                    .identity
                    .filter(|_| metadata.links != 0)
                    .map(|identity| hardlinks.group(identity).map(|(group, _)| group))
                    .transpose()?
            } else {
                None
            };
            Ok((metadata, file_identity))
        });
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
        let root_sent = emitter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .is_ok();

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
        while scheduler.active < options.threads && !cancel.load(Ordering::Acquire) {
            let Some(task) = scheduler.take() else { break };
            let worker_anchor = Arc::clone(&anchor);
            let worker_emitter = Arc::clone(&emitter);
            let worker_cancel = Arc::clone(&cancel);
            let worker_control = Arc::clone(&control);
            let worker_messages = messages.clone();
            let worker_options = options.clone();
            pool.spawn(move || {
                let id = task.id;
                scan_directory_emit(
                    &task,
                    &worker_anchor,
                    &worker_options,
                    &worker_emitter,
                    &worker_cancel,
                    &worker_control,
                    |tasks| {
                        if tasks.is_empty() {
                            return Ok(());
                        }
                        send_worker(
                            &worker_messages,
                            WorkerMessage::Discovered { parent: id, tasks },
                            &worker_cancel,
                        )
                    },
                );
                let _ = send_worker(
                    &worker_messages,
                    WorkerMessage::Complete(id),
                    &worker_cancel,
                );
            });
        }
        if scheduler.is_empty() || cancel.load(Ordering::Acquire) {
            break;
        }
        crossbeam_channel::select! {
            recv(receiver) -> message => match message {
                Ok(WorkerMessage::Discovered { parent, tasks }) => {
                    for task in tasks {
                        scheduler.insert(task, Some(parent));
                    }
                }
                Ok(WorkerMessage::Complete(id)) => scheduler.complete(id),
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

fn anchored_metadata(anchor: &RootAnchor, relative: &Path) -> io::Result<EntryMetadata> {
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
    if relative.as_os_str().is_empty() {
        return anchor.metadata();
    }
    #[cfg(target_os = "linux")]
    {
        let parent = anchor.open_parent(relative)?;
        backend::MetadataReader::Native(parent).metadata(relative.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "scan path has no final entry")
        })?)
    }
    #[cfg(not(target_os = "linux"))]
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
        |directories| {
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
    mut emit_directories: impl FnMut(Vec<DirectoryTask>) -> io::Result<()>,
) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let active = control.activate(task, cancel);
    let result = if active.cancel.load(Ordering::Acquire) {
        Ok(())
    } else {
        backend::enumerate_anchored(
            anchor,
            &task.path,
            options.backend,
            options.buffer_size,
            options.batch_size,
            &active.cancel,
            |names, reader| {
                if cancel.load(Ordering::Acquire) || active.cancel.load(Ordering::Acquire) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
                }
                // Nested Rayon jobs use the same bounded pool, including for a single wide directory.
                let discovered = names
                    .into_par_iter()
                    .map(|name| {
                        let metadata = if cancel.load(Ordering::Acquire)
                            || active.cancel.load(Ordering::Acquire)
                            || control.excludes_child(&task.path, &name)
                        {
                            Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"))
                        } else {
                            reader.metadata(&name)
                        };
                        (name, metadata)
                    })
                    .collect();
                let directories = emitter
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .entries(task, discovered, cancel, control)?;
                emit_directories(directories)
            },
        )
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
    fn scheduler_preserves_default_breadth_barriers_for_streamed_discoveries() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        assert_eq!(scheduler.take().unwrap().id, 0);
        scheduler.insert(task(1, "first"), Some(0));
        scheduler.insert(task(2, "second"), Some(0));
        assert!(
            scheduler.take().is_none(),
            "children wait for the root barrier"
        );
        scheduler.complete(0);
        assert_eq!(scheduler.take().unwrap().id, 1);
        assert_eq!(scheduler.take().unwrap().id, 2);
        scheduler.insert(task(3, "first/deep"), Some(1));
        assert!(scheduler.take().is_none());
        scheduler.complete(1);
        assert!(
            scheduler.take().is_none(),
            "all peers must finish the depth"
        );
        scheduler.complete(2);
        assert_eq!(scheduler.take().unwrap().id, 3);
        scheduler.complete(3);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn focused_pending_directory_bypasses_an_active_shallower_barrier() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        assert_eq!(scheduler.take().unwrap().id, 0);
        scheduler.insert(task(1, "earlier"), Some(0));
        scheduler.insert(task(2, "focused"), Some(0));
        assert!(scheduler.take().is_none());
        scheduler.focus = Some("focused".into());
        assert_eq!(scheduler.take().unwrap().id, 2);
        scheduler.insert(task(3, "focused/deep"), Some(2));
        assert_eq!(scheduler.focused(), Some(3));
        scheduler.complete(2);
        assert_eq!(scheduler.take().unwrap().id, 3);
        assert_eq!(
            scheduler.active, 2,
            "focus can use a second fixed worker while root is active"
        );
        scheduler.complete(3);
        assert!(
            scheduler.take().is_none(),
            "unrelated branch keeps its default barrier"
        );
        scheduler.complete(0);
        assert_eq!(scheduler.take().unwrap().id, 1);
    }

    #[test]
    fn newest_focus_replaces_previous_branch_priority() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "old"), Some(0));
        scheduler.insert(task(2, "new"), Some(0));
        scheduler.complete(0);
        scheduler.focus = Some("old".into());
        scheduler.focus = Some("new".into());
        assert_eq!(scheduler.take().unwrap().id, 2);
        scheduler.insert(task(3, "new/deep"), Some(2));
        scheduler.focus = Some("old".into());
        assert_eq!(scheduler.take().unwrap().id, 1);
        assert!(
            scheduler.take().is_none(),
            "old focus descendants no longer bypass barriers"
        );
        scheduler.complete(1);
        assert!(scheduler.take().is_none());
        scheduler.complete(2);
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
    fn clearing_latest_focus_restores_the_breadth_first_barrier() {
        let mut scheduler = Scheduler::default();
        scheduler.insert(task(0, ""), None);
        scheduler.take().unwrap();
        scheduler.insert(task(1, "focused"), Some(0));
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
        assert!(scheduler.take().is_none());
        scheduler.complete(0);
        assert_eq!(scheduler.take().unwrap().id, 1);

        control.clear_priority();
        control.prioritize(Path::new("focused"));
        assert_eq!(
            control.take_focus(),
            Some(FocusRequest::Prioritize(PathBuf::from("focused")))
        );
        assert_eq!(control.take_focus(), None);
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
                drop(excluded);
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
            hardlinks: Arc::new(HardlinkRegistry::default()),
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
                        "removed".into(),
                        Err(io::Error::from(io::ErrorKind::NotFound)),
                    ),
                    ("survivor".into(), Ok(metadata)),
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
        assert_eq!(entries[0].name, "survivor");
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
            hardlinks: Arc::new(HardlinkRegistry::default()),
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
        assert_eq!(entries[0].name, "payload");
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
            hardlinks: Arc::new(HardlinkRegistry::default()),
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
            initial.hardlinks(),
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

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn fallback_root_refresh_observes_symlink_replacement_and_missing_root() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(fixture.0.join("outside")).unwrap();
        fs::write(fixture.0.join("outside/payload"), b"must not traverse").unwrap();
        let initial = spawn(root.clone(), options());
        collect(&initial);
        let anchor = Arc::clone(initial.anchor.get().unwrap());
        fs::remove_dir(&root).unwrap();
        symlink("outside", &root).unwrap();

        let scoped = spawn_subtree(
            Arc::clone(&anchor),
            PathBuf::new(),
            options(),
            initial.hardlinks(),
        );
        assert!(matches!(
            collect(&scoped).as_slice(),
            [
                ScanEvent::RootInfo {
                    kind: NodeKind::Symlink,
                    error: None,
                    ..
                },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false }
            ]
        ));

        fs::remove_file(&root).unwrap();
        let missing = spawn_subtree(anchor, PathBuf::new(), options(), initial.hardlinks());
        assert!(matches!(
            collect(&missing).as_slice(),
            [
                ScanEvent::RootInfo {
                    kind: NodeKind::Unknown,
                    error: Some(_),
                    ..
                },
                ScanEvent::DirectoryDone { id: 0, error: None },
                ScanEvent::Finished { cancelled: false }
            ]
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
                    .find(|entry| entry.name == "outside")
                    .and_then(|entry| entry.hardlink_group),
                _ => None,
            })
            .expect("single-link files retain their identity for later refreshes");
        fs::hard_link(fixture.0.join("outside"), fixture.0.join("branch/alias")).unwrap();
        let scoped = spawn_subtree(
            Arc::clone(initial.anchor.get().unwrap()),
            "branch".into(),
            options(),
            initial.hardlinks(),
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
                ScanEvent::Entries { parent: 0, entries } => {
                    entries.into_iter().find(|entry| entry.name == "alias")
                }
                _ => None,
            })
            .expect("scoped scan enumerates its local root");
        assert_eq!(alias.id, 1);
        assert_eq!(alias.hardlink_group, Some(group));
        assert!(alias.hardlink_duplicate);
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
                ScanEvent::Entries { entries, .. } => {
                    entries.into_iter().find(|entry| entry.name == "outside")
                }
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
            initial.hardlinks(),
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
            let scoped = spawn_subtree(
                Arc::clone(&anchor),
                "branch".into(),
                scan_options,
                Arc::new(HardlinkRegistry::default()),
            );
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
            assert_eq!(names, vec![std::ffi::OsString::from("original")]);
        }
    }

    #[test]
    fn batches_have_contiguous_ids_and_strict_breadth_first_depths() {
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
        let mut last_parent_depth = 0;
        let mut saw_deep = false;
        for event in events {
            match event {
                ScanEvent::RootInfo { error, .. } => assert!(error.is_none()),
                ScanEvent::Entries { parent, entries } => {
                    let parent_depth = depths[&parent];
                    assert!(parent_depth >= last_parent_depth);
                    last_parent_depth = parent_depth;
                    saw_deep |= parent_depth >= 10;
                    for entry in entries {
                        assert_eq!(entry.id, expected_id);
                        expected_id += 1;
                        assert!(entry.error.is_none());
                        depths.insert(entry.id, parent_depth + 1);
                    }
                }
                ScanEvent::DirectoryDone { error, .. } => assert!(error.is_none()),
                ScanEvent::Finished { cancelled } => assert!(!cancelled),
            }
        }
        assert!(saw_deep);
        assert!(expected_id > 70);
    }

    #[test]
    fn late_hardlink_alias_keeps_group_after_link_count_shrinks() {
        let (sender, receiver) = bounded(2);
        let mut emitter = Emitter {
            sender,
            next_id: 1,
            hardlinks: Arc::new(HardlinkRegistry::default()),
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
                        name.into(),
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
            assert_eq!(entry.hardlink_group, Some(1));
            assert_eq!(entry.hardlink_duplicate, expected_id != 1);
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
        let mut apparent = 0;
        let mut allocated = 0;
        let mut duplicate_count = 0;
        let mut hardlink_group = None;
        let mut linked_entries = 0;
        let mut entry_count = 0;
        let mut directory_done_count = 0;
        for event in collect(&handle) {
            match event {
                ScanEvent::RootInfo { sizes, error, .. } => {
                    assert!(error.is_none());
                    apparent += sizes.apparent;
                    allocated += sizes.allocated;
                }
                ScanEvent::Entries { parent, entries } => {
                    assert_eq!(parent, 0, "the cycle symlink must not be enumerated");
                    for entry in entries {
                        entry_count += 1;
                        if !entry.hardlink_duplicate {
                            apparent += entry.sizes.apparent;
                            allocated += entry.sizes.allocated;
                        }
                        duplicate_count += usize::from(entry.hardlink_duplicate);
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
                        if entry.name == "cycle" {
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
        }
        assert_eq!(entry_count, 3);
        assert_eq!(duplicate_count, 1);
        assert_eq!(linked_entries, 2);
        assert_eq!(directory_done_count, 1);
        assert_eq!(apparent, expected_apparent);
        assert_eq!(allocated, expected_allocated);
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
