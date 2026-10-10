use crate::backend::RootAnchor;
use crate::batch::{BatchCapture, BatchEvent};
use crate::delete::{self, ConfirmedTarget};
use crate::model::Model;
use crate::order::{CachedOrderWorker, CachedRow, CachedSnapshot, OrderConfig};
use crate::queue::{DeleteQueue, QueueId};
use crate::retire::Retirer;
use crate::scan::{self, ScanHandle, ScanOptions};
use crate::selection::{RangeMode, VisualSelection};
use crate::types::{Entry, NodeId, NodeKind, ROOT, ScanEvent, SizeMode, SortMode};
use crossbeam_channel::{Receiver, TryRecvError, bounded};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, IsTerminal};
use std::panic::{self, PanicHookInfo};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const INPUT_INTERVAL: Duration = Duration::from_millis(25);
const WORK_BUDGET: Duration = Duration::from_millis(4);
const MAX_BATCHES: usize = 32;
const ORDER_BATCH: usize = 256;
const PENDING_LIMIT: usize = 8192;
const VIEW_BUDGET: usize = 1024;
const SCROLL_MARGIN: usize = 3;
const VISUAL_INTENT_LIMIT: usize = 128;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard(Option<Arc<PanicHook>>);
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self(None);
        crate::terminal_input::configure_read()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide, EnableMouseCapture)?;
        let previous = Arc::new(panic::take_hook());
        let hook = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            hook(info);
        }));
        guard.0 = Some(previous);
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        if !std::thread::panicking() {
            if let Some(previous) = self.0.take() {
                drop(panic::take_hook());
                if let Ok(previous) = Arc::try_unwrap(previous) {
                    panic::set_hook(previous);
                }
            }
        }
    }
}
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Graph {
    Bar,
    Percent,
    Off,
}
enum Modal {
    Text {
        title: &'static str,
        text: String,
        scroll: u16,
    },
    Queue {
        selected: Option<QueueId>,
        offset: Option<QueueId>,
    },
    Batch {
        target: String,
        count: usize,
        ready: bool,
        reviewable: bool,
        scroll: u16,
    },
    Enqueueing,
    Quit {
        reviewable: bool,
    },
    Stopping,
    Delete {
        id: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
        target: String,
        scroll: u16,
        reviewable: bool,
        confirmed: Option<ConfirmedTarget>,
        capture: Option<Receiver<io::Result<ConfirmedTarget>>>,
    },
}
impl Modal {
    fn help(visual: bool) -> Self {
        Self::Text {
            title: "Help",
            text: format!(
                "{}\n\n{COMMON_HELP}",
                if visual { VISUAL_HELP } else { NORMAL_HELP }
            ),
            scroll: 0,
        }
    }
    fn info(text: String) -> Self {
        Self::Text {
            title: "Info",
            text,
            scroll: 0,
        }
    }
    fn notice(title: &'static str, text: String) -> Self {
        Self::Text {
            title,
            text,
            scroll: 0,
        }
    }
}
enum Action {
    Continue,
    Refresh(NodeId),
    QuitWarning,
    Quit,
    QueueNavigate { end: Option<bool>, amount: isize },
    QueuePause,
    QueueCancel,
    QueueRemove,
    QueueDelete,
    QueueReorder(isize),
    BatchPrepare,
    BatchConfirm,
    BatchCancel,
    Delete(NodeId, Arc<RootAnchor>, PathBuf, ConfirmedTarget),
}

struct PendingMouse {
    anchor: NodeId,
    lead: NodeId,
    dragged: bool,
    released: bool,
    mode: RangeMode,
    edge: isize,
    view: Arc<CachedSnapshot>,
    offset: usize,
}

enum VisualIntent {
    Navigate(Navigation, bool),
    ToggleCurrent,
    ToggleStable(NodeId),
    CursorId(NodeId),
    Mouse(PendingMouse),
}

#[derive(Default)]
struct QueueView {
    status: String,
    active: bool,
    cancelling: bool,
    pending: usize,
    paused: bool,
    rows: Vec<(QueueId, bool, String)>,
    selected: Option<QueueId>,
    visible_rows: usize,
}

struct Preview {
    directory: Option<NodeId>,
    view: Arc<CachedSnapshot>,
    empty: Arc<CachedSnapshot>,
}

impl Preview {
    fn new(empty: Arc<CachedSnapshot>) -> Self {
        Self {
            directory: None,
            view: Arc::clone(&empty),
            empty,
        }
    }

    fn sync(&mut self, directory: Option<NodeId>, order: &CachedOrderWorker, model: &Model) {
        if self.directory != directory {
            self.directory = directory;
            self.view = Arc::clone(&self.empty);
        }
        if let Some(view) = directory.and_then(|id| {
            order
                .snapshot(id)
                .filter(|view| view.child_epoch == model.node(id).child_epoch)
        }) {
            self.view = view;
        } else if self.view.generation != order.generation()
            || directory.is_some_and(|id| self.view.child_epoch != model.node(id).child_epoch)
        {
            self.view = Arc::clone(&self.empty);
        }
    }
}

#[derive(Clone, Copy)]
struct Navigation {
    end: Option<bool>,
    amount: isize,
    cursor: Option<NavigationCursor>,
}

#[derive(Clone, Copy)]
struct NavigationCursor {
    next: Option<usize>,
    remaining: usize,
    target: Option<NodeId>,
    baseline: bool,
}

