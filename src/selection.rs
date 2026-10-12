//! Visual selection keeps stable IDs and bounds range work on the input thread.

use crate::order::CachedSnapshot;
use crate::retire::Retirer;
use crate::types::NodeId;
use im::HashSet;
use std::io;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeMode {
    Add,
    Remove,
    Invert,
}

struct Range {
    snapshot: Arc<CachedSnapshot>,
    baseline: HashSet<NodeId>,
    excluded: HashSet<NodeId>,
    anchor: usize,
    desired: (usize, usize),
    applied: Option<(usize, usize)>,
    mode: RangeMode,
    preserve_anchor: bool,
    explicit_anchor: Option<(NodeId, bool)>,
}

struct PendingRange {
    snapshot: Arc<CachedSnapshot>,
    lead: NodeId,
    mode: RangeMode,
    boundary: usize,
}

struct Retired {
    _selected: HashSet<NodeId>,
    _range: Option<Range>,
    _order: Option<Arc<CachedSnapshot>>,
}

/// A range retains its initial ordering while selection membership uses node IDs.
/// The caller pumps bounded work each frame and validates live entries before
/// using a completed snapshot to prepare batch deletion.
pub struct VisualSelection {
    directory: NodeId,
    active: bool,
    selected: HashSet<NodeId>,
    anchor: Option<NodeId>,
    removed_anchor: Option<NodeId>,
    preserve_anchor: bool,
    range: Option<Range>,
    pending_range: Option<PendingRange>,
    retirer: Retirer<Vec<Retired>>,
    retired: Vec<Retired>,
}

impl VisualSelection {
    pub fn new(directory: NodeId) -> io::Result<Self> {
        Ok(Self {
            directory,
            active: false,
            selected: HashSet::new(),
            anchor: None,
            removed_anchor: None,
            preserve_anchor: false,
            range: None,
            pending_range: None,
            retirer: Retirer::new()?,
            retired: Vec::new(),
        })
    }

