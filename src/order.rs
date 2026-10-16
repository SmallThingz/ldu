//! Shared background directory ordering. Exact size order is split into
//! groups whose largest value is at most 2% above their smallest. Previous ranks
//! are retained inside each group, so growing near-ties do not churn.
use crate::model::{Model, Node};
use crate::types::{NodeId, NodeKind, SizeMode, Sizes, SortMode};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet, VecDeque, hash_map::Entry},
    ffi::OsStr,
    io,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering as AtomicOrdering},
    },
    thread,
    time::{Duration, Instant},
};

const COMMAND_CAPACITY: usize = 8;
const SORT_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DRAIN: usize = 64;
const FILTER_BATCH: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderConfig {
    pub size_mode: SizeMode,
    pub sort_mode: SortMode,
    pub descending: bool,
    pub show_hidden: bool,
    pub directories_first: bool,
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self {
            size_mode: SizeMode::Allocated,
            sort_mode: SortMode::Size,
            descending: true,
            show_hidden: true,
            directories_first: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OrderRow {
    pub id: NodeId,
    pub name: Arc<OsStr>,
    pub kind: NodeKind,
    pub size: u64,
    pub sizes: Sizes,
    pub items: u64,
}

impl OrderRow {
    pub fn from_node(id: NodeId, node: &Node, mode: SizeMode) -> Self {
        Self {
            id,
            name: Arc::clone(&node.name),
            kind: node.kind,
            size: match mode {
                SizeMode::Allocated => node.sizes.allocated,
                SizeMode::Apparent => node.sizes.apparent,
            },
            sizes: node.sizes,
            items: node.items,
        }
    }
}

struct SortedRows {
    ids: Vec<NodeId>,
    positions: HashMap<NodeId, u32>,
}

struct RowState {
    row: OrderRow,
    // One-based ranks leave zero for rows not yet published.
    rank: Option<NonZeroUsize>,
}

struct State {
    config: OrderConfig,
    rows: HashMap<NodeId, RowState>,
    dirty: bool,
}

impl State {
    fn new() -> Self {
        Self {
            config: OrderConfig::default(),
            rows: HashMap::new(),
            dirty: false,
        }
    }

    #[cfg(test)]
    fn snapshot(&mut self) -> SortedRows {
        self.snapshot_unless_unchanged(None)
            .expect("initial snapshot")
    }

    fn snapshot_unless_unchanged(&mut self, previous: Option<&[NodeId]>) -> Option<SortedRows> {
        let config = self.config;
        let mut ranked: Vec<_> = self
            .rows
            .values_mut()
            .filter(|state| {
                config.show_hidden || state.row.name.as_encoded_bytes().first() != Some(&b'.')
            })
            .collect();
        ranked.sort_unstable_by(|a, b| compare_rows(a, b, config));
        if config.sort_mode == SortMode::Size {
            stabilize_size_groups(&mut ranked, config);
        }
        let unchanged = previous.is_some_and(|previous| {
            previous.len() == ranked.len()
                && ranked
                    .iter()
                    .zip(previous)
                    .all(|(state, &id)| state.row.id == id)
        });
        if unchanged {
            for (rank, state) in ranked.into_iter().enumerate() {
                state.rank = NonZeroUsize::new(rank + 1);
            }
            self.dirty = false;
            return None;
        }
        let mut ids = Vec::with_capacity(ranked.len());
        let mut positions = HashMap::with_capacity(ranked.len());
        for (rank, state) in ranked.into_iter().enumerate() {
            state.rank = NonZeroUsize::new(rank + 1);
            ids.push(state.row.id);
            // Each row has a distinct u32 node ID, so its zero-based rank fits.
            positions.insert(
                state.row.id,
                u32::try_from(rank).expect("node rank fits u32"),
            );
        }
        self.dirty = false;
        Some(SortedRows { ids, positions })
    }
}

fn compare_rows(a: &RowState, b: &RowState, config: OrderConfig) -> Ordering {
    if config.directories_first {
        let directories =
            (a.row.kind != NodeKind::Directory).cmp(&(b.row.kind != NodeKind::Directory));
        if directories != Ordering::Equal {
            return directories;
        }
    }
    let metric = match config.sort_mode {
        SortMode::Size => a.row.size.cmp(&b.row.size),
        SortMode::Name => a.row.name.cmp(&b.row.name),
        SortMode::Items => a.row.items.cmp(&b.row.items),
    };
    let metric = if config.descending {
        metric.reverse()
    } else {
        metric
    };
    metric.then_with(|| compare_previous_rank(a, b))
}

fn compare_previous_rank(a: &RowState, b: &RowState) -> Ordering {
    match (a.rank, b.rank) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| a.row.name.cmp(&b.row.name))
    .then_with(|| a.row.id.cmp(&b.row.id))
}

fn compare_group_rows(a: &RowState, b: &RowState, config: OrderConfig) -> Ordering {
    match (a.rank, b.rank) {
        (Some(a_rank), Some(b_rank)) => {
            a_rank.cmp(&b_rank).then_with(|| compare_rows(a, b, config))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => compare_rows(a, b, config),
    }
}

fn stabilize_size_groups(rows: &mut [&mut RowState], config: OrderConfig) {
    let mut start = 0;
    while start < rows.len() {
        let anchor = rows[start].row.size;
        let directory = rows[start].row.kind == NodeKind::Directory;
        let mut end = start + 1;
        while end < rows.len()
            && (!config.directories_first
                || (rows[end].row.kind == NodeKind::Directory) == directory)
            && near_sizes(anchor, rows[end].row.size)
        {
            end += 1;
        }
        // Compare against the group's endpoint, never its previous neighbor:
        // pairwise tolerance chaining could put arbitrarily distant sizes together.
        rows[start..end].sort_unstable_by(|a, b| compare_group_rows(a, b, config));
        start = end;
    }
}

fn near_sizes(a: u64, b: u64) -> bool {
    u128::from(a.max(b)) * 100 <= u128::from(a.min(b)) * 102
}

/// One shared index serves navigation and previews. Rows are submitted as scan
/// events arrive, so changing the selected directory only clones an Arc.
#[derive(Clone, Debug)]
pub struct CachedRow {
    pub parent: Option<NodeId>,
    pub parent_epoch: u64,
    pub child_epoch: u64,
    pub removed: bool,
    pub row: OrderRow,
}

impl CachedRow {
    pub fn from_model(id: NodeId, model: &Model, mode: SizeMode) -> Self {
        let node = model.node(id);
        Self {
            parent: node.parent,
            parent_epoch: node.parent_epoch,
            child_epoch: node.child_epoch,
            removed: !model.is_live(id),
            row: OrderRow::from_node(id, node, mode),
        }
    }
}

pub struct CachedSnapshot {
    pub generation: u64,
    pub child_epoch: u64,
    pub ids: Vec<NodeId>,
    pub positions: HashMap<NodeId, u32>,
}

impl CachedSnapshot {
    pub fn position(&self, id: NodeId) -> Option<usize> {
        self.positions.get(&id).map(|&rank| rank as usize)
    }
}

enum CacheCommand {
    Wake,
    Configure {
        generation: u64,
        config: OrderConfig,
        clear: bool,
    },
    Upsert {
        generation: u64,
        rows: Vec<CachedRow>,
    },
    Remove {
        generation: u64,
        ids: Vec<NodeId>,
    },
}

type PublishedCache = im::HashMap<NodeId, Arc<CachedSnapshot>>;
type SharedCache = Arc<ArcSwap<PublishedCache>>;

struct FilterRequest {
    revision: u64,
    directory: NodeId,
    query: Box<str>,
    directories_only: bool,
}

struct FilteredSnapshot {
    revision: u64,
    source: Arc<CachedSnapshot>,
    view: Arc<CachedSnapshot>,
}

struct FilterMailbox {
    request: ArcSwapOption<FilterRequest>,
    snapshot: ArcSwapOption<FilteredSnapshot>,
}

impl FilterMailbox {
    fn new() -> Self {
        Self {
            request: ArcSwapOption::empty(),
            snapshot: ArcSwapOption::empty(),
        }
    }

    fn publish(&self, snapshot: Option<Arc<FilteredSnapshot>>, state: &mut CacheState) {
        if let Some(previous) = self.snapshot.swap(snapshot) {
            // The input thread can release a large result immediately: the
            // ordering worker remains responsible for its eventual destruction.
            state.retire(Arc::clone(&previous.view));
        }
    }
}

struct FilterTask {
    request: Arc<FilterRequest>,
    source: Arc<CachedSnapshot>,
    query: String,
    next: usize,
    ids: Vec<NodeId>,
    positions: HashMap<NodeId, u32>,
}

#[derive(Default)]
struct FilterState {
    request: Option<Arc<FilterRequest>>,
    task: Option<FilterTask>,
}

impl FilterState {
    fn pump(&mut self, state: &mut CacheState, mailbox: &FilterMailbox) {
        let request = mailbox.request.load_full();
        if self.request.as_ref().map(|request| request.revision)
            != request.as_ref().map(|request| request.revision)
        {
            self.task = None;
            self.request = request;
            mailbox.publish(None, state);
        }
        let Some(request) = self.request.as_ref().map(Arc::clone) else {
            return;
        };
        let Some(source) = state
            .published
            .get(&request.directory)
            .cloned()
            .filter(|source| {
                source.generation == state.generation
                    && source.child_epoch
                        == state
                            .parents
                            .get(&request.directory)
                            .map_or(0, |parent| parent.child_epoch)
                    && !state.is_removed(request.directory)
            })
        else {
            self.task = None;
            mailbox.publish(None, state);
            return;
        };
        if self.task.as_ref().is_some_and(|task| {
            task.source.generation != source.generation
                || task.source.child_epoch != source.child_epoch
        }) {
            self.task = None;
            mailbox.publish(None, state);
        }
        if self.task.is_none() {
            if mailbox.snapshot.load().as_ref().is_some_and(|snapshot| {
                snapshot.revision == request.revision && Arc::ptr_eq(&snapshot.source, &source)
            }) {
                return;
            }
            self.task = Some(FilterTask {
                query: request.query.chars().flat_map(char::to_lowercase).collect(),
                request,
                source,
                next: 0,
                ids: Vec::new(),
                positions: HashMap::new(),
            });
        }
        let task = self.task.as_mut().expect("a filter task was started");
        let end = task
            .next
            .saturating_add(FILTER_BATCH)
            .min(task.source.ids.len());
        if let Some(directory) = state.directories.get(&task.request.directory) {
            for &id in &task.source.ids[task.next..end] {
                let Some(row) = directory.order.rows.get(&id).map(|state| &state.row) else {
                    continue;
                };
                if state.removed.contains(&id)
                    || (task.request.directories_only && row.kind != NodeKind::Directory)
                    || !fuzzy_matches(&row.name, &task.query)
                {
                    continue;
                }
                task.positions.insert(
                    id,
                    u32::try_from(task.ids.len()).expect("filtered node rank fits u32"),
                );
                task.ids.push(id);
            }
        }
        task.next = end;
        if end != task.source.ids.len() {
            return;
        }
        let task = self.task.take().expect("a completed filter task exists");
        // A newer keystroke can arrive while this batch is running. Never
        // publish its predecessor under the newest query's identity.
        if mailbox
            .request
            .load()
            .as_ref()
            .is_none_or(|latest| latest.revision != task.request.revision)
        {
            return;
        }
        let view = Arc::new(CachedSnapshot {
            generation: task.source.generation,
            child_epoch: task.source.child_epoch,
            ids: task.ids,
            positions: task.positions,
        });
        mailbox.publish(
            Some(Arc::new(FilteredSnapshot {
                revision: task.request.revision,
                source: task.source,
                view,
            })),
            state,
        );
    }
}

/// Case-insensitive subsequence matching keeps the directory's existing order.
/// The query is folded once when a background task starts.
fn fuzzy_matches(name: &OsStr, query: &str) -> bool {
    if query.is_ascii() && name.as_encoded_bytes().is_ascii() {
        let mut wanted = query.bytes();
        let Some(mut next) = wanted.next() else {
            return true;
        };
        for &byte in name.as_encoded_bytes() {
            if byte.to_ascii_lowercase() == next {
                let Some(following) = wanted.next() else {
                    return true;
                };
                next = following;
            }
        }
        return false;
    }
    let mut wanted = query.chars();
    let Some(mut next) = wanted.next() else {
        return true;
    };
    for ch in name.to_string_lossy().chars().flat_map(char::to_lowercase) {
        if ch == next {
            let Some(following) = wanted.next() else {
                return true;
            };
            next = following;
        }
    }
    false
}

pub struct CachedOrderWorker {
    sender: Sender<CacheCommand>,
    cache: SharedCache,
    priorities: Arc<[AtomicU32; 2]>,
    stop: Arc<AtomicBool>,
    generation: u64,
    pending_config: Option<(OrderConfig, bool)>,
    filter: Arc<FilterMailbox>,
    filter_revision: u64,
}

impl CachedOrderWorker {
    pub fn new() -> io::Result<Self> {
        let (sender, commands) = bounded(COMMAND_CAPACITY);
        let cache = Arc::new(ArcSwap::from_pointee(PublishedCache::new()));
        let priorities = Arc::new([AtomicU32::new(0), AtomicU32::new(NodeId::MAX)]);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_cache = Arc::clone(&cache);
        let worker_priorities = Arc::clone(&priorities);
        let worker_stop = Arc::clone(&stop);
        let filter = Arc::new(FilterMailbox::new());
        let worker_filter = Arc::clone(&filter);
        let _ = thread::Builder::new()
            .name("ldu-order-cache".into())
            .spawn(move || {
                run_cache(
                    commands,
                    worker_cache,
                    worker_priorities,
                    worker_stop,
                    worker_filter,
                );
            })?;
        Ok(Self {
            sender,
            cache,
            priorities,
            stop,
            generation: 0,
            pending_config: None,
            filter,
            filter_revision: 0,
        })
    }

    /// Discard the old scan on the worker, including its cached views.
    pub fn reset(&mut self, config: OrderConfig) -> u64 {
        self.set_config(config, true)
    }

    /// Keep indexed rows when only the display sort or size mode changes.
    pub fn configure(&mut self, config: OrderConfig) -> u64 {
        self.set_config(config, false)
    }

    fn set_config(&mut self, config: OrderConfig, clear: bool) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        let clear = clear || self.pending_config.is_some_and(|(_, pending)| pending);
        self.pending_config = Some((config, clear));
        self.flush_config();
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn prioritize(&self, directory: NodeId, preview: Option<NodeId>) {
        self.priorities[0].store(directory, AtomicOrdering::Relaxed);
        self.priorities[1].store(preview.unwrap_or(NodeId::MAX), AtomicOrdering::Relaxed);
    }

    /// Read an immutable index without waiting for sorting or publication.
    pub fn snapshot(&self, directory: NodeId) -> Option<Arc<CachedSnapshot>> {
        let cache = self.cache.load();
        cache
            .get(&directory)
            .filter(|view| view.generation == self.generation)
            .cloned()
    }

    /// Compare a retained frame without acquiring another snapshot reference.
    pub fn snapshot_matches(
        &self,
        directory: NodeId,
        expected: Option<&Arc<CachedSnapshot>>,
    ) -> bool {
        let cache = self.cache.load();
        let current = cache
            .get(&directory)
            .filter(|view| view.generation == self.generation);
        current.map(Arc::as_ptr) == expected.map(Arc::as_ptr)
    }

    /// Replace the current basename query without queueing earlier keystrokes.
    /// Normal directory snapshots remain warm and are never filtered in place.
    pub fn set_filter(&mut self, directory: NodeId, query: &str, directories_only: bool) {
        self.filter_revision = self.filter_revision.wrapping_add(1);
        self.filter.request.store(Some(Arc::new(FilterRequest {
            revision: self.filter_revision,
            directory,
            query: Box::from(query),
            directories_only,
        })));
        let _ = self.sender.try_send(CacheCommand::Wake);
    }

    pub fn clear_filter(&mut self) {
        self.filter.request.store(None);
        let _ = self.sender.try_send(CacheCommand::Wake);
    }

    /// Only completed results for the current query and sort generation qualify.
    pub fn filtered_snapshot(&self, directory: NodeId) -> Option<Arc<CachedSnapshot>> {
        let request = self.filter.request.load();
        let request = request.as_ref()?;
        if request.directory != directory {
            return None;
        }
        let snapshot = self.filter.snapshot.load();
        let snapshot = snapshot.as_ref()?;
        (snapshot.revision == request.revision && snapshot.view.generation == self.generation)
            .then(|| Arc::clone(&snapshot.view))
    }

    pub fn filtered_snapshot_matches(
        &self,
        directory: NodeId,
        expected: Option<&Arc<CachedSnapshot>>,
    ) -> bool {
        let request = self.filter.request.load();
        let snapshot = self.filter.snapshot.load();
        let current = request
            .as_ref()
            .filter(|request| request.directory == directory)
            .and_then(|request| {
                snapshot.as_ref().filter(|snapshot| {
                    snapshot.revision == request.revision
                        && snapshot.view.generation == self.generation
                })
            })
            .map(|snapshot| &snapshot.view);
        current.map(Arc::as_ptr) == expected.map(Arc::as_ptr)
    }

    pub fn try_upsert(&mut self, rows: Vec<CachedRow>) -> Result<(), Vec<CachedRow>> {
        if !self.flush_config() {
            return Err(rows);
        }
        match self.sender.try_send(CacheCommand::Upsert {
            generation: self.generation,
            rows,
        }) {
            Ok(()) => Ok(()),
            Err(
                TrySendError::Full(CacheCommand::Upsert { rows, .. })
                | TrySendError::Disconnected(CacheCommand::Upsert { rows, .. }),
            ) => Err(rows),
            Err(_) => unreachable!("only an Upsert was submitted"),
        }
    }

    pub fn try_remove(&mut self, ids: Vec<NodeId>) -> Result<(), Vec<NodeId>> {
        if !self.flush_config() {
            return Err(ids);
        }
        match self.sender.try_send(CacheCommand::Remove {
            generation: self.generation,
            ids,
        }) {
            Ok(()) => Ok(()),
            Err(
                TrySendError::Full(CacheCommand::Remove { ids, .. })
                | TrySendError::Disconnected(CacheCommand::Remove { ids, .. }),
            ) => Err(ids),
            Err(_) => unreachable!("only a Remove was submitted"),
        }
    }

    /// Retry a queued display change even when the scan has no further rows.
    pub fn pump_config(&mut self) -> bool {
        self.flush_config()
    }

    fn flush_config(&mut self) -> bool {
        let Some((config, clear)) = self.pending_config else {
            return true;
        };
        if self
            .sender
            .try_send(CacheCommand::Configure {
                generation: self.generation,
                config,
                clear,
            })
            .is_ok()
        {
            self.pending_config = None;
            true
        } else {
            false
        }
    }
}

impl Drop for CachedOrderWorker {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Release);
    }
}