impl Navigation {
    fn new(end: Option<bool>, amount: isize) -> Self {
        Self {
            end,
            amount,
            cursor: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CursorMode {
    Top,
    Waiting,
    Tracked,
}

struct App {
    model: Model,
    root: PathBuf,
    directory: NodeId,
    selected: Option<NodeId>,
    view: Arc<CachedSnapshot>,
    empty: Arc<CachedSnapshot>,
    view_directory: NodeId,
    offset: usize,
    page_size: usize,
    config: OrderConfig,
    filter: String,
    filter_editing: bool,
    order: CachedOrderWorker,
    next_new: usize,
    replay_cursor: Option<usize>,
    pending: VecDeque<NodeId>,
    pending_set: HashSet<NodeId>,
    unsent: Vec<CachedRow>,
    removals: Vec<NodeId>,
    preview: Preview,
    navigation_until: Instant,
    pending_navigation: Option<Navigation>,
    scan_entries: Option<(NodeId, std::vec::IntoIter<Entry>)>,
    graph: Graph,
    counts: bool,
    modal: Option<Modal>,
    delete_notice: Option<String>,
    started: bool,
    queue_view: QueueView,
    baseline_finished: bool,
    visual: VisualSelection,
    range_requested: bool,
    prioritize_requested: bool,
    list_area: Rect,
    mouse_dragging: bool,
    drag_anchor: Option<NodeId>,
    previous_modal: Option<Modal>,
    visual_intents: VecDeque<VisualIntent>,
    painted_rows: Vec<NodeId>,
    painted_view: Arc<CachedSnapshot>,
    painted_offset: usize,
    painted_directory: NodeId,
    mouse_baseline: Option<im::HashSet<NodeId>>,
    drag_started: bool,
    drag_mode: RangeMode,
    drag_scroll: Option<(isize, usize, Instant)>,
    mouse_position: Option<(u16, u16)>,
    capture_inflight: Option<Arc<AtomicBool>>,
    focus_after_selection: bool,
    cursor_mode: CursorMode,
    deferred_batch: Option<BatchRequest>,
    waiting_batch: Option<WaitingBatch>,
    deferred_delete: Option<DeleteRequest>,
    mouse_press_view: Option<Arc<CachedSnapshot>>,
}

impl App {
    fn new(root: PathBuf, size_mode: SizeMode) -> io::Result<Self> {
        let mut order = CachedOrderWorker::new()?;
        let config = OrderConfig {
            size_mode,
            ..OrderConfig::default()
        };
        order.reset(config);
        let empty = Arc::new(CachedSnapshot {
            generation: 0,
            child_epoch: 0,
            ids: Vec::new(),
            positions: HashMap::new(),
        });
        let preview = Preview::new(Arc::clone(&empty));
        Ok(Self {
            model: Model::new(root.clone()),
            root,
            directory: ROOT,
            selected: None,
            view: Arc::clone(&empty),
            empty: Arc::clone(&empty),
            view_directory: ROOT,
            offset: 0,
            page_size: 1,
            config,
            filter: String::new(),
            filter_editing: false,
            order,
            next_new: 0,
            replay_cursor: None,
            pending: VecDeque::new(),
            pending_set: HashSet::new(),
            unsent: Vec::new(),
            removals: Vec::new(),
            preview,
            navigation_until: Instant::now(),
            pending_navigation: None,
            scan_entries: None,
            graph: Graph::Bar,
            counts: false,
            modal: None,
            delete_notice: None,
            started: false,
            queue_view: QueueView::default(),
            baseline_finished: false,
            visual: VisualSelection::new(ROOT)?,
            range_requested: false,
            prioritize_requested: false,
            list_area: Rect::default(),
            mouse_dragging: false,
            drag_anchor: None,
            previous_modal: None,
            visual_intents: VecDeque::with_capacity(VISUAL_INTENT_LIMIT),
            painted_rows: Vec::new(),
            painted_view: Arc::clone(&empty),
            painted_offset: 0,
            painted_directory: ROOT,
            mouse_baseline: None,
            drag_started: false,
            drag_mode: RangeMode::Add,
            drag_scroll: None,
            mouse_position: None,
            capture_inflight: None,
            focus_after_selection: false,
            cursor_mode: CursorMode::Top,
            deferred_batch: None,
            waiting_batch: None,
            deferred_delete: None,
            mouse_press_view: None,
        })
    }

    fn reset_order(&mut self, keep_previous: bool) {
        if self.view_directory != self.directory {
            self.filter.clear();
            self.filter_editing = false;
            self.order.clear_filter();
        }
        if keep_previous {
            self.order.configure(self.config);
            if let Some(navigation) = self.pending_navigation.as_mut() {
                navigation.cursor = None;
            }
        } else {
            self.cursor_mode = if self.selected.is_some() {
                CursorMode::Tracked
            } else {
                CursorMode::Waiting
            };
            self.pending_navigation = None;
            self.view = Arc::clone(&self.empty);
            self.view_directory = self.directory;
            self.offset = 0;
        }
        self.navigation_until = Instant::now();
        self.order.prioritize(self.directory, self.selected);
        self.sync_order(true);
    }

    fn changed(&mut self, ids: Vec<NodeId>) {
        for id in ids {
            if !self.model.is_live(id) {
                self.removals.push(id);
            } else if (id as usize) < self.next_new && self.pending_set.insert(id) {
                if self.pending.len() >= PENDING_LIMIT {
                    self.pending_set.remove(&id);
                    self.replay_cursor = Some(
                        self.replay_cursor
                            .map_or(id as usize, |cursor| cursor.min(id as usize)),
                    );
                } else {
                    self.pending.push_back(id);
                }
            }
        }
    }

    fn delete_failed(&mut self, id: NodeId, error: &io::Error) {
        let message = printable(OsStr::new(&error.to_string()));
        let changed = self
            .model
            .note_error(id, format!("Delete failed: {message}"));
        self.changed(changed);
        let name = clipped(&printable(self.model.node(id).name.as_ref()), 80);
        self.delete_notice = Some(format!("{name}: {}", clipped(&message, 180)));
        self.show_delete_notice();
    }

    fn show_delete_notice(&mut self) -> bool {
        if self.modal.is_none() {
            if let Some(message) = self.delete_notice.take() {
                self.modal = Some(Modal::notice("Delete failed", message));
                return true;
            }
        }
        false
    }

    fn deletion_succeeded(&mut self, id: NodeId, parent_metadata_available: bool) {
        let parent = self.model.node(id).parent;
        self.removed(id);
        if !parent_metadata_available {
            if let Some(parent) = parent {
                let changed = self.model.mark_metadata_uncertain(parent);
                self.changed(changed);
            }
        }
    }

    fn removed(&mut self, id: NodeId) {
        let old_index = self.selection_index().unwrap_or(0);
        let changed = self.model.remove(id);
        // Deleted rows may already have contributed to a partial page movement.
        if let Some(navigation) = self.pending_navigation.as_mut() {
            navigation.cursor = None;
        }
        self.changed(changed);
        self.visual.remove(id);
        if let Some(baseline) = &mut self.mouse_baseline {
            baseline.remove(&id);
        }
        for modal in [&mut self.modal, &mut self.previous_modal] {
            if matches!(modal.as_ref(), Some(Modal::Text { title: "Info", .. }))
                || matches!(modal.as_ref(), Some(Modal::Delete { id: target, .. }) if !self.model.is_live(*target))
            {
                *modal = None;
            }
        }
        if !self.model.is_live(self.directory) {
            while !self.model.is_live(self.directory) {
                self.directory = self.model.node(self.directory).parent.unwrap_or(ROOT);
            }
            self.selected = None;
            self.focus_after_selection = true;
            self.prioritize_requested = true;
            self.reset_order(false);
        } else if self
            .selected
            .is_some_and(|selected| !self.model.is_live(selected))
        {
            self.selected = self.view.ids[old_index.min(self.view.ids.len())..]
                .iter()
                .take(128)
                .copied()
                .find(|&next| self.eligible(next))
                .or_else(|| {
                    self.view.ids[..old_index.min(self.view.ids.len())]
                        .iter()
                        .rev()
                        .take(128)
                        .copied()
                        .find(|&next| self.eligible(next))
                });
            self.focus_after_selection |= self.selected.is_none();
            self.prioritize_requested = true;
            self.keep_selection_visible();
        }
    }

    fn pump_order(&mut self) -> bool {
        // Navigation must progress even when the producer queue is full.
        self.sync_order(false);
        if !self.order.pump_config() {
            return false;
        }
        let mut progress = false;
        let deadline = Instant::now() + WORK_BUDGET;
        if !self.removals.is_empty() {
            if let Err(ids) = self.order.try_remove(std::mem::take(&mut self.removals)) {
                self.removals = ids;
                return false;
            }
            progress = true;
        }
        loop {
            if !self.unsent.is_empty() {
                match self.order.try_upsert(std::mem::take(&mut self.unsent)) {
                    Ok(()) => progress = true,
                    Err(rows) => {
                        self.unsent = rows;
                        break;
                    }
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            for turn in 0..ORDER_BATCH {
                let Some((id, structural)) = self.next_order_id(turn) else {
                    break;
                };
                progress = true;
                if structural || self.model.is_live(id) {
                    self.unsent.push(CachedRow::from_model(
                        id,
                        &self.model,
                        self.config.size_mode,
                    ));
                }
            }
            if self.unsent.is_empty() {
                break;
            }
        }
        progress
            && (!self.unsent.is_empty()
                || self.next_new < self.model.len()
                || self.replay_cursor.is_some()
                || !self.pending.is_empty())
    }

    fn next_order_id(&mut self, turn: usize) -> Option<(NodeId, bool)> {
        for offset in 0..3 {
            match (turn + offset) % 3 {
                0 if self.next_new < self.model.len() => {
                    // Keep every structural parent, including removed placeholders,
                    // ahead of its children in the worker's bounded command stream.
                    let id = self.next_new as NodeId;
                    self.next_new += 1;
                    return Some((id, true));
                }
                1 => {
                    if let Some(index) = self.replay_cursor {
                        if index < self.next_new {
                            self.replay_cursor = Some(index + 1);
                            return Some((index as NodeId, false));
                        }
                        self.replay_cursor = None;
                    }
                }
                2 => {
                    if let Some(id) = self.pending.pop_front() {
                        self.pending_set.remove(&id);
                        return Some((id, false));
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn sync_order(&mut self, force: bool) {
        if self.view_directory == self.directory
            && self.view.child_epoch != self.model.node(self.directory).child_epoch
        {
            self.view = Arc::clone(&self.empty);
            self.offset = 0;
            if let Some(navigation) = self.pending_navigation.as_mut() {
                navigation.cursor = None;
            }
        }
        if (self.mouse_press_view.is_some() || self.has_held_mouse())
            && self.pending_navigation.is_none()
        {
            return;
        }
        if !force && self.pending_navigation.is_none() && Instant::now() < self.navigation_until {
            return;
        }
        // A cursor belongs to the held order. Finish its bounded traversal before
        // accepting another live resort; a new filtering epoch can replace it.
        let traversing = self.view.generation == self.order.generation()
            && self.pending_navigation.is_some_and(|navigation| {
                navigation
                    .cursor
                    .is_some_and(|cursor| cursor.next.is_some())
            });
        if !traversing {
            if let Some(view) = self
                .current_snapshot()
                .filter(|view| view.child_epoch == self.model.node(self.directory).child_epoch)
            {
                if !Arc::ptr_eq(&self.view, &view) {
                    let row = (self.view_directory == self.directory)
                        .then(|| self.selection_screen_row())
                        .flatten();
                    self.view_directory = self.directory;
                    self.view = view;
                    if let Some(navigation) = self.pending_navigation.as_mut() {
                        navigation.cursor = None;
                    }
                    if self.cursor_mode == CursorMode::Top {
                        self.selected = None;
                        self.offset = 0;
                        self.pending_navigation = Some(Navigation::new(Some(false), 0));
                    } else if self.selected.is_some_and(|id| {
                        !self.eligible(id)
                            || self.model.node(id).parent != Some(self.directory)
                            || (!self.filter.is_empty() && !self.view.positions.contains_key(&id))
                    }) {
                        self.selected = None;
                        self.focus_after_selection = true;
                        self.prioritize_requested = true;
                    }
                    if self.selected.is_none() && self.pending_navigation.is_none() {
                        self.pending_navigation = Some(Navigation::new(Some(false), 0));
                    }
                    if let (Some(row), Some(index)) = (row, self.selection_index()) {
                        self.offset = self.offset_for_row(index, row);
                    }
                    if self.mouse_press_view.is_none() && !self.has_held_mouse() {
                        self.scroll_selection(SCROLL_MARGIN);
                    } else {
                        self.keep_selection_visible();
                    }
                }
            }
        }
        self.advance_navigation();
    }

    fn current_snapshot(&self) -> Option<Arc<CachedSnapshot>> {
        if self.filter.is_empty() {
            self.order.snapshot(self.directory)
        } else {
            self.order.filtered_snapshot(self.directory)
        }
    }

    fn update_filter(&mut self) {
        self.track_selection();
        let visual = self.visual.active();
        self.clear_visual_intent();
        self.visual.reset(self.directory);
        if visual {
            self.visual.enter(self.directory, None);
        }
        self.pending_navigation = None;
        self.prioritize_requested |= self.selected.take().is_some();
        self.focus_after_selection = true;
        self.offset = 0;
        self.view = Arc::clone(&self.empty);
        self.painted_rows.clear();
        self.painted_view = Arc::clone(&self.empty);
        if self.filter.is_empty() {
            self.order.clear_filter();
        } else {
            self.order.set_filter(self.directory, &self.filter, false);
        }
        self.navigation_until = Instant::now();
        self.sync_order(true);
    }

    fn eligible(&self, id: NodeId) -> bool {
        self.model.is_live(id)
            && (self.config.show_hidden
                || !self
                    .model
                    .node(id)
                    .name
                    .as_encoded_bytes()
                    .starts_with(b"."))
    }

    fn selection_index(&self) -> Option<usize> {
        self.selected
            .and_then(|id| self.view.positions.get(&id).copied())
    }
    fn screen_rows(&self) -> impl Iterator<Item = (usize, NodeId)> + '_ {
        self.rows_in(&self.view, self.offset)
    }

    fn rows_in<'a>(
        &'a self,
        view: &'a CachedSnapshot,
        offset: usize,
    ) -> impl Iterator<Item = (usize, NodeId)> + 'a {
        view.ids
            .iter()
            .copied()
            .enumerate()
            .skip(offset)
            .take(VIEW_BUDGET)
            .filter(|(_, id)| self.eligible(*id))
            .take(self.page_size)
    }

    fn selection_screen_row(&self) -> Option<usize> {
        self.screen_rows()
            .position(|(_, id)| self.selected == Some(id))
    }

    fn offset_for_row(&self, index: usize, row: usize) -> usize {
        let mut offset = index;
        let mut preceding = 0;
        for next in (index.saturating_sub(VIEW_BUDGET - 1)..index).rev() {
            if preceding >= row {
                break;
            }
            offset = next;
            if self.eligible(self.view.ids[next]) {
                preceding += 1;
            }
        }
        offset
    }

    fn keep_selection_visible(&mut self) {
        self.scroll_selection(0);
    }

    fn scroll_selection(&mut self, margin: usize) {
        let Some(index) = self.selection_index() else {
            self.offset = 0;
            return;
        };
        let margin = margin.min(self.page_size.saturating_sub(1) / 2);
        // Bound successor work and retain its raw extent: hidden or deleted
        // slots can otherwise exhaust the painted window before the context.
        let (following, last) = self
            .rows_in(&self.view, index)
            .skip(1)
            .take(margin)
            .fold((0, index), |(count, _), (next, _)| (count + 1, next));
        let row = self.selection_screen_row();
        if index < self.offset || row.is_some_and(|row| row < margin) {
            self.offset = self.offset_for_row(index, margin);
        } else if row.is_none_or(|row| row >= self.page_size.saturating_sub(margin)) {
            let bottom = self.page_size.saturating_sub(following + 1);
            if row.is_none_or(|row| row > bottom) {
                self.offset = self.offset_for_row(index, bottom);
            }
        }
        self.offset = self.offset.max(last.saturating_sub(VIEW_BUDGET - 1));
    }

    fn track_selection(&mut self) {
        if self.selected.is_some() {
            self.cursor_mode = CursorMode::Tracked;
        } else if self.cursor_mode == CursorMode::Top {
            self.cursor_mode = CursorMode::Waiting;
        }
    }

    fn edge_selection(&mut self, end: bool) {
        self.track_selection();
        self.pending_navigation = Some(Navigation::new(Some(end), 0));
        self.advance_navigation();
    }

    fn move_selection(&mut self, amount: isize) {
        self.track_selection();
        let mut navigation = self
            .pending_navigation
            .take()
            .unwrap_or_else(|| Navigation::new(None, 0));
        navigation.amount = navigation.amount.saturating_add(amount);
        // Recompose from the committed selection, never from a partially scanned
        // hidden span. Opposite keys therefore cancel without moving the cursor.
        navigation.cursor = None;
        if navigation.end.is_some() || navigation.amount != 0 {
            self.pending_navigation = Some(navigation);
            self.advance_navigation();
        }
    }

    fn advance_navigation(&mut self) {
        let Some(mut navigation) = self.pending_navigation.take() else {
            return;
        };
        let has_children = self
            .model
            .has_children(self.directory, false, self.config.show_hidden);
        let empty_filter = !self.filter.is_empty()
            && !Arc::ptr_eq(&self.view, &self.empty)
            && self.view.ids.is_empty();
        if !has_children
            && !empty_filter
            && self.cursor_mode == CursorMode::Waiting
            && (navigation.amount != 0 || navigation.end == Some(true))
            && !self.model.finished
            && !self.model.listing_complete(self.directory)
        {
            self.pending_navigation = Some(navigation);
            return;
        }
        if !has_children || empty_filter {
            let cleared = self.selected.take().is_some();
            self.prioritize_requested |= cleared;
            self.focus_after_selection |= cleared;
            self.offset = 0;
            self.range_requested = false;
            self.bind_visual_cursor(None);
            return;
        }
        if navigation.end.is_some() && self.view.generation != self.order.generation() {
            self.pending_navigation = Some(navigation);
            return;
        }
        if navigation.end.is_none()
            && self.selected.is_some_and(|id| self.eligible(id))
            && self.selection_index().is_none()
        {
            self.pending_navigation = Some(navigation);
            return;
        }
        if navigation
            .cursor
            .is_some_and(|cursor| cursor.target.is_some_and(|id| !self.eligible(id)))
        {
            navigation.cursor = None;
        }
        let backwards = navigation.amount < 0;
        let mut cursor = navigation.cursor.unwrap_or_else(|| {
            let edge = navigation.end;
            let selected = self.selected.filter(|&id| self.eligible(id));
            let baseline = edge.is_some() || selected.is_none();
            let next = if baseline {
                if edge == Some(true) {
                    self.view.ids.len().checked_sub(1)
                } else {
                    (!self.view.ids.is_empty()).then_some(0)
                }
            } else {
                self.selection_index()
                    .and_then(|index| self.navigation_next(index, backwards))
            };
            NavigationCursor {
                next,
                remaining: navigation.amount.unsigned_abs(),
                target: if baseline { None } else { selected },
                baseline,
            }
        });
        let budget = navigation
            .amount
            .unsigned_abs()
            .saturating_add(128)
            .clamp(128, VIEW_BUDGET);
        let mut complete = false;
        for _ in 0..budget {
            let Some(index) = cursor.next else {
                complete = cursor.target.is_some();
                break;
            };
            let direction = if cursor.baseline {
                navigation.end == Some(true)
            } else {
                backwards
            };
            cursor.next = self.navigation_next(index, direction);
            let id = self.view.ids[index];
            if !self.eligible(id) {
                continue;
            }
            cursor.target = Some(id);
            if cursor.baseline {
                cursor.baseline = false;
                cursor.next = self.navigation_next(index, backwards);
            } else {
                cursor.remaining = cursor.remaining.saturating_sub(1);
            }
            if cursor.remaining == 0 {
                complete = true;
                break;
            }
        }
        if complete {
            let previous_selected = self.selected;
            self.selected = cursor.target;
            if self.cursor_mode == CursorMode::Waiting {
                self.cursor_mode = CursorMode::Tracked;
            }
            self.bind_visual_cursor(self.selected);
            if std::mem::take(&mut self.focus_after_selection) && previous_selected != self.selected
            {
                self.prioritize_requested = true;
            }
            if self.range_requested {
                if let Some(id) = self.selected {
                    self.visual
                        .begin_range(Arc::clone(&self.view), id, RangeMode::Add);
                }
                self.range_requested = false;
            } else if self.visual.active() && previous_selected != self.selected {
                if let Some(id) = self.selected {
                    self.visual.reanchor(id);
                }
            }
            if let Some(end) = navigation.end {
                self.offset = if end {
                    self.selection_index().map_or(0, |index| {
                        self.offset_for_row(index, self.page_size.saturating_sub(1))
                    })
                } else {
                    0
                };
            }
            self.scroll_selection(SCROLL_MARGIN);
        } else {
            navigation.cursor = Some(cursor);
            self.pending_navigation = Some(navigation);
        }
    }

    fn navigation_next(&self, index: usize, backwards: bool) -> Option<usize> {
        if backwards {
            index.checked_sub(1)
        } else {
            (index + 1 < self.view.ids.len()).then_some(index + 1)
        }
    }

    fn sort(&mut self, mode: SortMode) {
        if mode == self.config.sort_mode {
            self.config.descending = !self.config.descending;
        } else {
            self.config.sort_mode = mode;
            self.config.descending = mode != SortMode::Name;
        }
        self.reset_order(true);
    }
    fn key(&mut self, key: KeyEvent, anchor: Option<Arc<RootAnchor>>, deleting: bool) -> Action {
        let interrupt =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        // Ctrl-C remains an escape hatch before a dialog has been painted and
        // in terminals too small to display ordinary confirmation choices.
        if interrupt && matches!(self.modal, Some(Modal::Quit { .. } | Modal::Stopping)) {
            return Action::Quit;
        }
        if matches!(self.modal, Some(Modal::Stopping)) {
            return Action::Continue;
        }
        if interrupt
            || (key.code == KeyCode::Char('q')
                && deleting
                && (!self.filter_editing || self.modal.is_some()))
        {
            if let Some(Modal::Quit { reviewable }) = self.modal.as_ref() {
                return if *reviewable {
                    Action::Quit
                } else {
                    Action::Continue
                };
            }
            if deleting {
                self.previous_modal = self.modal.take();
                self.modal = Some(Modal::Quit { reviewable: false });
                return Action::QuitWarning;
            }
            return Action::Quit;
        }
        if let Some(modal) = self.modal.take() {
            match modal {
                Modal::Queue { selected, offset } => {
                    self.modal = Some(Modal::Queue { selected, offset });
                    return match key.code {
                        KeyCode::Esc | KeyCode::Char('D') => {
                            self.modal = None;
                            Action::Continue
                        }
                        KeyCode::Char('p') | KeyCode::Char(' ') => Action::QueuePause,
                        KeyCode::Char('c') => Action::QueueCancel,
                        KeyCode::Char('d') => Action::QueueDelete,
                        KeyCode::Char('x') | KeyCode::Delete => Action::QueueRemove,
                        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            Action::QueueReorder(-1)
                        }
                        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            Action::QueueReorder(1)
                        }
                        KeyCode::Char('K') => Action::QueueReorder(-1),
                        KeyCode::Char('J') => Action::QueueReorder(1),
                        KeyCode::Up | KeyCode::Char('k') => Action::QueueNavigate {
                            end: None,
                            amount: -1,
                        },
                        KeyCode::Down | KeyCode::Char('j') => Action::QueueNavigate {
                            end: None,
                            amount: 1,
                        },
                        KeyCode::Home => Action::QueueNavigate {
                            end: Some(false),
                            amount: 0,
                        },
                        KeyCode::End => Action::QueueNavigate {
                            end: Some(true),
                            amount: 0,
                        },
                        KeyCode::PageUp => Action::QueueNavigate {
                            end: None,
                            amount: -8,
                        },
                        KeyCode::PageDown => Action::QueueNavigate {
                            end: None,
                            amount: 8,
                        },
                        KeyCode::Char('q') => {
                            self.modal = None;
                            Action::Continue
                        }
                        _ => Action::Continue,
                    };
                }
                Modal::Quit { reviewable } => match key.code {
                    KeyCode::Enter | KeyCode::Char('q') if reviewable => return Action::Quit,
                    KeyCode::Esc | KeyCode::Char('n') => self.modal = self.previous_modal.take(),
                    _ => self.modal = Some(Modal::Quit { reviewable }),
                },
                Modal::Stopping => self.modal = Some(Modal::Stopping),
                Modal::Enqueueing => match key.code {
                    KeyCode::Char('n') | KeyCode::Esc => return Action::BatchCancel,
                    _ => self.modal = Some(Modal::Enqueueing),
                },
                Modal::Batch {
                    target,
                    count,
                    ready,
                    reviewable,
                    mut scroll,
                } => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter if ready && reviewable && count > 0 => {
                        // Block later confirmations before returning to the event loop.
                        self.clear_visual_intent();
                        self.visual.exit();
                        self.modal = Some(Modal::Enqueueing);
                        return Action::BatchConfirm;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => return Action::BatchCancel,
                    other => {
                        scroll_key(other, &mut scroll);
                        self.modal = Some(Modal::Batch {
                            target,
                            count,
                            ready,
                            reviewable,
                            scroll,
                        })
                    }
                },
                Modal::Delete {
                    id,
                    anchor,
                    relative,
                    target,
                    mut scroll,
                    reviewable,
                    confirmed,
                    capture,
                } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter
                            if reviewable && confirmed.is_some() =>
                        {
                            return Action::Delete(id, anchor, relative, confirmed.unwrap());
                        }
                        KeyCode::Char('n') | KeyCode::Esc => return Action::Continue,
                        other => scroll_key(other, &mut scroll),
                    }
                    self.modal = Some(Modal::Delete {
                        id,
                        anchor,
                        relative,
                        target,
                        scroll,
                        reviewable,
                        confirmed,
                        capture,
                    });
                }
                Modal::Text {
                    title,
                    text,
                    mut scroll,
                } => match key.code {
                    KeyCode::Enter
                        if matches!(
                            title,
                            "Selecting entries" | "Preparing selection" | "Stopping selection"
                        ) =>
                    {
                        self.modal = Some(Modal::Text {
                            title,
                            text,
                            scroll,
                        });
                    }
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')
                        if matches!(
                            title,
                            "Stopping selection"
                                | "Preparing selection"
                                | "Selecting entries"
                                | "Stopping target"
                        ) =>
                    {
                        if title == "Stopping target" {
                            self.deferred_delete = None;
                        } else {
                            self.cancel_deferred_batch();
                        }
                    }
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {}
                    KeyCode::Char('?') if title != "Help" => {
                        self.modal = Some(Modal::help(self.visual.active()))
                    }
                    KeyCode::Char('?') => {}
                    other => {
                        scroll_key(other, &mut scroll);
                        self.modal = Some(Modal::Text {
                            title,
                            text,
                            scroll,
                        });
                    }
                },
            }
            return Action::Continue;
        }
        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Enter
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Char(
                    'j' | 'k' | 'h' | 'l' | '<' | 'v' | 'V' | '/' | 'e' | 'd' | 'r' | 'R' | 'i'
                )
        ) {
            self.track_selection();
        }
        if self.filter_editing {
            let changed = match key.code {
                KeyCode::Esc => {
                    self.filter_editing = false;
                    let changed = !self.filter.is_empty();
                    self.filter.clear();
                    changed
                }
                KeyCode::Enter => {
                    self.filter_editing = false;
                    false
                }
                KeyCode::Backspace => self.filter.pop().is_some(),
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let changed = !self.filter.is_empty();
                    self.filter.clear();
                    changed
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control()
                        && self.filter.len() + c.len_utf8() <= 1024 =>
                {
                    self.filter.push(c);
                    true
                }
                _ => false,
            };
            if changed {
                self.update_filter();
            }
            return Action::Continue;
        }
        if key.code == KeyCode::Char('/') {
            self.finish_mouse_for_input();
            self.filter_editing = true;
            return Action::Continue;
        }
        if key.code == KeyCode::Esc && !self.filter.is_empty() {
            self.filter.clear();
            self.update_filter();
            return Action::Continue;
        }
        let moving = matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char('j')
                | KeyCode::Char('k')
        );
        if self.visual.active() && (moving || key.code == KeyCode::Enter) {
            self.finish_mouse_for_input();
            if self.visual_busy() {
                if moving {
                    let (end, amount) = match key.code {
                        KeyCode::Home => (Some(false), 0),
                        KeyCode::End => (Some(true), 0),
                        KeyCode::PageUp => (None, -(self.page_size as isize)),
                        KeyCode::PageDown => (None, self.page_size as isize),
                        KeyCode::Up | KeyCode::Char('k') => (None, -1),
                        _ => (None, 1),
                    };
                    self.enqueue_visual(VisualIntent::Navigate(
                        Navigation::new(end, amount),
                        key.modifiers.contains(KeyModifiers::SHIFT),
                    ));
                } else {
                    let future_cursor = self.cursor_pending() || self.range_requested;
                    if future_cursor {
                        self.enqueue_visual(VisualIntent::ToggleCurrent);
                    } else if let Some(id) = self.selected {
                        self.enqueue_visual(VisualIntent::ToggleStable(id));
                    }
                }
                return Action::Continue;
            }
        }
        if self.visual.active() && matches!(key.code, KeyCode::Char('d' | 'D') | KeyCode::Delete) {
            self.finish_mouse_for_input();
            return Action::BatchPrepare;
        }
        if self.pending_navigation.is_some() && !moving {
            self.sync_order(true);
        }
        if moving {
            self.prioritize_requested = true;
            self.range_requested =
                self.visual.active() && key.modifiers.contains(KeyModifiers::SHIFT);
        }
        if (self.cursor_pending()
            || self
                .selected
                .is_some_and(|id| !self.eligible(id) || self.selection_index().is_none()))
            && matches!(
                key.code,
                KeyCode::Enter
                    | KeyCode::Right
                    | KeyCode::Char('l')
                    | KeyCode::Char('d')
                    | KeyCode::Char('r')
                    | KeyCode::Char('i')
            )
        {
            return Action::Continue;
        }
        if moving {
            self.navigation_until = Instant::now() + Duration::from_millis(150);
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('v' | 'V') => {
                self.clear_visual_intent();
                if self.visual.active() {
                    self.visual.exit();
                } else {
                    let pending = self.pending_navigation.is_some();
                    self.visual
                        .enter(self.directory, self.selected.filter(|_| !pending));
                    self.range_requested = pending;
                }
            }
            KeyCode::Esc if self.visual.active() => {
                self.clear_visual_intent();
                self.visual.exit();
            }
            KeyCode::Char('D') => {
                self.modal = Some(Modal::Queue {
                    selected: None,
                    offset: None,
                })
            }
            KeyCode::Char('?') => self.modal = Some(Modal::help(self.visual.active())),
            KeyCode::Up | KeyCode::Down
                if !self.visual.active() && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                let direction = if key.code == KeyCode::Up { -1 } else { 1 };
                self.move_selection(direction * self.page_size as isize);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home => self.edge_selection(false),
            KeyCode::End => self.edge_selection(true),
            KeyCode::PageUp => self.move_selection(-(self.page_size as isize)),
            KeyCode::PageDown => self.move_selection(self.page_size as isize),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Char('<') => {
                if let Some(parent) = self.model.node(self.directory).parent {
                    self.selected = Some(self.directory);
                    self.directory = parent;
                    self.clear_visual_intent();
                    self.visual.reset(parent);
                    self.prioritize_requested = true;
                    self.reset_order(false);
                }
            }
            KeyCode::Enter if self.visual.active() => {
                if let Some(id) = self.selected {
                    self.visual.toggle(id);
                    self.range_requested = false;
                }
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(id) = self.selected {
                    if self.eligible(id) && self.model.node(id).kind == NodeKind::Directory {
                        self.directory = id;
                        self.selected = None;
                        self.focus_after_selection = true;
                        self.clear_visual_intent();
                        self.visual.reset(id);
                        self.prioritize_requested = true;
                        self.reset_order(false);
                    }
                }
            }
            KeyCode::Char('n') => self.sort(SortMode::Name),
            KeyCode::Char('s') => self.sort(SortMode::Size),
            KeyCode::Char('C') => self.sort(SortMode::Items),
            KeyCode::Char('a') => {
                self.config.size_mode = match self.config.size_mode {
                    SizeMode::Allocated => SizeMode::Apparent,
                    SizeMode::Apparent => SizeMode::Allocated,
                };
                self.reset_order(true);
            }
            KeyCode::Char('t') => {
                self.config.directories_first = !self.config.directories_first;
                self.reset_order(true);
            }
            KeyCode::Char('e') => {
                self.clear_visual_intent();
                self.visual.reset(self.directory);
                self.config.show_hidden = !self.config.show_hidden;
                if self.selected.is_some_and(|id| !self.eligible(id)) {
                    self.selected = None;
                    self.focus_after_selection = true;
                    self.prioritize_requested = true;
                }
                self.reset_order(true);
            }
            KeyCode::Char('g') => {
                self.graph = match self.graph {
                    Graph::Bar => Graph::Percent,
                    Graph::Percent => Graph::Off,
                    Graph::Off => Graph::Bar,
                }
            }
            KeyCode::Char('c') => self.counts = !self.counts,
            KeyCode::Char('i') => self.modal = Some(Modal::info(self.info())),
            KeyCode::Char('r') => {
                if let Some(id) = self.selected.filter(|&id| {
                    self.model.is_live(id) && self.model.node(id).kind == NodeKind::Directory
                }) {
                    return Action::Refresh(id);
                }
            }
            KeyCode::Char('R') => return Action::Refresh(self.directory),
            KeyCode::Char('d') => {
                if let (Some(id), Some(anchor)) = (self.selected, anchor) {
                    let path = self.model.path(id);
                    match path.strip_prefix(&self.root) {
                        Ok(relative) if !relative.as_os_str().is_empty() => {
                            let request = DeleteRequest {
                                id,
                                anchor,
                                relative: relative.to_owned(),
                                target: printable(path.as_os_str()),
                            };
                            if self
                                .capture_inflight
                                .as_ref()
                                .is_some_and(|finished| !finished.load(Ordering::Acquire))
                            {
                                self.deferred_delete = Some(request);
                                self.modal = Some(Modal::notice(
                                    "Stopping target",
                                    "Waiting for the previous target check to stop.".to_owned(),
                                ));
                            } else {
                                self.open_delete(request);
                            }
                        }
                        _ => {
                            self.modal = Some(Modal::notice(
                                "Cannot delete",
                                "The scan root cannot be deleted.".to_owned(),
                            ))
                        }
                    }
                } else {
                    self.modal = Some(Modal::notice(
                        "Cannot delete",
                        "Select an entry after the scan root is ready.".to_owned(),
                    ));
                }
            }
            _ => {}
        }
        Action::Continue
    }

    fn sync_queue(&mut self, queue: &DeleteQueue) {
        self.queue_view.active = queue.active().is_some();
        self.queue_view.cancelling = queue.is_cancelling();
        self.queue_view.pending = queue.pending().len();
        self.queue_view.paused = queue.is_paused();
        self.queue_view.status = if queue.is_updating() {
            "Updating queue".to_owned()
        } else if let Some(progress) = queue.progress() {
            format!(
                "{} · {} removed · {} queued",
                if queue.is_cancelling() {
                    "Cancelling"
                } else {
                    "Deleting"
                },
                progress.removed_entries,
                queue.pending().len()
            )
        } else if !queue.pending().is_empty() {
            format!(
                "{} queued{}",
                queue.pending().len(),
                if queue.is_paused() { " · Paused" } else { "" }
            )
        } else {
            String::new()
        };
        let Some(Modal::Queue { selected, offset }) = self.modal.as_mut() else {
            self.queue_view.rows.clear();
            return;
        };
        if selected.is_none_or(|id| queue.entry(id).is_none()) {
            *selected = queue.first_id();
        }
        if offset.is_none_or(|id| queue.entry(id).is_none()) {
            *offset = *selected;
        }
        let visible = self.queue_view.visible_rows.clamp(1, 12);
        if queue.pending().len() + usize::from(queue.active().is_some()) <= visible {
            *offset = queue.first_id();
        }
        let mut probe = *offset;
        let mut found = false;
        for _ in 0..visible {
            if probe == *selected {
                found = true;
                break;
            }
            probe = probe.and_then(|id| queue.next_id(id));
        }
        if !found {
            *offset = *selected;
        }
        self.queue_view.rows.clear();
        self.queue_view.selected = *selected;
        let mut cursor = *offset;
        for _ in 0..visible {
            let Some(id) = cursor else {
                break;
            };
            let Some(entry) = queue.entry(id) else {
                break;
            };
            self.queue_view.rows.push((
                id,
                queue.active().is_some_and(|active| active.id == id),
                printable(entry.relative.as_os_str()),
            ));
            cursor = queue.next_id(id);
        }
    }

    fn queue_navigate(&mut self, queue: &DeleteQueue, end: Option<bool>, amount: isize) {
        if let Some(Modal::Queue { selected, .. }) = self.modal.as_mut() {
            match end {
                Some(false) => *selected = queue.first_id(),
                Some(true) => *selected = queue.last_id(),
                None => {
                    for _ in 0..amount.unsigned_abs().min(VIEW_BUDGET) {
                        let next = selected.and_then(|id| {
                            if amount < 0 {
                                queue.previous_id(id)
                            } else {
                                queue.next_id(id)
                            }
                        });
                        if next.is_none() {
                            break;
                        }
                        *selected = next;
                    }
                }
            }
        }
    }

    fn cancel_deferred_batch(&mut self) {
        self.waiting_batch = None;
        for modal in [&mut self.modal, &mut self.previous_modal] {
            if matches!(
                modal,
                Some(Modal::Text {
                    title: "Selecting entries",
                    ..
                })
            ) {
                *modal = None;
            }
        }
        if let Some(request) = self.deferred_batch.take() {
            self.visual
                .retire_snapshot_and_order(request.selection, request.order);
        }
    }

    fn open_delete(&mut self, request: DeleteRequest) {
        if matches!(
            self.modal,
            Some(Modal::Text {
                title: "Stopping target",
                ..
            })
        ) {
            self.modal = None;
        }
        if !self.model.is_live(request.id)
            || self.model.path(request.id) != self.root.join(&request.relative)
        {
            return;
        }
        let (capture, finished) =
            spawn_capture(Arc::clone(&request.anchor), request.relative.clone());
        self.capture_inflight = Some(finished);
        self.modal = Some(Modal::Delete {
            id: request.id,
            anchor: request.anchor,
            relative: request.relative,
            target: request.target,
            scroll: 0,
            reviewable: false,
            confirmed: None,
            capture: Some(capture),
        });
    }

    fn retire_mouse_press(&mut self) {
        let selected = self.mouse_baseline.take().unwrap_or_default();
        if let Some(view) = self.mouse_press_view.take() {
            self.visual.retire_snapshot_and_order(selected, view);
        } else if !selected.is_empty() {
            self.visual.retire_snapshot(selected);
        }
    }

    fn pump_confirmation(&mut self) {
        if self
            .capture_inflight
            .as_ref()
            .is_some_and(|finished| finished.load(Ordering::Acquire))
        {
            self.capture_inflight = None;
            if matches!(
                self.modal,
                Some(Modal::Text {
                    title: "Stopping target",
                    ..
                })
            ) {
                self.modal = None;
            }
        }
        if self.capture_inflight.is_none()
            && (self.modal.is_none()
                || matches!(
                    self.modal,
                    Some(Modal::Text {
                        title: "Stopping target",
                        ..
                    })
                ))
        {
            if let Some(request) = self.deferred_delete.take() {
                self.open_delete(request);
            }
        }
        let mut error = None;
        let modal = if matches!(self.modal, Some(Modal::Quit { .. })) {
            self.previous_modal.as_mut()
        } else {
            self.modal.as_mut()
        };
        if let Some(Modal::Delete {
            confirmed, capture, ..
        }) = modal
        {
            if let Some(receiver) = capture.as_ref() {
                match receiver.try_recv() {
                    Ok(Ok(target)) => {
                        *confirmed = Some(target);
                        *capture = None;
                    }
                    Ok(Err(failure)) => error = Some(failure.to_string()),
                    Err(TryRecvError::Disconnected) => {
                        error = Some("Could not check deletion target.".to_owned())
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if let Some(error) = error {
            let notice = Modal::notice("Cannot delete", printable(OsStr::new(&error)));
            if matches!(self.modal, Some(Modal::Quit { .. })) {
                self.previous_modal = Some(notice);
            } else {
                self.modal = Some(notice);
            }
        }
    }

    fn visual_busy(&self) -> bool {
        self.visual.is_pending()
            || self.pending_navigation.is_some()
            || self.range_requested
            || !self.visual_intents.is_empty()
            || self.mouse_dragging
    }

    fn visual_input_available(&self) -> bool {
        self.modal.is_some() || self.visual_intents.len() < VISUAL_INTENT_LIMIT
    }

    fn cursor_pending(&self) -> bool {
        self.pending_navigation.is_some()
            || self.visual_intents.iter().any(|intent| {
                matches!(
                    intent,
                    VisualIntent::Navigate(..) | VisualIntent::CursorId(_) | VisualIntent::Mouse(_)
                )
            })
    }

    fn enqueue_visual(&mut self, intent: VisualIntent) {
        match (self.visual_intents.back_mut(), &intent) {
            (Some(VisualIntent::CursorId(previous)), VisualIntent::CursorId(id)) => {
                *previous = *id;
                return;
            }
            (
                Some(VisualIntent::Navigate(previous, extend)),
                VisualIntent::Navigate(next, next_extend),
            ) if *extend == *next_extend
                && previous.end.is_none()
                && next.end.is_none()
                && previous.amount.signum() == next.amount.signum() =>
            {
                previous.amount = previous.amount.saturating_add(next.amount);
                return;
            }
            _ => {}
        }
        // Keep reading terminal events so interrupts and mouse release remain
        // reachable. Excess ordinary intents are rejected after coalescing.
        if self.visual_intents.len() >= VISUAL_INTENT_LIMIT {
            if let VisualIntent::Mouse(gesture) = intent {
                self.visual
                    .retire_snapshot_and_order(im::HashSet::new(), gesture.view);
            }
            return;
        }
        self.visual_intents.push_back(intent);
    }

    fn bind_visual_cursor(&mut self, id: Option<NodeId>) {
        let Some(id) = id else {
            while matches!(
                self.visual_intents.front(),
                Some(VisualIntent::ToggleCurrent)
            ) {
                self.visual_intents.pop_front();
            }
            return;
        };
        for intent in &mut self.visual_intents {
            match intent {
                VisualIntent::ToggleCurrent => *intent = VisualIntent::ToggleStable(id),
                VisualIntent::ToggleStable(_) => {}
                _ => break,
            }
        }
    }

    fn has_held_mouse(&self) -> bool {
        self.visual_intents.iter().any(|intent| {
            matches!(intent,
            VisualIntent::Mouse(gesture) if !gesture.released)
        })
    }

    fn held_mouse(&mut self) -> Option<&mut PendingMouse> {
        self.visual_intents
            .iter_mut()
            .rev()
            .find_map(|intent| match intent {
                VisualIntent::Mouse(gesture) if !gesture.released => Some(gesture),
                _ => None,
            })
    }

    fn focus_mouse_lead(&mut self, view: &CachedSnapshot, anchor: NodeId, lead: NodeId) {
        let live = |id| self.eligible(id) && self.model.node(id).parent == Some(self.directory);
        // Keep deleted endpoints as frozen range positions, and repair focus
        // separately. Any longer search continues through bounded navigation.
        let cursor = if live(lead) {
            Some(lead)
        } else {
            view.positions.get(&lead).and_then(|&index| {
                let anchor = view.positions.get(&anchor).copied().unwrap_or(index);
                (0..256)
                    .filter_map(|step| {
                        if anchor <= index {
                            index.checked_sub(step).filter(|&next| next >= anchor)
                        } else {
                            index.checked_add(step).filter(|&next| next <= anchor)
                        }
                    })
                    .map(|next| view.ids[next])
                    .find(|&id| live(id))
            })
        }
        .or_else(|| live(anchor).then_some(anchor))
        .or_else(|| {
            self.selected
                .filter(|&id| live(id) && view.positions.contains_key(&id))
        });
        self.selected = cursor;
        self.focus_after_selection |= cursor.is_none();
        self.prioritize_requested = true;
        if let Some(id) = cursor {
            self.bind_visual_cursor(Some(id));
            self.keep_selection_visible();
        } else {
            self.pending_navigation = Some(Navigation::new(Some(false), 0));
            self.advance_navigation();
        }
    }

    fn add_mouse_range(&mut self, view: Arc<CachedSnapshot>, id: NodeId) {
        self.visual.add_range(view, id);
    }

    fn finish_mouse_for_input(&mut self) {
        if let Some(gesture) = self.held_mouse() {
            gesture.released = true;
        }
        if self.mouse_dragging && !self.drag_started && self.drag_mode == RangeMode::Invert {
            if let Some(id) = self.drag_anchor {
                let view = self
                    .mouse_press_view
                    .as_ref()
                    .map_or_else(|| Arc::clone(&self.painted_view), Arc::clone);
                self.add_mouse_range(view, id);
            }
        }
        self.retire_mouse_press();
        self.mouse_dragging = false;
        self.drag_started = false;
        self.drag_anchor = None;
        self.drag_scroll = None;
        self.mouse_position = None;
    }

    fn set_drag_edge(&mut self, edge: isize, id: NodeId) {
        let previous = self
            .drag_scroll
            .filter(|(direction, _, _)| *direction == edge);
        let deadline = previous.map_or_else(
            || Instant::now() + Duration::from_millis(50),
            |(_, _, time)| time,
        );
        self.drag_scroll = self.mouse_press_view.as_ref().and_then(|view| {
            view.positions
                .get(&id)
                .copied()
                .filter(|_| edge != 0)
                .map(|index| {
                    let index = previous.map_or(index, |(_, old, _)| {
                        if edge < 0 {
                            old.min(index)
                        } else {
                            old.max(index)
                        }
                    });
                    (edge, index, deadline)
                })
        });
    }

    fn pump_mouse_scroll(&mut self, now: Instant) -> bool {
        if !self.mouse_dragging || !self.drag_started || self.modal.is_some() {
            return false;
        }
        let Some((direction, mut index, deadline)) = self.drag_scroll else {
            return false;
        };
        if now < deadline || self.visual.is_pending() {
            return false;
        }
        let Some(view) = self.mouse_press_view.as_ref().map(Arc::clone) else {
            return false;
        };
        let mut lead = None;
        let mut moved = 0;
        for _ in 0..256 {
            let next = if direction < 0 {
                index.checked_sub(1)
            } else {
                index.checked_add(1).filter(|&next| next < view.ids.len())
            };
            let Some(next) = next else { break };
            index = next;
            let id = view.ids[index];
            if self.eligible(id) && self.model.node(id).parent == Some(self.directory) {
                lead = Some(id);
                moved += 1;
                if moved == 3 {
                    break;
                }
            }
        }
        self.drag_scroll = Some((direction, index, now + Duration::from_millis(50)));
        let Some(id) = lead else { return false };
        self.visual
            .begin_range(Arc::clone(&view), id, self.drag_mode);
        self.view = view;
        self.selected = Some(id);
        self.prioritize_requested = true;
        self.keep_selection_visible();
        true
    }

    fn cancel_held_mouse(&mut self) {
        if let Some(index) = self
            .visual_intents
            .iter()
            .rposition(|intent| matches!(intent, VisualIntent::Mouse(gesture) if !gesture.released))
        {
            if let Some(VisualIntent::Mouse(gesture)) = self.visual_intents.remove(index) {
                self.visual
                    .retire_snapshot_and_order(im::HashSet::new(), gesture.view);
            }
        }
    }

    fn clear_visual_intent(&mut self) {
        self.cancel_deferred_batch();
        self.range_requested = false;
        for intent in self.visual_intents.drain(..) {
            if let VisualIntent::Mouse(gesture) = intent {
                self.visual
                    .retire_snapshot_and_order(im::HashSet::new(), gesture.view);
            }
        }
        self.mouse_dragging = false;
        self.drag_started = false;
        self.drag_anchor = None;
        self.drag_scroll = None;
        self.mouse_position = None;
        self.retire_mouse_press();
    }

    fn pump_visual(&mut self) -> bool {
        let scrolled = self.pump_mouse_scroll(Instant::now());
        let model = &self.model;
        let directory = self.directory;
        let hidden = self.config.show_hidden;
        let mut changed = self.visual.pump_filtered(256, |id| {
            model.is_live(id)
                && model.node(id).parent == Some(directory)
                && (hidden || !model.node(id).name.as_encoded_bytes().starts_with(b"."))
        }) || scrolled;
        for _ in 0..VISUAL_INTENT_LIMIT {
            if self.visual.is_pending()
                || self.pending_navigation.is_some()
                || self.range_requested
                || self.mouse_dragging
            {
                break;
            }
            if matches!(self.visual_intents.front(), Some(VisualIntent::Mouse(gesture))
                if !gesture.released && ((gesture.mode == RangeMode::Invert && !gesture.dragged) || self.modal.is_some()))
            {
                break;
            }
            let Some(intent) = self.visual_intents.pop_front() else {
                break;
            };
            match intent {
                VisualIntent::Navigate(navigation, extend) => {
                    self.range_requested = extend;
                    self.prioritize_requested = true;
                    if let Some(end) = navigation.end {
                        self.edge_selection(end);
                    }
                    if navigation.amount != 0 {
                        self.move_selection(navigation.amount);
                    }
                }
                VisualIntent::ToggleCurrent => {
                    if let Some(id) = self.selected.filter(|&id| self.eligible(id)) {
                        self.visual.toggle(id);
                    }
                }
                VisualIntent::ToggleStable(id) => {
                    if self.eligible(id) && self.model.node(id).parent == Some(self.directory) {
                        self.visual.toggle(id);
                    }
                }
                VisualIntent::CursorId(id) => {
                    self.bind_visual_cursor(Some(id));
                    if self.eligible(id) && self.model.node(id).parent == Some(self.directory) {
                        if self.visual.active()
                            && self.selected != Some(id)
                            && self.mouse_press_view.is_none()
                        {
                            self.visual.reanchor(id);
                        }
                        self.selected = Some(id);
                        self.keep_selection_visible();
                        self.prioritize_requested = true;
                    }
                }
                VisualIntent::Mouse(gesture) => {
                    let valid_interval = gesture.view.child_epoch
                        == self.model.node(self.directory).child_epoch
                        && gesture.view.positions.contains_key(&gesture.anchor)
                        && gesture.view.positions.contains_key(&gesture.lead)
                        && self.model.node(gesture.anchor).parent == Some(self.directory)
                        && self.model.node(gesture.lead).parent == Some(self.directory);
                    if valid_interval
                        && (gesture.dragged
                            || gesture.mode == RangeMode::Invert
                            || self.eligible(gesture.lead))
                    {
                        if !gesture.released {
                            self.retire_mouse_press();
                            self.mouse_baseline = Some(self.visual.snapshot());
                            self.mouse_press_view = Some(Arc::clone(&gesture.view));
                            self.view = Arc::clone(&gesture.view);
                            self.offset = gesture.offset;
                            self.drag_mode = gesture.mode;
                            self.drag_anchor = Some(gesture.anchor);
                        }
                        if gesture.dragged {
                            self.visual.reanchor(gesture.anchor);
                            if !self.model.is_live(gesture.anchor) {
                                self.visual.remove(gesture.anchor);
                            }
                            self.visual.begin_range(
                                Arc::clone(&gesture.view),
                                gesture.lead,
                                gesture.mode,
                            );
                            self.drag_started = !gesture.released;
                            self.mouse_dragging = !gesture.released;
                            if !gesture.released {
                                self.set_drag_edge(gesture.edge, gesture.lead);
                            }
                        } else if gesture.mode == RangeMode::Add {
                            self.visual.toggle(gesture.lead);
                        } else {
                            self.add_mouse_range(Arc::clone(&gesture.view), gesture.lead);
                        }
                        self.focus_mouse_lead(&gesture.view, gesture.anchor, gesture.lead);
                    }
                    self.visual
                        .retire_snapshot_and_order(im::HashSet::new(), gesture.view);
                }
            }
            changed = true;
        }
        changed
    }

    fn scan_focus(&self) -> Option<NodeId> {
        self.selected
            .filter(|&id| {
                self.model.is_live(id)
                    && self.model.node(id).parent == Some(self.directory)
                    && self.model.node(id).kind == NodeKind::Directory
                    && !self.model.node(id).complete
            })
            .or_else(|| {
                (self.directory != ROOT
                    && self.model.is_live(self.directory)
                    && self.model.node(self.directory).kind == NodeKind::Directory
                    && !self.model.node(self.directory).complete)
                    .then_some(self.directory)
            })
    }

    fn begin_model_refresh(&mut self, id: NodeId, generation: u64) {
        let previous_directory = self.directory;
        self.focus_after_selection = true;
        self.prioritize_requested = true;
        let changed = self.model.begin_rescan(id, generation);
        self.changed(changed);
        if !self.model.is_live(self.directory) {
            self.directory = id;
        }
        if self
            .selected
            .is_some_and(|selected| !self.model.is_live(selected))
        {
            self.selected = None;
        }
        if self.directory != previous_directory
            || self.directory == id
            || self.visual.directory() == id
        {
            self.clear_visual_intent();
            self.visual.reset(self.directory);
        }
        if self.directory != previous_directory {
            self.reset_order(false);
        } else {
            self.sync_order(true);
        }
    }

    fn repair_directory(&mut self) {
        if self.visual.directory() != self.directory {
            self.clear_visual_intent();
            self.visual.reset(self.directory);
        }
        if self.model.is_live(self.directory)
            && self.model.node(self.directory).kind == NodeKind::Directory
        {
            return;
        }
        let previous = self.directory;
        while self.directory != ROOT
            && (!self.model.is_live(self.directory)
                || self.model.node(self.directory).kind != NodeKind::Directory)
        {
            self.directory = self.model.node(self.directory).parent.unwrap_or(ROOT);
        }
        self.selected = self.model.is_live(previous).then_some(previous);
        self.focus_after_selection |= self.selected.is_none();
        self.prioritize_requested = true;
        self.clear_visual_intent();
        self.visual.reset(self.directory);
        self.reset_order(false);
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if self.modal.is_some() {
                self.cancel_held_mouse();
                self.retire_mouse_press();
                self.mouse_dragging = false;
                self.drag_started = false;
                self.drag_anchor = None;
                self.drag_scroll = None;
                self.mouse_position = None;
            } else {
                if self
                    .mouse_position
                    .is_some_and(|position| position != (mouse.column, mouse.row))
                {
                    self.mouse(MouseEvent {
                        kind: MouseEventKind::Drag(MouseButton::Left),
                        ..mouse
                    });
                }
                self.finish_mouse_for_input();
            }
            return;
        }
        if self.modal.is_some() {
            return;
        }
        let dragging = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left))
            && (self.mouse_press_view.is_some() || self.has_held_mouse());
        if self.painted_directory != self.directory
            || self.painted_view.child_epoch != self.model.node(self.directory).child_epoch
            || self.painted_rows.is_empty()
            || (!dragging
                && !self
                    .list_area
                    .contains(ratatui::layout::Position::new(mouse.column, mouse.row)))
        {
            return;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.track_selection();
            self.finish_mouse_for_input();
            let amount = if mouse.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            };
            if self.visual.active() && self.visual_busy() {
                self.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, amount), false));
                return;
            }
            self.move_selection(amount);
            self.prioritize_requested = true;
            return;
        }
        let row = usize::from(mouse.row.saturating_sub(self.list_area.y));
        let row = if dragging {
            row.min(self.painted_rows.len() - 1)
        } else {
            row
        };
        let Some(id) =
            self.painted_rows.get(row).copied().filter(|&id| {
                self.eligible(id) && self.model.node(id).parent == Some(self.directory)
            })
        else {
            return;
        };
        let edge = if mouse.row <= self.list_area.y {
            -1
        } else if mouse.row >= self.list_area.bottom().saturating_sub(1) {
            1
        } else {
            0
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cursor_mode = CursorMode::Tracked;
                if self.mouse_press_view.is_some() || self.has_held_mouse() {
                    self.finish_mouse_for_input();
                }
                let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                if shift || self.visual.active() {
                    if !self.visual.active() {
                        let pending = self.pending_navigation.is_some();
                        self.visual
                            .enter(self.directory, self.selected.filter(|_| !pending));
                        self.range_requested = pending;
                    }
                    let mode = if shift {
                        RangeMode::Invert
                    } else {
                        RangeMode::Add
                    };
                    self.mouse_position = Some((mouse.column, mouse.row));
                    if self.visual_busy() {
                        self.enqueue_visual(VisualIntent::Mouse(PendingMouse {
                            anchor: id,
                            lead: id,
                            dragged: false,
                            released: false,
                            mode,
                            edge,
                            view: Arc::clone(&self.painted_view),
                            offset: self.painted_offset,
                        }));
                        return;
                    }
                    self.retire_mouse_press();
                    self.mouse_baseline = Some(self.visual.snapshot());
                    self.mouse_press_view = Some(Arc::clone(&self.painted_view));
                    self.view = Arc::clone(&self.painted_view);
                    self.offset = self.painted_offset;
                    self.mouse_dragging = shift;
                    self.drag_started = false;
                    self.drag_anchor = Some(id);
                    self.drag_mode = mode;
                    if !shift {
                        self.visual.toggle(id);
                    }
                }
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if dragging => {
                self.mouse_position = Some((mouse.column, mouse.row));
                let painted = Arc::clone(&self.painted_view);
                let painted_offset = self.painted_offset;
                if let Some(gesture) = self.held_mouse() {
                    gesture.lead = id;
                    gesture.dragged = true;
                    gesture.edge = edge;
                    let old = (!gesture.view.positions.contains_key(&id)).then(|| {
                        gesture.offset = painted_offset;
                        std::mem::replace(&mut gesture.view, painted)
                    });
                    if let Some(old) = old {
                        self.visual
                            .retire_snapshot_and_order(im::HashSet::new(), old);
                    }
                    return;
                }
                if !self.drag_started {
                    let anchor = self
                        .drag_anchor
                        .take()
                        .unwrap_or(self.selected.unwrap_or(id));
                    let baseline = self
                        .mouse_baseline
                        .take()
                        .unwrap_or_else(|| self.visual.snapshot());
                    self.visual.restore_snapshot(baseline, anchor);
                    if !self.model.is_live(anchor) {
                        self.visual.remove(anchor);
                    }
                    self.drag_started = true;
                    self.mouse_dragging = true;
                }
                let view = self
                    .mouse_press_view
                    .as_ref()
                    .filter(|view| view.positions.contains_key(&id))
                    .map_or_else(|| Arc::clone(&self.painted_view), Arc::clone);
                if let Some(previous) = self.mouse_press_view.replace(Arc::clone(&view)) {
                    self.visual
                        .retire_snapshot_and_order(im::HashSet::new(), previous);
                }
                self.visual
                    .begin_range(Arc::clone(&view), id, self.drag_mode);
                self.view = view;
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
                self.keep_selection_visible();
                self.set_drag_edge(edge, id);
            }
            MouseEventKind::Moved if !self.mouse_dragging && !self.has_held_mouse() => {
                self.cursor_mode = CursorMode::Tracked;
                if self.visual.active() && self.visual_busy() {
                    self.enqueue_visual(VisualIntent::CursorId(id));
                    return;
                }
                if self.visual.active()
                    && self.selected != Some(id)
                    && self.mouse_press_view.is_none()
                {
                    self.visual.reanchor(id);
                }
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
            }
            _ => {}
        }
    }

    fn queue_selection(&self) -> Option<QueueId> {
        match self.modal.as_ref() {
            Some(Modal::Queue { selected, .. }) => *selected,
            _ => None,
        }
    }

    fn size(&self, id: NodeId) -> u64 {
        let sizes = self.model.node(id).sizes;
        match self.config.size_mode {
            SizeMode::Allocated => sizes.allocated,
            SizeMode::Apparent => sizes.apparent,
        }
    }
    fn phase(&self, refreshing: bool, deleting: bool) -> &'static str {
        if deleting {
            "Deleting…"
        } else if refreshing {
            "Refreshing…"
        } else if !self.started {
            "Waiting to scan"
        } else if self.model.node(ROOT).complete
            && self.model.has_partial_totals(ROOT)
            && self.model.node(ROOT).error_count == 0
            && self.model.last_error.is_none()
        {
            "Partial totals"
        } else if self.model.cancelled {
            "Scan cancelled"
        } else if self.model.has_pending_removals() {
            "Updating sizes"
        } else if self.model.finished {
            if self.model.node(ROOT).error_count > 0 || self.model.last_error.is_some() {
                "Scan complete with errors"
            } else {
                "Scan complete"
            }
        } else {
            "Scanning"
        }
    }
    fn info(&self) -> String {
        let id = self.selected.unwrap_or(self.directory);
        let node = self.model.node(id);
        let mut text = format!(
            "{}\n\nAllocated: {}   Apparent: {}\n{:?} · {} items · {} errors\n{}",
            printable(self.model.path(id).as_os_str()),
            human_size(node.sizes.allocated),
            human_size(node.sizes.apparent),
            node.kind,
            node.items,
            node.error_count,
            if !self.model.has_partial_totals(id) {
                "Complete"
            } else {
                "Partial total"
            },
        );
        if node.hardlink_duplicate {
            text.push_str("\nHardlink already counted elsewhere.");
        }
        if let Some(error) = self.model.last_error.as_ref() {
            text.push_str("\n\nLatest scan error: ");
            text.push_str(&printable(OsStr::new(error)));
        }
        text
    }

    fn row(
        &self,
        id: NodeId,
        total: u64,
        width: u16,
        selected: bool,
        spinner: &'static str,
    ) -> Line<'static> {
        let node = self.model.node(id);
        let size = self.size(id);
        let size_label = if node.kind == NodeKind::Unknown {
            "?".into()
        } else {
            format!(
                "{}{}",
                if node.error_count > 0 || (node.complete && self.model.has_partial_totals(id)) {
                    "~"
                } else {
                    ""
                },
                human_size(size)
            )
        };
        let marker = if selected {
            "›"
        } else if self.visual.contains(id) {
            "✓"
        } else {
            " "
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(Color::White)),
            Span::raw(format!("{size_label:>11} ")),
        ];
        let graph_width = match width {
            45.. => 10,
            30.. => 5,
            _ => 0,
        };
        if graph_width > 0 {
            match self.graph {
                Graph::Bar => spans.push(Span::styled(
                    format!("{} ", fractional_bar(size, total, graph_width)),
                    Style::default().fg(Color::Cyan),
                )),
                Graph::Percent => spans.push(Span::raw(format!(
                    "{:5.1}% ",
                    if total == 0 {
                        0.0
                    } else {
                        size as f64 * 100.0 / total as f64
                    }
                ))),
                Graph::Off => {}
            }
        }
        if self.counts {
            spans.push(Span::raw(format!("{:>8} ", node.items)));
        }
        let mut name = printable(&node.name);
        if node.kind == NodeKind::Directory {
            name.push('/');
        }
        if node.error_count > 0 {
            name.push_str(" !");
        }
        let color = if selected {
            Color::White
        } else if node.error_count > 0 {
            Color::Yellow
        } else if node.kind == NodeKind::Directory {
            Color::Cyan
        } else {
            Color::White
        };
        spans.push(Span::styled(
            format!("{} ", if node.complete { " " } else { spinner }),
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled(name, Style::default().fg(color)));
        let line = Line::from(spans);
        if self.visual.contains(id) {
            line.style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
        } else if selected {
            line.style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            line
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, refreshing: bool, deleting: bool) {
        let area = frame.area();
        let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let hint = if self.filter_editing {
            "Enter apply  Esc clear"
        } else if self.visual.active() {
            if area.width < 40 {
                "? help"
            } else if area.width >= 60 {
                "d delete  Esc clear  ? help"
            } else {
                "d delete  ? help"
            }
        } else if deleting {
            if area.width >= 60 {
                "v select  D queue  ? help"
            } else {
                "D queue  ? help"
            }
        } else if area.width >= 40 {
            "v select  ? help"
        } else {
            "? help"
        };
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(hint.len() as u16 + 2),
            ])
            .split(header);
        let mut phase_text = if self.filter_editing {
            format!(
                "Filter /{}",
                clipped_tail(
                    &self.filter,
                    usize::from(columns[0].width).saturating_sub(8)
                )
            )
        } else if self.visual.active() && area.width < 40 {
            format!("Visual {}", self.visual.len())
        } else if self.visual.active() {
            format!(
                "Visual · {} selected{}",
                self.visual.len(),
                if !self.visual_input_available() {
                    " · Input full"
                } else if self.visual.is_pending()
                    || self.pending_navigation.is_some()
                    || self.range_requested
                    || !self.visual_intents.is_empty()
                    || self.mouse_dragging
                {
                    " · Selecting…"
                } else {
                    ""
                }
            )
        } else if self.queue_view.status.is_empty() {
            self.phase(refreshing, deleting).to_owned()
        } else {
            self.queue_view.status.clone()
        };
        if !self.filter_editing && !self.filter.is_empty() {
            let _ = write!(phase_text, " · /{}", self.filter);
        }
        let phase = phase_text.as_str();
        let path_budget =
            usize::from(columns[0].width).saturating_sub(Line::from(phase).width() + 3);
        let path = clipped(
            &printable(self.model.path(self.directory).as_os_str()),
            path_budget,
        );
        let label = if path.is_empty() {
            phase.to_owned()
        } else {
            format!("{path}  {phase}")
        };
        let header_style = Style::default().bg(Color::DarkGray).fg(Color::White);
        if self.filter_editing && self.modal.is_none() && columns[0].width > 0 {
            let x = Line::from(label.as_str())
                .width()
                .min(usize::from(columns[0].width - 1));
            frame.set_cursor_position((columns[0].x + x as u16, columns[0].y));
        }
        frame.render_widget(Paragraph::new(label).style(header_style), columns[0]);
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(header_style),
            columns[1],
        );
        let body = Rect::new(
            area.x,
            area.y + area.height.min(1),
            area.width,
            area.height.saturating_sub(1),
        );
        let hovered = self.selected.filter(|&id| {
            area.width >= 120
                && self.eligible(id)
                && self.model.node(id).kind == NodeKind::Directory
        });
        self.order.prioritize(self.directory, hovered);
        self.preview.sync(hovered, &self.order, &self.model);
        let (body, preview_area) = if hovered.is_some() {
            let parts = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(70), Constraint::Length(46)])
                .split(body);
            (parts[0], Some(parts[1]))
        } else {
            (body, None)
        };
        self.list_area = body;
        let page_size = usize::from(body.height).clamp(1, VIEW_BUDGET);
        let resized = self.page_size != page_size;
        self.page_size = page_size;
        // Earlier keyboard navigation may still own self.view. Freeze the
        // physical rows under a queued press without retargeting that work.
        let held_view = self.visual_intents.iter().find_map(|intent| match intent {
            VisualIntent::Mouse(gesture) if !gesture.released => {
                Some((Arc::clone(&gesture.view), gesture.offset))
            }
            _ => None,
        });
        if held_view.is_none() {
            // Hover and held presses must not scroll rows away from the pointer.
            let margin = if resized && self.mouse_press_view.is_none() && !self.has_held_mouse() {
                SCROLL_MARGIN
            } else {
                0
            };
            self.scroll_selection(margin);
        }
        let total = self.size(self.directory);
        static SPINNER_CLOCK: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let spinner = spinner_frame(
            SPINNER_CLOCK
                .get_or_init(Instant::now)
                .elapsed()
                .as_millis()
                / FRAME_INTERVAL.as_millis(),
        );
        let (painted_view, painted_offset) =
            held_view.unwrap_or_else(|| (Arc::clone(&self.view), self.offset));
        self.painted_rows = self
            .rows_in(&painted_view, painted_offset)
            .map(|(_, id)| id)
            .collect();
        self.painted_view = painted_view;
        self.painted_offset = painted_offset;
        self.painted_directory = self.directory;
        let mut lines = Vec::with_capacity(self.page_size);
        for &id in &self.painted_rows {
            lines.push(self.row(id, total, body.width, self.selected == Some(id), spinner));
        }
        if lines.is_empty() {
            let message = if !self.filter.is_empty() {
                if Arc::ptr_eq(&self.view, &self.empty) {
                    "Filtering…"
                } else {
                    "No matching entries."
                }
            } else if self
                .model
                .has_children(self.directory, false, self.config.show_hidden)
            {
                "Ordering entries…"
            } else if !self.model.listing_complete(self.directory) {
                "Waiting for directory scan…"
            } else if self.model.node(self.directory).error_count > 0 {
                "Partial listing; press i for details."
            } else if self.config.show_hidden {
                "No child entries."
            } else {
                "No visible entries; press e to show hidden entries."
            };
            lines.push(Line::from(message).style(Style::default().fg(Color::DarkGray)));
        }
        frame.render_widget(Paragraph::new(lines), body);
        if let Some(preview_area) = preview_area {
            let separator = Rect::new(preview_area.x, preview_area.y, 1, preview_area.height);
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(separator.height)])
                    .style(Style::default().fg(Color::DarkGray)),
                separator,
            );
            let pane = Rect::new(
                preview_area.x.saturating_add(2),
                preview_area.y,
                preview_area.width.saturating_sub(2),
                preview_area.height,
            );
            let mut rows = vec![
                Line::from("Contents").style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            for &id in self.preview.view.ids.iter().take(1024) {
                if rows.len() >= usize::from(pane.height) {
                    break;
                }
                if !self.eligible(id) {
                    continue;
                }
                rows.push(self.row(
                    id,
                    self.size(hovered.expect("preview has a hovered directory")),
                    pane.width,
                    false,
                    spinner,
                ));
            }
            if rows.len() == 1 {
                let directory = self
                    .model
                    .node(hovered.expect("preview has a hovered directory"));
                let message = if self.model.has_children(
                    hovered.expect("preview has a hovered directory"),
                    false,
                    self.config.show_hidden,
                ) {
                    "Ordering entries…"
                } else if directory.error_count > 0 {
                    "Partial listing; press i for details."
                } else if !self.model.listing_complete(hovered.unwrap()) {
                    "Waiting for scan…"
                } else {
                    "No child entries."
                };
                rows.push(Line::from(message).style(Style::default().fg(Color::DarkGray)));
            }
            frame.render_widget(Paragraph::new(rows), pane);
        }
        if let Some(modal) = self.modal.as_mut() {
            draw_modal(frame, area, modal, &mut self.queue_view);
        }
    }
}

