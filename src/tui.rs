use crate::backend::RootAnchor;
use crate::batch::{BatchCapture, BatchEvent};
use crate::delete::{self, ConfirmedTarget, DeleteError};
use crate::model::Model;
use crate::order::{CachedOrderWorker, CachedRow, CachedSnapshot, OrderConfig};
use crate::preview::{FileContents, FilePreview};
use crate::queue::{DeleteQueue, DeleteState, QueueId, QueueMarks};
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
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
struct DeleteReport {
    target: String,
    errors: Vec<DeleteError>,
}

impl DeleteReport {
    fn single(path: PathBuf, message: String) -> Self {
        Self {
            target: printable(path.as_os_str()),
            errors: vec![DeleteError { path, message }],
        }
    }

    fn lines(&self) -> usize {
        self.errors.len().saturating_mul(2)
    }

    /// Only materialize the visible slice, including when scrolled past 65,535 rows.
    fn visible_lines(
        &self,
        scroll: usize,
        height: usize,
        horizontal: usize,
        width: usize,
    ) -> Vec<Line<'static>> {
        (scroll..scroll.saturating_add(height).min(self.lines()))
            .map(|row| {
                let error = &self.errors[row / 2];
                let text = if row % 2 == 0 {
                    printable(error.path.as_os_str())
                } else {
                    printable(OsStr::new(&error.message))
                };
                Line::from(horizontal_slice(&text, horizontal, width)).style(Style::default().fg(
                    if row % 2 == 0 {
                        Color::Cyan
                    } else {
                        Color::White
                    },
                ))
            })
            .collect()
    }
}

enum Modal {
    DeleteErrors {
        scroll: usize,
        horizontal: usize,
        page: usize,
    },
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

#[derive(Clone, Copy)]
struct PendingHover {
    column: u16,
    row: u16,
    directory: NodeId,
    child_epoch: u64,
}

struct PendingScroll {
    amount: isize,
    row: usize,
    view: Arc<CachedSnapshot>,
    offset: usize,
}

enum VisualIntent {
    ScrollViewport(PendingScroll),
    Navigate(Navigation, bool),
    ToggleCurrent,
    ToggleStable(NodeId),
    Hover(PendingHover),
    Mouse(PendingMouse),
}

#[derive(Default)]
struct QueueView {
    status: String,
    marks: QueueMarks,
    cancelling: bool,
    pending: usize,
    paused: bool,
    rows: Vec<(QueueId, bool, String)>,
    selected: Option<QueueId>,
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

struct FrameSnapshot {
    model_revision: u64,
    directory: NodeId,
    selected: Option<NodeId>,
    offset: usize,
    view: Arc<CachedSnapshot>,
    available: Option<Arc<CachedSnapshot>>,
    preview: Option<Arc<CachedSnapshot>>,
    file: Option<Arc<FileContents>>,
    modal: Option<std::mem::Discriminant<Modal>>,
    capturing: bool,
    confirmed: bool,
}

impl FrameSnapshot {
    fn capture(app: &App) -> Self {
        Self {
            model_revision: app.model.revision(),
            directory: app.directory,
            selected: app.selected,
            offset: app.offset,
            view: Arc::clone(&app.view),
            available: app.current_snapshot(),
            preview: app.preview.directory.and_then(|id| app.order.snapshot(id)),
            file: app.file_preview.snapshot(),
            modal: app.modal.as_ref().map(std::mem::discriminant),
            capturing: app.capture_inflight.is_some(),
            confirmed: matches!(
                app.modal,
                Some(Modal::Delete {
                    confirmed: Some(_),
                    ..
                })
            ),
        }
    }

    fn unchanged(&self, app: &App) -> bool {
        fn same<T>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
            match (left, right) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
        }
        let current = Self::capture(app);
        self.model_revision == current.model_revision
            && self.directory == current.directory
            && self.selected == current.selected
            && self.offset == current.offset
            && Arc::ptr_eq(&self.view, &current.view)
            && same(&self.available, &current.available)
            && same(&self.preview, &current.preview)
            && same(&self.file, &current.file)
            && self.modal == current.modal
            && self.capturing == current.capturing
            && self.confirmed == current.confirmed
    }
}

struct FrameGate {
    snapshot: FrameSnapshot,
    dirty: bool,
    active: bool,
    refreshing: bool,
}

impl FrameGate {
    fn new(app: &App) -> Self {
        Self {
            snapshot: FrameSnapshot::capture(app),
            dirty: true,
            active: false,
            refreshing: false,
        }
    }

    fn observe_activity(&mut self, active: bool) {
        // Keep one final frame when work finishes without changing model rows.
        self.dirty |= active || self.active;
        self.active = active;
    }

    fn observe_refreshing(&mut self, refreshing: bool) {
        self.dirty |= self.refreshing != refreshing;
        self.refreshing = refreshing;
    }

    fn needs_frame(&self, app: &App) -> bool {
        self.dirty || !self.snapshot.unchanged(app)
    }

