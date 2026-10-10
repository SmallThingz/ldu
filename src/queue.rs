//! Confirmed deletions are serialized without making the input thread wait.

use crate::backend::RootAnchor;
use crate::delete::{self, ConfirmedTarget, DeleteHandle, DeleteProgress, DeleteResult};
use crate::retire::Retirer;
use crate::types::NodeId;
use crossbeam_channel::TryRecvError;
use im::{HashMap, OrdMap, Vector};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type QueueId = u64;
type Paths = HashMap<usize, OrdMap<Arc<[OsString]>, QueueId>>;

#[derive(Clone, Default)]
pub struct QueueMarks {
    pending: HashMap<NodeId, QueueId>,
    superseded: HashMap<NodeId, QueueId>,
    pub active: Option<NodeId>,
}

#[derive(Clone, Copy, Default)]
pub struct DeleteState {
    latest: Option<QueueId>,
    cutoff: QueueId,
    active: bool,
}

impl QueueMarks {
    pub fn is_empty(&self) -> bool {
        self.active.is_none() && self.pending.is_empty()
    }

    pub fn include(&self, node: NodeId, mut state: DeleteState) -> DeleteState {
        state.latest = state.latest.max(self.pending.get(&node).copied());
        state.cutoff = state
            .cutoff
            .max(self.superseded.get(&node).copied().unwrap_or(0));
        state.active |= self.active == Some(node);
        state
    }
}

impl DeleteState {
    pub fn is_deleting(self) -> bool {
        self.active || self.latest.is_some_and(|id| id >= self.cutoff)
    }
}

#[derive(Clone, Debug)]
pub struct QueueEntry {
    pub id: QueueId,
    pub node: NodeId,
    pub anchor: Arc<RootAnchor>,
    pub relative: PathBuf,
    target: Option<ConfirmedTarget>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnqueueOutcome {
    pub id: QueueId,
    pub inserted: bool,
}

pub struct QueueCompletion {
    pub entry: QueueEntry,
    pub result: DeleteResult,
}

#[derive(Clone)]
struct PendingNode {
    entry: Arc<QueueEntry>,
    previous: Option<QueueId>,
    next: Option<QueueId>,
}

/// Stable-ID linked order. Persistent map nodes avoid whole-table resizing;
/// moving and removing a row touches a fixed number of links.
#[derive(Default)]
pub struct PendingQueue {
    entries: HashMap<QueueId, PendingNode>,
    targets: HashMap<NodeId, QueueId>,
    first: Option<QueueId>,
    last: Option<QueueId>,
}

impl PendingQueue {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn front(&self) -> Option<&QueueEntry> {
        self.first.and_then(|id| self.get(id))
    }

    pub fn back(&self) -> Option<&QueueEntry> {
        self.last.and_then(|id| self.get(id))
    }

    pub fn get(&self, id: QueueId) -> Option<&QueueEntry> {
        self.entries.get(&id).map(|node| node.entry.as_ref())
    }

    pub fn iter(&self) -> PendingIter<'_> {
        PendingIter {
            pending: self,
            next: self.first,
            remaining: self.len(),
        }
    }

    fn push_back(&mut self, entry: QueueEntry) {
        let id = entry.id;
        self.targets.insert(entry.node, id);
        self.entries.insert(
            id,
            PendingNode {
                entry: Arc::new(entry),
                previous: self.last,
                next: None,
            },
        );
        if let Some(last) = self.last {
            self.entries
                .get_mut(&last)
                .expect("pending tail exists")
                .next = Some(id);
        } else {
            self.first = Some(id);
        }
        self.last = Some(id);
    }

    fn remove(&mut self, id: QueueId) -> Option<QueueEntry> {
        let node = self.entries.remove(&id)?;
        if self.targets.get(&node.entry.node) == Some(&id) {
            self.targets.remove(&node.entry.node);
        }
        if let Some(previous) = node.previous {
            self.entries
                .get_mut(&previous)
                .expect("pending previous exists")
                .next = node.next;
        } else {
            self.first = node.next;
        }
        if let Some(next) = node.next {
            self.entries
                .get_mut(&next)
                .expect("pending next exists")
                .previous = node.previous;
        } else {
            self.last = node.previous;
        }
        Some(Arc::unwrap_or_clone(node.entry))
    }