fn spinner_frame(tick: u128) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick % FRAMES.len() as u128) as usize]
}

fn controls(keys: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            keys,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            action,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, modal: &mut Modal, queue: &mut QueueView) {
    let width = area.width.min(76);
    let inner_width = width.saturating_sub(2);
    let style = Style::default().bg(Color::Black).fg(Color::White);
    let border = |title: &'static str| {
        Block::bordered()
            .border_type(BorderType::Plain)
            .title(
                Line::from(format!(" {title} ")).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .border_style(Style::default().fg(Color::Gray))
            .style(style)
    };
    match modal {
        Modal::Batch {
            target,
            count,
            ready,
            reviewable,
            scroll,
        } => {
            let paragraph = Paragraph::new(target.as_str()).wrap(Wrap { trim: false });
            let target_lines = paragraph.line_count(inner_width);
            let height = area.height.min(14).min(
                u16::try_from(target_lines)
                    .unwrap_or(u16::MAX)
                    .saturating_add(6),
            );
            let popup = centered(area, width, height);
            let block = border("Delete selection?");
            let inner = block.inner(popup);
            let regions = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(3),
                ])
                .split(inner);
            let geometry = inner.width >= 32 && regions[1].height > 0 && regions[2].height == 3;
            *reviewable = geometry && *ready && *count > 0;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            if !geometry {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from("Resize to review"),
                        controls("n / Esc", ": cancel"),
                    ])
                    .style(style),
                    inner,
                );
                return;
            }
            frame.render_widget(
                Paragraph::new(format!("Permanently remove {count} selected entries in:"))
                    .style(style),
                regions[0],
            );
            clamp_scroll(scroll, target_lines, regions[1].height);
            frame.render_widget(
                paragraph
                    .scroll((*scroll, 0))
                    .style(style.add_modifier(Modifier::BOLD)),
                regions[1],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("↑↓ / PgUp / PgDown: scroll")
                        .style(Style::default().fg(Color::DarkGray)),
                    if *ready {
                        controls("y / Enter", ": enqueue selected")
                    } else {
                        Line::from("Checking targets…").style(Style::default().fg(Color::DarkGray))
                    },
                    controls("n / Esc", ": cancel"),
                ])
                .style(style),
                regions[2],
            );
        }
        Modal::Queue { .. } => {
            let height = area.height.min(20);
            let popup = centered(area, width, height);
            let block = border("Deletion queue");
            let inner = block.inner(popup);
            let regions = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(0),
                    Constraint::Length(4),
                ])
                .split(inner);
            queue.visible_rows = usize::from(regions[1].height).min(12);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let summary = if queue.active {
                queue.status.clone()
            } else if queue.pending == 0 {
                "No deletions queued".to_owned()
            } else {
                format!("{} queued", queue.pending)
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(summary).style(Style::default().fg(Color::Cyan)),
                    Line::from(if queue.paused {
                        "Pending paused"
                    } else {
                        "Pending running"
                    })
                    .style(Style::default().fg(Color::DarkGray)),
                ])
                .style(style),
                regions[0],
            );
            let rows: Vec<_> = queue
                .rows
                .iter()
                .map(|(id, active, target)| {
                    let row = Line::from(vec![
                        Span::styled(
                            if *active {
                                if queue.cancelling {
                                    "cancel  "
                                } else {
                                    "active  "
                                }
                            } else {
                                "queued  "
                            },
                            Style::default().fg(if *active {
                                Color::Cyan
                            } else {
                                Color::DarkGray
                            }),
                        ),
                        Span::raw(clipped(target, usize::from(inner.width).saturating_sub(8))),
                    ]);
                    if queue.selected == Some(*id) {
                        row.style(
                            Style::default()
                                .bg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        row
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(rows).style(style), regions[1]);
            frame.render_widget(
                Paragraph::new(vec![
                    controls("Space / p", ": pause/resume pending"),
                    controls("d / x / c", ": remove selected / cancel active"),
                    controls("Shift+↑↓ / K J", ": reorder queued"),
                    controls("↑↓ / D / Esc", ": select / close"),
                ])
                .style(style),
                regions[2],
            );
        }
        Modal::Quit { reviewable } => {
            let popup = centered(area, width, area.height.min(8));
            let block = border("Deletions in progress");
            let inner = block.inner(popup);
            *reviewable = inner.width >= 32 && inner.height >= 6;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let progress = if queue.status.is_empty() {
                "Waiting for confirmed deletions.".to_owned()
            } else {
                queue.status.clone()
            };
            let lines = if *reviewable {
                vec![
                    Line::from("Finishing deletions. Scanning stopped."),
                    Line::from(progress).style(Style::default().fg(Color::Cyan)),
                    Line::from("Quits when deletion finishes."),
                    controls("Ctrl-C", ": cancel all and quit"),
                    controls("q / Enter", ": cancel all and quit"),
                    controls("n / Esc", ": keep browsing"),
                ]
            } else {
                vec![
                    controls("Ctrl-C", ": cancel all and quit"),
                    Line::from("Scanning stopped."),
                    Line::from(progress),
                    controls("n / Esc", ": keep browsing"),
                ]
            };
            frame.render_widget(Paragraph::new(lines).style(style), inner);
        }
        Modal::Enqueueing => {
            let popup = centered(area, width, area.height.min(6));
            let block = border("Enqueueing selection");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Adding selected entries to the queue."),
                    Line::from("Queued deletions continue."),
                    controls("n / Esc", ": stop adding"),
                    controls("q / Ctrl-C", ": review quit"),
                ])
                .style(style),
                inner,
            );
        }
        Modal::Stopping => {
            let popup = centered(area, width, area.height.min(6));
            let block = border("Stopping deletions");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Cancelling deletions; exiting shortly."),
                    Line::from(queue.status.clone()).style(Style::default().fg(Color::Cyan)),
                    controls("Ctrl-C", ": quit now"),
                ])
                .style(style),
                inner,
            );
        }
        Modal::Delete {
            target,
            scroll,
            reviewable,
            confirmed,
            ..
        } => {
            let target_paragraph = Paragraph::new(target.as_str()).wrap(Wrap { trim: false });
            let target_lines = target_paragraph.line_count(inner_width);
            let prompt = Paragraph::new("Permanently remove:").wrap(Wrap { trim: false });
            let prompt_height = u16::try_from(prompt.line_count(inner_width)).unwrap_or(u16::MAX);
            let height = area.height.min(14).min(
                u16::try_from(target_lines)
                    .unwrap_or(u16::MAX)
                    .saturating_add(prompt_height)
                    .saturating_add(5),
            );
            let popup = centered(area, width, height);
            let block = border("Delete?");
            let inner = block.inner(popup);
            let regions = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(prompt_height),
                    Constraint::Min(0),
                    Constraint::Length(3),
                ])
                .split(inner);
            let geometry = inner.width >= 18 && regions[1].height > 0 && regions[2].height == 3;
            *reviewable = geometry && confirmed.is_some();
            frame.render_widget(Clear, popup);
            if !geometry {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from("Resize to review"),
                        controls("n / Esc", ": cancel"),
                    ])
                    .style(style),
                    popup,
                );
                return;
            }
            frame.render_widget(block, popup);
            frame.render_widget(prompt.style(style), regions[0]);
            clamp_scroll(scroll, target_lines, regions[1].height);
            frame.render_widget(
                target_paragraph
                    .scroll((*scroll, 0))
                    .style(style.add_modifier(Modifier::BOLD)),
                regions[1],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("↑↓ / PgUp / PgDown: scroll")
                        .style(Style::default().fg(Color::DarkGray)),
                    if confirmed.is_some() {
                        controls("y / Enter", ": delete")
                    } else {
                        Line::from("Checking target…").style(Style::default().fg(Color::DarkGray))
                    },
                    controls("n / Esc", ": cancel"),
                ])
                .style(style),
                regions[2],
            );
        }
        Modal::Text {
            title,
            text,
            scroll,
        } => {
            let lines: Vec<_> = text
                .lines()
                .map(|line| {
                    if *title == "Help" {
                        if matches!(
                            line,
                            "Normal mode" | "Visual mode" | "Common keys" | "Deletion queue"
                        ) {
                            return Line::from(line.to_owned()).style(
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            );
                        }
                        if let Some((keys, action)) = line.split_once("    ") {
                            return Line::from(vec![
                                Span::styled(
                                    keys.to_owned(),
                                    Style::default()
                                        .fg(Color::Yellow)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(format!("  {}", action.trim_start())),
                            ]);
                        }
                    }
                    Line::from(line.to_owned())
                })
                .collect();
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            let line_count = paragraph.line_count(inner_width);
            let height = area.height.min(22).min(
                u16::try_from(line_count)
                    .unwrap_or(u16::MAX)
                    .saturating_add(4),
            );
            let popup = centered(area, width, height);
            let block = border(title);
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let regions = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(2)])
                .split(inner);
            clamp_scroll(scroll, line_count, regions[0].height);
            frame.render_widget(paragraph.scroll((*scroll, 0)).style(style), regions[0]);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("↑↓ / PgUp / PgDown: scroll")
                        .style(Style::default().fg(Color::DarkGray)),
                    if matches!(
                        *title,
                        "Selecting entries" | "Preparing selection" | "Stopping selection"
                    ) {
                        controls("n / Esc", ": cancel")
                    } else {
                        controls("Enter / Esc", ": close")
                    },
                ])
                .style(style),
                regions[1],
            );
        }
    }
}

const NORMAL_HELP: &str = "Normal mode
Shift+↑/↓          Move one page
Enter              Open directory
d                  Delete current entry; confirm with y/Enter
D                  Deletion queue
v / V              Enter visual mode";

const VISUAL_HELP: &str = "Visual mode
Shift+↑/↓          Extend selection
Shift+Home/End     Select to first/last entry
Shift+PgUp/PgDown  Extend by one page
Enter              Select/deselect current entry
Click / drag       Toggle / add range; hold edge to scroll
Shift-click/drag   Extend / invert range
d / D / Delete     Confirm selected deletions
v / V / Esc        Leave visual mode; Esc clears an active filter first";

const COMMON_HELP: &str = "Common keys
↑/k ↓/j            Move cursor without selecting
Home/End           First/last entry
PageUp/PageDown    Move one page
→/l                Open directory
←/h/Backspace/<    Parent directory
/                  Filter file/folder names here; no subdirectory search
Enter / Esc        Finish filter input / clear filter
Backspace / Ctrl-U  Delete filter character / clear input
n/s/C              Sort by name/size/items; repeat to reverse
a                  Allocated/apparent sizes
t                  Directories first
g                  Fractional bar/percentage/no graph
c                  Item counts
e                  Show/hide dotfiles
i                  Entry information
r / R              Rescan selected folder / current directory
?                  Help
q / Ctrl-C         Quit; confirm cancellation if deleting

Deletion queue
Space / d          Pause waiting jobs / cancel selected job
c / x              Cancel active job / remove waiting job
Shift+↑↓ / K/J     Reorder waiting job
D / Esc            Close queue

⠋ unfinished entry    ~ partial size    ! read error
Sorting includes unfinished entries.
Near-size groups retain their previous order.
Graph shows the share of known directory size.
At 120+ columns, preview shows files and directories.
Symlinks are not followed; hardlinks counted once.";

struct DeleteRequest {
    id: NodeId,
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: String,
}

struct WaitingBatch {
    directory: NodeId,
    epoch: u64,
    anchor: Arc<RootAnchor>,
}

struct BatchRequest {
    selection: im::HashSet<NodeId>,
    directory: NodeId,
    epoch: u64,
    relative: PathBuf,
    target: String,
    anchor: Arc<RootAnchor>,
    order: Arc<CachedSnapshot>,
    show_hidden: bool,
}

struct BatchPreparation {
    worker: BatchCapture,
    remaining: im::HashSet<NodeId>,
    pending: Option<(NodeId, PathBuf, usize)>,
    directory: NodeId,
    epoch: u64,
    relative: PathBuf,
    anchor: Arc<RootAnchor>,
    order: Arc<CachedSnapshot>,
    show_hidden: bool,
    input_finished: bool,
    dispatching: bool,
    confirm_pending: bool,
}

fn prepare_batch(
    app: &mut App,
    anchor: Option<Arc<RootAnchor>>,
    batch: &mut Option<BatchPreparation>,
    retired: &VecDeque<BatchCapture>,
) {
    if !app.visual.active() {
        return;
    }
    let Some(anchor) = anchor else {
        app.modal = Some(Modal::notice(
            "Cannot delete",
            "Scan root unavailable.".to_owned(),
        ));
        return;
    };
    if app.visual_busy() {
        app.cancel_deferred_batch();
        app.waiting_batch = Some(WaitingBatch {
            directory: app.directory,
            epoch: app.model.node(app.directory).child_epoch,
            anchor,
        });
        app.modal = Some(Modal::notice(
            "Selecting entries",
            "Finishing selection. Esc cancels.".to_owned(),
        ));
        return;
    }
    if app.visual.is_empty() {
        app.modal = Some(Modal::notice(
            "Nothing selected",
            "Mark entries with Enter or Shift+arrows.".to_owned(),
        ));
        return;
    }
    let path = app.model.path(app.directory);
    let Ok(relative) = path.strip_prefix(&app.root) else {
        return;
    };
    let request = BatchRequest {
        selection: app.visual.snapshot(),
        directory: app.directory,
        epoch: app.model.node(app.directory).child_epoch,
        relative: relative.to_owned(),
        target: printable(path.as_os_str()),
        anchor,
        order: Arc::clone(&app.view),
        show_hidden: app.config.show_hidden,
    };
    if batch.is_some() || !retired.is_empty() {
        app.cancel_deferred_batch();
        app.deferred_batch = Some(request);
        app.modal = Some(Modal::notice(
            if batch.is_some() {
                "Preparing selection"
            } else {
                "Stopping selection"
            },
            "Waiting for the previous selection preparation to finish.".to_owned(),
        ));
        return;
    }
    start_batch_request(app, request, batch);
}

fn start_batch_request(app: &mut App, request: BatchRequest, batch: &mut Option<BatchPreparation>) {
    if matches!(
        app.modal,
        Some(Modal::Text {
            title: "Stopping selection" | "Preparing selection",
            ..
        })
    ) {
        app.modal = None;
    }
    if !app.model.is_live(request.directory)
        || app.model.node(request.directory).child_epoch != request.epoch
        || !app.visual.active()
        || app.visual.directory() != request.directory
    {
        app.visual
            .retire_snapshot_and_order(request.selection, request.order);
        return;
    }
    match BatchCapture::new(Arc::clone(&request.anchor)) {
        Ok(worker) => {
            let count = request.selection.len();
            *batch = Some(BatchPreparation {
                worker,
                remaining: request.selection,
                pending: None,
                directory: request.directory,
                epoch: request.epoch,
                relative: request.relative,
                anchor: request.anchor,
                order: request.order,
                show_hidden: request.show_hidden,
                input_finished: false,
                dispatching: false,
                confirm_pending: false,
            });
            app.modal = Some(Modal::Batch {
                target: request.target,
                count,
                ready: false,
                reviewable: false,
                scroll: 0,
            });
        }
        Err(error) => {
            app.visual
                .retire_snapshot_and_order(request.selection, request.order);
            app.modal = Some(Modal::notice(
                "Cannot delete",
                printable(OsStr::new(&error.to_string())),
            ));
        }
    }
}

fn resume_batch(
    app: &mut App,
    batch: &mut Option<BatchPreparation>,
    retired: &VecDeque<BatchCapture>,
) {
    if !(app.modal.is_none()
        || matches!(
            app.modal,
            Some(Modal::Text {
                title: "Stopping selection" | "Preparing selection" | "Selecting entries",
                ..
            })
        ))
    {
        return;
    }
    if let Some(waiting) = app.waiting_batch.as_ref() {
        if app.directory != waiting.directory
            || !app.model.is_live(waiting.directory)
            || app.model.node(waiting.directory).child_epoch != waiting.epoch
            || !app.visual.active()
            || app.visual.directory() != waiting.directory
        {
            app.cancel_deferred_batch();
            app.modal = Some(Modal::notice(
                "Selection changed",
                "Select entries again.".to_owned(),
            ));
            return;
        }
        if app.visual_busy() {
            return;
        }
        let waiting = app.waiting_batch.take().expect("waiting selection exists");
        app.modal = None;
        prepare_batch(app, Some(waiting.anchor), batch, retired);
        return;
    }
    if batch.is_none() && retired.is_empty() {
        if let Some(request) = app.deferred_batch.take() {
            start_batch_request(app, request, batch);
        }
    }
}

fn cancel_batch(
    app: &mut App,
    batch: &mut Option<BatchPreparation>,
    retired: &mut VecDeque<BatchCapture>,
) {
    if let Some(mut preparation) = batch.take() {
        preparation.worker.cancel();
        app.visual.retire_snapshot_and_order(
            std::mem::take(&mut preparation.remaining),
            preparation.order,
        );
        retired.push_back(preparation.worker);
    }
}

fn cancel_unconfirmed(
    app: &mut App,
    batch: &mut Option<BatchPreparation>,
    retired: &mut VecDeque<BatchCapture>,
) {
    app.cancel_deferred_batch();
    app.deferred_delete = None;
    if batch
        .as_ref()
        .is_some_and(|preparation| !preparation.dispatching && !preparation.confirm_pending)
    {
        cancel_batch(app, batch, retired);
    }
    // A quit warning can suspend a confirmation or a pending identity check.
    // Keep confirmed admission intact and never replace the warning itself.
    for modal in [&mut app.modal, &mut app.previous_modal] {
        if matches!(modal, Some(Modal::Delete { .. }))
            || (batch.is_none() && matches!(modal, Some(Modal::Batch { .. })))
            || matches!(
                modal,
                Some(Modal::Text {
                    title: "Selecting entries"
                        | "Preparing selection"
                        | "Stopping selection"
                        | "Stopping target",
                    ..
                })
            )
        {
            *modal = None;
        }
    }
}

fn exclude_job(
    app: &mut App,
    scanner: &ScanHandle,
    scopes: &[ScopedScan],
    id: NodeId,
    relative: &std::path::Path,
) {
    scanner.exclude(relative);
    for scope in scopes {
        scope.handle.exclude(relative);
    }
    if app.model.node(id).kind == NodeKind::Directory && !app.model.node(id).complete {
        let changed = app.model.suppress_scan(id);
        app.changed(changed);
    }
}

fn batch_modal(app: &mut App) -> Option<&mut Modal> {
    if matches!(app.modal, Some(Modal::Quit { .. })) {
        app.previous_modal.as_mut()
    } else {
        app.modal.as_mut()
    }
}

#[derive(Default)]
struct BatchPump {
    paint: bool,
    productive: bool,
}

fn pump_batch(
    app: &mut App,
    scanner: &ScanHandle,
    scopes: &[ScopedScan],
    queue: &mut DeleteQueue,
    batch: &mut Option<BatchPreparation>,
    retired: &mut VecDeque<BatchCapture>,
) -> BatchPump {
    let Some(preparation) = batch.as_mut() else {
        return BatchPump::default();
    };
    let deadline = Instant::now() + Duration::from_millis(1);
    let mut status = BatchPump::default();
    let mut done = false;
    let mut error = None;
    if preparation.confirm_pending && preparation.worker.confirm() {
        preparation.confirm_pending = false;
        preparation.dispatching = true;
        // Keep the admission modal until every confirmed entry has reached the FIFO.
        // The deletion worker continues independently while later confirmations wait.
        status.paint = true;
        status.productive = true;
    }
    if !preparation.input_finished {
        for _ in 0..128 {
            if Instant::now() >= deadline {
                break;
            }
            let candidate = if let Some(candidate) = preparation.pending.take() {
                Some(candidate)
            } else if let Some(id) = preparation.remaining.iter().next().copied() {
                preparation.remaining.remove(&id);
                status.productive = true;
                if !app.model.is_live(id)
                    || app.model.node(id).parent != Some(preparation.directory)
                    || (!preparation.show_hidden
                        && app.model.node(id).name.as_encoded_bytes().starts_with(b"."))
                {
                    continue;
                }
                preparation
                    .order
                    .positions
                    .get(&id)
                    .copied()
                    .map(|ordinal| {
                        (
                            id,
                            preparation.relative.join(app.model.node(id).name.as_ref()),
                            ordinal,
                        )
                    })
            } else {
                None
            };
            let Some((id, relative, ordinal)) = candidate else {
                break;
            };
            if let Err((id, relative)) = preparation.worker.try_push_ordered(id, relative, ordinal)
            {
                preparation.pending = Some((id, relative, ordinal));
                break;
            }
            status.productive = true;
        }
        if preparation.remaining.is_empty() && preparation.pending.is_none() {
            preparation.input_finished = preparation.worker.finish_input();
        }
    }
    let mut empty = false;
    for _ in 0..128 {
        if Instant::now() >= deadline {
            break;
        }
        let Some(event) = preparation.worker.event() else {
            empty = true;
            break;
        };
        status.productive = true;
        match event {
            BatchEvent::Ready { count } => {
                status.paint = true;
                if count == 0 {
                    done = true;
                } else if let Some(Modal::Batch {
                    count: displayed,
                    ready,
                    ..
                }) = batch_modal(app)
                {
                    *displayed = count;
                    *ready = true;
                }
            }
            BatchEvent::Entry {
                node,
                relative,
                target,
            } if preparation.dispatching => {
                if app.model.is_live(preparation.directory)
                    && app.model.node(preparation.directory).child_epoch == preparation.epoch
                    && app.model.is_live(node)
                    && app.model.node(node).parent == Some(preparation.directory)
                {
                    let outcome = queue.enqueue_confirmed(
                        node,
                        Arc::clone(&preparation.anchor),
                        relative.clone(),
                        target,
                    );
                    if outcome.inserted {
                        exclude_job(app, scanner, scopes, node, &relative);
                    }
                }
            }
            BatchEvent::Entry { .. } => {}
            BatchEvent::Failed(message) => {
                status.paint = true;
                error = Some(message);
                done = true;
            }
            BatchEvent::Done => {
                done = true;
                status.paint = true;
            }
        }
    }
    if empty && preparation.worker.is_drained() && !done {
        error = Some("Could not finish preparing selected deletions.".to_owned());
        done = true;
    }
    if done {
        let confirmed = preparation.dispatching || preparation.confirm_pending;
        cancel_batch(app, batch, retired);
        for modal in [&mut app.modal, &mut app.previous_modal] {
            if matches!(modal, Some(Modal::Batch { .. } | Modal::Enqueueing)) {
                *modal = None;
            }
        }
        if let Some(error) = error {
            let message = printable(OsStr::new(&error));
            if confirmed && matches!(app.modal, Some(Modal::Quit { .. } | Modal::Stopping)) {
                // Closing must report an incomplete confirmed batch after the
                // terminal is restored, or display it once if quitting is dismissed.
                app.delete_notice = Some(message);
            } else {
                let notice = Modal::notice("Cannot delete", message);
                if matches!(app.modal, Some(Modal::Quit { .. })) {
                    app.previous_modal = Some(notice);
                } else {
                    app.modal = Some(notice);
                }
            }
        }
    }
    status
}