struct CachedDirectory {
    order: State,
    published: Option<Instant>,
    published_has_rows: bool,
    directory_count: usize,
    visible_count: usize,
    published_has_directories: bool,
    // Even an unchanged ID order must invalidate filtered name/kind membership.
    membership_dirty: bool,
}

impl CachedDirectory {
    fn new(config: OrderConfig) -> Self {
        let mut order = State::new();
        order.config = config;
        order.dirty = true;
        Self {
            order,
            published: None,
            published_has_rows: false,
            directory_count: 0,
            visible_count: 0,
            published_has_directories: false,
            membership_dirty: false,
        }
    }
}

#[derive(Clone, Copy)]
struct CachedParent {
    parent: Option<NodeId>,
    parent_epoch: u64,
    child_epoch: u64,
}

struct RetiredRows {
    rows: std::collections::hash_map::IntoValues<NodeId, RowState>,
}

// Node IDs are never reused within a scan generation. Remember retired IDs as
// merged intervals, so repeated refreshes retain ranges rather than every old
// parent record. Insertion touches only adjacent ranges and stays bounded.
#[derive(Default)]
struct RetiredIds {
    ranges: BTreeMap<NodeId, NodeId>,
}

impl RetiredIds {
    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    fn clear(&mut self) {
        self.ranges.clear();
    }

    fn contains(&self, id: &NodeId) -> bool {
        self.ranges
            .range(..=*id)
            .next_back()
            .is_some_and(|(_, end)| id <= end)
    }

    fn insert(&mut self, id: NodeId) {
        let (mut start, mut end) = (id, id);
        if let Some((&previous_start, &previous_end)) = self.ranges.range(..=id).next_back() {
            if id <= previous_end {
                return;
            }
            if previous_end.checked_add(1) == Some(id) {
                start = previous_start;
            }
        }
        if let Some((&next_start, &next_end)) = self.ranges.range(id..).next()
            && id.checked_add(1) == Some(next_start)
        {
            end = next_end;
            self.ranges.remove(&next_start);
        }
        self.ranges.insert(start, end);
    }
}

struct CacheState {
    generation: u64,
    config: OrderConfig,
    directories: HashMap<NodeId, CachedDirectory>,
    parents: HashMap<NodeId, CachedParent>,
    removed: RetiredIds,
    has_epochs: bool,
    dirty: VecDeque<NodeId>,
    dirty_set: HashSet<NodeId>,
    published: PublishedCache,
    retired: VecDeque<Arc<CachedSnapshot>>,
    retired_indices: VecDeque<Arc<PublishedCache>>,
    cleanup: VecDeque<NodeId>,
    retired_rows: VecDeque<RetiredRows>,
    #[cfg(test)]
    liveness_parent_checks: std::cell::Cell<usize>,
}

impl CacheState {
    fn new() -> Self {
        Self {
            generation: 0,
            config: OrderConfig::default(),
            directories: HashMap::new(),
            parents: HashMap::new(),
            removed: RetiredIds::default(),
            has_epochs: false,
            dirty: VecDeque::new(),
            dirty_set: HashSet::new(),
            published: PublishedCache::new(),
            retired: VecDeque::new(),
            retired_indices: VecDeque::new(),
            cleanup: VecDeque::new(),
            retired_rows: VecDeque::new(),
            #[cfg(test)]
            liveness_parent_checks: std::cell::Cell::new(0),
        }
    }