    fn move_adjacent(&mut self, id: QueueId, direction: isize) -> bool {
        let Some(node) = self.entries.get(&id) else {
            return false;
        };
        let (left, right) = match direction {
            -1 => match node.previous {
                Some(previous) => (previous, id),
                None => return false,
            },
            1 => match node.next {
                Some(next) => (id, next),
                None => return false,
            },
            _ => return false,
        };
        let before = self.entries[&left].previous;
        let after = self.entries[&right].next;
        if let Some(before) = before {
            self.entries
                .get_mut(&before)
                .expect("pending predecessor exists")
                .next = Some(right);
        } else {
            self.first = Some(right);
        }
        if let Some(after) = after {
            self.entries
                .get_mut(&after)
                .expect("pending successor exists")
                .previous = Some(left);
        } else {
            self.last = Some(left);
        }
        let right_node = self.entries.get_mut(&right).expect("pending right exists");
        right_node.previous = before;
        right_node.next = Some(left);
        let left_node = self.entries.get_mut(&left).expect("pending left exists");
        left_node.previous = Some(right);
        left_node.next = after;
        true
    }
}

pub struct PendingIter<'a> {
    pending: &'a PendingQueue,
    next: Option<QueueId>,
    remaining: usize,
}

impl<'a> Iterator for PendingIter<'a> {
    type Item = &'a QueueEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.pending.entries.get(&self.next?)?;
        self.next = node.next;
        self.remaining = self.remaining.saturating_sub(1);
        Some(node.entry.as_ref())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for PendingIter<'_> {}

impl<'a> IntoIterator for &'a PendingQueue {
    type Item = &'a QueueEntry;
    type IntoIter = PendingIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

struct ActiveDelete {
    entry: QueueEntry,
    worker: Worker,
    cancel_requested: bool,
}

enum Worker {
    Running(DeleteHandle),
    #[cfg(test)]
    Fake(FakeWorker),
}

impl Worker {
    fn is_finished(&self) -> bool {
        match self {
            Self::Running(handle) => handle.is_finished(),
            #[cfg(test)]
            Self::Fake(handle) => handle.finished.load(std::sync::atomic::Ordering::Acquire),
        }
    }

    fn receive(&self) -> Result<DeleteResult, TryRecvError> {
        match self {
            Self::Running(handle) => handle.receiver.try_recv(),
            #[cfg(test)]
            Self::Fake(handle) => handle.receiver.try_recv(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Running(handle) => handle.cancel(),
            #[cfg(test)]
            Self::Fake(handle) => handle
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release),
        }
    }