fn pump_retired_batches(app: &mut App, retired: &mut VecDeque<BatchCapture>) {
    let Some(worker) = retired.front() else {
        return;
    };
    let mut empty = false;
    for _ in 0..128 {
        if worker.event().is_none() {
            empty = true;
            break;
        }
    }
    if empty && worker.is_drained() {
        retired.pop_front();
        if retired.is_empty()
            && matches!(
                app.modal,
                Some(Modal::Text {
                    title: "Stopping selection",
                    ..
                })
            )
        {
            app.modal = None;
        }
    }
}

fn prioritize_requested(app: &mut App, scanner: &ScanHandle, scopes: &[ScopedScan]) {
    if !app.prioritize_requested || (app.pending_navigation.is_some() && app.selected.is_some()) {
        return;
    }
    app.focus_after_selection |= app.pending_navigation.is_some();
    app.prioritize_requested = false;
    let focus = app.scan_focus();
    let Some(id) = focus else {
        scanner.clear_priority();
        for scope in scopes {
            scope.handle.clear_priority();
        }
        return;
    };
    if app.model.node(id).complete {
        scanner.clear_priority();
        for scope in scopes {
            scope.handle.clear_priority();
        }
        return;
    }
    let path = app.model.path(id);
    if let Ok(relative) = path.strip_prefix(&app.root) {
        scanner.prioritize(relative);
        for scope in scopes {
            scope.handle.prioritize(relative);
        }
    }
}

struct CaptureCompletion(Arc<AtomicBool>);
impl Drop for CaptureCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn spawn_capture(
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
) -> (Receiver<io::Result<ConfirmedTarget>>, Arc<AtomicBool>) {
    let (sender, receiver) = bounded(1);
    let failure_sender = sender.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    if let Err(error) = std::thread::Builder::new()
        .name("ldu-confirm".to_owned())
        .spawn(move || {
            let _completion = CaptureCompletion(worker_finished);
            let _ = sender.try_send(delete::capture(&anchor, &relative));
        })
    {
        let _ = failure_sender.try_send(Err(error));
        finished.store(true, Ordering::Release);
    }
    (receiver, finished)
}

struct ScopedScan {
    scope: NodeId,
    generation: u64,
    relative: PathBuf,
    handle: ScanHandle,
    entries: Option<(NodeId, std::vec::IntoIter<Entry>)>,
    finished: bool,
}

fn retire_scopes_with(
    scopes: &mut Vec<ScopedScan>,
    cancelled: bool,
    retire: impl FnOnce(Vec<ScopedScan>) -> Result<(), Vec<ScopedScan>>,
) {
    let mut ready = Vec::new();
    let mut index = 0;
    while index < scopes.len() {
        let scope = &scopes[index];
        if cancelled
            || (scope.finished
                && scope.handle.is_finished()
                && scope.entries.is_none()
                && scope.handle.receiver.is_empty())
        {
            ready.push(scopes.swap_remove(index));
        } else {
            index += 1;
        }
    }
    if !ready.is_empty() {
        if let Err(ready) = retire(ready) {
            // Returned ownership still occupies the same eight admission slots.
            scopes.extend(ready);
        }
    }
}

fn overlaps(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn request_refresh(requests: &mut VecDeque<NodeId>, id: NodeId) {
    if !requests.contains(&id) {
        requests.push_back(id);
    }
}

fn begin_refresh(
    app: &mut App,
    scan_context: (&ScanHandle, &ScanOptions),
    scopes: &mut Vec<ScopedScan>,
    queue: &DeleteQueue,
    batch: Option<&BatchPreparation>,
    id: NodeId,
    generation: &mut u64,
) -> bool {
    let (scanner, options) = scan_context;
    if !app.model.is_live(id) || app.model.node(id).kind != NodeKind::Directory {
        return true;
    }
    if scopes.len() >= 8 {
        return false;
    }
    let Some(anchor) = scanner.anchor.get().cloned() else {
        return false;
    };
    let path = app.model.path(id);
    let Ok(relative) = path.strip_prefix(&app.root) else {
        return true;
    };
    if queue.overlaps(&anchor, relative)
        || batch.is_some_and(|preparation| overlaps(relative, &preparation.relative))
        || app
            .deferred_batch
            .as_ref()
            .is_some_and(|request| overlaps(relative, &request.relative))
    {
        return false;
    }
    // Old scanners must stop publishing this subtree before its stable root is reused.
    scanner.exclude(relative);
    for scope in scopes.iter() {
        scope.handle.exclude(relative);
    }
    for scope in scopes.iter() {
        if scope.relative.starts_with(relative) {
            scope.handle.cancel();
        }
    }
    *generation = generation.wrapping_add(1);
    app.begin_model_refresh(id, *generation);
    app.prioritize_requested = true;
    scopes.push(ScopedScan {
        scope: id,
        generation: *generation,
        relative: relative.to_owned(),
        handle: scan::spawn_subtree(
            anchor,
            relative.to_owned(),
            options.clone(),
            scanner.hardlinks(),
        ),
        entries: None,
        finished: false,
    });
    true
}

fn stop_scanning(scanner: &ScanHandle, scopes: &[ScopedScan]) {
    scanner.cancel();
    for scope in scopes {
        scope.handle.cancel();
    }
}

fn prioritize_deletion(scanner: &ScanHandle, scopes: &[ScopedScan], deleting: bool) {
    scanner.set_paused(deleting);
    for scope in scopes {
        scope.handle.set_paused(deleting);
    }
}

fn take_interrupt(interrupts: &AtomicUsize) -> bool {
    // The UI is the only consumer; signal callbacks only increment the count.
    if interrupts.load(Ordering::Acquire) == 0 {
        return false;
    }
    interrupts.fetch_sub(1, Ordering::AcqRel);
    true
}

fn stop_work(app: &mut App, scanner: &ScanHandle, scopes: &[ScopedScan], queue: &mut DeleteQueue) {
    app.cancel_deferred_batch();
    app.deferred_delete = None;
    queue.cancel_all();
    stop_scanning(scanner, scopes);
    app.modal = Some(Modal::Stopping);
}

/// The first frame precedes all scanner filesystem work.
pub fn run(root: PathBuf, options: ScanOptions, size_mode: SizeMode) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive browsing requires a terminal; use --json for non-interactive output",
        ));
    }
    let interrupts = Arc::new(AtomicUsize::new(0));
    let signal_interrupts = Arc::clone(&interrupts);
    ctrlc::set_handler(move || {
        signal_interrupts.fetch_add(1, Ordering::Release);
    })
    .map_err(io::Error::other)?;
    let mut app = App::new(root.clone(), size_mode)?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.draw(|frame| app.draw(frame, false, false))?;
    if take_interrupt(&interrupts) {
        return Ok(());
    }
    let scanner = scan::spawn(root, options.clone());
    let mut queue = DeleteQueue::new()?;
    let mut scopes = Vec::new();
    let scope_retirer = Retirer::<Vec<ScopedScan>>::new()?;
    let mut batch = None;
    let mut retired_batches = VecDeque::new();
    let mut refresh_requests = VecDeque::new();
    let mut generation = 0;
    let mut quitting = false;
    let mut closing = false;
    let mut scans_stopped = false;
    let mut shutdown_deadline = None;
    let mut failure = None;
    app.started = true;
    let mut next_frame = Instant::now();
    let mut background_phase = 0;
    let mut order_backlog = true;
    let mut input_timeout = Duration::from_millis(1);
    'events: loop {
        app.pump_confirmation();
        let selection_progress = app.pump_visual();
        if selection_progress && !app.visual.is_pending() {
            next_frame = Instant::now();
        }
        let queue_progress = queue.pump(128);
        pump_retired_batches(&mut app, &mut retired_batches);
        if !quitting && !closing && failure.is_none() {
            resume_batch(&mut app, &mut batch, &retired_batches);
        }
        if !quitting && failure.is_some() {
            quitting = true;
            shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
            cancel_batch(&mut app, &mut batch, &mut retired_batches);
            stop_work(&mut app, &scanner, &scopes, &mut queue);
            next_frame = Instant::now();
        }
        if (quitting
            && (queue.active().is_none()
                || shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline)))
            || (closing && !queue.is_busy() && batch.is_none())
        {
            break;
        }
        prioritize_deletion(&scanner, &scopes, queue.active().is_some());
        let interrupt = take_interrupt(&interrupts);
        let input = if interrupt {
            Ok(true)
        } else {
            event::poll(input_timeout)
        };
        match input {
            Err(error) => {
                failure = Some(error);
                continue;
            }
            Ok(true) => {
                let event = if interrupt {
                    Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                } else {
                    match event::read() {
                        Ok(event) => event,
                        Err(error) => {
                            failure = Some(error);
                            continue;
                        }
                    }
                };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let reviewable = match app.modal.as_mut() {
                            Some(
                                Modal::Delete { reviewable, .. } | Modal::Batch { reviewable, .. },
                            ) if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) => {
                                Some(reviewable)
                            }
                            Some(Modal::Quit { reviewable })
                                if matches!(key.code, KeyCode::Char('q') | KeyCode::Enter) =>
                            {
                                Some(reviewable)
                            }
                            _ => None,
                        };
                        if let Some(reviewable) = reviewable.filter(|value| **value) {
                            // Keyboard events can precede a pending resize event. A
                            // confirmation must refer to the current terminal geometry.
                            let drawn = terminal.get_frame().area();
                            #[cfg(unix)]
                            let live_size = crossterm::terminal::window_size()
                                .map(|size| (size.columns, size.rows));
                            #[cfg(not(unix))]
                            let live_size = terminal.size().map(|size| (size.width, size.height));
                            match live_size {
                                Ok(size) if size != (drawn.width, drawn.height) => {
                                    // Ignore this key; the regular input redraw below
                                    // establishes reviewability at the new dimensions.
                                    *reviewable = false;
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    *reviewable = false;
                                    failure = Some(error);
                                    continue;
                                }
                            }
                        }
                        let anchor = scanner.anchor.get().cloned();
                        match app.key(
                            key,
                            anchor,
                            queue.is_busy()
                                || batch.as_ref().is_some_and(|batch| {
                                    batch.dispatching || batch.confirm_pending
                                }),
                        ) {
                            Action::QuitWarning => {
                                cancel_unconfirmed(&mut app, &mut batch, &mut retired_batches);
                                closing = true;
                                scans_stopped = true;
                                stop_scanning(&scanner, &scopes);
                                refresh_requests.clear();
                            }
                            Action::Quit => {
                                if quitting {
                                    break 'events;
                                }
                                quitting = true;
                                closing = false;
                                shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                                cancel_batch(&mut app, &mut batch, &mut retired_batches);
                                stop_work(&mut app, &scanner, &scopes, &mut queue);
                            }
                            Action::Refresh(id) => request_refresh(&mut refresh_requests, id),
                            Action::Delete(id, anchor, relative, target) => {
                                if app.model.is_live(id)
                                    && app.model.path(id) == app.root.join(&relative)
                                {
                                    let outcome = queue.enqueue_confirmed(
                                        id,
                                        anchor,
                                        relative.clone(),
                                        target,
                                    );
                                    if outcome.inserted {
                                        exclude_job(&mut app, &scanner, &scopes, id, &relative);
                                    }
                                }
                            }
                            Action::BatchPrepare => prepare_batch(
                                &mut app,
                                scanner.anchor.get().cloned(),
                                &mut batch,
                                &retired_batches,
                            ),
                            Action::BatchConfirm => {
                                if let Some(preparation) = batch.as_mut() {
                                    preparation.confirm_pending = true;
                                }
                            }
                            Action::BatchCancel => {
                                cancel_batch(&mut app, &mut batch, &mut retired_batches)
                            }
                            Action::QueuePause => {
                                queue.toggle_pause();
                            }
                            Action::QueueCancel => {
                                queue.cancel_active();
                            }
                            Action::QueueRemove | Action::QueueDelete => {
                                if let Some(id) = app.queue_selection() {
                                    let selected_active =
                                        queue.active().is_some_and(|entry| entry.id == id);
                                    if selected_active && matches!(key.code, KeyCode::Char('d')) {
                                        queue.cancel_active();
                                    } else if let Some(entry) = queue.remove_pending(id) {
                                        if !scans_stopped {
                                            request_refresh(&mut refresh_requests, entry.node);
                                        }
                                    }
                                }
                            }
                            Action::QueueReorder(direction) => {
                                if let Some(id) = app.queue_selection() {
                                    queue.move_pending(id, direction);
                                }
                            }
                            Action::QueueNavigate { end, amount } => {
                                app.queue_navigate(&queue, end, amount)
                            }
                            Action::Continue => {}
                        }
                        if closing && !matches!(app.modal, Some(Modal::Quit { .. })) {
                            closing = false;
                        }
                        if !quitting {
                            queue.start_next();
                        }
                        prioritize_deletion(&scanner, &scopes, queue.active().is_some());
                        app.pump_visual();
                        app.show_delete_notice();
                        prioritize_requested(&mut app, &scanner, &scopes);
                        app.sync_queue(&queue);
                        // Paint input immediately, before bounded background event work.
                        if failure.is_none() {
                            if let Err(error) = terminal.draw(|frame| {
                                app.draw(
                                    frame,
                                    !scopes.is_empty() || !refresh_requests.is_empty(),
                                    queue.is_busy() || batch.is_some(),
                                )
                            }) {
                                failure = Some(error);
                            }
                        }
                        next_frame = Instant::now() + FRAME_INTERVAL;
                    }
                    Event::Mouse(mouse) if !quitting => {
                        app.mouse(mouse);
                        app.pump_visual();
                        app.show_delete_notice();
                        prioritize_requested(&mut app, &scanner, &scopes);
                        app.sync_queue(&queue);
                        if let Err(error) = terminal.draw(|frame| {
                            app.draw(
                                frame,
                                !scopes.is_empty() || !refresh_requests.is_empty(),
                                queue.is_busy() || batch.is_some(),
                            )
                        }) {
                            failure = Some(error);
                        }
                        next_frame = Instant::now() + FRAME_INTERVAL;
                    }
                    Event::Resize(_, _) => next_frame = Instant::now(),
                    _ => {}
                }
            }
            Ok(false) => {}
        }
        let batch_status = if !quitting {
            pump_batch(
                &mut app,
                &scanner,
                &scopes,
                &mut queue,
                &mut batch,
                &mut retired_batches,
            )
        } else {
            BatchPump::default()
        };
        if batch_status.paint {
            next_frame = Instant::now();
        }
        if let Some(completion) = queue.poll() {
            let id = completion.entry.node;
            let result = completion.result;
            let parent = app.model.node(id).parent.unwrap_or(ROOT);
            let cancelled = result
                .outcome
                .as_ref()
                .err()
                .is_some_and(|error| error.kind() == io::ErrorKind::Interrupted);
            let needs_reconcile = result.outcome.is_err();
            if needs_reconcile && result.progress.removed_entries > 0 && app.model.is_live(id) {
                let changed = app.model.mark_partial(id);
                app.changed(changed);
            }
            match result.outcome {
                Ok(()) => app.deletion_succeeded(id, result.parent_metadata.is_some()),
                Err(error) if !quitting && !cancelled => {
                    app.delete_failed(id, &error);
                }
                Err(_) => {}
            }
            if let Some(metadata) = result.parent_metadata {
                let changed = app.model.update_directory_sizes(parent, metadata.sizes);
                app.changed(changed);
            }
            if let Some(error) = result.metadata_error {
                let changed = app.model.note_error(parent, error);
                app.changed(changed);
            }
            if needs_reconcile
                && !quitting
                && !scans_stopped
                && app.model.is_live(id)
                && app.model.node(id).kind == NodeKind::Directory
            {
                request_refresh(&mut refresh_requests, id);
            }
            next_frame = Instant::now();
        }
        match background_phase {
            0 => drain(&scanner, &mut app, quitting),
            1 => {
                let deadline = Instant::now() + WORK_BUDGET;
                for scope in &mut scopes {
                    if Instant::now() >= deadline {
                        break;
                    }
                    drain_scope(scope, &mut app, quitting, deadline);
                }
                if !scopes.is_empty() {
                    scopes.rotate_left(1);
                }
            }
            2 => {
                let changed = app.model.pump_removals();
                app.changed(changed);
            }
            _ => order_backlog = app.pump_order(),
        }
        background_phase = (background_phase + 1) % 4;
        retire_scopes_with(&mut scopes, false, |scopes| {
            scope_retirer.try_retire(scopes)
        });
        app.repair_directory();
        prioritize_requested(&mut app, &scanner, &scopes);
        let scan_backlog = app.scan_entries.is_some() || !scanner.receiver.is_empty();
        if !quitting {
            // Apply a stopped worker's completion before starting the next FIFO item.
            queue.start_next();
            let requests = if closing || queue.active().is_some() {
                0
            } else {
                refresh_requests.len()
            };
            for _ in 0..requests.min(MAX_BATCHES) {
                let Some(id) = refresh_requests.pop_front() else {
                    break;
                };
                if !begin_refresh(
                    &mut app,
                    (&scanner, &options),
                    &mut scopes,
                    &queue,
                    batch.as_ref(),
                    id,
                    &mut generation,
                ) {
                    refresh_requests.push_back(id);
                }
            }
        }
        prioritize_deletion(&scanner, &scopes, queue.active().is_some());
        if !quitting && app.show_delete_notice() {
            next_frame = Instant::now();
        }
        if Instant::now() >= next_frame {
            app.sync_queue(&queue);
            if failure.is_none() {
                if let Err(error) = terminal.draw(|frame| {
                    app.draw(
                        frame,
                        !scopes.is_empty() || !refresh_requests.is_empty(),
                        queue.is_busy() || batch.is_some(),
                    )
                }) {
                    failure = Some(error);
                }
            }
            next_frame = Instant::now() + FRAME_INTERVAL;
        }
        if quitting {
            input_timeout = INPUT_INTERVAL;
            continue;
        }
        let scope_backlog = scopes
            .iter()
            .any(|scope| scope.entries.is_some() || !scope.handle.receiver.is_empty());
        let timeout = if scan_backlog
            || scope_backlog
            || order_backlog
            || app.model.has_pending_removals()
            || batch_status.productive
            || queue_progress
            || queue.is_updating()
            || (selection_progress && app.visual.is_pending())
        {
            Duration::ZERO
        } else {
            let input_interval = if batch.is_some() {
                Duration::from_millis(1)
            } else {
                INPUT_INTERVAL
            };
            input_interval.min(next_frame.saturating_duration_since(Instant::now()))
        };
        input_timeout = timeout.max(Duration::from_millis(1));
    }
    scanner.cancel();
    for scope in &scopes {
        scope.handle.cancel();
    }
    // A cancelled syscall may still be blocked. Restore the terminal without
    // joining workers; process exit ends any remaining detached work.
    drop(terminal);
    drop(_guard);
    scopes.push(ScopedScan {
        scope: ROOT,
        generation: 0,
        relative: PathBuf::new(),
        handle: scanner,
        entries: app.scan_entries.take(),
        finished: app.baseline_finished,
    });
    if failure.is_none() {
        failure = app
            .delete_notice
            .take()
            .map(|message| io::Error::other(format!("Delete failed: {message}")));
    }
    // Reclaim large models, cached views, queues and scoped state off-thread.
    // If thread creation fails, dropping the closure must not synchronously
    // reclaim them here: the exiting process will release those allocations.
    let cleanup =
        std::mem::ManuallyDrop::new((app, queue, batch, retired_batches, scopes, refresh_requests));
    let _ = std::thread::Builder::new()
        .name("ldu-exit".into())
        .spawn(move || drop(std::mem::ManuallyDrop::into_inner(cleanup)));
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn drain_scope(scope: &mut ScopedScan, app: &mut App, discard: bool, deadline: Instant) {
    if discard {
        scope.entries = None;
    }
    for _ in 0..MAX_BATCHES {
        if Instant::now() >= deadline {
            break;
        }
        if let Some((parent, entries)) = scope.entries.as_mut() {
            let event = ScanEvent::Entries {
                parent: *parent,
                entries: entries.by_ref().take(128).collect(),
            };
            if entries.len() == 0 {
                scope.entries = None;
            }
            let changed = app.model.apply_scoped(scope.scope, scope.generation, event);
            app.changed(changed);
            continue;
        }
        match scope.handle.receiver.try_recv() {
            Ok(ScanEvent::Entries { parent, entries }) if !discard => {
                scope.entries = Some((parent, entries.into_iter()))
            }
            Ok(event) if !discard => {
                if matches!(event, ScanEvent::Finished { .. }) {
                    scope.finished = true;
                }
                let changed = app.model.apply_scoped(scope.scope, scope.generation, event);
                app.changed(changed);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if !discard && !scope.finished {
                    scope.finished = true;
                    let changed = app.model.note_scan_error(
                        scope.scope,
                        scope.generation,
                        "Scanner stopped before reporting completion.".into(),
                    );
                    app.changed(changed);
                    let changed = app.model.apply_scoped(
                        scope.scope,
                        scope.generation,
                        ScanEvent::Finished { cancelled: true },
                    );
                    app.changed(changed);
                }
                break;
            }
        }
    }
}

fn drain(handle: &ScanHandle, app: &mut App, discard: bool) {
    if discard {
        app.scan_entries = None;
    }
    let deadline = Instant::now() + WORK_BUDGET;
    for _ in 0..MAX_BATCHES {
        if Instant::now() >= deadline {
            break;
        }
        if let Some((parent, entries)) = app.scan_entries.as_mut() {
            let event = ScanEvent::Entries {
                parent: *parent,
                entries: entries.by_ref().take(128).collect(),
            };
            if entries.len() == 0 {
                app.scan_entries = None;
            }
            let changed = app.model.apply(event);
            app.changed(changed);
            continue;
        }
        match handle.receiver.try_recv() {
            Ok(ScanEvent::Entries { parent, entries }) if !discard => {
                app.scan_entries = Some((parent, entries.into_iter()));
            }
            Ok(event) if !discard => {
                if matches!(event, ScanEvent::Finished { .. }) {
                    app.baseline_finished = true;
                }
                let changed = app.model.apply(event);
                app.changed(changed);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if !discard && !app.baseline_finished {
                    app.baseline_finished = true;
                    let changed = app.model.note_scan_error(
                        ROOT,
                        0,
                        "Scanner stopped before reporting completion.".into(),
                    );
                    app.changed(changed);
                    let changed = app.model.apply(ScanEvent::Finished { cancelled: true });
                    app.changed(changed);
                }
                break;
            }
        }
    }
}

fn scroll_key(key: KeyCode, scroll: &mut u16) {
    match key {
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(8),
        KeyCode::PageDown => *scroll = scroll.saturating_add(8),
        KeyCode::Home => *scroll = 0,
        KeyCode::End => *scroll = u16::MAX,
        _ => {}
    }
}
fn clamp_scroll(scroll: &mut u16, lines: usize, height: u16) {
    *scroll = (*scroll).min(
        u16::try_from(lines)
            .unwrap_or(u16::MAX)
            .saturating_sub(height),
    );
}
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
fn fractional_bar(size: u64, total: u64, width: usize) -> String {
    const FRACTIONS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let units = if total == 0 {
        0
    } else {
        ((u128::from(size) * width as u128 * 8 + u128::from(total) / 2) / u128::from(total))
            .min(width as u128 * 8) as usize
    };
    let mut bar = "█".repeat(units / 8);
    if units % 8 != 0 {
        bar.push(FRACTIONS[units % 8]);
    }
    bar.push_str(&" ".repeat(width.saturating_sub(units.div_ceil(8))));
    bar
}
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
fn clipped_tail(text: &str, width: usize) -> String {
    let line = Line::from(text);
    if line.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut skip = line.width().saturating_sub(width - 1);
    let mut output = String::from("…");
    for grapheme in line.styled_graphemes(Style::default()) {
        if skip > 0 {
            skip = skip.saturating_sub(Span::raw(grapheme.symbol).width());
        } else {
            output.push_str(grapheme.symbol);
        }
    }
    output
}

