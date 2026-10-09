use std::{
    collections::HashMap,
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
    owner: Option<NodeId>,
    members: Vec<NodeId>,
}

#[derive(Debug, Default)]
struct RemovalCursor {
    group: usize,
    member: usize,
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
    removed: bool,
    read_done: bool,
    overflow_reported: bool,
    pending_dirs: u32,
}

impl Node {
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
            removed: false,
            read_done: complete,
            overflow_reported: false,
            pending_dirs: 0,
        }
    }

    /// Keep this node's warning after inherited subtree errors are removed.
    fn mark_overflow(&mut self, overflow: bool, inherited: &mut u64) -> bool {
        if !overflow {
            return false;
        }
        let newly_reported = !self.overflow_reported;
        if newly_reported {
            self.overflow_reported = true;
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
        if self.len % NODE_CHUNK == 0 {
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
    removal_cursor: Option<RemovalCursor>,
    last_error_id: Option<NodeId>,
    pub revision: u64,
    pub finished: bool,
    pub cancelled: bool,
    pub last_error: Option<String>,
    root_info_received: bool,
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
            removal_cursor: None,
            last_error_id: None,
            revision: 0,
            finished: false,
            cancelled: false,
            last_error: None,
            root_info_received: false,
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
            if node.removed {
                return true;
            }
            current = node.parent;
        }
        false
    }

    pub fn is_live(&self, id: NodeId) -> bool {
        !self.is_removed(id)
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
        let counts = &mut self.directories.get_mut(&parent).unwrap().children;
        counts.all -= 1;
        counts.hidden -= u32::from(hidden);
        counts.directories -= u32::from(directory);
        counts.hidden_directories -= u32::from(directory && hidden);
        self.has_removals = true;
        self.nodes[id as usize].removed = true;
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
            node.error_count = node
                .error_count
                .saturating_sub(errors)
                .max(u64::from(node.overflow_reported));
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
        if !self.hardlink_groups.is_empty() {
            self.removal_cursor = Some(RemovalCursor::default());
        }
        if self.last_error_id.is_some_and(|id| self.is_removed(id))
            || self.nodes[ROOT as usize].error_count == 0
        {
            self.last_error = None;
            self.last_error_id = None;
        }
        self.revision = self.revision.wrapping_add(1);
        changed
    }

    /// Repair hardlink ownership in small batches after subtree tombstoning.
    /// Only aliases with live ancestors may receive the group's byte charge.
    pub fn pump_removals(&mut self) -> Vec<NodeId> {
        let Some(mut cursor) = self.removal_cursor.take() else {
            return Vec::new();
        };
        let started = Instant::now();
        let mut checks = 0;
        let mut changed = Vec::new();
        while cursor.group < self.hardlink_groups.len()
            && checks < REMOVAL_CHECKS
            && started.elapsed() < REMOVAL_BUDGET
        {
            checks += 1;
            let group = &self.hardlink_groups[cursor.group];
            if group.owner.is_some_and(|id| self.is_live(id)) {
                cursor.group += 1;
                cursor.member = 0;
                continue;
            }
            if cursor.member >= group.members.len() {
                self.hardlink_groups[cursor.group].owner = None;
                cursor.group += 1;
                cursor.member = 0;
                continue;
            }
            let id = group.members[cursor.member];
            cursor.member += 1;
            if self.is_live(id) {
                let sizes = self.hardlink_groups[cursor.group].sizes;
                self.hardlink_groups[cursor.group].owner = Some(id);
                self.nodes[id as usize].hardlink_duplicate = false;
                self.add_totals(id, sizes, 0, 0, false, &mut changed);
                cursor.group += 1;
                cursor.member = 0;
            }
        }
        if cursor.group < self.hardlink_groups.len() {
            self.removal_cursor = Some(cursor);
        }
        if !changed.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    pub fn has_pending_removals(&self) -> bool {
        self.removal_cursor.is_some()
    }

    /// Refresh one surviving directory's own inode bytes after a deletion.
    pub fn update_directory_sizes(&mut self, id: NodeId, sizes: Sizes) -> Vec<NodeId> {
        if !self.is_live(id) || self.nodes[id as usize].kind != NodeKind::Directory {
            return Vec::new();
        }
        let old = std::mem::replace(&mut self.directories.entry(id).or_default().sizes, sizes);
        if old == sizes {
            return Vec::new();
        }
        let mut changed = Vec::new();
        let mut overflow = false;
        let mut inherited_overflows = 0;
        let mut reported = None;
        let mut current = Some(id);
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
        self.revision = self.revision.wrapping_add(1);
        changed
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
        self.is_live(id) && self.node(id).read_done
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
        let mut changed = Vec::new();
        self.revision = self.revision.wrapping_add(1);
        if self.finished {
            if !matches!(event, ScanEvent::Finished { .. }) {
                self.record_error(ROOT, "received scan data after completion".into());
            }
            return changed;
        }
        match event {
            ScanEvent::RootInfo { kind, sizes, error } => {
                if self.root_info_received {
                    self.record_error(ROOT, "received root metadata twice".into());
                    return changed;
                }
                self.root_info_received = true;
                if self.nodes[ROOT as usize].first_child != NO_NODE && kind != NodeKind::Directory {
                    self.record_error(ROOT, "non-directory root already has children".into());
                } else {
                    let root = &mut self.nodes[ROOT as usize];
                    root.kind = kind;
                    root.complete = kind != NodeKind::Directory;
                    root.read_done = root.complete;
                }
                if kind == NodeKind::Directory {
                    self.directories.entry(ROOT).or_default().sizes = sizes;
                }
                let errors = u64::from(error.is_some());
                if let Some(error) = error {
                    self.last_error = Some(error);
                    self.last_error_id = Some(ROOT);
                }
                self.add_totals(ROOT, sizes, 0, errors, false, &mut changed);
            }
            ScanEvent::Entries { parent, entries } => {
                self.add_entries(parent, entries, &mut changed)
            }
            ScanEvent::DirectoryDone { id, error } => {
                if self.is_removed(id) && (id as usize) < self.nodes.len() {
                    return changed;
                }
                let Some(node) = self.nodes.get(id as usize) else {
                    self.record_error(ROOT, format!("finished unknown directory {id}"));
                    return changed;
                };
                if node.read_done {
                    return changed;
                }
                if let Some(error) = error {
                    self.record_error(id, error);
                }
                self.nodes[id as usize].read_done = true;
                self.complete_ready(id);
            }
            ScanEvent::Finished { cancelled } => {
                self.cancelled = cancelled;
                self.finished = true;
                if !cancelled && !self.nodes[ROOT as usize].complete {
                    self.record_error(ROOT, "scan ended with unfinished directories".into());
                }
            }
        }
        changed
    }

    fn add_entries(&mut self, parent: NodeId, entries: Vec<Entry>, changed: &mut Vec<NodeId>) {
        if (parent as usize) < self.nodes.len() && self.is_removed(parent) {
            // The scanner owns append-only IDs. Consume its queued IDs even though
            // the corresponding subtree has disappeared; no basename allocation,
            // sibling links, totals, metadata errors, or completion tokens are needed.
            for entry in entries {
                if entry.id as usize != self.nodes.len() || entry.id == NO_NODE {
                    continue;
                }
                if let Some(group_id) = entry.hardlink_group {
                    self.remember_hardlink(group_id, entry.id, entry.sizes, false);
                }
                let mut node = Node::new(
                    Arc::clone(&self.tombstone_name),
                    Some(parent),
                    NodeKind::Unknown,
                    Sizes::default(),
                );
                node.removed = true;
                self.nodes.push(node);
            }
            return;
        }
        let Some(parent_node) = self.nodes.get(parent as usize) else {
            self.record_error(ROOT, format!("entries have unknown parent {parent}"));
            return;
        };
        if parent_node.kind != NodeKind::Directory || parent_node.read_done {
            self.record_error(
                parent,
                "entries arrived after directory enumeration ended".into(),
            );
            return;
        }

        let mut sizes = Sizes::default();
        let mut items = 0u64;
        let mut errors = 0u64;
        let mut overflow = false;
        for entry in entries {
            if entry.id as usize != self.nodes.len() || entry.id == NO_NODE {
                self.record_error(parent, format!("out-of-order entry id {}", entry.id));
                continue;
            }
            if entry.name.is_empty()
                || Path::new(&entry.name).file_name() != Some(entry.name.as_os_str())
            {
                self.record_error(parent, "entry has an invalid basename".into());
                continue;
            }
            let charged = if let Some(group_id) = entry.hardlink_group {
                self.remember_hardlink(group_id, entry.id, entry.sizes, true)
            } else if entry.hardlink_duplicate {
                Sizes::default()
            } else {
                entry.sizes
            };
            let mut node = Node::new(
                Arc::from(entry.name.into_boxed_os_str()),
                Some(parent),
                entry.kind,
                charged,
            );
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
            let counts = &mut self.directories.entry(parent).or_default().children;
            let hidden = node.name.as_encoded_bytes().starts_with(b".");
            let directory = node.kind == NodeKind::Directory;
            counts.all += 1;
            counts.hidden += u32::from(hidden);
            counts.directories += u32::from(directory);
            counts.hidden_directories += u32::from(directory && hidden);
            changed.push(entry.id);
            self.nodes.push(node);
        }
        // A wide directory batch traverses the ancestor chain only once.
        self.add_totals(parent, sizes, items, errors, overflow, changed);
    }

    fn remember_hardlink(
        &mut self,
        group_id: NodeId,
        id: NodeId,
        sizes: Sizes,
        live: bool,
    ) -> Sizes {
        let index = if let Some(&index) = self.hardlink_indices.get(&group_id) {
            index
        } else {
            let index = self.hardlink_groups.len();
            self.hardlink_indices.insert(group_id, index);
            self.hardlink_groups.push(HardlinkGroup {
                sizes,
                owner: None,
                members: Vec::new(),
            });
            index
        };
        let owner_live = self.hardlink_groups[index]
            .owner
            .is_some_and(|owner| self.is_live(owner));
        let group = &mut self.hardlink_groups[index];
        group.members.push(id);
        if live && !owner_live {
            group.owner = Some(id);
            group.sizes
        } else {
            Sizes::default()
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
            if node.complete || !node.read_done || node.pending_dirs != 0 {
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
    use std::ffi::OsString;

    fn entry(id: NodeId, name: &str, kind: NodeKind, apparent: u64, allocated: u64) -> Entry {
        Entry {
            id,
            name: OsString::from(name),
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
}