    fn painting(&mut self, app: &App) {
        // Observe before rendering so a concurrent publication cannot be
        // mistaken for contents that were already painted.
        self.snapshot = FrameSnapshot::capture(app);
        self.dirty = false;
    }
}

struct App {
    model: Model,
    root: PathBuf,
    report_root: PathBuf,
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
    file_preview: FilePreview,
    file_anchor: Option<Arc<OnceLock<Arc<RootAnchor>>>>,
    navigation_until: Instant,
    pending_navigation: Option<Navigation>,
    scan_entries: Option<(NodeId, std::vec::IntoIter<Entry>)>,
    graph: Graph,
    counts: bool,
    modal: Option<Modal>,
    delete_notices: VecDeque<DeleteReport>,
    retired_delete_notices: VecDeque<DeleteReport>,
    delete_notice_retirer: Retirer<DeleteReport>,
    started: bool,
    queue_view: QueueView,
    baseline_finished: bool,
    visual: VisualSelection,
    range_requested: bool,
    prioritize_requested: bool,
    list_area: Rect,
    back_area: Rect,
    back_hovered: bool,
    hover_position: Option<(u16, u16)>,
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
    normal_click: Option<NodeId>,
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
        let file_preview = FilePreview::new()?;
        Ok(Self {
            model: Model::new(root.clone()),
            report_root: std::path::absolute(&root)?,
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
            file_preview,
            file_anchor: None,
            navigation_until: Instant::now(),
            pending_navigation: None,
            scan_entries: None,
            graph: Graph::Bar,
            counts: false,
            modal: None,
            delete_notices: VecDeque::new(),
            retired_delete_notices: VecDeque::new(),
            delete_notice_retirer: Retirer::new()?,
            started: false,
            queue_view: QueueView::default(),
            baseline_finished: false,
            visual: VisualSelection::new(ROOT)?,
            range_requested: false,
            prioritize_requested: false,
            list_area: Rect::default(),
            back_area: Rect::default(),
            back_hovered: false,
            hover_position: None,
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
            normal_click: None,
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
            self.hover_position = None;
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

    fn report_path(&self, id: NodeId) -> PathBuf {
        let path = self.model.path(id);
        path.strip_prefix(&self.root)
            .map_or(path.clone(), |relative| self.report_root.join(relative))
    }

    fn delete_failed(&mut self, id: NodeId, error: &io::Error, mut errors: Vec<DeleteError>) {
        let message = printable(OsStr::new(&error.to_string()));
        let changed = self
            .model
            .note_error(id, format!("Delete failed: {message}"));
        self.changed(changed);
        let path = self.report_path(id);
        if errors.is_empty() {
            errors.push(DeleteError {
                path: path.clone(),
                message: error.to_string(),
            });
        }
        self.delete_notices.push_back(DeleteReport {
            target: printable(path.as_os_str()),
            errors,
        });
        self.show_delete_notice();
    }

    fn show_delete_notice(&mut self) -> bool {
        // Large reports must also be cheap to dismiss, not just cheap to paint.
        if let Some(report) = self.retired_delete_notices.pop_front() {
            if let Err(report) = self.delete_notice_retirer.try_retire(report) {
                self.retired_delete_notices.push_front(report);
            }
        }
        if self.modal.is_none() && !self.delete_notices.is_empty() {
            self.modal = Some(Modal::DeleteErrors {
                scroll: 0,
                horizontal: 0,
                page: 1,
            });
            return true;
        }
        false
    }

    fn dismiss_delete_notice(&mut self) {
        if let Some(report) = self.delete_notices.pop_front() {
            if let Err(report) = self.delete_notice_retirer.try_retire(report) {
                self.retired_delete_notices.push_back(report);
            }
        }
        self.show_delete_notice();
    }

    /// Apply removal and diagnostics independently; true requests survivor reconciliation.
    fn deletion_finished(&mut self, id: NodeId, result: delete::DeleteResult) -> bool {
        let parent = self.model.node(id).parent.unwrap_or(ROOT);
        let cancelled = result
            .outcome
            .as_ref()
            .err()
            .is_some_and(|error| error.kind() == io::ErrorKind::Interrupted);
        let needs_reconcile = result.outcome.is_err() && !result.target_removed;
        if needs_reconcile && result.progress.removed_entries > 0 && self.model.is_live(id) {
            let changed = self.model.mark_partial(id);
            self.changed(changed);
        }
        match result.outcome {
            Ok(()) => self.deletion_succeeded(id, result.parent_metadata.is_some()),
            Err(error) => {
                if !cancelled || !result.errors.is_empty() {
                    // Capture the target path before removing its model entry.
                    self.delete_failed(id, &error, result.errors);
                }
                if result.target_removed {
                    self.deletion_succeeded(id, result.parent_metadata.is_some());
                }
            }
        }
        if let Some(metadata) = result.parent_metadata {
            let changed = self.model.update_directory_sizes(parent, metadata.sizes);
            self.changed(changed);
        }
        if let Some(error) = result.metadata_error {
            let changed = self.model.note_error(parent, error);
            self.changed(changed);
        }
        needs_reconcile
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
                        self.scroll_selection(
                            if self.hover_position.is_some() || self.cursor_pending() {
                                0
                            } else {
                                SCROLL_MARGIN
                            },
                        );
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
        self.selected.and_then(|id| self.view.position(id))
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
        let previous_hover = self.hover_position;
        let interrupt =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if self.modal.is_some()
            || interrupt
            || (!self.filter_editing
                && (matches!(
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
                        | KeyCode::Char(
                            'j' | 'k'
                                | 'h'
                                | 'l'
                                | '<'
                                | 'v'
                                | 'V'
                                | '/'
                                | 'e'
                                | 'd'
                                | 'D'
                                | 'r'
                                | 'R'
                                | 'i'
                                | '?'
                                | 'q'
                        )
                ) || (key.code == KeyCode::Delete && self.visual.active())
                    || (key.code == KeyCode::Esc
                        && (self.visual.active() || !self.filter.is_empty()))))
        {
            self.hover_position = None;
            self.normal_click = None;
        }
        if self.filter_editing
            || matches!(
                key.code,
                KeyCode::Char('g' | 'c' | 'n' | 's' | 'C' | 'a' | 't')
            )
        {
            self.normal_click = None;
        }
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
                Modal::DeleteErrors {
                    mut scroll,
                    mut horizontal,
                    page,
                } => {
                    match key.code {
                        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                            self.dismiss_delete_notice();
                            return Action::Continue;
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            horizontal = horizontal.saturating_sub(8)
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            horizontal = horizontal.saturating_add(8)
                        }
                        KeyCode::Home => {
                            scroll = 0;
                            horizontal = 0;
                        }
                        other => scroll_report_key(other, &mut scroll, page),
                    }
                    let max = self
                        .delete_notices
                        .front()
                        .map_or(0, |report| report.lines().saturating_sub(page));
                    self.modal = Some(Modal::DeleteErrors {
                        scroll: scroll.min(max),
                        horizontal,
                        page,
                    });
                }
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
                | KeyCode::Char(
                    'j' | 'k' | 'h' | 'l' | '<' | 'v' | 'V' | '/' | 'e' | 'd' | 'r' | 'R' | 'i'
                )
        ) || (key.code == KeyCode::Delete && self.visual.active())
        {
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
            if changed || matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.hover_position = None;
                if !self.visual.active() {
                    self.finish_mouse_for_input();
                }
            }
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
        if !self.visual.active()
            && (moving
                || matches!(
                    key.code,
                    KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Enter
                        | KeyCode::Backspace
                        | KeyCode::Char('h' | 'l' | '<')
                ))
        {
            self.finish_mouse_for_input();
        }
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
                    if !self.try_enqueue_visual(VisualIntent::Navigate(
                        Navigation::new(end, amount),
                        key.modifiers.contains(KeyModifiers::SHIFT),
                    )) {
                        self.hover_position = previous_hover;
                    }
                } else {
                    let future_cursor = self.cursor_pending() || self.range_requested;
                    if future_cursor {
                        if !self.try_enqueue_visual(VisualIntent::ToggleCurrent) {
                            self.hover_position = previous_hover;
                        }
                    } else if let Some(id) = self.selected {
                        if !self.try_enqueue_visual(VisualIntent::ToggleStable(id)) {
                            self.hover_position = previous_hover;
                        }
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
            self.hover_position = previous_hover;
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
                self.navigate_parent();
            }
            KeyCode::Enter if self.visual.active() => {
                if let Some(id) = self.selected {
                    self.visual.toggle(id);
                    self.range_requested = false;
                }
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(id) = self.selected {
                    self.navigate_directory(id);
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

    fn sync_queue(&mut self, queue: &mut DeleteQueue, area: Rect) {
        queue.sync_marks(&mut self.queue_view.marks);
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
        let visible = usize::from(queue_layout(area).2[1].height).clamp(1, 12);
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
        self.normal_click = None;
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

    fn frame_work(&self) -> bool {
        !self.baseline_finished
            || self.scan_entries.is_some()
            || self.model.has_pending_removals()
            || self.next_new < self.model.len()
            || self.replay_cursor.is_some()
            || !self.pending.is_empty()
            || !self.unsent.is_empty()
            || !self.removals.is_empty()
            || self.visual.is_pending()
            || self.pending_navigation.is_some()
            || self.range_requested
    }

    fn visual_input_available(&self) -> bool {
        self.modal.is_some() || self.visual_intents.len() < VISUAL_INTENT_LIMIT
    }

    fn cursor_pending(&self) -> bool {
        self.pending_navigation.is_some()
            || self.visual_intents.iter().any(|intent| {
                matches!(
                    intent,
                    VisualIntent::Navigate(..)
                        | VisualIntent::ScrollViewport(_)
                        | VisualIntent::Hover(_)
                        | VisualIntent::Mouse(_)
                )
            })
    }

    #[cfg(test)]
    fn enqueue_visual(&mut self, intent: VisualIntent) {
        self.try_enqueue_visual(intent);
    }

    fn try_enqueue_visual(&mut self, intent: VisualIntent) -> bool {
        match (self.visual_intents.back_mut(), &intent) {
            (Some(VisualIntent::ScrollViewport(previous)), VisualIntent::ScrollViewport(next))
                if previous.amount.signum() == next.amount.signum()
                    && previous.offset == next.offset
                    && previous
                        .amount
                        .unsigned_abs()
                        .saturating_add(next.amount.unsigned_abs())
                        <= VIEW_BUDGET
                    && Arc::ptr_eq(&previous.view, &next.view) =>
            {
                previous.amount = previous.amount.saturating_add(next.amount);
                previous.row = next.row;
                return true;
            }
            (Some(VisualIntent::Hover(previous)), VisualIntent::Hover(next)) => {
                *previous = *next;
                return true;
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
                return true;
            }
            _ => {}
        }
        // Keep reading terminal events so interrupts and mouse release remain
        // reachable. Excess ordinary intents are rejected after coalescing.
        if self.visual_intents.len() >= VISUAL_INTENT_LIMIT {
            match intent {
                VisualIntent::Mouse(gesture) => self
                    .visual
                    .retire_snapshot_and_order(im::HashSet::new(), gesture.view),
                VisualIntent::ScrollViewport(scroll) => self
                    .visual
                    .retire_snapshot_and_order(im::HashSet::new(), scroll.view),
                _ => {}
            }
            return false;
        }
        self.visual_intents.push_back(intent);
        true
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
            view.position(lead).and_then(|index| {
                let anchor = view.position(anchor).unwrap_or(index);
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
        // A repaired cursor can lie outside this gesture's frozen interval.
        // Discard its dependent Enter keys before generic focus repair can bind
        // them to an unrelated row; later cursor intents keep their own keys.
        let follows_gesture = cursor.is_some_and(|id| {
            let Some(index) = view.position(id) else {
                return false;
            };
            let Some(anchor) = view.position(anchor) else {
                return false;
            };
            let Some(lead) = view.position(lead) else {
                return false;
            };
            (anchor.min(lead)..=anchor.max(lead)).contains(&index)
        });
        if !follows_gesture {
            self.bind_visual_cursor(None);
        }
        self.selected = cursor;
        self.focus_after_selection |= cursor.is_none();
        self.prioritize_requested = true;
        if let Some(id) = cursor {
            self.bind_visual_cursor(Some(id));
            // A queued press can acquire capture after a resize clips its row.
            // Keep its physical viewport until release or deliberate dragging.
            if self.mouse_press_view.is_none() {
                self.keep_selection_visible();
            }
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
            view.position(id).filter(|_| edge != 0).map(|index| {
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
        self.hover_position = None;
        self.cancel_deferred_batch();
        self.range_requested = false;
        for intent in self.visual_intents.drain(..) {
            match intent {
                VisualIntent::Mouse(gesture) => self
                    .visual
                    .retire_snapshot_and_order(im::HashSet::new(), gesture.view),
                VisualIntent::ScrollViewport(scroll) => self
                    .visual
                    .retire_snapshot_and_order(im::HashSet::new(), scroll.view),
                _ => {}
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
                VisualIntent::ScrollViewport(scroll) => self.scroll_viewport(scroll, true),
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
                VisualIntent::Hover(hover) => {
                    let id = (hover.directory == self.directory
                        && hover.child_epoch == self.model.node(self.directory).child_epoch
                        && self
                            .list_area
                            .contains(ratatui::layout::Position::new(hover.column, hover.row)))
                    .then(|| {
                        self.screen_rows()
                            .nth(usize::from(hover.row.saturating_sub(self.list_area.y)))
                            .map(|(_, id)| id)
                    })
                    .flatten();
                    self.bind_visual_cursor(id);
                    if let Some(id) = id {
                        if self.visual.active()
                            && self.selected != Some(id)
                            && self.mouse_press_view.is_none()
                        {
                            self.visual.reanchor(id);
                        }
                        self.selected = Some(id);
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
                        } else {
                            match gesture.mode {
                                RangeMode::Add => {
                                    self.visual.toggle(gesture.lead);
                                }
                                RangeMode::Remove => {
                                    self.visual.deselect(gesture.lead);
                                }
                                RangeMode::Invert => {
                                    self.add_mouse_range(Arc::clone(&gesture.view), gesture.lead);
                                }
                            }
                        }
                        self.focus_mouse_lead(&gesture.view, gesture.anchor, gesture.lead);
                    } else {
                        self.bind_visual_cursor(None);
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
        // The scan root is the final navigation boundary, including leaf and
        // failed roots. Repeated repair must not reset their settled input state.
        if self.directory == ROOT
            || (self.model.is_live(self.directory)
                && self.model.node(self.directory).kind == NodeKind::Directory)
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

    fn navigate_directory(&mut self, id: NodeId) {
        if self.eligible(id)
            && self.model.node(id).kind == NodeKind::Directory
            && self.model.node(id).parent == Some(self.directory)
        {
            self.directory = id;
            self.selected = None;
            self.focus_after_selection = true;
            self.clear_visual_intent();
            self.visual.reset(id);
            self.prioritize_requested = true;
            self.reset_order(false);
        }
    }

    fn navigate_parent(&mut self) {
        if let Some(parent) = self.model.node(self.directory).parent {
            self.selected = Some(self.directory);
            self.directory = parent;
            self.clear_visual_intent();
            self.visual.reset(parent);
            self.prioritize_requested = true;
            self.reset_order(false);
        }
    }

    fn scroll_viewport(&mut self, scroll: PendingScroll, deferred: bool) {
        if scroll.view.child_epoch != self.model.node(self.directory).child_epoch {
            self.visual
                .retire_snapshot_and_order(im::HashSet::new(), scroll.view);
            return;
        }
        self.track_selection();
        self.pending_navigation = None;
        self.range_requested = false;
        // Deferred input moves the committed viewport after earlier intents,
        // including across live resorts. Immediate input starts from the
        // physical rows the user saw even if a newer order is not yet painted.
        if deferred {
            self.visual
                .retire_snapshot_and_order(im::HashSet::new(), scroll.view);
        } else {
            self.view = scroll.view;
            self.offset = scroll.offset.min(self.view.ids.len());
        }
        let mut remaining = scroll.amount.unsigned_abs();
        let mut next_offset = self.offset;
        // Count displayed entries while keeping work bounded through stale
        // hidden/deleted slots. The ordering worker will compact those slots.
        if scroll.amount < 0 {
            for index in (0..self.offset).rev().take(VIEW_BUDGET) {
                if self.eligible(self.view.ids[index]) {
                    next_offset = index;
                    remaining = remaining.saturating_sub(1);
                    if remaining == 0 {
                        break;
                    }
                }
            }
        } else if scroll.amount > 0 {
            for (index, &id) in self
                .view
                .ids
                .iter()
                .enumerate()
                .skip(self.offset.saturating_add(1))
                .take(VIEW_BUDGET)
            {
                if self.eligible(id) {
                    next_offset = index;
                    remaining = remaining.saturating_sub(1);
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
        if let Some((last, _)) = self
            .view
            .ids
            .iter()
            .enumerate()
            .rev()
            .take(VIEW_BUDGET)
            .find(|(_, id)| self.eligible(**id))
        {
            next_offset =
                next_offset.min(self.offset_for_row(last, self.page_size.saturating_sub(1)));
        }
        self.offset = next_offset;
        let pointer_row = scroll.row.min(self.page_size.saturating_sub(1));
        let target = self
            .screen_rows()
            .take(pointer_row + 1)
            .last()
            .map(|(_, id)| id);
        if let Some(id) = target {
            if self.visual.active() && self.selected != Some(id) {
                self.visual.reanchor(id);
            }
            self.selected = Some(id);
            self.bind_visual_cursor(Some(id));
        }
        self.navigation_until = Instant::now() + Duration::from_millis(150);
        self.prioritize_requested = true;
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        // Only admitted physical input owns hover mode; older replay cannot reactivate it.
        let previous_hover = self.hover_position;
        self.hover_position = None;
        let captured = self.mouse_press_view.is_some() || self.has_held_mouse();
        if self.modal.is_some()
            || matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || (captured
                && (matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left))
                    || (mouse.kind == MouseEventKind::Moved
                        && self.mouse_position != Some((mouse.column, mouse.row)))))
        {
            // A drag cannot become a click by returning to its original row.
            // Keep capture itself so held rows and visual gestures remain stable.
            self.normal_click = None;
        }
        let captured_motion = captured
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            );
        self.back_hovered = self.modal.is_none()
            && !captured_motion
            && self
                .back_area
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row));
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
                // A normal click resumes hover only at its original live row.
                // Normal drags keep their pressed ID and never become ranges.
                let restore_hover = !self.visual.active()
                    && self.mouse_position == Some((mouse.column, mouse.row))
                    && self.mouse_press_view.as_ref().is_some_and(|view| {
                        view.child_epoch == self.model.node(self.directory).child_epoch
                    })
                    && self.painted_directory == self.directory
                    && self
                        .list_area
                        .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                    && self.drag_anchor.is_some_and(|id| {
                        self.selected == Some(id)
                            && self.eligible(id)
                            && self.model.node(id).parent == Some(self.directory)
                            && self
                                .painted_rows
                                .get(usize::from(mouse.row.saturating_sub(self.list_area.y)))
                                == Some(&id)
                    });
                let activate = self.normal_click.filter(|_| restore_hover);
                if captured
                    && self
                        .mouse_position
                        .is_some_and(|position| position != (mouse.column, mouse.row))
                {
                    self.mouse(MouseEvent {
                        kind: MouseEventKind::Drag(MouseButton::Left),
                        ..mouse
                    });
                }
                self.finish_mouse_for_input();
                // A rejected press never acquired capture. Its release may
                // leave the list, but cannot replace the last admitted hover.
                if restore_hover {
                    self.hover_position = Some((mouse.column, mouse.row));
                } else if !captured
                    && self
                        .list_area
                        .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
                {
                    self.hover_position = previous_hover;
                }
                if let Some(id) = activate {
                    self.navigate_directory(id);
                }
            }
            return;
        }
        if let Some(Modal::DeleteErrors { scroll, page, .. }) = self.modal.as_mut() {
            let max = self
                .delete_notices
                .front()
                .map_or(0, |report| report.lines().saturating_sub(*page));
            match mouse.kind {
                MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3).min(max),
                _ => {}
            }
        }
        if self.modal.is_some() {
            self.back_hovered = false;
            return;
        }
        if self.back_hovered {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.finish_mouse_for_input();
                self.navigate_parent();
            }
            return;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Right | MouseButton::Middle)
                | MouseEventKind::Up(MouseButton::Right | MouseButton::Middle)
                | MouseEventKind::Drag(MouseButton::Right | MouseButton::Middle)
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            // Ignored input cannot replace the last admitted list hover.
            if self
                .list_area
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
            {
                self.hover_position = previous_hover;
            }
            return;
        }
        let dragging = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) && captured;
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
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) && !captured {
            self.hover_position = previous_hover;
            return;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.track_selection();
            self.finish_mouse_for_input();
            let scroll = PendingScroll {
                amount: if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                },
                row: usize::from(mouse.row.saturating_sub(self.list_area.y)),
                view: Arc::clone(&self.painted_view),
                offset: self.painted_offset,
            };
            if self.visual.active() && self.visual_busy() {
                self.hover_position =
                    if self.try_enqueue_visual(VisualIntent::ScrollViewport(scroll)) {
                        Some((mouse.column, mouse.row))
                    } else {
                        previous_hover
                    };
                return;
            }
            self.hover_position = Some((mouse.column, mouse.row));
            self.scroll_viewport(scroll, false);
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
                    let mode = if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                        RangeMode::Remove
                    } else if shift {
                        RangeMode::Invert
                    } else {
                        RangeMode::Add
                    };
                    if self.visual_busy() {
                        if self.try_enqueue_visual(VisualIntent::Mouse(PendingMouse {
                            anchor: id,
                            lead: id,
                            dragged: false,
                            released: false,
                            mode,
                            edge,
                            view: Arc::clone(&self.painted_view),
                            offset: self.painted_offset,
                        })) {
                            self.cursor_mode = CursorMode::Tracked;
                            self.mouse_position = Some((mouse.column, mouse.row));
                        } else {
                            self.hover_position = previous_hover;
                        }
                        return;
                    }
                    self.mouse_position = Some((mouse.column, mouse.row));
                    self.retire_mouse_press();
                    self.mouse_baseline = Some(self.visual.snapshot());
                    self.mouse_press_view = Some(Arc::clone(&self.painted_view));
                    self.view = Arc::clone(&self.painted_view);
                    self.offset = self.painted_offset;
                    self.mouse_dragging = mode == RangeMode::Invert;
                    self.drag_started = false;
                    self.drag_anchor = Some(id);
                    self.drag_mode = mode;
                    match mode {
                        RangeMode::Add => {
                            self.visual.toggle(id);
                        }
                        RangeMode::Remove => {
                            self.visual.deselect(id);
                        }
                        RangeMode::Invert => {}
                    }
                } else {
                    self.normal_click = Some(id);
                    self.mouse_position = Some((mouse.column, mouse.row));
                    self.mouse_press_view = Some(Arc::clone(&self.painted_view));
                    self.view = Arc::clone(&self.painted_view);
                    self.offset = self.painted_offset;
                    self.drag_anchor = Some(id);
                }
                self.cursor_mode = CursorMode::Tracked;
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if dragging && self.visual.active() => {
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
            MouseEventKind::Moved if !captured => {
                self.cursor_mode = CursorMode::Tracked;
                if self.visual.active() && self.visual_busy() {
                    if self.try_enqueue_visual(VisualIntent::Hover(PendingHover {
                        column: mouse.column,
                        row: mouse.row,
                        directory: self.directory,
                        child_epoch: self.painted_view.child_epoch,
                    })) {
                        self.hover_position = Some((mouse.column, mouse.row));
                    } else {
                        self.hover_position = previous_hover;
                    }
                    return;
                }
                self.hover_position = Some((mouse.column, mouse.row));
                self.view = Arc::clone(&self.painted_view);
                self.offset = self.painted_offset;
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

    fn deletion_state(&self, mut id: NodeId) -> DeleteState {
        let mut state = DeleteState::default();
        if self.queue_view.marks.is_empty() || !self.model.is_live(id) {
            return state;
        }
        loop {
            state = self.queue_view.marks.include(id, state);
            let Some(parent) = self.model.node(id).parent else {
                return state;
            };
            id = parent;
        }
    }

    fn deleting(&self, id: NodeId) -> bool {
        self.deletion_state(id).is_deleting()
    }

    fn row(
        &self,
        id: NodeId,
        total: u64,
        width: u16,
        selected: bool,
        spinner: &'static str,
        state: DeleteState,
    ) -> Line<'static> {
        let deleting = self.queue_view.marks.include(id, state).is_deleting();
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
        let visually_selected = self.visual.contains(id);
        let marker = if selected {
            "›"
        } else if visually_selected {
            "✓"
        } else {
            " "
        };
        let mut spans = Vec::with_capacity(6);
        spans.extend([
            Span::styled(marker, Style::default().fg(Color::White)),
            Span::raw(format!("{size_label:>11} ")),
        ]);
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
            format!(
                "{} ",
                if deleting {
                    "×"
                } else if node.complete {
                    " "
                } else {
                    spinner
                }
            ),
            Style::default().fg(if deleting { Color::Red } else { Color::Gray }),
        ));
        spans.push(Span::styled(name, Style::default().fg(color)));
        let line = Line::from(spans);
        if visually_selected {
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
        let hint = if self.visual.active() && !self.filter_editing {
            if area.width >= 60 {
                "d delete  Esc clear  ? help"
            } else if area.width >= 40 {
                "d delete  ? help"
            } else {
                "? help"
            }
        } else {
            "? help"
        };
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(hint.len() as u16 + 2),
            ])
            .split(header);
        self.back_area = columns[0];
        let mut phase_text = if self.filter_editing {
            format!(
                "Filter /{}",
                clipped_tail(
                    &self.filter,
                    usize::from(columns[1].width).saturating_sub(8)
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
        let busy = deleting || !self.queue_view.status.is_empty();
        if busy {
            if self.visual.active() || self.filter_editing {
                phase_text.push_str(" · Deleting…");
            }
            phase_text.push_str(" [D]");
        }
        let width = usize::from(columns[1].width);
        let phase_budget = if self.visual.active() || self.filter_editing {
            width
        } else {
            width.saturating_sub(width.min(12) + 2)
        };
        let phase_text = if busy && phase_budget >= 4 {
            format!(
                "{} [D]",
                clipped(phase_text.trim_end_matches(" [D]"), phase_budget - 4)
            )
        } else {
            clipped(&phase_text, phase_budget)
        };
        let phase = phase_text.as_str();
        let path_budget =
            usize::from(columns[1].width).saturating_sub(Line::from(phase).width() + 3);
        let path = clipped_path_tail(
            &printable(self.model.path(self.directory).as_os_str()),
            path_budget,
        );
        let label = if path.is_empty() {
            phase.to_owned()
        } else {
            format!("{path}  {phase}")
        };
        let header_style = Style::default().bg(Color::DarkGray).fg(Color::White);
        let back_enabled = self.model.node(self.directory).parent.is_some();
        let back_style = if back_enabled && self.back_hovered && self.modal.is_none() {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            header_style.fg(if back_enabled {
                Color::White
            } else {
                Color::Gray
            })
        };
        frame.render_widget(Paragraph::new("←─").style(back_style), columns[0]);
        if self.filter_editing && self.modal.is_none() && columns[1].width > 0 {
            let x = Line::from(label.as_str())
                .width()
                .min(usize::from(columns[1].width - 1));
            frame.set_cursor_position((columns[1].x + x as u16, columns[1].y));
        }
        frame.render_widget(Paragraph::new(label).style(header_style), columns[1]);
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(header_style),
            columns[2],
        );
        let body = Rect::new(
            area.x,
            area.y + area.height.min(1),
            area.width,
            area.height.saturating_sub(1),
        );
        let (body, preview_area) = preview_layout(body);
        let hovered = self.selected.filter(|&id| {
            preview_area.is_some()
                && self.eligible(id)
                && self.model.node(id).parent == Some(self.directory)
        });
        let directory = hovered.filter(|&id| self.model.node(id).kind == NodeKind::Directory);
        self.order.prioritize(self.directory, directory);
        self.preview.sync(directory, &self.order, &self.model);
        if let Some(id) = hovered.filter(|&id| {
            preview_area.is_some_and(|area| area.height > 1)
                && matches!(self.model.node(id).kind, NodeKind::File | NodeKind::Symlink)
                && !self.deleting(id)
                && self.queue_view.marks.active.is_none()
                && !matches!(self.modal, Some(Modal::Quit { .. } | Modal::Stopping))
        }) {
            if !self.file_preview.matches(id, self.model.node(id).kind) {
                if let Some(anchor) = self
                    .file_anchor
                    .as_ref()
                    .and_then(|anchor| anchor.get())
                    .cloned()
                {
                    if let Ok(relative) = self.model.path(id).strip_prefix(&self.root) {
                        self.file_preview.request(
                            id,
                            anchor,
                            relative.to_owned(),
                            self.model.node(id).kind,
                        );
                    }
                }
            }
        } else {
            self.file_preview.clear();
        }
        self.list_area = body;
        if self.modal.is_some()
            || self.hover_position.is_some_and(|(column, row)| {
                !body.contains(ratatui::layout::Position::new(column, row))
            })
        {
            self.hover_position = None;
        }
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
        if held_view.is_none() && self.mouse_press_view.is_none() {
            // Held captures keep their viewport even when resizing clips the
            // pressed row. Hover otherwise suppresses only the keyboard margin.
            let margin = if resized && self.hover_position.is_none() && !self.cursor_pending() {
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
        let mut painted_rows = std::mem::take(&mut self.painted_rows);
        painted_rows.clear();
        painted_rows.extend(
            self.rows_in(&painted_view, painted_offset)
                .map(|(_, id)| id),
        );
        self.painted_rows = painted_rows;
        self.painted_view = painted_view;
        self.painted_offset = painted_offset;
        self.painted_directory = self.directory;
        let mut lines = Vec::with_capacity(self.page_size);
        let state = self.deletion_state(self.directory);
        for &id in &self.painted_rows {
            lines.push(self.row(
                id,
                total,
                body.width,
                self.selected == Some(id),
                spinner,
                state,
            ));
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
            let contents = self.file_preview.snapshot();
            let text_kind = contents
                .as_ref()
                .filter(|contents| contents.text.is_some())
                .map(|contents| contents.kind);
            let mut rows = vec![
                Line::from(if directory.is_some() {
                    "Contents"
                } else {
                    match text_kind {
                        Some("Symlink") => "Links to",
                        Some("Archive") => "Archive contents",
                        _ => "Preview",
                    }
                })
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(directory) = directory {
                let state = self.deletion_state(directory);
                for &id in self.preview.view.ids.iter().take(1024) {
                    if rows.len() >= usize::from(pane.height) {
                        break;
                    }
                    if !self.eligible(id) {
                        continue;
                    }
                    rows.push(self.row(
                        id,
                        self.size(directory),
                        pane.width,
                        false,
                        spinner,
                        state,
                    ));
                }
                if rows.len() == 1 {
                    let node = self.model.node(directory);
                    let message =
                        if self
                            .model
                            .has_children(directory, false, self.config.show_hidden)
                        {
                            "Ordering entries…"
                        } else if node.error_count > 0 {
                            "Partial listing; press i for details."
                        } else if !self.model.listing_complete(directory) {
                            "Waiting for scan…"
                        } else {
                            "No child entries."
                        };
                    rows.push(Line::from(message).style(Style::default().fg(Color::DarkGray)));
                }
            } else if let Some(id) = hovered {
                if let Some(text) = contents
                    .as_ref()
                    .and_then(|contents| contents.text.as_deref())
                {
                    let truncated = contents.as_ref().is_some_and(|contents| contents.truncated);
                    let limit = usize::from(pane.height).saturating_sub(1 + usize::from(truncated));
                    rows.extend(text.lines().take(limit).map(Line::from));
                    if truncated {
                        rows.push(Line::from("…").style(Style::default().fg(Color::DarkGray)));
                    }
                } else {
                    let node = self.model.node(id);
                    let kind = if self.deleting(id) {
                        "Marked for deletion"
                    } else {
                        contents.as_ref().map_or(
                            match node.kind {
                                NodeKind::File => "File",
                                NodeKind::Symlink => "Symlink",
                                _ => "Special file",
                            },
                            |contents| contents.kind,
                        )
                    };
                    rows.extend([
                        Line::from(kind),
                        Line::from(format!("Size  {}", human_size(node.sizes.apparent))),
                        Line::from(format!("Disk  {}", human_size(node.sizes.allocated))),
                    ]);
                }
            } else {
                rows.push(
                    Line::from("No entry selected").style(Style::default().fg(Color::DarkGray)),
                );
            }
            let mut paragraph = Paragraph::new(rows);
            if text_kind == Some("Symlink") {
                paragraph = paragraph.wrap(Wrap { trim: false });
            }
            let clipped_link = text_kind == Some("Symlink")
                && paragraph.line_count(pane.width) > usize::from(pane.height);
            frame.render_widget(
                paragraph,
                Rect {
                    height: pane.height.saturating_sub(u16::from(clipped_link)),
                    ..pane
                },
            );
            if clipped_link && pane.height > 0 {
                frame.render_widget(
                    Paragraph::new("…").style(Style::default().fg(Color::DarkGray)),
                    Rect::new(pane.x, pane.bottom() - 1, pane.width, 1),
                );
            }
        }
        if let Some(modal) = self.modal.as_mut() {
            draw_modal(
                frame,
                area,
                modal,
                &mut self.queue_view,
                self.delete_notices.front(),
            );
        }
    }
}

fn preview_layout(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < 96 {
        return (area, None);
    }
    let width = (u32::from(area.width) * 4 / 7) as u16;
    (
        Rect::new(area.x, area.y, width, area.height),
        Some(Rect::new(
            area.x + width,
            area.y,
            area.width - width,
            area.height,
        )),
    )
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

/// Share the current frame's queue capacity with row synchronization.
fn queue_layout(area: Rect) -> (Rect, Rect, [Rect; 3]) {
    let popup = centered(area, area.width.min(76), area.height.min(20));
    let inner = Block::bordered().inner(popup);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(4),
        ])
        .split(inner);
    (popup, inner, [regions[0], regions[1], regions[2]])
}

fn draw_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    modal: &mut Modal,
    queue: &mut QueueView,
    report: Option<&DeleteReport>,
) {
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
        Modal::DeleteErrors {
            scroll,
            horizontal,
            page,
        } => {
            let Some(report) = report else {
                return;
            };
            let popup = centered(area, width, area.height.min(24));
            let block = border("Delete failed");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let regions = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(0),
                    Constraint::Length(2),
                ])
                .split(inner);
            *page = usize::from(regions[1].height).max(1);
            *scroll = (*scroll).min(report.lines().saturating_sub(*page));
            let first = if report.errors.is_empty() {
                0
            } else {
                *scroll / 2 + 1
            };
            let last = scroll
                .saturating_add(*page)
                .div_ceil(2)
                .min(report.errors.len());
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Deletion inside this target failed:"),
                    Line::from(horizontal_slice(
                        &report.target,
                        *horizontal,
                        usize::from(inner.width),
                    ))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                    Line::from(format!("{} errors · {first}–{last}", report.errors.len()))
                        .style(Style::default().fg(Color::DarkGray)),
                ])
                .style(style),
                regions[0],
            );
            frame.render_widget(
                Paragraph::new(report.visible_lines(
                    *scroll,
                    usize::from(regions[1].height),
                    *horizontal,
                    usize::from(inner.width),
                ))
                .style(style),
                regions[1],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("↑↓ / PgUp/PgDown  ←→: scroll")
                        .style(Style::default().fg(Color::DarkGray)),
                    controls("Enter / Esc", ": close"),
                ])
                .style(style),
                regions[2],
            );
        }
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
            let (popup, inner, regions) = queue_layout(area);
            let block = border("Deletion queue");
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let summary = if queue.marks.active.is_some() {
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
Enter / click      Open directory
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
Ctrl-click/drag    Deselect entry / range
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

⠋ unfinished    × queued/deleting    ~ partial size    ! read error
Sorting includes unfinished entries.
Near-size groups retain their previous order.
Graph shows the share of known directory size.
Preview (96+ columns): folder contents, text, or file details.
Symlinks show their destination path without following it.
Archive listings and PDF text use available system utilities.
Hardlinks are counted once.";

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

fn deletion_busy(queue: &DeleteQueue, batch: Option<&BatchPreparation>) -> bool {
    queue.is_busy() || batch.is_some_and(|batch| batch.dispatching || batch.confirm_pending)
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
            } else {
                let Some(id) = preparation.remaining.iter().next().copied() else {
                    break;
                };
                preparation.remaining.remove(&id);
                status.productive = true;
                if !app.model.is_live(id)
                    || app.model.node(id).parent != Some(preparation.directory)
                    || (!preparation.show_hidden
                        && app.model.node(id).name.as_encoded_bytes().starts_with(b"."))
                {
                    continue;
                }
                preparation.order.position(id).map(|ordinal| {
                    (
                        id,
                        preparation.relative.join(app.model.node(id).name.as_ref()),
                        ordinal,
                    )
                })
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
            BatchEvent::Failures(errors) => {
                status.paint = true;
                app.delete_notices.push_back(DeleteReport {
                    target: printable(app.report_path(preparation.directory).as_os_str()),
                    errors,
                });
            }
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
        let preparation_directory = preparation.directory;
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
                app.delete_notices.push_back(DeleteReport::single(
                    app.report_path(preparation_directory),
                    message,
                ));
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
        match worker.event() {
            Some(BatchEvent::Failures(errors)) => {
                // Cancellation discards unqueued capture failures, but their
                // unbounded payload must still be reclaimed off the input thread.
                app.retired_delete_notices.push_back(DeleteReport {
                    target: String::new(),
                    errors,
                });
            }
            Some(_) => {}
            None => {
                empty = true;
                break;
            }
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

/// Persistent order and membership keep both wide admission and retries bounded.
#[derive(Clone, Default)]
struct RefreshRequests {
    order: im::Vector<NodeId>,
    members: im::HashSet<NodeId>,
    inserted: u64,
}

impl RefreshRequests {
    fn len(&self) -> usize {
        self.order.len()
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn request(&mut self, id: NodeId) {
        if self.members.insert(id).is_none() {
            self.order.push_back(id);
            self.inserted = self.inserted.wrapping_add(1);
        }
    }

    fn front(&self) -> Option<NodeId> {
        self.order.front().copied()
    }

    fn pop_front(&mut self) -> Option<NodeId> {
        let id = self.order.pop_front()?;
        self.members.remove(&id);
        Some(id)
    }

    fn defer_front(&mut self) {
        if let Some(id) = self.order.pop_front() {
            // A blocked request stays indexed while it moves behind its peers.
            self.order.push_back(id);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RefreshDependencies {
    model: u64,
    inserted: u64,
    anchor_ready: bool,
    scopes: usize,
    batch: Option<(NodeId, u64)>,
    deferred: Option<(NodeId, u64)>,
}

impl RefreshDependencies {
    fn capture(
        app: &App,
        scanner: &ScanHandle,
        scopes: &[ScopedScan],
        batch: Option<&BatchPreparation>,
        requests: &RefreshRequests,
    ) -> Self {
        Self {
            model: app.model.revision(),
            inserted: requests.inserted,
            anchor_ready: scanner.anchor.get().is_some(),
            scopes: scopes.len(),
            batch: batch.map(|batch| (batch.directory, batch.epoch)),
            deferred: app
                .deferred_batch
                .as_ref()
                .map(|request| (request.directory, request.epoch)),
        }
    }
}

#[derive(Default)]
struct RefreshRetry {
    dependencies: Option<RefreshDependencies>,
    remaining: usize,
}

impl RefreshRetry {
    fn invalidate(&mut self) {
        self.dependencies = None;
    }

    fn pump(
        &mut self,
        dependencies: RefreshDependencies,
        requests: &mut RefreshRequests,
        mut begin: impl FnMut(NodeId) -> bool,
    ) {
        if self.dependencies != Some(dependencies) {
            self.dependencies = Some(dependencies);
            self.remaining = requests.len();
        }
        // Rotate each blocked request once, spread across bounded UI turns.
        // An unchanged paused queue then costs no path or overlap checks.
        for _ in 0..self.remaining.min(MAX_BATCHES) {
            let Some(id) = requests.front() else {
                self.remaining = 0;
                break;
            };
            self.remaining -= 1;
            if begin(id) {
                requests.pop_front();
            } else {
                requests.defer_front();
            }
        }
    }
}

fn retire_refresh_requests_with(
    retired: &mut im::Vector<RefreshRequests>,
    retire: impl FnOnce(im::Vector<RefreshRequests>) -> Result<(), im::Vector<RefreshRequests>>,
) {
    if !retired.is_empty() {
        if let Err(returned) = retire(std::mem::take(retired)) {
            *retired = returned;
        }
    }
}

fn clear_refresh_requests_with(
    requests: &mut RefreshRequests,
    retired: &mut im::Vector<RefreshRequests>,
    retire: impl FnOnce(im::Vector<RefreshRequests>) -> Result<(), im::Vector<RefreshRequests>>,
) {
    if !requests.is_empty() {
        // Repeated quit warnings can race a busy disposer. Retain their roots
        // without growing or destroying a whole collection on the input thread.
        retired.push_back(std::mem::take(requests));
    }
    retire_refresh_requests_with(retired, retire);
}

fn request_manual_refresh(requests: &mut RefreshRequests, scans_stopped: &mut bool, id: NodeId) {
    // Explicit refresh resumes survivor reconciliation as soon as requested,
    // even while a queued deletion or an old scanner delays its admission.
    *scans_stopped = false;
    requests.request(id);
}

fn finish_deletion(
    app: &mut App,
    id: NodeId,
    result: delete::DeleteResult,
    requests: &mut RefreshRequests,
    refresh_allowed: bool,
) {
    if app.deletion_finished(id, result)
        && refresh_allowed
        && app.model.is_live(id)
        && app.model.node(id).kind == NodeKind::Directory
    {
        requests.request(id);
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

fn prioritize_deletion(app: &mut App, scanner: &ScanHandle, scopes: &[ScopedScan], deleting: bool) {
    if deleting || matches!(app.modal, Some(Modal::Quit { .. } | Modal::Stopping)) {
        app.file_preview.clear();
    }
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
    app.file_anchor = Some(Arc::clone(&scanner.anchor));
    let mut queue = DeleteQueue::new()?;
    #[cfg(target_os = "linux")]
    let mut input_waiter = crate::terminal_input::Waiter::new()?;
    let mut scopes = Vec::new();
    let scope_retirer = Retirer::<Vec<ScopedScan>>::new()?;
    let mut batch: Option<BatchPreparation> = None;
    let mut retired_batches = VecDeque::new();
    let mut refresh_requests = RefreshRequests::default();
    let mut refresh_retry = RefreshRetry::default();
    let mut retired_refresh_requests = im::Vector::new();
    let refresh_retirer = Retirer::<im::Vector<RefreshRequests>>::new()?;
    let mut generation = 0;
    let mut quitting = false;
    let mut closing = false;
    let mut scans_stopped = false;
    let mut shutdown_deadline = None;
    let mut failure = None;
    app.started = true;
    let mut next_frame = Instant::now();
    let mut frame_gate = FrameGate::new(&app);
    let mut background_phase = 0;
    let mut order_backlog = true;
    let mut input_timeout = Duration::from_millis(1);
    'events: loop {
        frame_gate.observe_refreshing(!scopes.is_empty() || !refresh_requests.is_empty());
        frame_gate.observe_activity(
            app.frame_work()
                || !scopes.is_empty()
                || queue.active().is_some()
                || queue.is_updating()
                || batch.as_ref().is_some_and(|preparation| {
                    preparation.dispatching || preparation.confirm_pending
                })
                || quitting,
        );
        app.pump_confirmation();
        let selection_progress = app.pump_visual();
        if selection_progress && !app.visual.is_pending() {
            frame_gate.dirty = true;
            next_frame = Instant::now();
        }
        let queue_progress = queue.pump(128);
        if queue_progress {
            refresh_retry.invalidate();
        }
        pump_retired_batches(&mut app, &mut retired_batches);
        retire_refresh_requests_with(&mut retired_refresh_requests, |retired| {
            refresh_retirer.try_retire(retired)
        });
        if !quitting && !closing && failure.is_none() {
            resume_batch(&mut app, &mut batch, &retired_batches);
        }
        if !quitting && failure.is_some() {
            quitting = true;
            shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
            cancel_batch(&mut app, &mut batch, &mut retired_batches);
            stop_work(&mut app, &scanner, &scopes, &mut queue);
            frame_gate.dirty = true;
            next_frame = Instant::now();
        }
        if (quitting
            && (queue.active().is_none()
                || shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline)))
            || (closing && !queue.is_busy() && batch.is_none())
        {
            break;
        }
        prioritize_deletion(&mut app, &scanner, &scopes, queue.active().is_some());
        let interrupt = take_interrupt(&interrupts);
        let input = if interrupt {
            Ok(true)
        } else {
            #[cfg(target_os = "linux")]
            let ready = input_waiter.poll(
                input_timeout,
                queue.completion_fd(),
                queue.active().is_some(),
            );
            #[cfg(not(target_os = "linux"))]
            // Crossterm's Unix reader skips raw and buffered input with a zero budget.
            let ready = event::poll(input_timeout.max(Duration::from_millis(1)));
            ready
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
                        // Keys can replace queue membership without changing its length.
                        refresh_retry.invalidate();
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
                        match app.key(key, anchor, deletion_busy(&queue, batch.as_ref())) {
                            Action::QuitWarning => {
                                cancel_unconfirmed(&mut app, &mut batch, &mut retired_batches);
                                closing = true;
                                scans_stopped = true;
                                stop_scanning(&scanner, &scopes);
                                clear_refresh_requests_with(
                                    &mut refresh_requests,
                                    &mut retired_refresh_requests,
                                    |retired| refresh_retirer.try_retire(retired),
                                );
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
                            Action::Refresh(id) => request_manual_refresh(
                                &mut refresh_requests,
                                &mut scans_stopped,
                                id,
                            ),
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
                                            refresh_requests.request(entry.node);
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
                        prioritize_deletion(&mut app, &scanner, &scopes, queue.active().is_some());
                        app.pump_visual();
                        app.show_delete_notice();
                        prioritize_requested(&mut app, &scanner, &scopes);
                        // Paint input immediately, before bounded background event work.
                        if failure.is_none() {
                            if let Err(error) = terminal.draw(|frame| {
                                app.sync_queue(&mut queue, frame.area());
                                frame_gate.painting(&app);
                                app.draw(
                                    frame,
                                    !scopes.is_empty() || !refresh_requests.is_empty(),
                                    deletion_busy(&queue, batch.as_ref()),
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
                        if let Err(error) = terminal.draw(|frame| {
                            app.sync_queue(&mut queue, frame.area());
                            frame_gate.painting(&app);
                            app.draw(
                                frame,
                                !scopes.is_empty() || !refresh_requests.is_empty(),
                                deletion_busy(&queue, batch.as_ref()),
                            )
                        }) {
                            failure = Some(error);
                        }
                        next_frame = Instant::now() + FRAME_INTERVAL;
                    }
                    Event::Resize(_, _) => {
                        frame_gate.dirty = true;
                        next_frame = Instant::now();
                    }
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
        if batch_status.productive {
            refresh_retry.invalidate();
        }
        if batch_status.paint {
            frame_gate.dirty = true;
            next_frame = Instant::now();
        }
        let completion = queue.poll();
        let completed_deletion = completion.is_some();
        if completed_deletion {
            refresh_retry.invalidate();
        }
        if let Some(completion) = completion {
            let error_dialog_open = matches!(app.modal, Some(Modal::DeleteErrors { .. }));
            let id = completion.entry.node;
            finish_deletion(
                &mut app,
                id,
                completion.result,
                &mut refresh_requests,
                !quitting && !scans_stopped,
            );
            // Many tiny targets may complete in one frame interval. Their model
            // updates continue immediately; only opening a report needs a frame.
            if !error_dialog_open && matches!(app.modal, Some(Modal::DeleteErrors { .. })) {
                frame_gate.dirty = true;
                next_frame = Instant::now();
            }
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
            if completed_deletion && queue.active().is_none() {
                frame_gate.dirty = true;
                next_frame = Instant::now();
            }
            if !closing && queue.active().is_none() {
                let dependencies = RefreshDependencies::capture(
                    &app,
                    &scanner,
                    &scopes,
                    batch.as_ref(),
                    &refresh_requests,
                );
                refresh_retry.pump(dependencies, &mut refresh_requests, |id| {
                    begin_refresh(
                        &mut app,
                        (&scanner, &options),
                        &mut scopes,
                        &queue,
                        batch.as_ref(),
                        id,
                        &mut generation,
                    )
                });
            }
        }
        prioritize_deletion(&mut app, &scanner, &scopes, queue.active().is_some());
        if !quitting && app.show_delete_notice() {
            frame_gate.dirty = true;
            next_frame = Instant::now();
        }
        frame_gate.observe_refreshing(!scopes.is_empty() || !refresh_requests.is_empty());
        frame_gate.observe_activity(
            app.frame_work()
                || !scopes.is_empty()
                || queue.active().is_some()
                || queue.is_updating()
                || batch.as_ref().is_some_and(|preparation| {
                    preparation.dispatching || preparation.confirm_pending
                })
                || quitting,
        );
        if Instant::now() >= next_frame {
            if frame_gate.needs_frame(&app) && failure.is_none() {
                if let Err(error) = terminal.draw(|frame| {
                    app.sync_queue(&mut queue, frame.area());
                    frame_gate.painting(&app);
                    app.draw(
                        frame,
                        !scopes.is_empty() || !refresh_requests.is_empty(),
                        deletion_busy(&queue, batch.as_ref()),
                    )
                }) {
                    failure = Some(error);
                }
            }
            // A skipped frame must still move the deadline beyond the next poll.
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
            INPUT_INTERVAL.min(next_frame.saturating_duration_since(Instant::now()))
        };
        // Keep polling input between bounded productive chunks without adding
        // one millisecond of sleep per chunk. A worker blocked on I/O or a full
        // channel still gets the ordinary minimum wait rather than a busy loop.
        input_timeout = if batch_status.productive
            || queue_progress
            || (selection_progress && app.visual.is_pending())
        {
            Duration::ZERO
        } else {
            timeout.max(Duration::from_millis(1))
        };
    }
    // External helpers must stop before App is retired off-thread. Signalling
    // owned groups does not join potentially blocked filesystem workers.
    app.file_preview.shutdown();
    scanner.cancel();
    for scope in &scopes {
        scope.handle.cancel();
    }
    // A cancelled syscall may still be blocked. Restore the terminal without
    // joining workers; process exit ends remaining in-process detached work.
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
    if failure.is_none() && !app.delete_notices.is_empty() {
        let mut message = String::from("Deletion failed:");
        for report in &app.delete_notices {
            let _ = write!(
                message,
                "\n{} ({} errors)",
                report.target,
                report.errors.len()
            );
            if let Some(error) = report.errors.first() {
                let _ = write!(
                    message,
                    "\n{}: {}",
                    printable(error.path.as_os_str()),
                    printable(OsStr::new(&error.message))
                );
            }
        }
        failure = Some(io::Error::other(message));
    }
    // Reclaim large models, cached views, queues and scoped state off-thread.
    // If thread creation fails, dropping the closure must not synchronously
    // reclaim them here: the exiting process will release those allocations.
    let cleanup = std::mem::ManuallyDrop::new((
        app,
        frame_gate,
        queue,
        batch,
        retired_batches,
        scopes,
        refresh_requests,
        retired_refresh_requests,
    ));
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

fn scroll_report_key(key: KeyCode, scroll: &mut usize, page: usize) {
    match key {
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
        KeyCode::PageDown => *scroll = scroll.saturating_add(page),
        KeyCode::Home => *scroll = 0,
        KeyCode::End => *scroll = usize::MAX,
        _ => {}
    }
}

/// Clip by terminal cells without walking any other error rows.
fn horizontal_slice(text: &str, offset: usize, width: usize) -> String {
    let line = Line::from(text);
    let mut output = String::new();
    let mut position: usize = 0;
    let end = offset.saturating_add(width);
    for grapheme in line.styled_graphemes(Style::default()) {
        let size = Span::raw(grapheme.symbol).width();
        if position >= end {
            break;
        }
        let next = position.saturating_add(size);
        if position >= offset && next <= end {
            output.push_str(grapheme.symbol);
        } else if position < offset && next > offset {
            output.extend(std::iter::repeat_n(' ', next.min(end) - offset));
        }
        position = next;
    }
    output
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
fn clipped_path_tail(text: &str, width: usize) -> String {
    if Line::from(text).width() <= width {
        return text.to_owned();
    }
    if width < 2 {
        return ".".repeat(width);
    }
    let tail = clipped_tail(text, width - 1);
    format!("..{}", tail.strip_prefix('…').unwrap_or(&tail))
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
mod tests;