fn clipped(text: &str, width: usize) -> String {
    if Line::from(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    let line = Line::from(text);
    for grapheme in line.styled_graphemes(Style::default()) {
        let size = Span::raw(grapheme.symbol).width();
        if used + size > width - 1 {
            break;
        }
        output.push_str(grapheme.symbol);
        used += size;
    }
    output.push('…');
    output
}
fn printable(value: &OsStr) -> String {
    let mut output = String::new();
    let mut bytes = value.as_encoded_bytes();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                printable_text(text, &mut output);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if let Ok(text) = std::str::from_utf8(&bytes[..valid]) {
                    printable_text(text, &mut output);
                }
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                for byte in &bytes[valid..valid + invalid] {
                    let _ = write!(output, "\\x{byte:02x}");
                }
                bytes = &bytes[valid + invalid..];
            }
        }
    }
    output
}
fn printable_text(text: &str, output: &mut String) {
    for ch in text.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}') =>
            {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(ch));
            }
            ch => output.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    fn test_confirmed_target() -> ConfirmedTarget {
        let anchor = RootAnchor::open(std::path::Path::new(".")).unwrap();
        delete::capture(&anchor, std::path::Path::new("unmodified-test-target")).unwrap()
    }

    #[test]
    fn fractional_bars_use_eighth_cells_and_keep_exact_width() {
        assert_eq!(fractional_bar(1, 80, 10), "▏         ");
        assert_eq!(fractional_bar(5, 10, 10), "█████     ");
        assert_eq!(fractional_bar(99, 10, 10), "██████████");
        assert_eq!(fractional_bar(0, 0, 10), "          ");
    }
    #[test]
    fn pending_rows_share_spinner_and_fractional_bar_style_at_both_pane_widths() {
        use crate::types::ScanEvent;
        let mut app = browser_fixture();
        let _ = app.model.apply(ScanEvent::Entries {
            parent: 5,
            entries: vec![entry(6, "data", NodeKind::File, 3)],
        });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 2)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(app.row(5, 24, 80, false, spinner_frame(0))),
                    Rect::new(0, 0, 80, 1),
                );
                frame.render_widget(
                    Paragraph::new(app.row(5, 24, 44, false, spinner_frame(1))),
                    Rect::new(0, 1, 44, 1),
                );
            })
            .unwrap();
        let text = rendered(&terminal);
        assert!(text.contains("█▎") && text.contains('▋'));
        assert!(text.contains("⠋ child-folder/") && text.contains("⠙ child-folder/"));
        assert!(!text.contains('≥') && !text.contains('~') && !text.contains("<="));
        for marker in ["⠋", "⠙"] {
            let cell = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .find(|cell| cell.symbol() == marker)
                .unwrap();
            assert_eq!(cell.fg, Color::Gray);
        }
        for tick in 0..10 {
            assert_eq!(Span::raw(spinner_frame(tick)).width(), 1);
            assert_ne!(spinner_frame(tick), spinner_frame(tick + 1));
        }
        assert_eq!(spinner_frame(10), spinner_frame(0));
        let _ = app
            .model
            .apply(ScanEvent::DirectoryDone { id: 5, error: None });
        let complete = app.row(5, 24, 44, false, spinner_frame(0));
        assert!(!complete.to_string().contains('⠋'));
    }

    #[test]
    fn terminal_controls_and_backslashes_are_visible() {
        assert_eq!(
            printable(OsStr::new("line\n\t\u{1b}[31m\\\u{202e}")),
            "line\\n\\t\\u{1b}[31m\\\\\\u{202e}"
        );
    }
    #[cfg(unix)]
    #[test]
    fn invalid_filename_bytes_remain_distinguishable() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let invalid = OsString::from_vec(vec![b'a', 0xff, b'\n', b'\\', b'b']);
        assert_eq!(printable(&invalid), "a\\xff\\n\\\\b");
        assert_ne!(printable(&invalid), printable(OsStr::new("a\\xff\n\\b")));
    }
    fn rendered(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in buffer.content.chunks(usize::from(buffer.area.width).max(1)) {
            let mut covered: usize = 0;
            for cell in row {
                if covered == 0 {
                    text.push_str(cell.symbol());
                }
                covered = covered
                    .max(Span::raw(cell.symbol()).width())
                    .saturating_sub(1);
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn info_long_spaced_unicode_path_scrolls_without_losing_title_or_close_hint() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let path = format!(
            "/{}END-OF-SPACED-PATH",
            "long name 1\u{fe0f}\u{20e3}/".repeat(16)
        );
        app.modal = Some(Modal::info(format!("{path}\nAllocated: 12 KiB\nComplete")));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(38, 8)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let first = rendered(&terminal);
        assert!(first.contains("Info") && first.contains("Enter / Esc: close"));
        assert!(!first.contains("Complete"));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let last = rendered(&terminal);
        assert!(last.contains("Info") && last.contains("Enter / Esc: close"));
        assert!(last.contains("END-OF-SPACED-PATH") && last.contains("Complete"));
        app.key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(rendered(&terminal), first);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn delete_long_spaced_target_can_scroll_to_last_component_with_pinned_choices() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let target = format!("/{}FINAL-TARGET-NAME", "long path component ".repeat(30));
        app.modal = Some(Modal::Delete {
            id: ROOT,
            anchor: Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap()),
            relative: PathBuf::from("unmodified-test-target"),
            target,
            scroll: 0,
            reviewable: false,
            confirmed: Some(test_confirmed_target()),
            capture: None,
        });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(38, 10)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("FINAL-TARGET-NAME"));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let last = rendered(&terminal);
        assert!(last.contains("Delete?") && last.contains("FINAL-TARGET-NAME"));
        assert!(last.contains("y / Enter: delete") && last.contains("n / Esc: cancel"));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                None,
                false
            ),
            Action::Continue
        ));
        assert!(app.modal.is_none());
    }

    #[test]
    fn help_scrolls_to_final_explanation_and_zero_width_dialog_remains_safe() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        app.modal = Some(Modal::help(app.visual.active()));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(38, 8)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let first = rendered(&terminal);
        assert!(first.contains("Normal mode"));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let last = rendered(&terminal);
        // Borders separate wrapped physical lines; verify the visible final content
        // without treating border cells as part of a logical sentence.
        assert!(
            last.contains("Help")
                && last.contains("directories.")
                && last.contains("counted once.")
        );
        assert!(last.contains("Enter / Esc: close"));
        app.key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(rendered(&terminal), first);
        let mut zero = Terminal::new(ratatui::backend::TestBackend::new(0, 8)).unwrap();
        zero.draw(|frame| app.draw(frame, false, false)).unwrap();
    }

    #[test]
    fn header_clipping_preserves_graphemes_and_column_budget() {
        let path = "1\u{fe0f}\u{20e3}2\u{fe0f}\u{20e3}3\u{fe0f}\u{20e3}abc";
        let clipped = clipped(path, 4);
        assert_eq!(clipped, "1\u{fe0f}\u{20e3}…");
        assert!(Line::from(clipped).width() <= 4);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn delete_confirmation_waits_for_a_visible_target_and_recovers_after_resize() {
        for (width, height) in [(80, 4), (38, 6)] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            app.modal = Some(Modal::Delete {
                id: ROOT,
                anchor: Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap()),
                relative: PathBuf::from("unmodified-test-target"),
                target: "/fixture/unmodified-test-target".into(),
                scroll: 0,
                reviewable: false,
                confirmed: Some(test_confirmed_target()),
                capture: None,
            });
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            assert!(
                matches!(app.key(enter, None, false), Action::Continue),
                "queued confirmation before the first dialog frame must be ignored"
            );
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(rendered(&terminal).contains("Resize to review"));
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                    None,
                    false
                ),
                Action::Continue
            ));
            assert!(matches!(app.key(enter, None, false), Action::Continue));
            terminal.backend_mut().resize(80, 12);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(rendered(&terminal).contains("/fixture/unmodified-test-target"));
            assert!(
                matches!(app.key(enter, None, false), Action::Delete(_, _, _, _)),
                "confirmation becomes available after a usable resized dialog is drawn"
            );
        }
    }
    fn entry(id: NodeId, name: &str, kind: NodeKind, bytes: u64) -> crate::types::Entry {
        crate::types::Entry {
            id,
            name: name.into(),
            kind,
            sizes: crate::types::Sizes {
                apparent: bytes,
                allocated: bytes,
            },
            hardlink_duplicate: false,
            hardlink_group: None,
            error: None,
        }
    }

    fn apply(app: &mut App, event: ScanEvent) {
        let changed = app.model.apply(event);
        app.changed(changed);
    }

    fn wait_cache(app: &mut App, directory: NodeId, ids: &[NodeId]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.pump_order();
            if app
                .order
                .snapshot(directory)
                .is_some_and(|view| view.ids == ids)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "cached directory did not converge"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn browser_fixture() -> App {
        browser_fixture_at(PathBuf::from("fixture"))
    }

    fn browser_fixture_at(root: PathBuf) -> App {
        let mut app = App::new(root, SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Directory,
                sizes: Default::default(),
                error: None,
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "folder", NodeKind::Directory, 0),
                    entry(2, "root-file", NodeKind::File, 3),
                ],
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 1,
                entries: vec![
                    entry(3, "remove-me", NodeKind::File, 12),
                    entry(4, "survivor", NodeKind::File, 7),
                    entry(5, "child-folder", NodeKind::Directory, 0),
                ],
            },
        );
        app.started = true;
        app.page_size = 10;
        wait_cache(&mut app, ROOT, &[1, 2]);
        wait_cache(&mut app, 1, &[3, 4, 5]);
        app.sync_order(true);
        app.cursor_mode = CursorMode::Tracked;
        app
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn busy_queue_still_allows_reviewable_delete_confirmation() {
        let mut app = browser_fixture();
        app.selected = Some(2);
        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                Some(anchor),
                true
            ),
            Action::Continue
        ));
        assert!(matches!(app.modal, Some(Modal::Delete { id: 2, .. })));
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(
            app.modal,
            Some(Modal::Delete {
                confirmed: None,
                ..
            })
        ) {
            app.pump_confirmation();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                true
            ),
            Action::Delete(2, _, _, _)
        ));
    }

    #[test]
    fn quit_key_keeps_reviewability_and_warning_shows_live_delete_progress() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(app.key(quit, None, true), Action::QuitWarning));
        assert!(matches!(app.modal, Some(Modal::Quit { reviewable: false })));
        assert!(matches!(app.key(quit, None, true), Action::Continue));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 4)).unwrap();
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                true
            ),
            Action::Continue
        ));
        app.queue_view.status = "Deleting · 12 removed · 3 queued".to_owned();
        terminal.backend_mut().resize(80, 12);
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        let text = rendered(&terminal);
        assert!(text.contains("Deletions in progress"));
        assert!(text.contains("Finishing deletions. Scanning stopped."));
        assert!(text.contains("12 removed · 3 queued"));
        assert!(text.contains("Ctrl-C") && text.contains("cancel all and quit"));
        app.queue_view.status = "Deleting · 23 removed · 2 queued".to_owned();
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        assert!(rendered(&terminal).contains("23 removed · 2 queued"));
        assert!(matches!(app.key(quit, None, true), Action::Quit));
    }

    #[test]
    fn second_ctrl_c_quits_before_first_paint_and_in_tiny_terminals() {
        for dimensions in [None, Some((1, 1)), Some((80, 4))] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            app.modal = Some(Modal::Enqueueing);
            let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            assert!(matches!(
                app.key(interrupt, None, true),
                Action::QuitWarning
            ));
            assert!(matches!(app.modal, Some(Modal::Quit { reviewable: false })));
            assert!(matches!(app.previous_modal, Some(Modal::Enqueueing)));
            if let Some((width, height)) = dimensions {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
                assert!(matches!(app.modal, Some(Modal::Quit { reviewable: false })));
            }
            assert!(matches!(app.key(interrupt, None, true), Action::Quit));
        }
    }

    #[test]
    fn ctrl_c_remains_available_while_stopping() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        app.modal = Some(Modal::Stopping);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                None,
                true
            ),
            Action::Continue
        ));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                None,
                true
            ),
            Action::Quit
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queue_selection_follows_job_identity_after_reorder_and_neighbor_removal() {
        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
        let mut queue = DeleteQueue::new().unwrap();
        queue.toggle_pause();
        let first = queue
            .enqueue(1, Arc::clone(&anchor), PathBuf::from("unmodified-first"))
            .id;
        let selected = queue
            .enqueue(2, Arc::clone(&anchor), PathBuf::from("unmodified-selected"))
            .id;
        queue.enqueue(3, anchor, PathBuf::from("unmodified-last"));
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        app.modal = Some(Modal::Queue {
            selected: None,
            offset: None,
        });
        app.sync_queue(&queue);
        app.queue_navigate(&queue, None, 1);
        app.sync_queue(&queue);
        assert_eq!(app.queue_selection(), Some(selected));
        assert!(queue.move_pending(selected, 1));
        app.sync_queue(&queue);
        assert_eq!(app.queue_selection(), Some(selected));
        queue.remove_pending(first);
        app.sync_queue(&queue);
        assert_eq!(app.queue_selection(), Some(selected));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 18)).unwrap();
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        app.sync_queue(&queue);
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        assert!(rendered(&terminal).contains("Deletion queue"));
        assert!(rendered(&terminal).contains("unmodified-selected"));
    }

    #[test]
    fn refresh_keys_target_only_the_selected_folder_or_current_directory() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                None,
                false
            ),
            Action::Refresh(1)
        ));
        app.selected = Some(2);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                None,
                false
            ),
            Action::Continue
        ));
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::Refresh(1)
        ));
        assert_eq!(app.directory, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn single_target_capture_admission_survives_confirmation_cancellation() {
        let mut app = browser_fixture();
        app.selected = Some(2);
        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
        let blocked = Arc::new(AtomicBool::new(false));
        app.capture_inflight = Some(Arc::clone(&blocked));
        for _ in 0..8 {
            app.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                Some(Arc::clone(&anchor)),
                false,
            );
            assert!(matches!(
                app.modal,
                Some(Modal::Text {
                    title: "Stopping target",
                    ..
                })
            ));
            assert!(Arc::ptr_eq(
                app.capture_inflight.as_ref().unwrap(),
                &blocked
            ));
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        }
        blocked.store(true, Ordering::Release);
        app.pump_confirmation();
        app.key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            Some(anchor),
            false,
        );
        assert!(matches!(app.modal, Some(Modal::Delete { .. })));
        let worker = Arc::clone(app.capture_inflight.as_ref().unwrap());
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.modal.is_none());
        assert!(Arc::ptr_eq(app.capture_inflight.as_ref().unwrap(), &worker));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.capture_inflight.is_some() {
            app.pump_confirmation();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deferred_single_target_retry_keeps_its_target_and_respects_cancellation() {
        let mut app = browser_fixture();
        app.selected = Some(2);
        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
        let blocked = Arc::new(AtomicBool::new(false));
        app.capture_inflight = Some(Arc::clone(&blocked));
        app.key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            Some(Arc::clone(&anchor)),
            false,
        );
        assert_eq!(
            app.deferred_delete.as_ref().map(|request| request.id),
            Some(2)
        );
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.deferred_delete.is_none());
        app.key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            Some(anchor),
            false,
        );
        app.selected = Some(1);
        blocked.store(true, Ordering::Release);
        app.pump_confirmation();
        assert!(matches!(app.modal, Some(Modal::Delete { id: 2, .. })));
        assert!(app.deferred_delete.is_none());
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            None,
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.capture_inflight.is_some() {
            app.pump_confirmation();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.modal.is_none());
    }

    #[test]
    fn scan_focus_keeps_an_entered_unfinished_folder_after_selecting_a_file() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        assert_eq!(app.scan_focus(), Some(1));
        app.selected = Some(5);
        assert_eq!(app.scan_focus(), Some(5));
        apply(&mut app, ScanEvent::DirectoryDone { id: 5, error: None });
        apply(&mut app, ScanEvent::DirectoryDone { id: 1, error: None });
        assert_eq!(app.scan_focus(), None);
        app.directory = ROOT;
        app.selected = Some(2);
        assert_eq!(app.scan_focus(), None);
    }

    #[test]
    fn refreshed_directory_auto_selection_requests_native_priority_without_initial_scan_focus() {
        let mut app = browser_fixture();
        assert!(!app.prioritize_requested);
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.begin_model_refresh(1, 1);
        assert!(app.prioritize_requested);
        let changed = app.model.apply_scoped(
            1,
            1,
            ScanEvent::RootInfo {
                kind: NodeKind::Directory,
                sizes: Default::default(),
                error: None,
                file_identity: None,
            },
        );
        app.changed(changed);
        let changed = app.model.apply_scoped(
            1,
            1,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, "rediscovered", NodeKind::Directory, 1)],
            },
        );
        app.changed(changed);
        let rediscovered = (app.model.len() - 1) as NodeId;
        app.prioritize_requested = false;
        wait_cache(&mut app, 1, &[rediscovered]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(rediscovered));
        assert!(app.prioritize_requested);
        assert_eq!(app.scan_focus(), Some(rediscovered));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_batch_must_retire_before_another_capture_worker_is_admitted() {
        let mut app = browser_fixture();
        app.visual.enter(ROOT, Some(1));
        app.selected = Some(1);
        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
        let mut batch = None;
        let mut retired = VecDeque::new();
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::BatchPrepare
        ));
        prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
        assert!(batch.is_some());
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                None,
                false
            ),
            Action::BatchCancel
        ));
        cancel_batch(&mut app, &mut batch, &mut retired);
        assert_eq!(retired.len(), 1);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::BatchPrepare
        ));
        prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
        assert!(batch.is_none());
        assert!(matches!(
            app.modal,
            Some(Modal::Text {
                title: "Stopping selection",
                ..
            })
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !retired.is_empty() {
            pump_retired_batches(&mut app, &mut retired);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.deferred_batch.is_some());
        app.previous_modal = app.modal.take();
        app.modal = Some(Modal::Quit { reviewable: false });
        resume_batch(&mut app, &mut batch, &retired);
        assert!(batch.is_none() && app.deferred_batch.is_some());
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            None,
            true,
        );
        resume_batch(&mut app, &mut batch, &retired);
        assert!(batch.is_some() && app.deferred_batch.is_none());
        cancel_batch(&mut app, &mut batch, &mut retired);
        prepare_batch(&mut app, Some(anchor), &mut batch, &retired);
        assert!(app.deferred_batch.is_some());
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.deferred_batch.is_none());
        while !retired.is_empty() {
            pump_retired_batches(&mut app, &mut retired);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn background_delete_failure_preserves_batch_cancellation_and_retry() {
        std::fs::create_dir_all(".tmp").unwrap();
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let target = fixture.path().join("root-file");
        std::fs::write(&target, b"retained").unwrap();
        let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
        let scanner = scan::spawn(fixture.path().to_owned(), ScanOptions::default());
        let mut app = browser_fixture_at(fixture.path().to_owned());
        app.selected = Some(2);
        app.visual.enter(ROOT, Some(2));
        let mut queue = DeleteQueue::new().unwrap();
        queue.toggle_pause();
        let mut batch = None;
        let mut retired = VecDeque::new();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        for (generation, confirm) in [false, true, true].into_iter().enumerate() {
            app.visual.enter(ROOT, Some(2));
            prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
            assert!(batch.is_some());
            app.delete_failed(
                1,
                &io::Error::new(io::ErrorKind::PermissionDenied, "parent denies removal"),
            );
            assert!(matches!(app.modal, Some(Modal::Batch { .. })));
            assert_eq!(app.model.node(1).error_count, 1);
            app.begin_model_refresh(1, generation as u64 + 1);
            assert_eq!(app.model.node(1).error_count, 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !matches!(app.modal, Some(Modal::Batch { ready: true, .. })) {
                pump_batch(
                    &mut app,
                    &scanner,
                    &[],
                    &mut queue,
                    &mut batch,
                    &mut retired,
                );
                assert!(Instant::now() < deadline, "batch confirmation was lost");
                std::thread::sleep(Duration::from_millis(1));
            }
            terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
            let action = app.key(
                KeyEvent::new(
                    KeyCode::Char(if confirm { 'y' } else { 'n' }),
                    KeyModifiers::NONE,
                ),
                None,
                true,
            );
            if confirm {
                assert!(matches!(action, Action::BatchConfirm));
                batch.as_mut().unwrap().confirm_pending = true;
            } else {
                assert!(matches!(action, Action::BatchCancel));
                cancel_batch(&mut app, &mut batch, &mut retired);
            }
            // Admission owns the modal even before confirmation is pumped.
            assert_eq!(app.show_delete_notice(), !confirm);
            if generation == 2 {
                app.key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    None,
                    true,
                );
                assert!(matches!(app.modal, Some(Modal::Quit { .. })));
            }
            while batch.is_some() || !retired.is_empty() {
                pump_batch(
                    &mut app,
                    &scanner,
                    &[],
                    &mut queue,
                    &mut batch,
                    &mut retired,
                );
                pump_retired_batches(&mut app, &mut retired);
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            if generation == 2 {
                assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                app.key(
                    KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                    None,
                    true,
                );
            }
            assert_eq!(app.show_delete_notice(), confirm);
            assert!(
                matches!(&app.modal, Some(Modal::Text { title: "Delete failed", text, .. }) if text.contains("folder") && text.contains("parent denies removal"))
            );
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true);
            assert!(target.is_file());
        }
        assert_eq!(queue.pending().len(), 1);
        assert_eq!(queue.pending().front().unwrap().node, 2);
        queue.start_next();
        assert!(queue.active().is_none());
        queue.cancel_all();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quit_warning_retires_unconfirmed_batch_while_confirmed_queue_finishes() {
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        std::fs::create_dir(fixture.path().join("folder")).unwrap();
        std::fs::write(fixture.path().join("root-file"), b"confirmed").unwrap();
        let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
        let mut app = browser_fixture_at(fixture.path().to_owned());
        app.selected = Some(1);
        app.visual.enter(ROOT, Some(1));
        let mut batch = None;
        let mut retired = VecDeque::new();
        prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
        assert!(batch.is_some());
        let mut queue = DeleteQueue::new().unwrap();
        assert!(
            queue
                .enqueue(2, Arc::clone(&anchor), "root-file".into())
                .inserted
        );
        assert!(queue.start_next());
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                None,
                true
            ),
            Action::QuitWarning
        ));
        cancel_unconfirmed(&mut app, &mut batch, &mut retired);
        assert!(batch.is_none());
        assert!(matches!(app.modal, Some(Modal::Quit { .. })));
        assert!(app.previous_modal.is_none());
        assert_eq!(queue.active().unwrap().node, 2);
        assert!(!queue.is_cancelling());

        let mut completed = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.is_busy() || !retired.is_empty() {
            queue.pump(128);
            if let Some(completion) = queue.poll() {
                assert_eq!(completion.entry.node, 2);
                assert!(completion.result.outcome.is_ok());
                completed = true;
            }
            pump_retired_batches(&mut app, &mut retired);
            assert!(Instant::now() < deadline, "quit preparation did not retire");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(completed);
        assert!(
            !queue.is_busy() && batch.is_none(),
            "graceful closing must finish"
        );
        assert!(fixture.path().join("folder").is_dir());
        assert!(!fixture.path().join("root-file").exists());
        assert!(matches!(app.modal, Some(Modal::Quit { .. })));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quit_warning_preserves_confirmed_admission_and_clears_later_requests() {
        for dispatching in [false, true] {
            let fixture = tempfile::tempdir_in(".tmp").unwrap();
            std::fs::write(fixture.path().join("root-file"), b"retained").unwrap();
            let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
            let mut app = browser_fixture_at(fixture.path().to_owned());
            app.selected = Some(2);
            app.visual.enter(ROOT, Some(2));
            let mut batch = None;
            let mut retired = VecDeque::new();
            prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
            let preparation = batch.as_mut().unwrap();
            preparation.dispatching = dispatching;
            preparation.confirm_pending = !dispatching;
            app.modal = Some(Modal::Enqueueing);
            app.waiting_batch = Some(WaitingBatch {
                directory: ROOT,
                epoch: app.model.node(ROOT).child_epoch,
                anchor: Arc::clone(&anchor),
            });
            app.deferred_delete = Some(DeleteRequest {
                id: 2,
                anchor: Arc::clone(&anchor),
                relative: "root-file".into(),
                target: "root-file".to_owned(),
            });
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    None,
                    true
                ),
                Action::QuitWarning
            ));
            cancel_unconfirmed(&mut app, &mut batch, &mut retired);
            let preparation = batch.as_ref().expect("confirmed admission was cancelled");
            assert_eq!(preparation.dispatching, dispatching);
            assert_eq!(preparation.confirm_pending, !dispatching);
            assert!(retired.is_empty());
            assert!(app.waiting_batch.is_none() && app.deferred_delete.is_none());
            assert!(matches!(app.modal, Some(Modal::Quit { .. })));
            assert!(matches!(app.previous_modal, Some(Modal::Enqueueing)));
            cancel_batch(&mut app, &mut batch, &mut retired);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !retired.is_empty() {
                pump_retired_batches(&mut app, &mut retired);
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn confirmed_batch_admission_blocks_bypass_keys_and_guards_quit_immediately() {
        for confirm in [KeyCode::Char('y'), KeyCode::Enter] {
            let mut app = browser_fixture();
            app.visual.enter(ROOT, Some(2));
            app.modal = Some(Modal::Batch {
                target: "fixture".to_owned(),
                count: 1,
                ready: true,
                reviewable: true,
                scroll: 0,
            });
            assert!(matches!(
                app.key(KeyEvent::new(confirm, KeyModifiers::NONE), None, true),
                Action::BatchConfirm
            ));
            assert!(!app.visual.active());
            app.delete_notice = Some("preserved failure".to_owned());
            assert!(!app.show_delete_notice());
            for code in [
                KeyCode::Char('d'),
                KeyCode::Char('D'),
                KeyCode::Delete,
                KeyCode::Enter,
                KeyCode::Char('y'),
                KeyCode::Char('?'),
                KeyCode::Char('i'),
                KeyCode::Char('v'),
                KeyCode::Char('r'),
                KeyCode::Char('R'),
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Left,
                KeyCode::Right,
            ] {
                assert!(matches!(
                    app.key(KeyEvent::new(code, KeyModifiers::NONE), None, true),
                    Action::Continue
                ));
                assert!(matches!(app.modal, Some(Modal::Enqueueing)));
            }
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
            let text = rendered(&terminal);
            assert!(text.contains("Enqueueing selection"));
            assert!(text.contains("Queued deletions continue."));
            assert!(text.contains("stop adding"));
            app.key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                None,
                true,
            );
            assert!(matches!(app.modal, Some(Modal::Quit { .. })));
            assert!(matches!(app.previous_modal, Some(Modal::Enqueueing)));
            terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    None,
                    true,
                ),
                Action::Quit
            ));
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                None,
                true,
            );
            assert!(matches!(app.modal, Some(Modal::Enqueueing)));
            assert!(matches!(
                app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true),
                Action::BatchCancel
            ));
            assert!(app.modal.is_none());
            assert!(app.show_delete_notice());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn batch_final_events_after_empty_observation_reach_the_fifo() {
        std::fs::create_dir_all(".tmp").unwrap();
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
        let scanner = scan::spawn(fixture.path().to_owned(), ScanOptions::default());
        let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Directory,
                sizes: Default::default(),
                error: None,
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=3)
                    .map(|id| entry(id, &format!("item-{id}"), NodeKind::File, 1))
                    .collect(),
            },
        );
        app.modal = Some(Modal::Enqueueing);
        let mut queue = DeleteQueue::new().unwrap();
        queue.toggle_pause();
        let target = delete::capture(&anchor, std::path::Path::new("item-1")).unwrap();
        assert!(
            queue
                .enqueue_confirmed(1, Arc::clone(&anchor), "item-1".into(), target)
                .inserted
        );
        let worker = BatchCapture::finishing_after_empty(vec![
            BatchEvent::Entry {
                node: 2,
                relative: "item-2".into(),
                target,
            },
            BatchEvent::Entry {
                node: 3,
                relative: "item-3".into(),
                target,
            },
            BatchEvent::Done,
        ]);
        let mut batch = Some(BatchPreparation {
            worker,
            remaining: im::HashSet::new(),
            pending: None,
            directory: ROOT,
            epoch: app.model.node(ROOT).child_epoch,
            relative: PathBuf::new(),
            anchor,
            order: Arc::clone(&app.view),
            show_hidden: true,
            input_finished: true,
            dispatching: true,
            confirm_pending: false,
        });
        let mut retired = VecDeque::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while batch
            .as_ref()
            .is_some_and(|preparation| !preparation.worker.is_finished())
        {
            pump_batch(
                &mut app,
                &scanner,
                &[],
                &mut queue,
                &mut batch,
                &mut retired,
            );
            assert!(Instant::now() < deadline, "empty receive hook did not run");
        }
        let preparation = batch
            .as_ref()
            .expect("a stale empty receive must not retire the batch");
        assert!(preparation.worker.is_finished(), "empty receive hook ran");
        assert!(!preparation.worker.is_drained());
        assert_eq!(queue.pending().len(), 1);
        assert!(matches!(app.modal, Some(Modal::Enqueueing)));
        assert!(app.delete_notice.is_none());
        while batch.is_some() {
            pump_batch(
                &mut app,
                &scanner,
                &[],
                &mut queue,
                &mut batch,
                &mut retired,
            );
            assert!(Instant::now() < deadline, "final events were not admitted");
        }
        assert_eq!(
            queue
                .pending()
                .iter()
                .map(|entry| entry.node)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(app.modal.is_none());
        assert!(app.delete_notice.is_none());
        pump_retired_batches(&mut app, &mut retired);
        assert!(retired.is_empty());
        queue.cancel_all();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmed_batch_failure_survives_quit_and_is_shown_only_once_if_dismissed() {
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
        let scanner = scan::spawn(fixture.path().to_owned(), ScanOptions::default());
        for stopping in [false, true] {
            for disconnected in [false, true] {
                for confirm_pending in [false, true] {
                    let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
                    app.modal = Some(if stopping {
                        Modal::Stopping
                    } else {
                        Modal::Quit { reviewable: false }
                    });
                    app.previous_modal = Some(Modal::Enqueueing);
                    let message = if disconnected {
                        "Could not finish preparing selected deletions."
                    } else {
                        "Confirmed batch admission failed."
                    };
                    let events = if disconnected {
                        Vec::new()
                    } else {
                        vec![BatchEvent::Failed(message.to_owned())]
                    };
                    let mut batch = Some(BatchPreparation {
                        worker: BatchCapture::finishing_after_empty(events),
                        remaining: im::HashSet::new(),
                        pending: None,
                        directory: ROOT,
                        epoch: app.model.node(ROOT).child_epoch,
                        relative: PathBuf::new(),
                        anchor: Arc::clone(&anchor),
                        order: Arc::clone(&app.view),
                        show_hidden: true,
                        input_finished: true,
                        dispatching: !confirm_pending,
                        confirm_pending,
                    });
                    let mut queue = DeleteQueue::new().unwrap();
                    let mut retired = VecDeque::new();
                    for _ in 0..3 {
                        pump_batch(
                            &mut app,
                            &scanner,
                            &[],
                            &mut queue,
                            &mut batch,
                            &mut retired,
                        );
                    }
                    assert!(batch.is_none() && !queue.is_busy());
                    assert_eq!(app.delete_notice.as_deref(), Some(message));
                    assert!(app.previous_modal.is_none());
                    assert!(!app.show_delete_notice());
                    if stopping {
                        assert!(matches!(app.modal, Some(Modal::Stopping)));
                    } else {
                        assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
                        assert!(app.show_delete_notice());
                        assert!(matches!(
                            app.modal,
                            Some(Modal::Text {
                                title: "Delete failed",
                                ..
                            })
                        ));
                        assert!(app.delete_notice.is_none());
                        app.key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            None,
                            false,
                        );
                        assert!(app.modal.is_none() && !app.show_delete_notice());
                    }
                    pump_retired_batches(&mut app, &mut retired);
                    assert!(retired.is_empty());
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_batch_admission_keeps_fifo_prefix_cancellation_and_quit_state() {
        const COUNT: NodeId = 256;
        for outcome in 0..4 {
            std::fs::create_dir_all(".tmp").unwrap();
            let fixture = tempfile::tempdir_in(".tmp").unwrap();
            let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
            let scanner = scan::spawn(fixture.path().to_owned(), ScanOptions::default());
            let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: (1..=COUNT + 1)
                        .map(|id| {
                            entry(
                                id,
                                &format!("item-{id:05}"),
                                NodeKind::File,
                                u64::from(COUNT + 2 - id),
                            )
                        })
                        .collect(),
                },
            );
            let ids: Vec<_> = (1..=COUNT + 1).collect();
            wait_cache(&mut app, ROOT, &ids);
            app.sync_order(true);
            app.selected = Some(COUNT + 1);
            app.visual.enter(ROOT, Some(1));
            for id in 2..=COUNT {
                app.visual.toggle(id);
            }
            // Missing targets have valid identity snapshots; this test removes no files.
            let mut queue = DeleteQueue::new().unwrap();
            queue.toggle_pause();
            let mut batch = None;
            let mut retired = VecDeque::new();
            prepare_batch(&mut app, Some(Arc::clone(&anchor)), &mut batch, &retired);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !matches!(app.modal, Some(Modal::Batch { ready: true, .. })) {
                pump_batch(
                    &mut app,
                    &scanner,
                    &[],
                    &mut queue,
                    &mut batch,
                    &mut retired,
                );
                assert!(
                    Instant::now() < deadline,
                    "batch capture did not become ready"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    true
                ),
                Action::BatchConfirm
            ));
            batch.as_mut().unwrap().confirm_pending = true;
            while queue.pending().is_empty() {
                pump_batch(
                    &mut app,
                    &scanner,
                    &[],
                    &mut queue,
                    &mut batch,
                    &mut retired,
                );
                assert!(
                    Instant::now() < deadline,
                    "batch dispatch did not admit a prefix"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let prefix = queue.pending().len();
            assert!(prefix <= 128 && prefix < COUNT as usize);
            assert!(batch.is_some() && !app.visual.active());
            assert!(matches!(app.modal, Some(Modal::Enqueueing)));
            // The cursor is on an unselected target; it cannot overtake the producer.
            app.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                Some(Arc::clone(&anchor)),
                true,
            );
            assert!(matches!(app.modal, Some(Modal::Enqueueing)));
            if outcome == 1 || outcome == 2 {
                let key = if outcome == 1 {
                    KeyCode::Char('n')
                } else {
                    KeyCode::Esc
                };
                assert!(matches!(
                    app.key(KeyEvent::new(key, KeyModifiers::NONE), None, true),
                    Action::BatchCancel
                ));
                cancel_batch(&mut app, &mut batch, &mut retired);
            } else if outcome == 3 {
                app.key(
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                    None,
                    true,
                );
                assert!(matches!(app.previous_modal, Some(Modal::Enqueueing)));
            }
            while batch.is_some() || !retired.is_empty() {
                pump_batch(
                    &mut app,
                    &scanner,
                    &[],
                    &mut queue,
                    &mut batch,
                    &mut retired,
                );
                pump_retired_batches(&mut app, &mut retired);
                assert!(Instant::now() < deadline, "batch admission did not finish");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(!app.visual.active());
            if outcome == 1 || outcome == 2 {
                assert!(app.modal.is_none());
                assert_eq!(
                    queue.pending().len(),
                    prefix,
                    "cancellation keeps the admitted prefix"
                );
                queue.toggle_pause();
                assert!(
                    queue.start_next(),
                    "admitted work remains runnable after cancellation"
                );
                queue.cancel_all();
                continue;
            }
            assert_eq!(
                queue
                    .pending()
                    .iter()
                    .map(|entry| entry.node)
                    .collect::<Vec<_>>(),
                ids[..COUNT as usize]
            );
            if outcome == 3 {
                assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                assert!(
                    app.previous_modal.is_none(),
                    "completed admission must not reappear"
                );
                app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true);
            }
            assert!(app.modal.is_none());
            app.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                Some(Arc::clone(&anchor)),
                true,
            );
            while !matches!(
                app.modal,
                Some(Modal::Delete {
                    confirmed: Some(_),
                    ..
                })
            ) {
                app.pump_confirmation();
                assert!(
                    Instant::now() < deadline,
                    "later target capture did not finish"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
            let Action::Delete(node, anchor, relative, target) = app.key(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                None,
                true,
            ) else {
                panic!("later target confirmation was not admitted");
            };
            queue.enqueue_confirmed(node, anchor, relative, target);
            assert_eq!(
                queue
                    .pending()
                    .iter()
                    .map(|entry| entry.node)
                    .collect::<Vec<_>>(),
                ids
            );
            queue.cancel_all();
        }
    }

    #[test]
    fn deferred_delete_notice_keeps_latest_failure_and_survives_other_removal() {
        let mut app = browser_fixture();
        app.modal = Some(Modal::help(app.visual.active()));
        app.delete_failed(2, &io::Error::other("first failure"));
        app.delete_failed(1, &io::Error::other("latest failure"));
        app.removed(2);
        assert!(matches!(app.modal, Some(Modal::Text { title: "Help", .. })));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(app.show_delete_notice());
        assert!(
            matches!(&app.modal, Some(Modal::Text { text, .. }) if text.contains("latest failure") && !text.contains("first failure"))
        );
        assert!(app.delete_notice.is_none());
        app.removed(5);
        assert!(matches!(
            app.modal,
            Some(Modal::Text {
                title: "Delete failed",
                ..
            })
        ));
    }

    #[test]
    fn visual_shift_arrows_extend_and_enter_toggles_before_batch_confirmation() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        app.key(
            KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT),
            None,
            false,
        );
        assert!(app.visual.contains(1));
        app.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            None,
            false,
        );
        while app.visual.is_pending() {
            app.pump_visual();
        }
        assert!(app.visual.contains(1) && app.visual.contains(2));
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.visual.contains(1) && !app.visual.contains(2));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::BatchPrepare
        ));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(!app.visual.active());
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::Continue
        ));
        assert!(matches!(app.modal, Some(Modal::Queue { .. })));
    }

    #[test]
    fn visual_aliases_mark_and_batch_without_affecting_normal_queue_keys() {
        for enter in ['v', 'V'] {
            for delete in [KeyCode::Char('d'), KeyCode::Char('D'), KeyCode::Delete] {
                let mut app = browser_fixture();
                app.selected = Some(1);
                app.key(
                    KeyEvent::new(KeyCode::Char(enter), KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(app.visual.active() && app.visual.contains(1));
                assert!(matches!(
                    app.key(KeyEvent::new(delete, KeyModifiers::NONE), None, false),
                    Action::BatchPrepare
                ));
                app.key(
                    KeyEvent::new(KeyCode::Char(enter), KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(!app.visual.active());
                app.key(
                    KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(matches!(app.modal, Some(Modal::Queue { .. })));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_batch_waits_for_prior_input_and_supports_cancel_epoch_and_quit() {
        for delete in [KeyCode::Char('d'), KeyCode::Char('D'), KeyCode::Delete] {
            for outcome in 0..5 {
                let mut app = pending_folder_selection();
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(matches!(
                    app.key(KeyEvent::new(delete, KeyModifiers::NONE), None, false),
                    Action::BatchPrepare
                ));
                let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
                let mut batch = None;
                let mut retired = VecDeque::new();
                prepare_batch(&mut app, Some(anchor), &mut batch, &retired);
                assert!(app.waiting_batch.is_some() && batch.is_none());
                assert!(matches!(
                    app.modal,
                    Some(Modal::Text {
                        title: "Selecting entries",
                        ..
                    })
                ));
                // A premature confirmation key must not cancel or authorize a batch.
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(app.waiting_batch.is_some() && batch.is_none());
                match outcome {
                    1 | 2 => {
                        let key = if outcome == 1 {
                            KeyCode::Esc
                        } else {
                            KeyCode::Char('n')
                        };
                        app.key(KeyEvent::new(key, KeyModifiers::NONE), None, false);
                        assert!(app.waiting_batch.is_none());
                    }
                    3 => {
                        app.model.begin_rescan(app.directory, 1);
                        resume_batch(&mut app, &mut batch, &retired);
                        assert!(app.waiting_batch.is_none() && batch.is_none());
                        assert!(matches!(
                            app.modal,
                            Some(Modal::Text {
                                title: "Selection changed",
                                ..
                            })
                        ));
                        continue;
                    }
                    4 => {
                        app.key(
                            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                            None,
                            true,
                        );
                        finish_visual(&mut app);
                        resume_batch(&mut app, &mut batch, &retired);
                        assert!(app.waiting_batch.is_some() && batch.is_none());
                        assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true);
                    }
                    _ => {}
                }
                finish_visual(&mut app);
                resume_batch(&mut app, &mut batch, &retired);
                assert!(app.visual.contains(3) && !app.visual.contains(4));
                if outcome == 1 || outcome == 2 {
                    assert!(batch.is_none() && app.modal.is_none());
                } else {
                    let preparation = batch.as_ref().expect("queued batch starts automatically");
                    assert_eq!(preparation.remaining.len(), 1);
                    assert!(preparation.remaining.contains(&3));
                    assert!(!preparation.confirm_pending);
                    cancel_batch(&mut app, &mut batch, &mut retired);
                }
            }
        }
    }

    #[test]
    fn queued_opposite_navigation_keeps_boundary_clamping_order() {
        for (start, first, second, expected) in [
            (1, KeyCode::Up, KeyCode::Down, 2),
            (2, KeyCode::Down, KeyCode::Up, 1),
        ] {
            let mut app = browser_fixture();
            app.selected = Some(start);
            app.visual.enter(ROOT, Some(start));
            app.visual
                .begin_range(Arc::clone(&app.view), start, RangeMode::Add);
            for key in [first, second] {
                app.key(KeyEvent::new(key, KeyModifiers::SHIFT), None, false);
            }
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(expected));
            assert!(app.visual.contains(1) && app.visual.contains(2));
        }
    }

    #[test]
    fn deferred_enter_finishes_range_then_toggles_the_stable_endpoint() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        app.visual.enter(ROOT, Some(1));
        app.visual
            .begin_range(Arc::clone(&app.view), 2, RangeMode::Add);
        app.selected = Some(2);
        assert!(app.visual.is_pending());
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(matches!(
            app.visual_intents.front(),
            Some(VisualIntent::ToggleStable(2))
        ));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
                None,
                false
            ),
            Action::BatchPrepare
        ));
        app.pump_visual();
        assert!(app.visual.contains(1) && !app.visual.contains(2));
        assert!(app.visual_intents.is_empty());
    }

    #[test]
    fn newly_discovered_rows_preserve_an_explicit_keyboard_deselection() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Directory,
                sizes: Default::default(),
                error: None,
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "first", NodeKind::File, 20),
                    entry(2, "second", NodeKind::File, 10),
                ],
            },
        );
        app.page_size = 10;
        wait_cache(&mut app, ROOT, &[1, 2]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(1));
        for code in [KeyCode::Char('v'), KeyCode::Enter, KeyCode::Down] {
            let modifiers = if code == KeyCode::Down {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            app.key(KeyEvent::new(code, modifiers), None, false);
        }
        finish_visual(&mut app);
        assert!(!app.visual.contains(1) && app.visual.contains(2));

        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(3, "new-largest", NodeKind::File, 30)],
            },
        );
        wait_cache(&mut app, ROOT, &[3, 1, 2]);
        app.sync_order(true);
        app.key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT),
            None,
            false,
        );
        finish_visual(&mut app);
        assert_eq!(app.selected, Some(3));
        assert!(app.visual.contains(2) && app.visual.contains(3));
        assert!(
            !app.visual.contains(1),
            "discovering a row must not undo an explicit keyboard deselection"
        );
        assert_eq!(app.visual.len(), 2);
    }

    #[test]
    fn keyboard_continuation_keeps_an_inverse_ranges_deleted_anchor() {
        for pending in [false, true] {
            for backwards in [false, true] {
                for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
                    let mut app = browser_fixture();
                    app.directory = 1;
                    app.selected = Some(4);
                    app.reset_order(false);
                    app.visual.enter(1, None);
                    for id in 3..=5 {
                        app.visual.toggle(id);
                    }
                    let anchor = if backwards { 5 } else { 3 };
                    app.visual.reanchor(anchor);
                    app.visual
                        .begin_range(Arc::clone(&app.view), 4, RangeMode::Invert);
                    if pending {
                        app.visual.pump(1);
                    } else {
                        finish_visual(&mut app);
                    }
                    app.removed(anchor);
                    let key = if backwards {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    };
                    app.key(KeyEvent::new(key, modifiers), None, false);
                    finish_visual(&mut app);
                    assert_eq!(app.selected, Some(if backwards { 3 } else { 5 }));
                    for id in 3..=5 {
                        assert_eq!(
                            app.visual.contains(id),
                            id != anchor
                                && (modifiers == KeyModifiers::SHIFT
                                    || id == if backwards { 3 } else { 5 })
                        );
                    }
                    assert_eq!(
                        app.visual.len(),
                        if modifiers == KeyModifiers::SHIFT {
                            2
                        } else {
                            1
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn paired_pending_enters_preserve_the_next_range_anchor() {
        for queued_cursor in [false, true] {
            let mut app = pending_folder_selection();
            if queued_cursor {
                app.key(
                    KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
                    None,
                    false,
                );
            }
            for code in [KeyCode::Enter, KeyCode::Enter, KeyCode::Up] {
                let modifiers = if code == KeyCode::Up {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                app.key(KeyEvent::new(code, modifiers), None, false);
            }
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(if queued_cursor { 4 } else { 3 }));
            let marked: Vec<_> = (3..=5).filter(|&id| app.visual.contains(id)).collect();
            assert_eq!(
                marked,
                if queued_cursor {
                    vec![3, 4, 5]
                } else {
                    vec![3, 4]
                }
            );
        }
    }

    fn finish_visual(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.visual_busy() {
            app.sync_order(true);
            app.pump_visual();
            assert!(Instant::now() < deadline, "visual input did not converge");
        }
    }

    fn pending_folder_selection() -> App {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(4);
        app.reset_order(false);
        app.visual.enter(1, Some(3));
        app.visual
            .begin_range(Arc::clone(&app.view), 4, RangeMode::Add);
        app
    }

    #[test]
    fn pending_visual_inputs_preserve_toggle_navigation_and_click_order() {
        for pending_navigation in [false, true] {
            let mut app = pending_folder_selection();
            if pending_navigation {
                app.selected = Some(3);
                app.pending_navigation = Some(Navigation::new(None, 1));
                app.range_requested = true;
            }
            for code in [KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
                let modifiers = if code == KeyCode::Down {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                app.key(KeyEvent::new(code, modifiers), None, false);
            }
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(5));
            assert!(app.visual.contains(3) && !app.visual.contains(4) && !app.visual.contains(5));
        }
        let mut app = pending_folder_selection();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        finish_visual(&mut app);
        assert!(app.visual.is_empty());
        assert_eq!(app.selected, Some(3));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_visual_cursor_changes_block_target_actions_but_stable_toggles_do_not() {
        for code in [
            KeyCode::Right,
            KeyCode::Char('l'),
            KeyCode::Char('r'),
            KeyCode::Char('i'),
        ] {
            for kind in 0..4 {
                let mut app = pending_folder_selection();
                app.selected = Some(5);
                match kind {
                    0 => {
                        app.key(
                            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                            None,
                            false,
                        );
                    }
                    1 => app.enqueue_visual(VisualIntent::Mouse(PendingMouse {
                        anchor: 3,
                        lead: 3,
                        dragged: false,
                        released: true,
                        mode: RangeMode::Add,
                        edge: 0,
                        view: Arc::clone(&app.view),
                        offset: app.offset,
                    })),
                    2 => app.enqueue_visual(VisualIntent::CursorId(3)),
                    _ => app.enqueue_visual(VisualIntent::Mouse(PendingMouse {
                        anchor: 3,
                        lead: 4,
                        dragged: false,
                        released: true,
                        mode: RangeMode::Invert,
                        edge: 0,
                        view: Arc::clone(&app.view),
                        offset: app.offset,
                    })),
                }
                assert!(app.pending_navigation.is_none() && app.cursor_pending());
                let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
                assert!(matches!(
                    app.key(KeyEvent::new(code, KeyModifiers::NONE), Some(anchor), false),
                    Action::Continue
                ));
                assert_eq!(app.directory, 1);
                assert!(app.modal.is_none());
                assert!(app.cursor_pending());
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(if kind == 3 { 4 } else { 3 }));
            }
        }
        let mut app = pending_folder_selection();
        app.selected = Some(5);
        app.enqueue_visual(VisualIntent::ToggleStable(3));
        app.key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, 5);
    }

    #[test]
    fn pending_edge_and_relative_navigation_toggle_the_final_destination() {
        for (edge, movement, selected, marked) in [
            (KeyCode::Home, KeyCode::Down, 4, false),
            (KeyCode::End, KeyCode::Up, 4, false),
        ] {
            let mut app = pending_folder_selection();
            for code in [edge, movement, KeyCode::Enter] {
                app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
            }
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(selected));
            assert_eq!(app.visual.contains(selected), marked);
        }
        let mut app = pending_folder_selection();
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Enter] {
            app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
        }
        finish_visual(&mut app);
        assert!(app.visual.contains(3) && !app.visual.contains(4));
        assert!(!app.range_requested);
    }

    #[test]
    fn deferred_toggles_never_retarget_a_removed_cursor() {
        for clicked in [false, true] {
            let mut app = pending_folder_selection();
            if clicked {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                app.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 35,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                });
            }
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
            app.removed(4);
            finish_visual(&mut app);
            assert!(
                app.visual.contains(3),
                "a removed target toggled its neighbour"
            );
            assert!(!app.visual.contains(4));
        }
        for published_empty in [false, true] {
            let mut app = pending_folder_selection();
            app.removed(3);
            app.removed(4);
            app.removed(5);
            if published_empty {
                wait_cache(&mut app, 1, &[]);
                app.sync_order(true);
            }
            app.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, 1), true));
            app.enqueue_visual(VisualIntent::ToggleCurrent);
            finish_visual(&mut app);
            assert!(app.visual_intents.is_empty() && !app.range_requested);
        }
    }

    #[test]
    fn deferred_mouse_focus_requests_priority_for_click_and_drag() {
        for dragged in [false, true] {
            let mut app = browser_fixture();
            app.selected = Some(2);
            app.visual.enter(ROOT, Some(1));
            app.visual
                .begin_range(Arc::clone(&app.view), 2, RangeMode::Add);
            app.prioritize_requested = false;
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let mouse = |kind, row| MouseEvent {
                kind,
                column: 35,
                row,
                modifiers: KeyModifiers::SHIFT,
            };
            app.mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                if dragged { 2 } else { 1 },
            ));
            if dragged {
                app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 1));
            }
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1));
            assert!(!app.prioritize_requested);
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(1));
            assert_eq!(app.scan_focus(), Some(1));
            assert!(app.prioritize_requested);
        }
    }

    #[test]
    fn completed_mouse_gestures_keep_their_order_and_held_drag_waits_under_help() {
        let mut app = pending_folder_selection();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind, row| MouseEvent {
            kind,
            column: 35,
            row,
            modifiers: KeyModifiers::SHIFT,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
        finish_visual(&mut app);
        assert_eq!(app.visual.len(), 1);
        assert!(app.visual.contains(5));
        let mut app = pending_folder_selection();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
        app.key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            None,
            false,
        );
        app.pump_visual();
        assert!(app.visual.contains(3) && app.visual.contains(4));
        assert!(app.has_held_mouse());
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
        assert!(!app.has_held_mouse());
        assert!(app.visual.contains(3) && app.visual.contains(4));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        finish_visual(&mut app);
    }

    #[test]
    fn visual_admission_is_bounded_and_keyboard_finishes_a_held_gesture() {
        let mut app = pending_folder_selection();
        for index in 0..VISUAL_INTENT_LIMIT {
            app.enqueue_visual(VisualIntent::ToggleStable(if index % 2 == 0 {
                3
            } else {
                4
            }));
        }
        assert!(!app.visual_input_available());
        finish_visual(&mut app);
        assert!(app.visual_input_available() && app.visual.contains(3) && app.visual.contains(4));
        let mut app = pending_folder_selection();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 3,
            modifiers: KeyModifiers::SHIFT,
        });
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(!app.has_held_mouse());
        finish_visual(&mut app);
        assert!(app.visual.contains(3) && app.visual.contains(4) && !app.visual.contains(5));
    }

    #[test]
    fn full_visual_queue_rejects_overflow_and_preserves_coalescing() {
        for hover in [false, true] {
            let mut app = pending_folder_selection();
            for index in 0..VISUAL_INTENT_LIMIT - 1 {
                app.enqueue_visual(VisualIntent::ToggleStable(if index % 2 == 0 {
                    3
                } else {
                    4
                }));
            }
            if hover {
                app.enqueue_visual(VisualIntent::CursorId(3));
            } else {
                app.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, 1), false));
            }
            assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
            assert!(!app.visual_input_available());
            app.enqueue_visual(VisualIntent::ToggleStable(5));
            assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
            for (index, intent) in app
                .visual_intents
                .iter()
                .take(VISUAL_INTENT_LIMIT - 1)
                .enumerate()
            {
                assert!(matches!(intent, VisualIntent::ToggleStable(id)
                    if *id == if index % 2 == 0 { 3 } else { 4 }));
            }
            if hover {
                app.enqueue_visual(VisualIntent::CursorId(5));
                assert!(matches!(
                    app.visual_intents.back(),
                    Some(VisualIntent::CursorId(5))
                ));
            } else {
                app.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, 2), false));
                app.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, -1), false));
                assert!(
                    matches!(app.visual_intents.back(), Some(VisualIntent::Navigate(navigation, false))
                    if navigation.amount == 3 && navigation.end.is_none())
                );
            }
            assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(rendered(&terminal).contains("Input full"));
        }
    }

    #[test]
    fn full_visual_queue_keeps_interrupt_escape_and_release_reachable() {
        for deleting in [false, true] {
            let mut app = pending_folder_selection();
            for _ in 0..VISUAL_INTENT_LIMIT {
                app.enqueue_visual(VisualIntent::ToggleCurrent);
            }
            assert!(!app.visual_input_available());
            let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let action = app.key(interrupt, None, deleting);
            if deleting {
                assert!(matches!(action, Action::QuitWarning));
                assert!(matches!(app.key(interrupt, None, true), Action::Quit));
            } else {
                assert!(matches!(action, Action::Quit));
            }
        }
        let mut app = pending_folder_selection();
        app.enqueue_visual(VisualIntent::Mouse(PendingMouse {
            anchor: 3,
            lead: 4,
            dragged: false,
            released: false,
            mode: RangeMode::Invert,
            edge: 0,
            view: Arc::clone(&app.view),
            offset: app.offset,
        }));
        for _ in 1..VISUAL_INTENT_LIMIT {
            app.enqueue_visual(VisualIntent::ToggleCurrent);
        }
        assert!(!app.visual_input_available() && app.has_held_mouse());
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 35,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.has_held_mouse());
        assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(!app.visual.active() && app.visual.is_empty());
        assert!(app.visual_intents.is_empty() && app.visual_input_available());
    }

    #[test]
    fn scope_retirement_preserves_rejected_ownership_and_transfers_completed_handles() {
        let handle = scan::spawn(
            PathBuf::from(".tmp/tui-retirement-absent"),
            ScanOptions::default(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            while handle.receiver.try_recv().is_ok() {}
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        while handle.receiver.try_recv().is_ok() {}
        let cancelled = Arc::clone(&handle.cancel);
        let mut scopes = vec![ScopedScan {
            scope: ROOT,
            generation: 1,
            relative: PathBuf::new(),
            handle,
            entries: None,
            finished: false,
        }];
        retire_scopes_with(&mut scopes, false, |_| {
            panic!("unprocessed completion was retired")
        });
        assert_eq!(scopes.len(), 1);
        scopes[0].finished = true;
        retire_scopes_with(&mut scopes, false, Err);
        assert_eq!(scopes.len(), 1);
        assert!(!cancelled.load(Ordering::Acquire));
        let retirer = Retirer::new().unwrap();
        retire_scopes_with(&mut scopes, false, |ready| retirer.try_retire(ready));
        assert!(scopes.is_empty());
        while !cancelled.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn mouse_ranges_preserve_click_drag_modifiers_and_release_endpoints() {
        for pending in [false, true] {
            for (shift, start, middle, end, expected) in [
                (false, 2, None, 2, vec![3, 4]),
                (false, 2, None, 3, vec![3, 4, 5]),
                (true, 1, None, 2, vec![4]),
                (true, 3, None, 3, vec![3, 4, 5]),
                (true, 1, Some(2), 3, vec![4, 5]),
            ] {
                let mut app = browser_fixture();
                app.directory = 1;
                app.selected = Some(3);
                app.reset_order(false);
                app.visual.enter(1, Some(3));
                if pending {
                    app.visual
                        .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
                }
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let event = |kind, row, modifiers| MouseEvent {
                    kind,
                    row,
                    column: 35,
                    modifiers,
                };
                app.mouse(event(
                    MouseEventKind::Down(MouseButton::Left),
                    start,
                    if shift {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                ));
                if let Some(row) = middle {
                    app.mouse(event(
                        MouseEventKind::Drag(MouseButton::Left),
                        row,
                        KeyModifiers::NONE,
                    ));
                }
                app.mouse(event(
                    MouseEventKind::Up(MouseButton::Left),
                    end,
                    KeyModifiers::NONE,
                ));
                finish_visual(&mut app);
                let marked: Vec<_> = (3..=5).filter(|&id| app.visual.contains(id)).collect();
                assert_eq!(
                    marked, expected,
                    "pending={pending}, shift={shift}, rows={start}..{end}"
                );
                assert!(app.mouse_press_view.is_none() && app.drag_scroll.is_none());
            }
        }
    }

    #[test]
    fn mouse_press_baselines_discard_deleted_ids_before_first_drag() {
        for deferred in [false, true] {
            for shift in [false, true] {
                let mut app = browser_fixture();
                app.directory = 1;
                app.selected = Some(5);
                app.reset_order(false);
                app.visual.enter(1, Some(5));
                if deferred {
                    app.visual
                        .begin_range(Arc::clone(&app.view), 5, RangeMode::Add);
                }
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let mouse = |kind, row| MouseEvent {
                    kind,
                    row,
                    column: 35,
                    modifiers: if shift {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                };
                app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
                app.pump_visual();
                if let Some(baseline) = &app.mouse_baseline {
                    assert!(baseline.contains(&5));
                }
                app.removed(5);
                app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
                app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(4));
                assert_eq!(app.visual.len(), 2, "deferred={deferred}, shift={shift}");
                assert!(app.visual.contains(3) && app.visual.contains(4));
                assert!(!app.visual.contains(5));
            }
        }
    }

    #[test]
    fn deferred_drags_keep_surviving_rows_when_either_endpoint_is_deleted() {
        for shift in [false, true] {
            for released in [false, true] {
                for removed in [3, 5] {
                    let mut app = browser_fixture();
                    app.directory = 1;
                    app.selected = Some(3);
                    app.reset_order(false);
                    app.visual.enter(1, None);
                    if shift {
                        for id in 3..=5 {
                            app.visual.toggle(id);
                        }
                    }
                    app.visual.reanchor(3);
                    app.visual
                        .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    let mouse = |kind, row| MouseEvent {
                        kind,
                        row,
                        column: 35,
                        modifiers: if shift {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    };
                    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
                    app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
                    if released {
                        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
                    }
                    assert_eq!(app.visual_intents.len(), 1);
                    app.removed(removed);
                    app.pump_visual();
                    if !released {
                        assert!(app.drag_started && app.mouse_dragging);
                        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
                    }
                    finish_visual(&mut app);
                    for id in 3..=5 {
                        assert_eq!(
                            app.visual.contains(id),
                            id != removed && !shift,
                            "shift={shift}, released={released}, removed={removed}, id={id}"
                        );
                    }
                    assert!(app.selected.is_some_and(|id| app.eligible(id)));
                }
            }
        }
    }

    #[test]
    fn shift_click_keeps_surviving_interval_without_a_preexisting_range() {
        for toggled in [false, true] {
            for backwards in [false, true] {
                for deferred in [false, true] {
                    for prior_range in [false, true] {
                        for remove_anchor in [true, false] {
                            let mut app = browser_fixture();
                            let anchor = if backwards { 5 } else { 3 };
                            let lead = if backwards { 3 } else { 5 };
                            app.directory = 1;
                            app.selected = Some(anchor);
                            app.reset_order(false);
                            app.visual.enter(1, Some(anchor));
                            if toggled {
                                app.visual.toggle(anchor);
                            }
                            if prior_range {
                                app.visual.begin_range(
                                    Arc::clone(&app.view),
                                    anchor,
                                    RangeMode::Add,
                                );
                                finish_visual(&mut app);
                            }
                            assert!(!app.visual.is_pending());
                            let mut terminal =
                                Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                            terminal
                                .draw(|frame| app.draw(frame, false, false))
                                .unwrap();
                            if deferred {
                                app.enqueue_visual(VisualIntent::CursorId(anchor));
                            }
                            let mouse = |kind| MouseEvent {
                                kind,
                                row: if backwards { 1 } else { 3 },
                                column: 35,
                                modifiers: KeyModifiers::SHIFT,
                            };
                            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
                            app.pump_visual();
                            let removed = if remove_anchor { anchor } else { lead };
                            app.removed(removed);
                            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
                            finish_visual(&mut app);
                            for id in 3..=5 {
                                assert_eq!(
                                    app.visual.contains(id),
                                    id != removed,
                                    "toggled={toggled}, backwards={backwards}, deferred={deferred}, prior_range={prior_range}, removed={removed}, id={id}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shift_click_extends_a_drag_whose_pressed_anchor_was_deleted() {
        for deferred in [false, true] {
            for shift in [false, true] {
                let mut app = browser_fixture();
                apply(
                    &mut app,
                    ScanEvent::Entries {
                        parent: 1,
                        entries: (6..=8)
                            .map(|id| entry(id, &format!("item-{id}"), NodeKind::File, 0))
                            .collect(),
                    },
                );
                app.directory = 1;
                app.selected = Some(4);
                app.reset_order(false);
                let make_view = |ids: Vec<NodeId>| {
                    Arc::new(CachedSnapshot {
                        generation: app.view.generation,
                        child_epoch: app.model.node(1).child_epoch,
                        positions: ids.iter().enumerate().map(|(i, &id)| (id, i)).collect(),
                        ids,
                    })
                };
                let original = make_view(vec![3, 4, 5, 6, 7, 8]);
                let newer = make_view(vec![3, 6, 5, 7, 8]);
                app.view = original;
                app.offset = 1;
                // This fixture starts with an already scrolled four-row viewport.
                app.page_size = 4;
                app.visual.enter(1, None);
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 5)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.painted_rows, vec![4, 5, 6, 7]);
                if deferred {
                    app.enqueue_visual(VisualIntent::CursorId(4));
                }
                let mouse = |kind, row, modifiers| MouseEvent {
                    kind,
                    row,
                    column: 35,
                    modifiers,
                };
                app.mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    1,
                    if shift {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                ));
                if !deferred {
                    app.removed(4);
                }
                app.mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    3,
                    KeyModifiers::NONE,
                ));
                app.mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    3,
                    KeyModifiers::NONE,
                ));
                if deferred {
                    app.removed(4);
                }
                finish_visual(&mut app);
                assert!(app.visual.contains(5) && app.visual.contains(6));
                assert!(!app.visual.contains(4));
                app.view = newer;
                app.offset = 1;
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.painted_rows, vec![6, 5, 7, 8]);
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    app.mouse(mouse(kind, 4, KeyModifiers::SHIFT));
                }
                finish_visual(&mut app);
                for id in 3..=8 {
                    assert_eq!(
                        app.visual.contains(id),
                        id >= 5,
                        "deferred={deferred}, shift={shift}, id={id}"
                    );
                }
            }
        }
    }

    #[test]
    fn deferred_shift_click_keeps_its_range_after_either_endpoint_is_deleted() {
        for removed in [3, 5] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.selected = Some(3);
            app.reset_order(false);
            app.visual.enter(1, Some(3));
            app.visual
                .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.mouse(MouseEvent {
                    kind,
                    row: 3,
                    column: 35,
                    modifiers: KeyModifiers::SHIFT,
                });
            }
            assert_eq!(app.visual_intents.len(), 1);
            app.removed(removed);
            finish_visual(&mut app);
            for id in 3..=5 {
                assert_eq!(app.visual.contains(id), id != removed);
            }
        }
    }

    #[test]
    fn mouse_presses_freeze_painted_order_and_scrolled_viewport() {
        for deferred in [false, true] {
            for shift in [false, true] {
                for offset in [0, 1] {
                    let mut app = browser_fixture();
                    apply(
                        &mut app,
                        ScanEvent::Entries {
                            parent: 1,
                            entries: vec![
                                entry(6, "six", NodeKind::File, 2),
                                entry(7, "seven", NodeKind::File, 1),
                            ],
                        },
                    );
                    app.directory = 1;
                    app.reset_order(false);
                    let make_view = |ids: Vec<NodeId>| {
                        Arc::new(CachedSnapshot {
                            generation: app.view.generation,
                            child_epoch: app.model.node(1).child_epoch,
                            positions: ids.iter().enumerate().map(|(i, &id)| (id, i)).collect(),
                            ids,
                        })
                    };
                    let original = make_view(vec![3, 4, 5, 6, 7]);
                    let mut ids = original.ids.clone();
                    ids.swap(offset + 1, offset + 2);
                    let newer = make_view(ids);
                    let anchor = original.ids[offset];
                    let lead = original.ids[offset + 1];
                    let untouched = original.ids[offset + 2];
                    app.view = Arc::clone(&original);
                    app.offset = offset;
                    // Preserve the prepared viewport before freezing its physical rows.
                    app.page_size = 3;
                    app.selected = Some(anchor);
                    app.visual.enter(1, Some(anchor));
                    if deferred {
                        app.visual
                            .begin_range(Arc::clone(&original), anchor, RangeMode::Add);
                    }
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(80, 4)).unwrap();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    let painted_rows = app.painted_rows.clone();
                    app.view = newer;
                    app.offset = 1 - offset;
                    let mouse = |kind, row| MouseEvent {
                        kind,
                        row,
                        column: 35,
                        modifiers: if shift {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    };
                    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
                    for _ in 0..2 {
                        terminal
                            .draw(|frame| app.draw(frame, false, false))
                            .unwrap();
                        assert!(Arc::ptr_eq(&app.painted_view, &original));
                        assert_eq!(app.painted_offset, offset);
                        assert_eq!(app.painted_rows, painted_rows);
                        app.pump_visual();
                    }
                    app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
                    app.pump_visual();
                    app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
                    finish_visual(&mut app);
                    assert_eq!(app.visual.contains(anchor), !shift);
                    assert!(app.visual.contains(lead));
                    assert!(!app.visual.contains(untouched));
                    assert_eq!(app.selected, Some(lead));
                }
            }
        }
    }

    #[test]
    fn queued_press_freezes_presentation_without_retargeting_earlier_navigation() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.visual.enter(1, Some(3));
        let original = Arc::clone(&app.view);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let newer = Arc::new(CachedSnapshot {
            generation: original.generation,
            child_epoch: original.child_epoch,
            ids: vec![3, 5, 4],
            positions: HashMap::from([(3, 0), (5, 1), (4, 2)]),
        });
        app.view = Arc::clone(&newer);
        app.pending_navigation = Some(Navigation {
            end: None,
            amount: 1,
            cursor: Some(NavigationCursor {
                next: Some(1),
                remaining: 1,
                target: Some(3),
                baseline: false,
            }),
        });
        let mouse = |kind| MouseEvent {
            kind,
            row: 2,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(Arc::ptr_eq(&app.view, &newer));
        assert!(Arc::ptr_eq(&app.painted_view, &original));
        assert_eq!(app.painted_rows, vec![3, 4, 5]);
        app.advance_navigation();
        assert_eq!(app.selected, Some(5));
        app.pump_visual();
        assert_eq!(app.selected, Some(4));
        assert!(Arc::ptr_eq(&app.view, &original));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        finish_visual(&mut app);
        assert!(app.visual.contains(3) && app.visual.contains(4));
        assert!(!app.visual.contains(5));
    }

    #[test]
    fn blank_rows_ignore_clicks_and_hover_but_support_drags_and_wheel() {
        for visual in [false, true] {
            for shift in [false, true] {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Moved,
                ] {
                    let mut app = browser_fixture();
                    app.directory = 1;
                    app.selected = Some(3);
                    app.reset_order(false);
                    if visual {
                        app.visual.enter(1, Some(3));
                    }
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    let modifiers = if shift {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    };
                    app.mouse(MouseEvent {
                        kind,
                        row: 10,
                        column: 35,
                        modifiers,
                    });
                    app.mouse(MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        row: 10,
                        column: 35,
                        modifiers,
                    });
                    finish_visual(&mut app);
                    assert_eq!(app.selected, Some(3));
                    assert_eq!(app.visual.active(), visual);
                    assert_eq!(app.visual.len(), usize::from(visual));
                    assert!(!app.visual.contains(5));
                    app.selected = Some(5);
                    app.mouse(MouseEvent {
                        kind: MouseEventKind::ScrollUp,
                        row: 10,
                        column: 35,
                        modifiers,
                    });
                    finish_visual(&mut app);
                    assert_eq!(app.selected, Some(3));
                }
            }
        }
        for shift in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.selected = Some(3);
            app.reset_order(false);
            app.visual.enter(1, Some(3));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let edge = app.list_area.bottom() - 1;
            let mouse = |kind, row, modifiers| MouseEvent {
                kind,
                row,
                column: 35,
                modifiers,
            };
            app.mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                1,
                if shift {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            ));
            app.mouse(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                edge,
                KeyModifiers::NONE,
            ));
            app.pump_visual();
            assert!(app.drag_scroll.is_some());
            app.mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                edge,
                KeyModifiers::NONE,
            ));
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(5));
            assert_eq!(app.visual.contains(3), !shift);
            assert!(app.visual.contains(4) && app.visual.contains(5));
            assert!(app.drag_scroll.is_none());
        }
    }

    #[test]
    fn held_mouse_edge_scroll_is_bounded_keeps_progress_and_stops_on_release() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=300)
                    .map(|id| {
                        entry(
                            id,
                            &format!("item-{id}"),
                            NodeKind::File,
                            u64::from(301 - id),
                        )
                    })
                    .collect(),
            },
        );
        let ids: Vec<_> = (1..=300).collect();
        wait_cache(&mut app, ROOT, &ids);
        app.sync_order(true);
        app.visual.enter(ROOT, Some(1));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind, row| MouseEvent {
            kind,
            row,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5));
        app.pump_visual();
        let deadline = app.drag_scroll.unwrap().2;
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5));
        assert_eq!(
            app.drag_scroll.unwrap().2,
            deadline,
            "edge motion postpones timer"
        );
        assert!(app.pump_mouse_scroll(deadline));
        app.pump_visual();
        assert_eq!(app.selected, Some(8));
        assert_eq!(app.visual.len(), 8);
        assert!(app.offset > 0);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        for id in 9..=280 {
            app.removed(id);
        }
        let deadline = app.drag_scroll.unwrap().2;
        assert!(!app.pump_mouse_scroll(deadline));
        assert_eq!(app.drag_scroll.unwrap().1, 263);
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5));
        assert_eq!(
            app.drag_scroll.unwrap().1,
            263,
            "repeated edge motion restarts tombstone traversal"
        );
        let deadline = app.drag_scroll.unwrap().2;
        assert!(app.pump_mouse_scroll(deadline));
        app.pump_visual();
        assert_eq!(app.selected, Some(283));
        app.modal = Some(Modal::help(app.visual.active()));
        assert!(!app.pump_mouse_scroll(deadline + Duration::from_secs(1)));
        app.modal = None;
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5));
        assert!(app.drag_scroll.is_none());
        assert!(!app.pump_mouse_scroll(deadline + Duration::from_secs(2)));
    }

    #[test]
    fn a_held_press_freezes_discovered_rows_until_release() {
        for pending in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.selected = Some(3);
            app.reset_order(false);
            app.visual.enter(1, Some(3));
            if pending {
                app.visual
                    .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
            }
            let old = Arc::clone(&app.view);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let mouse = |kind| MouseEvent {
                kind,
                row: 2,
                column: 35,
                modifiers: KeyModifiers::SHIFT,
            };
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: 1,
                    entries: vec![entry(6, "new", NodeKind::File, 100)],
                },
            );
            wait_cache(&mut app, 1, &[6, 3, 4, 5]);
            app.sync_order(true);
            assert!(Arc::ptr_eq(&old, &app.view));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
            finish_visual(&mut app);
            app.sync_order(true);
            assert!(app.view.positions.contains_key(&6));
            assert!(app.visual.contains(3) && app.visual.contains(4) && !app.visual.contains(6));
        }
    }

    #[test]
    fn visual_entry_and_normal_shift_page_keep_distinct_pending_navigation() {
        for shifted in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.selected = Some(3);
            app.reset_order(false);
            let old = Arc::clone(&app.view);
            app.order.reset(app.config);
            app.pending_navigation = Some(Navigation::new(Some(true), 0));
            app.key(
                KeyEvent::new(
                    if shifted {
                        KeyCode::Up
                    } else {
                        KeyCode::Char('v')
                    },
                    if shifted {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                ),
                None,
                false,
            );
            assert_eq!(app.visual.active(), !shifted);
            assert!(app.visual.is_empty());
            app.view = Arc::new(CachedSnapshot {
                generation: app.order.generation(),
                child_epoch: old.child_epoch,
                ids: old.ids.clone(),
                positions: old.positions.clone(),
            });
            app.advance_navigation();
            finish_visual(&mut app);
            assert_eq!(app.visual.active(), !shifted);
            assert_eq!(app.visual.contains(5), !shifted);
            assert!(!app.visual.contains(3) && !app.visual.contains(4));
            assert_eq!(app.selected, Some(if shifted { 3 } else { 5 }));
        }
    }

    #[test]
    fn entering_visual_with_shift_mouse_waits_for_normal_navigation() {
        for (name, initial, end, amount, pressed, lead, click_marks, drag_marks) in [
            ("forward", 3, None, 1, 5, 4, vec![4, 5], vec![5]),
            ("backward", 5, None, -1, 3, 4, vec![3, 4], vec![3]),
            ("end", 3, Some(true), 0, 4, 5, vec![4, 5], vec![4]),
            ("home", 5, Some(false), 0, 4, 3, vec![3, 4], vec![4]),
        ] {
            for dragged in [false, true] {
                for pending in [false, true] {
                    let mut app = browser_fixture();
                    app.directory = 1;
                    app.selected = Some(initial);
                    app.reset_order(false);
                    app.pending_navigation = Some(Navigation::new(end, amount));
                    if !pending {
                        app.advance_navigation();
                    }
                    assert!(!app.visual.active());
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    let row = |id| {
                        app.list_area.y
                            + app.painted_rows.iter().position(|&row| row == id).unwrap() as u16
                    };
                    let pressed_row = row(pressed);
                    let lead_row = row(lead);
                    let mouse = |kind, row, modifiers| MouseEvent {
                        kind,
                        column: 35,
                        row,
                        modifiers,
                    };
                    app.mouse(mouse(
                        MouseEventKind::Down(MouseButton::Left),
                        pressed_row,
                        KeyModifiers::SHIFT,
                    ));
                    if pending {
                        assert!(app.has_held_mouse());
                    }
                    if dragged {
                        app.mouse(mouse(
                            MouseEventKind::Drag(MouseButton::Left),
                            lead_row,
                            KeyModifiers::NONE,
                        ));
                    }
                    app.mouse(mouse(
                        MouseEventKind::Up(MouseButton::Left),
                        if dragged { lead_row } else { pressed_row },
                        KeyModifiers::NONE,
                    ));
                    #[cfg(target_os = "linux")]
                    let (mut batch, mut retired) = (None, VecDeque::new());
                    #[cfg(target_os = "linux")]
                    if pending {
                        assert!(matches!(
                            app.key(
                                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                                None,
                                false,
                            ),
                            Action::BatchPrepare
                        ));
                        let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
                        prepare_batch(&mut app, Some(anchor), &mut batch, &retired);
                        assert!(app.waiting_batch.is_some() && batch.is_none());
                    }
                    if pending {
                        app.advance_navigation();
                    }
                    finish_visual(&mut app);
                    let marked: Vec<_> = (3..=5).filter(|&id| app.visual.contains(id)).collect();
                    assert_eq!(
                        &marked,
                        if dragged { &drag_marks } else { &click_marks },
                        "{name}, dragged={dragged}, pending={pending}"
                    );
                    assert_eq!(app.selected, Some(if dragged { lead } else { pressed }));
                    #[cfg(target_os = "linux")]
                    if pending {
                        resume_batch(&mut app, &mut batch, &retired);
                        assert_eq!(batch.as_ref().unwrap().remaining, app.visual.snapshot());
                        assert!(matches!(
                            app.key(
                                KeyEvent::new(
                                    if dragged {
                                        KeyCode::Esc
                                    } else {
                                        KeyCode::Char('n')
                                    },
                                    KeyModifiers::NONE,
                                ),
                                None,
                                false,
                            ),
                            Action::BatchCancel
                        ));
                        cancel_batch(&mut app, &mut batch, &mut retired);
                        let preserved: Vec<_> =
                            (3..=5).filter(|&id| app.visual.contains(id)).collect();
                        assert_eq!(preserved, marked);
                    }
                }
            }
        }
    }

    #[test]
    fn selection_preparation_enter_waits_and_cancel_remains_available() {
        for title in [
            "Selecting entries",
            "Preparing selection",
            "Stopping selection",
        ] {
            let mut app = browser_fixture();
            app.modal = Some(Modal::notice(title, "Wait".to_owned()));
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
            assert!(
                matches!(app.modal, Some(Modal::Text { title: actual, .. }) if actual == title)
            );
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                None,
                false,
            );
            assert!(app.modal.is_none());
        }
    }

    #[test]
    fn visual_cursor_markers_and_count_remain_clear_at_narrow_widths() {
        let mut app = browser_fixture();
        app.visual.enter(ROOT, Some(1));
        app.visual.toggle(2);
        app.selected = Some(2);
        assert!(app.row(1, 30, 80, false, " ").to_string().starts_with('✓'));
        assert!(app.row(2, 30, 80, true, " ").to_string().starts_with('›'));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(rendered(&terminal).contains("Visual 2"));
        app.visual.toggle(2);
        assert_eq!(
            app.row(2, 30, 80, true, " ").style.bg,
            Some(Color::DarkGray)
        );
    }

    #[test]
    fn shift_press_keeps_displayed_order_until_release_or_first_drag() {
        for drag in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.selected = Some(3);
            app.reset_order(false);
            app.visual.enter(1, Some(3));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 35,
                row: if drag { 1 } else { 3 },
                modifiers: KeyModifiers::SHIFT,
            });
            app.view = Arc::new(CachedSnapshot {
                generation: app.view.generation,
                child_epoch: app.view.child_epoch,
                ids: vec![3, 5, 4],
                positions: HashMap::from([(3, 0), (5, 1), (4, 2)]),
            });
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            app.mouse(MouseEvent {
                kind: if drag {
                    MouseEventKind::Drag(MouseButton::Left)
                } else {
                    MouseEventKind::Up(MouseButton::Left)
                },
                column: 35,
                row: 3,
                modifiers: KeyModifiers::SHIFT,
            });
            while app.visual.is_pending() {
                app.pump_visual();
            }
            if drag {
                assert!(
                    !app.visual.contains(3) && app.visual.contains(4) && !app.visual.contains(5)
                );
            } else {
                assert!(app.visual.contains(3) && app.visual.contains(4) && app.visual.contains(5));
            }
        }
    }

    #[test]
    fn shift_drag_inverts_the_pre_press_selection_and_shift_click_adds() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.visual.enter(1, Some(3));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind, row| MouseEvent {
            kind,
            column: 35,
            row,
            modifiers: KeyModifiers::SHIFT,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2));
        assert_eq!(
            app.visual.len(),
            1,
            "a press waits to distinguish click from drag"
        );
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
        while app.visual.is_pending() {
            app.pump_visual();
        }
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
        assert!(app.visual.contains(3) && app.visual.contains(4) && app.visual.contains(5));
        app.clear_visual_intent();
        app.visual.enter(1, Some(3));
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
        app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
        while app.visual.is_pending() {
            app.pump_visual();
        }
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
        assert!(!app.visual.contains(3) && app.visual.contains(4) && app.visual.len() == 1);
        app.clear_visual_intent();
        app.visual.enter(1, Some(3));
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
        while app.visual.is_pending() {
            app.pump_visual();
        }
        assert_eq!(app.visual.len(), 3);
    }

    #[test]
    fn mouse_release_in_a_modal_clears_the_interrupted_drag_without_selecting() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.visual.enter(1, Some(3));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 2,
            modifiers: KeyModifiers::SHIFT,
        });
        assert!(app.mouse_dragging);
        app.key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            None,
            false,
        );
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 35,
            row: 2,
            modifiers: KeyModifiers::SHIFT,
        });
        assert!(!app.mouse_dragging && !app.has_held_mouse() && app.mouse_baseline.is_none());
        assert_eq!(app.visual.len(), 1);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 35,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, Some(5));
    }

    #[test]
    fn mouse_uses_painted_ids_after_an_unpainted_resort() {
        let mut app = browser_fixture();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.view = Arc::new(CachedSnapshot {
            generation: app.view.generation,
            child_epoch: app.view.child_epoch,
            ids: vec![2, 1],
            positions: HashMap::from([(2, 0), (1, 1)]),
        });
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, Some(2));
    }

    #[test]
    fn scoped_child_refresh_preserves_the_parent_viewport_and_cursor() {
        let mut app = browser_fixture();
        app.page_size = 1;
        app.selected = Some(2);
        app.offset = 1;
        let held = Arc::clone(&app.view);
        app.begin_model_refresh(1, 1);
        assert_eq!(app.directory, ROOT);
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.offset, 1);
        assert_eq!(app.selection_screen_row(), Some(0));
        assert!(Arc::ptr_eq(&app.view, &held));
    }

    #[test]
    fn replaced_current_folder_repairs_to_its_parent_and_retains_target_identity() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.begin_model_refresh(1, 1);
        let changed = app.model.apply_scoped(
            1,
            1,
            ScanEvent::RootInfo {
                kind: NodeKind::File,
                sizes: Default::default(),
                error: None,
                file_identity: None,
            },
        );
        app.changed(changed);
        app.repair_directory();
        assert_eq!(app.directory, ROOT);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.model.node(1).kind, NodeKind::File);
        assert!(app.model.is_live(1) && !app.model.is_live(3));
    }

    #[test]
    fn batch_confirmation_scrolls_long_targets_with_pinned_choices_and_resize_guard() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        app.modal = Some(Modal::Batch {
            target: format!("{}final-component", "long spaced directory/".repeat(35)),
            count: 2,
            ready: true,
            reviewable: false,
            scroll: 0,
        });
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(
            screen.contains("final-component")
                && screen.contains("y / Enter: enqueue selected")
                && screen.contains("n / Esc: cancel")
        );
        terminal.backend_mut().resize(30, 5);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false
            ),
            Action::Continue
        ));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                None,
                false
            ),
            Action::BatchCancel
        ));
    }

    #[test]
    fn partially_mutated_complete_directory_reports_partial_totals() {
        let mut app = browser_fixture();
        apply(&mut app, ScanEvent::DirectoryDone { id: 5, error: None });
        apply(&mut app, ScanEvent::DirectoryDone { id: 1, error: None });
        app.selected = Some(1);
        assert!(app.info().contains("Complete"));
        let changed = app.model.mark_partial(1);
        app.changed(changed);
        assert!(app.info().contains("Partial total"));
        let row = app.row(1, 22, 80, false, spinner_frame(0));
        assert!(row.to_string().contains('~'));
    }

    #[test]
    fn successful_delete_without_parent_metadata_marks_totals_without_restarting_scan() {
        for parent in [ROOT, 1] {
            for parent_metadata_available in [false, true] {
                let mut app = browser_fixture();
                apply(&mut app, ScanEvent::DirectoryDone { id: 5, error: None });
                apply(&mut app, ScanEvent::DirectoryDone { id: 1, error: None });
                apply(
                    &mut app,
                    ScanEvent::DirectoryDone {
                        id: ROOT,
                        error: None,
                    },
                );
                apply(&mut app, ScanEvent::Finished { cancelled: false });
                let target = if parent == ROOT { 2 } else { 3 };
                let removed_bytes = app.size(target);
                let total = app.size(ROOT);
                let epoch = app.model.node(parent).child_epoch;
                assert!(app.model.node(ROOT).complete && app.model.finished);
                app.deletion_succeeded(target, parent_metadata_available);
                assert!(!app.model.is_live(target));
                assert_eq!(app.size(ROOT), total - removed_bytes);
                assert_eq!(app.model.node(parent).child_epoch, epoch);
                assert!(app.model.node(parent).complete && app.model.node(ROOT).complete);
                assert_eq!(app.model.node(ROOT).error_count, 0);
                assert!(app.model.last_error.is_none() && app.delete_notice.is_none());
                assert_eq!(
                    app.model.has_partial_totals(parent),
                    !parent_metadata_available
                );
                assert_eq!(
                    app.model.has_partial_totals(ROOT),
                    !parent_metadata_available
                );
                let row = app
                    .row(parent, app.size(ROOT), 80, false, spinner_frame(0))
                    .to_string();
                assert_eq!(row.contains('~'), !parent_metadata_available);
                assert!(!row.contains(spinner_frame(0)));
                app.selected = Some(parent);
                assert_eq!(
                    app.info().contains("Partial total"),
                    !parent_metadata_available
                );
                if !parent_metadata_available {
                    assert_eq!(app.phase(false, false), "Partial totals");
                }
                assert!(app.modal.is_none());
            }
        }
    }

    #[test]
    fn incremental_delete_preserves_folder_neighbor_and_live_scan_without_success_dialog() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        app.modal = Some(Modal::info("old dialog".into()));
        let before = app.model.node(ROOT).sizes.allocated;
        app.removed(3);
        assert_eq!(app.directory, 1);
        assert_eq!(app.selected, Some(4));
        assert!(app.prioritize_requested);
        assert_eq!(app.scan_focus(), Some(1));
        assert!(app.started && !app.model.finished);
        assert!(app.modal.is_none());
        assert_eq!(app.model.node(ROOT).sizes.allocated, before - 12);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("fixture/folder") && screen.contains("survivor"));
        assert!(!screen.contains("remove-me") && !screen.contains("Deleted:"));
        app.directory = 5;
        app.selected = None;
        app.reset_order(false);
        app.removed(1);
        assert_eq!(app.directory, ROOT);
        assert_eq!(app.selected, Some(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removal_invalidates_visible_and_suspended_target_dialogs_only() {
        for suspended in [false, true] {
            for kind in 0..4 {
                let mut app = browser_fixture();
                app.modal = Some(match kind {
                    0 => Modal::Delete {
                        id: 1,
                        anchor: Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap()),
                        relative: "folder".into(),
                        target: "fixture/folder".into(),
                        scroll: 0,
                        reviewable: true,
                        confirmed: Some(test_confirmed_target()),
                        capture: None,
                    },
                    1 => Modal::info("old target".into()),
                    2 => Modal::help(app.visual.active()),
                    _ => Modal::notice("Delete failed", "unrelated failure".into()),
                });
                if suspended {
                    app.key(
                        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                        None,
                        true,
                    );
                    assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                }
                app.removed(1);
                if suspended {
                    assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                    app.key(
                        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                        None,
                        false,
                    );
                }
                if kind < 2 {
                    assert!(app.modal.is_none());
                } else {
                    let expected = if kind == 2 { "Help" } else { "Delete failed" };
                    assert!(
                        matches!(app.modal, Some(Modal::Text { title, .. }) if title == expected)
                    );
                }
            }
        }
    }

    #[test]
    fn navigation_paints_known_children_without_waiting_for_background_order() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false
            ),
            Action::Continue
        ));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("fixture/folder") && screen.contains("survivor"));
        assert!(!screen.contains("Ordering entries"));
    }

    #[test]
    fn wide_preview_is_conditional_and_pending_marker_is_gray_before_the_name() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        let preview_rows: Vec<_> = screen
            .lines()
            .filter_map(|line| line.split_once('│').map(|(_, row)| row))
            .collect();
        assert!(preview_rows[0].contains("Contents"));
        assert!(preview_rows[1].trim_end().ends_with(" remove-me"));
        assert!(preview_rows[2].trim_end().ends_with(" survivor"));
        assert!(preview_rows[3].trim_end().ends_with(" child-folder/"));
        assert!(preview_rows[1].chars().any(|ch| "▏▎▍▌▋▊▉█".contains(ch)));
        assert!(!screen.contains("folder/…") && !screen.contains('≥'));
        assert!((0..10).any(|tick| screen.contains(&format!("{} folder/", spinner_frame(tick)))));
        let buffer = terminal.backend().buffer();
        let marker = buffer
            .content
            .iter()
            .find(|cell| (0..10).any(|tick| cell.symbol() == spinner_frame(tick)))
            .unwrap();
        assert_eq!(marker.fg, Color::Gray);
        terminal.backend_mut().resize(119, 12);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("Contents"));
        terminal.backend_mut().resize(120, 12);
        app.selected = Some(2);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("Contents"));
    }
    #[test]
    fn preview_uses_live_entry_kinds_in_a_warmed_snapshot() {
        for kind in [NodeKind::File, NodeKind::Symlink] {
            let mut app = browser_fixture();
            app.selected = Some(1);
            let cached = app.order.snapshot(1).unwrap();
            let changed = app.model.begin_rescan(5, 1);
            app.changed(changed);
            let changed = app.model.apply_scoped(
                5,
                1,
                ScanEvent::RootInfo {
                    kind,
                    sizes: Default::default(),
                    error: None,
                    file_identity: None,
                },
            );
            app.changed(changed);
            assert!(cached.positions.contains_key(&5));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(Arc::ptr_eq(&cached, &app.preview.view));
            let screen = rendered(&terminal);
            assert!(screen.contains("Contents") && screen.contains("child-folder"));
            assert!(!screen.contains("child-folder/"));
        }
    }

    #[test]
    fn entering_a_warmed_directory_starts_sorted_and_selects_the_first_row_once() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, "folder", NodeKind::Directory, 0)],
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 1,
                entries: vec![
                    entry(2, "small-discovered-first", NodeKind::File, 1),
                    entry(3, "large-discovered-last", NodeKind::File, 50),
                ],
            },
        );
        wait_cache(&mut app, ROOT, &[1]);
        wait_cache(&mut app, 1, &[3, 2]);
        app.selected = Some(1);
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.view.ids, vec![3, 2]);
        assert_eq!(app.selected, Some(3));
        let view = Arc::clone(&app.view);
        app.pump_order();
        assert!(Arc::ptr_eq(&app.view, &view));
        assert_eq!(app.selected, Some(3));
        app.key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.view.ids, vec![1]);
    }

    #[test]
    fn hover_uses_the_warmed_directory_snapshot_in_the_first_frame() {
        let mut app = browser_fixture();
        app.selected = Some(1);
        let cached = app.order.snapshot(1).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(Arc::ptr_eq(&cached, &app.preview.view));
        let screen = rendered(&terminal);
        assert!(screen.contains("remove-me") && screen.contains("survivor"));
        assert!(screen.contains("child-folder/"));
        assert!(!rendered(&terminal).contains("Loading"));
        app.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("Contents"));
        app.key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(Arc::ptr_eq(&cached, &app.preview.view));
        let screen = rendered(&terminal);
        assert!(screen.contains("remove-me") && screen.contains("survivor"));
        assert!(screen.contains("child-folder/"));
    }

    #[test]
    fn untouched_startup_follows_the_largest_row_without_requesting_scan_focus() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        app.page_size = 10;
        wait_cache(&mut app, ROOT, &[]);
        app.sync_order(true);
        assert!(app.selected.is_none());
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "first", NodeKind::Directory, 10),
                    entry(2, "second", NodeKind::Directory, 100),
                ],
            },
        );
        wait_cache(&mut app, ROOT, &[2, 1]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(2));
        for code in [
            KeyCode::Char('g'),
            KeyCode::Char('c'),
            KeyCode::Char('?'),
            KeyCode::Esc,
        ] {
            app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
        }
        let changed = app.model.update_directory_sizes(
            1,
            crate::types::Sizes {
                apparent: 1000,
                allocated: 1000,
            },
        );
        app.changed(changed);
        wait_cache(&mut app, ROOT, &[1, 2]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(1));
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(3, "third", NodeKind::Directory, 10000)],
            },
        );
        wait_cache(&mut app, ROOT, &[3, 1, 2]);
        for _ in 0..3 {
            app.sync_order(true);
            assert_eq!(
                (app.selected, app.offset, app.selection_screen_row()),
                (Some(3), 0, Some(0))
            );
            assert!(
                app.cursor_mode == CursorMode::Top
                    && !app.prioritize_requested
                    && !app.focus_after_selection
            );
        }
    }

    #[test]
    fn early_navigation_survives_discovery_then_resorts_with_visible_context() {
        for height in [1, 5, 10] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            app.page_size = height;
            app.key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                None,
                false,
            );
            assert!(app.cursor_mode != CursorMode::Top && app.pending_navigation.is_some());
            for _ in 0..3 {
                app.sync_order(true);
                assert!(app.pending_navigation.is_some());
            }
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: (1..=40)
                        .map(|id| {
                            entry(
                                id,
                                &format!("folder-{id}"),
                                NodeKind::Directory,
                                u64::from(41 - id) * 100,
                            )
                        })
                        .collect(),
                },
            );
            let ids: Vec<_> = (1..=40).collect();
            wait_cache(&mut app, ROOT, &ids);
            app.sync_order(true);
            assert_eq!(app.selected, Some(2));
            assert!(app.pending_navigation.is_none());
            let changed = app.model.update_directory_sizes(
                2,
                crate::types::Sizes {
                    apparent: 1950,
                    allocated: 1950,
                },
            );
            app.changed(changed);
            let ids: Vec<_> = (1..=40).filter(|&id| id != 2).collect();
            let mut reordered = ids;
            reordered.insert(20, 2);
            wait_cache(&mut app, ROOT, &reordered);
            app.sync_order(true);
            let margin = SCROLL_MARGIN.min((height - 1) / 2);
            assert_eq!(app.selected, Some(2));
            assert_eq!(app.selection_screen_row(), Some(margin), "height={height}");
            assert_eq!(app.offset, 20 - margin);
            assert!(app.cursor_mode != CursorMode::Top);
        }
    }

    #[test]
    fn empty_startup_navigation_settles_on_scan_completion_errors_and_leaf_roots() {
        for event in [
            ScanEvent::DirectoryDone {
                id: ROOT,
                error: None,
            },
            ScanEvent::DirectoryDone {
                id: ROOT,
                error: Some("unreadable".into()),
            },
            ScanEvent::Finished { cancelled: true },
            ScanEvent::RootInfo {
                kind: NodeKind::File,
                sizes: Default::default(),
                error: None,
                file_identity: None,
            },
        ] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            app.key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                None,
                false,
            );
            assert!(app.pending_navigation.is_some());
            apply(&mut app, event);
            app.sync_order(true);
            assert!(app.pending_navigation.is_none() && app.selected.is_none());
            assert!(app.cursor_mode != CursorMode::Top);
        }
    }

    #[test]
    fn cold_entry_with_pending_movement_prioritizes_the_folder_then_its_selected_child() {
        std::fs::create_dir_all(".tmp").unwrap();
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let mut app = browser_fixture_at(fixture.path().to_owned());
        app.directory = 1;
        app.selected = Some(5);
        app.reset_order(false);
        app.key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, 5);
        assert!(app.pending_navigation.is_none());
        assert_eq!(app.scan_focus(), Some(5));
        app.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.pending_navigation.is_some());
        let scanner = scan::spawn(
            fixture.path().to_owned(),
            ScanOptions {
                threads: 1,
                ..ScanOptions::default()
            },
        );
        prioritize_requested(&mut app, &scanner, &[]);
        assert!(!app.prioritize_requested && app.focus_after_selection);
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 5,
                entries: vec![
                    entry(6, "first", NodeKind::Directory, 2),
                    entry(7, "second", NodeKind::Directory, 1),
                ],
            },
        );
        wait_cache(&mut app, 5, &[6, 7]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(7));
        assert_eq!(app.scan_focus(), Some(7));
        assert!(app.prioritize_requested && !app.focus_after_selection);
        prioritize_requested(&mut app, &scanner, &[]);
        for _ in 0..3 {
            app.sync_order(true);
            assert!(!app.prioritize_requested && !app.focus_after_selection);
        }
    }

    #[test]
    fn active_navigation_keeps_its_order_then_resorts_without_moving_the_cursor_row() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=10)
                    .map(|id| {
                        entry(
                            id,
                            &format!("folder-{id}"),
                            NodeKind::Directory,
                            u64::from(id) * 10,
                        )
                    })
                    .collect(),
            },
        );
        wait_cache(&mut app, ROOT, &[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
        app.sync_order(true);
        app.page_size = 4;
        app.selected = Some(7);
        app.offset = 2;
        app.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(Instant::now() < app.navigation_until);
        assert_eq!(app.selected, Some(6));
        let old_view = Arc::clone(&app.view);
        let old_row = app.selection_screen_row().unwrap();
        // Hold the gesture independently of worker scheduling on a busy host.
        app.navigation_until = Instant::now() + Duration::from_secs(20);
        let changed = app.model.update_directory_sizes(
            6,
            crate::types::Sizes {
                apparent: 75,
                allocated: 75,
            },
        );
        app.changed(changed);
        wait_cache(&mut app, ROOT, &[10, 9, 8, 6, 7, 5, 4, 3, 2, 1]);
        assert!(Arc::ptr_eq(&old_view, &app.view));
        assert_eq!(app.selected, Some(6));
        assert_eq!(app.selection_screen_row().unwrap(), old_row);
        app.navigation_until = Instant::now();
        app.sync_order(false);
        assert!(!Arc::ptr_eq(&old_view, &app.view));
        assert_eq!(app.selected, Some(6));
        assert_eq!(app.selection_screen_row().unwrap(), old_row);
        app.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.selected, Some(7));
    }

    #[test]
    fn short_lists_allow_an_empty_tail_to_keep_the_selected_row_anchored() {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        let old_row = app.selection_screen_row().unwrap();
        app.view = Arc::new(CachedSnapshot {
            child_epoch: app.model.node(app.directory).child_epoch,
            generation: app.order.generation(),
            ids: vec![4, 5, 3],
            positions: HashMap::from([(4, 0), (5, 1), (3, 2)]),
        });
        app.offset = app.selection_index().unwrap() - old_row;
        app.keep_selection_visible();
        assert_eq!(app.offset, 2);
        assert_eq!(app.selection_screen_row().unwrap(), old_row);
    }

    #[test]
    fn stale_empty_snapshots_cannot_hide_known_main_or_preview_children() {
        for kind in [NodeKind::Directory, NodeKind::File] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: vec![entry(1, "parent", NodeKind::Directory, 0)],
                },
            );
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: 1,
                    entries: vec![entry(2, "child", kind, 0)],
                },
            );
            if kind == NodeKind::Directory {
                apply(&mut app, ScanEvent::DirectoryDone { id: 2, error: None });
            }
            apply(&mut app, ScanEvent::DirectoryDone { id: 1, error: None });
            apply(
                &mut app,
                ScanEvent::DirectoryDone {
                    id: ROOT,
                    error: None,
                },
            );
            app.selected = Some(1);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let screen = rendered(&terminal);
            assert!(screen.contains("Ordering entries…"));
            assert!(!screen.contains("No child entries."));
            app.removed(2);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(rendered(&terminal).contains("No child entries."));
        }
    }

    #[test]
    fn hidden_policy_applies_immediately_to_cached_rows_and_empty_list_readiness() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(1, ".hidden-parent", NodeKind::Directory, 0)],
            },
        );
        apply(
            &mut app,
            ScanEvent::DirectoryDone {
                id: ROOT,
                error: None,
            },
        );
        wait_cache(&mut app, ROOT, &[1]);
        app.sync_order(true);
        assert!(!app.model.node(ROOT).complete);
        app.key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.selected.is_none());
        assert!(app.prioritize_requested);
        assert_eq!(app.scan_focus(), None);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(!screen.contains(".hidden-parent"));
        assert!(screen.contains("No visible entries"));
        assert!(!screen.contains("Waiting for directory scan"));
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, ROOT);
    }

    fn navigation_fixture(count: NodeId, hidden: std::ops::RangeInclusive<NodeId>) -> App {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=count)
                    .map(|id| {
                        entry(
                            id,
                            &format!(
                                "{}item-{id:05}",
                                if hidden.contains(&id) { "." } else { "" }
                            ),
                            NodeKind::File,
                            u64::from(count - id + 1),
                        )
                    })
                    .collect(),
            },
        );
        let ids: Vec<_> = (1..=count).collect();
        wait_cache(&mut app, ROOT, &ids);
        app.sync_order(true);
        app.page_size = 10;
        app.cursor_mode = CursorMode::Tracked;
        app
    }

    #[test]
    fn navigation_scroll_margin_tracks_both_edges_and_relaxes_at_list_ends() {
        let mut app = navigation_fixture(40, 41..=41);
        app.edge_selection(false);
        app.move_selection(6);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(7), 0, Some(6))
        );
        app.move_selection(1);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(8), 1, Some(6))
        );
        app.move_selection(6);
        app.move_selection(-4);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(10), 6, Some(3))
        );
        app.edge_selection(true);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(40), 30, Some(9))
        );
        for (id, row) in [(39, 8), (38, 7), (37, 6)] {
            app.move_selection(-1);
            assert_eq!(
                (app.selected, app.offset, app.selection_screen_row()),
                (Some(id), 30, Some(row))
            );
        }
        app.edge_selection(false);
        app.move_selection(-1);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(1), 0, Some(0))
        );
    }

    #[test]
    fn navigation_scroll_margin_fits_tiny_viewports() {
        let mut app = navigation_fixture(40, 41..=41);
        for height in 1..=7 {
            app.page_size = height;
            let margin = 3.min((height - 1) / 2);
            app.edge_selection(false);
            app.move_selection(20);
            assert_eq!(app.selected, Some(21));
            assert_eq!(
                app.selection_screen_row(),
                Some(height - 1 - margin),
                "height={height}"
            );
            app.move_selection(-1);
            assert_eq!(app.selection_screen_row(), Some(margin), "height={height}");
            app.edge_selection(true);
            assert_eq!(
                app.selection_screen_row(),
                Some(height - 1),
                "height={height}"
            );
            app.edge_selection(false);
            assert_eq!((app.offset, app.selection_screen_row()), (0, Some(0)));
        }
    }

    #[test]
    fn navigation_scroll_margin_counts_eligible_rows_and_bounds_stale_hidden_spans() {
        for deleted in [false, true] {
            let mut app = navigation_fixture(40, 2..=5);
            app.config.show_hidden = false;
            if deleted {
                app.removed(9);
            }
            app.edge_selection(false);
            app.move_selection(7);
            assert_eq!(app.selected, Some(if deleted { 13 } else { 12 }));
            assert_eq!(app.selection_screen_row(), Some(6));
            assert_eq!(app.screen_rows().next().map(|(_, id)| id), Some(6));
            app.move_selection(-4);
            assert_eq!(
                (app.selected, app.selection_screen_row()),
                (Some(8), Some(3))
            );
            assert_eq!(app.screen_rows().next().map(|(_, id)| id), Some(1));
        }
        let count = (VIEW_BUDGET * 2 + 5) as NodeId;
        let mut app = navigation_fixture(count, 2..=count - 4);
        app.config.show_hidden = false;
        app.selected = Some(count);
        app.offset = 0;
        app.scroll_selection(3);
        assert_eq!(app.selection_screen_row(), Some(3));
        assert!(app.selection_index().unwrap() - app.offset < VIEW_BUDGET);
    }

    #[test]
    fn navigation_scroll_margin_keeps_successors_inside_the_raw_paint_budget() {
        let count = (VIEW_BUDGET + 3) as NodeId;
        for (hidden_end, selected) in [
            ((VIEW_BUDGET - 1) as NodeId, VIEW_BUDGET as NodeId),
            (VIEW_BUDGET as NodeId, 7),
        ] {
            let mut app = navigation_fixture(count, 8..=hidden_end);
            app.config.show_hidden = false;
            app.selected = Some(selected);
            app.offset = 0;
            app.scroll_selection(3);
            let rows: Vec<_> = app.screen_rows().map(|(_, id)| id).collect();
            let cursor = rows.iter().position(|&id| id == selected).unwrap();
            assert!(cursor >= 3);
            assert_eq!(
                &rows[cursor + 1..],
                &[
                    (VIEW_BUDGET + 1) as NodeId,
                    (VIEW_BUDGET + 2) as NodeId,
                    count
                ]
            );
            assert_eq!(app.offset, 3);
        }
    }

    #[test]
    fn normal_shift_arrows_move_one_page_without_entering_visual_mode() {
        for (key, expected, reverse) in [
            (KeyCode::Up, 10, KeyCode::Down),
            (KeyCode::Down, 30, KeyCode::Up),
        ] {
            let mut app = navigation_fixture(40, 41..=41);
            app.selected = Some(20);
            app.offset = 10;
            app.key(KeyEvent::new(key, KeyModifiers::SHIFT), None, false);
            assert_eq!(app.selected, Some(expected));
            assert!(!app.visual.active() && app.visual.is_empty());
            assert!(!app.range_requested);
            app.key(KeyEvent::new(reverse, KeyModifiers::SHIFT), None, false);
            assert_eq!(app.selected, Some(20));
            assert!(!app.visual.active() && app.visual.is_empty());
        }
    }

    #[test]
    fn visual_plain_arrows_preserve_marks_and_reanchor_the_next_shift_range() {
        for pending in [false, true] {
            for (key, cursor, endpoint, skipped) in
                [(KeyCode::Down, 12, 13, 11), (KeyCode::Up, 8, 7, 9)]
            {
                let mut app = navigation_fixture(20, 21..=21);
                app.selected = Some(10);
                app.visual.enter(ROOT, Some(10));
                app.visual.toggle(2);
                app.visual.reanchor(10);
                if pending {
                    app.visual
                        .begin_range(Arc::clone(&app.view), 10, RangeMode::Add);
                }
                for _ in 0..2 {
                    app.key(KeyEvent::new(key, KeyModifiers::NONE), None, false);
                }
                if !pending {
                    assert_eq!(app.selected, Some(cursor));
                    assert_eq!(app.visual.len(), 2);
                    assert!(app.visual.contains(2) && app.visual.contains(10));
                    assert!(!app.visual.contains(cursor) && !app.visual.contains(skipped));
                }
                app.key(KeyEvent::new(key, KeyModifiers::SHIFT), None, false);
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(endpoint));
                assert_eq!(app.visual.len(), 4);
                assert!(
                    [2, 10, cursor, endpoint]
                        .into_iter()
                        .all(|id| app.visual.contains(id))
                );
                assert!(!app.visual.contains(skipped));
            }
        }
    }

    #[test]
    fn visual_hover_reanchors_shift_ranges_but_same_row_keeps_enter_deselection() {
        for deferred in [false, true] {
            let mut app = navigation_fixture(20, 21..=21);
            app.selected = Some(1);
            app.visual.enter(ROOT, Some(1));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 21)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let hover = |row| MouseEvent {
                kind: MouseEventKind::Moved,
                row,
                column: 35,
                modifiers: KeyModifiers::NONE,
            };
            if deferred {
                app.visual
                    .begin_range(Arc::clone(&app.view), 1, RangeMode::Add);
            }
            app.mouse(hover(10));
            app.key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
                None,
                false,
            );
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(11));
            assert_eq!(app.visual.len(), 3);
            assert!([1, 10, 11].into_iter().all(|id| app.visual.contains(id)));
            assert!((2..10).all(|id| !app.visual.contains(id)));

            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(!app.visual.contains(11));
            if deferred {
                app.visual
                    .begin_range(Arc::clone(&app.view), 11, RangeMode::Add);
            }
            app.mouse(hover(11));
            app.key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
                None,
                false,
            );
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(12));
            assert_eq!(app.visual.len(), 3);
            assert!([1, 10, 12].into_iter().all(|id| app.visual.contains(id)));
            assert!(!app.visual.contains(11));
        }
    }

    #[test]
    fn visual_enter_deselection_survives_shift_and_plain_boundary_noops() {
        for boundary_noop in [false, true] {
            for (start, edge, movement, endpoint) in [
                (1, KeyCode::Up, KeyCode::Down, 2),
                (5, KeyCode::Down, KeyCode::Up, 4),
            ] {
                let mut app = navigation_fixture(5, 6..=6);
                app.selected = Some(start);
                app.visual.enter(ROOT, Some(start));
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(app.visual.is_empty());
                if boundary_noop {
                    app.key(KeyEvent::new(edge, KeyModifiers::NONE), None, false);
                    assert_eq!(app.selected, Some(start));
                    assert!(app.visual.is_empty());
                }
                app.key(KeyEvent::new(movement, KeyModifiers::SHIFT), None, false);
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(endpoint));
                assert!(!app.visual.contains(start) && app.visual.contains(endpoint));
                assert_eq!(app.visual.len(), 1);
            }
        }
    }

    #[test]
    fn help_describes_the_active_navigation_mode() {
        for visual in [false, true] {
            let mut app = browser_fixture();
            if visual {
                app.visual.enter(ROOT, app.selected);
            }
            app.key(
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
                None,
                false,
            );
            let Some(Modal::Text { title, text, .. }) = app.modal.as_ref() else {
                panic!("help did not open");
            };
            assert_eq!(*title, "Help");
            assert!(text.contains(COMMON_HELP));
            assert!(text.contains(if visual { VISUAL_HELP } else { NORMAL_HELP }));
            assert!(!text.contains(if visual { NORMAL_HELP } else { VISUAL_HELP }));
        }
    }

    #[test]
    fn visual_keyboard_ranges_keep_scroll_margin_and_all_selected_ids() {
        let mut app = navigation_fixture(40, 41..=41);
        app.key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            None,
            false,
        );
        app.key(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            None,
            false,
        );
        for _ in 0..8 {
            app.key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
                None,
                false,
            );
        }
        finish_visual(&mut app);
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(9), 2, Some(6))
        );
        assert_eq!(app.visual.len(), 9);
        assert!((1..=9).all(|id| app.visual.contains(id)));
        assert!(!app.visual.contains(10));
    }

    #[test]
    fn stationary_hover_preserves_rows_then_keyboard_and_resize_restore_margin() {
        let mut app = navigation_fixture(40, 41..=41);
        app.edge_selection(false);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        for _ in 0..3 {
            app.mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 35,
                row: 10,
                modifiers: KeyModifiers::NONE,
            });
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(
                (app.selected, app.offset, app.selection_screen_row()),
                (Some(10), 0, Some(9))
            );
        }
        app.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(11), 4, Some(6))
        );
        terminal.backend_mut().resize(80, 7);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            (app.selected, app.page_size, app.selection_screen_row()),
            (Some(11), 6, Some(3))
        );
        terminal.backend_mut().resize(80, 11);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            (app.selected, app.offset, app.selection_screen_row()),
            (Some(11), 7, Some(3))
        );
    }

    #[test]
    fn resizing_held_mouse_presses_preserves_the_pressed_viewport() {
        for deferred in [false, true] {
            let mut app = navigation_fixture(40, 41..=41);
            app.selected = Some(11);
            app.offset = 10;
            app.visual.enter(ROOT, Some(11));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let pressed_view = Arc::clone(&app.painted_view);
            if deferred {
                app.visual
                    .begin_range(Arc::clone(&app.view), 11, RangeMode::Add);
            }
            let mouse = |kind, row| MouseEvent {
                kind,
                row,
                column: 35,
                modifiers: KeyModifiers::NONE,
            };
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
            assert_eq!(app.has_held_mouse(), deferred);
            terminal.backend_mut().resize(80, 7);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(Arc::ptr_eq(&app.painted_view, &pressed_view));
            assert_eq!(app.painted_offset, 10);
            assert_eq!(app.painted_rows, vec![11, 12, 13, 14, 15, 16]);
            app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2));
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(12));
            assert_eq!(app.visual.len(), 2);
            assert!(app.visual.contains(11) && app.visual.contains(12));
        }
    }

    #[test]
    fn page_movement_counts_visible_rows_and_does_not_scroll_past_visible_selection() {
        let mut app = navigation_fixture(16, 2..=5);
        app.config.show_hidden = false;
        app.selected = Some(1);
        app.move_selection(10);
        assert_eq!(app.selected, Some(15));
        assert_eq!(app.selection_screen_row(), Some(8));
        app.selected = Some(10);
        app.offset = 0;
        app.keep_selection_visible();
        assert_eq!(app.offset, 0, "raw hidden slots must not trigger scrolling");
        assert_eq!(app.selection_screen_row(), Some(5));
        app.offset = 5;
        app.selected = Some(6);
        app.move_selection(-1);
        assert_eq!(app.selected, Some(1));
        assert_eq!(
            app.offset, 0,
            "Up past the viewport top shifts to the selected row"
        );
    }

    #[test]
    fn filtered_and_deleted_resorts_preserve_the_actual_painted_selection_row() {
        for delete in [false, true] {
            let mut app = navigation_fixture(24, 7..=9);
            let stale = Arc::clone(&app.view);
            app.config.show_hidden = false;
            app.order.configure(app.config);
            let filtered: Vec<_> = (1..=24).filter(|id| !(7..=9).contains(id)).collect();
            wait_cache(&mut app, ROOT, &filtered);
            app.view = Arc::clone(&stale);
            app.offset = 5;
            app.selected = Some(13);
            app.keep_selection_visible();
            assert_eq!(app.offset, 5);
            assert_eq!(app.selection_screen_row(), Some(4));
            if delete {
                app.removed(13);
                assert_eq!(app.selected, Some(14));
                assert_eq!(app.selection_screen_row(), Some(4));
                let after: Vec<_> = filtered.into_iter().filter(|&id| id != 13).collect();
                wait_cache(&mut app, ROOT, &after);
                app.view = stale;
                app.offset = 5;
            }
            app.sync_order(true);
            assert_eq!(app.selected, Some(if delete { 14 } else { 13 }));
            assert_eq!(app.selection_screen_row(), Some(4));
        }
    }

    #[test]
    fn relative_intent_resumes_on_the_same_snapshot_and_opposite_keys_cancel() {
        let mut app = navigation_fixture(2502, 2..=2501);
        app.config.show_hidden = false;
        app.selected = Some(1);
        let held = Arc::clone(&app.view);
        app.move_selection(1);
        assert!(app.pending_navigation.is_some());
        app.move_selection(-1);
        assert!(app.pending_navigation.is_none());
        assert_eq!(app.selected, Some(1));
        app.move_selection(1);
        for _ in 0..30 {
            app.sync_order(false);
            if app.pending_navigation.is_none() {
                break;
            }
        }
        assert!(Arc::ptr_eq(&held, &app.view));
        assert!(app.pending_navigation.is_none());
        assert_eq!(app.selected, Some(2502));
    }

    #[test]
    fn partial_page_intent_restarts_from_the_committed_row_when_cancelled_or_rows_are_deleted() {
        for removed in [2, 4] {
            let mut app = navigation_fixture(2515, 5..=2504);
            app.config.show_hidden = false;
            app.selected = Some(1);
            app.move_selection(10);
            assert_eq!(
                app.pending_navigation.unwrap().cursor.unwrap().target,
                Some(4)
            );
            assert_eq!(app.selected, Some(1));
            app.move_selection(-10);
            assert!(app.pending_navigation.is_none());
            assert_eq!(app.selected, Some(1));
            app.move_selection(10);
            app.removed(removed);
            for _ in 0..30 {
                app.sync_order(false);
                if app.pending_navigation.is_none() {
                    break;
                }
            }
            assert!(app.pending_navigation.is_none());
            assert_eq!(app.selected, Some(2512));
            assert!(app.eligible(app.selected.unwrap()));
        }
    }

    #[test]
    fn tall_pages_complete_without_another_snapshot_and_cold_movement_has_a_consistent_baseline() {
        let mut app = navigation_fixture(1500, 0..=0);
        app.page_size = 300;
        app.selected = Some(1);
        let held = Arc::clone(&app.view);
        app.move_selection(300);
        assert_eq!(app.selected, Some(301));
        assert!(app.pending_navigation.is_none());
        assert!(Arc::ptr_eq(&held, &app.view));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(30, 1100)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.page_size, VIEW_BUDGET);
        app.selected = None;
        app.move_selection(1);
        assert_eq!(app.selected, Some(2));
        app.view = Arc::clone(&app.empty);
        app.selected = None;
        app.move_selection(1);
        assert!(app.pending_navigation.is_some());
        app.sync_order(true);
        assert_eq!(app.selected, Some(2));
        assert!(app.pending_navigation.is_none());
    }

    #[test]
    fn exhausted_empty_cursor_accepts_later_rows_in_the_same_generation() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
                file_identity: None,
                kind: NodeKind::Directory,
                sizes: Default::default(),
                error: None,
            },
        );
        wait_cache(&mut app, ROOT, &[]);
        app.sync_order(true);
        let empty = Arc::clone(&app.view);
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "first", NodeKind::File, 2),
                    entry(2, "second", NodeKind::File, 1),
                ],
            },
        );
        app.move_selection(1);
        assert!(app.pending_navigation.is_some());
        assert_eq!(empty.generation, app.order.generation());
        wait_cache(&mut app, ROOT, &[1, 2]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(2));
        assert!(app.pending_navigation.is_none());
    }

    fn hidden_span_fixture(hidden_last: bool) -> (App, Arc<CachedSnapshot>) {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let mut entries: Vec<_> = (1..=140)
            .map(|id| entry(id, &format!(".hidden-{id:03}"), NodeKind::File, 1))
            .collect();
        entries.push(entry(141, "big", NodeKind::File, 100));
        entries.push(entry(
            142,
            "small",
            NodeKind::File,
            if hidden_last { 90 } else { 0 },
        ));
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries,
            },
        );
        let expected: Vec<_> = if hidden_last {
            [vec![141, 142], (1..=140).collect()].concat()
        } else {
            [vec![141], (1..=140).collect(), vec![142]].concat()
        };
        wait_cache(&mut app, ROOT, &expected);
        app.sync_order(true);
        let stale = Arc::clone(&app.view);
        app.key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            None,
            false,
        );
        // Preserve the old publication regardless of background scheduling.
        app.view = Arc::clone(&stale);
        (app, stale)
    }

    #[test]
    fn end_survives_more_than_128_hidden_entries_in_a_stale_cache_tail() {
        let (mut app, _) = hidden_span_fixture(true);
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        assert!(
            app.pending_navigation
                .is_some_and(|navigation| navigation.end == Some(true))
        );
        wait_cache(&mut app, ROOT, &[141, 142]);
        app.sync_order(false);
        assert_eq!(app.selected, Some(142));
        assert!(app.pending_navigation.is_none());
    }

    #[test]
    fn down_survives_more_than_128_hidden_entries_between_visible_rows() {
        let (mut app, _) = hidden_span_fixture(false);
        app.selected = Some(141);
        app.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(
            app.pending_navigation
                .is_some_and(|navigation| navigation.end.is_none() && navigation.amount == 1)
        );
        assert_eq!(app.selected, Some(141));
        wait_cache(&mut app, ROOT, &[141, 142]);
        app.sync_order(false);
        assert_eq!(app.selected, Some(142));
        assert!(app.pending_navigation.is_none());
    }

    #[test]
    fn overflowing_updates_rewind_the_arena_feed_and_refresh_the_skipped_ancestor() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=10_002)
                    .map(|id| {
                        entry(
                            id,
                            &format!("entry-{id:05}"),
                            if id == 1 {
                                NodeKind::Directory
                            } else {
                                NodeKind::File
                            },
                            if id == 1 {
                                1
                            } else if id == 2 {
                                100
                            } else {
                                0
                            },
                        )
                    })
                    .collect(),
            },
        );
        let previous: Vec<_> = [vec![2, 1], (3..=10_002).collect()].concat();
        wait_cache(&mut app, ROOT, &previous);
        app.pending = (2..2 + PENDING_LIMIT as NodeId).collect();
        app.pending_set = app.pending.iter().copied().collect();
        app.replay_cursor = Some(10_000);
        let changed = app.model.update_directory_sizes(
            1,
            crate::types::Sizes {
                apparent: 200,
                allocated: 200,
            },
        );
        app.changed(changed);
        assert!(app.replay_cursor.is_some_and(|cursor| cursor <= 1));
        let expected: Vec<_> = (1..=10_002).collect();
        wait_cache(&mut app, ROOT, &expected);
        assert_eq!(app.order.snapshot(ROOT).unwrap().ids[0], 1);
    }

    #[test]
    fn structural_feed_preserves_removed_ancestors_and_late_placeholder_parent_links() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "deleted", NodeKind::Directory, 0),
                    entry(2, "survivor", NodeKind::File, 1),
                ],
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 1,
                entries: vec![entry(3, "nested", NodeKind::Directory, 0)],
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 3,
                entries: vec![entry(4, "deep", NodeKind::Directory, 0)],
            },
        );
        assert!(
            app.pending.is_empty(),
            "new IDs must use the monotone structural cursor"
        );
        app.removed(1);
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 4,
                entries: vec![entry(5, "late-directory", NodeKind::Directory, 0)],
            },
        );
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 5,
                entries: vec![entry(6, "late-file", NodeKind::File, 99)],
            },
        );
        wait_cache(&mut app, ROOT, &[2]);
        assert_eq!(app.next_new, app.model.len());
        assert_eq!(app.model.node(5).parent, Some(4));
        assert_eq!(app.model.node(6).parent, Some(5));
        for id in [1, 3, 4, 5, 6] {
            assert!(app.model.is_removed(id));
            assert!(
                app.order.snapshot(id).is_none(),
                "deleted ancestry cannot acquire a cached view"
            );
        }
    }

    #[test]
    fn unresolved_navigation_cannot_activate_the_previous_selection() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "previous", NodeKind::Directory, 0),
                    entry(2, "requested", NodeKind::Directory, 0),
                ],
            },
        );
        app.selected = Some(1);
        app.pending_navigation = Some(Navigation::new(Some(true), 0));
        // No rows have been sent to the worker, so the intent remains unresolved.
        for code in [
            KeyCode::Enter,
            KeyCode::Right,
            KeyCode::Char('l'),
            KeyCode::Char('d'),
            KeyCode::Char('i'),
        ] {
            assert!(matches!(
                app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false),
                Action::Continue
            ));
            assert_eq!(app.directory, ROOT);
            assert_eq!(app.selected, Some(1));
            assert!(app.modal.is_none());
        }
    }

    #[test]
    fn deletion_neighbor_repair_does_not_scan_an_unbounded_hidden_span() {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let mut entries = vec![entry(1, "deleted", NodeKind::File, 200)];
        entries.extend(
            (2..=300).map(|id| entry(id, &format!(".hidden-{id:03}"), NodeKind::File, 100)),
        );
        entries.push(entry(301, "survivor", NodeKind::File, 1));
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries,
            },
        );
        let old: Vec<_> = (1..=301).collect();
        wait_cache(&mut app, ROOT, &old);
        app.sync_order(true);
        let stale = Arc::clone(&app.view);
        app.key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            None,
            false,
        );
        app.view = stale;
        app.selected = Some(1);
        app.removed(1);
        assert!(
            app.selected.is_none(),
            "bounded repair defers a distant eligible neighbor"
        );
        wait_cache(&mut app, ROOT, &[301]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(301));
    }

    #[test]
    fn home_and_end_target_the_requested_sort_even_when_old_edges_remain_eligible() {
        for (code, end, expected) in [(KeyCode::Home, false, 2), (KeyCode::End, true, 1)] {
            let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
            app.config.directories_first = true;
            app.order.configure(app.config);
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: vec![
                        entry(1, "small-folder", NodeKind::Directory, 1),
                        entry(2, "large-file", NodeKind::File, 100),
                        entry(3, "medium-file", NodeKind::File, 50),
                    ],
                },
            );
            wait_cache(&mut app, ROOT, &[1, 2, 3]);
            app.sync_order(true);
            app.page_size = 10;
            let stale = Arc::clone(&app.view);
            app.key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                None,
                false,
            );
            // Model the old frame still on screen independently of worker scheduling.
            app.view = stale;
            app.selected = Some(1);
            app.offset = 0;
            app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
            assert!(
                app.pending_navigation
                    .is_some_and(|intent| intent.end == Some(end))
            );
            assert_eq!(app.selected, Some(1));
            wait_cache(&mut app, ROOT, &[2, 3, 1]);
            app.sync_order(false);
            assert_eq!(app.selected, Some(expected));
            assert!(app.pending_navigation.is_none());
            assert_eq!(
                app.offset, 0,
                "explicit edge navigation fills a short viewport"
            );
        }
    }
    fn enter_filter_query(app: &mut App, query: &str) {
        if !app.filter_editing {
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    None,
                    false
                ),
                Action::Continue
            ));
        }
        app.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            None,
            false,
        );
        for character in query.chars() {
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    None,
                    false
                ),
                Action::Continue
            ));
        }
        assert!(app.filter_editing);
        assert_eq!(app.filter, query);
    }

    fn wait_filtered_rows(app: &mut App, ids: &[NodeId]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.pump_order();
            app.sync_order(true);
            if app
                .current_snapshot()
                .is_some_and(|view| view.ids == ids && Arc::ptr_eq(&view, &app.view))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "current filter did not publish {ids:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn filter_focus_survives_empty_results_and_prioritizes_later_matches_once() {
        let mut app = browser_fixture();
        assert!(!app.prioritize_requested && !app.focus_after_selection);
        assert_eq!(app.selected, Some(1));
        enter_filter_query(&mut app, "future");
        wait_filtered_rows(&mut app, &[]);
        assert!(app.prioritize_requested && app.focus_after_selection);
        assert_eq!(app.scan_focus(), None);
        app.prioritize_requested = false;
        for _ in 0..8 {
            app.sync_order(true);
        }
        assert!(!app.prioritize_requested && app.focus_after_selection);

        // The first ready result was empty; discovery supplies a match later.
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(6, "future-folder", NodeKind::Directory, 1)],
            },
        );
        wait_filtered_rows(&mut app, &[6]);
        assert_eq!(app.selected, Some(6));
        assert_eq!(app.scan_focus(), Some(6));
        assert!(app.prioritize_requested && !app.focus_after_selection);
        app.prioritize_requested = false;
        for _ in 0..8 {
            app.pump_order();
            app.sync_order(true);
        }
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(!app.prioritize_requested && !app.focus_after_selection);

        enter_filter_query(&mut app, "absent");
        wait_filtered_rows(&mut app, &[]);
        assert_eq!(app.selected, None);
        assert_eq!(app.scan_focus(), None);
        assert!(app.prioritize_requested && app.focus_after_selection);
        app.prioritize_requested = false;
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        wait_filtered_rows(&mut app, &[1, 2, 6]);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.scan_focus(), Some(1));
        assert!(app.prioritize_requested && !app.focus_after_selection);
    }

    #[test]
    fn hidden_selection_and_cold_directory_entry_keep_delayed_focus() {
        for hidden in [false, true] {
            let mut app = browser_fixture();
            if hidden {
                apply(
                    &mut app,
                    ScanEvent::Entries {
                        parent: ROOT,
                        entries: vec![entry(6, ".hidden-folder", NodeKind::Directory, 1)],
                    },
                );
                wait_cache(&mut app, ROOT, &[1, 2, 6]);
                app.sync_order(true);
                app.selected = Some(6);
                app.key(
                    KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                    None,
                    false,
                );
            } else {
                // No children of this directory have been discovered yet.
                app.directory = 1;
                app.selected = Some(5);
                app.reset_order(false);
                app.key(
                    KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
                    None,
                    false,
                );
                assert_eq!(app.directory, 5);
                assert!(app.prioritize_requested && app.focus_after_selection);
                assert_eq!(app.scan_focus(), Some(5));
                app.prioritize_requested = false;
                apply(
                    &mut app,
                    ScanEvent::Entries {
                        parent: 5,
                        entries: vec![entry(6, "late-child", NodeKind::Directory, 1)],
                    },
                );
            }
            if hidden && app.selected.is_none() {
                assert!(app.focus_after_selection);
                app.prioritize_requested = false;
            }
            let expected = if hidden { 1 } else { 6 };
            wait_cache(
                &mut app,
                if hidden { ROOT } else { 5 },
                if hidden { &[1, 2] } else { &[6] },
            );
            app.sync_order(true);
            assert_eq!(app.selected, Some(expected));
            assert_eq!(app.scan_focus(), Some(expected));
            assert!(app.prioritize_requested && !app.focus_after_selection);
            app.prioritize_requested = false;
            for _ in 0..8 {
                app.sync_order(true);
            }
            assert!(!app.prioritize_requested);
        }
    }

    #[test]
    fn live_fuzzy_filter_matches_immediate_basenames_for_files_and_directories() {
        let mut app = browser_fixture();
        for (query, ids) in [
            ("fldr", vec![1]),
            ("rtfl", vec![2]),
            ("rm", vec![]),      // remove-me exists only inside folder.
            ("fixture", vec![]), // The scan root is not part of a basename.
        ] {
            enter_filter_query(&mut app, query);
            wait_filtered_rows(&mut app, &ids);
            assert_eq!(app.directory, ROOT);
        }
        enter_filter_query(&mut app, "fldr");
        wait_filtered_rows(&mut app, &[1]);
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(6, "folder-file", NodeKind::File, 2),
                    entry(7, "folder-two", NodeKind::Directory, 0),
                ],
            },
        );
        wait_filtered_rows(&mut app, &[1, 6, 7]);
        assert!(
            app.filter_editing,
            "live updates must not finish filter input"
        );
        assert_eq!(app.order.snapshot(ROOT).unwrap().ids, vec![1, 2, 6, 7]);
    }

    #[test]
    fn rapid_filter_edits_publish_only_the_latest_query_and_escape_restores_rows() {
        let mut app = browser_fixture();
        for query in ["fldr", "no-such-entry", "rtfl"] {
            enter_filter_query(&mut app, query);
        }
        assert!(
            app.current_snapshot()
                .is_none_or(|view| view.ids == vec![2])
        );
        wait_filtered_rows(&mut app, &[2]);
        for _ in 0..16 {
            app.pump_order();
            app.sync_order(true);
            assert_eq!(app.view.ids, vec![2]);
        }
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(!app.filter_editing);
        assert_eq!(app.filter, "rtfl");
        assert_eq!(app.directory, ROOT);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(app.filter.is_empty());
        wait_filtered_rows(&mut app, &[1, 2]);
        enter_filter_query(&mut app, "fldr");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(!app.filter_editing && app.filter.is_empty());
        wait_filtered_rows(&mut app, &[1, 2]);
    }

    #[test]
    fn filter_editing_backspace_removes_unicode_characters_and_ctrl_u_clears_input() {
        let mut app = browser_fixture();
        enter_filter_query(&mut app, "aé界");
        app.key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.filter, "aé");
        app.key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.filter, "a");
        app.key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            None,
            false,
        );
        assert!(app.filter.is_empty() && app.filter_editing);
        app.key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.filter.is_empty() && app.filter_editing);
        wait_filtered_rows(&mut app, &[1, 2]);
    }

    #[test]
    fn directory_navigation_clears_the_applied_filter_in_both_directions() {
        let mut app = browser_fixture();
        enter_filter_query(&mut app, "fldr");
        wait_filtered_rows(&mut app, &[1]);
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, ROOT, "Enter first finishes the query");
        app.key(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, 1);
        assert!(!app.filter_editing && app.filter.is_empty());
        wait_filtered_rows(&mut app, &[3, 4, 5]);
        enter_filter_query(&mut app, "chfldr");
        wait_filtered_rows(&mut app, &[5]);
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        app.key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            None,
            false,
        );
        assert_eq!(app.directory, ROOT);
        assert!(!app.filter_editing && app.filter.is_empty());
        wait_filtered_rows(&mut app, &[1, 2]);
    }

    #[test]
    fn filter_changes_clear_visual_marks_and_pending_input_without_leaving_visual_mode() {
        let mut app = browser_fixture();
        app.key(
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            None,
            false,
        );
        app.key(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            None,
            false,
        );
        app.key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            None,
            false,
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_rows, vec![1, 2]);
        enter_filter_query(&mut app, "fldr");
        assert!(app.visual.active() && app.visual.is_empty());
        assert!(!app.visual.is_pending() && app.visual_intents.is_empty());
        assert!(app.pending_navigation.is_none() && app.mouse_press_view.is_none());
        assert!(
            app.painted_rows.is_empty(),
            "the old frame must not remain clickable"
        );
        let selected = app.selected;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, selected);
        assert!(app.visual.is_empty());
        wait_filtered_rows(&mut app, &[1]);
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.visual.active() && app.visual.is_empty());
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.visual.contains(1));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
        assert!(app.filter.is_empty() && app.visual.active() && app.visual.is_empty());
        wait_filtered_rows(&mut app, &[1, 2]);
    }

    #[test]
    fn filter_q_is_text_during_deletion_but_ctrl_c_still_uses_the_quit_guard() {
        for deleting in [false, true] {
            let mut app = browser_fixture();
            app.key(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                None,
                deleting,
            );
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                    None,
                    deleting
                ),
                Action::Continue
            ));
            assert_eq!(app.filter, "q");
            assert!(app.filter_editing && app.modal.is_none());
            let action = app.key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                None,
                deleting,
            );
            if deleting {
                assert!(matches!(action, Action::QuitWarning));
                assert!(matches!(app.modal, Some(Modal::Quit { reviewable: false })));
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
                assert!(matches!(
                    app.key(
                        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                        None,
                        true
                    ),
                    Action::Quit
                ));
            } else {
                assert!(matches!(action, Action::Quit));
            }
        }
    }

    #[test]
    fn counted_interrupts_remain_available_for_both_quit_steps() {
        let pending = AtomicUsize::new(2);
        assert!(take_interrupt(&pending));
        assert_eq!(pending.load(Ordering::Acquire), 1);
        assert!(take_interrupt(&pending));
        assert!(!take_interrupt(&pending));
        pending.fetch_add(1, Ordering::Release);
        assert!(take_interrupt(&pending));
        assert!(!take_interrupt(&pending));
    }

    #[test]
    fn filter_refresh_retains_current_query_and_clears_ancestor_fallback_state() {
        for ancestor in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.reset_order(false);
            app.filter = "sur".to_owned();
            app.filter_editing = true;
            app.update_filter();

            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                app.pump_order();
                app.sync_order(true);
                if app.view.ids == [4] && app.selected == Some(4) {
                    break;
                }
                assert!(Instant::now() < deadline, "initial filter did not converge");
                std::thread::sleep(Duration::from_millis(1));
            }

            app.visual.enter(1, Some(4));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            app.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: 1,
                column: 35,
                modifiers: KeyModifiers::SHIFT,
            });
            assert!(app.mouse_press_view.is_some() && app.mouse_dragging);
            app.enqueue_visual(VisualIntent::Navigate(Navigation::new(None, 1), true));

            let scope = if ancestor { ROOT } else { 1 };
            app.begin_model_refresh(scope, 1);
            assert_eq!(app.directory, scope);
            assert_eq!(app.filter, if ancestor { "" } else { "sur" });
            assert_eq!(app.filter_editing, !ancestor);
            assert_eq!(app.selected, None);
            assert!(app.pending_navigation.is_none());
            assert!(app.visual.is_empty() && app.visual_intents.is_empty());
            assert!(!app.range_requested && !app.mouse_dragging && !app.has_held_mouse());
            assert!(app.mouse_press_view.is_none() && app.drag_scroll.is_none());
            if ancestor {
                assert!(app.order.filtered_snapshot(1).is_none());
            }

            // Neither a delayed press nor the release may reuse the old painted ID.
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                app.mouse(MouseEvent {
                    kind,
                    row: 1,
                    column: 35,
                    modifiers: KeyModifiers::NONE,
                });
                assert_eq!(app.selected, None);
                assert!(app.visual.is_empty() && app.visual_intents.is_empty());
                assert!(app.mouse_press_view.is_none());
            }

            let changed = app.model.apply_scoped(
                scope,
                1,
                ScanEvent::RootInfo {
                    kind: NodeKind::Directory,
                    sizes: Default::default(),
                    error: None,
                    file_identity: None,
                },
            );
            app.changed(changed);
            let first = app.model.len() as NodeId;
            let changed = app.model.apply_scoped(
                scope,
                1,
                ScanEvent::Entries {
                    parent: ROOT,
                    entries: vec![
                        entry(1, "outside", NodeKind::File, 9),
                        entry(2, "survivor-new", NodeKind::File, 7),
                    ],
                },
            );
            app.changed(changed);
            let expected = if ancestor {
                vec![first, first + 1]
            } else {
                vec![first + 1]
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                app.pump_order();
                app.sync_order(true);
                if app.view.ids == expected
                    && app.view_directory == scope
                    && app.view.child_epoch == app.model.node(scope).child_epoch
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "refreshed filter did not converge"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(app.selected, expected.first().copied());
            assert!(app.selected.is_some_and(|id| app.model.is_live(id)));
            assert!(!app.model.is_live(4));
            assert!(app.visual.is_empty() && !app.visual_busy());
            assert_eq!(app.filter, if ancestor { "" } else { "sur" });
        }
    }
}