    pub fn directory(&self) -> NodeId {
        self.directory
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.active && self.selected.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn is_pending(&self) -> bool {
        self.pending_range.is_some() || self.range_pending()
    }

    fn range_pending(&self) -> bool {
        self.range
            .as_ref()
            .is_some_and(|range| range.applied != Some(range.desired))
    }

    /// Persistent cloning shares the selection tree. Wait until !is_pending()
    /// to capture the whole requested range.
    pub fn snapshot(&self) -> HashSet<NodeId> {
        self.selected.clone()
    }

    pub fn enter(&mut self, directory: NodeId, cursor: Option<NodeId>) {
        self.reset(directory);
        self.active = true;
        self.anchor = cursor;
        if let Some(id) = cursor {
            self.selected.insert(id);
        }
    }

    pub fn exit(&mut self) {
        self.reset(self.directory);
    }

    /// Directory changes discard the old gesture and selection off thread.
    pub fn reset(&mut self, directory: NodeId) {
        self.directory = directory;
        self.active = false;
        self.anchor = None;
        self.removed_anchor = None;
        self.preserve_anchor = false;
        self.clear_pending_range();
        self.retired.push(Retired {
            _selected: std::mem::take(&mut self.selected),
            _range: self.range.take(),
            _order: None,
        });
        self.flush_retired();
    }

    /// Commit the materialized selection and cancel any unfinished old range.
    /// Later pumping therefore cannot overwrite an explicit toggle.
    pub fn toggle(&mut self, id: NodeId) -> bool {
        if !self.active {
            return false;
        }
        self.reanchor(id);
        self.preserve_anchor = true;
        if self.selected.contains(&id) {
            self.selected.remove(&id);
        } else {
            self.selected.insert(id);
        }
        true
    }

    /// Explicit deselection changes one row without treating it as deleted.
    /// Preserve that choice when a later additive range starts at this anchor.
    pub fn deselect(&mut self, id: NodeId) -> bool {
        if !self.active {
            return false;
        }
        self.reanchor(id);
        self.preserve_anchor = true;
        self.selected.remove(&id);
        true
    }

    /// Restore the persistent baseline captured at mouse press before applying
    /// a drag, so its result does not depend on an earlier click update.
    pub fn restore_snapshot(&mut self, selected: HashSet<NodeId>, anchor: NodeId) {
        self.clear_pending_range();
        self.preserve_anchor = false;
        self.retired.push(Retired {
            _selected: std::mem::replace(&mut self.selected, selected),
            _range: self.range.take(),
            _order: None,
        });
        self.active = true;
        self.anchor = Some(anchor);
        self.removed_anchor = None;
        self.flush_retired();
    }

    /// Begin a new gesture without changing this row's selected membership.
    pub fn reanchor(&mut self, id: NodeId) {
        self.clear_pending_range();
        self.anchor = Some(id);
        self.removed_anchor = None;
        self.preserve_anchor = false;
        if let Some(range) = self.range.take() {
            self.retired.push(Retired {
                _selected: HashSet::new(),
                _range: Some(range),
                _order: None,
            });
            self.flush_retired();
        }
    }

    pub fn remove(&mut self, id: NodeId) {
        self.selected.remove(&id);
        if self.anchor == Some(id) {
            // A press can precede the first range; keep its deleted anchor slot
            // until a frozen snapshot transfers it into the range exclusions.
            self.removed_anchor = self.range.is_none().then_some(id);
            self.anchor = None;
            self.preserve_anchor = false;
        }
        if let Some(range) = &mut self.range {
            // The frozen slot still defines this gesture's interval. Retain it
            // until the intended range has materialized, while never restoring
            // deleted IDs through additive or inverse pumping.
            range.baseline.remove(&id);
            range.excluded.insert(id);
        }
    }

    /// Add from the anchor while committing existing marks. A deleted anchor
    /// retains its frozen slot, including when the previous gesture inverted it.
    pub fn add_range(&mut self, snapshot: Arc<CachedSnapshot>, lead: NodeId) -> bool {
        if !self.active {
            return false;
        }
        self.restart_range(lead, RangeMode::Add);
        self.begin_range(snapshot, lead, RangeMode::Add)
    }

    /// Commit current marks for a new gesture while retaining a deleted
    /// anchor's frozen interval and exclusions.
    fn restart_range(&mut self, lead: NodeId, mode: RangeMode) {
        if self.anchor.is_none() && self.range.is_some() {
            self.clear_pending_range();
            self.preserve_anchor = false;
            let range = self.range.as_mut().expect("retained range exists");
            let baseline = std::mem::replace(&mut range.baseline, self.selected.clone());
            range.mode = mode;
            range.applied = None;
            range.preserve_anchor = false;
            range.explicit_anchor = None;
            self.retire_snapshot(baseline);
        } else if self.removed_anchor.is_none() {
            self.reanchor(self.anchor.unwrap_or(lead));
        }
    }

    /// The first snapshot fixes a gesture's row order. Later resorts cannot
    /// retarget a range to unrelated IDs. Every gesture restores its original
    /// baseline when the endpoint reverses.
    pub fn begin_range(
        &mut self,
        snapshot: Arc<CachedSnapshot>,
        lead: NodeId,
        mode: RangeMode,
    ) -> bool {
        if !self.active {
            return false;
        }
        // Removing an anchor excludes its ID but retains the frozen range slot.
        if self.range.as_ref().is_some_and(|range| range.mode != mode) {
            self.restart_range(lead, mode);
        }
        if let Some(range) = &self.range {
            if !range.snapshot.positions.contains_key(&lead) {
                if !snapshot.positions.contains_key(&lead) {
                    return false;
                }
                let boundary = if range.desired.0 == range.anchor {
                    range.desired.1
                } else {
                    range.desired.0
                };
                self.clear_pending_range();
                self.pending_range = Some(PendingRange {
                    snapshot,
                    lead,
                    mode,
                    boundary,
                });
                return true;
            }
        }
        self.clear_pending_range();
        if self.range.is_none() {
            if !snapshot.positions.contains_key(&lead) {
                return false;
            }
            let removed_anchor = self.removed_anchor.take();
            let anchor_id = self
                .anchor
                .or(removed_anchor)
                .filter(|id| snapshot.positions.contains_key(id))
                .unwrap_or(lead);
            let anchor = snapshot
                .position(anchor_id)
                .expect("anchor is in the snapshot");
            let preserve_anchor = self.preserve_anchor && self.anchor == Some(anchor_id);
            self.anchor = (removed_anchor != Some(anchor_id)).then_some(anchor_id);
            self.range = Some(Range {
                snapshot,
                baseline: self.selected.clone(),
                excluded: removed_anchor.into_iter().collect(),
                anchor,
                desired: (anchor, anchor),
                applied: None,
                mode,
                preserve_anchor,
                explicit_anchor: preserve_anchor
                    .then(|| (anchor_id, self.selected.contains(&anchor_id))),
            });
        }
        let range = self.range.as_mut().expect("range was initialized");
        let Some(lead) = range.snapshot.position(lead) else {
            return false;
        };
        range.desired = (range.anchor.min(lead), range.anchor.max(lead));
        true
    }

    pub fn pump(&mut self, limit: usize) -> bool {
        self.pump_filtered(limit, |_| true)
    }

    pub fn pump_filtered(
        &mut self,
        limit: usize,
        mut eligible: impl FnMut(NodeId) -> bool,
    ) -> bool {
        self.flush_retired();
        let mut progressed = false;
        for _ in 0..limit {
            if !self.range_pending() {
                if let Some(next) = &mut self.pending_range {
                    // A deleted endpoint may leave a long missing span. Find a
                    // surviving shared boundary one slot per budget step before
                    // starting the next segment, preserving its existing mark.
                    let range = self.range.as_ref().expect("pending segment has a range");
                    let id = range.snapshot.ids[next.boundary];
                    let boundary = if !range.excluded.contains(&id)
                        && next.snapshot.positions.contains_key(&id)
                    {
                        Some(Some(id))
                    } else if next.boundary == range.anchor {
                        Some(None)
                    } else {
                        next.boundary = if next.boundary > range.anchor {
                            next.boundary - 1
                        } else {
                            next.boundary + 1
                        };
                        None
                    };
                    progressed = true;
                    if let Some(boundary) = boundary {
                        let excluded = range.excluded.clone();
                        let explicit_anchor = range.explicit_anchor;
                        let next = self.pending_range.take().expect("pending segment exists");
                        self.reanchor(boundary.unwrap_or(next.lead));
                        self.preserve_anchor = boundary.is_some();
                        self.begin_range(next.snapshot, next.lead, next.mode);
                        let range = self.range.as_mut().expect("next segment exists");
                        range.excluded = excluded;
                        // Only a user toggle owns this override. The new shared
                        // boundary is protected separately by preserve_anchor.
                        range.explicit_anchor = explicit_anchor;
                    }
                    continue;
                }
            }
            let Some(range) = &mut self.range else {
                break;
            };
            let (index, entering) = match range.applied {
                None => {
                    range.applied = Some((range.anchor, range.anchor));
                    (range.anchor, true)
                }
                Some((low, high)) if low < range.desired.0 => {
                    range.applied = Some((low + 1, high));
                    (low, false)
                }
                Some((low, high)) if high > range.desired.1 => {
                    range.applied = Some((low, high - 1));
                    (high, false)
                }
                Some((low, high)) if low > range.desired.0 => {
                    range.applied = Some((low - 1, high));
                    (low - 1, true)
                }
                Some((low, high)) if high < range.desired.1 => {
                    range.applied = Some((low, high + 1));
                    (high + 1, true)
                }
                Some(_) => break,
            };
            progressed = true;
            let id = range.snapshot.ids[index];
            let baseline = range.baseline.contains(&id);
            let selected = if entering && !(index == range.anchor && range.preserve_anchor) {
                match range.mode {
                    RangeMode::Add => true,
                    RangeMode::Remove => false,
                    RangeMode::Invert => !baseline,
                }
            } else {
                baseline
            };
            let selected = range
                .explicit_anchor
                .filter(|&(anchor, _)| anchor == id)
                .map_or(selected, |(_, selected)| selected);
            if selected && !range.excluded.contains(&id) && eligible(id) {
                self.selected.insert(id);
            } else {
                self.selected.remove(&id);
            }
        }
        progressed
    }

    /// Batch conversion can transfer its persistent remaining-ID snapshot here
    /// when cancelled, without destroying a large tree on the keyboard path.
    pub fn retire_snapshot(&mut self, selected: HashSet<NodeId>) {
        self.retired.push(Retired {
            _selected: selected,
            _range: None,
            _order: None,
        });
        self.flush_retired();
    }

    /// Retain a cancelled batch's ordering until its selection can be destroyed
    /// with it on the existing cleanup worker.
    pub fn retire_snapshot_and_order(
        &mut self,
        selected: HashSet<NodeId>,
        order: Arc<CachedSnapshot>,
    ) {
        self.retired.push(Retired {
            _selected: selected,
            _range: None,
            _order: Some(order),
        });
        self.flush_retired();
    }

    fn clear_pending_range(&mut self) {
        if let Some(pending) = self.pending_range.take() {
            self.retire_snapshot_and_order(HashSet::new(), pending.snapshot);
        }
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

impl Drop for VisualSelection {
    fn drop(&mut self) {
        self.clear_pending_range();
        self.retired.push(Retired {
            _selected: std::mem::take(&mut self.selected),
            _range: self.range.take(),
            _order: None,
        });
        self.flush_retired();
        if !self.retired.is_empty() {
            let retired = std::mem::take(&mut self.retired);
            // Cleanup may already be destroying an earlier large selection.
            // Ownership is moved to a detached thread rather than joined here.
            let _ = std::thread::Builder::new()
                .name("ldu-selection-retire".into())
                .spawn(move || drop(retired));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn view(ids: &[NodeId]) -> Arc<CachedSnapshot> {
        Arc::new(CachedSnapshot {
            generation: 1,
            child_epoch: 0,
            ids: ids.to_vec(),
            positions: ids
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, u32::try_from(i).unwrap()))
                .collect::<StdHashMap<_, _>>(),
        })
    }

    fn finish(selection: &mut VisualSelection) {
        while selection.is_pending() {
            selection.pump(2);
        }
    }

    #[test]
    fn additive_reversal_restores_noncontiguous_baseline() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.toggle(5);
        selection.reanchor(2);
        let view = view(&[1, 2, 3, 4, 5]);
        selection.begin_range(Arc::clone(&view), 4, RangeMode::Add);
        finish(&mut selection);
        assert_eq!(selection.len(), 5);
        selection.begin_range(view, 2, RangeMode::Add);
        finish(&mut selection);
        assert!(selection.contains(1));
        assert!(selection.contains(2));
        assert!(selection.contains(5));
        assert!(!selection.contains(3));
        assert!(!selection.contains(4));
    }

    #[test]
    fn inverse_drag_and_reversal_restore_each_baseline_membership() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.toggle(3);
        selection.reanchor(1);
        let view = view(&[1, 2, 3, 4]);
        selection.begin_range(Arc::clone(&view), 4, RangeMode::Invert);
        finish(&mut selection);
        assert!(!selection.contains(1));
        assert!(selection.contains(2));
        assert!(!selection.contains(3));
        assert!(selection.contains(4));
        selection.begin_range(view, 2, RangeMode::Invert);
        finish(&mut selection);
        assert!(!selection.contains(1));
        assert!(selection.contains(2));
        assert!(selection.contains(3));
        assert!(!selection.contains(4));
    }

    #[test]
    fn subtractive_range_is_bounded_and_restores_its_frozen_sparse_baseline() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, None);
        let ids: Vec<_> = (1..=700).collect();
        for id in ids.iter().copied().filter(|id| id % 2 == 1) {
            selection.toggle(id);
        }
        selection.reanchor(3);
        let original = view(&ids);
        selection.begin_range(Arc::clone(&original), 697, RangeMode::Remove);
        selection.pump(256);
        assert!(selection.is_pending());
        assert!(!selection.contains(3));
        assert!(selection.contains(259));
        finish(&mut selection);
        assert_eq!(selection.snapshot(), [1u32, 699].into_iter().collect());