    fn is_removed(&self, mut id: NodeId) -> bool {
        if self.removed.is_empty() && !self.has_epochs {
            return false;
        }
        // Every admitted child also belongs to its parent's cached row map,
        // including unresolved orphan parents. Once both retirement queues
        // drain, disconnected descendants have their own tombstones. Delayed
        // rows still need their immediate parent and epoch witness checked.
        let settled = self.cleanup.is_empty() && self.retired_rows.is_empty();
        loop {
            if self.removed.contains(&id) {
                return true;
            }
            let Some(link) = self.parents.get(&id) else {
                return false;
            };
            let Some(parent) = link.parent else {
                return false;
            };
            #[cfg(test)]
            self.liveness_parent_checks
                .set(self.liveness_parent_checks.get() + 1);
            if self
                .parents
                .get(&parent)
                .is_some_and(|parent| link.parent_epoch != parent.child_epoch)
            {
                return true;
            }
            if settled {
                return self.removed.contains(&parent);
            }
            id = parent;
        }
    }

    fn reject_row(&mut self, id: NodeId, cache: &SharedCache) {
        // Keep a compact witness before unlinking a dead parent. Descendants
        // and delayed updates can then reject it without its old ancestry.
        self.removed.insert(id);
        self.parents.remove(&id);
        if self.directories.contains_key(&id) {
            self.cleanup.push_back(id);
        }
        self.remove_view(id, cache);
    }

    fn retire_directory(&mut self, directory: CachedDirectory) {
        self.retired_rows.push_back(RetiredRows {
            rows: directory.order.rows.into_values(),
        });
    }

    fn reset_children(&mut self, id: NodeId, cache: &SharedCache) {
        if let Some(previous) = self
            .directories
            .insert(id, CachedDirectory::new(self.config))
        {
            self.retire_directory(previous);
        }
        self.remove_view(id, cache);
        self.mark_dirty(id);
    }

    fn retire(&mut self, view: Arc<CachedSnapshot>) {
        if Arc::strong_count(&view) != 1 {
            self.retired.push_back(view);
        }
    }

    fn publish_index(&mut self, cache: &SharedCache) {
        let previous = cache.swap(Arc::new(self.published.clone()));
        if Arc::strong_count(&previous) != 1 {
            self.retired_indices.push_back(previous);
        }
    }

    fn remove_view(&mut self, id: NodeId, cache: &SharedCache) {
        if let Some(previous) = self.published.remove(&id) {
            self.publish_index(cache);
            self.retire(previous);
        }
    }

    fn mark_dirty(&mut self, id: NodeId) {
        if let Some(directory) = self.directories.get_mut(&id) {
            directory.order.dirty = true;
            if self.dirty_set.insert(id) {
                self.dirty.push_back(id);
            }
        }
    }

    fn apply(&mut self, command: CacheCommand, cache: &SharedCache) {
        match command {
            CacheCommand::Configure {
                generation,
                config,
                clear,
            } => {
                self.generation = generation;
                self.config = config;
                // The worker keeps the removed cache references until the UI releases them.
                let previous = std::mem::take(&mut self.published);
                let views: Vec<_> = previous.values().cloned().collect();
                self.publish_index(cache);
                drop(previous);
                for view in views {
                    self.retire(view);
                }
                self.dirty.clear();
                self.dirty_set.clear();
                if clear {
                    self.directories.clear();
                    self.parents.clear();
                    self.removed.clear();
                    self.has_epochs = false;
                    self.cleanup.clear();
                    self.retired_rows.clear();
                } else {
                    for (&id, directory) in &mut self.directories {
                        directory.order.config = config;
                        directory.published = None;
                        directory.published_has_rows = false;
                        directory.published_has_directories = false;
                        directory.directory_count = 0;
                        directory.visible_count = 0;
                        for state in directory.order.rows.values_mut() {
                            let visible = config.show_hidden
                                || !state.row.name.as_encoded_bytes().starts_with(b".");
                            directory.visible_count += usize::from(visible);
                            directory.directory_count +=
                                usize::from(visible && state.row.kind == NodeKind::Directory);
                            state.row.size = match config.size_mode {
                                SizeMode::Allocated => state.row.sizes.allocated,
                                SizeMode::Apparent => state.row.sizes.apparent,
                            };
                            state.rank = None;
                        }
                        directory.order.dirty = true;
                        self.dirty.push_back(id);
                        self.dirty_set.insert(id);
                    }
                }
            }
            CacheCommand::Upsert { generation, rows } if generation == self.generation => {
                for CachedRow {
                    parent,
                    parent_epoch,
                    child_epoch,
                    removed,
                    mut row,
                } in rows
                {
                    if self.removed.contains(&row.id) {
                        continue;
                    }
                    // Do not recreate a reclaimed parent while applying a late
                    // child's epoch witness. Its descendants are retired too.
                    if parent.is_some_and(|parent| self.removed.contains(&parent)) {
                        self.reject_row(row.id, cache);
                        continue;
                    }
                    self.has_epochs |= parent_epoch != 0 || child_epoch != 0;
                    let previous_epoch = self.parents.get(&row.id).map(|parent| parent.child_epoch);
                    if previous_epoch.is_some_and(|previous| previous > child_epoch) {
                        continue;
                    }
                    // A fresh child can be submitted before its retained parent
                    // reaches the pending-update queue. Its witness advances that
                    // parent's epoch first, so the later parent update keeps it.
                    if let Some(parent) = parent {
                        let known_epoch =
                            self.parents.get(&parent).map_or(0, |link| link.child_epoch);
                        if parent_epoch > known_epoch {
                            self.parents
                                .entry(parent)
                                .or_insert(CachedParent {
                                    parent: None,
                                    parent_epoch: 0,
                                    child_epoch: 0,
                                })
                                .child_epoch = parent_epoch;
                            self.reset_children(parent, cache);
                        }
                    }
                    self.parents.insert(
                        row.id,
                        CachedParent {
                            parent,
                            parent_epoch,
                            child_epoch,
                        },
                    );
                    if removed {
                        self.removed.insert(row.id);
                    }
                    if self.is_removed(row.id) {
                        // A late parent can connect an earlier orphan view to a tombstone.
                        self.reject_row(row.id, cache);
                        continue;
                    }
                    if previous_epoch.unwrap_or(0) != child_epoch {
                        self.reset_children(row.id, cache);
                    }
                    row.size = match self.config.size_mode {
                        SizeMode::Allocated => row.sizes.allocated,
                        SizeMode::Apparent => row.sizes.apparent,
                    };
                    if (row.kind == NodeKind::Directory || parent.is_none())
                        && !self.directories.contains_key(&row.id)
                    {
                        self.directories
                            .insert(row.id, CachedDirectory::new(self.config));
                        self.mark_dirty(row.id);
                    }
                    if let Some(parent) = parent {
                        if self.is_removed(parent) {
                            continue;
                        }
                        let (directory, parent_created) = match self.directories.entry(parent) {
                            Entry::Occupied(entry) => (entry.into_mut(), false),
                            Entry::Vacant(entry) => {
                                (entry.insert(CachedDirectory::new(self.config)), true)
                            }
                        };
                        let visible = self.config.show_hidden
                            || !row.name.as_encoded_bytes().starts_with(b".");
                        let order_changed = match directory.order.rows.entry(row.id) {
                            Entry::Occupied(mut entry) => {
                                let previous = &entry.get().row;
                                // Snapshots contain IDs and positions only. Keep all
                                // metadata current without rebuilding an unchanged order.
                                // Names and kinds also affect filtered membership.
                                let membership_changed =
                                    previous.name != row.name || previous.kind != row.kind;
                                let order_changed = membership_changed
                                    || match self.config.sort_mode {
                                        SortMode::Size => previous.size != row.size,
                                        SortMode::Items => previous.items != row.items,
                                        SortMode::Name => false,
                                    };
                                let previous_visible = self.config.show_hidden
                                    || !entry.get().row.name.as_encoded_bytes().starts_with(b".");
                                directory.membership_dirty |=
                                    membership_changed && (visible || previous_visible);
                                directory.visible_count -= usize::from(previous_visible);
                                directory.visible_count += usize::from(visible);
                                directory.directory_count -= usize::from(
                                    previous_visible && entry.get().row.kind == NodeKind::Directory,
                                );
                                directory.directory_count +=
                                    usize::from(visible && row.kind == NodeKind::Directory);
                                entry.get_mut().row = row;
                                order_changed && (visible || previous_visible)
                            }
                            Entry::Vacant(entry) => {
                                directory.visible_count += usize::from(visible);
                                directory.directory_count +=
                                    usize::from(visible && row.kind == NodeKind::Directory);
                                entry.insert(RowState { row, rank: None });
                                visible
                            }
                        };
                        if order_changed || parent_created {
                            self.mark_dirty(parent);
                        }
                    }
                }
            }
            CacheCommand::Remove { generation, ids } if generation == self.generation => {
                for id in ids {
                    self.removed.insert(id);
                    if let Some(parent) = self.parents.get(&id).and_then(|link| link.parent)
                        && let Some(directory) = self.directories.get_mut(&parent)
                        && let Some(removed) = directory.order.rows.remove(&id)
                    {
                        let visible = self.config.show_hidden
                            || !removed.row.name.as_encoded_bytes().starts_with(b".");
                        directory.visible_count -= usize::from(visible);
                        directory.directory_count -=
                            usize::from(visible && removed.row.kind == NodeKind::Directory);
                        if visible {
                            self.mark_dirty(parent);
                        }
                    }
                    self.cleanup.push_back(id);
                    self.remove_view(id, cache);
                }
            }
            _ => {}
        }
    }

    fn due(&self, id: NodeId) -> bool {
        self.directories.get(&id).is_some_and(|directory| {
            directory.order.dirty
                && (directory
                    .published
                    .is_none_or(|last| last.elapsed() >= SORT_INTERVAL)
                    || (!directory.published_has_rows && directory.visible_count != 0)
                    || (!directory.published_has_directories && directory.directory_count != 0))
        }) && !self.is_removed(id)
    }

    fn next_dirty(&mut self, priorities: &[AtomicU32; 2]) -> Option<NodeId> {
        for priority in priorities.iter().rev() {
            let id = priority.load(AtomicOrdering::Relaxed);
            if self.due(id) {
                self.dirty_set.remove(&id);
                return Some(id);
            }
        }
        for _ in 0..self.dirty.len().min(MAX_DRAIN) {
            let id = self.dirty.pop_front()?;
            if !self.dirty_set.contains(&id) {
                continue;
            }
            if self.is_removed(id) {
                self.dirty_set.remove(&id);
            } else if self.due(id) {
                self.dirty_set.remove(&id);
                return Some(id);
            } else {
                self.dirty.push_back(id);
            }
        }
        None
    }