    fn progress(&self) -> DeleteProgress {
        match self {
            Self::Running(handle) => handle.progress(),
            #[cfg(test)]
            Self::Fake(_) => DeleteProgress::default(),
        }
    }
}

#[derive(Clone)]
struct Prune {
    anchor: usize,
    node: NodeId,
    ancestor: QueueId,
    prefix: Arc<[OsString]>,
    // A cheap persistent snapshot fixes which already-confirmed jobs were
    // superseded. Later confirmations must survive an older cancelled ancestor.
    paths: OrdMap<Arc<[OsString]>, QueueId>,
}

enum Retired {
    Marks {
        _marks: QueueMarks,
    },
    Entry {
        _entry: QueueEntry,
    },
    Prune {
        _prune: Prune,
    },
    Pending {
        _pending: PendingQueue,
        _paths: Paths,
        _superseded: Paths,
        _superseded_nodes: HashMap<NodeId, QueueId>,
        _pruning: Vector<Prune>,
    },
}

/// The caller excludes new targets, pumps bounded ancestor supersession, and
/// reconciles each completion with the model before starting another worker.
pub struct DeleteQueue {
    pending: PendingQueue,
    paths: Paths,
    superseded: Paths,
    superseded_nodes: HashMap<NodeId, QueueId>,
    pruning: Vector<Prune>,
    active: Option<ActiveDelete>,
    paused: bool,
    next_id: QueueId,
    retirer: Retirer<Vec<Retired>>,
    retired: Vec<Retired>,
}

impl DeleteQueue {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            pending: PendingQueue::default(),
            paths: Paths::new(),
            superseded: Paths::new(),
            superseded_nodes: HashMap::new(),
            pruning: Vector::new(),
            active: None,
            paused: false,
            next_id: 0,
            retirer: Retirer::new()?,
            retired: Vec::new(),
        })
    }

    /// Production jobs retain the identity captured before confirmation.
    pub fn enqueue_confirmed(
        &mut self,
        node: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
        target: ConfirmedTarget,
    ) -> EnqueueOutcome {
        self.enqueue_inner(node, anchor, relative, Some(target))
    }

    #[cfg(test)]
    pub fn enqueue(
        &mut self,
        node: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
    ) -> EnqueueOutcome {
        self.enqueue_inner(node, anchor, relative, None)
    }

    fn enqueue_inner(
        &mut self,
        node: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
        target: Option<ConfirmedTarget>,
    ) -> EnqueueOutcome {
        if let Some(active) = self
            .active
            .as_ref()
            .filter(|active| !active.cancel_requested && covers(&active.entry, &anchor, &relative))
        {
            let active = &active.entry;
            return EnqueueOutcome {
                id: active.id,
                inserted: false,
            };
        }
        let root = anchor_id(&anchor);
        let components = path_components(&relative);
        if let Some(id) = self.covering_pending(root, &components) {
            return EnqueueOutcome {
                id,
                inserted: false,
            };
        }
        let superseded = self
            .first_descendant(root, &components, None)
            .map(|_| self.paths[&root].clone());
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("deletion queue ID exhausted");
        self.paths
            .entry(root)
            .or_default()
            .insert(components.clone(), id);
        self.pending.push_back(QueueEntry {
            id,
            node,
            anchor,
            relative,
            target,
        });
        if let Some(paths) = superseded {
            self.superseded_nodes.insert(node, id);
            self.superseded
                .entry(root)
                .or_default()
                .insert(components.clone(), id);
            self.pruning.push_back(Prune {
                anchor: root,
                node,
                ancestor: id,
                prefix: components,
                paths,
            });
        }
        EnqueueOutcome { id, inserted: true }
    }

    pub fn sync_marks(&mut self, marks: &mut QueueMarks) {
        let active = self.active().map(|entry| entry.node);
        if marks.active == active
            && marks.pending.ptr_eq(&self.pending.targets)
            && marks.superseded.ptr_eq(&self.superseded_nodes)
        {
            return;
        }
        let previous = std::mem::replace(
            marks,
            QueueMarks {
                pending: self.pending.targets.clone(),
                superseded: self.superseded_nodes.clone(),
                active,
            },
        );
        self.retired.push(Retired::Marks { _marks: previous });
        self.flush_retired();
    }

    pub fn active(&self) -> Option<&QueueEntry> {
        self.active.as_ref().map(|active| &active.entry)
    }

    pub fn pending(&self) -> &PendingQueue {
        &self.pending
    }

    pub fn entry(&self, id: QueueId) -> Option<&QueueEntry> {
        self.active()
            .filter(|entry| entry.id == id)
            .or_else(|| self.pending.get(id))
    }

    pub fn first_id(&self) -> Option<QueueId> {
        self.active().map(|entry| entry.id).or(self.pending.first)
    }

    pub fn last_id(&self) -> Option<QueueId> {
        self.pending
            .last
            .or_else(|| self.active().map(|entry| entry.id))
    }

    pub fn next_id(&self, id: QueueId) -> Option<QueueId> {
        if self.active().is_some_and(|entry| entry.id == id) {
            self.pending.first
        } else {
            self.pending.entries.get(&id).and_then(|node| node.next)
        }
    }

    pub fn previous_id(&self, id: QueueId) -> Option<QueueId> {
        self.pending.entries.get(&id).and_then(|node| {
            node.previous
                .or_else(|| self.active().map(|entry| entry.id))
        })
    }

    pub fn progress(&self) -> Option<DeleteProgress> {
        self.active.as_ref().map(|active| active.worker.progress())
    }

    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.pending.is_empty() || self.is_updating()
    }

    pub fn is_updating(&self) -> bool {
        !self.pruning.is_empty()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn is_cancelling(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.cancel_requested)
    }

    pub fn toggle_pause(&mut self) -> bool {
        self.paused = !self.paused;
        self.paused
    }

    pub fn cancel_active(&mut self) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        active.cancel_requested = true;
        active.worker.cancel();
        true
    }

    pub fn remove_pending(&mut self, id: QueueId) -> Option<QueueEntry> {
        // The newly confirmed ancestor can be cancelled during pruning, but its
        // already superseded descendants cannot be revived or manipulated.
        if self.is_updating()
            && self.pending.get(id).is_some_and(|entry| {
                self.covering_pending(anchor_id(&entry.anchor), &path_components(&entry.relative))
                    != Some(id)
            })
        {
            return None;
        }
        self.remove_pending_inner(id)
    }

    fn remove_pending_inner(&mut self, id: QueueId) -> Option<QueueEntry> {
        let entry = self.pending.remove(id)?;
        let root = anchor_id(&entry.anchor);
        let path = path_components(&entry.relative);
        if let Some(paths) = self.paths.get_mut(&root) {
            // An older superseded job may have been explicitly reconfirmed at
            // the same path with a fresh ID before its old prune completed.
            if paths.get(&path) == Some(&id) {
                paths.remove(&path);
            }
            if paths.is_empty() {
                self.paths.remove(&root);
            }
        }
        Some(entry)
    }

    pub fn move_pending(&mut self, id: QueueId, direction: isize) -> bool {
        !self.is_updating() && self.pending.move_adjacent(id, direction)
    }

    /// Pause and cancel, transferring pending ownership for off-thread cleanup.
    /// The return value is the number of removed pending jobs.
    pub fn cancel_all(&mut self) -> usize {
        self.paused = true;
        self.cancel_active();
        let removed = self.pending.len();
        self.retired.push(Retired::Pending {
            _pending: std::mem::take(&mut self.pending),
            _paths: std::mem::take(&mut self.paths),
            _superseded: std::mem::take(&mut self.superseded),
            _superseded_nodes: std::mem::take(&mut self.superseded_nodes),
            _pruning: std::mem::take(&mut self.pruning),
        });
        self.flush_retired();
        removed
    }

    /// Exact ancestor/descendant overlap without walking the whole pending list.
    pub fn overlaps(&self, anchor: &Arc<RootAnchor>, relative: &Path) -> bool {
        if self.active().is_some_and(|entry| {
            Arc::ptr_eq(&entry.anchor, anchor)
                && (entry.relative.starts_with(relative) || relative.starts_with(&entry.relative))
        }) {
            return true;
        }
        let root = anchor_id(anchor);
        let path = path_components(relative);
        self.covering_pending(root, &path).is_some()
            || self.first_descendant(root, &path, None).is_some()
    }

    fn covering_pending(&self, root: usize, path: &[OsString]) -> Option<QueueId> {
        let paths = self.paths.get(&root)?;
        for end in 0..=path.len() {
            if let Some(id) = paths.get(&path[..end]) {
                if !self.is_superseded(root, &path[..end], *id) {
                    return Some(*id);
                }
            }
        }
        None
    }

    fn is_superseded(&self, root: usize, path: &[OsString], id: QueueId) -> bool {
        let Some(markers) = self.superseded.get(&root) else {
            return false;
        };
        (0..=path.len()).any(|end| markers.get(&path[..end]).is_some_and(|cutoff| id < *cutoff))
    }

    fn first_descendant(
        &self,
        root: usize,
        prefix: &[OsString],
        except: Option<QueueId>,
    ) -> Option<(Arc<[OsString]>, QueueId)> {
        let paths = self.paths.get(&root)?;
        let (path, id) = paths
            .range(Arc::<[OsString]>::from(prefix)..)
            .find(|(_, id)| Some(**id) != except)?;
        path.starts_with(prefix).then(|| (path.clone(), *id))
    }

    /// Ancestors replace potentially many pending descendants in bounded steps.
    /// Dispatch waits for this transition; cancellation and input stay live.
    pub fn pump(&mut self, limit: usize) -> bool {
        self.flush_retired();
        let mut changed = false;
        for _ in 0..limit {
            // A busy cleanup worker must not let deferred per-entry garbage
            // grow into a large reallocating collection on the input thread.
            if self.retired.len() >= 128 {
                break;
            }
            let Some(prune) = self.pruning.front() else {
                break;
            };
            let candidate = prune
                .paths
                .range(prune.prefix.clone()..)
                .next()
                .filter(|(path, _)| path.starts_with(&prune.prefix))
                .map(|(path, id)| (path.clone(), *id));
            if let Some((path, id)) = candidate {
                self.pruning
                    .front_mut()
                    .expect("prune exists")
                    .paths
                    .remove(&path);
                if let Some(entry) = self.remove_pending_inner(id) {
                    self.retired.push(Retired::Entry { _entry: entry });
                }
                changed = true;
            } else {
                let prune = self.pruning.pop_front().expect("prune exists");
                if self.superseded_nodes.get(&prune.node) == Some(&prune.ancestor) {
                    self.superseded_nodes.remove(&prune.node);
                }
                if let Some(markers) = self.superseded.get_mut(&prune.anchor) {
                    if markers.get(&prune.prefix) == Some(&prune.ancestor) {
                        markers.remove(&prune.prefix);
                    }
                    if markers.is_empty() {
                        self.superseded.remove(&prune.anchor);
                    }
                }
                self.retired.push(Retired::Prune { _prune: prune });
                changed = true;
            }
        }
        self.flush_retired();
        changed
    }

    pub fn start_next(&mut self) -> bool {
        self.start_next_with(|entry| {
            let handle = match entry.target {
                Some(target) => delete::spawn_confirmed(
                    Arc::clone(&entry.anchor),
                    entry.relative.clone(),
                    target,
                ),
                None => delete::spawn(Arc::clone(&entry.anchor), entry.relative.clone()),
            };
            Worker::Running(handle)
        })
    }

    fn start_next_with(&mut self, spawn: impl FnOnce(&QueueEntry) -> Worker) -> bool {
        if self.paused || self.active.is_some() || self.is_updating() {
            return false;
        }
        let Some(id) = self.pending.first else {
            return false;
        };
        let entry = self.remove_pending_inner(id).expect("pending head exists");
        let worker = spawn(&entry);
        self.active = Some(ActiveDelete {
            entry,
            worker,
            cancel_requested: false,
        });
        true
    }

    pub fn poll(&mut self) -> Option<QueueCompletion> {
        let active = self.active.as_ref()?;
        if !active.worker.is_finished() {
            return None;
        }
        let result = match active.worker.receive() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => DeleteResult {
                relative: active.entry.relative.clone(),
                outcome: Err(io::Error::other("deletion worker stopped without a result")),
                parent_metadata: None,
                metadata_error: None,
                progress: active.worker.progress(),
            },
        };
        let active = self.active.take()?;
        Some(QueueCompletion {
            entry: active.entry,
            result,
        })
    }

    fn flush_retired(&mut self) {
        if self.retired.is_empty() {
            return;
        }
        if let Err(retired) = self.retirer.try_retire(std::mem::take(&mut self.retired)) {
            self.retired = retired;
        }
    }
}

