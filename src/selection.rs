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
    range: Option<Range>,
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
            range: None,
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
        if self.selected.contains(&id) {
            self.selected.remove(&id);
        } else {
            self.selected.insert(id);
        }
        true
    }

    /// Restore the persistent baseline captured at mouse press before deciding
    /// whether the gesture is an additive click or an inverse drag.
    pub fn restore_snapshot(&mut self, selected: HashSet<NodeId>, anchor: NodeId) {
        self.retired.push(Retired {
            _selected: std::mem::replace(&mut self.selected, selected),
            _range: self.range.take(),
            _order: None,
        });
        self.active = true;
        self.anchor = Some(anchor);
        self.flush_retired();
    }

    /// Begin a new gesture without changing this row's selected membership.
    pub fn reanchor(&mut self, id: NodeId) {
        self.anchor = Some(id);
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
            self.anchor = None;
        }
        if let Some(range) = &mut self.range {
            // The frozen slot still defines this gesture's interval. Retain it
            // until the intended range has materialized, while never restoring
            // deleted IDs through additive or inverse pumping.
            range.baseline.remove(&id);
            range.excluded.insert(id);
        }
    }

    /// The first snapshot fixes a gesture's row order. Later resorts cannot
    /// retarget a range to unrelated IDs. Additive and inverse gestures both
    /// restore their original baseline when the endpoint reverses.
    pub fn begin_range(
        &mut self,
        snapshot: Arc<CachedSnapshot>,
        lead: NodeId,
        mode: RangeMode,
    ) -> bool {
        if !self.active {
            return false;
        }
        if self.anchor.is_none() && self.range.is_some() && !self.is_pending() {
            self.reanchor(lead);
        }
        if self.range.as_ref().is_some_and(|range| range.mode != mode) {
            self.reanchor(self.anchor.unwrap_or(lead));
        }
        if self.range.is_none() {
            if !snapshot.positions.contains_key(&lead) {
                return false;
            }
            let anchor_id = self
                .anchor
                .filter(|id| snapshot.positions.contains_key(id))
                .unwrap_or(lead);
            let anchor = snapshot.positions[&anchor_id];
            self.anchor = Some(anchor_id);
            self.range = Some(Range {
                snapshot,
                baseline: self.selected.clone(),
                excluded: HashSet::new(),
                anchor,
                desired: (anchor, anchor),
                applied: None,
                mode,
            });
        }
        let range = self.range.as_mut().expect("range was initialized");
        let Some(&lead) = range.snapshot.positions.get(&lead) else {
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
        let Some(range) = &mut self.range else {
            return false;
        };
        let mut progressed = false;
        for _ in 0..limit {
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
            let selected = if entering {
                match range.mode {
                    RangeMode::Add => true,
                    RangeMode::Invert => !baseline,
                }
            } else {
                baseline
            };
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
            directory_ids: Vec::new(),
            positions: ids
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, i))
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
}