    fn publish(&mut self, id: NodeId, cache: &SharedCache) {
        let child_epoch = self.parents.get(&id).map_or(0, |link| link.child_epoch);
        let directory = self.directories.get_mut(&id).unwrap();
        let previous_ids = self
            .published
            .get(&id)
            .filter(|view| {
                view.generation == self.generation
                    && view.child_epoch == child_epoch
                    && !directory.membership_dirty
            })
            .map(|view| view.ids.as_slice());
        let snapshot = directory.order.snapshot_unless_unchanged(previous_ids);
        directory.published = Some(Instant::now());
        directory.published_has_rows = directory.visible_count != 0;
        directory.published_has_directories = directory.directory_count != 0;
        directory.membership_dirty = false;
        let Some(snapshot) = snapshot else {
            return;
        };
        let view = Arc::new(CachedSnapshot {
            generation: self.generation,
            child_epoch,
            ids: snapshot.ids,
            positions: snapshot.positions,
        });
        let previous = self.published.insert(id, view);
        self.publish_index(cache);
        if let Some(old) = previous {
            self.retire(old);
        }
    }

    fn cleanup(&mut self, cache: &SharedCache) {
        // A lookup briefly owns an index Arc; callers then keep only the view.
        // Retain both kinds until the worker becomes their last owner.
        let deadline = Instant::now() + Duration::from_millis(4);
        for _ in 0..self.retired_indices.len() {
            let index = self.retired_indices.pop_front().unwrap();
            if Arc::strong_count(&index) != 1 {
                self.retired_indices.push_back(index);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        let deadline = Instant::now() + Duration::from_millis(4);
        for _ in 0..self.retired.len() {
            let view = self.retired.pop_front().unwrap();
            if Arc::strong_count(&view) != 1 {
                self.retired.push_back(view);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        let deadline = Instant::now() + Duration::from_millis(4);
        for _ in 0..MAX_DRAIN {
            let Some(id) = self.cleanup.pop_front() else {
                break;
            };
            self.removed.insert(id);
            self.parents.remove(&id);
            self.dirty_set.remove(&id);
            if let Some(directory) = self.directories.remove(&id) {
                self.retire_directory(directory);
            }
            self.remove_view(id, cache);
            if Instant::now() >= deadline {
                break;
            }
        }
        let deadline = Instant::now() + Duration::from_millis(4);
        for _ in 0..MAX_DRAIN {
            let Some(retired) = self.retired_rows.front_mut() else {
                break;
            };
            if let Some(state) = retired.rows.next() {
                self.removed.insert(state.row.id);
                self.parents.remove(&state.row.id);
                if state.row.kind == NodeKind::Directory {
                    self.cleanup.push_back(state.row.id);
                }
            } else {
                self.retired_rows.pop_front();
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

fn run_cache(
    commands: Receiver<CacheCommand>,
    cache: SharedCache,
    priorities: Arc<[AtomicU32; 2]>,
    stop: Arc<AtomicBool>,
    filter: Arc<FilterMailbox>,
) {
    let mut state = CacheState::new();
    let mut filtering = FilterState::default();
    while !stop.load(AtomicOrdering::Acquire) {
        let timeout = if filtering.task.is_some() {
            Some(Duration::ZERO)
        } else if state.dirty.is_empty()
            && state.cleanup.is_empty()
            && state.retired_rows.is_empty()
            && state.retired.is_empty()
            && state.retired_indices.is_empty()
        {
            // No deadline or reclamation remains. Updates and filter changes
            // send commands; dropping the owner disconnects the only sender.
            None
        } else if state.dirty.is_empty()
            && state.cleanup.is_empty()
            && state.retired_rows.is_empty()
            && state.retired.len() <= MAX_DRAIN
            && state.retired_indices.len() <= MAX_DRAIN
        {
            Some(SORT_INTERVAL)
        } else {
            Some(Duration::from_millis(2))
        };
        let received = match timeout {
            Some(timeout) => commands.recv_timeout(timeout),
            None => commands
                .recv()
                .map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(command) => {
                state.apply(command, &cache);
                for _ in 0..MAX_DRAIN {
                    match commands.try_recv() {
                        Ok(command) => state.apply(command, &cache),
                        Err(_) => break,
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        let deadline = Instant::now() + Duration::from_millis(4);
        while let Some(id) = state.next_dirty(&priorities) {
            state.publish(id, &cache);
            if Instant::now() >= deadline {
                break;
            }
        }
        filtering.pump(&mut state, &filter);
        state.cleanup(&cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::Model,
        types::{ROOT, ScanEvent, Sizes},
    };
    use std::path::PathBuf;

    fn row(id: NodeId, name: &str, size: u64) -> OrderRow {
        OrderRow {
            id,
            name: Arc::from(OsStr::new(name)),
            kind: NodeKind::File,
            size,
            sizes: Sizes {
                apparent: size,
                allocated: size,
            },
            items: 0,
        }
    }

    fn put(state: &mut State, rows: Vec<OrderRow>) {
        for row in rows {
            match state.rows.entry(row.id) {
                Entry::Occupied(mut entry) => entry.get_mut().row = row,
                Entry::Vacant(entry) => {
                    entry.insert(RowState { row, rank: None });
                }
            }
        }
    }

    fn cache_row(
        parent: Option<NodeId>,
        id: NodeId,
        name: &str,
        size: u64,
        kind: NodeKind,
    ) -> CachedRow {
        let mut row = row(id, name, size);
        row.kind = kind;
        CachedRow {
            parent,
            parent_epoch: 0,
            child_epoch: 0,
            removed: false,
            row,
        }
    }

    fn cache_state() -> (CacheState, SharedCache) {
        let cache = Arc::new(ArcSwap::from_pointee(PublishedCache::new()));
        let mut state = CacheState::new();
        state.apply(
            CacheCommand::Configure {
                generation: 1,
                config: OrderConfig::default(),
                clear: true,
            },
            &cache,
        );
        (state, cache)
    }

    #[test]
    fn hidden_children_before_parent_schedule_initial_empty_view() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Configure {
                generation: 1,
                config: OrderConfig {
                    show_hidden: false,
                    ..OrderConfig::default()
                },
                clear: false,
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(2), 3, ".child", 100, NodeKind::File)],
            },
            &cache,
        );
        let priorities = [AtomicU32::new(ROOT), AtomicU32::new(NodeId::MAX)];
        while let Some(id) = state.next_dirty(&priorities) {
            state.publish(id, &cache);
        }
        let child_view = Arc::clone(&cache.load_full()[&2]);
        assert!(child_view.ids.is_empty());
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(
                    Some(ROOT),
                    2,
                    ".parent",
                    100,
                    NodeKind::Directory,
                )],
            },
            &cache,
        );
        while let Some(id) = state.next_dirty(&priorities) {
            state.publish(id, &cache);
        }
        assert!(cache.load_full()[&ROOT].ids.is_empty());
        assert!(Arc::ptr_eq(&child_view, &cache.load_full()[&2]));
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(2), 3, ".renamed", 999, NodeKind::File),
                    cache_row(Some(2), 4, ".new", 1, NodeKind::File),
                ],
            },
            &cache,
        );
        state.apply(
            CacheCommand::Remove {
                generation: 1,
                ids: vec![4],
            },
            &cache,
        );
        assert!(state.next_dirty(&priorities).is_none());
        assert!(!state.directories[&2].order.dirty);
        assert!(Arc::ptr_eq(&child_view, &cache.load_full()[&2]));
    }

    #[test]
    fn hidden_rows_keep_metadata_warm_without_resorting_visible_views() {
        for sort_mode in [SortMode::Size, SortMode::Items, SortMode::Name] {
            let (mut state, cache) = cache_state();
            let config = OrderConfig {
                show_hidden: false,
                sort_mode,
                ..OrderConfig::default()
            };
            state.apply(
                CacheCommand::Configure {
                    generation: 1,
                    config,
                    clear: false,
                },
                &cache,
            );
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![
                        cache_row(Some(ROOT), 1, "visible", 100, NodeKind::File),
                        cache_row(Some(ROOT), 2, ".hidden", 200, NodeKind::File),
                    ],
                },
                &cache,
            );
            state.publish(ROOT, &cache);
            let original = Arc::clone(&cache.load_full()[&ROOT]);
            let mut hidden = cache_row(Some(ROOT), 2, ".renamed", 999, NodeKind::Directory);
            hidden.row.items = 42;
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![
                        hidden,
                        cache_row(Some(ROOT), 3, ".new", 300, NodeKind::File),
                    ],
                },
                &cache,
            );
            assert!(!state.directories[&ROOT].order.dirty);
            assert!(!state.directories[&ROOT].membership_dirty);
            assert_eq!(state.directories[&ROOT].visible_count, 1);
            assert_eq!(state.directories[&ROOT].directory_count, 0);
            assert_eq!(state.directories[&ROOT].order.rows[&2].row.size, 999);
            assert_eq!(state.directories[&ROOT].order.rows[&2].row.items, 42);
            assert!(state.directories.contains_key(&2));
            state.apply(
                CacheCommand::Remove {
                    generation: 1,
                    ids: vec![3],
                },
                &cache,
            );
            assert!(!state.directories[&ROOT].order.dirty);
            assert!(Arc::ptr_eq(&original, &cache.load_full()[&ROOT]));
            state.apply(
                CacheCommand::Configure {
                    generation: 2,
                    config: OrderConfig {
                        show_hidden: true,
                        ..config
                    },
                    clear: false,
                },
                &cache,
            );
            state.publish(ROOT, &cache);
            let view = &cache.load_full()[&ROOT];
            assert_eq!(view.ids.len(), 2);
            assert!(view.position(1).is_some() && view.position(2).is_some());
            assert_eq!(state.directories[&ROOT].directory_count, 1);
            assert_eq!(state.directories[&ROOT].order.rows[&2].row.size, 999);
        }
    }

    #[test]
    fn hidden_visible_transitions_still_invalidate_order_and_filters() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Configure {
                generation: 1,
                config: OrderConfig {
                    show_hidden: false,
                    ..OrderConfig::default()
                },
                clear: false,
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, ".hidden", 100, NodeKind::File)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "visible", false);
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "visible", 100, NodeKind::File)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        assert!(state.directories[&ROOT].membership_dirty);
        state.publish(ROOT, &cache);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [1]);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, ".hidden", 100, NodeKind::File)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
        assert_eq!(state.directories[&ROOT].visible_count, 0);
    }

    #[test]
    fn stable_size_order_reuses_view_and_filter_but_updates_metadata() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(ROOT), 1, "alpha", 100, NodeKind::File),
                    cache_row(Some(ROOT), 2, "beta", 99, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let original_index = cache.load_full();
        let original = Arc::clone(&original_index[&ROOT]);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "alpha", false);
        let filtered = finish_filter(&mut filtering, &mut state, &mailbox);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 2, "beta", 101, NodeKind::File)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert!(Arc::ptr_eq(&original_index, &cache.load_full()));
        assert!(Arc::ptr_eq(&original, &cache.load_full()[&ROOT]));
        assert!(!state.directories[&ROOT].order.dirty);
        assert_eq!(state.directories[&ROOT].order.rows[&2].row.size, 101);
        assert!(Arc::ptr_eq(
            &filtered,
            &finish_filter(&mut filtering, &mut state, &mailbox)
        ));
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 2, "beta", 1000, NodeKind::File)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        assert_eq!(cache.load_full()[&ROOT].ids, [2, 1]);
        assert!(!Arc::ptr_eq(&original, &cache.load_full()[&ROOT]));
    }

    #[test]
    fn unchanged_ids_still_invalidate_name_and_kind_filter_membership() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "alpha", 100, NodeKind::File)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "alpha", false);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [1]);
        let original = Arc::clone(&cache.load_full()[&ROOT]);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "beta", 100, NodeKind::File)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        assert!(!Arc::ptr_eq(&original, &cache.load_full()[&ROOT]));
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
        filter_request(&mailbox, 2, "beta", true);
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "beta", 100, NodeKind::Directory)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [1]);
    }

    #[test]
    fn name_sort_reuses_snapshots_but_retains_updated_metrics() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Configure {
                generation: 1,
                config: OrderConfig {
                    sort_mode: SortMode::Name,
                    descending: false,
                    ..OrderConfig::default()
                },
                clear: false,
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(ROOT), 1, "alpha", 10, NodeKind::File),
                    cache_row(Some(ROOT), 2, "beta", 20, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let original = Arc::clone(&cache.load_full()[&ROOT]);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "alpha", false);
        let filtered = finish_filter(&mut filtering, &mut state, &mailbox);
        let mut updated = cache_row(Some(ROOT), 1, "alpha", 1000, NodeKind::File);
        updated.row.sizes.apparent = 2000;
        updated.row.items = 50;
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![updated],
            },
            &cache,
        );
        assert!(!state.directories[&ROOT].order.dirty);
        assert!(Arc::ptr_eq(&original, &cache.load_full()[&ROOT]));
        let same_filter = finish_filter(&mut filtering, &mut state, &mailbox);
        assert!(Arc::ptr_eq(&filtered, &same_filter));
        for (generation, sort_mode, size_mode) in [
            (2, SortMode::Size, SizeMode::Allocated),
            (3, SortMode::Size, SizeMode::Apparent),
            (4, SortMode::Items, SizeMode::Allocated),
        ] {
            state.apply(
                CacheCommand::Configure {
                    generation,
                    config: OrderConfig {
                        sort_mode,
                        size_mode,
                        ..OrderConfig::default()
                    },
                    clear: false,
                },
                &cache,
            );
            state.publish(ROOT, &cache);
            assert_eq!(cache.load_full()[&ROOT].ids, [1, 2]);
        }
    }

    #[test]
    fn active_order_metrics_republish_and_duplicate_rows_do_not() {
        for sort_mode in [SortMode::Size, SortMode::Items, SortMode::Name] {
            let (mut state, cache) = cache_state();
            state.apply(
                CacheCommand::Configure {
                    generation: 1,
                    config: OrderConfig {
                        sort_mode,
                        ..OrderConfig::default()
                    },
                    clear: false,
                },
                &cache,
            );
            let mut first = cache_row(Some(ROOT), 1, "alpha", 10, NodeKind::File);
            first.row.items = 1;
            let mut second = cache_row(Some(ROOT), 2, "beta", 20, NodeKind::File);
            second.row.items = 2;
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![first.clone(), second],
                },
                &cache,
            );
            state.publish(ROOT, &cache);
            let original = Arc::clone(&cache.load_full()[&ROOT]);
            assert_eq!(original.ids, [2, 1]);
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![first.clone()],
                },
                &cache,
            );
            assert!(!state.directories[&ROOT].order.dirty);
            match sort_mode {
                SortMode::Size => first.row.sizes.allocated = 1000,
                SortMode::Items => first.row.items = 1000,
                SortMode::Name => first.row.name = Arc::from(OsStr::new("zeta")),
            }
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![first],
                },
                &cache,
            );
            assert!(state.directories[&ROOT].order.dirty);
            state.publish(ROOT, &cache);
            let updated = Arc::clone(&cache.load_full()[&ROOT]);
            assert!(!Arc::ptr_eq(&original, &updated));
            assert_eq!(updated.ids, [1, 2]);
        }
    }

    #[test]
    fn kind_and_membership_changes_refresh_filtered_order() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Configure {
                generation: 1,
                config: OrderConfig {
                    sort_mode: SortMode::Name,
                    descending: false,
                    show_hidden: false,
                    ..OrderConfig::default()
                },
                clear: false,
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "alpha", 10, NodeKind::File)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "a", true);
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, "alpha", 10, NodeKind::Directory)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [1]);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 2, "beta", 20, NodeKind::Directory)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert_eq!(
            finish_filter(&mut filtering, &mut state, &mailbox).ids,
            [1, 2]
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 1, ".alpha", 10, NodeKind::Directory)],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [2]);
        state.apply(
            CacheCommand::Remove {
                generation: 1,
                ids: vec![2],
            },
            &cache,
        );
        assert!(state.directories[&ROOT].order.dirty);
        state.publish(ROOT, &cache);
        assert!(
            finish_filter(&mut filtering, &mut state, &mailbox)
                .ids
                .is_empty()
        );
    }

    #[test]
    fn cached_views_are_prewarmed_sorted_and_share_directory_order() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(None, 0, "root", 0, NodeKind::Directory),
                    cache_row(Some(0), 1, "small", 10, NodeKind::Directory),
                    cache_row(Some(0), 2, "large", 100, NodeKind::Directory),
                    cache_row(Some(0), 3, "file", 50, NodeKind::File),
                    cache_row(Some(1), 4, "nested", 20, NodeKind::Directory),
                ],
            },
            &cache,
        );
        state.publish(0, &cache);
        state.publish(1, &cache);
        state.publish(2, &cache);
        let views = cache.load_full();
        assert_eq!(views[&0].ids, vec![2, 3, 1]);
        assert_eq!(state.directories[&ROOT].directory_count, 2);
        assert_eq!(views[&0].positions[&1], 2);
        assert_eq!(views[&1].ids, vec![4]);
        assert!(views[&2].ids.is_empty());
    }

    #[test]
    fn completed_file_symlink_and_failed_roots_have_an_empty_cached_view() {
        for kind in [NodeKind::File, NodeKind::Symlink, NodeKind::Unknown] {
            let (mut state, cache) = cache_state();
            state.apply(
                CacheCommand::Upsert {
                    generation: 1,
                    rows: vec![cache_row(None, 0, "root", 0, kind)],
                },
                &cache,
            );
            assert!(state.due(0));
            state.publish(0, &cache);
            assert!(cache.load_full()[&0].ids.is_empty());
        }
    }

    #[test]
    fn first_children_and_hovered_directories_do_not_wait_for_size_churn_throttle() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(None, 0, "root", 0, NodeKind::Directory),
                    cache_row(Some(0), 1, "hovered", 100, NodeKind::Directory),
                ],
            },
            &cache,
        );
        state.publish(0, &cache);
        state.publish(1, &cache);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(1), 2, "file", 10, NodeKind::File)],
            },
            &cache,
        );
        assert!(
            state.due(1),
            "first known child must bypass the 100ms interval"
        );
        state.publish(1, &cache);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(1), 3, "nested", 20, NodeKind::Directory)],
            },
            &cache,
        );
        assert!(
            state.due(1),
            "first preview subdirectory must bypass the interval"
        );
        state.directories.get_mut(&0).unwrap().published = None;
        state.mark_dirty(0);
        let priorities = [AtomicU32::new(0), AtomicU32::new(1)];
        assert_eq!(state.next_dirty(&priorities), Some(1));
    }

    #[test]
    fn cached_config_changes_use_both_sizes_and_retain_snapshots_for_background_drop() {
        let (mut state, cache) = cache_state();
        let mut apparent = cache_row(Some(0), 1, "apparent", 10, NodeKind::File);
        apparent.row.sizes.apparent = 1000;
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    apparent,
                    cache_row(Some(0), 2, "allocated", 100, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(0, &cache);
        let previous = Arc::clone(&cache.load_full()[&0]);
        let retired = Arc::downgrade(&previous);
        let config = OrderConfig {
            size_mode: SizeMode::Apparent,
            ..OrderConfig::default()
        };
        state.apply(
            CacheCommand::Configure {
                generation: 2,
                config,
                clear: false,
            },
            &cache,
        );
        state.publish(0, &cache);
        assert_eq!(cache.load_full()[&0].ids, vec![1, 2]);
        state.cleanup(&cache);
        assert_eq!(
            Arc::strong_count(&previous),
            2,
            "worker retains the UI's replaced view"
        );
        drop(previous);
        assert!(
            retired.upgrade().is_some(),
            "UI drop must leave a background owner"
        );
        state.cleanup(&cache);
        assert!(
            retired.upgrade().is_none(),
            "worker reclaims the unreferenced view"
        );
    }

    #[test]
    fn cached_deletion_rejects_stale_subtree_rows_and_stale_scan_generations() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(0), 1, "deleted", 100, NodeKind::Directory),
                    cache_row(Some(1), 2, "nested", 80, NodeKind::Directory),
                    cache_row(Some(2), 3, "file", 60, NodeKind::File),
                    cache_row(Some(0), 4, "survivor", 10, NodeKind::Directory),
                ],
            },
            &cache,
        );
        state.publish(0, &cache);
        state.publish(1, &cache);
        state.publish(2, &cache);
        state.apply(
            CacheCommand::Remove {
                generation: 1,
                ids: vec![1],
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(0), 1, "late", 999, NodeKind::Directory),
                    cache_row(Some(2), 5, "late-descendant", 999, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(0, &cache);
        assert_eq!(cache.load_full()[&0].ids, vec![4]);
        assert!(!state.directories[&2].order.rows.contains_key(&5));
        while !state.cleanup.is_empty() || !state.retired_rows.is_empty() {
            state.cleanup(&cache);
        }
        assert!(!cache.load_full().contains_key(&2));
        state.apply(
            CacheCommand::Configure {
                generation: 2,
                config: OrderConfig::default(),
                clear: true,
            },
            &cache,
        );
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(0), 8, "old-scan", 10, NodeKind::File)],
            },
            &cache,
        );
        assert!(state.directories.is_empty());
    }

    #[test]
    fn pending_config_is_retried_after_a_full_queue_without_further_rows() {
        let (sender, commands) = bounded(COMMAND_CAPACITY);
        for _ in 0..COMMAND_CAPACITY {
            sender
                .try_send(CacheCommand::Upsert {
                    generation: 0,
                    rows: Vec::new(),
                })
                .ok()
                .unwrap();
        }
        let mut worker = CachedOrderWorker {
            sender,
            cache: Arc::new(ArcSwap::from_pointee(PublishedCache::new())),
            priorities: Arc::new([AtomicU32::new(0), AtomicU32::new(NodeId::MAX)]),
            stop: Arc::new(AtomicBool::new(false)),
            generation: 0,
            pending_config: None,
            filter: Arc::new(FilterMailbox::new()),
            filter_revision: 0,
        };
        let config = OrderConfig {
            sort_mode: SortMode::Name,
            ..OrderConfig::default()
        };
        assert_eq!(worker.configure(config), 1);
        assert!(worker.pending_config.is_some());
        assert!(!worker.pump_config());
        assert!(matches!(
            commands.try_recv(),
            Ok(CacheCommand::Upsert { .. })
        ));
        assert!(worker.pump_config());
        assert!(worker.pending_config.is_none());
        for _ in 1..COMMAND_CAPACITY {
            let _ = commands.try_recv().unwrap();
        }
        match commands.try_recv().unwrap() {
            CacheCommand::Configure {
                generation,
                config: queued,
                clear,
            } => {
                assert_eq!(generation, 1);
                assert_eq!(queued, config);
                assert!(!clear);
            }
            _ => panic!("display change was not retried"),
        }
    }

    #[test]
    fn settled_worker_wakes_for_filters_and_stops_when_owner_drops() {
        let mut worker = CachedOrderWorker::new().unwrap();
        worker
            .try_upsert(vec![cache_row(Some(ROOT), 1, "alpha", 10, NodeKind::File)])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.snapshot(ROOT).is_none() {
            assert!(Instant::now() < deadline, "initial order was not published");
            thread::sleep(Duration::from_millis(1));
        }
        worker.set_filter(ROOT, "alpha", false);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(view) = worker.filtered_snapshot(ROOT) {
                assert_eq!(view.ids, [1]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "filter request did not wake the worker"
            );
            thread::sleep(Duration::from_millis(1));
        }
        worker.clear_filter();
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.filter.snapshot.load().is_some() {
            assert!(
                Instant::now() < deadline,
                "clearing the filter did not wake the worker"
            );
            thread::sleep(Duration::from_millis(1));
        }
        // The thread holds the remaining mailbox owner until run_cache returns.
        // A stop flag alone cannot wake an indefinitely blocked receiver.
        let mailbox = Arc::downgrade(&worker.filter);
        drop(worker);
        let deadline = Instant::now() + Duration::from_secs(2);
        while mailbox.upgrade().is_some() {
            assert!(
                Instant::now() < deadline,
                "dropping the owner left the worker blocked"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn warm_cache_lookup_never_misses_during_concurrent_publication() {
        let worker = CachedOrderWorker::new().unwrap();
        let view = Arc::new(CachedSnapshot {
            generation: 0,
            child_epoch: 0,
            ids: vec![9],
            positions: HashMap::from([(9, 0)]),
        });
        let mut index = PublishedCache::new();
        index.insert(0, view);
        worker.cache.store(Arc::new(index.clone()));
        let cache = Arc::clone(&worker.cache);
        let gate = Arc::new(std::sync::Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let writer_gate = Arc::clone(&gate);
        let writer_done = Arc::clone(&done);
        let writer = thread::spawn(move || {
            writer_gate.wait();
            for id in 1..4096 {
                index.insert(
                    id,
                    Arc::new(CachedSnapshot {
                        generation: 0,
                        child_epoch: 0,
                        ids: Vec::new(),
                        positions: HashMap::new(),
                    }),
                );
                cache.store(Arc::new(index.clone()));
            }
            writer_done.store(true, AtomicOrdering::Release);
        });
        gate.wait();
        let mut reads = 0;
        loop {
            let view = worker
                .snapshot(0)
                .expect("publication must not hide a warmed directory");
            assert_eq!(view.ids, vec![9]);
            reads += 1;
            if done.load(AtomicOrdering::Acquire) {
                break;
            }
        }
        writer.join().unwrap();
        assert!(reads > 0);
        assert!(worker.snapshot(4095).is_some());
    }

    #[test]
    fn replaced_index_and_view_remain_worker_owned_after_ui_releases_them() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(0), 1, "file", 10, NodeKind::File)],
            },
            &cache,
        );
        state.publish(0, &cache);
        let index = cache.load_full();
        let view = Arc::clone(&index[&0]);
        let old_index = Arc::downgrade(&index);
        let old_view = Arc::downgrade(&view);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(0), 2, "new file", 20, NodeKind::File)],
            },
            &cache,
        );
        state.publish(0, &cache);
        assert_eq!(cache.load_full()[&0].ids, [2, 1]);
        assert_eq!(view.ids, [1]);
        drop(index);
        assert!(
            old_index.upgrade().is_some(),
            "UI index release leaves background ownership"
        );
        state.cleanup(&cache);
        assert!(old_index.upgrade().is_none());
        drop(view);
        assert!(
            old_view.upgrade().is_some(),
            "UI view release leaves background ownership"
        );
        state.cleanup(&cache);
        assert!(old_view.upgrade().is_none());
    }

    #[test]
    fn late_parent_links_reclaim_an_already_cached_removed_orphan() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(Some(6), 7, "nested", 10, NodeKind::Directory),
                    cache_row(Some(7), 8, "file", 10, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(6, &cache);
        state.publish(7, &cache);
        state.apply(
            CacheCommand::Remove {
                generation: 1,
                ids: vec![5],
            },
            &cache,
        );
        state.cleanup(&cache);
        assert!(cache.load_full().contains_key(&6));
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(
                    Some(5),
                    6,
                    "late-parent",
                    10,
                    NodeKind::Directory,
                )],
            },
            &cache,
        );
        while !state.cleanup.is_empty() || !state.retired_rows.is_empty() {
            state.cleanup(&cache);
        }
        assert!(!cache.load_full().contains_key(&6));
        assert!(!cache.load_full().contains_key(&7));
        assert!(!state.directories.contains_key(&7));
    }

    #[test]
    fn live_near_ties_retain_order_and_large_changes_reorder() {
        let mut state = State::new();
        assert!(near_sizes(1000, 1001));
        put(&mut state, vec![row(1, "a", 1000), row(2, "b", 1001)]);
        assert_eq!(state.snapshot().ids, vec![2, 1]);
        put(&mut state, vec![row(1, "a", 1001), row(2, "b", 1000)]);
        assert_eq!(state.snapshot().ids, vec![2, 1]);
        put(&mut state, vec![row(1, "a", 1100)]);
        assert_eq!(state.snapshot().ids, vec![1, 2]);
        state.config.descending = false;
        assert_eq!(state.snapshot().ids, vec![2, 1]);
    }

    #[test]
    fn monotonically_growing_close_rows_do_not_swap_at_absolute_boundaries() {
        let sequence = [
            (1003, 1004),
            (1024, 1023),
            (1043, 1044),
            (1065, 1064),
            (1086, 1087),
            (1108, 1107),
        ];
        for descending in [false, true] {
            let mut state = State::new();
            state.config.descending = descending;
            let mut previous_sizes = (0, 0);
            for (a, b) in sequence {
                assert!(a >= previous_sizes.0 && b >= previous_sizes.1);
                assert!(near_sizes(a, b));
                put(&mut state, vec![row(1, "a", a), row(2, "b", b)]);
                assert_eq!(
                    state.snapshot().ids,
                    if descending { vec![2, 1] } else { vec![1, 2] },
                );
                previous_sizes = (a, b);
            }
        }
    }

    #[test]
    fn new_close_rows_use_exact_metric_order_while_existing_rows_keep_rank() {
        for descending in [false, true] {
            let mut state = State::new();
            state.config.descending = descending;
            put(&mut state, vec![row(1, "a", 1000), row(2, "b", 1001)]);
            assert_eq!(
                state.snapshot().ids,
                if descending { vec![2, 1] } else { vec![1, 2] },
            );
            put(
                &mut state,
                vec![row(3, "z-new", 1010), row(4, "a-new", 1005)],
            );
            assert_eq!(
                state.snapshot().ids,
                if descending {
                    vec![2, 1, 3, 4]
                } else {
                    vec![1, 2, 4, 3]
                },
            );
        }
    }

    #[test]
    fn near_tie_groups_do_not_chain_beyond_two_percent() {
        assert!(near_sizes(1000, 1010));
        assert!(near_sizes(1010, 1021));
        assert!(!near_sizes(1000, 1021));
        assert!(!near_sizes(0, 1));
        assert!(near_sizes(u64::MAX, u64::MAX - 1));
        for descending in [false, true] {
            let mut state = State::new();
            state.config.sort_mode = SortMode::Name;
            state.config.descending = !descending;
            put(
                &mut state,
                vec![row(1, "a", 1000), row(2, "b", 1010), row(3, "c", 1021)],
            );
            state.snapshot();
            state.config.sort_mode = SortMode::Size;
            state.config.descending = descending;
            let snapshot = state.snapshot();
            let low = snapshot.positions[&1];
            let high = snapshot.positions[&3];
            assert_eq!(
                high < low,
                descending,
                "distant endpoints must remain in exact order"
            );
        }
    }

    #[test]
    fn exact_name_item_sorts_and_hidden_filter_do_not_use_completion() {
        let mut state = State::new();
        let mut directory = row(1, "z", 100);
        directory.kind = NodeKind::Directory;
        directory.items = 3;
        put(
            &mut state,
            vec![directory, row(2, "a", 1), row(3, ".hidden", 1000)],
        );
        state.config.show_hidden = false;
        assert_eq!(state.snapshot().ids, vec![1, 2]);
        state.config.sort_mode = SortMode::Name;
        state.config.descending = false;
        assert_eq!(state.snapshot().ids, vec![2, 1]);
        state.config.sort_mode = SortMode::Items;
        state.config.descending = true;
        assert_eq!(state.snapshot().ids, vec![1, 2]);
    }

    #[test]
    fn worker_keys_share_the_models_basename_allocation() {
        let mut model = Model::new(PathBuf::from("fixture"));
        model.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Directory,
            sizes: Sizes::default(),
            error: None,
        });
        let key = OrderRow::from_node(ROOT, model.node(ROOT), SizeMode::Allocated);
        assert!(Arc::ptr_eq(&key.name, &model.node(ROOT).name));
    }

    #[test]
    fn preview_keeps_live_normal_order_and_hidden_policy() {
        let mut state = State::new();
        state.config.show_hidden = false;
        let mut small = row(1, "small", 10);
        small.kind = NodeKind::Directory;
        let mut large = row(2, "large", 100);
        large.kind = NodeKind::Directory;
        let mut hidden = row(3, ".hidden", 1000);
        hidden.kind = NodeKind::Directory;
        put(&mut state, vec![small, large, hidden, row(4, "file", 2000)]);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.ids, vec![4, 2, 1]);
        state.rows.remove(&2);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.ids, vec![4, 1]);
    }

    fn epoch_row(
        parent: Option<NodeId>,
        id: NodeId,
        parent_epoch: u64,
        child_epoch: u64,
        kind: NodeKind,
    ) -> CachedRow {
        let mut row = cache_row(parent, id, &format!("node-{id}"), 10, kind);
        row.parent_epoch = parent_epoch;
        row.child_epoch = child_epoch;
        row
    }

    fn upsert_epoch(state: &mut CacheState, cache: &SharedCache, rows: Vec<CachedRow>) {
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows,
            },
            cache,
        );
    }

    fn finish_cache_cleanup(state: &mut CacheState, cache: &SharedCache) {
        for _ in 0..1000 {
            if state.cleanup.is_empty() && state.retired_rows.is_empty() {
                return;
            }
            state.cleanup(cache);
        }
        panic!("bounded cached-row cleanup did not finish");
    }

    #[test]
    fn settled_cache_bounds_parent_checks_after_removal_and_refresh() {
        const DEPTH: NodeId = 96;
        for refresh in [false, true] {
            let (mut state, cache) = cache_state();
            let mut rows = vec![epoch_row(None, 0, 0, 0, NodeKind::Directory)];
            rows.extend((1..=DEPTH + 1).map(|id| {
                epoch_row(
                    Some(id - 1),
                    id,
                    0,
                    0,
                    if id <= DEPTH {
                        NodeKind::Directory
                    } else {
                        NodeKind::File
                    },
                )
            }));
            upsert_epoch(&mut state, &cache, rows);
            if refresh {
                upsert_epoch(
                    &mut state,
                    &cache,
                    vec![epoch_row(None, 0, 0, 1, NodeKind::Directory)],
                );
                assert!(state.cleanup.is_empty());
                assert!(!state.retired_rows.is_empty());
            } else {
                state.apply(
                    CacheCommand::Remove {
                        generation: 1,
                        ids: vec![1],
                    },
                    &cache,
                );
                assert!(!state.cleanup.is_empty());
                assert!(state.retired_rows.is_empty());
            }
            state.liveness_parent_checks.set(0);
            assert!(state.is_removed(DEPTH + 1));
            assert!(state.liveness_parent_checks.get() >= DEPTH as usize);

            // A surviving branch can arrive while the old branch is retiring.
            let fresh: Vec<_> = (200..=200 + DEPTH)
                .map(|id| {
                    epoch_row(
                        Some(if id == 200 { 0 } else { id - 1 }),
                        id,
                        if id == 200 { u64::from(refresh) } else { 0 },
                        0,
                        if id < 200 + DEPTH {
                            NodeKind::Directory
                        } else {
                            NodeKind::File
                        },
                    )
                })
                .collect();
            upsert_epoch(&mut state, &cache, fresh.clone());
            finish_cache_cleanup(&mut state, &cache);
            state.liveness_parent_checks.set(0);
            for _ in 0..1024 {
                assert!(!state.is_removed(200 + DEPTH));
                assert!(state.is_removed(DEPTH + 1));
            }
            assert_eq!(state.liveness_parent_checks.get(), 1024);

            // Each ancestor-size update validates only the row and its parent.
            state.liveness_parent_checks.set(0);
            upsert_epoch(&mut state, &cache, fresh);
            assert!(state.liveness_parent_checks.get() <= 2 * (DEPTH as usize + 1));
        }
    }

    #[test]
    fn settled_cache_rejects_delayed_epochs_and_preserves_fresh_child_witnesses() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 2, NodeKind::Directory),
                epoch_row(Some(0), 1, 2, 5, NodeKind::Directory),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(Some(1), 2, 4, 0, NodeKind::File)],
        );
        assert!(state.removed.contains(&2));
        assert!(!state.directories[&1].order.rows.contains_key(&2));
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(1), 3, 6, 0, NodeKind::File),
                epoch_row(Some(0), 1, 2, 6, NodeKind::Directory),
                epoch_row(Some(0), 1, 2, 5, NodeKind::Directory),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert!(!state.is_removed(1));
        assert!(!state.is_removed(3));
        state.publish(1, &cache);
        assert_eq!(cache.load_full()[&1].ids, [3]);
        assert_eq!(cache.load_full()[&1].child_epoch, 6);
    }

    #[test]
    fn settled_orphan_attachment_retires_descendants_before_shortcut_resumes() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 2, NodeKind::Directory),
                epoch_row(Some(6), 7, 0, 0, NodeKind::Directory),
                epoch_row(Some(7), 8, 0, 0, NodeKind::File),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert!(!state.is_removed(8));
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(Some(0), 6, 1, 0, NodeKind::Directory)],
        );
        assert!(!state.cleanup.is_empty());
        assert!(state.is_removed(8));
        finish_cache_cleanup(&mut state, &cache);
        for id in 6..=8 {
            assert!(state.removed.contains(&id));
            assert!(!state.parents.contains_key(&id));
            assert!(!state.directories.contains_key(&id));
        }
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(Some(6), 9, 99, 0, NodeKind::Directory)],
        );
        assert!(state.removed.contains(&9));
        assert!(!state.parents.contains_key(&6));

        // A valid late parent keeps an already-admitted fresh orphan child.
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(11), 12, 4, 0, NodeKind::File),
                epoch_row(Some(0), 11, 2, 4, NodeKind::Directory),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert!(!state.is_removed(11));
        assert!(!state.is_removed(12));
        state.publish(11, &cache);
        assert_eq!(cache.load_full()[&11].ids, [12]);
    }

    #[test]
    fn refreshed_directory_keeps_its_id_and_rejects_old_descendant_views() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 0, NodeKind::Directory),
                epoch_row(Some(0), 1, 0, 0, NodeKind::Directory),
                epoch_row(Some(1), 2, 0, 0, NodeKind::Directory),
                epoch_row(Some(2), 3, 0, 0, NodeKind::File),
            ],
        );
        state.publish(1, &cache);
        state.publish(2, &cache);
        let held = Arc::clone(&cache.load_full()[&1]);
        let weak = Arc::downgrade(&held);
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(Some(0), 1, 0, 1, NodeKind::Directory)],
        );
        assert!(!state.is_removed(1));
        assert!(state.is_removed(2) && state.is_removed(3));
        assert!(!state.removed.contains(&1));
        assert!(!cache.load_full().contains_key(&1));
        drop(held);
        assert!(
            weak.upgrade().is_some(),
            "releasing old UI view must leave worker ownership"
        );
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(1), 4, 1, 0, NodeKind::File),
                epoch_row(Some(1), 2, 0, 0, NodeKind::Directory),
            ],
        );
        state.publish(1, &cache);
        assert_eq!(cache.load_full()[&1].ids, vec![4]);
        assert_eq!(cache.load_full()[&1].child_epoch, 1);
        finish_cache_cleanup(&mut state, &cache);
        assert!(!state.directories.contains_key(&2));
        assert!(!cache.load_full().contains_key(&2));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn fresh_children_before_parent_update_preserve_epoch_and_sorted_rows() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 0, NodeKind::Directory),
                epoch_row(Some(0), 1, 0, 0, NodeKind::Directory),
                epoch_row(Some(1), 2, 0, 0, NodeKind::File),
            ],
        );
        state.publish(1, &cache);
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(1), 3, 1, 0, NodeKind::File),
                epoch_row(Some(0), 1, 0, 1, NodeKind::Directory),
                epoch_row(Some(0), 1, 0, 0, NodeKind::Directory),
            ],
        );
        state.publish(1, &cache);
        assert_eq!(cache.load_full()[&1].ids, vec![3]);
        assert_eq!(cache.load_full()[&1].child_epoch, 1);
        assert!(!state.is_removed(1));
        assert!(state.is_removed(2));
    }

    #[test]
    fn repeated_nested_refresh_never_tombstones_surviving_scope_ids() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 0, NodeKind::Directory),
                epoch_row(Some(0), 1, 0, 0, NodeKind::Directory),
                epoch_row(Some(1), 2, 0, 0, NodeKind::Directory),
            ],
        );
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(0), 1, 0, 1, NodeKind::Directory),
                epoch_row(Some(1), 3, 1, 0, NodeKind::Directory),
                epoch_row(Some(3), 4, 0, 0, NodeKind::File),
            ],
        );
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(1), 3, 1, 1, NodeKind::Directory),
                epoch_row(Some(3), 5, 1, 0, NodeKind::File),
            ],
        );
        state.publish(3, &cache);
        assert_eq!(cache.load_full()[&3].ids, vec![5]);
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(0), 1, 0, 2, NodeKind::Directory),
                epoch_row(Some(1), 6, 2, 0, NodeKind::Directory),
                epoch_row(Some(6), 7, 0, 0, NodeKind::File),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        state.publish(1, &cache);
        state.publish(6, &cache);
        assert_eq!(cache.load_full()[&1].ids, vec![6]);
        assert_eq!(cache.load_full()[&6].ids, vec![7]);
        assert!(!state.is_removed(1) && !state.is_removed(6));
        assert!(
            state.is_removed(2)
                && state.is_removed(3)
                && state.is_removed(4)
                && state.is_removed(5)
        );
        assert!(!state.directories.contains_key(&2) && !state.directories.contains_key(&3));
    }

    #[test]
    fn old_directory_rows_are_destroyed_in_bounded_background_batches() {
        let (mut state, cache) = cache_state();
        let mut rows = vec![epoch_row(None, 0, 0, 0, NodeKind::Directory)];
        rows.extend((1..1000).map(|id| epoch_row(Some(0), id, 0, 0, NodeKind::File)));
        upsert_epoch(&mut state, &cache, rows);
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(None, 0, 0, 1, NodeKind::Directory)],
        );
        assert_eq!(state.retired_rows.len(), 1);
        state.cleanup(&cache);
        assert_eq!(state.retired_rows.len(), 1);
        assert!(state.retired_rows.front().unwrap().rows.len() >= 999 - MAX_DRAIN);
        finish_cache_cleanup(&mut state, &cache);
        assert!(state.retired_rows.is_empty());
        assert!(!state.is_removed(0));
    }

    #[test]
    fn retired_id_ranges_merge_neighbors_without_overflow_or_live_gaps() {
        let mut ids = RetiredIds::default();
        assert!(ids.is_empty());
        for id in [2, 4, 6, 3, 5, 1, 1, 0] {
            ids.insert(id);
        }
        assert_eq!(ids.ranges, BTreeMap::from([(0, 6)]));
        for id in 0..=6 {
            assert!(ids.contains(&id));
        }
        assert!(!ids.contains(&7));
        for id in [NodeId::MAX, NodeId::MAX - 2, NodeId::MAX - 1, NodeId::MAX] {
            ids.insert(id);
        }
        assert_eq!(
            ids.ranges,
            BTreeMap::from([(0, 6), (NodeId::MAX - 2, NodeId::MAX)])
        );
        assert!(!ids.contains(&(NodeId::MAX - 3)));
        ids.clear();
        assert!(ids.is_empty());
        assert!(!ids.contains(&0));
    }

    #[test]
    fn repeated_refresh_reclaims_parent_history_and_compacts_retired_ids() {
        const WIDTH: NodeId = 1152;
        const ROUNDS: NodeId = 32;
        let (mut state, cache) = cache_state();
        for epoch in 0..ROUNDS {
            let first = epoch * WIDTH + 1;
            let mut rows = vec![epoch_row(None, 0, 0, u64::from(epoch), NodeKind::Directory)];
            rows.extend(
                (first..first + WIDTH)
                    .map(|id| epoch_row(Some(0), id, u64::from(epoch), 0, NodeKind::File)),
            );
            upsert_epoch(&mut state, &cache, rows);
            finish_cache_cleanup(&mut state, &cache);
            assert_eq!(state.parents.len(), WIDTH as usize + 1);
            assert_eq!(state.removed.ranges.len(), usize::from(epoch != 0));
            assert!(state.parents.capacity() <= WIDTH as usize * 8);
            state.publish(0, &cache);
            assert_eq!(cache.load_full()[&0].ids.len(), WIDTH as usize);
        }
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(
                None,
                0,
                0,
                u64::from(ROUNDS),
                NodeKind::Directory,
            )],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert_eq!(state.parents.len(), 1);
        assert_eq!(state.removed.ranges, BTreeMap::from([(1, ROUNDS * WIDTH)]));

        // Old known rows and never-delivered old rows cannot rebuild parent
        // history. An old scope update must not retire the surviving root.
        let delayed = ROUNDS * WIDTH + 1;
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(0), 1, 0, 0, NodeKind::File),
                epoch_row(Some(0), WIDTH + 1, 1, 0, NodeKind::File),
                epoch_row(Some(0), delayed, 0, 0, NodeKind::Directory),
                epoch_row(None, 0, 0, 0, NodeKind::Directory),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert_eq!(state.parents.len(), 1);
        assert_eq!(state.removed.ranges, BTreeMap::from([(1, delayed)]));
        assert!(!state.is_removed(0));
        upsert_epoch(
            &mut state,
            &cache,
            vec![epoch_row(
                Some(0),
                delayed + 1,
                u64::from(ROUNDS),
                0,
                NodeKind::File,
            )],
        );
        state.publish(0, &cache);
        assert_eq!(cache.load_full()[&0].ids, vec![delayed + 1]);
        assert_eq!(state.parents.len(), 2);
    }

    #[test]
    fn pruned_ancestors_still_reject_late_children_with_newer_epoch_witnesses() {
        let (mut state, cache) = cache_state();
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(None, 0, 0, 0, NodeKind::Directory),
                epoch_row(Some(0), 1, 0, 0, NodeKind::Directory),
                epoch_row(Some(1), 2, 0, 0, NodeKind::Directory),
                epoch_row(Some(2), 3, 0, 0, NodeKind::File),
            ],
        );
        state.apply(
            CacheCommand::Remove {
                generation: 1,
                ids: vec![1],
            },
            &cache,
        );
        state.cleanup(&cache);
        assert!(!state.parents.contains_key(&1));
        assert!(state.is_removed(3));
        upsert_epoch(
            &mut state,
            &cache,
            vec![
                epoch_row(Some(1), 4, 99, 0, NodeKind::Directory),
                epoch_row(Some(4), 5, 99, 0, NodeKind::File),
            ],
        );
        finish_cache_cleanup(&mut state, &cache);
        assert_eq!(state.parents.len(), 1);
        assert_eq!(state.removed.ranges, BTreeMap::from([(1, 5)]));
        assert_eq!(state.directories.len(), 1);
        state.publish(0, &cache);
        assert!(cache.load_full()[&0].ids.is_empty());
    }

    fn filter_request(mailbox: &FilterMailbox, revision: u64, query: &str, directories_only: bool) {
        mailbox.request.store(Some(Arc::new(FilterRequest {
            revision,
            directory: ROOT,
            query: Box::from(query),
            directories_only,
        })));
    }

    fn finish_filter(
        filtering: &mut FilterState,
        state: &mut CacheState,
        mailbox: &FilterMailbox,
    ) -> Arc<CachedSnapshot> {
        for _ in 0..16 {
            filtering.pump(state, mailbox);
            if filtering.task.is_none()
                && let Some(snapshot) = mailbox.snapshot.load_full()
            {
                return Arc::clone(&snapshot.view);
            }
        }
        panic!("filter did not finish");
    }

    #[test]
    fn fuzzy_subsequences_fold_unicode_and_keep_literal_spaces() {
        for (name, query, matches) in [
            ("Alpha Notes.txt", "ANT", true),
            ("alpha beta", "a b", true),
            ("beta alpha", "ab", false),
            ("abc", "acb", false),
            ("Éclair", "ÉCR", true),
            ("İSTANBUL", "i", true),
            ("İSTANBUL", "İs", true),
            ("Kelvin", "k", true),
            ("anything", "", true),
        ] {
            let query: String = query.chars().flat_map(char::to_lowercase).collect();
            assert_eq!(
                fuzzy_matches(OsStr::new(name), &query),
                matches,
                "{name}: {query}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = OsStr::from_bytes(b"a\xffb");
            assert!(fuzzy_matches(raw, "ab"));
            assert!(fuzzy_matches(raw, "\u{fffd}"));
        }
    }

    #[test]
    fn filtering_keeps_normal_order_and_matches_only_current_directory_names() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(None, ROOT, "root", 0, NodeKind::Directory),
                    cache_row(Some(ROOT), 1, "Alpha Notes.txt", 100, NodeKind::File),
                    cache_row(Some(ROOT), 2, "Annual", 90, NodeKind::Directory),
                    cache_row(Some(ROOT), 3, "Beta", 80, NodeKind::File),
                    cache_row(Some(2), 4, "AN nested", 1000, NodeKind::Directory),
                ],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        state.publish(2, &cache);
        let normal = Arc::clone(&state.published[&ROOT]);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "AN", false);
        let view = finish_filter(&mut filtering, &mut state, &mailbox);
        assert_eq!(view.ids, [1, 2]);
        assert_eq!(view.positions[&2], 1);
        assert!(Arc::ptr_eq(&normal, &state.published[&ROOT]));
        assert_eq!(normal.ids, [1, 2, 3]);
        filter_request(&mailbox, 2, "AN", true);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [2]);
        filter_request(&mailbox, 3, "", false);
        assert_eq!(
            finish_filter(&mut filtering, &mut state, &mailbox).ids,
            [1, 2, 3]
        );
    }

    #[test]
    fn filter_batches_cancel_superseded_queries_and_retire_results_on_worker() {
        let (mut state, cache) = cache_state();
        let mut rows = vec![cache_row(None, ROOT, "root", 0, NodeKind::Directory)];
        rows.extend((1..=FILTER_BATCH as NodeId + 5).map(|id| {
            cache_row(
                Some(ROOT),
                id,
                &format!("match-{id}"),
                u64::from(id),
                NodeKind::File,
            )
        }));
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows,
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "match", false);
        filtering.pump(&mut state, &mailbox);
        assert_eq!(filtering.task.as_ref().unwrap().next, FILTER_BATCH);
        assert!(mailbox.snapshot.load().is_none());
        filter_request(&mailbox, 2, "missing", false);
        let empty = finish_filter(&mut filtering, &mut state, &mailbox);
        assert!(empty.ids.is_empty());
        assert_eq!(mailbox.snapshot.load().as_ref().unwrap().revision, 2);
        filter_request(&mailbox, 3, "match", false);
        let held = finish_filter(&mut filtering, &mut state, &mailbox);
        assert_eq!(held.ids.len(), FILTER_BATCH + 5);
        mailbox.request.store(None);
        filtering.pump(&mut state, &mailbox);
        assert!(mailbox.snapshot.load().is_none());
        assert!(state.retired.iter().any(|view| Arc::ptr_eq(view, &held)));
        assert!(Arc::strong_count(&held) > 1);
    }

    #[test]
    fn filtering_refreshes_after_scan_growth_sort_changes_and_child_epochs() {
        let (mut state, cache) = cache_state();
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![
                    cache_row(None, ROOT, "root", 0, NodeKind::Directory),
                    cache_row(Some(ROOT), 1, "same-z", 10, NodeKind::File),
                ],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let mailbox = FilterMailbox::new();
        let mut filtering = FilterState::default();
        filter_request(&mailbox, 1, "same", false);
        assert_eq!(finish_filter(&mut filtering, &mut state, &mailbox).ids, [1]);
        state.apply(
            CacheCommand::Upsert {
                generation: 1,
                rows: vec![cache_row(Some(ROOT), 2, "same-a", 5, NodeKind::Directory)],
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        assert_eq!(
            finish_filter(&mut filtering, &mut state, &mailbox).ids,
            [1, 2]
        );
        state.apply(
            CacheCommand::Configure {
                generation: 2,
                config: OrderConfig {
                    sort_mode: SortMode::Name,
                    descending: false,
                    ..OrderConfig::default()
                },
                clear: false,
            },
            &cache,
        );
        state.publish(ROOT, &cache);
        let view = finish_filter(&mut filtering, &mut state, &mailbox);
        assert_eq!(view.generation, 2);
        assert_eq!(view.ids, [2, 1]);
        let mut refreshed = cache_row(None, ROOT, "root", 0, NodeKind::Directory);
        refreshed.child_epoch = 1;
        state.apply(
            CacheCommand::Upsert {
                generation: 2,
                rows: vec![refreshed],
            },
            &cache,
        );
        filtering.pump(&mut state, &mailbox);
        assert!(mailbox.snapshot.load().is_none());
        state.publish(ROOT, &cache);
        let view = finish_filter(&mut filtering, &mut state, &mailbox);
        assert_eq!(view.child_epoch, 1);
        assert!(view.ids.is_empty());
    }

    #[test]
    fn filter_lookup_rejects_previous_queries_and_sort_generations_immediately() {
        let (sender, _commands) = bounded(COMMAND_CAPACITY);
        let filter = Arc::new(FilterMailbox::new());
        let mut worker = CachedOrderWorker {
            sender,
            cache: Arc::new(ArcSwap::from_pointee(PublishedCache::new())),
            priorities: Arc::new([AtomicU32::new(ROOT), AtomicU32::new(NodeId::MAX)]),
            stop: Arc::new(AtomicBool::new(false)),
            generation: 1,
            pending_config: None,
            filter: Arc::clone(&filter),
            filter_revision: 0,
        };
        worker.set_filter(ROOT, "first", false);
        let view = Arc::new(CachedSnapshot {
            generation: 1,
            child_epoch: 0,
            ids: vec![1],
            positions: HashMap::from([(1, 0)]),
        });
        assert!(worker.snapshot_matches(ROOT, None));
        let mut published = PublishedCache::new();
        published.insert(ROOT, Arc::clone(&view));
        worker.cache.store(Arc::new(published));
        filter.snapshot.store(Some(Arc::new(FilteredSnapshot {
            revision: 1,
            source: Arc::clone(&view),
            view: Arc::clone(&view),
        })));
        assert!(worker.snapshot_matches(ROOT, Some(&view)));
        assert!(!worker.snapshot_matches(ROOT, None));
        assert!(worker.filtered_snapshot(ROOT).is_some());
        assert!(worker.filtered_snapshot_matches(ROOT, Some(&view)));
        assert!(!worker.filtered_snapshot_matches(ROOT, None));
        assert!(worker.filtered_snapshot(1).is_none());
        assert!(worker.filtered_snapshot_matches(1, None));
        assert!(!worker.filtered_snapshot_matches(1, Some(&view)));
        worker.generation = 2;
        assert!(worker.snapshot_matches(ROOT, None));
        assert!(!worker.snapshot_matches(ROOT, Some(&view)));
        assert!(worker.filtered_snapshot(ROOT).is_none());
        assert!(worker.filtered_snapshot_matches(ROOT, None));
        assert!(!worker.filtered_snapshot_matches(ROOT, Some(&view)));
        worker.generation = 1;
        worker.set_filter(ROOT, "second", false);
        assert!(worker.filtered_snapshot(ROOT).is_none());
        assert!(worker.filtered_snapshot_matches(ROOT, None));
        assert!(!worker.filtered_snapshot_matches(ROOT, Some(&view)));
        worker.clear_filter();
        assert!(worker.filtered_snapshot(ROOT).is_none());
        assert!(worker.filtered_snapshot_matches(ROOT, None));
    }
}
