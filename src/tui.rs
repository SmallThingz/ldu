use crate::backend::RootAnchor;
use crate::batch::{BatchCapture, BatchEvent};
use crate::config::Settings;
use crate::delete::{self, ConfirmedTarget, DeleteError};
use crate::model::Model;
use crate::order::{CachedOrderWorker, CachedRow, CachedSnapshot, OrderConfig};
use crate::preview::{FileContents, FilePreview};
use crate::queue::{DeleteQueue, DeleteState, QueueId, QueueMarks};
use crate::retire::Retirer;
use crate::scan::{self, ScanHandle, ScanOptions};
use crate::selection::{RangeMode, VisualSelection};
use crate::theme::Theme;
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
use ratatui::layout::{Alignment, Direction, Rect};
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
use std::sync::atomic::{AtomicBool, Ordering};
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

// Ratatui prints cursor-restoration errors to stderr from Terminal::drop.
// After a PTY hangup both streams may be closed, so that diagnostic can panic.
// TerminalGuard owns restoration; suppress only the redundant cleanup writes.
struct TerminalOutput<'a, W> {
    writer: W,
    cleaning_up: &'a std::cell::Cell<bool>,
}

impl<W: io::Write> io::Write for TerminalOutput<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cleaning_up.get() {
            Ok(bytes.len())
        } else {
            self.writer.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.cleaning_up.get() {
            Ok(())
        } else {
            self.writer.flush()
        }
    }
}

struct TerminalOutputCleanup<'a>(&'a std::cell::Cell<bool>);

impl Drop for TerminalOutputCleanup<'_> {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