        // A resort cannot change the subtraction interval, and contraction
        // restores only the rows that were marked before this gesture.
        let reversed: Vec<_> = ids.iter().copied().rev().collect();
        selection.begin_range(view(&reversed), 5, RangeMode::Remove);
        finish(&mut selection);
        for id in &ids {
            assert_eq!(
                selection.contains(*id),
                id % 2 == 1 && !(3..=5).contains(id)
            );
        }
        selection.remove(697);
        selection.begin_range(Arc::clone(&original), 697, RangeMode::Remove);
        finish(&mut selection);
        selection.begin_range(original, 5, RangeMode::Remove);
        finish(&mut selection);
        assert!(!selection.contains(697));
        assert!(selection.contains(695) && selection.contains(699));
        assert!(!selection.contains(696) && !selection.contains(698));
    }

    #[test]
    fn subtractive_segments_preserve_boundaries_and_explicit_deselection() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        for id in [3, 4, 5] {
            selection.toggle(id);
        }
        selection.reanchor(1);
        selection.begin_range(view(&[1, 2, 3]), 3, RangeMode::Remove);
        finish(&mut selection);
        let extended = view(&[1, 2, 3, 4, 5]);
        selection.begin_range(Arc::clone(&extended), 5, RangeMode::Remove);
        finish(&mut selection);
        assert!(selection.is_empty());
        selection.begin_range(Arc::clone(&extended), 4, RangeMode::Remove);
        finish(&mut selection);
        assert_eq!(selection.snapshot(), [5u32].into_iter().collect());

        selection.deselect(5);
        selection.deselect(5);
        assert!(
            selection.is_empty(),
            "deselecting an unmarked row must not add it"
        );
        selection.begin_range(extended, 2, RangeMode::Add);
        finish(&mut selection);
        assert_eq!(selection.snapshot(), [2u32, 3, 4].into_iter().collect());
    }

    #[test]
    fn range_order_is_frozen_by_stable_id_and_work_is_bounded() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.begin_range(view(&[1, 2, 3, 4, 5]), 5, RangeMode::Add);
        selection.pump(2);
        assert_eq!(selection.len(), 2);
        assert!(selection.is_pending());
        selection.begin_range(view(&[1, 5, 4, 3, 2]), 3, RangeMode::Add);
        finish(&mut selection);
        assert_eq!(selection.len(), 3);
        assert!(selection.contains(2));
        assert!(selection.contains(3));
        assert!(!selection.contains(5));
    }

    #[test]
    fn toggle_cancels_old_pump_and_directory_reset_clears_membership() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.begin_range(view(&[1, 2, 3, 4, 5]), 5, RangeMode::Add);
        selection.pump(2);
        selection.toggle(5);
        assert!(!selection.is_pending());
        assert!(selection.contains(5));
        assert!(!selection.contains(3));
        assert!(!selection.pump(100));
        selection.reset(9);
        assert!(!selection.active());
        assert_eq!(selection.directory(), 9);
        assert!(selection.is_empty());
    }

    #[test]
    fn range_pump_can_filter_deleted_and_hidden_entries() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.begin_range(view(&[1, 2, 3, 4]), 4, RangeMode::Add);
        selection.pump_filtered(100, |id| id != 2 && id != 4);
        assert!(!selection.is_pending());
        assert_eq!(selection.len(), 2);
        assert!(selection.contains(1));
        assert!(selection.contains(3));
    }

    #[test]
    fn deleted_unmaterialized_anchor_stays_excluded_across_ranges() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for backwards in [false, true] {
                for stale in [false, true] {
                    let anchor = if backwards { 5 } else { 1 };
                    let mut ids = vec![1, 2, 3, 4, 5];
                    if !stale {
                        ids.retain(|&id| id != anchor);
                    }
                    let mut selection = VisualSelection::new(0).unwrap();
                    selection.enter(0, Some(anchor));
                    selection.remove(anchor);
                    assert!(selection.begin_range(view(&ids), 3, mode));
                    finish(&mut selection);
                    assert!(!selection.contains(anchor));
                    assert!(selection.contains(3));
                    if stale {
                        assert!(selection.contains(if backwards { 4 } else { 2 }));
                    }
                    // A later stale snapshot must not restore the deleted ID,
                    // including when the first snapshot omitted its old slot.
                    selection.begin_range(view(&[1, 2, 3, 4, 5]), anchor, mode);
                    finish(&mut selection);
                    assert!(!selection.contains(anchor));
                }
            }
        }
    }

    #[test]
    fn explicit_toggle_replaces_a_deleted_unmaterialized_anchor() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.remove(1);
        selection.toggle(3);
        selection.toggle(3);
        selection.begin_range(view(&[1, 2, 3, 4]), 4, RangeMode::Add);
        finish(&mut selection);
        assert_eq!(selection.len(), 1);
        assert!(selection.contains(4));
        assert!(!selection.contains(1) && !selection.contains(2) && !selection.contains(3));
    }

    #[test]
    fn deleting_the_anchor_allows_a_new_range_in_the_updated_directory() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.begin_range(view(&[1, 2, 3]), 3, RangeMode::Add);
        selection.pump(2);
        selection.remove(1);
        assert!(selection.is_pending());
        finish(&mut selection);
        assert!(selection.begin_range(view(&[2, 3, 4]), 3, RangeMode::Add));
        finish(&mut selection);
        assert!(!selection.contains(1));
        assert!(selection.contains(2));
        assert!(selection.contains(3));
        assert!(!selection.contains(4));
    }

    #[test]
    fn an_invalid_initial_lead_cannot_install_an_inverse_gesture() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        assert!(!selection.begin_range(view(&[1, 2]), 99, RangeMode::Invert));
        assert!(!selection.is_pending());
        assert!(!selection.pump(100));
        assert_eq!(selection.len(), 1);
        assert!(selection.contains(1));
    }

    #[test]
    fn restoring_mouse_press_baseline_makes_inverse_drag_independent_of_click_pumping() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        let before_press = selection.snapshot();
        selection.begin_range(view(&[1, 2, 3, 4]), 3, RangeMode::Add);
        finish(&mut selection);
        assert!(selection.contains(2));

        selection.restore_snapshot(before_press, 3);
        selection.begin_range(view(&[1, 2, 3, 4]), 4, RangeMode::Invert);
        finish(&mut selection);
        assert!(selection.contains(1));
        assert!(!selection.contains(2));
        assert!(selection.contains(3));
        assert!(selection.contains(4));
    }

    #[test]
    fn deleting_a_pending_range_anchor_preserves_all_intended_surviving_members() {
        let mut selection = VisualSelection::new(0).unwrap();
        selection.enter(0, Some(1));
        selection.begin_range(view(&[1, 2, 3, 4, 5]), 5, RangeMode::Add);
        selection.pump(2);
        selection.remove(1);
        assert!(selection.is_pending());
        finish(&mut selection);
        assert!(!selection.contains(1));
        for id in 2..=5 {
            assert!(selection.contains(id));
        }
        assert_eq!(selection.len(), 4);
    }

    #[test]
    fn toggled_anchor_keeps_its_mark_when_extending_in_either_direction() {
        for lead in [1, 3] {
            for initially_marked in [false, true] {
                let mut selection = VisualSelection::new(0).unwrap();
                selection.enter(0, None);
                if initially_marked {
                    selection.toggle(2);
                }
                selection.toggle(2);
                selection.begin_range(view(&[1, 2, 3]), lead, RangeMode::Add);
                finish(&mut selection);
                assert_eq!(selection.contains(2), !initially_marked);
                assert!(selection.contains(lead));
                selection.begin_range(view(&[3, 2, 1]), 2, RangeMode::Add);
                finish(&mut selection);
                assert_eq!(selection.contains(2), !initially_marked);
                assert!(!selection.contains(lead));

                selection.reanchor(2);
                selection.begin_range(view(&[1, 2, 3]), lead, RangeMode::Add);
                finish(&mut selection);
                assert!(selection.contains(2) && selection.contains(lead));
            }
        }
    }

    #[test]
    fn explicit_anchor_membership_survives_segments_reversal_and_deletion() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for marked in [false, true] {
                for pending in [false, true] {
                    let mut selection = VisualSelection::new(0).unwrap();
                    selection.enter(0, Some(99));
                    selection.toggle(2);
                    if !marked {
                        selection.toggle(2);
                    }
                    selection.begin_range(view(&[1, 2, 3]), 3, mode);
                    if pending {
                        selection.pump(1);
                    } else {
                        finish(&mut selection);
                    }
                    selection.begin_range(view(&[4, 2, 1, 3]), 4, mode);
                    finish(&mut selection);
                    assert_eq!(selection.contains(2), marked);
                    assert!(selection.contains(1) && selection.contains(3));
                    assert!(selection.contains(4) && selection.contains(99));

                    // Reverse past the explicit anchor, then cross it again in
                    // another segment without turning a shared boundary into
                    // an additional explicit membership override.
                    selection.begin_range(view(&[4, 2, 1, 3]), 1, mode);
                    finish(&mut selection);
                    assert_eq!(selection.contains(2), marked);
                    assert!(!selection.contains(4));
                    selection.begin_range(view(&[5, 2, 3, 1, 4]), 5, mode);
                    finish(&mut selection);
                    assert_eq!(selection.contains(2), marked);
                    assert_eq!(selection.contains(3), mode == RangeMode::Add);
                    assert!(selection.contains(1) && selection.contains(5));
                    assert!(selection.contains(99));

                    // Deletion still wins even over an explicit selected mark.
                    selection.remove(2);
                    selection.begin_range(view(&[6, 2, 5, 3, 1, 4]), 6, mode);
                    finish(&mut selection);
                    assert!(!selection.contains(2));
                    assert!(selection.contains(6));
                }
            }
        }
    }

    #[test]
    fn deliberate_gestures_reset_the_original_explicit_anchor_override() {
        for gesture in 0..3 {
            let mut selection = VisualSelection::new(0).unwrap();
            selection.enter(0, Some(1));
            selection.toggle(1);
            selection.begin_range(view(&[1, 2]), 2, RangeMode::Add);
            finish(&mut selection);
            let next = view(&[3, 1, 2]);
            selection.begin_range(Arc::clone(&next), 3, RangeMode::Add);
            finish(&mut selection);
            assert!(!selection.contains(1));
            assert!(selection.contains(2) && selection.contains(3));
            match gesture {
                0 => {
                    // A new drag starts at the explicitly deselected row.
                    selection.reanchor(1);
                    selection.begin_range(next, 3, RangeMode::Add);
                }
                1 => {
                    // Shift-click deliberately adds even after its current
                    // segment anchor was deleted, keeping deletion exclusions.
                    selection.remove(2);
                    selection.add_range(next, 3);
                }
                _ => {
                    selection.toggle(1);
                    selection.begin_range(next, 3, RangeMode::Invert);
                }
            }
            finish(&mut selection);
            assert!(selection.contains(1));
            assert_eq!(selection.contains(2), gesture != 1);
        }
    }

    #[test]
    fn new_rows_start_bounded_segments_without_losing_previous_marks() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for (endpoint, lead) in [(1, 0), (3, 4)] {
                for pending in [false, true] {
                    let mut selection = VisualSelection::new(0).unwrap();
                    selection.enter(0, Some(99));
                    selection.toggle(2);
                    selection.reanchor(2);
                    selection.begin_range(view(&[1, 2, 3]), endpoint, mode);
                    if pending {
                        selection.pump(1);
                        assert!(selection.is_pending());
                    } else {
                        finish(&mut selection);
                    }
                    assert!(selection.begin_range(view(&[0, 1, 2, 3, 4]), lead, mode));
                    assert!(selection.is_pending());
                    assert!(!selection.contains(lead));
                    selection.pump(1);
                    assert!(
                        selection.is_pending(),
                        "one step cannot finish both segments"
                    );
                    finish(&mut selection);
                    assert!(selection.contains(99));
                    assert!(selection.contains(endpoint));
                    assert!(selection.contains(lead));
                    assert_eq!(selection.contains(2), mode == RangeMode::Add);
                    assert_eq!(selection.len(), if mode == RangeMode::Add { 4 } else { 3 });

                    // Reversing the new segment restores its baseline, and
                    // reordering cannot reinterpret the original row interval.
                    selection.begin_range(view(&[4, 3, 2, 1, 0]), endpoint, mode);
                    finish(&mut selection);
                    assert!(!selection.contains(lead));
                    assert!(selection.contains(endpoint) && selection.contains(99));
                    assert_eq!(selection.contains(2), mode == RangeMode::Add);
                }
            }
        }
    }

    #[test]
    fn deleting_an_anchor_keeps_pending_and_completed_ranges_reversible() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for pending in [false, true] {
                for reversed in [false, true] {
                    let mut ids = vec![1, 2, 3, 4, 5];
                    if reversed {
                        ids.reverse();
                    }
                    let mut selection = VisualSelection::new(0).unwrap();
                    selection.enter(0, Some(1));
                    selection.toggle(4);
                    selection.reanchor(1);
                    selection.begin_range(view(&ids), 3, mode);
                    if pending {
                        selection.pump(1);
                    } else {
                        finish(&mut selection);
                    }
                    selection.remove(1);
                    ids.retain(|&id| id != 1);
                    selection.begin_range(view(&ids), 5, mode);
                    finish(&mut selection);
                    assert!(!selection.contains(1));
                    assert!(selection.contains(2) && selection.contains(3));
                    assert_eq!(selection.contains(4), mode == RangeMode::Add);
                    assert!(selection.contains(5));

                    // A resort cannot reinterpret the surviving frozen interval.
                    ids.reverse();
                    selection.begin_range(view(&ids), 2, mode);
                    finish(&mut selection);
                    assert_eq!(selection.len(), 2);
                    assert!(selection.contains(2) && selection.contains(4));
                }
            }
        }
    }

    #[test]
    fn mode_changes_keep_deleted_anchor_intervals_and_exclusions() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for pending in [false, true] {
                for backwards in [false, true] {
                    for stale in [false, true] {
                        let anchor = if backwards { 5 } else { 1 };
                        let removed = if backwards { 4 } else { 2 };
                        let lead = if backwards { 1 } else { 5 };
                        let mut selection = VisualSelection::new(0).unwrap();
                        selection.enter(0, Some(99));
                        if mode == RangeMode::Invert {
                            for id in 1..=5 {
                                selection.toggle(id);
                            }
                        }
                        selection.reanchor(anchor);
                        selection.begin_range(view(&[1, 2, 3, 4, 5]), 3, mode);
                        if pending {
                            selection.pump(1);
                        } else {
                            finish(&mut selection);
                        }
                        selection.remove(anchor);
                        selection.remove(removed);
                        let baseline = selection.snapshot();
                        let mut next: Vec<_> = (1..=5)
                            .filter(|&id| stale || (id != anchor && id != removed))
                            .collect();
                        next.reverse();
                        let next_mode = match mode {
                            RangeMode::Add => RangeMode::Invert,
                            RangeMode::Invert | RangeMode::Remove => RangeMode::Add,
                        };
                        assert!(selection.begin_range(view(&next), lead, next_mode));
                        assert_eq!(selection.snapshot(), baseline);
                        selection.pump(1);
                        assert!(selection.is_pending());
                        finish(&mut selection);
                        for id in 1..=5 {
                            let expected = id != anchor
                                && id != removed
                                && (next_mode == RangeMode::Add || !baseline.contains(&id));
                            assert_eq!(selection.contains(id), expected);
                        }
                        assert!(selection.contains(99));

                        // Reversal still uses the original interval and restores
                        // the membership committed at the mode change.
                        assert!(selection.begin_range(view(&next), 3, next_mode));
                        finish(&mut selection);
                        for id in 1..=5 {
                            let in_range = if backwards { id >= 3 } else { id <= 3 };
                            let expected = id != anchor
                                && id != removed
                                && if in_range {
                                    next_mode == RangeMode::Add || !baseline.contains(&id)
                                } else {
                                    baseline.contains(&id)
                                };
                            assert_eq!(selection.contains(id), expected);
                        }
                        assert!(selection.contains(99));
                    }
                }
            }
        }
    }

    #[test]
    fn deleted_segment_endpoints_preserve_surviving_marks_and_new_rows() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for pending in [false, true] {
                for reversed in [false, true] {
                    for stale in [false, true] {
                        let mut old = vec![1, 2, 3];
                        let mut next = if stale {
                            vec![1, 2, 3, 4, 5, 6]
                        } else {
                            vec![1, 2, 4, 5, 6]
                        };
                        if reversed {
                            old.reverse();
                            next.reverse();
                        }
                        let mut selection = VisualSelection::new(0).unwrap();
                        selection.enter(0, None);
                        selection.toggle(2);
                        selection.reanchor(1);
                        selection.begin_range(view(&old), 3, mode);
                        if pending {
                            selection.pump(1);
                        } else {
                            finish(&mut selection);
                        }
                        selection.remove(3);
                        selection.begin_range(view(&next), 6, mode);
                        finish(&mut selection);
                        assert!(selection.contains(1));
                        assert_eq!(selection.contains(2), mode == RangeMode::Add);
                        assert!(!selection.contains(3));
                        assert!(selection.contains(4) && selection.contains(5));
                        assert!(selection.contains(6));

                        selection.begin_range(view(&next), 4, mode);
                        finish(&mut selection);
                        assert!(selection.contains(4));
                        assert!(!selection.contains(3) && !selection.contains(5));
                        assert!(!selection.contains(6));
                        selection.begin_range(view(&next), 2, mode);
                        finish(&mut selection);
                        assert_eq!(selection.len(), if mode == RangeMode::Add { 2 } else { 1 });
                        assert!(selection.contains(1));
                        assert_eq!(selection.contains(2), mode == RangeMode::Add);
                    }
                }
            }
        }
    }

    #[test]
    fn additive_clicks_commit_marks_and_retain_deleted_anchor_slots() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            for deleted_anchor in [false, true] {
                for new_rows in [false, true] {
                    for reversed in [false, true] {
                        let mut old = if new_rows {
                            vec![1, 2, 3]
                        } else {
                            vec![1, 2, 3, 4, 5]
                        };
                        let mut next = vec![1, 2, 3, 4, 5];
                        if deleted_anchor {
                            next.remove(0);
                        }
                        if reversed {
                            old.reverse();
                            next.reverse();
                        }
                        let mut selection = VisualSelection::new(0).unwrap();
                        selection.enter(0, Some(99));
                        selection.toggle(1);
                        selection.toggle(2);
                        selection.reanchor(1);
                        selection.begin_range(view(&old), 3, mode);
                        finish(&mut selection);
                        if deleted_anchor {
                            selection.remove(1);
                        }
                        let before = selection.snapshot();
                        assert!(selection.add_range(view(&next), 5));
                        assert_eq!(selection.snapshot(), before);
                        selection.pump(1);
                        assert!(selection.is_pending());
                        finish(&mut selection);
                        assert_eq!(selection.contains(1), !deleted_anchor);
                        assert!(selection.contains(99));
                        for id in 2..=5 {
                            assert!(selection.contains(id));
                        }

                        // A second Shift-click adds without undoing the first.
                        let committed = selection.snapshot();
                        selection.add_range(view(&next), 2);
                        finish(&mut selection);
                        assert_eq!(selection.snapshot(), committed);
                    }
                }
            }
        }
    }

    #[test]
    fn finding_a_surviving_segment_boundary_obeys_the_pump_budget() {
        for mode in [RangeMode::Add, RangeMode::Invert] {
            let old: Vec<_> = (1..=4097).collect();
            let mut selection = VisualSelection::new(0).unwrap();
            selection.enter(0, Some(1));
            selection.begin_range(view(&old), 4097, mode);
            finish(&mut selection);
            for id in 2..=4097 {
                selection.remove(id);
            }
            // Keep removed slots in the current cache to exercise exclusions as
            // well as bounded boundary recovery and their transfer to the segment.
            let next: Vec<_> = (1..=4099).collect();
            selection.begin_range(view(&next), 4099, mode);
            assert_eq!(selection.pending_range.as_ref().unwrap().boundary, 4096);
            assert!(selection.pump(8));
            assert_eq!(selection.pending_range.as_ref().unwrap().boundary, 4088);
            assert!(!selection.contains(4098) && !selection.contains(4099));
            finish(&mut selection);
            assert_eq!(selection.len(), if mode == RangeMode::Add { 3 } else { 2 });
            assert_eq!(selection.contains(1), mode == RangeMode::Add);
            assert!(selection.contains(4098) && selection.contains(4099));
            selection.begin_range(view(&next), 1, mode);
            finish(&mut selection);
            assert_eq!(selection.len(), usize::from(mode == RangeMode::Add));
        }
    }

    #[test]
    fn pending_new_row_can_be_replaced_or_cancelled_by_the_same_gesture() {
        for cancel in [false, true] {
            let mut selection = VisualSelection::new(0).unwrap();
            selection.enter(0, Some(1));
            selection.begin_range(view(&[1, 2, 3]), 3, RangeMode::Add);
            selection.pump(1);
            selection.begin_range(view(&[1, 2, 3, 4]), 4, RangeMode::Add);
            let lead = if cancel { 2 } else { 5 };
            selection.begin_range(view(&[1, 2, 3, 4, 5]), lead, RangeMode::Add);
            finish(&mut selection);
            for id in 1..=5 {
                assert_eq!(selection.contains(id), id <= lead);
            }
        }
    }
}
