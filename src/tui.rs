use crate::backend::RootAnchor;
use crate::delete::{self, DeleteHandle};
use crate::model::Model;
use crate::order::{CachedOrderWorker, CachedRow, CachedSnapshot, OrderConfig, OrderRow};
use crate::retire::Retirer;
use crate::scan::{self, ScanHandle, ScanOptions};
use crate::types::{Entry, NodeId, NodeKind, ROOT, ScanEvent, SizeMode, SortMode};
use crossbeam_channel::TryRecvError;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const INPUT_INTERVAL: Duration = Duration::from_millis(25);
const WORK_BUDGET: Duration = Duration::from_millis(4);
const MAX_BATCHES: usize = 32;
const ORDER_BATCH: usize = 256;
const PENDING_LIMIT: usize = 8192;
const VIEW_BUDGET: usize = 1024;
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard(Option<Arc<PanicHook>>);
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self(None);
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
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
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
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
    Delete {
        id: NodeId,
        anchor: Arc<RootAnchor>,
        relative: PathBuf,
        target: String,
        scroll: u16,
        reviewable: bool,
    },
}
impl Modal {
    fn help() -> Self {
        Self::Text {
            title: "Help",
            text: HELP.to_owned(),
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
    Refresh,
    Quit,
    Delete(NodeId, Arc<RootAnchor>, PathBuf),
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

    fn sync(&mut self, directory: Option<NodeId>, order: &CachedOrderWorker) {
        if self.directory != directory {
            self.directory = directory;
            self.view = Arc::clone(&self.empty);
        }
        if let Some(view) = directory.and_then(|id| order.snapshot(id)) {
            self.view = view;
        } else if self.view.generation != order.generation() {
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

struct App {
    model: Model,
    retirer: Retirer<Model>,
    root: PathBuf,
    directory: NodeId,
    selected: Option<NodeId>,
    view: Arc<CachedSnapshot>,
    empty: Arc<CachedSnapshot>,
    view_directory: NodeId,
    offset: usize,
    page_size: usize,
    config: OrderConfig,
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
    started: bool,
}

impl App {
    fn new(root: PathBuf, size_mode: SizeMode) -> io::Result<Self> {
        let retirer = Retirer::new()?;
        let mut order = CachedOrderWorker::new()?;
        let config = OrderConfig {
            size_mode,
            ..OrderConfig::default()
        };
        order.reset(config);
        let empty = Arc::new(CachedSnapshot {
            generation: 0,
            ids: Vec::new(),
            directory_ids: Vec::new(),
            positions: HashMap::new(),
        });
        let preview = Preview::new(Arc::clone(&empty));
        Ok(Self {
            model: Model::new(root.clone()),
            retirer,
            root,
            directory: ROOT,
            selected: None,
            view: Arc::clone(&empty),
            empty,
            view_directory: ROOT,
            offset: 0,
            page_size: 1,
            config,
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
            started: false,
        })
    }

    fn reset_order(&mut self, keep_previous: bool) {
        if keep_previous {
            self.order.configure(self.config);
            if let Some(navigation) = self.pending_navigation.as_mut() {
                navigation.cursor = None;
            }
        } else {
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

    fn removed(&mut self, id: NodeId) {
        let old_index = self.selection_index().unwrap_or(0);
        let changed = self.model.remove(id);
        // Deleted rows may already have contributed to a partial page movement.
        if let Some(navigation) = self.pending_navigation.as_mut() {
            navigation.cursor = None;
        }
        self.changed(changed);
        self.modal = None;
        if !self.model.is_live(self.directory) {
            while !self.model.is_live(self.directory) {
                self.directory = self.model.node(self.directory).parent.unwrap_or(ROOT);
            }
            self.selected = None;
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
                    let node = self.model.node(id);
                    self.unsent.push(CachedRow {
                        parent: node.parent,
                        row: OrderRow::from_node(id, node, self.config.size_mode),
                    });
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
            if let Some(view) = self.order.snapshot(self.directory) {
                if !Arc::ptr_eq(&self.view, &view) {
                    let row = (self.view_directory == self.directory)
                        .then(|| self.selection_screen_row())
                        .flatten();
                    self.view_directory = self.directory;
                    self.view = view;
                    if let Some(navigation) = self.pending_navigation.as_mut() {
                        navigation.cursor = None;
                    }
                    if self.selected.is_some_and(|id| {
                        !self.eligible(id) || self.model.node(id).parent != Some(self.directory)
                    }) {
                        self.selected = None;
                    }
                    if self.selected.is_none() && self.pending_navigation.is_none() {
                        self.pending_navigation = Some(Navigation::new(Some(false), 0));
                    }
                    if let (Some(row), Some(index)) = (row, self.selection_index()) {
                        self.offset = self.offset_for_row(index, row);
                    }
                    self.keep_selection_visible();
                }
            }
        }
        self.advance_navigation();
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
        self.view
            .ids
            .iter()
            .copied()
            .enumerate()
            .skip(self.offset)
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
        let Some(index) = self.selection_index() else {
            self.offset = 0;
            return;
        };
        if index < self.offset {
            self.offset = index;
        } else if self.selection_screen_row().is_none() {
            self.offset = self.offset_for_row(index, self.page_size.saturating_sub(1));
        }
    }

    fn edge_selection(&mut self, end: bool) {
        self.pending_navigation = Some(Navigation::new(Some(end), 0));
        self.advance_navigation();
    }

    fn move_selection(&mut self, amount: isize) {
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
        if !has_children {
            self.selected = None;
            self.offset = 0;
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
            self.selected = cursor.target;
            if let Some(end) = navigation.end {
                self.offset = if end {
                    self.selection_index().map_or(0, |index| {
                        self.offset_for_row(index, self.page_size.saturating_sub(1))
                    })
                } else {
                    0
                };
            }
            self.keep_selection_visible();
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
    fn reset_scan(&mut self) -> bool {
        let old = std::mem::replace(&mut self.model, Model::new(self.root.clone()));
        if let Err(old) = self.retirer.try_retire(old) {
            self.model = old;
            return false;
        }
        self.directory = ROOT;
        self.selected = None;
        self.started = false;
        self.order.reset(self.config);
        self.next_new = 0;
        self.replay_cursor = None;
        self.pending.clear();
        self.pending_set.clear();
        self.unsent.clear();
        self.removals.clear();
        self.scan_entries = None;
        self.preview.sync(None, &self.order);
        self.reset_order(false);
        true
    }

    fn key(&mut self, key: KeyEvent, anchor: Option<Arc<RootAnchor>>, deleting: bool) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if let Some(modal) = self.modal.take() {
            match modal {
                Modal::Delete {
                    id,
                    anchor,
                    relative,
                    target,
                    mut scroll,
                    reviewable,
                } => {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter if reviewable => {
                            return Action::Delete(id, anchor, relative);
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
                    });
                }
                Modal::Text {
                    title,
                    text,
                    mut scroll,
                } => match key.code {
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {}
                    KeyCode::Char('?') if title != "Help" => self.modal = Some(Modal::help()),
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
        if self.pending_navigation.is_some() && !moving {
            self.sync_order(true);
        }
        if (self.pending_navigation.is_some()
            || self
                .selected
                .is_some_and(|id| !self.eligible(id) || self.selection_index().is_none()))
            && matches!(
                key.code,
                KeyCode::Enter
                    | KeyCode::Right
                    | KeyCode::Char('l')
                    | KeyCode::Char('d')
                    | KeyCode::Char('i')
            )
        {
            return Action::Continue;
        }
        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char('j')
                | KeyCode::Char('k')
        ) {
            self.navigation_until = Instant::now() + Duration::from_millis(150);
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('?') => self.modal = Some(Modal::help()),
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
                    self.reset_order(false);
                }
            }
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(id) = self.selected {
                    if self.eligible(id) && self.model.node(id).kind == NodeKind::Directory {
                        self.directory = id;
                        self.selected = None;
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
                self.config.show_hidden = !self.config.show_hidden;
                if self.selected.is_some_and(|id| !self.eligible(id)) {
                    self.selected = None;
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
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('d') => {
                if deleting {
                    self.modal = Some(Modal::notice(
                        "Deletion in progress",
                        "Wait for the current deletion.".into(),
                    ));
                } else if let (Some(id), Some(anchor)) = (self.selected, anchor) {
                    let path = self.model.path(id);
                    match path.strip_prefix(&self.root) {
                        Ok(relative) if !relative.as_os_str().is_empty() => {
                            self.modal = Some(Modal::Delete {
                                id,
                                anchor,
                                relative: relative.to_owned(),
                                target: printable(path.as_os_str()),
                                scroll: 0,
                                reviewable: false,
                            });
                        }
                        _ => {
                            self.modal = Some(Modal::notice(
                                "Cannot delete",
                                "The scan root cannot be deleted.".into(),
                            ))
                        }
                    }
                } else {
                    self.modal = Some(Modal::notice(
                        "Cannot delete",
                        "Select an entry after the scan root is ready.".into(),
                    ));
                }
            }
            _ => {}
        }
        Action::Continue
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
            if node.complete && node.error_count == 0 {
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
                if node.error_count > 0 { "~" } else { "" },
                human_size(size)
            )
        };
        let mut spans = vec![Span::raw(format!("{size_label:>12} "))];
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
        if selected {
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
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(8)])
            .split(header);
        let phase = self.phase(refreshing, deleting);
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
        frame.render_widget(Paragraph::new(label).style(header_style), columns[0]);
        frame.render_widget(
            Paragraph::new("? help")
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
        self.preview.sync(hovered, &self.order);
        let (body, preview_area) = if hovered.is_some() {
            let parts = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(70), Constraint::Length(46)])
                .split(body);
            (parts[0], Some(parts[1]))
        } else {
            (body, None)
        };
        self.page_size = usize::from(body.height).clamp(1, VIEW_BUDGET);
        self.keep_selection_visible();
        let total = self.size(self.directory);
        static SPINNER_CLOCK: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let spinner = spinner_frame(
            SPINNER_CLOCK
                .get_or_init(Instant::now)
                .elapsed()
                .as_millis()
                / FRAME_INTERVAL.as_millis(),
        );
        let mut lines = Vec::with_capacity(self.page_size);
        for (_, id) in self.screen_rows() {
            lines.push(self.row(id, total, body.width, self.selected == Some(id), spinner));
        }
        if lines.is_empty() {
            let message = if self
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
                Line::from("Subdirectories").style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            for &id in self.preview.view.directory_ids.iter().take(1024) {
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
                    true,
                    self.config.show_hidden,
                ) {
                    "Ordering entries…"
                } else if directory.error_count > 0 {
                    "Partial listing; press i for details."
                } else if !self.model.listing_complete(hovered.unwrap()) {
                    "Waiting for scan…"
                } else {
                    "No subdirectories."
                };
                rows.push(Line::from(message).style(Style::default().fg(Color::DarkGray)));
            }
            frame.render_widget(Paragraph::new(rows), pane);
        }
        if let Some(modal) = self.modal.as_mut() {
            draw_modal(frame, area, modal);
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

fn draw_modal(frame: &mut Frame<'_>, area: Rect, modal: &mut Modal) {
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
        Modal::Delete {
            target,
            scroll,
            reviewable,
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
            *reviewable = inner.width >= 18 && regions[1].height > 0 && regions[2].height == 3;
            frame.render_widget(Clear, popup);
            if !*reviewable {
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
                    controls("y / Enter", ": delete"),
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
                    controls("Enter / Esc", ": close"),
                ])
                .style(style),
                regions[1],
            );
        }
    }
}

const HELP: &str = "↑/k ↓/j            Move selection
→/l/Enter          Open directory
←/h/Backspace/<    Parent directory
Home/End           First/last entry
PageUp/PageDown    Move one page
n/s/C              Sort by name/size/items; repeat to reverse
a                  Allocated/apparent sizes
t                  Directories first
g                  Fractional bar/percentage/no graph
c                  Item counts
e                  Show/hide dotfiles
i                  Entry information
d                  Delete after y/Enter confirmation; n/Esc cancels
r                  Rescan from the scan root
? / Esc            Close help
q / Ctrl-C         Quit (q closes this help first)

⠋ unfinished entry    ~ partial size after errors    ! read error
Sorting includes unfinished entries.
Near-size groups retain their previous order.
Graph shows the share of known directory size.
At 120+ columns, hovered directories preview their subdirectories.
Symlinks are not followed; hardlinks counted once.";

/// The first frame precedes all scanner filesystem work.
pub fn run(root: PathBuf, options: ScanOptions, size_mode: SizeMode) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive browsing requires a terminal; use --json for non-interactive output",
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Release))
        .map_err(io::Error::other)?;
    let mut app = App::new(root.clone(), size_mode)?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.draw(|frame| app.draw(frame, false, false))?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut scanner = Some(scan::spawn(root.clone(), options.clone()));
    let mut deletion: Option<(NodeId, DeleteHandle)> = None;
    let mut refreshing = false;
    app.started = true;
    terminal.draw(|frame| app.draw(frame, false, false))?;
    let mut next_frame = Instant::now() + FRAME_INTERVAL;
    while !stop.load(Ordering::Acquire) {
        if event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let anchor = if refreshing {
                        None
                    } else {
                        scanner
                            .as_ref()
                            .and_then(|handle| handle.anchor.get().cloned())
                    };
                    match app.key(key, anchor, deletion.is_some()) {
                        Action::Quit => break,
                        Action::Refresh => {
                            refreshing = true;
                            if let Some(handle) = scanner.as_ref() {
                                handle.cancel();
                            }
                        }
                        Action::Delete(id, anchor, relative) => {
                            deletion = Some((id, delete::spawn(anchor, relative)))
                        }
                        Action::Continue => {}
                    }
                    // Key changes are painted before another filesystem/event drain.
                    terminal.draw(|frame| app.draw(frame, refreshing, deletion.is_some()))?;
                    next_frame = Instant::now() + FRAME_INTERVAL;
                }
                Event::Resize(_, _) => {
                    terminal.draw(|frame| app.draw(frame, refreshing, deletion.is_some()))?;
                    next_frame = Instant::now() + FRAME_INTERVAL;
                }
                _ => {}
            }
            continue;
        }
        let scan_backlog = if let Some(handle) = scanner.as_ref() {
            drain(handle, &mut app, refreshing);
            app.scan_entries.is_some() || !handle.receiver.is_empty()
        } else {
            false
        };
        let changed = app.model.pump_removals();
        app.changed(changed);
        let order_backlog = app.pump_order();
        let delete_result =
            deletion
                .as_ref()
                .and_then(|(id, handle)| match handle.receiver.try_recv() {
                    Ok(result) => Some((*id, Some(result))),
                    Err(TryRecvError::Disconnected) => Some((*id, None)),
                    Err(TryRecvError::Empty) => None,
                });
        if let Some((id, result)) = delete_result {
            deletion.take();
            let parent = app.model.node(id).parent.unwrap_or(ROOT);
            let error = match result {
                Some(result) => {
                    match result.outcome {
                        Ok(()) => app.removed(id),
                        Err(error) => {
                            let message = printable(OsStr::new(&error.to_string()));
                            let changed = app
                                .model
                                .note_error(id, format!("Delete failed: {message}"));
                            app.changed(changed);
                            app.modal =
                                Some(Modal::notice("Delete failed", clipped(&message, 180)));
                        }
                    }
                    if let Some(metadata) = result.parent_metadata {
                        let changed = app.model.update_directory_sizes(parent, metadata.sizes);
                        app.changed(changed);
                    }
                    result.metadata_error
                }
                None => {
                    let message = "Deletion worker stopped before reporting its result.";
                    let changed = app.model.note_error(id, message.into());
                    app.changed(changed);
                    app.modal = Some(Modal::notice("Delete failed", message.into()));
                    None
                }
            };
            if let Some(error) = error {
                let changed = app.model.note_error(parent, error);
                app.changed(changed);
            }
            terminal.draw(|frame| app.draw(frame, refreshing, false))?;
            next_frame = Instant::now() + FRAME_INTERVAL;
        }
        if Instant::now() >= next_frame {
            if refreshing
                && deletion.is_none()
                && scanner.as_ref().is_none_or(ScanHandle::is_finished)
            {
                if !app.reset_scan() {
                    next_frame = Instant::now() + FRAME_INTERVAL;
                    continue;
                }
                scanner.take();
                terminal.draw(|frame| app.draw(frame, false, false))?;
                if stop.load(Ordering::Acquire) {
                    break;
                }
                scanner = Some(scan::spawn(root.clone(), options.clone()));
                app.started = true;
                refreshing = false;
            }
            terminal.draw(|frame| app.draw(frame, refreshing, deletion.is_some()))?;
            next_frame = Instant::now() + FRAME_INTERVAL;
        }
        let timeout = if scan_backlog || order_backlog || app.model.has_pending_removals() {
            Duration::ZERO
        } else {
            INPUT_INTERVAL.min(next_frame.saturating_duration_since(Instant::now()))
        };
        // A ready input is handled at the top of the next iteration.
        event::poll(timeout)?;
    }
    if let Some(handle) = scanner.as_ref() {
        handle.cancel();
    }
    Ok(())
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
                let changed = app.model.apply(event);
                app.changed(changed);
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if !discard && !app.model.finished {
                    app.model.finished = true;
                    app.model.last_error =
                        Some("Scanner stopped before reporting completion.".into());
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
        app.modal = Some(Modal::help());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(38, 8)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let first = rendered(&terminal);
        assert!(first.contains("Move selection"));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let last = rendered(&terminal);
        // Borders separate wrapped physical lines; verify the visible final content
        // without treating border cells as part of a logical sentence.
        assert!(
            last.contains("Help")
                && last.contains("subdirectories.")
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
                matches!(app.key(enter, None, false), Action::Delete(_, _, _)),
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
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
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
        app
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
        assert!(screen.contains("Subdirectories") && screen.contains("child-folder/"));
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
        assert!(!rendered(&terminal).contains("Subdirectories"));
        terminal.backend_mut().resize(120, 12);
        app.selected = Some(2);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("Subdirectories"));
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
        assert!(rendered(&terminal).contains("child-folder/"));
        assert!(!rendered(&terminal).contains("Loading"));
        app.key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!rendered(&terminal).contains("Subdirectories"));
        app.key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            None,
            false,
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(Arc::ptr_eq(&cached, &app.preview.view));
        assert!(rendered(&terminal).contains("child-folder/"));
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
            generation: app.order.generation(),
            ids: vec![4, 5, 3],
            directory_ids: vec![5],
            positions: HashMap::from([(4, 0), (5, 1), (3, 2)]),
        });
        app.offset = app.selection_index().unwrap() - old_row;
        app.keep_selection_visible();
        assert_eq!(app.offset, 2);
        assert_eq!(app.selection_screen_row().unwrap(), old_row);
    }

    #[test]
    fn stale_empty_snapshots_cannot_hide_known_main_or_preview_children() {
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
                entries: vec![entry(2, "child", NodeKind::Directory, 0)],
            },
        );
        apply(&mut app, ScanEvent::DirectoryDone { id: 2, error: None });
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
        assert!(!screen.contains("No child entries.") && !screen.contains("No subdirectories."));
        app.removed(2);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(rendered(&terminal).contains("No subdirectories."));
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
        app
    }

    #[test]
    fn page_movement_counts_visible_rows_and_does_not_scroll_past_visible_selection() {
        let mut app = navigation_fixture(16, 2..=5);
        app.config.show_hidden = false;
        app.selected = Some(1);
        app.move_selection(10);
        assert_eq!(app.selected, Some(15));
        assert_eq!(app.selection_screen_row(), Some(9));
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
}
