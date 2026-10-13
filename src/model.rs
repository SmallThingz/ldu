use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    ops::{Index, IndexMut},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::types::{Entry, NodeId, NodeKind, ROOT, ScanEvent, Sizes};
pub use crate::types::{SizeMode, SortMode};

const NO_NODE: NodeId = NodeId::MAX;
const REMOVAL_CHECKS: usize = 256;
const NODE_CHUNK: usize = 512;
const REMOVAL_BUDGET: Duration = Duration::from_millis(4);

#[derive(Debug)]
struct HardlinkGroup {
    sizes: Sizes,
    metadata_generation: u64,
    owner: Option<NodeId>,
    members: Option<Box<HardlinkMembers>>,
}

// Only actual aliases allocate member state. The monotonic cursor never
// revisits tombstoned candidates when successive owners are removed.
#[derive(Debug, Default)]
struct HardlinkMembers {
    ids: Vec<NodeId>,
    next: usize,
    pending: bool,
}

#[derive(Debug)]
struct CleanupCursor {
    next: NodeId,
    siblings: bool,
}

/// A scanner's local IDs stay contiguous even when several scopes interleave.
/// Retaining these small chunked maps avoids destroying a whole scan on input.
#[derive(Debug)]
struct ScopeScan {
    scope: NodeId,
    ids: Vec<Vec<NodeId>>,
    len: usize,
    metadata_received: bool,
    finished: bool,
}

impl ScopeScan {
    fn new(scope: NodeId) -> Self {
        Self {
            scope,
            ids: vec![vec![scope]],
            len: 1,
            metadata_received: false,
            finished: false,
        }
    }

    fn get(&self, local: NodeId) -> Option<NodeId> {
        ((local as usize) < self.len)
            .then(|| self.ids[local as usize / NODE_CHUNK][local as usize % NODE_CHUNK])
    }

    fn push(&mut self, id: NodeId) {
        if self.len.is_multiple_of(NODE_CHUNK) {
            self.ids.push(Vec::with_capacity(NODE_CHUNK));
        }
        self.ids.last_mut().unwrap().push(id);
        self.len += 1;
    }
}

#[derive(Debug)]
pub struct Node {
    pub name: Arc<OsStr>,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    pub sizes: Sizes,
    /// Number of discovered descendants, excluding this node.
    pub items: u64,
    pub error_count: u64,
    pub complete: bool,
    pub hardlink_duplicate: bool,
    first_child: NodeId,
    last_child: NodeId,
    next_sibling: NodeId,
    previous_sibling: NodeId,
    flags: u8,
    pending_dirs: u32,
    pub child_epoch: u64,
    pub parent_epoch: u64,
    scan_generation: u64,
    suppression_cutoff: u64,
    suppressed_subtrees: u64,
}

impl Node {
    const REMOVED: u8 = 1;
    const READ_DONE: u8 = 2;
    const OVERFLOW_REPORTED: u8 = 4;
    const METADATA_UNCERTAIN: u8 = 8;
    const SCAN_SUPPRESSED: u8 = 16;

    fn new(name: Arc<OsStr>, parent: Option<NodeId>, kind: NodeKind, sizes: Sizes) -> Self {
        let complete = kind != NodeKind::Directory;
        Self {
            name,
            parent,
            kind,
            sizes,
            items: 0,
            error_count: 0,
            complete,
            hardlink_duplicate: false,
            first_child: NO_NODE,
            last_child: NO_NODE,
            next_sibling: NO_NODE,
            previous_sibling: NO_NODE,
            flags: if complete { Self::READ_DONE } else { 0 },
            pending_dirs: 0,
            child_epoch: 0,
            parent_epoch: 0,
            scan_generation: 0,
            suppression_cutoff: 0,
            suppressed_subtrees: 0,
        }
    }

    fn removed(&self) -> bool {
        self.flags & Self::REMOVED != 0
    }

    fn read_done(&self) -> bool {
        self.flags & Self::READ_DONE != 0
    }

    fn overflow_reported(&self) -> bool {
        self.flags & Self::OVERFLOW_REPORTED != 0
    }

    fn suppression_cutoff(&self) -> Option<u64> {
        (self.flags & Self::SCAN_SUPPRESSED != 0).then_some(self.suppression_cutoff)
    }

    fn set_suppression_cutoff(&mut self, cutoff: Option<u64>) {
        if let Some(cutoff) = cutoff {
            self.suppression_cutoff = cutoff;
            self.flags |= Self::SCAN_SUPPRESSED;
        } else {
            self.flags &= !Self::SCAN_SUPPRESSED;
        }
    }

    fn set_read_done(&mut self, done: bool) {
        if done {
            self.flags |= Self::READ_DONE;
        } else {
            self.flags &= !Self::READ_DONE;
        }
    }

    /// Keep this node's warning after inherited subtree errors are removed.
    fn mark_overflow(&mut self, overflow: bool, inherited: &mut u64) -> bool {
        if !overflow {
            return false;
        }
        let newly_reported = !self.overflow_reported();
        if newly_reported {
            self.flags |= Self::OVERFLOW_REPORTED;
            *inherited += 1;
        }
        self.error_count = self.error_count.saturating_add(*inherited);
        newly_reported
    }
}

#[derive(Debug, Default)]
struct ChildCounts {
    all: u32,
    hidden: u32,
    directories: u32,
    hidden_directories: u32,
}

#[derive(Debug, Default)]
struct DirectoryInfo {
    sizes: Sizes,
    children: ChildCounts,
}

// Fixed-size chunks keep growing scans from moving the whole model on the UI thread.
#[derive(Debug, Default)]
struct Nodes {
    chunks: Vec<Vec<Node>>,
    len: usize,
}

impl Nodes {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, index: usize) -> Option<&Node> {
        (index < self.len).then(|| &self[index])
    }

    fn push(&mut self, node: Node) {
        if self.len.is_multiple_of(NODE_CHUNK) {
            self.chunks.push(Vec::with_capacity(NODE_CHUNK));
        }
        self.chunks.last_mut().unwrap().push(node);
        self.len += 1;
    }
}

impl Index<usize> for Nodes {
    type Output = Node;

    fn index(&self, index: usize) -> &Node {
        &self.chunks[index / NODE_CHUNK][index % NODE_CHUNK]
    }
}

impl IndexMut<usize> for Nodes {
    fn index_mut(&mut self, index: usize) -> &mut Node {
        &mut self.chunks[index / NODE_CHUNK][index % NODE_CHUNK]
    }
}

#[derive(Debug)]
pub struct Model {
    root_path: PathBuf,
    nodes: Nodes,
    has_removals: bool,
    tombstone_name: Arc<OsStr>,
    directories: HashMap<NodeId, DirectoryInfo>,
    hardlink_indices: HashMap<NodeId, usize>,
    hardlink_groups: Vec<HardlinkGroup>,
    hardlink_owners: HashMap<NodeId, usize>,
    removals: VecDeque<usize>,
    cleanup: VecDeque<CleanupCursor>,
    #[cfg(test)]
    removal_checks: usize,
    scans: HashMap<u64, ScopeScan>,
    active_scopes: HashMap<NodeId, u64>,
    generation: u64,
    last_error_id: Option<NodeId>,
    pub revision: u64,
    pub finished: bool,
    pub cancelled: bool,
    pub last_error: Option<String>,
}