impl Drop for DeleteQueue {
    fn drop(&mut self) {
        self.cancel_all();
        if !self.retired.is_empty() {
            let retired = std::mem::take(&mut self.retired);
            let _ = std::thread::Builder::new()
                .name("ldu-queue-retire".into())
                .spawn(move || drop(retired));
        }
    }
}

fn anchor_id(anchor: &Arc<RootAnchor>) -> usize {
    Arc::as_ptr(anchor) as usize
}

fn path_components(relative: &Path) -> Arc<[OsString]> {
    relative
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>()
        .into()
}

fn covers(entry: &QueueEntry, anchor: &Arc<RootAnchor>, relative: &Path) -> bool {
    Arc::ptr_eq(&entry.anchor, anchor) && relative.starts_with(&entry.relative)
}

#[cfg(test)]
struct FakeWorker {
    receiver: crossbeam_channel::Receiver<DeleteResult>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Sender, bounded};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeControl {
        sender: Sender<DeleteResult>,
        finished: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        relative: PathBuf,
    }

    impl FakeControl {
        fn send_result(&self, outcome: io::Result<()>) {
            self.sender
                .try_send(DeleteResult {
                    relative: self.relative.clone(),
                    outcome,
                    parent_metadata: None,
                    metadata_error: None,
                    progress: DeleteProgress::default(),
                })
                .unwrap();
        }

        fn finish(&self) {
            self.finished.store(true, Ordering::Release);
        }
    }

    fn anchor() -> Arc<RootAnchor> {
        Arc::new(RootAnchor::open(Path::new(".")).unwrap())
    }

    fn enqueue(queue: &mut DeleteQueue, anchor: &Arc<RootAnchor>, path: &str) -> QueueId {
        queue.enqueue(1, Arc::clone(anchor), path.into()).id
    }

    fn start(queue: &mut DeleteQueue) -> Option<FakeControl> {
        let mut control = None;
        queue.start_next_with(|entry| {
            let (sender, receiver) = bounded(1);
            let finished = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            control = Some(FakeControl {
                sender,
                finished: Arc::clone(&finished),
                cancelled: Arc::clone(&cancelled),
                relative: entry.relative.clone(),
            });
            Worker::Fake(FakeWorker {
                receiver,
                finished,
                cancelled,
            })
        });
        control
    }

    #[test]
    fn fifo_waits_for_worker_stop_and_explicit_model_reconciliation() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let first = enqueue(&mut queue, &anchor, "first");
        let second = enqueue(&mut queue, &anchor, "second");
        let worker = start(&mut queue).unwrap();
        worker.send_result(Ok(()));
        assert!(queue.poll().is_none());
        assert!(start(&mut queue).is_none());
        assert_eq!(queue.active().unwrap().id, first);
        worker.finish();
        let completed = queue.poll().unwrap();
        assert_eq!(completed.entry.id, first);
        assert!(completed.result.outcome.is_ok());
        assert!(queue.active().is_none());
        assert_eq!(queue.pending().front().unwrap().id, second);
        let worker = start(&mut queue).unwrap();
        assert_eq!(queue.active().unwrap().id, second);
        worker.send_result(Ok(()));
        worker.finish();
        assert_eq!(queue.poll().unwrap().entry.id, second);
        assert!(!queue.is_busy());
    }

    #[test]
    fn pause_holds_pending_without_cancelling_active() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        enqueue(&mut queue, &anchor, "first");
        enqueue(&mut queue, &anchor, "second");
        let worker = start(&mut queue).unwrap();
        assert!(queue.toggle_pause());
        assert!(!worker.cancelled.load(Ordering::Acquire));
        worker.send_result(Ok(()));
        worker.finish();
        assert!(queue.poll().is_some());
        assert!(start(&mut queue).is_none());
        assert!(!queue.toggle_pause());
        assert!(start(&mut queue).is_some());
    }

    #[test]
    fn cancel_keeps_active_slot_until_completion_then_reorder_and_remove_use_ids() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let first = enqueue(&mut queue, &anchor, "first");
        let second = enqueue(&mut queue, &anchor, "second");
        let third = enqueue(&mut queue, &anchor, "third");
        let worker = start(&mut queue).unwrap();
        assert!(queue.cancel_active());
        assert!(worker.cancelled.load(Ordering::Acquire));
        assert!(queue.is_cancelling());
        assert!(!queue.is_paused());
        assert!(start(&mut queue).is_none());
        assert!(!queue.move_pending(first, 1));
        assert!(queue.move_pending(third, -1));
        assert_eq!(queue.pending().front().unwrap().id, third);
        assert!(!queue.move_pending(third, -1));
        assert_eq!(queue.remove_pending(second).unwrap().id, second);
        worker.send_result(Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")));
        worker.finish();
        assert_eq!(
            queue.poll().unwrap().result.outcome.unwrap_err().kind(),
            io::ErrorKind::Interrupted,
        );
        assert!(!queue.is_cancelling());
        assert!(start(&mut queue).is_some());
        assert_eq!(queue.active().unwrap().id, third);
    }

    #[test]
    fn ancestor_subsumes_pending_children_but_runs_after_active_child() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let child = enqueue(&mut queue, &anchor, "parent/active");
        enqueue(&mut queue, &anchor, "parent/pending");
        let sibling = enqueue(&mut queue, &anchor, "sibling");
        let worker = start(&mut queue).unwrap();
        let parent = queue.enqueue(2, Arc::clone(&anchor), "parent".into());
        assert!(parent.inserted);
        queue.pump(128);
        assert_eq!(queue.active().unwrap().id, child);
        assert!(!worker.cancelled.load(Ordering::Acquire));
        assert_eq!(
            queue
                .pending()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![sibling, parent.id],
        );
        let duplicate = queue.enqueue(3, Arc::clone(&anchor), "parent/another".into());
        assert_eq!(
            duplicate,
            EnqueueOutcome {
                id: parent.id,
                inserted: false
            },
        );
        worker.send_result(Ok(()));
        worker.finish();
        assert_eq!(queue.poll().unwrap().entry.id, child);
        assert!(start(&mut queue).is_some());
        assert_eq!(queue.active().unwrap().id, sibling);
    }

    #[test]
    fn overlap_uses_path_components_and_retained_anchor_identity() {
        let anchor = anchor();
        let other_anchor = Arc::new(RootAnchor::open(Path::new(".")).unwrap());
        let mut queue = DeleteQueue::new().unwrap();
        let parent = enqueue(&mut queue, &anchor, "parent");
        let _worker = start(&mut queue).unwrap();
        assert_eq!(
            queue.enqueue(1, Arc::clone(&anchor), "parent".into()),
            EnqueueOutcome {
                id: parent,
                inserted: false
            },
        );
        assert_eq!(
            queue.enqueue(2, Arc::clone(&anchor), "parent/child".into()),
            EnqueueOutcome {
                id: parent,
                inserted: false
            },
        );
        assert!(
            queue
                .enqueue(3, Arc::clone(&anchor), "parent-sibling".into())
                .inserted
        );
        assert!(queue.enqueue(4, other_anchor, "parent".into()).inserted);
        assert_eq!(queue.pending().len(), 2);
    }

    #[test]
    fn cancel_all_and_drop_request_stop_without_waiting() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let first = enqueue(&mut queue, &anchor, "first");
        let second = enqueue(&mut queue, &anchor, "second");
        let worker = start(&mut queue).unwrap();
        let removed = queue.cancel_all();
        assert_eq!(removed, 1);
        assert!(queue.pending().is_empty());
        assert!(queue.entry(second).is_none());
        assert_eq!(queue.active().unwrap().id, first);
        assert!(queue.is_paused());
        assert!(worker.cancelled.load(Ordering::Acquire));
        drop(queue);

        let mut queue = DeleteQueue::new().unwrap();
        enqueue(&mut queue, &anchor, "third");
        let worker = start(&mut queue).unwrap();
        drop(queue);
        assert!(worker.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn disconnected_finished_worker_returns_failure_and_releases_slot() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let id = enqueue(&mut queue, &anchor, "first");
        let worker = start(&mut queue).unwrap();
        worker.finish();
        drop(worker);
        let completed = queue.poll().unwrap();
        assert_eq!(completed.entry.id, id);
        assert!(completed.result.outcome.is_err());
        assert!(!queue.is_busy());
    }

    #[test]
    fn linked_controls_match_stable_id_order_after_moves_and_removals() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let mut expected = (0..256)
            .map(|n| enqueue(&mut queue, &anchor, &format!("item-{n}")))
            .collect::<Vec<_>>();
        for step in 0..128 {
            let position = (step * 37) % expected.len();
            let id = expected[position];
            if step % 3 == 0 {
                assert_eq!(
                    queue.remove_pending(id).unwrap().id,
                    expected.remove(position)
                );
            } else {
                let direction = if step % 2 == 0 { 1 } else { -1 };
                let next = position
                    .checked_add_signed(direction)
                    .filter(|&n| n < expected.len());
                assert_eq!(queue.move_pending(id, direction), next.is_some());
                if let Some(next) = next {
                    expected.swap(position, next);
                }
            }
            assert_eq!(
                queue
                    .pending()
                    .iter()
                    .map(|entry| entry.id)
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(queue.first_id(), expected.first().copied());
            assert_eq!(queue.last_id(), expected.last().copied());
            for (position, &id) in expected.iter().enumerate() {
                assert_eq!(
                    queue.previous_id(id),
                    position.checked_sub(1).map(|n| expected[n])
                );
                assert_eq!(queue.next_id(id), expected.get(position + 1).copied());
                assert_eq!(queue.entry(id).unwrap().id, id);
            }
        }
    }

    #[test]
    fn broad_ancestor_pruning_is_bounded_and_cancellation_does_not_revive_children() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        for n in 0..4096 {
            enqueue(&mut queue, &anchor, &format!("parent/child-{n}"));
        }
        let parent = enqueue(&mut queue, &anchor, "parent");
        assert!(queue.is_updating());
        assert!(start(&mut queue).is_none());
        let before = queue.pending().len();
        queue.pump(17);
        assert!(before - queue.pending().len() <= 17);
        assert!(queue.overlaps(&anchor, Path::new("parent/other")));
        assert!(!queue.overlaps(&anchor, Path::new("parent-sibling")));
        assert_eq!(queue.remove_pending(parent).unwrap().id, parent);
        while queue.is_updating() {
            let before = queue.pending().len();
            queue.pump(17);
            assert!(before - queue.pending().len() <= 17);
            std::thread::yield_now();
        }
        assert!(queue.pending().is_empty());
        assert!(!queue.overlaps(&anchor, Path::new("parent")));
        assert!(!queue.is_busy());
    }

    #[test]
    fn a_cancelled_ancestor_prunes_only_its_previously_confirmed_descendants() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        for n in 0..32 {
            enqueue(&mut queue, &anchor, &format!("parent/old-{n}"));
        }
        let ancestor = enqueue(&mut queue, &anchor, "parent");
        assert_eq!(queue.remove_pending(ancestor).unwrap().id, ancestor);
        let later = enqueue(&mut queue, &anchor, "parent/new");
        while queue.is_updating() {
            queue.pump(4);
            std::thread::yield_now();
        }
        assert_eq!(queue.pending().len(), 1);
        assert_eq!(queue.pending().front().unwrap().id, later);
        assert_eq!(
            queue.entry(later).unwrap().relative,
            Path::new("parent/new")
        );

        // Reconfirming the ancestor creates its own newer supersession snapshot.
        let ancestor = enqueue(&mut queue, &anchor, "parent");
        while queue.is_updating() {
            queue.pump(4);
            std::thread::yield_now();
        }
        assert_eq!(queue.pending().len(), 1);
        assert_eq!(queue.pending().front().unwrap().id, ancestor);
    }

    #[test]
    fn exact_path_reconfirmation_survives_old_supersession_and_index_cleanup() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let old = enqueue(&mut queue, &anchor, "parent/old");
        let ancestor = enqueue(&mut queue, &anchor, "parent");
        queue.remove_pending(ancestor).unwrap();
        let fresh = queue.enqueue(1, Arc::clone(&anchor), "parent/old".into());
        assert!(fresh.inserted);
        assert_ne!(fresh.id, old);
        while queue.is_updating() {
            queue.pump(1);
            std::thread::yield_now();
        }
        assert_eq!(queue.pending().len(), 1);
        assert_eq!(queue.pending().front().unwrap().id, fresh.id);
        assert_eq!(
            queue.enqueue(1, Arc::clone(&anchor), "parent/old".into()),
            EnqueueOutcome {
                id: fresh.id,
                inserted: false
            },
        );
        assert!(queue.overlaps(&anchor, Path::new("parent/old")));

        let ancestor = enqueue(&mut queue, &anchor, "parent");
        queue.remove_pending(ancestor).unwrap();
        let fresh_again = queue.enqueue(1, Arc::clone(&anchor), "parent/old".into());
        assert!(fresh_again.inserted);
        let newest_ancestor = enqueue(&mut queue, &anchor, "parent");
        while queue.is_updating() {
            queue.pump(1);
            std::thread::yield_now();
        }
        assert_eq!(queue.pending().len(), 1);
        assert_eq!(queue.pending().front().unwrap().id, newest_ancestor);
    }

    #[test]
    fn cancelling_active_jobs_do_not_coalesce_fresh_confirmations() {
        let anchor = anchor();
        for target in ["parent", "parent/child"] {
            let mut queue = DeleteQueue::new().unwrap();
            let old = enqueue(&mut queue, &anchor, "parent");
            let worker = start(&mut queue).unwrap();
            assert!(queue.cancel_active());
            let fresh = queue.enqueue(2, Arc::clone(&anchor), target.into());
            assert!(fresh.inserted);
            assert_ne!(fresh.id, old);
            assert_eq!(queue.active().unwrap().id, old);
            assert!(start(&mut queue).is_none());

            worker.send_result(Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")));
            assert!(queue.poll().is_none());
            worker.finish();
            assert_eq!(queue.poll().unwrap().entry.id, old);
            let next_worker = start(&mut queue).unwrap();
            assert_eq!(queue.active().unwrap().id, fresh.id);
            assert_eq!(queue.active().unwrap().relative, Path::new(target));
            next_worker.send_result(Ok(()));
            next_worker.finish();
            assert!(queue.poll().unwrap().result.outcome.is_ok());
            assert!(!queue.is_busy());
        }
    }

    #[test]
    fn deletion_marks_follow_supersession_cancellation_and_same_node_reconfirmation() {
        let anchor = anchor();
        let mut queue = DeleteQueue::new().unwrap();
        let mut marks = QueueMarks::default();
        let marked = |marks: &QueueMarks, ids: &[NodeId]| {
            ids.iter()
                .fold(DeleteState::default(), |state, &id| {
                    marks.include(id, state)
                })
                .is_deleting()
        };
        queue.toggle_pause();
        queue.enqueue(2, Arc::clone(&anchor), "parent/child".into());
        let parent = queue.enqueue(1, Arc::clone(&anchor), "parent".into()).id;
        queue.sync_marks(&mut marks);
        assert!(marked(&marks, &[2, 1]) && marked(&marks, &[3, 1]));
        queue.remove_pending(parent).unwrap();
        queue.sync_marks(&mut marks);
        // The child is still physically queued but cannot run after supersession.
        assert!(queue.is_updating() && !queue.pending().is_empty());
        assert!(!marked(&marks, &[2, 1]) && !marked(&marks, &[3, 1]));
        let fresh = queue
            .enqueue(2, Arc::clone(&anchor), "parent/child".into())
            .id;
        queue.sync_marks(&mut marks);
        assert!(marked(&marks, &[2, 1]) && !marked(&marks, &[3, 1]));
        while queue.is_updating() {
            queue.pump(1);
        }
        queue.sync_marks(&mut marks);
        assert!(marked(&marks, &[2, 1]));
        queue.remove_pending(fresh).unwrap();
        queue.sync_marks(&mut marks);
        assert!(marks.is_empty() && !marked(&marks, &[2, 1]));
    }

    #[test]
    fn deletion_marks_retain_active_work_until_completion_and_keep_fresh_targets() {
        for keep_fresh in [false, true] {
            let anchor = anchor();
            let mut queue = DeleteQueue::new().unwrap();
            let mut marks = QueueMarks::default();
            queue.enqueue(1, Arc::clone(&anchor), "parent".into());
            let worker = start(&mut queue).unwrap();
            queue.cancel_active();
            queue.toggle_pause();
            queue.enqueue(1, Arc::clone(&anchor), "parent".into());
            if !keep_fresh {
                queue.cancel_all();
            }
            queue.sync_marks(&mut marks);
            assert!(marks.include(1, DeleteState::default()).is_deleting());
            worker.send_result(Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")));
            assert!(queue.poll().is_none());
            worker.finish();
            queue.poll().unwrap();
            queue.sync_marks(&mut marks);
            assert_eq!(
                marks.include(1, DeleteState::default()).is_deleting(),
                keep_fresh
            );
            queue.cancel_all();
            queue.sync_marks(&mut marks);
            assert!(marks.is_empty());
        }
    }
}