struct TerminalGuard {
    previous_hook: Option<Arc<PanicHook>>,
    mouse: bool,
}
impl TerminalGuard {
    fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self {
            previous_hook: None,
            mouse,
        };
        crate::terminal_input::configure_read()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        if mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        let previous = Arc::new(panic::take_hook());
        let hook = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore_terminal(mouse);
            hook(info);
        }));
        guard.previous_hook = Some(previous);
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(self.mouse);
        if !std::thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            drop(panic::take_hook());
            if let Ok(previous) = Arc::try_unwrap(previous) {
                panic::set_hook(previous);
            }
        }
    }
}
fn restore_terminal(mouse: bool) {
    let _ = disable_raw_mode();
    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
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
        accent: Color,
    ) -> Vec<Line<'static>> {
        (scroll..scroll.saturating_add(height).min(self.lines()))
            .map(|row| {
                let error = &self.errors[row / 2];
                let text = if row % 2 == 0 {
                    printable(error.path.as_os_str())
                } else {
                    printable(OsStr::new(&error.message))
                };
                Line::from(horizontal_slice(&text, horizontal, width))
                    .style(Style::default().fg(if row % 2 == 0 { accent } else { Color::White }))
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
        confirm_requested: bool,
        scroll: u16,
    },
    Enqueueing,
    ConfirmQuit {
        previous: Box<Option<Modal>>,
        reviewable: bool,
    },
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
        confirm_requested: bool,
        confirmed: Option<ConfirmedTarget>,
        capture: Option<Receiver<io::Result<ConfirmedTarget>>>,
        guard_until: Option<Instant>,
        mouse_guarded: bool,
    },
}
impl Modal {
    fn underlying(&self) -> &Self {
        match self {
            Self::ConfirmQuit { previous, .. } => {
                previous.as_ref().as_ref().map_or(self, Self::underlying)
            }
            _ => self,
        }
    }

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
fn underlying_modal_slot(slot: &mut Option<Modal>) -> &mut Option<Modal> {
    match slot {
        Some(Modal::ConfirmQuit { previous, .. }) => underlying_modal_slot(previous),
        _ => slot,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseControl {
    Help,
    Queue,
    VisualDelete,
    SelectionClear,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ControlPress {
    control: MouseControl,
    area: Rect,
    position: (u16, u16),
    dialog: Option<(std::mem::Discriminant<Modal>, usize)>,
}

#[derive(Clone, Copy)]
struct HeaderRightPress {
    position: (u16, u16),
    frame: Rect,
    dialog: Option<(std::mem::Discriminant<Modal>, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DialogOption {
    key: KeyEvent,
    area: Rect,
}

#[derive(Clone, Copy)]
struct DialogOptionPress {
    option: DialogOption,
    position: (u16, u16),
    dialog: Option<(std::mem::Discriminant<Modal>, usize)>,
    frame: Rect,
    queue_target: Option<QueueId>,
}

#[derive(Default)]
struct DialogMouse {
    accent: Color,
    hover: Style,
    guard_warning: bool,
    options: [Option<DialogOption>; 6],
    press: Option<DialogOptionPress>,
    hovered: Option<KeyEvent>,
    popup: Rect,
    scroll_area: Rect,
    scroll_max: usize,
}

impl DialogMouse {
    fn option(&mut self, area: Rect, row: u16, key: KeyEvent, line: &Line<'_>) {
        let width = line.width();
        // Never turn a clipped or unpainted choice into a destructive hitbox.
        if row >= area.height || width == 0 || width > usize::from(area.width) {
            return;
        }
        if let Some(slot) = self.options.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(DialogOption {
                key,
                area: Rect::new(area.x, area.y + row, width as u16, 1),
            });
        }
    }

    fn scroll(&mut self, popup: Rect, lines: usize, height: u16) {
        self.scroll_area = popup;
        self.scroll_max = lines.saturating_sub(usize::from(height));
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
    origin_selected: bool,
    mode: RangeMode,
    edge: isize,
    view: Arc<CachedSnapshot>,
    offset: usize,
}

#[derive(Clone, Copy)]
struct ParentSelection {
    epoch: u64,
    initial_end: usize,
    cursor: Option<NodeId>,
    membership_revision: u64,
    order_revision: u64,
    order_cursor: usize,
    order_rejected: bool,
    all_selected: bool,
}

#[derive(Clone, Copy)]
struct ParentPress {
    directory: NodeId,
    area: Rect,
    position: (u16, u16),
    activate: bool,
}

#[derive(Clone, Copy)]
struct PendingHover {
    column: u16,
    row: u16,
    directory: NodeId,
    child_epoch: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserTarget {
    Entry(NodeId),
    Parent,
}

struct RightPress {
    target: BrowserTarget,
    directory: NodeId,
    epoch: u64,
    position: (u16, u16),
    area: Rect,
    view: Arc<CachedSnapshot>,
    offset: usize,
    valid: bool,
    modifiers: KeyModifiers,
    origin_selected: bool,
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
    SelectStable(NodeId, bool),
    SelectParent(bool),
    ToggleStable(NodeId),
    FocusStable(NodeId),
    ToggleParent,
    Parent(Option<bool>),
    Hover(PendingHover),
    Mouse(PendingMouse),
}

struct QueueDrag {
    id: QueueId,
    row: u16,
    pressed: (u16, u16),
    button: MouseButton,
    active: bool,
    moved: bool,
    edge: isize,
    next_step: Instant,
}

#[derive(Default)]
struct QueueView {
    status: String,
    marks: QueueMarks,
    cancelling: bool,
    pending: usize,
    paused: bool,
    active_target: Option<(QueueId, String)>,
    active_progress: delete::DeleteProgress,
    active_total: u64,
    rows: Vec<(QueueId, bool, String)>,
    selected: Option<QueueId>,
    // Endpoints are exact; the linked queue uses a midpoint when both ends are hidden.
    scroll_position: usize,
    progress_scroll: u16,
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
    parent_selected: bool,
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
            parent_selected: app.parent_selected,
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
        self.model_revision == app.model.revision()
            && self.directory == app.directory
            && self.selected == app.selected
            && self.parent_selected == app.parent_selected
            && self.offset == app.offset
            && Arc::ptr_eq(&self.view, &app.view)
            && if app.filter.is_empty() {
                app.order
                    .snapshot_matches(app.directory, self.available.as_ref())
            } else {
                app.order
                    .filtered_snapshot_matches(app.directory, self.available.as_ref())
            }
            && match app.preview.directory {
                Some(id) => app.order.snapshot_matches(id, self.preview.as_ref()),
                None => self.preview.is_none(),
            }
            && app.file_preview.snapshot_matches(self.file.as_ref())
            && self.modal == app.modal.as_ref().map(std::mem::discriminant)
            && self.capturing == app.capture_inflight.is_some()
            && self.confirmed
                == matches!(
                    app.modal,
                    Some(Modal::Delete {
                        confirmed: Some(_),
                        ..
                    })
                )
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

struct BreadcrumbPart {
    text: String,
    width: usize,
    target: Option<NodeId>,
    separator: bool,
}

#[derive(Default)]
struct Breadcrumbs {
    directory: Option<NodeId>,
    parts: Vec<BreadcrumbPart>,
    width: usize,
    hits: Vec<(Rect, NodeId)>,
    hovered: Option<NodeId>,
    pressed: Option<(NodeId, NodeId, Rect, (u16, u16))>,
}

impl Breadcrumbs {
    fn sync(&mut self, root: &std::path::Path, model: &Model, directory: NodeId) {
        // Names and ancestry stay fixed for a node ID. Rebuild only when the
        // displayed directory changes, not for progress or hover redraws.
        if self.directory == Some(directory) {
            return;
        }
        self.directory = Some(directory);
        self.parts.clear();
        self.width = 0;
        self.hits.clear();
        self.hovered = None;
        self.pressed = None;
        let mut components = root.components().peekable();
        while let Some(component) = components.next() {
            self.push(
                printable(component.as_os_str()),
                components.peek().is_none().then_some(ROOT),
            );
        }
        if self.parts.is_empty() {
            self.push(".".to_owned(), Some(ROOT));
        }
        let mut ancestors = Vec::new();
        let mut id = directory;
        while id != ROOT {
            ancestors.push(id);
            let Some(parent) = model.node(id).parent else {
                break;
            };
            id = parent;
        }
        for id in ancestors.into_iter().rev() {
            self.push(printable(&model.node(id).name), Some(id));
        }
    }

    fn push(&mut self, text: String, target: Option<NodeId>) {
        let separator = self.parts.last().is_some_and(|part| part.text != "/");
        let width = Line::from(text.as_str()).width();
        self.width += width + usize::from(separator);
        self.parts.push(BreadcrumbPart {
            text,
            width,
            target,
            separator,
        });
    }

    fn line(
        &mut self,
        width: usize,
        area: Rect,
        modal: bool,
        hover: Style,
        foreground: Color,
    ) -> Line<'static> {
        self.hits.clear();
        let mut line = Line::default();
        if width == 0 || area.height == 0 {
            return line;
        }
        let Some(last) = self.parts.last() else {
            return line;
        };
        if self.width > width && last.width + 2 > width {
            let text = breadcrumb_tail(&last.text, width);
            let shown = Line::from(text.as_str()).width();
            line.spans
                .push(Span::styled("…", Style::default().fg(Color::Gray)));
            if shown > 1 {
                let target = last.target;
                self.add_hit(area, 1, shown - 1, target);
                line.spans.push(Span::styled(
                    text['…'.len_utf8()..].to_owned(),
                    self.style(target, modal, hover, foreground),
                ));
            }
            return line;
        }
        let mut first = 0;
        let mut offset = 0;
        if self.width > width {
            first = self.parts.len() - 1;
            let mut used = self.parts[first].width + 2;
            while first > 0 && used + self.parts[first - 1].width < width {
                first -= 1;
                used += self.parts[first].width + 1;
            }
            line.spans
                .push(Span::styled("…/", Style::default().fg(Color::Gray)));
            offset = 2;
        }
        for index in first..self.parts.len() {
            let part = &self.parts[index];
            if part.separator && (index > first || first == 0) {
                line.spans
                    .push(Span::styled("/", Style::default().fg(Color::Gray)));
                offset += 1;
            }
            let target = part.target;
            let part_width = part.width;
            line.spans.push(Span::styled(
                part.text.clone(),
                self.style(target, modal, hover, foreground),
            ));
            self.add_hit(area, offset, part_width, target);
            offset += part_width;
        }
        line
    }

    fn style(&self, target: Option<NodeId>, modal: bool, hover: Style, foreground: Color) -> Style {
        if target.is_none() {
            Style::default()
                .fg(Color::Gray)
                .remove_modifier(Modifier::DIM)
        } else if !modal && self.hovered == target {
            hover
        } else {
            Style::default()
                .fg(foreground)
                .remove_modifier(Modifier::DIM)
        }
    }

    fn add_hit(&mut self, area: Rect, offset: usize, width: usize, target: Option<NodeId>) {
        if width > 0
            && let Some(target) = target
        {
            self.hits.push((
                Rect::new(area.x + offset as u16, area.y, width as u16, 1),
                target,
            ));
        }
    }
}

fn breadcrumb_tail(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let line = Line::from(text);
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

struct App {
    settings: Settings,
    theme: Theme,
    recent_mouse_entry: Option<(NodeId, Instant)>,
    model: Model,
    root: PathBuf,
    report_root: PathBuf,
    directory: NodeId,
    selected: Option<NodeId>,
    parent_selected: bool,
    parent_navigation: bool,
    parent_select_pending: bool,
    parent_selection: Option<ParentSelection>,
    #[cfg(test)]
    parent_select_checks: usize,
    parent_select_view: Option<Arc<CachedSnapshot>>,
    parent_area: Rect,
    parent_press: Option<ParentPress>,
    right_press: Option<RightPress>,
    right_dragging: bool,
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
    queue_area: Rect,
    queue_drag: Option<QueueDrag>,
    baseline_finished: bool,
    visual: VisualSelection,
    range_requested: bool,
    prioritize_requested: bool,
    list_area: Rect,
    breadcrumbs: Breadcrumbs,
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
    help_area: Rect,
    header_queue_area: Rect,
    header_delete_area: Rect,
    selection_clear_area: Rect,
    dialog_close_area: Rect,
    dialog_mouse: DialogMouse,
    control_frame: Rect,
    painted_control_dialog: Option<(std::mem::Discriminant<Modal>, usize)>,
    control_hovered: Option<MouseControl>,
    control_press: Option<ControlPress>,
    header_right_press: Option<HeaderRightPress>,
}

impl App {
    #[cfg(test)]
    fn new(root: PathBuf, size_mode: SizeMode) -> io::Result<Self> {
        Self::with_settings(root, size_mode, Settings::default())
    }

    fn with_settings(root: PathBuf, size_mode: SizeMode, settings: Settings) -> io::Result<Self> {
        let theme = Theme::detect(settings.color);
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
            settings,
            theme,
            recent_mouse_entry: None,
            model: Model::new(root.clone()),
            report_root: std::path::absolute(&root)?,
            root,
            directory: ROOT,
            selected: None,
            parent_selected: false,
            parent_navigation: false,
            parent_select_pending: false,
            parent_selection: None,
            #[cfg(test)]
            parent_select_checks: 0,
            parent_select_view: None,
            parent_area: Rect::default(),
            parent_press: None,
            right_press: None,
            right_dragging: false,
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
            queue_area: Rect::default(),
            queue_drag: None,
            baseline_finished: false,
            visual: VisualSelection::new(ROOT)?,
            range_requested: false,
            prioritize_requested: false,
            list_area: Rect::default(),
            breadcrumbs: Breadcrumbs::default(),
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
            help_area: Rect::default(),
            header_queue_area: Rect::default(),
            header_delete_area: Rect::default(),
            selection_clear_area: Rect::default(),
            dialog_close_area: Rect::default(),
            dialog_mouse: DialogMouse::default(),
            control_frame: Rect::default(),
            painted_control_dialog: None,
            control_hovered: None,
            control_press: None,
            header_right_press: None,
        })
    }

    fn reset_order(&mut self, keep_previous: bool) {
        if !keep_previous {
            self.recent_mouse_entry = None;
            self.parent_selected = false;
            self.parent_navigation = false;
            self.parent_press = None;
        }
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
            let live = self.model.is_live(id);
            if self.parent_select_pending
                && let Some(selection) = &mut self.parent_selection
                && selection.epoch == self.model.node(self.directory).child_epoch
                && self.model.node(id).parent == Some(self.directory)
            {
                if !live {
                    selection.membership_revision = selection.membership_revision.wrapping_add(1);
                } else if id as usize >= selection.initial_end && !self.visual.contains(id) {
                    // Entry admission is already bounded. Select only new direct
                    // children here; size updates never restart the initial walk.
                    selection.all_selected = false;
                    self.visual.select(id);
                    selection.membership_revision = selection.membership_revision.wrapping_add(1);
                    #[cfg(test)]
                    {
                        self.parent_select_checks += 1;
                    }
                }
            }
            if !live {
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
        if let Some(report) = self.retired_delete_notices.pop_front()
            && let Err(report) = self.delete_notice_retirer.try_retire(report)
        {
            self.retired_delete_notices.push_front(report);
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
        if let Some(report) = self.delete_notices.pop_front()
            && let Err(report) = self.delete_notice_retirer.try_retire(report)
        {
            self.retired_delete_notices.push_back(report);
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
        if !parent_metadata_available && let Some(parent) = parent {
            let changed = self.model.mark_metadata_uncertain(parent);
            self.changed(changed);
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
        // The first Shift-drag still needs the pressed origin's membership.
        // Keep that private witness until conversion; first motion removes the
        // dead anchor immediately after restoring the baseline.
        if let Some(baseline) = &mut self.mouse_baseline
            && !(self.drag_mode == RangeMode::Invert
                && !self.drag_started
                && self.drag_anchor == Some(id))
        {
            baseline.remove(&id);
        }
        for modal in [&mut self.modal, &mut self.previous_modal] {
            let modal = underlying_modal_slot(modal);
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
        if (self.mouse_press_view.is_some() || self.has_held_mouse() || self.right_press.is_some())
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
        if !traversing
            && let Some(view) = self
                .current_snapshot()
                .filter(|view| view.child_epoch == self.model.node(self.directory).child_epoch)
            && !Arc::ptr_eq(&self.view, &view)
        {
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
            if self.selected.is_none() && !self.parent_selected && self.pending_navigation.is_none()
            {
                self.pending_navigation = Some(Navigation::new(Some(false), 0));
            }
            if let (Some(row), Some(index)) = (row, self.selection_index()) {
                self.offset = self.offset_for_row(index, row);
            }
            if self.mouse_press_view.is_none() && !self.has_held_mouse() {
                self.scroll_selection(if self.hover_position.is_some() || self.cursor_pending() {
                    0
                } else {
                    SCROLL_MARGIN
                });
            } else {
                self.keep_selection_visible();
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
        self.parent_selected = false;
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
        if self.parent_selected {
            return;
        }
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

    fn focus_parent(&mut self, top: bool) {
        self.parent_selected = true;
        self.parent_navigation = false;
        self.selected = None;
        self.cursor_mode = CursorMode::Tracked;
        self.pending_navigation = None;
        self.range_requested = false;
        self.focus_after_selection = false;
        self.hover_position = None;
        if top {
            self.offset = 0;
        }
        // A queued Enter follows the synthetic row without binding a fake ID.
        for intent in &mut self.visual_intents {
            match intent {
                VisualIntent::ToggleCurrent => *intent = VisualIntent::ToggleParent,
                VisualIntent::ToggleParent => {}
                _ => break,
            }
        }
    }

    fn browser_edge_selection(&mut self, end: bool) {
        if end {
            self.parent_navigation = false;
            self.edge_selection(true);
        } else {
            let select = self.range_requested && self.visual.active();
            self.focus_parent(true);
            if select {
                self.select_parent_contents(false);
            }
        }
    }

    fn browser_move_selection(&mut self, amount: isize) {
        if self.parent_selected {
            if amount <= 0
                || !self
                    .model
                    .has_children(self.directory, false, self.config.show_hidden)
                || (!self.filter.is_empty()
                    && !Arc::ptr_eq(&self.view, &self.empty)
                    && self.view.ids.is_empty())
            {
                return;
            }
            self.parent_selected = false;
            self.parent_navigation = false;
            self.pending_navigation = Some(Navigation::new(Some(false), amount - 1));
            self.advance_navigation();
        } else {
            self.parent_navigation = true;
            self.move_selection(amount);
        }
    }

    fn select_parent_contents(&mut self, remove: bool) {
        self.cancel_parent_selection();
        self.filter.clear();
        self.filter_editing = false;
        self.order.clear_filter();
        if !self.config.show_hidden {
            self.config.show_hidden = true;
            self.reset_order(true);
        }
        let all_selected = self.visual.active() && !self.visual.is_empty();
        if remove || !self.visual.active() {
            self.visual.enter(self.directory, None);
        } else if let Some(id) = self
            .selected
            .or_else(|| self.model.first_child(self.directory))
        {
            self.visual.reanchor(id);
        }
        self.parent_select_pending = !remove;
        self.parent_selection = (!remove).then(|| ParentSelection {
            epoch: self.model.node(self.directory).child_epoch,
            initial_end: self.model.len(),
            cursor: self.model.first_child(self.directory),
            membership_revision: 0,
            order_revision: 0,
            order_cursor: 0,
            order_rejected: false,
            all_selected,
        });
        // An empty, finished listing completes in the input turn without
        // scheduling selection work. Growing folders still wait for admission.
        if self.model.child_count(self.directory) == 0 {
            self.pump_parent_selection();
        }
    }

    fn ensure_parent_contents(&mut self) {
        self.select_parent_contents(false);
        if let Some(selection) = &mut self.parent_selection {
            // Right-click and visual entry add the group even if it was all marked.
            selection.all_selected = false;
        }
    }

    fn cancel_parent_selection(&mut self) {
        self.parent_select_pending = false;
        self.parent_selection = None;
        if let Some(view) = self.parent_select_view.take() {
            self.visual
                .retire_snapshot_and_order(im::HashSet::new(), view);
        }
    }

    fn finish_parent_selection(&mut self, incomplete: bool) {
        if !incomplete
            && self
                .parent_selection
                .is_some_and(|selection| selection.all_selected)
        {
            self.visual.enter(self.directory, None);
        }
        // Select-all is a completed action. Commit its materialized marks before
        // another Shift gesture can reuse and shrink the original empty baseline.
        if let Some(anchor) = self
            .parent_select_view
            .as_ref()
            .and_then(|view| view.ids.first())
            .copied()
            .or_else(|| self.model.first_child(self.directory))
            .filter(|&id| self.visual.contains(id))
        {
            self.visual.reanchor(anchor);
        }
        if incomplete {
            // Preserve the known marks, but reject any retained confirmation:
            // an incomplete enumeration cannot satisfy an all-entry request.
            self.clear_visual_intent();
            *self.background_modal_slot() = Some(Modal::notice(
                "Selection incomplete",
                "Folder listing incomplete. Refresh to select all entries.".to_owned(),
            ));
        } else {
            self.cancel_parent_selection();
        }
    }

    fn parent_selection_work(&self) -> bool {
        self.parent_selection.is_some_and(|selection| {
            selection.cursor.is_some()
                || (!selection.order_rejected
                    && selection.order_revision == selection.membership_revision
                    && self.parent_select_view.as_ref().is_some_and(|view| {
                        view.generation == self.order.generation()
                            && selection.order_cursor < view.ids.len()
                    }))
        })
    }

    fn pump_parent_selection(&mut self) -> bool {
        if !self.parent_select_pending {
            return false;
        }
        let Some(mut selection) = self.parent_selection else {
            self.cancel_parent_selection();
            return true;
        };
        if !self.visual.active() || selection.epoch != self.model.node(self.directory).child_epoch {
            self.cancel_parent_selection();
            return true;
        }
        let mut progressed = false;
        let mut budget = 256;
        while let Some(id) = selection.cursor {
            // Appended children are handled by changed(), including an append
            // after the initial tail was removed. Retained sibling links keep
            // the initial walk valid when an unvisited child disappears.
            if id as usize >= selection.initial_end {
                selection.cursor = None;
                break;
            }
            if budget == 0 {
                break;
            }
            budget -= 1;
            progressed = true;
            selection.cursor = self.model.next_sibling(id);
            if self.model.is_live(id) && self.model.node(id).parent == Some(self.directory) {
                selection.all_selected &= self.visual.contains(id);
                self.visual.select(id);
            }
            #[cfg(test)]
            {
                self.parent_select_checks += 1;
            }
        }
        self.parent_selection = Some(selection);
        if selection.cursor.is_some() {
            return progressed;
        }
        // Finishing direct enumeration need not wait for descendant sizes.
        let listed = self.model.listing_complete(self.directory);
        let stopped = self.model.listing_failed(self.directory) || (!listed && self.model.finished);
        if stopped {
            self.finish_parent_selection(true);
            return true;
        }
        if !listed {
            return progressed;
        }
        let count = self.model.child_count(self.directory);
        if count == 0 {
            self.finish_parent_selection(false);
            return true;
        }
        if self.visual.len() != count {
            return progressed;
        }
        // Admission can mark new children before ordering catches up. Validate
        // one complete sorted view before releasing a retained confirmation;
        // count alone cannot detect same-length replacement snapshots.
        if self
            .parent_select_view
            .as_ref()
            .is_none_or(|view| view.generation != self.order.generation())
            || selection.order_revision != selection.membership_revision
            || selection.order_rejected
        {
            let Some(view) = self.order.snapshot(self.directory).filter(|view| {
                view.child_epoch == selection.epoch
                    && view.ids.len() == count
                    && self
                        .parent_select_view
                        .as_ref()
                        .is_none_or(|attempted| !Arc::ptr_eq(attempted, view))
            }) else {
                return progressed;
            };
            if let Some(previous) = self.parent_select_view.replace(view) {
                self.visual
                    .retire_snapshot_and_order(im::HashSet::new(), previous);
            }
            selection.order_cursor = 0;
            selection.order_revision = selection.membership_revision;
            selection.order_rejected = false;
        }
        let view = self
            .parent_select_view
            .as_ref()
            .expect("parent order exists");
        for _ in 0..budget {
            let Some(&id) = view.ids.get(selection.order_cursor) else {
                break;
            };
            progressed = true;
            #[cfg(test)]
            {
                self.parent_select_checks += 1;
            }
            if !self.model.is_live(id)
                || self.model.node(id).parent != Some(self.directory)
                || !self.visual.contains(id)
            {
                selection.order_rejected = true;
                break;
            }
            selection.order_cursor += 1;
        }
        self.parent_selection = Some(selection);
        if !selection.order_rejected && selection.order_cursor == view.ids.len() {
            self.view = Arc::clone(view);
            self.view_directory = self.directory;
            self.finish_parent_selection(false);
            return true;
        }
        progressed
    }

    fn cancel_right_press(&mut self) {
        if std::mem::take(&mut self.right_dragging) {
            self.finish_mouse_capture(false);
        }
        if let Some(press) = self.right_press.take() {
            self.visual
                .retire_snapshot_and_order(im::HashSet::new(), press.view);
        }
    }

    fn focus_clicked_entry(&mut self, id: NodeId) {
        if !self.eligible(id) || self.model.node(id).parent != Some(self.directory) {
            self.bind_visual_cursor(None);
            return;
        }
        if !self.visual.active() {
            self.visual.enter(self.directory, None);
        }
        self.parent_selected = false;
        self.selected = Some(id);
        self.cursor_mode = CursorMode::Tracked;
        self.pending_navigation = None;
        self.range_requested = false;
        self.visual.reanchor(id);
        self.bind_visual_cursor(Some(id));
        self.prioritize_requested = true;
    }

    fn right_browser_mouse(&mut self, mouse: MouseEvent) -> bool {
        let right_event = matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Right)
                | MouseEventKind::Up(MouseButton::Right)
                | MouseEventKind::Drag(MouseButton::Right)
        );
        if self.modal.is_some() {
            self.cancel_right_press();
            return right_event;
        }
        if self.right_dragging {
            let kind = match mouse.kind {
                MouseEventKind::Drag(MouseButton::Right) => {
                    Some(MouseEventKind::Drag(MouseButton::Left))
                }
                MouseEventKind::Up(MouseButton::Right) => {
                    self.right_dragging = false;
                    Some(MouseEventKind::Up(MouseButton::Left))
                }
                MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.cancel_right_press();
                    None
                }
                MouseEventKind::Up(MouseButton::Left) => return true,
                _ => None,
            };
            if let Some(kind) = kind {
                self.mouse(MouseEvent { kind, ..mouse });
                return true;
            }
        }
        if let Some(mut press) = self.right_press.take() {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Right)
                | MouseEventKind::Up(MouseButton::Right)
                    if mouse.kind == MouseEventKind::Drag(MouseButton::Right)
                        || press.position != (mouse.column, mouse.row) =>
                {
                    let released = mouse.kind == MouseEventKind::Up(MouseButton::Right);
                    let valid = press.valid
                        && press.directory == self.directory
                        && press.epoch == self.model.node(self.directory).child_epoch
                        && press.area
                            == match press.target {
                                BrowserTarget::Entry(_) => self.list_area,
                                BrowserTarget::Parent => self.parent_area,
                            };
                    if valid {
                        match press.target {
                            BrowserTarget::Entry(id) => {
                                let mode = if press.modifiers.contains(KeyModifiers::CONTROL) {
                                    RangeMode::Remove
                                } else if press.modifiers.contains(KeyModifiers::SHIFT) {
                                    RangeMode::Invert
                                } else {
                                    RangeMode::Add
                                };
                                // Reuse the bounded range queue without a synthetic
                                // click. It resolves membership after earlier input
                                // and retains the origin if that row disappears.
                                self.right_dragging =
                                    self.try_enqueue_visual(VisualIntent::Mouse(PendingMouse {
                                        anchor: id,
                                        lead: id,
                                        dragged: true,
                                        released: false,
                                        origin_selected: press.origin_selected,
                                        mode,
                                        edge: 0,
                                        view: Arc::clone(&press.view),
                                        offset: press.offset,
                                    }));
                                if self.right_dragging {
                                    self.mouse_position = Some(press.position);
                                    self.mouse(MouseEvent {
                                        kind: MouseEventKind::Drag(MouseButton::Left),
                                        ..mouse
                                    });
                                    if released {
                                        self.right_dragging = false;
                                        self.mouse(MouseEvent {
                                            kind: MouseEventKind::Up(MouseButton::Left),
                                            ..mouse
                                        });
                                    }
                                }
                            }
                            BrowserTarget::Parent => {
                                let remove = press.modifiers.contains(KeyModifiers::CONTROL);
                                if self.visual_busy() {
                                    self.try_enqueue_visual(VisualIntent::Parent(Some(remove)));
                                } else {
                                    self.select_parent_contents(remove);
                                }
                            }
                        }
                    }
                    self.visual
                        .retire_snapshot_and_order(im::HashSet::new(), press.view);
                    return true;
                }
                MouseEventKind::Up(MouseButton::Right) => {
                    // Mark only a completed click. A drag keeps its original
                    // membership so entering visual mode cannot reverse it.
                    let valid = press.valid
                        && press.directory == self.directory
                        && press.epoch == self.model.node(self.directory).child_epoch
                        && press.area
                            == match press.target {
                                BrowserTarget::Entry(_) => self.list_area,
                                BrowserTarget::Parent => self.parent_area,
                            };
                    if valid {
                        let remove = press.modifiers.contains(KeyModifiers::CONTROL);
                        match press.target {
                            BrowserTarget::Entry(id) if self.visual_busy() => {
                                self.try_enqueue_visual(VisualIntent::SelectStable(id, remove));
                            }
                            BrowserTarget::Entry(id)
                                if self.eligible(id)
                                    && self.model.node(id).parent == Some(self.directory) =>
                            {
                                if remove {
                                    self.visual.deselect(id);
                                } else {
                                    self.visual.select(id);
                                }
                            }
                            BrowserTarget::Parent if self.visual_busy() => {
                                self.try_enqueue_visual(VisualIntent::SelectParent(remove));
                            }
                            BrowserTarget::Parent => {
                                if remove {
                                    self.select_parent_contents(true);
                                } else {
                                    self.ensure_parent_contents();
                                }
                            }
                            _ => {}
                        }
                    }
                    self.visual
                        .retire_snapshot_and_order(im::HashSet::new(), press.view);
                    return true;
                }
                MouseEventKind::Moved => {
                    press.valid &= press.position == (mouse.column, mouse.row);
                    self.right_press = Some(press);
                    return true;
                }
                MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown => {
                    self.visual
                        .retire_snapshot_and_order(im::HashSet::new(), press.view);
                }
                _ => self.right_press = Some(press),
            }
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Right) {
            return right_event;
        }
        if self.mouse_press_view.is_some()
            || self.has_held_mouse()
            || self.parent_press.is_some()
            || self.painted_directory != self.directory
            || self.painted_view.child_epoch != self.model.node(self.directory).child_epoch
        {
            return true;
        }
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        let (target, area) = if self.parent_area.contains(position) {
            (BrowserTarget::Parent, self.parent_area)
        } else if self.list_area.contains(position) {
            let Some(&id) = self
                .painted_rows
                .get(usize::from(mouse.row - self.list_area.y))
            else {
                return true;
            };
            if !self.eligible(id) || self.model.node(id).parent != Some(self.directory) {
                return true;
            }
            (BrowserTarget::Entry(id), self.list_area)
        } else {
            return true;
        };
        if !self.visual.active() {
            self.visual.enter(self.directory, None);
        }
        let accepted = match target {
            BrowserTarget::Entry(id) if self.visual_busy() => {
                self.try_enqueue_visual(VisualIntent::FocusStable(id))
            }
            BrowserTarget::Entry(id) => {
                self.focus_clicked_entry(id);
                true
            }
            BrowserTarget::Parent if self.visual_busy() => {
                self.try_enqueue_visual(VisualIntent::Parent(None))
            }
            BrowserTarget::Parent => {
                self.focus_parent(false);
                true
            }
        };
        if !accepted {
            return true;
        }
        self.hover_position = None;
        self.normal_click = None;
        self.cursor_mode = CursorMode::Tracked;
        self.right_press = Some(RightPress {
            target,
            directory: self.directory,
            epoch: self.model.node(self.directory).child_epoch,
            position: (mouse.column, mouse.row),
            area,
            view: Arc::clone(&self.painted_view),
            offset: self.painted_offset,
            valid: true,
            modifiers: mouse.modifiers,
            origin_selected: match target {
                BrowserTarget::Entry(id) => self.visual.contains(id),
                BrowserTarget::Parent => false,
            },
        });
        true
    }

    fn parent_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.modal.is_some() {
            self.parent_press = None;
            return false;
        }
        if let Some(press) = self.parent_press.as_mut() {
            match mouse.kind {
                MouseEventKind::Up(MouseButton::Left) => {
                    let activate = press.activate
                        && press.directory == self.directory
                        && press.area == self.parent_area
                        && press.position == (mouse.column, mouse.row)
                        && self
                            .parent_area
                            .contains(ratatui::layout::Position::new(mouse.column, mouse.row));
                    self.parent_press = None;
                    if activate && !self.visual.active() {
                        self.navigate_parent();
                    }
                    return true;
                }
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                    if mouse.kind != MouseEventKind::Moved
                        || press.position != (mouse.column, mouse.row)
                    {
                        press.activate = false;
                    }
                    return true;
                }
                MouseEventKind::Down(MouseButton::Left) => self.parent_press = None,
                _ => {}
            }
        }
        if self.mouse_press_view.is_some()
            || self.has_held_mouse()
            || !self
                .parent_area
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
        {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.finish_mouse_for_input();
                let visual = self.visual.active();
                let select = visual.then_some(mouse.modifiers.contains(KeyModifiers::CONTROL));
                if visual && self.visual_busy() {
                    if !self.try_enqueue_visual(VisualIntent::Parent(select)) {
                        return true;
                    }
                } else {
                    self.focus_parent(false);
                    if let Some(remove) = select {
                        self.select_parent_contents(remove);
                    }
                }
                self.parent_press = Some(ParentPress {
                    directory: self.directory,
                    area: self.parent_area,
                    position: (mouse.column, mouse.row),
                    activate: !visual,
                });
                true
            }
            MouseEventKind::Moved => {
                if self.visual.active() && self.visual_busy() {
                    self.try_enqueue_visual(VisualIntent::Parent(None));
                } else {
                    self.focus_parent(false);
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.list_area.height > 0 => {
                // The pinned row is part of the scrollable pane's mouse surface.
                self.mouse(MouseEvent {
                    row: self.list_area.y,
                    ..mouse
                });
                if self.visual.active() && self.visual_busy() {
                    self.try_enqueue_visual(VisualIntent::Parent(None));
                } else {
                    self.focus_parent(false);
                }
                true
            }
            _ => false,
        }
    }

    fn edge_selection(&mut self, end: bool) {
        self.parent_selected = false;
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
                if self.parent_navigation && backwards && cursor.remaining > 0 {
                    let select = self.range_requested && self.visual.active();
                    self.focus_parent(true);
                    if select {
                        self.select_parent_contents(false);
                    }
                    return;
                }
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
            self.parent_selected = false;
            self.parent_navigation = false;
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
            } else if self.visual.active()
                && previous_selected != self.selected
                && let Some(id) = self.selected
            {
                self.visual.reanchor(id);
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
    fn request_quit_confirmation(&mut self) -> Action {
        if matches!(self.modal, Some(Modal::ConfirmQuit { .. })) {
            return Action::Continue;
        }
        let mut previous = self.modal.take();
        if let Some(
            Modal::Delete {
                confirm_requested,
                reviewable,
                ..
            }
            | Modal::Batch {
                confirm_requested,
                reviewable,
                ..
            },
        ) = previous.as_mut()
        {
            // Returning from an unrelated prompt requires a new confirmation.
            *confirm_requested = false;
            *reviewable = false;
        }
        self.dialog_mouse.press = None;
        self.dialog_mouse.hovered = None;
        self.modal = Some(Modal::ConfirmQuit {
            previous: Box::new(previous),
            reviewable: false,
        });
        Action::Continue
    }

    fn finishing_deletions(&self) -> bool {
        self.modal
            .as_ref()
            .is_some_and(|modal| matches!(modal.underlying(), Modal::Quit { .. }))
    }

    fn background_modal_slot(&mut self) -> &mut Option<Modal> {
        if self.finishing_deletions() {
            &mut self.previous_modal
        } else {
            underlying_modal_slot(&mut self.modal)
        }
    }

    fn approve_quit_confirmation(&mut self, deleting: bool) -> Action {
        if !deleting {
            Action::Quit
        } else if matches!(self.modal, Some(Modal::Quit { .. } | Modal::Stopping)) {
            // A previous quit request is already waiting for confirmed jobs.
            Action::Continue
        } else {
            self.previous_modal = self.modal.take();
            self.modal = Some(Modal::Quit { reviewable: false });
            Action::QuitWarning
        }
    }

    fn record_mouse_entry_change(&mut self, previous: Option<NodeId>, id: NodeId, now: Instant) {
        if self.settings.mouse && !self.visual.active() && previous != Some(id) {
            self.recent_mouse_entry = Some((id, now));
        }
    }

    fn mouse_delete_guard(&self, id: NodeId, now: Instant) -> Option<Instant> {
        let (hovered, changed_at) = self.recent_mouse_entry?;
        (self.settings.mouse
            && self.settings.mouse_debounce_ms != 0
            && self.settings.mouse_debounce_delay_ms != 0
            && hovered == id
            && now.saturating_duration_since(changed_at)
                < Duration::from_millis(self.settings.mouse_debounce_ms))
        .then(|| now.checked_add(Duration::from_millis(self.settings.mouse_debounce_delay_ms)))
        .flatten()
    }

    fn delete_guard_deadline(&self) -> Option<Instant> {
        [&self.modal, &self.previous_modal]
            .into_iter()
            .filter_map(|modal| match modal.as_ref()?.underlying() {
                Modal::Delete { guard_until, .. } => *guard_until,
                _ => None,
            })
            .min()
    }

    fn expire_delete_guard(&mut self, now: Instant) -> bool {
        if let Some(request) = self.deferred_delete.as_mut()
            && request.guard_until.is_some_and(|deadline| now >= deadline)
        {
            request.guard_until = None;
        }
        let mut repaint = false;
        for (visible, slot) in [(true, &mut self.modal), (false, &mut self.previous_modal)] {
            let direct = visible && matches!(slot, Some(Modal::Delete { .. }));
            if let Some(Modal::Delete { guard_until, .. }) = underlying_modal_slot(slot)
                && guard_until.is_some_and(|deadline| now >= deadline)
            {
                *guard_until = None;
                repaint |= direct;
            }
        }
        repaint
    }

    fn key(&mut self, key: KeyEvent, anchor: Option<Arc<RootAnchor>>, deleting: bool) -> Action {
        if self.modal.is_none()
            && (self.filter_editing
                || matches!(
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
                            'j' | 'k' | 'h' | 'l' | '<' | 'v' | 'V' | '/' | 'e' | 'r' | 'R'
                        )
                )
                || (key.code == KeyCode::Esc && (self.visual.active() || !self.filter.is_empty())))
        {
            // Navigation owns the target; display toggles and help do not.
            self.recent_mouse_entry = None;
        }
        self.breadcrumbs.pressed = None;
        self.header_right_press = None;
        self.cancel_right_press();
        self.dialog_mouse.press = None;
        self.dialog_mouse.hovered = None;
        self.parent_press = None;
        self.control_press = None;
        self.control_hovered = None;
        self.queue_drag = None;
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
        if matches!(self.modal, Some(Modal::ConfirmQuit { .. })) {
            let Some(Modal::ConfirmQuit {
                previous,
                reviewable,
            }) = self.modal.take()
            else {
                unreachable!()
            };
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.modal = *previous,
                KeyCode::Enter | KeyCode::Char('y') if reviewable => {
                    self.modal = *previous;
                    return self.approve_quit_confirmation(deleting);
                }
                _ if interrupt => {
                    self.modal = *previous;
                    return if matches!(self.modal, Some(Modal::Quit { .. } | Modal::Stopping)) {
                        Action::Quit
                    } else {
                        self.approve_quit_confirmation(deleting)
                    };
                }
                _ => {
                    self.modal = Some(Modal::ConfirmQuit {
                        previous,
                        reviewable,
                    })
                }
            }
            return Action::Continue;
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
                Modal::ConfirmQuit { .. } => unreachable!("quit confirmation handled above"),
                Modal::Quit { reviewable } => match key.code {
                    KeyCode::Enter | KeyCode::Char('q') if reviewable => return Action::Quit,
                    KeyCode::Esc | KeyCode::Char('n') => self.modal = self.previous_modal.take(),
                    other => {
                        scroll_key(other, &mut self.queue_view.progress_scroll);
                        self.modal = Some(Modal::Quit { reviewable });
                    }
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
                    mut confirm_requested,
                    mut scroll,
                } => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter if ready && reviewable && count > 0 => {
                        // Block later confirmations before returning to the event loop.
                        self.clear_visual_intent();
                        self.visual.exit();
                        self.modal = Some(Modal::Enqueueing);
                        return Action::BatchConfirm;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        self.cancel_deferred_batch();
                        return Action::BatchCancel;
                    }
                    other => {
                        if reviewable && matches!(other, KeyCode::Char('y') | KeyCode::Enter) {
                            confirm_requested = true;
                        } else {
                            scroll_key(other, &mut scroll);
                        }
                        self.modal = Some(Modal::Batch {
                            target,
                            count,
                            ready,
                            reviewable,
                            confirm_requested,
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
                    mut confirm_requested,
                    confirmed,
                    capture,
                    guard_until,
                    mouse_guarded,
                } => {
                    let guarded = guard_until.is_some_and(|deadline| Instant::now() < deadline);
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter if guarded => {
                            // Drop early input. Async identity completion must never replay it.
                            confirm_requested = false;
                        }
                        KeyCode::Char('y') | KeyCode::Enter
                            if reviewable && confirmed.is_some() =>
                        {
                            return Action::Delete(id, anchor, relative, confirmed.unwrap());
                        }
                        KeyCode::Char('n') | KeyCode::Esc => {
                            self.deferred_delete = None;
                            return Action::Continue;
                        }
                        KeyCode::Char('y') | KeyCode::Enter if reviewable => {
                            confirm_requested = true;
                        }
                        other => scroll_key(other, &mut scroll),
                    }
                    self.modal = Some(Modal::Delete {
                        id,
                        anchor,
                        relative,
                        target,
                        scroll,
                        reviewable,
                        confirm_requested,
                        confirmed,
                        capture,
                        guard_until,
                        mouse_guarded,
                    });
                }
                Modal::Text {
                    title,
                    text,
                    mut scroll,
                } => match key.code {
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
                    } else if self.parent_selected {
                        self.try_enqueue_visual(VisualIntent::ToggleParent);
                    } else if let Some(id) = self.selected
                        && !self.try_enqueue_visual(VisualIntent::ToggleStable(id))
                    {
                        self.hover_position = previous_hover;
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
                    self.track_selection();
                    let current = self.selected.filter(|&id| {
                        !self.parent_selected
                            && self.eligible(id)
                            && self.model.node(id).parent == Some(self.directory)
                    });
                    self.visual.enter(
                        self.directory,
                        current.filter(|_| self.pending_navigation.is_none()),
                    );
                    self.range_requested = false;
                    if self.pending_navigation.is_some() {
                        self.try_enqueue_visual(VisualIntent::ToggleCurrent);
                    } else if self.parent_selected {
                        self.ensure_parent_contents();
                    }
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
                self.browser_move_selection(direction * self.page_size as isize);
            }
            KeyCode::Up | KeyCode::Char('k') => self.browser_move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.browser_move_selection(1),
            KeyCode::Home => self.browser_edge_selection(false),
            KeyCode::End => self.browser_edge_selection(true),
            KeyCode::PageUp => self.browser_move_selection(-(self.page_size as isize)),
            KeyCode::PageDown => self.browser_move_selection(self.page_size as isize),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Char('<') => {
                self.navigate_parent();
            }
            KeyCode::Enter if self.visual.active() => {
                if self.parent_selected {
                    self.select_parent_contents(false);
                } else if let Some(id) = self.selected {
                    self.visual.toggle(id);
                    self.range_requested = false;
                }
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if self.parent_selected {
                    self.navigate_parent();
                } else if let Some(id) = self.selected {
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
                            let guard_until = self.mouse_delete_guard(id, Instant::now());
                            let request = DeleteRequest {
                                id,
                                anchor,
                                relative: relative.to_owned(),
                                target: printable(path.as_os_str()),
                                guard_until,
                                mouse_guarded: guard_until.is_some(),
                            };
                            if self
                                .capture_inflight
                                .as_ref()
                                .is_some_and(|finished| !finished.load(Ordering::Acquire))
                            {
                                self.modal = Some(Modal::Delete {
                                    id: request.id,
                                    anchor: Arc::clone(&request.anchor),
                                    relative: request.relative.clone(),
                                    target: request.target.clone(),
                                    scroll: 0,
                                    reviewable: false,
                                    confirm_requested: false,
                                    confirmed: None,
                                    capture: None,
                                    guard_until: request.guard_until,
                                    mouse_guarded: request.mouse_guarded,
                                });
                                self.deferred_delete = Some(request);
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
        if self.queue_view.active_target.as_ref().map(|(id, _)| *id)
            != queue.active().map(|entry| entry.id)
        {
            self.queue_view.progress_scroll = 0;
        }
        queue.sync_marks(&mut self.queue_view.marks);
        self.queue_view.cancelling = queue.is_cancelling();
        self.queue_view.pending = queue.pending().len();
        self.queue_view.paused = queue.is_paused();
        if let Some(entry) = queue.active() {
            if self.queue_view.active_target.as_ref().map(|(id, _)| *id) != Some(entry.id) {
                // Suppressed targets retain their discovered descendant counts.
                // Resolve ancestor liveness once per job, not on every frame.
                self.queue_view.active_total = if self.model.is_live(entry.node) {
                    self.model.node(entry.node).items.saturating_add(1)
                } else {
                    0
                };
                self.queue_view.active_target = Some((
                    entry.id,
                    printable(self.report_root.join(entry.relative.as_ref()).as_os_str()),
                ));
            }
        } else {
            self.queue_view.active_target = None;
            self.queue_view.active_total = 0;
        }
        self.queue_view.active_progress = queue.progress().unwrap_or_default();
        self.queue_view.active_total = self
            .queue_view
            .active_total
            .max(self.queue_view.active_progress.examined_entries);
        self.queue_view.status = if !queue.pending().is_empty() {
            format!("{} queued", queue.pending().len())
        } else if queue.active().is_some() || queue.is_updating() {
            "deleting…".to_owned()
        } else {
            String::new()
        };
        let Some(Modal::Queue { selected, offset }) = self.modal.as_mut() else {
            self.queue_view.rows.clear();
            self.queue_drag = None;
            self.queue_area = Rect::default();
            return;
        };
        let row_area = queue_layout(area).2[1];
        if self.queue_area != row_area
            || self.queue_drag.as_ref().is_some_and(|drag| {
                queue.entry(drag.id).is_none()
                    || queue.active().is_some_and(|entry| entry.id == drag.id) != drag.active
            })
        {
            self.queue_drag = None;
        }
        self.queue_area = row_area;
        if selected.is_none_or(|id| queue.entry(id).is_none()) {
            *selected = queue.first_id();
        }
        if offset.is_none_or(|id| queue.entry(id).is_none()) {
            *offset = *selected;
        }
        let visible = usize::from(queue_layout(area).2[1].height).clamp(1, 15);
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
        let last_position = (queue.pending().len() + usize::from(queue.active().is_some()))
            .saturating_sub(self.queue_view.rows.len());
        self.queue_view.scroll_position =
            if self.queue_view.rows.first().map(|row| row.0) == queue.first_id() {
                0
            } else if self.queue_view.rows.last().map(|row| row.0) == queue.last_id() {
                last_position
            } else {
                last_position.div_ceil(2)
            };
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

    /// Queue gestures use the painted IDs, never row indices into a changing queue.
    fn queue_mouse(&mut self, mouse: MouseEvent, queue: &mut DeleteQueue) -> Option<Action> {
        if !matches!(self.modal, Some(Modal::Queue { .. })) {
            self.queue_drag = None;
            return None;
        }
        self.hover_position = None;
        self.breadcrumbs.hovered = None;
        self.normal_click = None;
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        let inside = self.queue_area.contains(position);
        let inside_popup = inside || queue_layout(self.control_frame).0.contains(position);
        let painted = inside
            .then(|| {
                self.queue_view
                    .rows
                    .get(usize::from(mouse.row - self.queue_area.y))
            })
            .flatten()
            .filter(|(id, active, _)| {
                queue.entry(*id).is_some()
                    && queue.active().is_some_and(|entry| entry.id == *id) == *active
            })
            .map(|(id, active, _)| (*id, *active));
        let mut action = Action::Continue;
        match mouse.kind {
            MouseEventKind::Moved if self.queue_drag.is_none() => {
                if let (Some((id, _)), Some(Modal::Queue { selected, .. })) =
                    (painted, self.modal.as_mut())
                {
                    *selected = Some(id);
                }
            }
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                self.finish_mouse_for_input();
                self.queue_drag = None;
                if let Some((id, active)) = painted {
                    if let Some(Modal::Queue { selected, .. }) = self.modal.as_mut() {
                        *selected = Some(id);
                    }
                    if mouse.modifiers.is_empty() {
                        self.queue_drag = Some(QueueDrag {
                            id,
                            row: mouse.row,
                            pressed: (mouse.column, mouse.row),
                            button,
                            active,
                            moved: false,
                            edge: 0,
                            next_step: Instant::now(),
                        });
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self
                    .queue_drag
                    .as_ref()
                    .is_some_and(|drag| drag.button == MouseButton::Left)
                {
                    self.queue_drag_to(mouse, queue);
                } else {
                    self.queue_drag = None;
                }
            }
            MouseEventKind::Up(button @ (MouseButton::Left | MouseButton::Right)) => {
                // Some terminals omit the final drag event before release.
                if button == MouseButton::Left
                    && self.queue_drag.as_ref().is_some_and(|drag| {
                        drag.button == button && drag.pressed != (mouse.column, mouse.row)
                    })
                {
                    self.queue_drag_to(mouse, queue);
                }
                if let Some(drag) = self.queue_drag.take()
                    && drag.button == button
                    && !drag.moved
                    && drag.pressed == (mouse.column, mouse.row)
                    && painted == Some((drag.id, drag.active))
                {
                    if let Some(Modal::Queue { selected, .. }) = self.modal.as_mut() {
                        *selected = Some(drag.id);
                    }
                    action = if button == MouseButton::Right {
                        Action::QueueDelete
                    } else {
                        Action::QueuePause
                    };
                }
                // A browser press held while opening the queue still needs its release.
                self.mouse(mouse);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside_popup => {
                self.queue_drag = None;
                self.queue_navigate(
                    queue,
                    None,
                    if mouse.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    },
                );
            }
            MouseEventKind::Drag(_) | MouseEventKind::Down(_) | MouseEventKind::Moved => {
                self.queue_drag = None;
            }
            _ => {}
        }
        Some(action)
    }

    fn queue_drag_to(&mut self, mouse: MouseEvent, queue: &mut DeleteQueue) {
        let Some(drag) = self.queue_drag.as_mut() else {
            return;
        };
        drag.moved = true;
        if drag.active || queue.pending().get(drag.id).is_none() || self.queue_area.is_empty() {
            self.queue_drag = None;
            return;
        }
        drag.row = mouse.row;
        // Moving out through the side does not keep reordering off-screen.
        if mouse.column < self.queue_area.x || mouse.column >= self.queue_area.right() {
            drag.edge = 0;
            return;
        }
        let edge = if mouse.row <= self.queue_area.y {
            -1
        } else if mouse.row >= self.queue_area.bottom().saturating_sub(1) {
            1
        } else {
            0
        };
        if drag.edge != edge {
            drag.next_step = Instant::now() + FRAME_INTERVAL;
        }
        drag.edge = edge;
        let id = drag.id;
        let Some(source_row) = self.queue_view.rows.iter().position(|row| row.0 == id) else {
            self.queue_drag = None;
            return;
        };
        let target_row = usize::from(mouse.row.saturating_sub(self.queue_area.y))
            .min(self.queue_view.rows.len().saturating_sub(1));
        let Some(&(target, active, _)) = self.queue_view.rows.get(target_row) else {
            return;
        };
        let target = if active {
            queue.pending().front().map(|entry| entry.id)
        } else {
            Some(target)
        };
        let Some(target) =
            target.filter(|target| *target != id && queue.pending().get(*target).is_some())
        else {
            return;
        };
        let direction = if target_row < source_row { -1 } else { 1 };
        // At most a painted page is examined. Never partially reorder toward a
        // target that disappeared or is no longer on this side of the press.
        let mut cursor = id;
        let mut steps = 0;
        for count in 1..=self.queue_view.rows.len() {
            let next = if direction < 0 {
                queue.previous_id(cursor)
            } else {
                queue.next_id(cursor)
            };
            let Some(next) = next else { break };
            cursor = next;
            if cursor == target {
                steps = count;
                break;
            }
        }
        if steps == 0 {
            return;
        }
        for _ in 0..steps {
            if !queue.move_pending(id, direction) {
                return;
            }
        }
        self.queue_drag_viewport(queue, id, target_row.max(usize::from(active)));
    }

    fn queue_drag_viewport(&mut self, queue: &DeleteQueue, id: QueueId, row: usize) {
        let mut first = id;
        for _ in 0..row.min(14) {
            let Some(previous) = queue.previous_id(first) else {
                break;
            };
            first = previous;
        }
        if let Some(Modal::Queue { selected, offset }) = self.modal.as_mut() {
            *selected = Some(id);
            *offset = Some(first);
        }
    }

    /// One linked-list swap per timer tick keeps edge drags independent of queue length.
    fn pump_queue_drag(&mut self, queue: &mut DeleteQueue) -> bool {
        let Some(drag) = self.queue_drag.as_mut() else {
            return false;
        };
        if !matches!(self.modal, Some(Modal::Queue { .. }))
            || queue.entry(drag.id).is_none()
            || queue.active().is_some_and(|entry| entry.id == drag.id) != drag.active
            || self.queue_area.is_empty()
        {
            self.queue_drag = None;
            return false;
        }
        if drag.button != MouseButton::Left || drag.active || !drag.moved {
            return false;
        }
        let now = Instant::now();
        if drag.edge == 0 || now < drag.next_step {
            return false;
        }
        drag.next_step = now + FRAME_INTERVAL;
        let id = drag.id;
        let row = usize::from(drag.row.saturating_sub(self.queue_area.y))
            .min(usize::from(self.queue_area.height.saturating_sub(1)));
        if !queue.move_pending(id, drag.edge) {
            // A queue boundary must not keep an otherwise idle UI repainting.
            drag.edge = 0;
            return false;
        }
        self.queue_drag_viewport(queue, id, row);
        true
    }

    fn cancel_deferred_batch(&mut self) {
        let pending = self.waiting_batch.take().is_some() || self.deferred_batch.is_some();
        if pending {
            for modal in [&mut self.modal, &mut self.previous_modal] {
                let modal = underlying_modal_slot(modal);
                if matches!(modal, Some(Modal::Batch { .. })) {
                    *modal = None;
                }
            }
        }
        if let Some(request) = self.deferred_batch.take() {
            self.visual
                .retire_snapshot_and_order(request.selection, request.order);
        }
    }

    fn open_delete(&mut self, request: DeleteRequest) {
        let (confirm_requested, reviewable) = match self.modal.as_ref() {
            Some(Modal::Delete {
                id,
                relative,
                confirm_requested,
                reviewable,
                ..
            }) if *id == request.id && *relative == request.relative => {
                (*confirm_requested, *reviewable)
            }
            _ => (false, false),
        };
        if !self.model.is_live(request.id)
            || self.model.path(request.id) != self.root.join(&request.relative)
        {
            self.modal = None;
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
            reviewable,
            confirm_requested,
            confirmed: None,
            capture: Some(capture),
            guard_until: request
                .guard_until
                .filter(|deadline| Instant::now() < *deadline),
            mouse_guarded: request.mouse_guarded,
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

    fn batch_confirmation_state(&self) -> (bool, bool) {
        match self.modal {
            Some(Modal::Batch {
                confirm_requested,
                reviewable,
                ..
            }) => (confirm_requested, reviewable),
            _ => (false, false),
        }
    }

    fn confirmation_ready(&self) -> bool {
        if matches!(self.modal, Some(Modal::Delete { guard_until: Some(deadline), .. }) if Instant::now() < deadline)
        {
            return false;
        }
        matches!(
            self.modal,
            Some(
                Modal::Delete {
                    confirm_requested: true,
                    reviewable: true,
                    confirmed: Some(_),
                    ..
                } | Modal::Batch {
                    confirm_requested: true,
                    reviewable: true,
                    ready: true,
                    count: 1..,
                    ..
                }
            )
        )
    }

    fn take_ready_confirmation(&mut self) -> Action {
        if self.confirmation_ready() {
            self.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            )
        } else {
            Action::Continue
        }
    }

    fn pump_confirmation(&mut self) {
        if self
            .capture_inflight
            .as_ref()
            .is_some_and(|finished| finished.load(Ordering::Acquire))
        {
            self.capture_inflight = None;
        }
        if self.capture_inflight.is_none()
            && (self.modal.is_none()
                || matches!(
                    self.modal,
                    Some(Modal::Delete {
                        confirmed: None,
                        capture: None,
                        ..
                    })
                ))
            && let Some(request) = self.deferred_delete.take()
        {
            self.open_delete(request);
        }
        let mut error = None;
        let modal = self.background_modal_slot().as_mut();
        if let Some(Modal::Delete {
            confirmed, capture, ..
        }) = modal
            && let Some(receiver) = capture.as_ref()
        {
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
        if let Some(error) = error {
            let notice = Modal::notice("Cannot delete", printable(OsStr::new(&error)));
            *self.background_modal_slot() = Some(notice);
        }
    }

    fn visual_busy(&self) -> bool {
        self.parent_select_pending
            || self.visual.is_pending()
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
            || self.parent_selection_work()
            || self.pending_navigation.is_some()
            || self.range_requested
    }

    #[cfg(test)]
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
                        | VisualIntent::Parent(_)
                        | VisualIntent::FocusStable(_)
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
            // Stable operations do not establish a new cursor. Discard dependent
            // toggles across them, stopping at the next independent cursor intent.
            let mut dependent = true;
            self.visual_intents.retain(|intent| {
                if !dependent {
                    return true;
                }
                match intent {
                    VisualIntent::ToggleCurrent => false,
                    VisualIntent::ToggleStable(_) | VisualIntent::SelectStable(..) => true,
                    _ => {
                        dependent = false;
                        true
                    }
                }
            });
            return;
        };
        for intent in &mut self.visual_intents {
            match intent {
                VisualIntent::ToggleCurrent => *intent = VisualIntent::ToggleStable(id),
                VisualIntent::ToggleStable(_) | VisualIntent::SelectStable(..) => {}
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
        self.parent_selected = false;
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
        self.finish_mouse_capture(true);
    }

    fn finish_mouse_capture(&mut self, complete_shift_click: bool) {
        self.breadcrumbs.pressed = None;
        if !complete_shift_click
            && self
                .held_mouse()
                .is_some_and(|gesture| gesture.mode == RangeMode::Invert && !gesture.dragged)
        {
            self.cancel_held_mouse();
        }
        if let Some(gesture) = self.held_mouse() {
            gesture.released = true;
            gesture.edge = 0;
        }
        if complete_shift_click
            && self.mouse_dragging
            && !self.drag_started
            && self.drag_mode == RangeMode::Invert
            && let Some(id) = self.drag_anchor
        {
            let view = self
                .mouse_press_view
                .as_ref()
                .map_or_else(|| Arc::clone(&self.painted_view), Arc::clone);
            self.add_mouse_range(view, id);
        }
        self.retire_mouse_press();
        self.mouse_dragging = false;
        self.drag_started = false;
        self.drag_anchor = None;
        self.drag_scroll = None;
        self.mouse_position = None;
    }

    fn reject_stale_mouse_geometry(&mut self, kind: MouseEventKind, size: (u16, u16)) -> bool {
        if size == (self.control_frame.width, self.control_frame.height) {
            return false;
        }
        self.header_right_press = None;
        self.parent_press = None;
        self.dialog_mouse.press = None;
        self.control_press = None;
        self.control_hovered = None;
        self.queue_drag = None;
        let right_capture = self.right_dragging;
        self.cancel_right_press();
        self.cancel_breadcrumb_press();
        if kind == MouseEventKind::Up(MouseButton::Left)
            || (right_capture && kind == MouseEventKind::Up(MouseButton::Right))
        {
            // A physical release still ends capture when its old hitboxes are
            // unusable. Keep accepted presses/ranges, without inventing a final
            // drag endpoint, opening a directory, or completing a Shift click.
            self.finish_mouse_capture(false);
            self.hover_position = None;
        }
        true
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
            && let Some(VisualIntent::Mouse(gesture)) = self.visual_intents.remove(index)
        {
            self.visual
                .retire_snapshot_and_order(im::HashSet::new(), gesture.view);
        }
    }

    fn clear_visual_intent(&mut self) {
        self.cancel_right_press();
        self.cancel_parent_selection();
        self.parent_press = None;
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
        let parent_progress = self.pump_parent_selection();
        let scrolled = self.pump_mouse_scroll(Instant::now());
        let model = &self.model;
        let directory = self.directory;
        let hidden = self.config.show_hidden;
        let mut changed = self.visual.pump_filtered(256, |id| {
            model.is_live(id)
                && model.node(id).parent == Some(directory)
                && (hidden || !model.node(id).name.as_encoded_bytes().starts_with(b"."))
        }) || scrolled
            || parent_progress;
        for _ in 0..VISUAL_INTENT_LIMIT {
            if self.parent_select_pending
                || self.visual.is_pending()
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
                        self.browser_edge_selection(end);
                    }
                    if navigation.amount != 0 {
                        self.browser_move_selection(navigation.amount);
                    }
                }
                VisualIntent::ToggleCurrent => {
                    if self.parent_selected {
                        self.select_parent_contents(false);
                    } else if let Some(id) = self.selected.filter(|&id| self.eligible(id)) {
                        self.visual.toggle(id);
                    }
                }
                VisualIntent::SelectStable(id, remove) => {
                    if self.eligible(id) && self.model.node(id).parent == Some(self.directory) {
                        if remove {
                            self.visual.deselect(id);
                        } else {
                            self.visual.select(id);
                        }
                    }
                }
                VisualIntent::SelectParent(remove) => {
                    if remove {
                        self.select_parent_contents(true);
                    } else {
                        self.ensure_parent_contents();
                    }
                }
                VisualIntent::FocusStable(id) => self.focus_clicked_entry(id),
                VisualIntent::ToggleParent => self.select_parent_contents(false),
                VisualIntent::Parent(select) => {
                    self.focus_parent(false);
                    if let Some(remove) = select {
                        self.select_parent_contents(remove);
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
                        self.parent_selected = false;
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
                        // Live origins use selection after older queued input.
                        // Removal discards that membership, so a vanished origin
                        // falls back to the direction captured at physical press.
                        let origin_selected = if self.model.is_live(gesture.anchor) {
                            self.visual.contains(gesture.anchor)
                        } else {
                            gesture.origin_selected
                        };
                        let mode = if gesture.mode == RangeMode::Add
                            || (gesture.dragged && gesture.mode == RangeMode::Invert)
                        {
                            if origin_selected {
                                RangeMode::Remove
                            } else {
                                RangeMode::Add
                            }
                        } else {
                            gesture.mode
                        };
                        if !gesture.released {
                            self.retire_mouse_press();
                            self.mouse_baseline = Some(self.visual.snapshot());
                            self.mouse_press_view = Some(Arc::clone(&gesture.view));
                            self.view = Arc::clone(&gesture.view);
                            self.offset = gesture.offset;
                            self.drag_mode = mode;
                            self.drag_anchor = Some(gesture.anchor);
                        }
                        if gesture.dragged {
                            self.visual.reanchor(gesture.anchor);
                            if !self.model.is_live(gesture.anchor) {
                                self.visual.remove(gesture.anchor);
                            }
                            self.visual
                                .begin_range(Arc::clone(&gesture.view), gesture.lead, mode);
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
            self.parent_selected = false;
            self.selected = Some(id);
            self.bind_visual_cursor(Some(id));
        }
        self.navigation_until = Instant::now() + Duration::from_millis(150);
        self.prioritize_requested = true;
    }

    fn control_dialog(&self) -> Option<(std::mem::Discriminant<Modal>, usize)> {
        self.modal.as_ref().map(|modal| {
            // Strings move with their dialog. Their allocation identifies a
            // replacement report or notice even if it has the same variant.
            let identity = match modal {
                Modal::ConfirmQuit { previous, .. } => {
                    std::ptr::from_ref(previous.as_ref()) as usize
                }
                Modal::Text { text, .. } => text.as_ptr() as usize,
                Modal::Batch { target, .. } | Modal::Delete { target, .. } => {
                    target.as_ptr() as usize
                }
                Modal::DeleteErrors { .. } => self
                    .delete_notices
                    .front()
                    .map_or(0, |report| report.target.as_ptr() as usize),
                _ => 0,
            };
            (std::mem::discriminant(modal), identity)
        })
    }

    fn modal_option_mouse(&mut self, mouse: MouseEvent, deleting: bool) -> Option<Action> {
        let dialog = self.control_dialog();
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        let hit = (dialog.is_some() && dialog == self.painted_control_dialog)
            .then(|| {
                self.dialog_mouse
                    .options
                    .iter()
                    .flatten()
                    .copied()
                    .find(|option| option.area.contains(position))
                    .or_else(|| {
                        (matches!(
                            self.modal,
                            Some(
                                Modal::Text {
                                    title: "Help" | "Info",
                                    ..
                                } | Modal::Queue { .. }
                                    | Modal::Delete { .. }
                                    | Modal::Batch { .. }
                                    | Modal::ConfirmQuit { .. }
                            )
                        ) && !self.dialog_mouse.popup.is_empty()
                            && self.control_frame.contains(position)
                            && !self.dialog_mouse.popup.contains(position))
                        .then_some(DialogOption {
                            key: KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                            area: Rect::new(mouse.column, mouse.row, 1, 1),
                        })
                    })
            })
            .flatten();
        self.dialog_mouse.hovered = hit
            .filter(|option| self.dialog_mouse.options.contains(&Some(*option)))
            .map(|option| option.key);
        if let Some(press) = self.dialog_mouse.press {
            if press.dialog != dialog
                || (matches!(self.modal, Some(Modal::Queue { .. }))
                    && press.option.key.code == KeyCode::Char('d')
                    && press.queue_target != self.queue_selection())
                || press.frame != self.control_frame
                || hit != Some(press.option)
                || matches!(
                    mouse.kind,
                    MouseEventKind::Drag(_)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                        | MouseEventKind::ScrollLeft
                        | MouseEventKind::ScrollRight
                )
                || (mouse.kind == MouseEventKind::Moved
                    && press.position != (mouse.column, mouse.row))
            {
                self.dialog_mouse.press = None;
                self.dialog_mouse.hovered = None;
                // A cancelled option gesture must not become a queue gesture.
                return Some(Action::Continue);
            }
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                self.dialog_mouse.press = None;
                return Some(self.key(press.option.key, None, deleting));
            }
            return Some(Action::Continue);
        }
        if let Some(option) = hit {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if matches!(option.key.code, KeyCode::Enter | KeyCode::Char('y'))
                    && matches!(self.modal, Some(Modal::Delete { guard_until: Some(deadline), .. }) if Instant::now() < deadline)
                {
                    // Never carry a press begun during the hold across its deadline.
                    return Some(Action::Continue);
                }
                self.finish_mouse_for_input();
                self.parent_press = None;
                self.control_press = None;
                self.queue_drag = None;
                self.cancel_breadcrumb_press();
                self.dialog_mouse.press = Some(DialogOptionPress {
                    option,
                    position: (mouse.column, mouse.row),
                    dialog,
                    frame: self.control_frame,
                    queue_target: self.queue_selection(),
                });
                return Some(Action::Continue);
            }
            if mouse.kind == MouseEventKind::Moved && self.queue_drag.is_none() {
                return Some(Action::Continue);
            }
        }
        None
    }

    fn scroll_modal_mouse(&mut self, mouse: MouseEvent) {
        if self.control_dialog() != self.painted_control_dialog
            || !self
                .dialog_mouse
                .scroll_area
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
        {
            return;
        }
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                if let Some(Modal::DeleteErrors { horizontal, .. }) = self.modal.as_mut() {
                    *horizontal = if mouse.kind == MouseEventKind::ScrollLeft {
                        horizontal.saturating_sub(8)
                    } else {
                        horizontal.saturating_add(8)
                    };
                }
                return;
            }
            _ => return,
        };
        self.dialog_mouse.press = None;
        let max = self.dialog_mouse.scroll_max;
        match self.modal.as_mut() {
            Some(Modal::DeleteErrors { scroll, .. }) => {
                *scroll = if up {
                    scroll.saturating_sub(3)
                } else {
                    scroll.saturating_add(3).min(max)
                };
            }
            Some(Modal::Quit { .. }) => {
                let scroll = &mut self.queue_view.progress_scroll;
                *scroll = if up {
                    scroll.saturating_sub(3)
                } else {
                    scroll
                        .saturating_add(3)
                        .min(u16::try_from(max).unwrap_or(u16::MAX))
                };
            }
            Some(
                Modal::Text { scroll, .. }
                | Modal::Delete { scroll, .. }
                | Modal::Batch { scroll, .. },
            ) => {
                *scroll = if up {
                    scroll.saturating_sub(3)
                } else {
                    scroll
                        .saturating_add(3)
                        .min(u16::try_from(max).unwrap_or(u16::MAX))
                };
            }
            _ => {}
        }
    }

    fn control_at(&self, column: u16, row: u16, deleting: bool) -> Option<(MouseControl, Rect)> {
        if self.control_dialog() != self.painted_control_dialog {
            return None;
        }
        let position = ratatui::layout::Position::new(column, row);
        let controls = if self.modal.is_some() {
            [
                (MouseControl::Close, self.dialog_close_area),
                (MouseControl::Help, Rect::default()),
                (MouseControl::Queue, Rect::default()),
                (MouseControl::VisualDelete, Rect::default()),
            ]
        } else {
            [
                (MouseControl::Help, self.help_area),
                (
                    MouseControl::Queue,
                    if deleting && !self.visual.active() {
                        self.header_queue_area
                    } else {
                        Rect::default()
                    },
                ),
                (
                    MouseControl::VisualDelete,
                    if self.visual.active() {
                        self.header_delete_area
                    } else {
                        Rect::default()
                    },
                ),
                (
                    MouseControl::SelectionClear,
                    if self.visual.active() {
                        self.selection_clear_area
                    } else {
                        Rect::default()
                    },
                ),
            ]
        };
        controls
            .into_iter()
            .find(|(_, area)| area.contains(position))
    }

    fn header_right_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let header = Rect::new(
            self.control_frame.x,
            self.control_frame.y,
            self.control_frame.width,
            self.control_frame.height.min(1),
        );
        let position = (mouse.column, mouse.row);
        let inside = header.contains(ratatui::layout::Position::new(mouse.column, mouse.row));
        if let Some(press) = self.header_right_press.take() {
            let unchanged = press.frame == self.control_frame
                && press.dialog == self.control_dialog()
                && press.position == position;
            if unchanged && inside && mouse.kind == MouseEventKind::Up(MouseButton::Right) {
                return Some(self.request_quit_confirmation());
            }
            if unchanged && mouse.kind == MouseEventKind::Moved {
                self.header_right_press = Some(press);
            }
            // A drag, competing press, scroll, or replaced dialog cancels the
            // gesture without passing its endpoint into another control.
            return Some(Action::Continue);
        }
        if !inside || self.right_press.is_some() || self.right_dragging {
            return None;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                self.finish_mouse_for_input();
                self.parent_press = None;
                self.cancel_right_press();
                self.cancel_breadcrumb_press();
                self.queue_drag = None;
                self.dialog_mouse.press = None;
                self.dialog_mouse.hovered = None;
                self.control_press = None;
                self.control_hovered = None;
                self.hover_position = None;
                self.normal_click = None;
                if self.control_dialog() == self.painted_control_dialog {
                    self.header_right_press = Some(HeaderRightPress {
                        position,
                        frame: self.control_frame,
                        dialog: self.control_dialog(),
                    });
                }
                Some(Action::Continue)
            }
            MouseEventKind::Up(MouseButton::Right) | MouseEventKind::Drag(MouseButton::Right) => {
                self.cancel_right_press();
                Some(Action::Continue)
            }
            _ => None,
        }
    }

    fn control_mouse(&mut self, mouse: MouseEvent, deleting: bool) -> Option<Action> {
        let hit = self.control_at(mouse.column, mouse.row, deleting);
        self.control_hovered = hit.map(|(control, _)| control);
        if let Some(press) = self.control_press {
            let changed =
                self.control_dialog() != press.dialog || hit != Some((press.control, press.area));
            if changed
                || matches!(mouse.kind, MouseEventKind::Drag(_))
                || (mouse.kind == MouseEventKind::Moved
                    && press.position != (mouse.column, mouse.row))
            {
                self.control_press = None;
                self.control_hovered = None;
                // A control drag must not become a browser/queue gesture.
                return Some(Action::Continue);
            }
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                self.control_press = None;
                self.control_hovered = None;
                return Some(match press.control {
                    MouseControl::Help => {
                        self.modal = Some(Modal::help(self.visual.active()));
                        Action::Continue
                    }
                    MouseControl::Queue => {
                        self.modal = Some(Modal::Queue {
                            selected: None,
                            offset: None,
                        });
                        Action::Continue
                    }
                    MouseControl::VisualDelete => Action::BatchPrepare,
                    MouseControl::SelectionClear => {
                        self.clear_visual_intent();
                        self.visual.exit();
                        Action::Continue
                    }
                    MouseControl::Close => {
                        let key = if matches!(self.modal, Some(Modal::Stopping)) {
                            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
                        } else {
                            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
                        };
                        self.key(key, None, deleting)
                    }
                });
            }
            return Some(Action::Continue);
        }
        if let Some((control, area)) = hit {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.finish_mouse_for_input();
                self.parent_press = None;
                self.cancel_right_press();
                self.queue_drag = None;
                self.cancel_breadcrumb_press();
                self.hover_position = None;
                self.normal_click = None;
                self.control_press = Some(ControlPress {
                    control,
                    area,
                    position: (mouse.column, mouse.row),
                    dialog: self.control_dialog(),
                });
                return Some(Action::Continue);
            }
            // Releases and drags from another target keep their original
            // capture, including a visual range passing through the header.
            if mouse.kind == MouseEventKind::Moved
                && self.mouse_press_view.is_none()
                && !self.has_held_mouse()
                && self.queue_drag.is_none()
                && self.parent_press.is_none()
                && self.right_press.is_none()
            {
                self.cancel_breadcrumb_press();
                self.hover_position = None;
                return Some(Action::Continue);
            }
        }
        None
    }

    fn cancel_breadcrumb_press(&mut self) {
        self.breadcrumbs.pressed = None;
        self.breadcrumbs.hovered = None;
        self.breadcrumbs.hits.clear();
    }

    fn breadcrumb_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.modal.is_some()
            || self.breadcrumbs.directory != Some(self.directory)
            || self.mouse_press_view.is_some()
            || self.has_held_mouse()
        {
            self.breadcrumbs.pressed = None;
            self.breadcrumbs.hovered = None;
            return false;
        }
        let position = ratatui::layout::Position::new(mouse.column, mouse.row);
        let hit = self
            .breadcrumbs
            .hits
            .iter()
            .copied()
            .find(|(area, _)| area.contains(position));
        self.breadcrumbs.hovered = hit.map(|(_, id)| id);
        if let Some((directory, id, area, pressed_at)) = self.breadcrumbs.pressed {
            if directory != self.directory
                || hit != Some((area, id))
                || matches!(mouse.kind, MouseEventKind::Drag(_))
                || (mouse.kind == MouseEventKind::Moved && pressed_at != (mouse.column, mouse.row))
            {
                self.breadcrumbs.pressed = None;
                return true;
            }
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                self.breadcrumbs.pressed = None;
                self.navigate_ancestor(id);
            }
            return true;
        }
        if let Some((area, id)) = hit {
            self.hover_position = None;
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.finish_mouse_for_input();
                self.breadcrumbs.pressed =
                    Some((self.directory, id, area, (mouse.column, mouse.row)));
            }
            return true;
        }
        false
    }

    fn navigate_ancestor(&mut self, ancestor: NodeId) {
        if ancestor == self.directory || !self.model.is_live(ancestor) {
            return;
        }
        let mut child = self.directory;
        while child != ROOT {
            let Some(parent) = self.model.node(child).parent else {
                return;
            };
            if parent == ancestor {
                self.finish_mouse_for_input();
                self.selected = Some(child);
                self.directory = ancestor;
                self.clear_visual_intent();
                self.visual.reset(ancestor);
                self.prioritize_requested = true;
                self.reset_order(false);
                self.cancel_breadcrumb_press();
                return;
            }
            child = parent;
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if !self.settings.mouse {
            return;
        }
        if mouse.kind != MouseEventKind::Moved {
            self.recent_mouse_entry = None;
        }
        // The pressed parent row owns motion and release even over a
        // different header target. Leaving and returning cannot become a click.
        if self.parent_press.is_some() && self.parent_mouse(mouse) {
            return;
        }
        if self.right_browser_mouse(mouse) {
            return;
        }
        if self.breadcrumb_mouse(mouse) {
            return;
        }
        if self.parent_mouse(mouse) {
            return;
        }
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
                // Release-only motion still becomes a range from the pressed ID.
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
        self.scroll_modal_mouse(mouse);
        if self.modal.is_some() {
            self.breadcrumbs.hovered = None;
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
                let shift = mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && !mouse.modifiers.contains(KeyModifiers::CONTROL);
                if shift || self.visual.active() {
                    if !self.visual.active() {
                        self.visual.enter(self.directory, None);
                        // Shift-click keeps the current range anchor, but mode
                        // entry must not invent a mark that makes dragging erase.
                        if let Some(id) = self.selected {
                            self.visual.reanchor(id);
                        }
                        self.range_requested = false;
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
                            origin_selected: self.visual.contains(id),
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
                    let mode = if mode == RangeMode::Add && self.visual.contains(id) {
                        RangeMode::Remove
                    } else {
                        mode
                    };
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
                    self.drag_mode = if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                        RangeMode::Remove
                    } else {
                        RangeMode::Add
                    };
                }
                self.cursor_mode = CursorMode::Tracked;
                self.parent_selected = false;
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if dragging => {
                if !self.visual.active() {
                    self.visual.enter(self.directory, None);
                    self.mouse_baseline = Some(self.visual.snapshot());
                }
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
                    if self.drag_mode == RangeMode::Invert {
                        self.drag_mode = if baseline.contains(&anchor) {
                            RangeMode::Remove
                        } else {
                            RangeMode::Add
                        };
                    }
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
                self.parent_selected = false;
                self.selected = Some(id);
                self.pending_navigation = None;
                self.range_requested = false;
                self.prioritize_requested = true;
                self.keep_selection_visible();
                self.set_drag_edge(edge, id);
            }
            MouseEventKind::Moved if !captured => {
                let previous = (!self.parent_selected).then_some(self.selected).flatten();
                if !self.visual.active() && previous != Some(id) {
                    self.record_mouse_entry_change(previous, id, Instant::now());
                }
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
                self.parent_selected = false;
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
            "deleting…"
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
        let highlighted = selected || visually_selected;
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
                    Style::default().fg(if highlighted {
                        Color::White
                    } else {
                        self.theme.accent
                    }),
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
        let color = if highlighted {
            Color::White
        } else if node.error_count > 0 {
            Color::Yellow
        } else if node.kind == NodeKind::Directory {
            self.theme.accent
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
            Style::default().fg(if deleting {
                if highlighted {
                    Color::White
                } else {
                    Color::Red
                }
            } else {
                Color::Gray
            }),
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
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            line
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, refreshing: bool, deleting: bool) {
        let area = frame.area();
        if self.control_frame != area {
            self.header_right_press = None;
            self.dialog_mouse.press = None;
            self.dialog_mouse.hovered = None;
            self.control_press = None;
            self.control_hovered = None;
        }
        self.control_frame = area;
        let header = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let visual = self.visual.active();
        let selection_label = visual.then(|| format!("{} selected", self.visual.len()));
        let available = usize::from(area.width);
        let count_width = selection_label.as_ref().map_or(0, String::len);
        let (delete_label, hint) = if visual {
            if count_width + 2 + 10 + 2 + 6 <= available {
                ("[d] delete", "? help")
            } else if count_width + 2 + 6 + 2 + 6 <= available {
                ("delete", "? help")
            } else if count_width + 2 + 10 <= available {
                ("[d] delete", "")
            } else if count_width + 2 + 6 <= available {
                ("delete", "")
            } else {
                ("", "")
            }
        } else {
            ("", "? help")
        };
        let columns = split_edges(
            header,
            Direction::Horizontal,
            0,
            hint.len() as u16 + 2 * u16::from(!hint.is_empty()),
        );
        let busy =
            deleting || self.queue_view.pending > 0 || self.queue_view.marks.active.is_some();
        let queue_status = if !busy {
            String::new()
        } else if self.queue_view.pending > 0 {
            format!("{} queued", self.queue_view.pending)
        } else {
            "deleting…".to_owned()
        };
        let mut phase_text = if let Some(label) = &selection_label {
            if self.modal.is_none() && self.control_hovered == Some(MouseControl::SelectionClear) {
                // Keep the component's painted width and hitbox stable on hover.
                format!("{:<width$}", "X selected", width = label.len())
            } else {
                label.clone()
            }
        } else if self.filter_editing {
            format!(
                "Filter /{}",
                clipped_tail(
                    &self.filter,
                    usize::from(columns[1].width).saturating_sub(8)
                )
            )
        } else if busy {
            queue_status.clone()
        } else {
            self.phase(refreshing, false).to_owned()
        };
        if visual && !delete_label.is_empty() {
            let _ = write!(phase_text, "  {delete_label}");
        }
        if (!self.filter_editing || visual) && !self.filter.is_empty() {
            let _ = write!(phase_text, " · /{}", self.filter);
        }
        let width = usize::from(columns[1].width);
        let phase_budget = if visual || self.filter_editing {
            width
        } else {
            width.saturating_sub(width.min(12) + 2)
        };
        let queue_label = if busy && !visual && phase_budget >= 4 {
            format!("{} [D]", clipped(&queue_status, phase_budget - 4))
        } else {
            String::new()
        };
        let phase_text = if !queue_label.is_empty() {
            let queue_width = Line::from(queue_label.as_str()).width();
            let filter_width = phase_budget.saturating_sub(queue_width + 3);
            if filter_width > 0 && (self.filter_editing || !self.filter.is_empty()) {
                let filter_text = if self.filter_editing {
                    phase_text.clone()
                } else {
                    format!("/{}", self.filter)
                };
                format!("{} · {queue_label}", clipped(&filter_text, filter_width))
            } else {
                queue_label.clone()
            }
        } else {
            clipped(&phase_text, phase_budget)
        };
        let phase = phase_text.as_str();
        let path_budget =
            usize::from(columns[1].width).saturating_sub(Line::from(phase).width() + 3);
        self.breadcrumbs
            .sync(&self.root, &self.model, self.directory);
        let mut label = self.breadcrumbs.line(
            path_budget,
            columns[1],
            self.modal.is_some(),
            self.theme.hover,
            self.theme.header_foreground,
        );
        if label.width() > 0 && !phase.is_empty() {
            label.spans.push(Span::raw("  "));
        }
        let phase_offset = label.width();
        label.spans.push(Span::raw(phase.to_owned()));
        let label_width = label.width();
        let header_style = Style::default()
            .bg(Color::DarkGray)
            .fg(self.theme.header_foreground)
            .remove_modifier(Modifier::DIM);
        if self.filter_editing && self.modal.is_none() && columns[1].width > 0 {
            let x = label_width.min(usize::from(columns[1].width - 1));
            frame.set_cursor_position((columns[1].x + x as u16, columns[1].y));
        }
        frame.render_widget(Paragraph::new(label).style(header_style), columns[1]);
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(header_style),
            columns[2],
        );
        let previous = [
            self.help_area,
            self.header_queue_area,
            self.header_delete_area,
            self.selection_clear_area,
        ];
        self.help_area = if hint.ends_with("? help") && columns[2].width >= 6 {
            Rect::new(columns[2].right() - 6, columns[2].y, 6, columns[2].height)
        } else {
            Rect::default()
        };
        let queue_width = Line::from(queue_label.as_str()).width();
        self.header_queue_area = if !queue_label.is_empty()
            && phase.ends_with(&queue_label)
            && label_width <= usize::from(columns[1].width)
        {
            Rect::new(
                columns[1].x + (label_width - queue_width) as u16,
                columns[1].y,
                queue_width as u16,
                columns[1].height,
            )
        } else {
            Rect::default()
        };
        self.header_delete_area = if !delete_label.is_empty()
            && count_width + 2 + delete_label.len() <= phase_budget
            && label_width <= width
        {
            Rect::new(
                columns[1].x + (phase_offset + count_width + 2) as u16,
                columns[1].y,
                delete_label.len() as u16,
                columns[1].height,
            )
        } else {
            Rect::default()
        };
        self.selection_clear_area = if let Some(label) = selection_label
            .as_ref()
            .filter(|label| label.len() <= phase_budget && label_width <= width)
        {
            Rect::new(
                columns[1].x + phase_offset as u16,
                columns[1].y,
                label.len() as u16,
                columns[1].height,
            )
        } else {
            Rect::default()
        };
        if previous
            != [
                self.help_area,
                self.header_queue_area,
                self.header_delete_area,
                self.selection_clear_area,
            ]
        {
            self.control_hovered = None;
        }
        for (control, label, hitbox) in [
            (MouseControl::Help, "? help", self.help_area),
            (
                MouseControl::Queue,
                queue_label.as_str(),
                self.header_queue_area,
            ),
            (
                MouseControl::VisualDelete,
                delete_label,
                self.header_delete_area,
            ),
        ] {
            frame.render_widget(
                Paragraph::new(label).style(mouse_control_style(
                    header_style,
                    self.modal.is_none() && self.control_hovered == Some(control),
                    self.theme.hover,
                )),
                hitbox,
            );
        }
        if self.control_hovered == Some(MouseControl::SelectionClear) && self.modal.is_none() {
            frame.render_widget(
                Paragraph::new("X selected").style(mouse_control_style(
                    header_style,
                    true,
                    self.theme.hover,
                )),
                self.selection_clear_area,
            );
        }
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
            if !self.file_preview.matches(id, self.model.node(id).kind)
                && let Some(anchor) = self
                    .file_anchor
                    .as_ref()
                    .and_then(|anchor| anchor.get())
                    .cloned()
                && let Ok(relative) = self.model.path(id).strip_prefix(&self.root)
            {
                self.file_preview.request(
                    id,
                    anchor,
                    relative.to_owned(),
                    self.model.node(id).kind,
                );
            }
        } else {
            self.file_preview.clear();
        }
        let parent_area = Rect::new(body.x, body.y, body.width, body.height.min(1));
        if self.parent_area != parent_area {
            self.parent_press = None;
        }
        self.parent_area = parent_area;
        let parent_style = if self.parent_selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.theme.accent)
        };
        frame.render_widget(
            Paragraph::new(if self.parent_selected {
                "› ../"
            } else {
                "  ../"
            })
            .style(parent_style),
            parent_area,
        );
        let body = Rect::new(
            body.x,
            body.y.saturating_add(parent_area.height),
            body.width,
            body.height.saturating_sub(parent_area.height),
        );
        self.list_area = body;
        if self.right_press.as_ref().is_some_and(|press| {
            self.modal.is_some()
                || press.directory != self.directory
                || press.epoch != self.model.node(self.directory).child_epoch
                || press.area
                    != match press.target {
                        BrowserTarget::Entry(_) => self.list_area,
                        BrowserTarget::Parent => self.parent_area,
                    }
        }) {
            self.cancel_right_press();
        }
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
        let held_view = self
            .right_press
            .as_ref()
            .map(|press| (Arc::clone(&press.view), press.offset))
            .or_else(|| {
                self.visual_intents.iter().find_map(|intent| match intent {
                    VisualIntent::Mouse(gesture) if !gesture.released => {
                        Some((Arc::clone(&gesture.view), gesture.offset))
                    }
                    _ => None,
                })
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
                        .fg(self.theme.accent)
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
        let old_close_area = self.dialog_close_area;
        self.dialog_close_area = Rect::default();
        self.dialog_mouse.accent = self.theme.dialog_accent;
        self.dialog_mouse.hover = self.theme.hover;
        self.dialog_mouse.guard_warning = self.settings.mouse_debounce_warning;
        if let Some(modal) = self.modal.as_mut() {
            draw_modal(
                frame,
                area,
                modal,
                &mut self.queue_view,
                self.delete_notices.front(),
                &mut self.dialog_close_area,
                &mut self.dialog_mouse,
            );
            if self.dialog_close_area != old_close_area {
                self.control_hovered = None;
            }
            let marker = if self.dialog_close_area.width == 3 {
                "[x]"
            } else {
                "x"
            };
            frame.render_widget(
                Paragraph::new(marker).style(mouse_control_style(
                    Style::default().bg(Color::Black).fg(Color::Yellow),
                    self.control_hovered == Some(MouseControl::Close),
                    self.theme.hover,
                )),
                self.dialog_close_area,
            );
        }
        if self.settings.mouse_debounce_warning
            && matches!(
                self.modal,
                Some(Modal::Delete {
                    mouse_guarded: true,
                    ..
                })
            )
        {
            // Tint after all widgets so cleared dialogs and blank cells are covered.
            // Preserve the hover warning until this confirmation closes.
            let style = Style::default()
                .bg(self.theme.warning_background)
                .fg(self.theme.warning_foreground);
            frame.buffer_mut().set_style(area, style);
            let popup = self.dialog_mouse.popup;
            if !popup.is_empty() {
                frame
                    .buffer_mut()
                    .set_style(popup, style.bg(self.theme.warning_dialog_background));
            }
        }
        self.painted_control_dialog = self.control_dialog();
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

fn split_edges(area: Rect, direction: Direction, leading: u16, trailing: u16) -> [Rect; 3] {
    let available = match direction {
        Direction::Horizontal => area.width,
        Direction::Vertical => area.height,
    };
    let trailing = trailing.min(available);
    let leading = leading.min(available - trailing);
    let middle = available - leading - trailing;
    match direction {
        Direction::Horizontal => [
            Rect::new(area.x, area.y, leading, area.height),
            Rect::new(area.x + leading, area.y, middle, area.height),
            Rect::new(area.x + leading + middle, area.y, trailing, area.height),
        ],
        Direction::Vertical => [
            Rect::new(area.x, area.y, area.width, leading),
            Rect::new(area.x, area.y + leading, area.width, middle),
            Rect::new(area.x, area.y + leading + middle, area.width, trailing),
        ],
    }
}

/// Share the current frame's queue capacity with row synchronization.
fn queue_layout(area: Rect) -> (Rect, Rect, [Rect; 3]) {
    let popup = centered(area, area.width.min(76), area.height.min(20));
    let inner = Block::bordered().inner(popup);
    // Two footer rows hold either both buttons plus the mouse hint, or the
    // stacked buttons alone when the terminal is narrow.
    let regions = split_edges(inner, Direction::Vertical, 1, 2);
    (popup, inner, regions)
}

fn mouse_control_style(base: Style, hovered: bool, hover: Style) -> Style {
    if hovered { base.patch(hover) } else { base }
}

fn dialog_close_area(popup: Rect) -> Rect {
    if popup.is_empty() {
        return Rect::default();
    }
    let width = if popup.width >= 5 { 3 } else { 1 };
    let border = u16::from(popup.width >= 3);
    Rect::new(popup.right() - border - width, popup.y, width, 1)
}

#[derive(Clone, Copy)]
struct DialogButton {
    key: Option<KeyEvent>,
    label: &'static str,
    hint: &'static str,
}

impl DialogButton {
    fn new(code: KeyCode, label: &'static str, hint: &'static str) -> Self {
        Self {
            key: Some(KeyEvent::new(code, KeyModifiers::NONE)),
            label,
            hint,
        }
    }

    fn width(self) -> usize {
        Line::from(self.label).width()
            + 4
            + if self.hint.is_empty() {
                0
            } else {
                Line::from(self.hint).width() + 3
            }
    }
}

fn dialog_button_rows(width: u16, buttons: &[DialogButton]) -> u16 {
    let needed = buttons.iter().map(|button| button.width()).sum::<usize>()
        + buttons.len().saturating_sub(1) * 2;
    if needed <= usize::from(width) {
        1
    } else {
        buttons.len() as u16
    }
}

fn draw_dialog_buttons(
    frame: &mut Frame<'_>,
    mouse: &mut DialogMouse,
    area: Rect,
    buttons: &[DialogButton],
    style: Style,
) {
    let rows = dialog_button_rows(area.width, buttons);
    let mut x = area.x;
    for (index, button) in buttons.iter().enumerate() {
        let y = area.y + if rows == 1 { 0 } else { index as u16 };
        let rect = Rect::new(
            x,
            y,
            area.right().saturating_sub(x),
            u16::from(y < area.bottom()),
        );
        let mut line = Line::from(vec![
            Span::raw("[ "),
            Span::styled(
                button.label,
                Style::default()
                    .fg(mouse.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        if !button.hint.is_empty() {
            line.spans.push(Span::styled(
                format!(" · {}", button.hint),
                Style::default().fg(Color::Gray),
            ));
        }
        line.spans.push(Span::raw(" ]"));
        if let Some(key) = button.key {
            mouse.option(rect, 0, key, &line);
            if mouse.hovered == Some(key) {
                line = line.style(mouse_control_style(style, true, mouse.hover));
                for span in &mut line.spans {
                    span.style = Style::default();
                }
            }
        }
        frame.render_widget(Paragraph::new(line).style(style), rect);
        if rows == 1 {
            x = x.saturating_add(button.width() as u16 + 2);
        }
    }
}

fn draw_dialog_scroll_controls(
    frame: &mut Frame<'_>,
    mouse: &mut DialogMouse,
    popup: Rect,
    vertical: bool,
    horizontal: bool,
) {
    if popup.width < 8 || popup.height < 2 || (!vertical && !horizontal) {
        return;
    }
    let rail = Rect::new(popup.x + 1, popup.bottom() - 1, popup.width - 2, 1);
    let style = Style::default().bg(Color::Black).fg(Color::Gray);
    let mut x = rail.x;
    for (enabled, code, text) in [
        (vertical, KeyCode::Up, " ↑ "),
        (vertical, KeyCode::Down, " ↓ "),
        (horizontal, KeyCode::Left, " ← "),
        (horizontal, KeyCode::Right, " → "),
    ] {
        if !enabled || x.saturating_add(3) > rail.right() {
            continue;
        }
        let rect = Rect::new(x, rail.y, 3, 1);
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        let line = Line::from(text);
        mouse.option(rect, 0, key, &line);
        frame.render_widget(
            Paragraph::new(line).style(mouse_control_style(
                style,
                mouse.hovered == Some(key),
                mouse.hover,
            )),
            rect,
        );
        x += 3;
    }
    let hint = " wheel · PgUp/PgDn ";
    let hint_width = Line::from(hint).width() as u16;
    if vertical && rail.right().saturating_sub(x) > hint_width {
        frame.render_widget(
            Paragraph::new(hint).style(style),
            Rect::new(rail.right() - hint_width, rail.y, hint_width, 1),
        );
    }
}

fn draw_dialog_scrollbar(
    frame: &mut Frame<'_>,
    popup: Rect,
    content: Rect,
    position: usize,
    total: usize,
    accent: Color,
) {
    let visible = usize::from(content.height);
    if popup.width < 2 || visible == 0 || total <= visible {
        return;
    }
    // Only the visible rail is drawn; even a million-error report remains O(height).
    let thumb = ((visible as u128 * visible as u128) / total as u128).max(1) as usize;
    let max = total - visible;
    let start = ((position.min(max) as u128 * (visible - thumb) as u128) / max as u128) as usize;
    for row in 0..visible {
        frame.render_widget(
            Paragraph::new(if (start..start + thumb).contains(&row) {
                "█"
            } else {
                "│"
            })
            .style(Style::default().bg(Color::Black).fg(accent)),
            Rect::new(popup.right() - 1, content.y + row as u16, 1, 1),
        );
    }
}

fn draw_review_size_notice(
    frame: &mut Frame<'_>,
    popup: Rect,
    inner: Rect,
    close: Rect,
    style: Style,
) {
    let area = if !inner.is_empty() {
        Rect::new(inner.x, inner.y, inner.width, 1)
    } else if !popup.is_empty() {
        // Two-row terminals have no dialog interior. Use the lower border for
        // the refusal; on a single row reserve the close control's exact cells.
        let y = popup.bottom() - 1;
        let x = popup.x.saturating_add(u16::from(popup.width > 1));
        let right = if close.y == y {
            close.x
        } else {
            popup.right().saturating_sub(1)
        };
        Rect::new(x, y, right.saturating_sub(x), 1)
    } else {
        Rect::default()
    };
    let text = if area.width >= 16 {
        "Resize to review"
    } else if area.width >= 6 {
        "Resize"
    } else {
        "…"
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    modal: &mut Modal,
    queue: &mut QueueView,
    report: Option<&DeleteReport>,
    close_area: &mut Rect,
    mouse: &mut DialogMouse,
) {
    mouse.options.fill(None);
    mouse.popup = Rect::default();
    mouse.scroll_area = Rect::default();
    mouse.scroll_max = 0;
    let width = area.width.min(76);
    let inner_width = width.saturating_sub(2);
    let style = Style::default().bg(Color::Black).fg(Color::White);
    let accent = mouse.accent;
    let border = |title: &'static str| {
        Block::bordered()
            .border_type(BorderType::Plain)
            .title(
                Line::from(format!(" {title} "))
                    .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
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
            let height = u16::try_from(report.lines())
                .unwrap_or(u16::MAX)
                .saturating_add(5);
            let popup = centered(area, width, area.height.min(24).min(height));
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Delete failed");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let regions = split_edges(inner, Direction::Vertical, 3, 0);
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
                    Line::from("Could not delete everything in:"),
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
                    accent,
                ))
                .style(style),
                regions[1],
            );
            mouse.scroll(popup, report.lines(), regions[1].height);
            draw_dialog_scrollbar(frame, popup, regions[1], *scroll, report.lines(), accent);
            let horizontal_overflow = *horizontal > 0
                || Line::from(report.target.as_str()).width() > usize::from(inner.width)
                || report
                    .errors
                    .iter()
                    .skip(*scroll / 2)
                    .take(page.div_ceil(2) + 1)
                    .any(|error| {
                        Line::from(printable(error.path.as_os_str())).width()
                            > usize::from(inner.width)
                            || Line::from(printable(OsStr::new(&error.message))).width()
                                > usize::from(inner.width)
                    });
            draw_dialog_scroll_controls(
                frame,
                mouse,
                popup,
                report.lines() > usize::from(regions[1].height),
                horizontal_overflow,
            );
        }
        Modal::Batch {
            target,
            count,
            ready,
            reviewable,
            confirm_requested,
            scroll,
        } => {
            let buttons = [
                DialogButton::new(KeyCode::Enter, "Yes", "Enter"),
                DialogButton::new(KeyCode::Esc, "Cancel", "Esc"),
            ];
            let button_rows = dialog_button_rows(inner_width, &buttons);
            let paragraph = Paragraph::new(target.as_str()).wrap(Wrap { trim: false });
            let target_lines = paragraph.line_count(inner_width);
            let height = area.height.min(14).min(
                u16::try_from(target_lines)
                    .unwrap_or(u16::MAX)
                    .saturating_add(3 + button_rows),
            );
            let popup = centered(area, width, height);
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Delete selection?");
            let inner = block.inner(popup);
            let regions = split_edges(inner, Direction::Vertical, 1, button_rows);
            let geometry =
                inner.width >= 32 && regions[1].height > 0 && regions[2].height == button_rows;
            *reviewable = geometry;
            *confirm_requested &= geometry;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            if !geometry {
                draw_review_size_notice(frame, popup, inner, *close_area, style);
                let cancel_area = Rect::new(
                    inner.x,
                    inner.y.saturating_add(1),
                    inner.width,
                    inner.height.saturating_sub(1),
                );
                draw_dialog_buttons(
                    frame,
                    mouse,
                    cancel_area,
                    &[DialogButton::new(KeyCode::Esc, "Cancel", "Esc")],
                    style,
                );
                return;
            }
            frame.render_widget(
                Paragraph::new(if *ready {
                    format!("Delete {count} entries in:")
                } else {
                    "Delete selected entries in:".to_owned()
                })
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
            mouse.scroll(popup, target_lines, regions[1].height);
            draw_dialog_scrollbar(
                frame,
                popup,
                regions[1],
                usize::from(*scroll),
                target_lines,
                accent,
            );
            draw_dialog_buttons(frame, mouse, regions[2], &buttons, style);
            draw_dialog_scroll_controls(
                frame,
                mouse,
                popup,
                target_lines > usize::from(regions[1].height),
                false,
            );
        }
        Modal::Queue { .. } => {
            let (popup, inner, regions) = queue_layout(area);
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
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
                Paragraph::new(Line::from(summary).style(Style::default().fg(accent))).style(style),
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
                            Style::default().fg(if queue.selected == Some(*id) {
                                Color::White
                            } else if *active {
                                accent
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
            draw_dialog_scrollbar(
                frame,
                popup,
                regions[1],
                queue.scroll_position,
                queue
                    .pending
                    .saturating_add(usize::from(queue.marks.active.is_some())),
                accent,
            );
            let buttons = [
                DialogButton::new(
                    KeyCode::Char(' '),
                    if queue.paused { "Resume" } else { "Pause" },
                    "Space",
                ),
                DialogButton::new(KeyCode::Char('d'), "Remove", "d"),
            ];
            let button_rows = dialog_button_rows(regions[2].width, &buttons);
            draw_dialog_buttons(
                frame,
                mouse,
                Rect::new(
                    regions[2].x,
                    regions[2].y,
                    regions[2].width,
                    button_rows.min(regions[2].height),
                ),
                &buttons,
                style,
            );
            if regions[2].height > button_rows {
                frame.render_widget(
                    Paragraph::new("Click: pause/resume · right-click: remove · drag: reorder")
                        .style(style.fg(Color::Gray)),
                    Rect::new(
                        regions[2].x,
                        regions[2].y + button_rows,
                        regions[2].width,
                        1,
                    ),
                );
            }
            let total = queue
                .pending
                .saturating_add(usize::from(queue.marks.active.is_some()));
            draw_dialog_scroll_controls(
                frame,
                mouse,
                popup,
                total > usize::from(regions[1].height),
                false,
            );
        }
        Modal::ConfirmQuit { reviewable, .. } => {
            let buttons = [
                DialogButton::new(KeyCode::Enter, "Yes", "Enter"),
                DialogButton::new(KeyCode::Esc, "No", "Esc"),
            ];
            let popup_width = width.min(38);
            let rows = dialog_button_rows(popup_width.saturating_sub(2), &buttons);
            let popup = centered(area, popup_width, area.height.min(rows + 2));
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Quit LDU?");
            let inner = block.inner(popup);
            *reviewable = inner.width >= 15 && inner.height >= rows;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            if *reviewable {
                draw_dialog_buttons(frame, mouse, inner, &buttons, style);
            } else {
                draw_review_size_notice(frame, popup, inner, *close_area, style);
            }
        }
        Modal::Quit { reviewable } => {
            let buttons = [
                DialogButton::new(KeyCode::Enter, "Exit now", "Enter"),
                DialogButton::new(KeyCode::Esc, "Browse", "Esc"),
            ];
            let button_rows = dialog_button_rows(inner_width, &buttons);
            let summary = Paragraph::new(format!(
                "{} queued{}",
                queue.pending,
                if queue.paused { " · paused" } else { "" }
            ))
            .wrap(Wrap { trim: false });
            let summary_rows =
                u16::try_from(summary.line_count(inner_width.max(1))).unwrap_or(u16::MAX);
            let target = queue
                .active_target
                .as_ref()
                .map_or("Waiting for queued deletions", |(_, target)| {
                    target.as_str()
                });
            let path = Paragraph::new(target).wrap(Wrap { trim: false });
            let path_lines = path.line_count(inner_width.max(1));
            let counts =
                Paragraph::new(queue.active_target.as_ref().map_or_else(String::new, |_| {
                    format!(
                        "{} / {} known items removed",
                        queue.active_progress.removed_entries,
                        queue
                            .active_total
                            .max(queue.active_progress.examined_entries)
                    )
                }))
                .wrap(Wrap { trim: false });
            let count_rows = if queue.active_target.is_some() {
                u16::try_from(counts.line_count(inner_width.max(1))).unwrap_or(u16::MAX)
            } else {
                0
            };
            let fixed = summary_rows
                .saturating_add(count_rows)
                .saturating_add(button_rows);
            let height = u16::try_from(path_lines)
                .unwrap_or(u16::MAX)
                .saturating_add(fixed)
                .saturating_add(2);
            let popup = centered(area, width, area.height.min(18).min(height));
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Finishing deletions");
            let inner = block.inner(popup);
            *reviewable = inner.width >= 32 && inner.height > fixed;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            if *reviewable {
                let regions = split_edges(
                    inner,
                    Direction::Vertical,
                    summary_rows,
                    count_rows.saturating_add(button_rows),
                );
                let footer = split_edges(regions[2], Direction::Vertical, count_rows, 0);
                frame.render_widget(summary.style(style.fg(accent)), regions[0]);
                clamp_scroll(&mut queue.progress_scroll, path_lines, regions[1].height);
                frame.render_widget(
                    path.scroll((queue.progress_scroll, 0)).style(style),
                    regions[1],
                );
                mouse.scroll(popup, path_lines, regions[1].height);
                draw_dialog_scrollbar(
                    frame,
                    popup,
                    regions[1],
                    usize::from(queue.progress_scroll),
                    path_lines,
                    accent,
                );
                frame.render_widget(counts.style(style.fg(accent)), footer[0]);
                draw_dialog_buttons(frame, mouse, footer[1], &buttons, style);
                draw_dialog_scroll_controls(
                    frame,
                    mouse,
                    popup,
                    path_lines > usize::from(regions[1].height),
                    false,
                );
            } else {
                draw_review_size_notice(frame, popup, inner, *close_area, style);
            }
        }
        Modal::Enqueueing => {
            let buttons = [
                DialogButton::new(KeyCode::Esc, "Stop adding", "Esc"),
                DialogButton::new(KeyCode::Char('q'), "Quit", "q"),
            ];
            let rows = dialog_button_rows(inner_width, &buttons);
            let popup = centered(area, width, area.height.min(rows + 2));
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Adding selection to queue");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            draw_dialog_buttons(frame, mouse, inner, &buttons, style);
        }
        Modal::Stopping => {
            let popup = centered(area, width, area.height.min(3));
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Stopping deletions…");
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            let buttons = [DialogButton {
                key: Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                label: "Exit now",
                hint: "Ctrl-C",
            }];
            draw_dialog_buttons(frame, mouse, inner, &buttons, style);
        }
        Modal::Delete {
            target,
            scroll,
            reviewable,
            confirm_requested,
            guard_until,
            mouse_guarded,
            ..
        } => {
            let guarded = guard_until.is_some_and(|deadline| Instant::now() < deadline);
            let buttons = [
                if guarded {
                    DialogButton {
                        key: None,
                        label: "Please wait…",
                        hint: "",
                    }
                } else {
                    DialogButton::new(KeyCode::Enter, "Yes", "Enter")
                },
                DialogButton::new(KeyCode::Esc, "Cancel", "Esc"),
            ];
            let button_rows = dialog_button_rows(inner_width, &buttons);
            let target_paragraph = Paragraph::new(target.as_str()).wrap(Wrap { trim: false });
            let target_lines = target_paragraph.line_count(inner_width);
            let prompt = Paragraph::new(if *mouse_guarded && mouse.guard_warning {
                "Mouse hover changed selection just before delete.\nDisable red tint (delay stays):\nCLI: --mouse-debounce-warning=false\nConfig: mouse_debounce_warning=false\nPermanently remove:"
            } else {
                "Permanently remove:"
            })
            .wrap(Wrap { trim: false });
            let prompt_height = u16::try_from(prompt.line_count(inner_width)).unwrap_or(u16::MAX);
            let height_limit = if *mouse_guarded && mouse.guard_warning {
                24
            } else {
                14
            };
            let height = area.height.min(height_limit).min(
                u16::try_from(target_lines)
                    .unwrap_or(u16::MAX)
                    .saturating_add(prompt_height)
                    .saturating_add(2 + button_rows),
            );
            let popup = centered(area, width, height);
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border("Delete?");
            let inner = block.inner(popup);
            let regions = split_edges(inner, Direction::Vertical, prompt_height, button_rows);
            let geometry =
                inner.width >= 18 && regions[1].height > 0 && regions[2].height == button_rows;
            *reviewable = geometry;
            *confirm_requested &= geometry;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            if !geometry {
                draw_review_size_notice(frame, popup, inner, *close_area, style);
                let cancel_area = Rect::new(
                    inner.x,
                    inner.y.saturating_add(1),
                    inner.width,
                    inner.height.saturating_sub(1),
                );
                draw_dialog_buttons(
                    frame,
                    mouse,
                    cancel_area,
                    &[DialogButton::new(KeyCode::Esc, "Cancel", "Esc")],
                    style,
                );
                return;
            }
            frame.render_widget(prompt.style(style), regions[0]);
            clamp_scroll(scroll, target_lines, regions[1].height);
            frame.render_widget(
                target_paragraph
                    .scroll((*scroll, 0))
                    .style(style.add_modifier(Modifier::BOLD)),
                regions[1],
            );
            mouse.scroll(popup, target_lines, regions[1].height);
            draw_dialog_scrollbar(
                frame,
                popup,
                regions[1],
                usize::from(*scroll),
                target_lines,
                accent,
            );
            draw_dialog_buttons(frame, mouse, regions[2], &buttons, style);
            draw_dialog_scroll_controls(
                frame,
                mouse,
                popup,
                target_lines > usize::from(regions[1].height),
                false,
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
                            return Line::from(line.to_owned())
                                .style(Style::default().fg(accent).add_modifier(Modifier::BOLD));
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
                    .saturating_add(2),
            );
            let popup = centered(area, width, height);
            mouse.popup = popup;
            *close_area = dialog_close_area(popup);
            let block = border(title);
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            clamp_scroll(scroll, line_count, inner.height);
            frame.render_widget(paragraph.scroll((*scroll, 0)).style(style), inner);
            mouse.scroll(popup, line_count, inner.height);
            draw_dialog_scrollbar(
                frame,
                popup,
                inner,
                usize::from(*scroll),
                line_count,
                accent,
            );
            draw_dialog_scroll_controls(
                frame,
                mouse,
                popup,
                line_count > usize::from(inner.height),
                false,
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
Click / drag       Toggle / add range (marked start removes)
Hold list edge    Scroll while dragging
Shift-click/drag   Extend / add or remove
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
Archive listings use available system utilities.
Hardlinks are counted once.";

struct DeleteRequest {
    id: NodeId,
    anchor: Arc<RootAnchor>,
    relative: PathBuf,
    target: String,
    guard_until: Option<Instant>,
    mouse_guarded: bool,
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
        app.modal = Some(Modal::Batch {
            target: printable(app.model.path(app.directory).as_os_str()),
            count: 0,
            ready: false,
            reviewable: false,
            confirm_requested: false,
            scroll: 0,
        });
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
        let (confirm_requested, reviewable) = app.batch_confirmation_state();
        app.cancel_deferred_batch();
        app.deferred_batch = Some(request);
        app.modal = Some(Modal::Batch {
            target: printable(path.as_os_str()),
            count: 0,
            ready: false,
            reviewable,
            confirm_requested,
            scroll: 0,
        });
        return;
    }
    start_batch_request(app, request, batch);
}

fn start_batch_request(app: &mut App, request: BatchRequest, batch: &mut Option<BatchPreparation>) {
    let (confirm_requested, reviewable) = app.batch_confirmation_state();
    if !app.model.is_live(request.directory)
        || app.model.node(request.directory).child_epoch != request.epoch
        || !app.visual.active()
        || app.visual.directory() != request.directory
    {
        app.visual
            .retire_snapshot_and_order(request.selection, request.order);
        app.modal = Some(Modal::notice(
            "Selection changed",
            "Select entries again.".to_owned(),
        ));
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
                reviewable,
                confirm_requested,
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
        || (matches!(app.modal, Some(Modal::Batch { .. }))
            && (app.waiting_batch.is_some() || app.deferred_batch.is_some())))
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
        prepare_batch(app, Some(waiting.anchor), batch, retired);
        return;
    }
    if batch.is_none()
        && retired.is_empty()
        && let Some(request) = app.deferred_batch.take()
    {
        start_batch_request(app, request, batch);
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
    // A stopped listing must not replace quit progress with a selection notice.
    // Discard queued selection requests too, while keeping materialized marks.
    app.clear_visual_intent();
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
        {
            *modal = None;
        }
    }
}

fn enqueue_delete(
    app: &mut App,
    scanner: &ScanHandle,
    scopes: &[ScopedScan],
    queue: &mut DeleteQueue,
    (id, anchor, relative, target): (NodeId, Arc<RootAnchor>, PathBuf, ConfirmedTarget),
) {
    if app.model.is_live(id) && app.model.path(id) == app.root.join(&relative) {
        let outcome = queue.enqueue_confirmed(id, anchor, relative, target);
        if outcome.inserted {
            let entry = queue.entry(outcome.id).expect("inserted deletion exists");
            exclude_job(app, scanner, scopes, id, &entry.relative);
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
    app.background_modal_slot().as_mut()
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
                        relative,
                        target,
                    );
                    if outcome.inserted {
                        let entry = queue.entry(outcome.id).expect("inserted deletion exists");
                        exclude_job(app, scanner, scopes, node, &entry.relative);
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
        for slot in [&mut app.modal, &mut app.previous_modal] {
            let modal = underlying_modal_slot(slot);
            if matches!(modal, Some(Modal::Batch { .. } | Modal::Enqueueing)) {
                *modal = None;
            }
        }
        if let Some(error) = error {
            let message = printable(OsStr::new(&error));
            if confirmed
                && (app.finishing_deletions()
                    || app
                        .modal
                        .as_ref()
                        .is_some_and(|modal| matches!(modal.underlying(), Modal::Stopping)))
            {
                app.delete_notices.push_back(DeleteReport::single(
                    app.report_path(preparation_directory),
                    message,
                ));
            } else {
                *app.background_modal_slot() = Some(Modal::notice("Cannot delete", message));
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
    if !ready.is_empty()
        && let Err(ready) = retire(ready)
    {
        // Returned ownership still occupies the same eight admission slots.
        scopes.extend(ready);
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
    if !retired.is_empty()
        && let Err(returned) = retire(std::mem::take(retired))
    {
        *retired = returned;
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

fn stop_work(app: &mut App, scanner: &ScanHandle, scopes: &[ScopedScan], queue: &mut DeleteQueue) {
    app.clear_visual_intent();
    app.deferred_delete = None;
    queue.cancel_all();
    stop_scanning(scanner, scopes);
    app.modal = Some(Modal::Stopping);
}

/// The first frame precedes all scanner filesystem work.
pub fn run(
    root: PathBuf,
    options: ScanOptions,
    size_mode: SizeMode,
    settings: Settings,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive browsing requires a terminal; use -o FILE to export, or -o - for stdout",
        ));
    }
    #[cfg(unix)]
    let mut interrupts = crate::interrupts::Interrupts::new()?;
    let mouse = settings.mouse;
    let mut app = App::with_settings(root.clone(), size_mode, settings)?;
    let _guard = TerminalGuard::enter(mouse)?;
    let cleaning_up = std::cell::Cell::new(false);
    let output = TerminalOutput {
        writer: io::stdout(),
        cleaning_up: &cleaning_up,
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(output))?;
    // Declared after Terminal so every early return and unwind disables its
    // destructor's output before TerminalGuard restores the terminal itself.
    let output_cleanup = TerminalOutputCleanup(&cleaning_up);
    terminal.draw(|frame| app.draw(frame, false, false))?;
    #[cfg(unix)]
    if interrupts.take()? {
        return Ok(());
    }
    let scanner = scan::spawn(root, options.clone());
    app.file_anchor = Some(Arc::clone(&scanner.anchor));
    let mut queue = DeleteQueue::new()?;
    #[cfg(unix)]
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
        if app.expire_delete_guard(Instant::now()) {
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
        app.pump_confirmation();
        let selection_progress = app.pump_visual();
        if app.pump_queue_drag(&mut queue) {
            frame_gate.dirty = true;
            next_frame = Instant::now();
        }
        if selection_progress && !app.visual.is_pending() && !app.parent_selection_work() {
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
        let (interrupt, input) = if failure.is_some() {
            // A disconnected input source stays ready forever. Do not poll it
            // again or skip queue completion while cancelled workers stop.
            let remaining = shutdown_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(INPUT_INTERVAL);
            std::thread::sleep(INPUT_INTERVAL.min(remaining));
            (false, Ok(false))
        } else {
            #[cfg(unix)]
            let interrupt = match interrupts.take() {
                Ok(interrupt) => interrupt,
                Err(error) => {
                    failure.get_or_insert(error);
                    continue;
                }
            };
            #[cfg(not(unix))]
            let interrupt = false;
            let input = if interrupt {
                Ok(true)
            } else {
                #[cfg(unix)]
                let ready = input_waiter.poll(
                    input_timeout,
                    queue.completion_fd(),
                    interrupts.as_fd(),
                    queue.active().is_some(),
                );
                #[cfg(not(unix))]
                // Crossterm's Unix reader skips raw and buffered input with a zero budget.
                let ready = event::poll(input_timeout.max(Duration::from_millis(1)));
                ready
            };
            (interrupt, input)
        };
        match input {
            Err(error) => {
                failure.get_or_insert(error);
                continue;
            }
            Ok(true) => {
                let event = if interrupt {
                    Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                } else {
                    match event::read() {
                        Ok(event) => event,
                        Err(error) => {
                            failure.get_or_insert(error);
                            continue;
                        }
                    }
                };
                let action = match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        // Keys can replace queue membership without changing its length.
                        refresh_retry.invalidate();
                        let reviewable = match app.modal.as_mut() {
                            Some(
                                Modal::Delete { reviewable, .. } | Modal::Batch { reviewable, .. },
                            ) if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) => {
                                Some(reviewable)
                            }
                            Some(
                                Modal::Quit { reviewable } | Modal::ConfirmQuit { reviewable, .. },
                            ) if matches!(key.code, KeyCode::Char('q' | 'y') | KeyCode::Enter) => {
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
                                    failure.get_or_insert(error);
                                    continue;
                                }
                            }
                        }
                        let anchor = scanner.anchor.get().cloned();
                        Some(app.key(key, anchor, deletion_busy(&queue, batch.as_ref())))
                    }
                    Event::Mouse(mouse) if app.settings.mouse => {
                        if matches!(
                            mouse.kind,
                            MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
                                | MouseEventKind::Up(MouseButton::Left | MouseButton::Right)
                        ) {
                            // Mouse events may precede the terminal's resize event.
                            // Never apply an old painted hitbox to new dimensions.
                            #[cfg(unix)]
                            let live_size = crossterm::terminal::window_size()
                                .map(|size| (size.columns, size.rows));
                            #[cfg(not(unix))]
                            let live_size = terminal.size().map(|size| (size.width, size.height));
                            match live_size {
                                Ok(size) if app.reject_stale_mouse_geometry(mouse.kind, size) => {
                                    frame_gate.dirty = true;
                                    next_frame = Instant::now();
                                    continue;
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    failure.get_or_insert(error);
                                    continue;
                                }
                            }
                        }
                        let deleting = deletion_busy(&queue, batch.as_ref());
                        let action = app
                            .header_right_mouse(mouse)
                            .or_else(|| app.modal_option_mouse(mouse, deleting))
                            .or_else(|| app.control_mouse(mouse, deleting))
                            .or_else(|| {
                                (!quitting)
                                    .then(|| app.queue_mouse(mouse, &mut queue))
                                    .flatten()
                            });
                        if action.is_some() {
                            refresh_retry.invalidate();
                        }
                        if action.is_none() && !quitting {
                            app.mouse(mouse);
                        }
                        Some(action.unwrap_or(Action::Continue))
                    }
                    Event::Resize(_, _) => {
                        app.header_right_press = None;
                        app.dialog_mouse.press = None;
                        app.cancel_right_press();
                        app.parent_press = None;
                        app.parent_area = Rect::default();
                        app.control_press = None;
                        app.control_hovered = None;
                        app.queue_drag = None;
                        app.cancel_breadcrumb_press();
                        frame_gate.dirty = true;
                        next_frame = Instant::now();
                        None
                    }
                    _ => None,
                };
                // Keyboard and mouse share admission, cancellation and shutdown.
                if let Some(action) = action {
                    match action {
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
                        Action::Refresh(id) => {
                            request_manual_refresh(&mut refresh_requests, &mut scans_stopped, id)
                        }
                        Action::Delete(id, anchor, relative, target) => {
                            enqueue_delete(
                                &mut app,
                                &scanner,
                                &scopes,
                                &mut queue,
                                (id, anchor, relative, target),
                            );
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
                        action @ (Action::QueueRemove | Action::QueueDelete) => {
                            if let Some(id) = app.queue_selection() {
                                let selected_active =
                                    queue.active().is_some_and(|entry| entry.id == id);
                                if selected_active && matches!(action, Action::QueueDelete) {
                                    queue.cancel_active();
                                } else if let Some(entry) = queue.remove_pending(id)
                                    && !scans_stopped
                                {
                                    refresh_requests.request(entry.node);
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
                    if closing && !app.finishing_deletions() {
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
                    if failure.is_none()
                        && let Err(error) = terminal.draw(|frame| {
                            app.sync_queue(&mut queue, frame.area());
                            frame_gate.painting(&app);
                            app.draw(
                                frame,
                                !scopes.is_empty() || !refresh_requests.is_empty(),
                                deletion_busy(&queue, batch.as_ref()),
                            )
                        })
                    {
                        failure.get_or_insert(error);
                    }
                    next_frame = Instant::now() + FRAME_INTERVAL;
                }
            }
            Ok(false) => {}
        }
        // A confirmation key can precede the asynchronous identity result.
        // Consume its intent once, after polling input so cancellation stays responsive.
        if !quitting && app.confirmation_ready() {
            let drawn = terminal.get_frame().area();
            #[cfg(unix)]
            let live_size =
                crossterm::terminal::window_size().map(|size| (size.columns, size.rows));
            #[cfg(not(unix))]
            let live_size = terminal.size().map(|size| (size.width, size.height));
            match live_size {
                Ok(size) if size == (drawn.width, drawn.height) => {
                    match app.take_ready_confirmation() {
                        Action::Delete(id, anchor, relative, target) => {
                            enqueue_delete(
                                &mut app,
                                &scanner,
                                &scopes,
                                &mut queue,
                                (id, anchor, relative, target),
                            );
                            queue.start_next();
                        }
                        Action::BatchConfirm => {
                            if let Some(preparation) = batch.as_mut() {
                                preparation.confirm_pending = true;
                            }
                        }
                        _ => {}
                    }
                    frame_gate.dirty = true;
                    next_frame = Instant::now();
                }
                Ok(_) => {
                    if let Some(
                        Modal::Delete {
                            reviewable,
                            confirm_requested,
                            ..
                        }
                        | Modal::Batch {
                            reviewable,
                            confirm_requested,
                            ..
                        },
                    ) = app.modal.as_mut()
                    {
                        *reviewable = false;
                        *confirm_requested = false;
                    }
                    frame_gate.dirty = true;
                    next_frame = Instant::now();
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
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
            if frame_gate.needs_frame(&app)
                && failure.is_none()
                && let Err(error) = terminal.draw(|frame| {
                    app.sync_queue(&mut queue, frame.area());
                    frame_gate.painting(&app);
                    app.draw(
                        frame,
                        !scopes.is_empty() || !refresh_requests.is_empty(),
                        deletion_busy(&queue, batch.as_ref()),
                    )
                })
            {
                failure.get_or_insert(error);
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
            || (selection_progress && (app.visual.is_pending() || app.parent_selection_work()))
        {
            Duration::ZERO
        } else {
            INPUT_INTERVAL.min(next_frame.saturating_duration_since(Instant::now()))
        };
        // Ready scanner output can be consumed on the next scan phase. Poll
        // input between bounded chunks without sleeping for work already here.
        // Producers without ready output and blocked ordering/admission workers
        // retain the minimum wait below.
        input_timeout = if scan_backlog
            || scope_backlog
            || batch_status.productive
            || queue_progress
            || (selection_progress && (app.visual.is_pending() || app.parent_selection_work()))
        {
            Duration::ZERO
        } else {
            timeout.max(Duration::from_millis(1))
        };
        if let Some(deadline) = app.delete_guard_deadline() {
            input_timeout = input_timeout.min(deadline.saturating_duration_since(Instant::now()));
        }
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
    drop(output_cleanup);
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

fn take_scan_entries(
    pending: &mut Option<(NodeId, std::vec::IntoIter<Entry>)>,
) -> Option<ScanEvent> {
    let (parent, entries) = pending.as_mut()?;
    if entries.len() > 128 {
        return Some(ScanEvent::Entries {
            parent: *parent,
            entries: entries.by_ref().take(128).collect(),
        });
    }
    // Moving an unadvanced IntoIter into collect preserves its Vec allocation.
    // Larger publications still advance only one bounded chunk at a time.
    let (parent, entries) = pending.take().expect("pending scan entries exist");
    Some(ScanEvent::Entries {
        parent,
        entries: entries.collect(),
    })
}

fn drain_scope(scope: &mut ScopedScan, app: &mut App, discard: bool, deadline: Instant) {
    if discard {
        scope.entries = None;
    }
    for _ in 0..MAX_BATCHES {
        if Instant::now() >= deadline {
            break;
        }
        if let Some(event) = take_scan_entries(&mut scope.entries) {
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
        if let Some(event) = take_scan_entries(&mut app.scan_entries) {
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