impl Model {
    pub fn new(root_path: PathBuf) -> Self {
        let name = root_path
            .file_name()
            .unwrap_or(root_path.as_os_str())
            .into();
        let mut nodes = Nodes::default();
        nodes.push(Node::new(name, None, NodeKind::Directory, Sizes::default()));
        Self {
            root_path,
            nodes,
            has_removals: false,
            tombstone_name: Arc::from(OsStr::new("")),
            directories: HashMap::new(),
            hardlink_indices: HashMap::new(),
            hardlink_groups: Vec::new(),
            hardlink_owners: HashMap::new(),
            removals: VecDeque::new(),
            cleanup: VecDeque::new(),
            #[cfg(test)]
            removal_checks: 0,
            scans: HashMap::from([(0, ScopeScan::new(ROOT))]),
            active_scopes: HashMap::from([(ROOT, 0)]),
            generation: 0,
            last_error_id: None,
            revision: 0,
            finished: false,
            cancelled: false,
            last_error: None,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Removed descendants remain addressable, but do not participate in totals or views.
    pub fn is_removed(&self, id: NodeId) -> bool {
        if id as usize >= self.nodes.len() {
            return true;
        }
        if !self.has_removals {
            return false;
        }
        let mut current = Some(id);
        while let Some(id) = current {
            let Some(node) = self.nodes.get(id as usize) else {
                return true;
            };
            if node.removed() {
                return true;
            }
            if let Some(parent) = node.parent {
                let Some(parent_node) = self.nodes.get(parent as usize) else {
                    return true;
                };
                if node.parent_epoch != parent_node.child_epoch {
                    return true;
                }
            }
            current = node.parent;
        }
        false
    }

    pub fn is_live(&self, id: NodeId) -> bool {
        !self.is_removed(id)
    }

    /// Freeze scanner contributions without hiding or subtracting known data.
    /// A cancelled confirmation therefore retains the exact partial view.
    pub fn suppress_scan(&mut self, id: NodeId) -> Vec<NodeId> {
        if !self.is_live(id) {
            return Vec::new();
        }
        let already_suppressed = self.nodes[id as usize].suppression_cutoff().is_some();
        self.nodes[id as usize].set_suppression_cutoff(Some(self.generation));
        let mut changed = Vec::new();
        let mut current = Some(id);
        while let Some(ancestor) = current {
            let node = &mut self.nodes[ancestor as usize];
            if !already_suppressed {
                node.suppressed_subtrees = node.suppressed_subtrees.saturating_add(1);
            }
            changed.push(ancestor);
            current = node.parent;
        }
        if !self.nodes[id as usize].complete {
            self.cancelled = true;
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    /// Interrupted deletion can invalidate previously complete totals. Keep
    /// their known values and completion tokens until reconciliation can run.
    pub fn mark_partial(&mut self, id: NodeId) -> Vec<NodeId> {
        let changed = self.suppress_scan(id);
        if !changed.is_empty() {
            self.cancelled = true;
        }
        changed
    }

    /// A completed removal can leave its parent's own inode bytes unchecked.
    /// Mark that uncertainty without suppressing unrelated scan contributions.
    pub fn mark_metadata_uncertain(&mut self, id: NodeId) -> Vec<NodeId> {
        if !self.is_live(id)
            || self.nodes[id as usize].kind != NodeKind::Directory
            || self.nodes[id as usize].flags & Node::METADATA_UNCERTAIN != 0
        {
            return Vec::new();
        }
        self.nodes[id as usize].flags |= Node::METADATA_UNCERTAIN;
        let mut changed = Vec::new();
        let mut current = Some(id);
        while let Some(ancestor) = current {
            let node = &mut self.nodes[ancestor as usize];
            node.suppressed_subtrees = node.suppressed_subtrees.saturating_add(1);
            changed.push(ancestor);
            current = node.parent;
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    /// Completion describes scanner work; partial totals also include later
    /// deletions whose filesystem changes have not yet been reconciled.
    pub fn has_partial_totals(&self, id: NodeId) -> bool {
        if !self.is_live(id) {
            return false;
        }
        let node = &self.nodes[id as usize];
        !node.complete || node.error_count != 0 || node.suppressed_subtrees != 0
    }

    /// Refresh one directory while retaining its identity and surrounding tree.
    /// Changing a child epoch hides all previous descendants in O(ancestor depth).
    pub fn begin_rescan(&mut self, id: NodeId, generation: u64) -> Vec<NodeId> {
        if !self.is_live(id)
            || self.nodes[id as usize].kind != NodeKind::Directory
            || generation <= self.generation
        {
            return Vec::new();
        }
        let node = &self.nodes[id as usize];
        let Some(epoch) = node.child_epoch.checked_add(1) else {
            return self.note_error(id, "directory refresh generation overflow".into());
        };
        let sizes = node.sizes;
        let items = node.items;
        let errors = node.error_count;
        let suppressed = node.suppressed_subtrees;
        let was_complete = node.complete;
        let parent = node.parent;
        let old_children = node.first_child;
        let mut changed = vec![id];
        let mut current = parent;
        while let Some(ancestor) = current {
            let node = &mut self.nodes[ancestor as usize];
            node.sizes.apparent = node.sizes.apparent.saturating_sub(sizes.apparent);
            node.sizes.allocated = node.sizes.allocated.saturating_sub(sizes.allocated);
            node.items = node.items.saturating_sub(items);
            node.error_count = node
                .error_count
                .saturating_sub(errors)
                .max(u64::from(node.overflow_reported()));
            node.suppressed_subtrees = node.suppressed_subtrees.saturating_sub(suppressed);
            changed.push(ancestor);
            current = node.parent;
        }
        let node = &mut self.nodes[id as usize];
        node.sizes = Sizes::default();
        node.items = 0;
        node.error_count = 0;
        node.complete = false;
        node.flags &= !(Node::READ_DONE | Node::OVERFLOW_REPORTED | Node::METADATA_UNCERTAIN);
        node.pending_dirs = 0;
        node.first_child = NO_NODE;
        node.last_child = NO_NODE;
        node.child_epoch = epoch;
        node.scan_generation = generation;
        node.set_suppression_cutoff(None);
        node.suppressed_subtrees = 0;
        self.directories.insert(id, DirectoryInfo::default());
        // Reopen only the completed part of the ancestor chain. An incomplete
        // ancestor already holds its parent's unique pending-directory token.
        if was_complete {
            let mut current = parent;
            while let Some(ancestor) = current {
                let node = &mut self.nodes[ancestor as usize];
                node.pending_dirs = node.pending_dirs.saturating_add(1);
                if !node.complete {
                    break;
                }
                node.complete = false;
                current = node.parent;
            }
        }
        self.generation = generation;
        self.scans.insert(generation, ScopeScan::new(id));
        self.active_scopes.insert(id, generation);
        self.has_removals = true;
        self.finished = false;
        self.cancelled = self.nodes[ROOT as usize].suppressed_subtrees != 0;
        self.schedule_cleanup(old_children, true);
        if self
            .last_error_id
            .is_some_and(|error_id| error_id == id || self.is_removed(error_id))
            || self.nodes[ROOT as usize].error_count == 0
        {
            self.last_error = None;
            self.last_error_id = None;
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    fn accepts_scan(&self, id: NodeId, generation: u64) -> bool {
        let Some(mut node) = self.nodes.get(id as usize) else {
            return false;
        };
        if node.scan_generation != generation {
            return false;
        }
        loop {
            if node.removed()
                || node
                    .suppression_cutoff()
                    .is_some_and(|cutoff| generation <= cutoff)
            {
                return false;
            }
            let Some(parent) = node.parent else {
                return true;
            };
            let Some(parent) = self.nodes.get(parent as usize) else {
                return false;
            };
            if node.parent_epoch != parent.child_epoch {
                return false;
            }
            node = parent;
        }
    }

    /// Unlink a subtree without walking or freeing it on the input thread.
    /// Its pending-directory token and known totals are released exactly once.
    pub fn remove(&mut self, id: NodeId) -> Vec<NodeId> {
        if id == ROOT || !self.is_live(id) {
            return Vec::new();
        }
        let node = &self.nodes[id as usize];
        let Some(parent) = node.parent else {
            return Vec::new();
        };
        let previous = node.previous_sibling;
        let next = node.next_sibling;
        let sizes = node.sizes;
        let items = node.items.saturating_add(1);
        let errors = node.error_count;
        let directory = node.kind == NodeKind::Directory;
        let hidden = node.name.as_encoded_bytes().starts_with(b".");
        let pending = directory && !node.complete;
        let suppressed = node.suppressed_subtrees;
        let counts = &mut self.directories.get_mut(&parent).unwrap().children;
        counts.all -= 1;
        counts.hidden -= u32::from(hidden);
        counts.directories -= u32::from(directory);
        counts.hidden_directories -= u32::from(directory && hidden);
        self.has_removals = true;
        self.nodes[id as usize].flags |= Node::REMOVED;
        if previous == NO_NODE {
            self.nodes[parent as usize].first_child = next;
        } else {
            self.nodes[previous as usize].next_sibling = next;
        }
        if next == NO_NODE {
            self.nodes[parent as usize].last_child = previous;
        } else {
            self.nodes[next as usize].previous_sibling = previous;
        }
        // Retain this node's sibling/parent links for in-flight view cursors.
        let mut changed = vec![id];
        let mut current = Some(parent);
        while let Some(ancestor) = current {
            let node = &mut self.nodes[ancestor as usize];
            node.sizes.apparent = node.sizes.apparent.saturating_sub(sizes.apparent);
            node.sizes.allocated = node.sizes.allocated.saturating_sub(sizes.allocated);
            node.items = node.items.saturating_sub(items);
            node.suppressed_subtrees = node.suppressed_subtrees.saturating_sub(suppressed);
            node.error_count = node
                .error_count
                .saturating_sub(errors)
                .max(u64::from(node.overflow_reported()));
            changed.push(ancestor);
            current = node.parent;
        }
        if pending {
            if let Some(count) = self.nodes[parent as usize].pending_dirs.checked_sub(1) {
                self.nodes[parent as usize].pending_dirs = count;
            } else {
                self.record_error(parent, "directory removal counter mismatch".into());
            }
            self.complete_ready(parent);
        }
        self.schedule_cleanup(id, false);
        if self.last_error_id.is_some_and(|id| self.is_removed(id))
            || self.nodes[ROOT as usize].error_count == 0
        {
            self.last_error = None;
            self.last_error_id = None;
        }
        if self.nodes[ROOT as usize].complete {
            self.cancelled = self.nodes[ROOT as usize].suppressed_subtrees != 0;
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    /// Retire only detached subtrees and repair only their charged alias groups.
    /// Both kinds of work share one bounded input-thread budget.
    pub fn pump_removals(&mut self) -> Vec<NodeId> {
        let started = Instant::now();
        let mut changed = Vec::new();
        for _ in 0..REMOVAL_CHECKS {
            if started.elapsed() >= REMOVAL_BUDGET {
                break;
            }
            if let Some(&index) = self.removals.front() {
                self.repair_hardlink(index, &mut changed);
            } else if !self.cleanup.is_empty() {
                self.pump_cleanup();
            } else {
                break;
            }
            #[cfg(test)]
            {
                self.removal_checks += 1;
            }
        }
        if !changed.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    fn repair_hardlink(&mut self, index: usize, changed: &mut Vec<NodeId>) {
        if self.hardlink_groups[index]
            .owner
            .is_some_and(|id| self.is_live(id))
        {
            self.finish_hardlink_repair(index);
            return;
        }
        if let Some(previous) = self.hardlink_groups[index].owner.take() {
            self.hardlink_owners.remove(&previous);
        }
        let members = self.hardlink_groups[index].members.as_mut().unwrap();
        let Some(&id) = members.ids.get(members.next) else {
            self.hardlink_groups[index].owner = None;
            self.finish_hardlink_repair(index);
            return;
        };
        members.next += 1;
        if self.is_live(id) {
            let sizes = self.hardlink_groups[index].sizes;
            self.hardlink_groups[index].owner = Some(id);
            self.hardlink_owners.insert(id, index);
            self.nodes[id as usize].hardlink_duplicate = false;
            self.add_totals(id, sizes, 0, 0, false, changed);
            self.finish_hardlink_repair(index);
        }
    }

    fn finish_hardlink_repair(&mut self, index: usize) {
        self.hardlink_groups[index]
            .members
            .as_mut()
            .unwrap()
            .pending = false;
        self.removals.pop_front();
    }

    pub fn has_pending_removals(&self) -> bool {
        !self.removals.is_empty() || !self.cleanup.is_empty()
    }

    fn schedule_cleanup(&mut self, next: NodeId, siblings: bool) {
        if next != NO_NODE {
            self.cleanup.push_back(CleanupCursor { next, siblings });
        }
    }

    fn pump_cleanup(&mut self) {
        let mut cursor = self.cleanup.pop_front().unwrap();
        let id = cursor.next;
        let node = &self.nodes[id as usize];
        let children = node.first_child;
        let next = node.next_sibling;
        // A refresh detaches a whole sibling chain; an individual removal
        // detaches just its target. Neither follows unrelated live siblings.
        if cursor.siblings && next != NO_NODE {
            cursor.next = next;
            self.cleanup.push_front(cursor);
        }
        if children != NO_NODE {
            self.cleanup.push_front(CleanupCursor {
                next: children,
                siblings: true,
            });
        }
        if let Some(index) = self.hardlink_owners.remove(&id) {
            let members = self.hardlink_groups[index].members.as_mut().unwrap();
            if !members.pending {
                members.pending = true;
                self.removals.push_back(index);
            }
        }
        let node = &mut self.nodes[id as usize];
        node.flags |= Node::REMOVED;
        node.name = Arc::clone(&self.tombstone_name);
        self.directories.remove(&id);
    }

    /// Refresh one surviving directory's own inode bytes after a deletion.
    pub fn update_directory_sizes(&mut self, id: NodeId, sizes: Sizes) -> Vec<NodeId> {
        if !self.is_live(id) || self.nodes[id as usize].kind != NodeKind::Directory {
            return Vec::new();
        }
        let uncertain = self.nodes[id as usize].flags & Node::METADATA_UNCERTAIN != 0;
        let old = std::mem::replace(&mut self.directories.entry(id).or_default().sizes, sizes);
        if old == sizes && !uncertain {
            return Vec::new();
        }
        let mut changed = Vec::new();
        if uncertain {
            self.nodes[id as usize].flags &= !Node::METADATA_UNCERTAIN;
            let mut current = Some(id);
            while let Some(ancestor) = current {
                let node = &mut self.nodes[ancestor as usize];
                node.suppressed_subtrees = node.suppressed_subtrees.saturating_sub(1);
                changed.push(ancestor);
                current = node.parent;
            }
            if self.nodes[ROOT as usize].complete {
                self.cancelled = self.nodes[ROOT as usize].suppressed_subtrees != 0;
            }
        }
        if old != sizes {
            self.replace_node_charge(id, old, sizes, &mut changed);
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    /// Transport failures belong to the originating scanner, not merely its path.
    pub fn note_scan_error(
        &mut self,
        scope: NodeId,
        generation: u64,
        error: String,
    ) -> Vec<NodeId> {
        if self
            .scans
            .get(&generation)
            .is_none_or(|scan| scan.scope != scope || scan.finished)
            || !self.accepts_scan(scope, generation)
        {
            return Vec::new();
        }
        self.note_error(scope, error)
    }

    /// A failed recursive deletion leaves a partial view until explicit refresh.
    pub fn note_error(&mut self, id: NodeId, error: String) -> Vec<NodeId> {
        if !self.is_live(id) {
            return Vec::new();
        }
        self.record_error(id, error);
        self.revision = self.revision.wrapping_add(1);
        let mut changed = Vec::new();
        let mut current = Some(id);
        while let Some(id) = current {
            changed.push(id);
            current = self.nodes[id as usize].parent;
        }
        changed
    }

    /// Whether the direct child list contains a row accepted by this view.
    pub fn has_children(&self, id: NodeId, directories_only: bool, show_hidden: bool) -> bool {
        if !self.is_live(id) {
            return false;
        }
        let Some(info) = self.directories.get(&id) else {
            return false;
        };
        let counts = &info.children;
        let (all, hidden) = if directories_only {
            (counts.directories, counts.hidden_directories)
        } else {
            (counts.all, counts.hidden)
        };
        all > if show_hidden { 0 } else { hidden }
    }

    /// Enumeration can finish before descendants finish contributing their sizes.
    pub fn listing_complete(&self, id: NodeId) -> bool {
        self.is_live(id) && self.node(id).read_done()
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        if !self.is_live(id) {
            return None;
        }
        self.nodes
            .get(id as usize)
            .and_then(|node| (node.first_child != NO_NODE).then_some(node.first_child))
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.nodes
            .get(id as usize)
            .and_then(|node| (node.next_sibling != NO_NODE).then_some(node.next_sibling))
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            model: self,
            next: if self.is_live(id) {
                self.nodes
                    .get(id as usize)
                    .map_or(NO_NODE, |node| node.first_child)
            } else {
                NO_NODE
            },
        }
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut current = id;
        while let Some(node) = self.nodes.get(current as usize) {
            let Some(parent) = node.parent else { break };
            names.push(node.name.as_ref());
            current = parent;
        }
        let mut path = self.root_path.clone();
        for name in names.into_iter().rev() {
            path.push(name);
        }
        path
    }

    pub fn sorted_children(
        &self,
        dir: NodeId,
        size_mode: SizeMode,
        sort_mode: SortMode,
        descending: bool,
    ) -> Vec<NodeId> {
        let mut children: Vec<_> = self.children(dir).collect();
        children.sort_unstable_by(|&left, &right| {
            let a = self.node(left);
            let b = self.node(right);
            let order = match sort_mode {
                SortMode::Size => match size_mode {
                    SizeMode::Allocated => a.sizes.allocated.cmp(&b.sizes.allocated),
                    SizeMode::Apparent => a.sizes.apparent.cmp(&b.sizes.apparent),
                },
                SortMode::Name => a.name.cmp(&b.name),
                SortMode::Items => a.items.cmp(&b.items),
            };
            let order = if descending { order.reverse() } else { order };
            order
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| left.cmp(&right))
        });
        children
    }

    pub fn apply(&mut self, event: ScanEvent) -> Vec<NodeId> {
        self.apply_scoped(ROOT, 0, event)
    }

    /// Apply scanner-local IDs within one retained directory and scan generation.
    /// Late messages still consume local IDs, but cannot resurrect old descendants.
    pub fn apply_scoped(
        &mut self,
        scope: NodeId,
        generation: u64,
        event: ScanEvent,
    ) -> Vec<NodeId> {
        let mut changed = Vec::new();
        let Some(scan) = self.scans.get(&generation) else {
            return changed;
        };
        if scan.scope != scope {
            return changed;
        }
        let scope_live = self.accepts_scan(scope, generation);
        if scan.finished {
            let event_live = match &event {
                ScanEvent::RootInfo { .. } => scope_live,
                ScanEvent::Entries { parent, .. } => scan
                    .get(*parent)
                    .map_or(scope_live, |id| self.accepts_scan(id, generation)),
                ScanEvent::DirectoryDone { id, .. } => scan
                    .get(*id)
                    .map_or(scope_live, |id| self.accepts_scan(id, generation)),
                ScanEvent::Finished { .. } => false,
            };
            if event_live {
                self.record_error(scope, "received scan data after completion".into());
                self.revision = self.revision.wrapping_add(1);
            }
            return changed;
        }
        self.revision = self.revision.wrapping_add(1);
        match event {
            ScanEvent::RootInfo {
                file_identity,
                kind,
                sizes,
                error,
            } => {
                if !scope_live {
                    return changed;
                }
                let scan = self.scans.get_mut(&generation).unwrap();
                if scan.metadata_received {
                    self.record_error(scope, "received root metadata twice".into());
                    return changed;
                }
                scan.metadata_received = true;
                if self.nodes[scope as usize].first_child != NO_NODE && kind != NodeKind::Directory
                {
                    self.record_error(scope, "non-directory root already has children".into());
                } else {
                    let node = &mut self.nodes[scope as usize];
                    let was_directory = node.kind == NodeKind::Directory;
                    let parent = node.parent;
                    let hidden = node.name.as_encoded_bytes().starts_with(b".");
                    node.kind = kind;
                    node.set_read_done(kind != NodeKind::Directory);
                    // complete_ready releases a retained directory's old parent token
                    // if replacement metadata now describes a file or symlink.
                    node.complete = false;
                    if was_directory
                        && kind != NodeKind::Directory
                        && let Some(parent) = parent
                    {
                        let counts = &mut self.directories.get_mut(&parent).unwrap().children;
                        counts.directories -= 1;
                        counts.hidden_directories -= u32::from(hidden);
                    }
                }
                let charged = if kind == NodeKind::File {
                    if let Some(group) = file_identity {
                        let charged = self.remember_hardlink(
                            group,
                            scope,
                            sizes,
                            generation,
                            true,
                            &mut changed,
                        );
                        self.nodes[scope as usize].hardlink_duplicate =
                            self.hardlink_groups[self.hardlink_indices[&group]].owner
                                != Some(scope);
                        charged
                    } else {
                        sizes
                    }
                } else {
                    self.nodes[scope as usize].hardlink_duplicate = false;
                    sizes
                };
                if kind == NodeKind::Directory {
                    self.directories.entry(scope).or_default().sizes = sizes;
                }
                let errors = u64::from(error.is_some());
                if let Some(error) = error {
                    self.last_error = Some(error);
                    self.last_error_id = Some(scope);
                }
                self.add_totals(scope, charged, 0, errors, false, &mut changed);
                self.complete_ready(scope);
                // Metadata can change kind and directory-only ordering even
                // when a zero-byte file or duplicate contributes no charge.
                if !changed.contains(&scope) {
                    changed.push(scope);
                }
            }
            ScanEvent::Entries {
                parent,
                mut entries,
            } => {
                let mapped_parent = self.scans[&generation].get(parent);
                let parent_live =
                    mapped_parent.map_or(scope_live, |id| self.accepts_scan(id, generation));
                let scan = self.scans.get_mut(&generation).unwrap();
                let mut next_id = self.nodes.len();
                let mut exhausted = false;
                let mut errors = Vec::new();
                // Reuse the incoming batch and borrow its scope map only once.
                entries.retain_mut(|entry| {
                    if exhausted {
                        return false;
                    }
                    if entry.id == NO_NODE || entry.id as usize != scan.len {
                        if parent_live {
                            errors.push(format!("out-of-order entry id {}", entry.id));
                        }
                        return false;
                    }
                    let Some(id) = NodeId::try_from(next_id).ok().filter(|&id| id != NO_NODE)
                    else {
                        if parent_live {
                            errors.push("node identifier overflow".into());
                        }
                        exhausted = true;
                        return false;
                    };
                    scan.push(id);
                    entry.id = id;
                    next_id += 1;
                    true
                });
                for error in errors {
                    self.record_error(scope, error);
                }
                if let Some(parent) = mapped_parent {
                    self.add_entries(parent, generation, entries, &mut changed);
                } else {
                    if parent_live {
                        self.record_error(scope, format!("entries have unknown parent {parent}"));
                    }
                    for entry in entries {
                        self.append_tombstone(scope, generation, entry);
                    }
                }
            }
            ScanEvent::DirectoryDone { id, error } => {
                let Some(id) = self.scans[&generation].get(id) else {
                    if scope_live {
                        self.record_error(scope, format!("finished unknown directory {id}"));
                    }
                    return changed;
                };
                if !self.accepts_scan(id, generation) || self.nodes[id as usize].read_done() {
                    return changed;
                }
                if let Some(error) = error {
                    self.record_error(id, error);
                }
                self.nodes[id as usize].flags |= Node::READ_DONE;
                self.complete_ready(id);
            }
            ScanEvent::Finished { cancelled } => {
                self.scans.get_mut(&generation).unwrap().finished = true;
                if self.active_scopes.get(&scope) == Some(&generation) {
                    self.active_scopes.remove(&scope);
                }
                if scope_live {
                    self.cancelled |= cancelled;
                }
                self.finished = self.active_scopes.is_empty();
                if self.finished {
                    if self.nodes[ROOT as usize].suppressed_subtrees != 0 {
                        self.cancelled = true;
                    } else if self.nodes[ROOT as usize].complete {
                        self.cancelled = false;
                    } else if scope_live && !self.cancelled {
                        self.record_error(ROOT, "scan ended with unfinished directories".into());
                    }
                }
            }
        }
        changed
    }

    fn append_tombstone(&mut self, parent: NodeId, generation: u64, entry: Entry) {
        if entry.id as usize != self.nodes.len() || entry.id == NO_NODE {
            return;
        }
        if let Some(group_id) = entry.hardlink_group {
            self.remember_hardlink(
                group_id,
                entry.id,
                entry.sizes,
                generation,
                false,
                &mut Vec::new(),
            );
        }
        let mut node = Node::new(
            Arc::clone(&self.tombstone_name),
            Some(parent),
            NodeKind::Unknown,
            Sizes::default(),
        );
        node.flags |= Node::REMOVED;
        node.parent_epoch = self.nodes[parent as usize].child_epoch;
        node.scan_generation = generation;
        self.has_removals = true;
        self.nodes.push(node);
    }

    fn add_entries(
        &mut self,
        parent: NodeId,
        generation: u64,
        entries: Vec<Entry>,
        changed: &mut Vec<NodeId>,
    ) {
        if !self.accepts_scan(parent, generation) {
            for entry in entries {
                self.append_tombstone(parent, generation, entry);
            }
            return;
        }
        if self.nodes[parent as usize].kind != NodeKind::Directory
            || self.nodes[parent as usize].read_done()
        {
            self.record_error(
                parent,
                "entries arrived after directory enumeration ended".into(),
            );
            for entry in entries {
                self.append_tombstone(parent, generation, entry);
            }
            return;
        }

        let batch_start = self.nodes.len() as NodeId;
        let mut sizes = Sizes::default();
        let mut items = 0u64;
        let mut errors = 0u64;
        let mut overflow = false;
        let mut counts = ChildCounts::default();
        for entry in entries {
            if entry.id as usize != self.nodes.len() || entry.id == NO_NODE {
                self.record_error(parent, format!("out-of-order entry id {}", entry.id));
                continue;
            }
            if entry.name.is_empty()
                || Path::new(entry.name.as_ref()).file_name() != Some(entry.name.as_ref())
            {
                self.record_error(parent, "entry has an invalid basename".into());
                self.append_tombstone(parent, generation, entry);
                continue;
            }
            let charged = if let Some(group_id) = entry.hardlink_group {
                // The charge for an earlier owner in this same batch has not yet
                // reached the parent. Correct that local sum before updating it.
                if let Some(&index) = self.hardlink_indices.get(&group_id)
                    && let Some(owner) = self.hardlink_groups[index].owner
                    && owner >= batch_start
                    && self.is_live(owner)
                    && generation >= self.hardlink_groups[index].metadata_generation
                {
                    let old = self.hardlink_groups[index].sizes;
                    if old != entry.sizes {
                        self.nodes[owner as usize].sizes = entry.sizes;
                        sizes.apparent = replace_charge(
                            sizes.apparent,
                            old.apparent,
                            entry.sizes.apparent,
                            &mut overflow,
                        );
                        sizes.allocated = replace_charge(
                            sizes.allocated,
                            old.allocated,
                            entry.sizes.allocated,
                            &mut overflow,
                        );
                        self.hardlink_groups[index].sizes = entry.sizes;
                        changed.push(owner);
                    }
                }
                self.remember_hardlink(group_id, entry.id, entry.sizes, generation, true, changed)
            } else if entry.hardlink_duplicate {
                Sizes::default()
            } else {
                entry.sizes
            };
            let mut node = Node::new(entry.name, Some(parent), entry.kind, charged);
            node.parent_epoch = self.nodes[parent as usize].child_epoch;
            node.scan_generation = generation;
            node.hardlink_duplicate = if let Some(group_id) = entry.hardlink_group {
                self.hardlink_groups[self.hardlink_indices[&group_id]].owner != Some(entry.id)
            } else {
                entry.hardlink_duplicate
            };
            if entry.kind == NodeKind::Directory {
                self.directories.insert(
                    entry.id,
                    DirectoryInfo {
                        sizes: charged,
                        ..DirectoryInfo::default()
                    },
                );
            }
            node.error_count = u64::from(entry.error.is_some());
            if let Some(error) = entry.error {
                self.last_error = Some(error);
                self.last_error_id = Some(entry.id);
            }
            let previous = self.nodes[parent as usize].last_child;
            node.previous_sibling = previous;
            if previous == NO_NODE {
                self.nodes[parent as usize].first_child = entry.id;
            } else {
                self.nodes[previous as usize].next_sibling = entry.id;
            }
            let parent_node = &mut self.nodes[parent as usize];
            parent_node.last_child = entry.id;
            if entry.kind == NodeKind::Directory {
                // Each pending child has a distinct ID, so this cannot exceed the arena limit.
                parent_node.pending_dirs = parent_node.pending_dirs.saturating_add(1);
            }
            sizes.apparent = sum(sizes.apparent, charged.apparent, &mut overflow);
            sizes.allocated = sum(sizes.allocated, charged.allocated, &mut overflow);
            items = sum(items, 1, &mut overflow);
            errors = sum(errors, node.error_count, &mut overflow);
            let hidden = node.name.as_encoded_bytes().starts_with(b".");
            let directory = node.kind == NodeKind::Directory;
            counts.all += 1;
            counts.hidden += u32::from(hidden);
            counts.directories += u32::from(directory);
            counts.hidden_directories += u32::from(directory && hidden);
            changed.push(entry.id);
            self.nodes.push(node);
        }
        if counts.all != 0 {
            let children = &mut self.directories.entry(parent).or_default().children;
            children.all += counts.all;
            children.hidden += counts.hidden;
            children.directories += counts.directories;
            children.hidden_directories += counts.hidden_directories;
        }
        // A wide directory batch traverses the ancestor chain only once.
        self.add_totals(parent, sizes, items, errors, overflow, changed);
    }

    fn remember_hardlink(
        &mut self,
        group_id: NodeId,
        id: NodeId,
        sizes: Sizes,
        generation: u64,
        live: bool,
        changed: &mut Vec<NodeId>,
    ) -> Sizes {
        let (index, new_group) = if let Some(&index) = self.hardlink_indices.get(&group_id) {
            (index, false)
        } else {
            let index = self.hardlink_groups.len();
            self.hardlink_indices.insert(group_id, index);
            self.hardlink_groups.push(HardlinkGroup {
                sizes,
                metadata_generation: 0,
                owner: None,
                members: None,
            });
            (index, true)
        };
        let owner_live = self.hardlink_groups[index]
            .owner
            .is_some_and(|owner| self.is_live(owner));
        // A delayed alias in an untouched older scan remains a valid row,
        // but cannot replace metadata already observed by a newer refresh.
        if live && generation >= self.hardlink_groups[index].metadata_generation {
            let old = self.hardlink_groups[index].sizes;
            if owner_live && old != sizes {
                let owner = self.hardlink_groups[index].owner.unwrap();
                self.replace_node_charge(owner, old, sizes, changed);
            }
            self.hardlink_groups[index].sizes = sizes;
            self.hardlink_groups[index].metadata_generation = generation;
        }
        let group = &mut self.hardlink_groups[index];
        // The initial member is already stored as owner. Once tombstoned it
        // can never return, so repair only needs later aliases. Ordinary files
        // allocate neither member state nor entries in the repair owner index.
        if !new_group {
            group
                .members
                .get_or_insert_with(Default::default)
                .ids
                .push(id);
        }
        let charged = if live && !owner_live {
            if let Some(previous) = group.owner {
                self.hardlink_owners.remove(&previous);
            }
            group.owner = Some(id);
            group.sizes
        } else {
            Sizes::default()
        };
        if group.members.is_some() && (owner_live || live) {
            self.hardlink_owners.insert(group.owner.unwrap(), index);
        }
        charged
    }

    fn replace_node_charge(
        &mut self,
        start: NodeId,
        old: Sizes,
        sizes: Sizes,
        changed: &mut Vec<NodeId>,
    ) {
        let mut overflow = false;
        let mut inherited_overflows = 0;
        let mut reported = None;
        let mut current = Some(start);
        while let Some(id) = current {
            let node = &mut self.nodes[id as usize];
            node.sizes.apparent = replace_charge(
                node.sizes.apparent,
                old.apparent,
                sizes.apparent,
                &mut overflow,
            );
            node.sizes.allocated = replace_charge(
                node.sizes.allocated,
                old.allocated,
                sizes.allocated,
                &mut overflow,
            );
            if node.mark_overflow(overflow, &mut inherited_overflows) && reported.is_none() {
                reported = Some(id);
            }
            changed.push(id);
            current = node.parent;
        }
        if let Some(id) = reported {
            self.report_overflow(id);
        }
    }

    fn add_totals(
        &mut self,
        start: NodeId,
        sizes: Sizes,
        items: u64,
        errors: u64,
        mut overflow: bool,
        changed: &mut Vec<NodeId>,
    ) {
        let mut current = Some(start);
        let mut inherited_overflows = 0;
        let mut reported = None;
        while let Some(id) = current {
            if sizes.apparent != 0 || sizes.allocated != 0 || items != 0 {
                changed.push(id);
            }
            let node = &mut self.nodes[id as usize];
            node.sizes.apparent = sum(node.sizes.apparent, sizes.apparent, &mut overflow);
            node.sizes.allocated = sum(node.sizes.allocated, sizes.allocated, &mut overflow);
            node.items = sum(node.items, items, &mut overflow);
            node.error_count = sum(node.error_count, errors, &mut overflow);
            if node.mark_overflow(overflow, &mut inherited_overflows) && reported.is_none() {
                reported = Some(id);
            }
            current = node.parent;
        }
        if let Some(id) = reported {
            self.report_overflow(id);
        }
    }

    fn record_error(&mut self, id: NodeId, error: String) {
        if self.is_removed(id) {
            return;
        }
        self.last_error = Some(error);
        self.last_error_id = Some(id);
        let mut current = Some(id);
        while let Some(id) = current {
            let node = &mut self.nodes[id as usize];
            node.error_count = node.error_count.saturating_add(1);
            current = node.parent;
        }
    }

    fn report_overflow(&mut self, id: NodeId) {
        self.last_error = Some("disk usage counter overflow; totals are saturated".into());
        self.last_error_id = Some(id);
    }

    fn complete_ready(&mut self, start: NodeId) {
        let mut current = start;
        loop {
            let node = &mut self.nodes[current as usize];
            if node.complete || !node.read_done() || node.pending_dirs != 0 {
                break;
            }
            node.complete = true;
            let Some(parent) = node.parent else { break };
            let parent_node = &mut self.nodes[parent as usize];
            let Some(pending) = parent_node.pending_dirs.checked_sub(1) else {
                self.record_error(parent, "directory completion counter mismatch".into());
                break;
            };
            parent_node.pending_dirs = pending;
            current = parent;
        }
    }
}

fn replace_charge(total: u64, old: u64, new: u64, overflow: &mut bool) -> u64 {
    if new >= old {
        sum(total, new - old, overflow)
    } else {
        total.saturating_sub(old - new)
    }
}

fn sum(left: u64, right: u64, overflow: &mut bool) -> u64 {
    match left.checked_add(right) {
        Some(value) => value,
        None => {
            *overflow = true;
            u64::MAX
        }
    }
}

pub struct Children<'a> {
    model: &'a Model,
    next: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next;
        let node = self.model.nodes.get(id as usize)?;
        self.next = node.next_sibling;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: NodeId, name: &str, kind: NodeKind, apparent: u64, allocated: u64) -> Entry {
        Entry {
            id,
            name: Arc::from(OsStr::new(name)),
            kind,
            sizes: Sizes {
                apparent,
                allocated,
            },
            hardlink_duplicate: false,
            hardlink_group: None,
            error: None,
        }
    }

    fn model() -> Model {
        let mut model = Model::new(PathBuf::from("fixture"));
        model.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Directory,
            sizes: Sizes {
                apparent: 2,
                allocated: 4,
            },
            error: None,
        });
        model
    }

    #[test]
    fn suppression_cutoffs_preserve_zero_and_maximum_scan_generations() {
        let mut model = model();
        assert!(model.accepts_scan(ROOT, 0));
        model.suppress_scan(ROOT);
        assert_eq!(model.node(ROOT).suppression_cutoff(), Some(0));
        assert!(!model.accepts_scan(ROOT, 0));
        model.suppress_scan(ROOT);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 1);

        model.begin_rescan(ROOT, u64::MAX);
        assert_eq!(model.node(ROOT).suppression_cutoff(), None);
        assert!(model.accepts_scan(ROOT, u64::MAX));
        assert!(!model.accepts_scan(ROOT, 0));
        model.suppress_scan(ROOT);
        assert_eq!(model.node(ROOT).suppression_cutoff(), Some(u64::MAX));
        assert!(!model.accepts_scan(ROOT, u64::MAX));
        model.suppress_scan(ROOT);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
    }

    #[test]
    fn arena_growth_preserves_nodes_and_removed_ancestors_hide_descendants() {
        let mut model = model();
        let root_address = model.node(ROOT) as *const Node;
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "branch", NodeKind::Directory, 3, 4),
                entry(2, "survivor", NodeKind::File, 7, 8),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: (3..(NODE_CHUNK * 3 + 1) as NodeId)
                .map(|id| entry(id, &format!("file-{id}"), NodeKind::File, 1, 4))
                .collect(),
        });
        assert_eq!(model.nodes.chunks.len(), 4);
        assert_eq!(model.node(ROOT) as *const Node, root_address);
        assert!(model.nodes.get(model.len()).is_none());
        assert!(!model.is_live(model.len() as NodeId));
        assert!(!model.has_removals);
        for id in [3, NODE_CHUNK as NodeId, (NODE_CHUNK * 3) as NodeId] {
            assert!(model.is_live(id));
            assert_eq!(model.node(id).parent, Some(1));
            assert_eq!(
                model.node(id).name.as_ref(),
                OsStr::new(&format!("file-{id}"))
            );
        }
        model.remove(1);
        assert!(model.has_removals);
        assert!(!model.is_live(3));
        assert!(!model.is_live((NODE_CHUNK * 3) as NodeId));
        assert!(model.is_live(2));
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![2]);
        assert_eq!(model.node(ROOT).items, 1);
    }

    #[test]
    fn child_witnesses_follow_filters_unlink_and_enumeration_without_descendants() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, ".file", NodeKind::File, 1, 4),
                entry(2, "file", NodeKind::File, 1, 4),
                entry(3, ".directory", NodeKind::Directory, 1, 4),
                entry(4, "directory", NodeKind::Directory, 1, 4),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 4,
            entries: vec![entry(5, "nested", NodeKind::Directory, 1, 4)],
        });
        assert!(model.has_children(ROOT, true, false));
        assert!(model.has_children(ROOT, false, false));
        assert!(model.has_children(4, true, false));
        assert!(!model.has_children(99, false, true));
        assert!(!model.listing_complete(ROOT));
        assert!(!model.listing_complete(99));
        model.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        assert!(model.listing_complete(ROOT));
        assert!(!model.node(ROOT).complete);
        model.remove(4);
        assert!(!model.has_children(ROOT, true, false));
        assert!(model.has_children(ROOT, true, true));
        assert!(!model.has_children(4, true, true));
        assert!(!model.listing_complete(4));
        model.remove(2);
        assert!(!model.has_children(ROOT, false, false));
        assert!(model.has_children(ROOT, false, true));
        model.remove(3);
        assert!(!model.has_children(ROOT, true, true));
        model.remove(3);
        assert!(model.has_children(ROOT, false, true));
        model.remove(1);
        assert!(!model.has_children(ROOT, false, true));

        let mut early = Model::new(PathBuf::from("early"));
        early.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "known", NodeKind::File, 1, 4)],
        });
        early.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Directory,
            sizes: Sizes {
                apparent: 2,
                allocated: 4,
            },
            error: None,
        });
        early.update_directory_sizes(
            ROOT,
            Sizes {
                apparent: 3,
                allocated: 8,
            },
        );
        assert!(early.has_children(ROOT, false, false));
        assert!(!early.has_children(ROOT, true, true));
        assert_eq!(early.node(ROOT).sizes.apparent, 4);
        assert_eq!(early.node(ROOT).sizes.allocated, 12);
        for kind in [NodeKind::File, NodeKind::Symlink, NodeKind::Unknown] {
            let mut leaf = Model::new(PathBuf::from("leaf"));
            leaf.apply(ScanEvent::RootInfo {
                file_identity: None,
                kind,
                sizes: Sizes::default(),
                error: (kind == NodeKind::Unknown).then(|| "missing".into()),
            });
            assert!(leaf.listing_complete(ROOT));
            assert!(!leaf.has_children(ROOT, false, true));
        }
    }

    #[test]
    fn live_totals_and_completion_follow_descendants() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "dir", NodeKind::Directory, 3, 4),
                entry(2, "leaf", NodeKind::File, 5, 8),
            ],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        assert!(!model.node(ROOT).complete);
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(3, "nested", NodeKind::Directory, 7, 8)],
        });
        model.apply(ScanEvent::DirectoryDone { id: 1, error: None });
        assert!(!model.node(1).complete);
        model.apply(ScanEvent::Entries {
            parent: 3,
            entries: vec![entry(4, "data", NodeKind::File, 11, 12)],
        });
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 28,
                allocated: 36
            }
        );
        assert_eq!(
            model.node(1).sizes,
            Sizes {
                apparent: 21,
                allocated: 24
            }
        );
        assert_eq!(model.node(ROOT).items, 4);
        assert_eq!(model.path(4), PathBuf::from("fixture/dir/nested/data"));
        model.apply(ScanEvent::DirectoryDone { id: 3, error: None });
        assert!(model.node(ROOT).complete);
        assert!(model.node(1).complete);
        model.apply(ScanEvent::DirectoryDone { id: 3, error: None });
        assert!(model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn unfinished_rows_sort_normally_and_changes_identify_affected_rows() {
        let mut model = model();
        let changed = model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "z-pending", NodeKind::Directory, 100, 100),
                entry(2, "a-file", NodeKind::File, 1, 1),
                entry(3, "m-dir", NodeKind::Directory, 2, 2),
            ],
        });
        assert_eq!(changed, vec![1, 2, 3, ROOT]);
        model.apply(ScanEvent::DirectoryDone { id: 3, error: None });
        assert_eq!(
            model.sorted_children(ROOT, SizeMode::Allocated, SortMode::Size, true),
            vec![1, 3, 2]
        );
        assert_eq!(
            model.sorted_children(ROOT, SizeMode::Allocated, SortMode::Size, false),
            vec![2, 3, 1]
        );
        assert_eq!(
            model.sorted_children(ROOT, SizeMode::Allocated, SortMode::Name, false),
            vec![2, 3, 1]
        );
        assert_eq!(model.first_child(ROOT), Some(1));
        assert_eq!(model.next_sibling(1), Some(2));
        let changed = model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(4, "child", NodeKind::File, 10, 10)],
        });
        assert_eq!(changed, vec![4, 1, ROOT]);
        assert_eq!(
            model.sorted_children(ROOT, SizeMode::Allocated, SortMode::Items, true),
            vec![1, 2, 3]
        );
        assert_eq!(model.node(2).name.as_ref(), OsStr::new("a-file"));
    }

    #[test]
    fn duplicates_errors_and_cancellation_preserve_known_totals() {
        let mut model = model();
        let mut duplicate = entry(2, "link", NodeKind::File, 100, 512);
        duplicate.hardlink_duplicate = true;
        let mut unreadable = entry(3, "unreadable", NodeKind::Unknown, 0, 0);
        unreadable.error = Some("metadata denied".into());
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "original", NodeKind::File, 100, 512),
                duplicate,
                unreadable,
            ],
        });
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 102,
                allocated: 516
            }
        );
        assert_eq!(model.node(2).sizes, Sizes::default());
        assert!(model.node(3).complete);
        assert_eq!(model.node(ROOT).error_count, 1);
        model.apply(ScanEvent::Finished { cancelled: true });
        assert!(model.finished && model.cancelled);
        assert!(!model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 1);
    }

    #[test]
    fn malformed_events_and_overflow_are_visible_without_panics() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "huge", NodeKind::File, u64::MAX, u64::MAX)],
        });
        assert_eq!(model.node(ROOT).sizes.apparent, u64::MAX);
        assert_eq!(model.node(ROOT).error_count, 1);
        model.apply(ScanEvent::Entries {
            parent: 999,
            entries: vec![],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: 999,
            error: None,
        });
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(9, "wrong-id", NodeKind::File, 1, 1)],
        });
        assert_eq!(model.len(), 2);
        assert_eq!(model.node(ROOT).error_count, 4);
        model.apply(ScanEvent::Finished { cancelled: false });
        assert!(!model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 5);

        let mut nested = Model::new(PathBuf::from("overflow"));
        nested.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Directory,
            sizes: Sizes::default(),
            error: None,
        });
        nested.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "batch-parent", NodeKind::Directory, 0, 0),
                entry(2, "cross-batch", NodeKind::Directory, 0, 0),
                entry(3, "unaffected", NodeKind::Directory, 0, 0),
            ],
        });
        nested.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![
                entry(4, "batch", NodeKind::Directory, 0, 0),
                entry(5, "ordinary", NodeKind::File, 0, 10),
            ],
        });
        let large = i64::MAX as u64;
        nested.apply(ScanEvent::Entries {
            parent: 4,
            entries: vec![
                entry(6, "large-a", NodeKind::File, large, 0),
                entry(7, "large-b", NodeKind::File, large, 0),
                entry(8, "large-c", NodeKind::File, large, 0),
            ],
        });
        // The capped batch fits into its zero-sized parent and root, but both
        // still need their own persistent warning about the lost precision.
        assert_eq!(nested.node(4).sizes.apparent, u64::MAX);
        assert_eq!(nested.node(4).error_count, 1);
        assert_eq!(nested.node(1).error_count, 2);
        assert_eq!(nested.node(ROOT).error_count, 3);
        assert_eq!(nested.node(2).error_count, 0);
        assert_eq!(nested.node(3).error_count, 0);
        for id in [4, 1] {
            nested.apply(ScanEvent::DirectoryDone { id, error: None });
            assert!(nested.node(id).complete);
        }
        for id in [9, 10] {
            nested.apply(ScanEvent::Entries {
                parent: 2,
                entries: vec![entry(id, &format!("large-{id}"), NodeKind::File, large, 0)],
            });
        }
        assert_eq!(nested.node(2).sizes.apparent, u64::MAX - 1);
        assert_eq!(nested.node(2).error_count, 0);
        nested.apply(ScanEvent::Entries {
            parent: 2,
            entries: vec![entry(11, "overflow", NodeKind::File, 2, 0)],
        });
        assert_eq!(nested.node(2).error_count, 1);
        assert_eq!(nested.node(ROOT).error_count, 4);
        assert_eq!(nested.node(3).error_count, 0);

        nested.remove(5);
        assert_eq!(nested.node(1).error_count, 2);
        assert_eq!(nested.node(1).sizes.allocated, 0);
        nested.remove(4);
        assert_eq!(nested.node(1).sizes.apparent, 0);
        assert_eq!(nested.node(1).error_count, 1);
        assert_eq!(nested.node(ROOT).error_count, 3);
        nested.remove(1);
        assert_eq!(nested.node(ROOT).error_count, 2);
        nested.remove(2);
        assert_eq!(nested.node(ROOT).error_count, 1);
        nested.apply(ScanEvent::DirectoryDone { id: 3, error: None });
        nested.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        nested.apply(ScanEvent::Finished { cancelled: false });
        assert!(nested.node(ROOT).complete);
        assert_eq!(nested.node(ROOT).error_count, 1);
        assert!(nested.node(3).complete);
        assert_eq!(nested.node(3).error_count, 0);

        let mut boundary = Model::new(PathBuf::from("exact-max"));
        boundary.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Directory,
            sizes: Sizes::default(),
            error: None,
        });
        boundary.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "dir", NodeKind::Directory, 0, 0)],
        });
        boundary.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(2, "exact-max", NodeKind::File, u64::MAX, u64::MAX)],
        });
        assert_eq!(boundary.node(1).error_count, 0);
        assert_eq!(boundary.node(ROOT).error_count, 0);
        assert_eq!(
            boundary.update_directory_sizes(
                1,
                Sizes {
                    apparent: 1,
                    allocated: 1
                }
            ),
            vec![1, ROOT]
        );
        assert_eq!(boundary.node(1).error_count, 1);
        assert_eq!(boundary.node(ROOT).error_count, 2);
        boundary.update_directory_sizes(1, Sizes::default());
        boundary.remove(2);
        assert_eq!(boundary.node(1).error_count, 1);
        assert_eq!(boundary.node(ROOT).error_count, 2);
        boundary.remove(1);
        assert_eq!(boundary.node(ROOT).error_count, 1);
    }

    #[test]
    fn malformed_scoped_batches_preserve_following_ids_and_child_counts() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "scope", NodeKind::Directory, 0, 0),
                entry(2, "sibling", NodeKind::File, 1, 4),
            ],
        });
        model.begin_rescan(1, 1);
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(2, "out-of-order", NodeKind::File, 100, 100),
                    entry(1, ".hidden", NodeKind::File, 2, 4),
                    entry(1, "repeated", NodeKind::File, 100, 100),
                    entry(2, "invalid/name", NodeKind::File, 100, 100),
                    entry(NO_NODE, "sentinel", NodeKind::File, 100, 100),
                    entry(3, "directory", NodeKind::Directory, 3, 4),
                ],
            },
        );
        assert_eq!(model.len(), 6);
        assert_eq!(model.children(1).collect::<Vec<_>>(), vec![3, 5]);
        assert!(model.is_removed(4));
        assert_eq!(model.node(1).sizes.apparent, 5);
        assert_eq!(model.node(1).items, 2);
        assert_eq!(model.node(1).error_count, 4);
        assert_eq!(model.node(ROOT).error_count, 4);
        assert_eq!(model.node(ROOT).items, 4);
        assert!(model.has_children(1, true, false));
        model.remove(5);
        assert!(model.has_children(1, false, true));
        assert!(!model.has_children(1, false, false));
        assert!(!model.has_children(1, true, true));
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(4, "following", NodeKind::File, 7, 8)],
            },
        );
        assert_eq!(model.children(1).collect::<Vec<_>>(), vec![3, 6]);
        assert_eq!(model.path(6), PathBuf::from("fixture/scope/following"));
        assert_eq!(model.node(1).sizes.apparent, 9);
        assert_eq!(model.node(1).items, 2);
        assert_eq!(model.node(1).error_count, 4);
        assert!(model.has_children(1, false, false));
    }

    #[test]
    fn removing_an_inflight_subtree_preserves_ids_and_live_sibling_completion() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "removed", NodeKind::Directory, 3, 4),
                entry(2, "live", NodeKind::Directory, 7, 8),
                entry(3, "leaf", NodeKind::File, 11, 12),
            ],
        });
        let mut unreadable = entry(5, "unreadable", NodeKind::Unknown, 13, 16);
        unreadable.error = Some("old metadata error".into());
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(4, "nested", NodeKind::Directory, 5, 8), unreadable],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        model.apply(ScanEvent::DirectoryDone { id: 1, error: None });
        assert_eq!(model.remove(1), vec![1, ROOT]);
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(model.next_sibling(1), Some(2));
        assert!(!model.is_live(4));
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 20,
                allocated: 24
            }
        );
        assert_eq!(model.node(ROOT).items, 2);
        assert_eq!(model.node(ROOT).error_count, 0);
        assert!(model.last_error.is_none());
        assert!(!model.node(ROOT).complete);
        let mut late_error = entry(7, "gone", NodeKind::Unknown, 1000, 1000);
        late_error.error = Some("late metadata error".into());
        assert!(
            model
                .apply(ScanEvent::Entries {
                    parent: 4,
                    entries: vec![
                        entry(6, "late-dir", NodeKind::Directory, 17, 20),
                        late_error
                    ],
                })
                .is_empty()
        );
        model.apply(ScanEvent::Entries {
            parent: 6,
            entries: vec![entry(8, "later", NodeKind::File, 1000, 1000)],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: 4,
            error: Some("late enumeration error".into()),
        });
        model.apply(ScanEvent::DirectoryDone {
            id: 6,
            error: Some("later error".into()),
        });
        assert_eq!(model.len(), 9);
        assert!(!model.is_live(8));
        assert!(!model.has_children(1, false, true));
        assert!(!model.has_children(4, true, true));
        assert!(!model.has_children(6, false, true));
        assert!(model.has_children(ROOT, true, true));
        assert!(model.node(6).name.is_empty());
        assert_eq!(model.node(ROOT).error_count, 0);
        assert!(model.last_error.is_none());
        assert_eq!(model.remove(1), Vec::<NodeId>::new());
        assert_eq!(model.remove(4), Vec::<NodeId>::new());
        assert_eq!(
            model.apply(ScanEvent::Entries {
                parent: 2,
                entries: vec![entry(9, "survivor", NodeKind::File, 19, 24)],
            }),
            vec![9, 2, ROOT]
        );
        model.apply(ScanEvent::DirectoryDone { id: 2, error: None });
        model.apply(ScanEvent::DirectoryDone { id: 4, error: None });
        model.apply(ScanEvent::Finished { cancelled: false });
        assert!(model.node(ROOT).complete);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 39,
                allocated: 48
            }
        );
        assert_eq!(model.node(ROOT).items, 3);
        assert_eq!(model.node(ROOT).error_count, 0);
        assert_eq!(model.path(9), PathBuf::from("fixture/live/survivor"));
    }

    #[test]
    fn overlapping_refresh_and_removal_cleanup_keeps_the_unfinished_prefix() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "refresh", NodeKind::Directory, 0, 0)],
        });
        let later = (REMOVAL_CHECKS * 2 + 2) as NodeId;
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: (2..later)
                .map(|id| entry(id, "stale", NodeKind::File, 1, 4))
                .collect(),
        });
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(later, "remove", NodeKind::Directory, 0, 0)],
        });
        model.apply(ScanEvent::Entries {
            parent: later,
            entries: vec![entry(later + 1, "removed-child", NodeKind::File, 1, 4)],
        });
        model.begin_rescan(1, 1);
        model.pump_removals();
        assert!(model.has_pending_removals());
        model.remove(later);
        finish_removals(&mut model);
        for id in 2..=later + 1 {
            assert!(model.is_removed(id));
            assert!(model.node(id).name.is_empty(), "uncleaned node {id}");
        }
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![1]);
        assert_eq!(model.node(1).name.as_ref(), OsStr::new("refresh"));
        assert_eq!(model.node(ROOT).items, 1);
    }

    #[test]
    fn removal_unlinks_first_middle_last_and_subtracts_descendants_once() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "first", NodeKind::File, 10, 12),
                entry(2, "middle", NodeKind::Directory, 3, 4),
                entry(3, "last", NodeKind::File, 20, 24),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 2,
            entries: vec![entry(4, "child", NodeKind::File, 7, 8)],
        });
        model.apply(ScanEvent::DirectoryDone { id: 2, error: None });
        assert_eq!(model.remove(4), vec![4, 2, ROOT]);
        assert_eq!(
            model.node(2).sizes,
            Sizes {
                apparent: 3,
                allocated: 4
            }
        );
        assert_eq!(model.node(2).items, 0);
        model.remove(2);
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![1, 3]);
        model.remove(1);
        assert_eq!(model.first_child(ROOT), Some(3));
        model.remove(3);
        assert_eq!(model.first_child(ROOT), None);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 2,
                allocated: 4
            }
        );
        assert_eq!(model.node(ROOT).items, 0);
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(5, "new", NodeKind::File, 30, 32)],
        });
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![5]);
        assert!(model.remove(ROOT).is_empty());
        assert!(model.remove(999).is_empty());
        model.apply(ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        });
        assert!(model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    fn linked(id: NodeId, name: &str, group: NodeId) -> Entry {
        let mut value = entry(id, name, NodeKind::File, 100, 512);
        value.hardlink_group = Some(group);
        value.hardlink_duplicate = id != group;
        value
    }

    fn finish_removals(model: &mut Model) {
        for _ in 0..100 {
            if !model.has_pending_removals() {
                return;
            }
            model.pump_removals();
        }
        panic!("bounded hardlink repair did not finish");
    }

    fn assert_removal_work_stays_settled(model: &mut Model) {
        assert!(!model.has_pending_removals());
        let revision = model.revision();
        let checks = model.removal_checks;
        let sizes = model.node(ROOT).sizes;
        for _ in 0..64 {
            assert!(model.pump_removals().is_empty());
            assert!(!model.has_pending_removals());
            assert_eq!(model.revision(), revision);
            assert_eq!(model.removal_checks, checks);
            assert_eq!(model.node(ROOT).sizes, sizes);
        }
    }

    #[test]
    fn completed_pristine_scan_with_many_aliases_has_no_removal_work() {
        const GROUPS: NodeId = 4096;
        const ALIASES: NodeId = 4;
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: (1..=GROUPS * ALIASES)
                .map(|id| linked(id, "link", 1 + (id - 1) / ALIASES * ALIASES))
                .collect(),
        });
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);

        assert!(model.finished && model.node(ROOT).complete);
        assert!(!model.has_partial_totals(ROOT));
        assert_eq!(model.node(ROOT).items, u64::from(GROUPS * ALIASES));
        assert_eq!(model.node(ROOT).sizes.apparent, 2 + u64::from(GROUPS) * 100);
        assert_eq!(model.hardlink_owners.len(), GROUPS as usize);
        assert_eq!(model.removal_checks, 0);
        assert_removal_work_stays_settled(&mut model);
    }

    #[test]
    fn interleaved_refresh_cleanup_drains_once_and_stays_settled() {
        const GROUPS: NodeId = 4096;
        const CHUNK: NodeId = 256;
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "refresh", NodeKind::Directory, 0, 0),
                entry(2, "untouched", NodeKind::Directory, 0, 0),
            ],
        });
        let mut old_ids = Vec::new();
        let mut aliases = Vec::new();
        let mut baseline_next = 3;
        for _ in 0..GROUPS / CHUNK {
            let start = baseline_next;
            old_ids.extend(start..start + CHUNK);
            model.apply(ScanEvent::Entries {
                parent: 1,
                entries: (start..start + CHUNK)
                    .map(|id| linked(id, "old-owner", id))
                    .collect(),
            });
            aliases.extend(start + CHUNK..start + CHUNK * 2);
            model.apply(ScanEvent::Entries {
                parent: 2,
                entries: (0..CHUNK)
                    .map(|offset| linked(start + CHUNK + offset, "survivor", start + offset))
                    .collect(),
            });
            baseline_next += CHUNK * 2;
        }
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        assert!(!model.has_pending_removals());
        model.begin_rescan(1, 1);
        scoped_root(&mut model, 1, 1, 7);
        assert!(model.has_pending_removals());

        let mut fresh_ids = Vec::new();
        for wave in 0..8 {
            let checks = model.removal_checks;
            model.pump_removals();
            assert!(model.removal_checks - checks <= REMOVAL_CHECKS);
            fresh_ids.push(model.len() as NodeId);
            model.apply_scoped(
                1,
                1,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: vec![entry(wave + 1, "fresh", NodeKind::File, 3, 4)],
                },
            );
            fresh_ids.push(model.len() as NodeId);
            model.apply(ScanEvent::Entries {
                parent: 2,
                entries: vec![entry(
                    baseline_next + wave,
                    "late-baseline",
                    NodeKind::File,
                    5,
                    8,
                )],
            });
        }
        done(&mut model, 1, 1, ROOT);
        finish(&mut model, 1, 1);
        done(&mut model, ROOT, 0, 2);
        finish(&mut model, ROOT, 0);
        assert!(model.finished && model.node(ROOT).complete);
        assert!(model.has_pending_removals());

        // Count detached nodes and ownership repairs, independently of timing.
        let expected_checks = GROUPS as usize * 2;
        for _ in 0..=expected_checks {
            if !model.has_pending_removals() {
                break;
            }
            let checks = model.removal_checks;
            model.pump_removals();
            assert!(model.removal_checks - checks <= REMOVAL_CHECKS);
            assert!(model.removal_checks <= expected_checks);
        }
        assert_eq!(model.removal_checks, expected_checks);
        assert!(!model.has_pending_removals());
        assert!(!model.has_partial_totals(ROOT));
        assert_eq!(model.node(ROOT).items, u64::from(GROUPS) + 18);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 2 + 7 + u64::from(GROUPS) * 100 + 8 * (3 + 5),
                allocated: 4 + 7 + u64::from(GROUPS) * 512 + 8 * (4 + 8),
            }
        );
        for id in old_ids {
            assert!(model.node(id).name.is_empty());
        }
        for id in aliases {
            assert_eq!(model.node(id).name.as_ref(), OsStr::new("survivor"));
            assert_eq!(model.node(id).sizes.apparent, 100);
        }
        for id in fresh_ids {
            assert!(model.is_live(id));
            assert!(!model.node(id).name.is_empty());
        }
        assert_removal_work_stays_settled(&mut model);
    }

    #[test]
    fn independent_leaf_removals_never_sweep_unrelated_nodes_or_groups() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: (1..=4096).map(|id| linked(id, "ordinary", id)).collect(),
        });
        assert!(model.hardlink_owners.is_empty());
        for id in 1..=512 {
            let checks = model.removal_checks;
            model.remove(id);
            finish_removals(&mut model);
            assert_eq!(model.removal_checks - checks, 1);
            assert!(model.node(id).name.is_empty());
        }
        assert_eq!(model.node(ROOT).items, 4096 - 512);
        assert_eq!(model.node(513).name.as_ref(), OsStr::new("ordinary"));
        assert_eq!(model.node(4096).name.as_ref(), OsStr::new("ordinary"));
        assert!(model.hardlink_owners.is_empty());
    }

    #[test]
    fn detached_subtree_cleanup_skips_interleaved_live_arena_entries() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "gone", NodeKind::Directory, 0, 0),
                entry(2, "live", NodeKind::Directory, 0, 0),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: (3..=10).map(|id| linked(id, "gone-child", id)).collect(),
        });
        model.apply(ScanEvent::Entries {
            parent: 2,
            entries: (11..=4096).map(|id| linked(id, "live-child", id)).collect(),
        });
        model.remove(1);
        finish_removals(&mut model);
        assert_eq!(model.removal_checks, 9);
        for id in 3..=10 {
            assert!(model.node(id).name.is_empty());
        }
        assert_eq!(model.node(11).name.as_ref(), OsStr::new("live-child"));
        assert_eq!(model.node(4096).name.as_ref(), OsStr::new("live-child"));
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn successive_hardlink_owners_never_revisit_dead_alias_prefixes() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: (1..=1024).map(|id| linked(id, "link", 1)).collect(),
        });
        for id in 1..=1024 {
            assert_eq!(model.node(id).sizes.apparent, 100);
            model.remove(id);
            finish_removals(&mut model);
        }
        assert_eq!(model.removal_checks, 2048);
        assert!(model.hardlink_owners.is_empty());
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 2,
                allocated: 4
            }
        );
    }

    #[test]
    fn alias_arrival_during_pending_repair_keeps_one_charge_and_owner_index() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(1, "old", 1), linked(2, "survivor", 1)],
        });
        model.remove(1);
        model.pump_cleanup();
        assert_eq!(model.removals.len(), 1);
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(3, "new-owner", 1)],
        });
        assert_eq!(model.node(3).sizes.apparent, 100);
        model.remove(3);
        finish_removals(&mut model);
        assert_eq!(model.node(2).sizes.apparent, 100);
        assert_eq!(model.hardlink_owners.len(), 1);
        assert!(model.hardlink_owners.contains_key(&2));
        assert_eq!(model.node(ROOT).sizes.apparent, 102);
        assert_eq!(model.node(ROOT).items, 1);
    }

    #[test]
    fn removing_a_charged_link_promotes_a_survivor_but_zero_alias_removal_does_not() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                linked(1, "owner", 1),
                linked(2, "alias", 1),
                linked(3, "alias2", 1),
            ],
        });
        model.remove(2);
        finish_removals(&mut model);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 102,
                allocated: 516
            }
        );
        assert_eq!(model.node(ROOT).items, 2);
        model.remove(1);
        finish_removals(&mut model);
        assert_eq!(
            model.node(3).sizes,
            Sizes {
                apparent: 100,
                allocated: 512
            }
        );
        assert!(!model.node(3).hardlink_duplicate);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 102,
                allocated: 516
            }
        );
        assert_eq!(model.node(ROOT).items, 1);
        model.remove(3);
        finish_removals(&mut model);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 2,
                allocated: 4
            }
        );
    }

    #[test]
    fn queued_removed_link_metadata_can_charge_a_later_live_alias() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "gone", NodeKind::Directory, 3, 4)],
        });
        model.remove(1);
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![linked(2, "owner", 2)],
        });
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(3, "survivor", 2)],
        });
        assert!(model.is_removed(2));
        assert!(model.is_live(3));
        assert_eq!(
            model.node(3).sizes,
            Sizes {
                apparent: 100,
                allocated: 512
            }
        );
        assert!(!model.node(3).hardlink_duplicate);
        assert_eq!(model.node(ROOT).items, 1);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 102,
                allocated: 516
            }
        );
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn hardlink_repair_is_bounded_and_does_not_promote_inside_the_removed_subtree() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "gone", NodeKind::Directory, 3, 4)],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: (2..603)
                .map(|id| linked(id, &format!("link-{id}"), 2))
                .collect(),
        });
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(603, "survivor", 2)],
        });
        model.remove(1);
        model.pump_removals();
        assert!(model.has_pending_removals());
        assert_eq!(model.node(603).sizes, Sizes::default());
        finish_removals(&mut model);
        assert_eq!(
            model.node(603).sizes,
            Sizes {
                apparent: 100,
                allocated: 512
            }
        );
        assert_eq!(model.node(ROOT).items, 1);
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 102,
                allocated: 516
            }
        );
    }

    #[test]
    fn parent_inode_refresh_changes_only_own_bytes_and_failed_delete_marks_partial() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "dir", NodeKind::Directory, 20, 24),
                entry(2, "sibling", NodeKind::File, 100, 512),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(3, "leaf", NodeKind::File, 10, 12)],
        });
        model.remove(3);
        assert_eq!(
            model.update_directory_sizes(
                1,
                Sizes {
                    apparent: 15,
                    allocated: 16
                }
            ),
            vec![1, ROOT]
        );
        assert!(!model.has_children(1, false, true));
        assert!(model.has_children(ROOT, true, false));
        assert!(model.has_children(ROOT, false, false));
        assert_eq!(
            model.node(1).sizes,
            Sizes {
                apparent: 15,
                allocated: 16
            }
        );
        assert_eq!(
            model.node(ROOT).sizes,
            Sizes {
                apparent: 117,
                allocated: 532
            }
        );
        assert_eq!(
            model.node(2).sizes,
            Sizes {
                apparent: 100,
                allocated: 512
            }
        );
        assert_eq!(
            model.note_error(1, "partial delete failure".into()),
            vec![1, ROOT]
        );
        assert_eq!(model.node(ROOT).error_count, 1);
        assert_eq!(model.node(1).error_count, 1);
        model.remove(1);
        assert!(model.update_directory_sizes(1, Sizes::default()).is_empty());
        assert!(model.note_error(1, "late error".into()).is_empty());
        assert_eq!(model.node(ROOT).error_count, 0);
        assert!(model.last_error.is_none());
    }

    fn done(model: &mut Model, scope: NodeId, generation: u64, local: NodeId) {
        model.apply_scoped(
            scope,
            generation,
            ScanEvent::DirectoryDone {
                id: local,
                error: None,
            },
        );
    }

    fn finish(model: &mut Model, scope: NodeId, generation: u64) {
        model.apply_scoped(scope, generation, ScanEvent::Finished { cancelled: false });
    }

    fn scoped_root(model: &mut Model, scope: NodeId, generation: u64, apparent: u64) {
        model.apply_scoped(
            scope,
            generation,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Directory,
                sizes: Sizes {
                    apparent,
                    allocated: apparent,
                },
                error: None,
            },
        );
    }

    #[test]
    fn scoped_refresh_retains_directory_and_siblings_and_reopens_complete_ancestors() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "refresh", NodeKind::Directory, 3, 4),
                entry(2, "untouched", NodeKind::File, 40, 40),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(3, "old", NodeKind::File, 10, 10)],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        let root_address = model.node(ROOT) as *const Node;
        let branch_address = model.node(1) as *const Node;
        assert_eq!(model.begin_rescan(1, 1), vec![1, ROOT]);
        assert_eq!(model.node(ROOT) as *const Node, root_address);
        assert_eq!(model.node(1) as *const Node, branch_address);
        assert_eq!(model.path(1), PathBuf::from("fixture/refresh"));
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![1, 2]);
        assert!(model.is_removed(3));
        assert!(!model.has_children(1, false, true));
        assert!(!model.listing_complete(1));
        assert_eq!(model.node(ROOT).items, 2);
        assert_eq!(model.node(ROOT).sizes.apparent, 42);
        assert!(!model.node(ROOT).complete);
        assert!(!model.finished);
        scoped_root(&mut model, 1, 1, 5);
        let changed = model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, "new", NodeKind::File, 20, 20)],
            },
        );
        assert_eq!(changed, vec![4, 1, ROOT]);
        assert_eq!(model.children(1).collect::<Vec<_>>(), vec![4]);
        done(&mut model, 1, 1, ROOT);
        finish(&mut model, 1, 1);
        assert!(model.node(ROOT).complete && model.finished);
        assert!(!model.cancelled);
        assert_eq!(model.node(ROOT).sizes.apparent, 67);
        assert_eq!(model.node(ROOT).items, 3);
        assert_eq!(model.node(2).sizes.apparent, 40);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn interleaved_scopes_remap_local_ids_and_ignore_baseline_late_branch_events() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "refresh", NodeKind::Directory, 3, 4),
                entry(2, "live", NodeKind::Directory, 7, 8),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(3, "old-nested", NodeKind::Directory, 10, 10)],
        });
        done(&mut model, ROOT, 0, ROOT);
        model.begin_rescan(1, 1);
        scoped_root(&mut model, 1, 1, 5);
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, "new", NodeKind::File, 20, 20)],
            },
        );
        let mut stale = entry(4, "stale", NodeKind::File, 900, 900);
        stale.error = Some("stale metadata denied".into());
        assert!(
            model
                .apply(ScanEvent::Entries {
                    parent: 3,
                    entries: vec![stale]
                })
                .is_empty()
        );
        model.apply(ScanEvent::DirectoryDone {
            id: 1,
            error: Some("stale enumeration error".into()),
        });
        model.apply(ScanEvent::Entries {
            parent: 2,
            entries: vec![entry(5, "sibling-data", NodeKind::File, 30, 30)],
        });
        assert!(model.is_removed(5));
        assert_eq!(model.children(2).collect::<Vec<_>>(), vec![6]);
        done(&mut model, ROOT, 0, 2);
        finish(&mut model, ROOT, 0);
        assert!(!model.finished);
        assert_eq!(model.node(ROOT).error_count, 0);
        done(&mut model, 1, 1, ROOT);
        finish(&mut model, 1, 1);
        assert!(model.finished && model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).sizes.apparent, 64);
        assert_eq!(model.node(ROOT).items, 4);
        assert!(model.last_error.is_none());
    }

    #[test]
    fn suppression_preserves_partial_totals_and_errors_until_explicit_refresh() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "partial", NodeKind::Directory, 3, 4)],
        });
        let mut known = entry(2, "known-error", NodeKind::Unknown, 10, 10);
        known.error = Some("known metadata error".into());
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![known],
        });
        done(&mut model, ROOT, 0, ROOT);
        let sizes = model.node(ROOT).sizes;
        let errors = model.node(ROOT).error_count;
        model.suppress_scan(1);
        let mut late = entry(3, "late", NodeKind::File, 1000, 1000);
        late.error = Some("late metadata error".into());
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![late],
        });
        model.apply(ScanEvent::DirectoryDone {
            id: 1,
            error: Some("late done error".into()),
        });
        finish(&mut model, ROOT, 0);
        assert_eq!(model.node(ROOT).sizes, sizes);
        assert_eq!(model.node(ROOT).error_count, errors);
        assert_eq!(model.node(1).items, 1);
        assert_eq!(model.children(1).collect::<Vec<_>>(), vec![2]);
        assert!(model.is_live(1) && model.is_live(2) && model.is_removed(3));
        assert!(!model.node(ROOT).complete && model.finished && model.cancelled);
        assert_eq!(model.last_error.as_deref(), Some("known metadata error"));
        model.begin_rescan(1, 1);
        scoped_root(&mut model, 1, 1, 6);
        done(&mut model, 1, 1, ROOT);
        finish(&mut model, 1, 1);
        assert!(model.node(ROOT).complete && !model.cancelled);
        assert_eq!(model.node(ROOT).sizes.apparent, 8);
        assert_eq!(model.node(ROOT).error_count, 0);
        assert!(model.last_error.is_none());
    }

    #[test]
    fn uncertain_parent_metadata_keeps_scanning_and_reconciles_its_own_marker_once() {
        for repair in 0..3 {
            let mut model = model();
            model.apply(ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "parent", NodeKind::Directory, 3, 4),
                    entry(2, "other", NodeKind::Directory, 5, 8),
                ],
            });
            assert_eq!(model.mark_metadata_uncertain(1), vec![1, ROOT]);
            assert!(model.mark_metadata_uncertain(1).is_empty());
            assert!(model.node(1).suppression_cutoff().is_none());
            assert!(!model.cancelled);
            model.apply(ScanEvent::Entries {
                parent: 1,
                entries: vec![entry(3, "late", NodeKind::File, 7, 8)],
            });
            assert!(model.is_live(3));
            assert_eq!(model.node(1).sizes.apparent, 10);
            done(&mut model, ROOT, 0, 1);
            done(&mut model, ROOT, 0, 2);
            done(&mut model, ROOT, 0, ROOT);
            finish(&mut model, ROOT, 0);
            assert!(model.node(ROOT).complete && model.has_partial_totals(ROOT));
            assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
            model.mark_partial(2);
            assert_eq!(model.node(ROOT).suppressed_subtrees, 2);
            match repair {
                0 => {
                    model.update_directory_sizes(
                        1,
                        Sizes {
                            apparent: 3,
                            allocated: 4,
                        },
                    );
                    assert!(
                        model
                            .update_directory_sizes(
                                1,
                                Sizes {
                                    apparent: 3,
                                    allocated: 4
                                }
                            )
                            .is_empty()
                    );
                }
                1 => {
                    model.begin_rescan(1, 1);
                    scoped_root(&mut model, 1, 1, 3);
                    done(&mut model, 1, 1, ROOT);
                    finish(&mut model, 1, 1);
                }
                _ => {
                    model.remove(1);
                }
            }
            assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
            assert!(model.has_partial_totals(ROOT) && model.has_partial_totals(2));
            assert!(!model.has_partial_totals(1));
            assert_eq!(model.node(ROOT).error_count, 0);
            assert!(model.cancelled && model.node(ROOT).complete);
            model.begin_rescan(2, 2);
            scoped_root(&mut model, 2, 2, 5);
            done(&mut model, 2, 2, ROOT);
            finish(&mut model, 2, 2);
            assert_eq!(model.node(ROOT).suppressed_subtrees, 0);
            assert!(model.node(ROOT).complete && !model.has_partial_totals(ROOT));
            assert!(!model.cancelled && model.last_error.is_none());
        }
    }

    #[test]
    fn metadata_recovery_preserves_descendant_and_same_node_suppression_markers() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "parent", NodeKind::Directory, 3, 4)],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.mark_metadata_uncertain(ROOT);
        model.mark_metadata_uncertain(1);
        model.mark_partial(1);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 3);
        let root_sizes = model.directories.get(&ROOT).unwrap().sizes;
        assert_eq!(model.update_directory_sizes(ROOT, root_sizes), vec![ROOT]);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 2);
        assert_eq!(model.node(1).suppressed_subtrees, 2);
        let revision = model.revision;
        assert_eq!(
            model.update_directory_sizes(
                1,
                Sizes {
                    apparent: 3,
                    allocated: 4
                }
            ),
            vec![1, ROOT]
        );
        assert!(model.revision != revision);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
        assert_eq!(model.node(1).suppressed_subtrees, 1);
        assert!(model.node(1).suppression_cutoff().is_some());
        assert!(model.has_partial_totals(1) && model.cancelled);
        assert!(
            model
                .update_directory_sizes(
                    1,
                    Sizes {
                        apparent: 3,
                        allocated: 4
                    }
                )
                .is_empty()
        );
        model.remove(1);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 0);
        assert!(model.node(ROOT).complete && !model.has_partial_totals(ROOT));
        assert!(!model.cancelled && model.last_error.is_none());
    }

    #[test]
    fn completed_nested_partial_branch_stays_approximate_while_reconciliation_waits() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "paused-ancestor", NodeKind::Directory, 3, 4),
                entry(2, "unrelated", NodeKind::Directory, 5, 8),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(3, "interrupted", NodeKind::Directory, 7, 8)],
        });
        model.apply(ScanEvent::Entries {
            parent: 3,
            entries: vec![entry(4, "previously-known", NodeKind::File, 100, 128)],
        });
        done(&mut model, ROOT, 0, 3);
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, 2);
        done(&mut model, ROOT, 0, ROOT);
        let known = model.node(ROOT).sizes;
        let items = model.node(ROOT).items;
        assert!(model.node(ROOT).complete);
        assert!(!model.has_partial_totals(ROOT));
        assert_eq!(model.mark_partial(3), vec![3, 1, ROOT]);
        model.mark_partial(3);
        assert_eq!(model.node(ROOT).sizes, known);
        assert_eq!(model.node(ROOT).items, items);
        for id in [ROOT, 1, 3] {
            assert!(model.node(id).complete);
            assert_eq!(model.node(id).pending_dirs, 0);
            assert_eq!(model.node(id).suppressed_subtrees, 1);
            assert!(model.has_partial_totals(id));
        }
        assert!(!model.has_partial_totals(2));
        finish(&mut model, ROOT, 0);
        assert!(model.finished && model.cancelled);
        assert_eq!(model.node(ROOT).error_count, 0);
        // An unrelated refresh must not erase the pending ancestor's accuracy marker.
        model.begin_rescan(2, 1);
        scoped_root(&mut model, 2, 1, 5);
        done(&mut model, 2, 1, ROOT);
        finish(&mut model, 2, 1);
        assert!(model.node(ROOT).complete && model.cancelled);
        assert!(model.has_partial_totals(ROOT));
        assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
        // Once the paused ancestor permits reconciliation, its refresh clears once.
        model.begin_rescan(1, 2);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 0);
        assert_eq!(model.node(ROOT).pending_dirs, 1);
        assert!(model.is_removed(3) && model.is_removed(4));
        scoped_root(&mut model, 1, 2, 3);
        done(&mut model, 1, 2, ROOT);
        finish(&mut model, 1, 2);
        assert!(model.node(ROOT).complete && !model.cancelled);
        assert!(!model.has_partial_totals(ROOT));
        assert!(!model.has_partial_totals(3));
        assert_eq!(model.node(ROOT).pending_dirs, 0);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn removing_completed_partial_branches_clears_only_their_accuracy_witnesses() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "ancestor", NodeKind::Directory, 3, 4)],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![
                entry(2, "partial-a", NodeKind::Directory, 5, 8),
                entry(3, "partial-b", NodeKind::Directory, 7, 8),
            ],
        });
        done(&mut model, ROOT, 0, 2);
        done(&mut model, ROOT, 0, 3);
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.mark_partial(2);
        model.mark_partial(3);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 2);
        model.remove(2);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 1);
        assert!(model.node(ROOT).complete && model.cancelled);
        assert!(model.has_partial_totals(ROOT));
        assert_eq!(model.node(ROOT).pending_dirs, 0);
        assert!(model.remove(2).is_empty());
        model.remove(3);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 0);
        assert!(model.node(ROOT).complete && !model.cancelled);
        assert!(!model.has_partial_totals(ROOT));
        assert_eq!(model.node(ROOT).pending_dirs, 0);
        assert_eq!(model.node(ROOT).sizes.apparent, 5);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn deleting_suppressed_partial_branch_releases_its_token_once() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "partial", NodeKind::Directory, 3, 4)],
        });
        done(&mut model, ROOT, 0, ROOT);
        model.suppress_scan(1);
        finish(&mut model, ROOT, 0);
        assert!(model.cancelled);
        model.remove(1);
        assert!(model.node(ROOT).complete && !model.cancelled);
        assert_eq!(model.node(ROOT).pending_dirs, 0);
        assert_eq!(model.node(ROOT).suppressed_subtrees, 0);
        assert!(model.remove(1).is_empty());
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn refreshed_hardlink_updates_live_owner_outside_scope_and_stale_metadata_cannot() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "refresh", NodeKind::Directory, 3, 4),
                linked(2, "outside-owner", 77),
            ],
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![linked(3, "old-alias", 77)],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.begin_rescan(1, 1);
        scoped_root(&mut model, 1, 1, 5);
        let mut alias = linked(1, "fresh-alias", 77);
        alias.sizes = Sizes {
            apparent: 60,
            allocated: 256,
        };
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![alias],
            },
        );
        assert_eq!(model.node(2).sizes.apparent, 60);
        assert_eq!(model.node(ROOT).sizes.apparent, 67);
        assert_eq!(model.node(4).sizes, Sizes::default());
        model.suppress_scan(1);
        let mut stale = linked(2, "late-alias", 77);
        stale.sizes = Sizes {
            apparent: 2000,
            allocated: 2000,
        };
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![stale],
            },
        );
        assert_eq!(model.node(2).sizes.apparent, 60);
        assert_eq!(model.node(ROOT).sizes.apparent, 67);
        model.remove(2);
        finish_removals(&mut model);
        assert_eq!(model.node(4).sizes.apparent, 60);
        assert!(!model.node(4).hardlink_duplicate);
        assert_eq!(model.node(ROOT).sizes.apparent, 67);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn delayed_live_baseline_alias_keeps_newer_refresh_metadata_and_transfer_charge() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "refresh", NodeKind::Directory, 3, 4),
                entry(2, "baseline-sibling", NodeKind::Directory, 5, 8),
            ],
        });
        model.begin_rescan(1, 1);
        scoped_root(&mut model, 1, 1, 7);
        let mut fresh = linked(1, "fresh-owner", 77);
        fresh.sizes = Sizes {
            apparent: 200,
            allocated: 1024,
        };
        model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![fresh],
            },
        );
        let before = model.node(ROOT).sizes;
        let errors = model.node(ROOT).error_count;
        model.apply(ScanEvent::Entries {
            parent: 2,
            entries: vec![linked(3, "delayed-alias", 77)],
        });
        assert!(model.is_live(4));
        assert!(model.node(4).hardlink_duplicate);
        assert_eq!(model.node(4).sizes, Sizes::default());
        assert_eq!(model.node(3).sizes.apparent, 200);
        assert_eq!(model.node(3).sizes.allocated, 1024);
        assert_eq!(model.node(ROOT).sizes, before);
        assert_eq!(model.node(ROOT).error_count, errors);
        assert_eq!(model.hardlink_groups[0].metadata_generation, 1);
        model.remove(1);
        finish_removals(&mut model);
        assert_eq!(model.node(4).sizes.apparent, 200);
        assert_eq!(model.node(4).sizes.allocated, 1024);
        assert!(!model.node(4).hardlink_duplicate);
        assert_eq!(model.node(ROOT).sizes.apparent, 207);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn changing_duplicate_metadata_in_one_batch_corrects_pending_parent_sum() {
        let mut model = model();
        let mut owner = linked(1, "owner", 77);
        owner.sizes = Sizes {
            apparent: 200,
            allocated: 512,
        };
        let mut alias = linked(2, "alias", 77);
        alias.sizes = Sizes {
            apparent: 100,
            allocated: 256,
        };
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![owner, alias],
        });
        assert_eq!(model.node(1).sizes.apparent, 100);
        assert_eq!(model.node(ROOT).sizes.apparent, 102);
        assert_eq!(model.node(ROOT).sizes.allocated, 260);
    }

    #[test]
    fn root_refresh_hides_old_tree_and_bounds_cleanup_without_replacing_arena() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: (1..700)
                .map(|id| entry(id, &format!("old-{id}"), NodeKind::File, 1, 4))
                .collect(),
        });
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        let address = model.node(ROOT) as *const Node;
        model.begin_rescan(ROOT, 1);
        assert_eq!(model.node(ROOT) as *const Node, address);
        assert!(model.is_removed(1) && model.is_removed(699));
        assert_eq!(model.children(ROOT).count(), 0);
        assert_eq!(model.node(ROOT).sizes, Sizes::default());
        model.pump_removals();
        assert!(model.has_pending_removals());
        assert!(model.node(1).name.is_empty());
        assert!(!model.node(699).name.is_empty());
        model.apply(ScanEvent::RootInfo {
            file_identity: None,
            kind: NodeKind::Unknown,
            sizes: Sizes::default(),
            error: Some("late root error".into()),
        });
        assert_eq!(model.node(ROOT).error_count, 0);
        scoped_root(&mut model, ROOT, 1, 7);
        model.apply_scoped(
            ROOT,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, "fresh", NodeKind::File, 9, 9)],
            },
        );
        assert_eq!(model.children(ROOT).collect::<Vec<_>>(), vec![700]);
        assert_eq!(model.node(ROOT).sizes.apparent, 16);
        done(&mut model, ROOT, 1, ROOT);
        finish(&mut model, ROOT, 1);
        finish_removals(&mut model);
        assert!(model.finished && model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn replacement_symlink_finishes_retained_target_and_updates_directory_witness() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "replaced", NodeKind::Directory, 3, 4)],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.begin_rescan(1, 1);
        model.apply_scoped(
            1,
            1,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Symlink,
                sizes: Sizes {
                    apparent: 5,
                    allocated: 5,
                },
                error: None,
            },
        );
        finish(&mut model, 1, 1);
        assert_eq!(model.node(1).kind, NodeKind::Symlink);
        assert!(model.node(ROOT).complete);
        assert!(model.has_children(ROOT, false, false));
        assert!(!model.has_children(ROOT, true, true));
        assert_eq!(model.node(ROOT).sizes.apparent, 7);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn refreshed_directory_replaced_by_hardlinked_file_retains_id_and_deduplicates() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![
                entry(1, "replacement", NodeKind::Directory, 3, 4),
                linked(2, "outside-owner", 77),
            ],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.begin_rescan(1, 1);
        model.apply_scoped(
            1,
            1,
            ScanEvent::RootInfo {
                file_identity: Some(77),
                kind: NodeKind::File,
                sizes: Sizes {
                    apparent: 200,
                    allocated: 1024,
                },
                error: None,
            },
        );
        finish(&mut model, 1, 1);
        assert!(model.is_live(1));
        assert_eq!(model.node(1).parent, Some(ROOT));
        assert_eq!(model.node(1).kind, NodeKind::File);
        assert!(model.node(1).hardlink_duplicate);
        assert_eq!(model.node(1).sizes, Sizes::default());
        assert_eq!(model.node(2).sizes.apparent, 200);
        assert_eq!(model.node(ROOT).sizes.apparent, 202);
        assert_eq!(model.node(ROOT).items, 2);
        assert!(!model.has_children(ROOT, true, true));
        assert!(model.node(ROOT).complete);
        assert_eq!(model.node(ROOT).error_count, 0);
        model.remove(2);
        finish_removals(&mut model);
        assert_eq!(model.node(1).sizes.apparent, 200);
        assert!(!model.node(1).hardlink_duplicate);
        assert_eq!(model.node(ROOT).sizes.apparent, 202);
    }

    #[test]
    fn zero_charge_replacement_metadata_notifies_retained_row_and_directory_witnesses() {
        for (file_identity, sizes) in [
            (None, Sizes::default()),
            (
                Some(77),
                Sizes {
                    apparent: 100,
                    allocated: 512,
                },
            ),
        ] {
            let mut model = model();
            model.apply(ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, ".replacement", NodeKind::Directory, 3, 4),
                    linked(2, "outside-owner", 77),
                ],
            });
            done(&mut model, ROOT, 0, 1);
            done(&mut model, ROOT, 0, ROOT);
            finish(&mut model, ROOT, 0);
            model.begin_rescan(1, 1);
            let before = model.node(ROOT).sizes;
            let changed = model.apply_scoped(
                1,
                1,
                ScanEvent::RootInfo {
                    file_identity,
                    kind: NodeKind::File,
                    sizes,
                    error: None,
                },
            );
            assert_eq!(changed, vec![1]);
            assert_eq!(model.node(1).kind, NodeKind::File);
            assert_eq!(model.node(1).sizes, Sizes::default());
            assert_eq!(model.node(1).hardlink_duplicate, file_identity.is_some());
            assert_eq!(model.node(ROOT).sizes, before);
            assert_eq!(model.node(2).sizes.apparent, 100);
            assert_eq!(model.node(2).sizes.allocated, 512);
            assert_eq!(model.node(ROOT).items, 2);
            assert!(model.has_children(ROOT, false, true));
            assert!(!model.has_children(ROOT, true, true));
            let counts = &model.directories[&ROOT].children;
            assert_eq!(counts.all, 2);
            assert_eq!(counts.hidden, 1);
            assert_eq!(counts.directories, 0);
            assert_eq!(counts.hidden_directories, 0);
            assert!(model.node(ROOT).complete);
            assert_eq!(model.node(ROOT).pending_dirs, 0);
            assert_eq!(model.node(ROOT).error_count, 0);
        }
    }

    #[test]
    fn initial_regular_file_root_with_canonical_identity_keeps_its_full_charge() {
        let mut model = Model::new(PathBuf::from("fixture-file"));
        model.apply(ScanEvent::RootInfo {
            file_identity: Some(77),
            kind: NodeKind::File,
            sizes: Sizes {
                apparent: 200,
                allocated: 1024,
            },
            error: None,
        });
        finish(&mut model, ROOT, 0);
        assert_eq!(model.node(ROOT).kind, NodeKind::File);
        assert_eq!(model.node(ROOT).sizes.apparent, 200);
        assert_eq!(model.node(ROOT).sizes.allocated, 1024);
        assert!(!model.node(ROOT).hardlink_duplicate);
        assert!(model.node(ROOT).complete && model.finished);
        assert_eq!(model.node(ROOT).error_count, 0);
    }

    #[test]
    fn late_removed_branch_events_after_scan_completion_leave_known_errors_unchanged() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(1, "removed", NodeKind::Directory, 3, 4)],
        });
        done(&mut model, ROOT, 0, 1);
        done(&mut model, ROOT, 0, ROOT);
        finish(&mut model, ROOT, 0);
        model.remove(1);
        let sizes = model.node(ROOT).sizes;
        model.apply(ScanEvent::DirectoryDone {
            id: 1,
            error: Some("late scan error".into()),
        });
        model.apply(ScanEvent::Entries {
            parent: 1,
            entries: vec![entry(2, "late", NodeKind::File, 999, 999)],
        });
        assert_eq!(model.node(ROOT).sizes, sizes);
        assert_eq!(model.node(ROOT).error_count, 0);
        assert!(model.last_error.is_none());
    }

    #[test]
    fn singleton_hardlink_groups_allocate_members_only_for_later_aliases() {
        let mut model = model();
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(1, "single", 77)],
        });
        let index = model.hardlink_indices[&77];
        assert!(model.hardlink_groups[index].members.is_none());
        model.apply(ScanEvent::Entries {
            parent: ROOT,
            entries: vec![linked(2, "alias", 77)],
        });
        assert_eq!(
            model.hardlink_groups[index].members.as_ref().unwrap().ids,
            vec![2]
        );
        model.remove(1);
        finish_removals(&mut model);
        assert_eq!(model.node(2).sizes.apparent, 100);
        assert!(!model.node(2).hardlink_duplicate);
        assert_eq!(model.node(ROOT).sizes.apparent, 102);
    }

    #[test]
    fn transport_failures_respect_current_generation_and_scan_suppression() {
        let mut model = model();
        model.begin_rescan(ROOT, 1);
        let revision = model.revision();
        assert!(
            model
                .note_scan_error(ROOT, 0, "stale baseline transport failure".into())
                .is_empty()
        );
        assert_eq!(model.revision(), revision);
        assert_eq!(model.node(ROOT).error_count, 0);
        assert_eq!(
            model.note_scan_error(ROOT, 1, "current scanner stopped".into()),
            vec![ROOT]
        );
        assert_eq!(model.node(ROOT).error_count, 1);
        assert_eq!(model.last_error.as_deref(), Some("current scanner stopped"));
        model.suppress_scan(ROOT);
        let revision = model.revision();
        assert!(
            model
                .note_scan_error(ROOT, 1, "suppressed transport failure".into())
                .is_empty()
        );
        assert_eq!(model.revision(), revision);
        assert_eq!(model.node(ROOT).error_count, 1);
        assert_eq!(model.last_error.as_deref(), Some("current scanner stopped"));
        finish(&mut model, ROOT, 1);
        assert!(
            model
                .note_scan_error(ROOT, 1, "completed transport failure".into())
                .is_empty()
        );
        assert_eq!(model.node(ROOT).error_count, 1);
    }
}
