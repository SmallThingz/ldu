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
                Paragraph::new(app.row(
                    5,
                    24,
                    80,
                    false,
                    spinner_frame(0),
                    DeleteState::default(),
                )),
                Rect::new(0, 0, 80, 1),
            );
            frame.render_widget(
                Paragraph::new(app.row(
                    5,
                    24,
                    44,
                    false,
                    spinner_frame(1),
                    DeleteState::default(),
                )),
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
    let complete = app.row(5, 24, 44, false, spinner_frame(0), DeleteState::default());
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
            && last.contains("system utilities.")
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
        name: Arc::from(std::ffi::OsStr::new(name)),
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

fn finished_frame_fixture(root: PathBuf) -> App {
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
            entries: vec![entry(1, "root-file", NodeKind::File, 3)],
        },
    );
    apply(
        &mut app,
        ScanEvent::DirectoryDone {
            id: ROOT,
            error: None,
        },
    );
    apply(&mut app, ScanEvent::Finished { cancelled: false });
    app.started = true;
    app.baseline_finished = true;
    wait_cache(&mut app, ROOT, &[1]);
    app.sync_order(true);
    assert!(!app.frame_work());
    app
}

#[test]
fn settled_frames_stop_after_scan_and_static_visual_input() {
    let mut app = finished_frame_fixture(PathBuf::from("fixture"));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    let mut gate = FrameGate::new(&app);
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("Scan complete"));
    for _ in 0..8 {
        app.pump_confirmation();
        assert!(!app.pump_visual());
        app.pump_order();
        let changed = app.model.pump_removals();
        app.changed(changed);
        gate.observe_activity(app.frame_work());
        assert!(
            !gate.needs_frame(&app),
            "settled scan requested another frame"
        );
    }
    app.key(
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        None,
        false,
    );
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 35,
        row: 1,
        modifiers: KeyModifiers::SHIFT,
    });
    app.pump_visual();
    assert!(app.mouse_dragging);
    assert!(!app.frame_work(), "a held visual press does not animate");
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    gate.observe_activity(app.frame_work());
    assert!(!gate.needs_frame(&app));
}

fn assert_root_repairs_stay_settled(app: &mut App) {
    assert_eq!(app.directory, ROOT);
    assert!(!app.frame_work());
    app.prioritize_requested = false;
    let navigation_until = app.navigation_until;
    let view = Arc::clone(&app.view);
    let visual = app.visual.active();
    let mut gate = FrameGate::new(app);
    gate.painting(app);
    for _ in 0..8 {
        app.pump_confirmation();
        assert!(!app.pump_visual());
        app.pump_order();
        let changed = app.model.pump_removals();
        app.changed(changed);
        // Include the repair step used on every event-loop turn, even idle ones.
        app.repair_directory();
        assert!(
            !app.prioritize_requested,
            "settled root rearmed scan priority"
        );
        assert_eq!(app.navigation_until, navigation_until);
        assert!(Arc::ptr_eq(&app.view, &view));
        assert_eq!(app.visual.active(), visual);
        assert!(app.selected.is_none());
        gate.observe_activity(app.frame_work());
        assert!(!gate.needs_frame(app));
    }
}

#[test]
fn settled_leaf_and_failed_roots_do_not_rearm_repair_or_clear_visual_mode() {
    for kind in [
        NodeKind::Unknown,
        NodeKind::File,
        NodeKind::Symlink,
        NodeKind::Other,
    ] {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::RootInfo {
                file_identity: None,
                kind,
                sizes: Default::default(),
                error: (kind == NodeKind::Unknown).then(|| "missing root".into()),
            },
        );
        apply(&mut app, ScanEvent::Finished { cancelled: false });
        app.started = true;
        app.baseline_finished = true;
        wait_cache(&mut app, ROOT, &[]);
        app.sync_order(true);
        assert_root_repairs_stay_settled(&mut app);
        app.key(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.visual.active());
        assert_root_repairs_stay_settled(&mut app);
    }
}

#[test]
fn root_refresh_to_leaf_clears_old_targets_then_stays_settled() {
    for kind in [NodeKind::Unknown, NodeKind::File, NodeKind::Symlink] {
        let mut app = finished_frame_fixture(PathBuf::from("fixture"));
        app.visual.enter(ROOT, Some(1));
        app.begin_model_refresh(ROOT, 1);
        assert!(app.selected.is_none() && !app.visual.active());
        for event in [
            ScanEvent::RootInfo {
                file_identity: None,
                kind,
                sizes: Default::default(),
                error: (kind == NodeKind::Unknown).then(|| "missing root".into()),
            },
            ScanEvent::Finished { cancelled: false },
        ] {
            let changed = app.model.apply_scoped(ROOT, 1, event);
            app.changed(changed);
        }
        let changed = app.model.pump_removals();
        app.changed(changed);
        wait_cache(&mut app, ROOT, &[]);
        app.sync_order(true);
        assert!(!app.model.is_live(1));
        assert_root_repairs_stay_settled(&mut app);
    }
}

#[test]
fn final_status_frame_survives_work_ending_without_model_changes() {
    let mut app = finished_frame_fixture(PathBuf::from("fixture"));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    let mut gate = FrameGate::new(&app);
    gate.observe_activity(true);
    gate.painting(&app);
    terminal.draw(|frame| app.draw(frame, true, false)).unwrap();
    assert!(rendered(&terminal).contains("Refreshing"));
    let revision = app.model.revision();
    gate.observe_activity(false);
    assert_eq!(app.model.revision(), revision);
    assert!(gate.needs_frame(&app));
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("Scan complete"));
    gate.observe_activity(false);
    assert!(!gate.needs_frame(&app));
}

#[test]
fn deferred_refresh_phase_changes_once_without_continuous_frames() {
    let mut app = finished_frame_fixture(PathBuf::from("fixture"));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    let mut requests = RefreshRequests::default();
    let mut gate = FrameGate::new(&app);
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    requests.request(ROOT);
    gate.observe_refreshing(!requests.is_empty());
    assert!(gate.needs_frame(&app));
    gate.painting(&app);
    terminal.draw(|frame| app.draw(frame, true, false)).unwrap();
    assert!(rendered(&terminal).contains("Refreshing"));
    for _ in 0..8 {
        // A request waiting behind a paused overlapping deletion cannot run.
        requests.defer_front();
        gate.observe_refreshing(!requests.is_empty());
        gate.observe_activity(app.frame_work());
        assert!(!gate.needs_frame(&app));
    }
    requests.pop_front();
    gate.observe_refreshing(!requests.is_empty());
    assert!(gate.needs_frame(&app));
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("Scan complete"));
}

#[test]
fn idle_frame_gate_observes_late_main_and_directory_preview_orders() {
    let mut app = browser_fixture();
    app.selected = Some(1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let mut gate = FrameGate::new(&app);
    gate.painting(&app);
    let revision = app.model.revision();
    let mut child = CachedRow::from_model(4, &app.model, app.config.size_mode);
    child.row.sizes.allocated = 100;
    child.row.size = 100;
    app.order.try_upsert(vec![child]).unwrap();
    wait_cache(&mut app, 1, &[4, 3, 5]);
    assert_eq!(app.model.revision(), revision);
    assert!(gate.needs_frame(&app), "late preview publication was lost");
    gate.painting(&app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.preview.view.ids, [4, 3, 5]);
    assert!(!gate.needs_frame(&app));
    let mut child = CachedRow::from_model(2, &app.model, app.config.size_mode);
    child.row.sizes.allocated = 200;
    child.row.size = 200;
    app.order.try_upsert(vec![child]).unwrap();
    wait_cache(&mut app, ROOT, &[2, 1]);
    assert!(gate.needs_frame(&app), "late main publication was lost");
}

#[test]
fn preview_published_after_frame_observation_still_requests_a_frame() {
    std::fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(fixture.path().join("root-file"), "late preview").unwrap();
    let mut app = finished_frame_fixture(fixture.path().to_owned());
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
    let mut gate = FrameGate::new(&app);
    gate.painting(&app);
    // Completion after the pre-draw observation must remain unpainted,
    // even if it occurs before Terminal::draw returns.
    app.file_preview
        .request(1, anchor, "root-file".into(), NodeKind::File);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.file_preview.snapshot().is_none() {
        assert!(Instant::now() < deadline, "file preview did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(gate.needs_frame(&app));
    gate.painting(&app);
    assert!(!gate.needs_frame(&app));
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
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    app.queue_navigate(&queue, None, 1);
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    assert_eq!(app.queue_selection(), Some(selected));
    assert!(queue.move_pending(selected, 1));
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    assert_eq!(app.queue_selection(), Some(selected));
    queue.remove_pending(first);
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    assert_eq!(app.queue_selection(), Some(selected));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 18)).unwrap();
    terminal
        .draw(|frame| {
            app.sync_queue(&mut queue, frame.area());
            app.draw(frame, false, true);
        })
        .unwrap();
    assert!(rendered(&terminal).contains("Deletion queue"));
    assert!(rendered(&terminal).contains("unmodified-selected"));
}

#[cfg(target_os = "linux")]
#[test]
fn paused_queue_resize_paints_current_capacity_without_an_idle_correction() {
    fn paint(
        app: &mut App,
        queue: &mut DeleteQueue,
        gate: &mut FrameGate,
        terminal: &mut Terminal<ratatui::backend::TestBackend>,
        expected_rows: usize,
    ) {
        assert!(gate.needs_frame(app));
        terminal
            .draw(|frame| {
                app.sync_queue(queue, frame.area());
                gate.painting(app);
                app.draw(frame, false, true);
            })
            .unwrap();
        assert_eq!(app.queue_view.rows.len(), expected_rows);
        assert!(
            app.queue_view
                .rows
                .iter()
                .any(|row| Some(row.0) == app.queue_selection())
        );
        let selected = app
            .queue_view
            .rows
            .iter()
            .find(|row| Some(row.0) == app.queue_selection())
            .unwrap();
        let screen = rendered(terminal);
        assert!(screen.contains(&selected.2));
        assert_eq!(screen.matches("queued  unmodified-").count(), expected_rows);
        for _ in 0..8 {
            app.pump_order();
            gate.observe_activity(app.frame_work() || queue.is_updating());
            assert!(
                !gate.needs_frame(app),
                "paused queue required a corrective frame"
            );
        }
    }

    let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
    let mut queue = DeleteQueue::new().unwrap();
    queue.toggle_pause();
    for id in 1..=3 {
        queue.enqueue(id, Arc::clone(&anchor), format!("unmodified-{id}").into());
    }
    let mut app = finished_frame_fixture(PathBuf::from("fixture"));
    assert!(matches!(
        app.key(
            KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
            None,
            true
        ),
        Action::Continue
    ));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    let mut gate = FrameGate::new(&app);
    // The cold frame must already contain every row that fits.
    paint(&mut app, &mut queue, &mut gate, &mut terminal, 3);
    for height in [9, 20] {
        terminal.backend_mut().resize(80, height);
        gate.dirty = true; // The event loop's Resize branch.
        paint(
            &mut app,
            &mut queue,
            &mut gate,
            &mut terminal,
            if height == 9 { 1 } else { 3 },
        );
    }
    let Action::QueueNavigate { end, amount } =
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, true)
    else {
        panic!("queue End did not navigate");
    };
    app.queue_navigate(&queue, end, amount);
    gate.dirty = true; // Key input paints immediately.
    paint(&mut app, &mut queue, &mut gate, &mut terminal, 3);
    assert_eq!(app.queue_selection(), queue.last_id());
    for height in [9, 20] {
        terminal.backend_mut().resize(80, height);
        gate.dirty = true;
        paint(
            &mut app,
            &mut queue,
            &mut gate,
            &mut terminal,
            if height == 9 { 1 } else { 3 },
        );
        assert_eq!(app.queue_selection(), queue.last_id());
    }
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
            Vec::new(),
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
            matches!(&app.modal, Some(Modal::DeleteErrors { .. }))
                && app
                    .delete_notices
                    .front()
                    .is_some_and(|report| report.target.contains("folder")
                        && report.errors[0].message.contains("parent denies removal"))
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
        app.delete_notices.push_back(DeleteReport::single(
            app.root.clone(),
            "preserved failure".to_owned(),
        ));
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
    assert!(app.delete_notices.is_empty());
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
    assert!(app.delete_notices.is_empty());
    pump_retired_batches(&mut app, &mut retired);
    assert!(retired.is_empty());
    queue.cancel_all();
}

#[cfg(target_os = "linux")]
#[test]
fn cancelled_batch_retires_large_published_failures_without_dropping_them() {
    std::fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
    let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
    app.modal = Some(Modal::Enqueueing);
    let errors: Vec<_> = (0..100_000)
        .map(|index| DeleteError {
            path: fixture.path().join(format!("target-{index:06}")),
            message: "Could not prepare deletion: permission denied".to_owned(),
        })
        .collect();
    let allocation = errors.as_ptr();
    let worker = BatchCapture::finishing_after_empty(vec![
        BatchEvent::Failures(errors),
        BatchEvent::Done,
    ]);
    // Publish the owned payload deterministically before admission is cancelled.
    assert!(worker.event().is_none());
    assert!(worker.is_finished() && !worker.is_drained());
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
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true),
        Action::BatchCancel
    ));
    cancel_batch(&mut app, &mut batch, &mut retired);
    pump_retired_batches(&mut app, &mut retired);
    assert!(batch.is_none() && retired.is_empty());
    assert!(app.modal.is_none() && app.delete_notices.is_empty());
    let report = app.retired_delete_notices.front().unwrap();
    assert_eq!(report.errors.len(), 100_000);
    assert_eq!(report.errors.as_ptr(), allocation);
    assert!(
        report.target.is_empty(),
        "cancelled errors became a visible report"
    );
    // The next regular notice pump transfers ownership to background retirement.
    assert!(!app.show_delete_notice());
    assert!(app.retired_delete_notices.is_empty());
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
                assert_eq!(
                    app.delete_notices
                        .front()
                        .map(|report| report.errors[0].message.as_str()),
                    Some(message)
                );
                assert!(app.previous_modal.is_none());
                assert!(!app.show_delete_notice());
                if stopping {
                    assert!(matches!(app.modal, Some(Modal::Stopping)));
                } else {
                    assert!(matches!(app.modal, Some(Modal::Quit { .. })));
                    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
                    assert!(app.show_delete_notice());
                    assert!(matches!(app.modal, Some(Modal::DeleteErrors { .. })));
                    assert_eq!(app.delete_notices.len(), 1);
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
fn deferred_delete_notices_preserve_fifo_and_survive_other_removal() {
    let mut app = browser_fixture();
    app.modal = Some(Modal::help(app.visual.active()));
    app.delete_failed(2, &io::Error::other("first failure"), Vec::new());
    app.delete_failed(1, &io::Error::other("second failure"), Vec::new());
    app.removed(2);
    assert!(matches!(app.modal, Some(Modal::Text { title: "Help", .. })));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
    assert!(app.show_delete_notice());
    assert_eq!(app.delete_notices.len(), 2);
    assert_eq!(app.delete_notices[0].errors[0].message, "first failure");
    app.removed(5);
    assert!(matches!(app.modal, Some(Modal::DeleteErrors { .. })));
    app.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        None,
        false,
    );
    assert!(matches!(
        app.modal,
        Some(Modal::DeleteErrors { scroll: 0, .. })
    ));
    assert_eq!(app.delete_notices.len(), 1);
    assert_eq!(app.delete_notices[0].errors[0].message, "second failure");
    app.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        None,
        false,
    );
    assert!(app.modal.is_none() && app.delete_notices.is_empty());
}

#[test]
fn deletion_report_virtualizes_a_hundred_thousand_errors_and_scrolls_past_u16() {
    let mut app = browser_fixture();
    let report = DeleteReport {
        target: "/full/absolute/deletion/target".to_owned(),
        errors: (0..100_000)
            .map(|index| DeleteError {
                path: PathBuf::from(format!("/full/absolute/deletion/target/file-{index:06}")),
                message: format!("denied-{index:06}"),
            })
            .collect(),
    };
    let lines = report.visible_lines(199_994, 4, 0, 120);
    assert_eq!(lines.len(), 4);
    assert!(lines[0].to_string().ends_with("file-099997"));
    assert_eq!(lines[1].to_string(), "denied-099997");
    assert_eq!(lines[3].to_string(), "denied-099998");
    assert!(report.visible_lines(200_000, 50, 0, 120).is_empty());
    app.delete_notices.push_back(report);
    assert!(app.show_delete_notice());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 15)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let initial = rendered(&terminal);
    assert!(initial.contains("100000 errors"));
    assert!(initial.contains("/full/absolute/deletion/target"));
    assert!(!initial.contains("denied-099999"));
    app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), None, false);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("denied-099999"));
    assert!(
        matches!(app.modal, Some(Modal::DeleteErrors { scroll, .. }) if scroll > usize::from(u16::MAX))
    );
    app.key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        None,
        false,
    );
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("denied-000000"));
    // The large owned report is handed to the retire worker on dismissal.
    app.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        None,
        false,
    );
    assert!(app.modal.is_none() && app.delete_notices.is_empty());
}

#[test]
fn deletion_reports_keep_full_escaped_paths_and_mouse_horizontal_scrolling() {
    let mut app = browser_fixture();
    let path = PathBuf::from(format!("/{}end\nname", "long-component/".repeat(15)));
    app.delete_failed(
        1,
        &io::Error::other("failed"),
        vec![DeleteError {
            path: path.clone(),
            message: "denied\nagain".to_owned(),
        }],
    );
    assert!(app.delete_notices[0].target.starts_with('/'));
    assert_eq!(app.delete_notices[0].errors[0].path, path);
    assert_eq!(
        app.delete_notices[0].visible_lines(1, 1, 0, 50)[0].to_string(),
        "denied\\nagain"
    );
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    for _ in 0..23 {
        app.key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            None,
            false,
        );
    }
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("end\\nname"));
    app.key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        None,
        false,
    );
    assert!(matches!(
        app.modal,
        Some(Modal::DeleteErrors {
            horizontal: 0,
            scroll: 0,
            ..
        })
    ));
    assert_eq!(horizontal_slice("a界bc", 2, 3), " bc");
}

#[test]
fn deletion_reports_arriving_while_one_is_open_are_queued_and_keep_quit_guards() {
    let mut app = browser_fixture();
    app.delete_failed(1, &io::Error::other("first"), Vec::new());
    app.delete_failed(2, &io::Error::other("second"), Vec::new());
    assert_eq!(app.delete_notices.len(), 2);
    assert!(matches!(
        app.key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            None,
            true
        ),
        Action::QuitWarning
    ));
    assert!(matches!(app.modal, Some(Modal::Quit { reviewable: false })));
    assert!(matches!(
        app.previous_modal,
        Some(Modal::DeleteErrors { .. })
    ));
    app.delete_failed(5, &io::Error::other("third"), Vec::new());
    assert_eq!(app.delete_notices.len(), 3);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true);
    for message in ["first", "second", "third"] {
        assert!(matches!(app.modal, Some(Modal::DeleteErrors { .. })));
        assert_eq!(app.delete_notices[0].errors[0].message, message);
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            true,
        );
    }
    assert!(app.modal.is_none() && app.delete_notices.is_empty());
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

fn hover_intent(app: &App, id: NodeId) -> VisualIntent {
    let row = app.screen_rows().position(|(_, row)| row == id).unwrap();
    VisualIntent::Hover(PendingHover {
        column: app.list_area.x,
        row: app.list_area.y + row as u16,
        directory: app.directory,
        child_epoch: app.view.child_epoch,
    })
}

fn pending_folder_selection() -> App {
    let mut app = browser_fixture();
    app.directory = 1;
    app.selected = Some(4);
    app.reset_order(false);
    app.visual.enter(1, Some(3));
    app.visual
        .begin_range(Arc::clone(&app.view), 4, RangeMode::Add);
    app.list_area = Rect::new(0, 1, 80, app.page_size as u16);
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
                2 => app.enqueue_visual(hover_intent(&app, 3)),
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
        assert!(
            !app.visual.contains(5),
            "a removed target marked its neighbour"
        );
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
fn rejected_deferred_mouse_clicks_discard_only_dependent_toggles() {
    for stale_epoch in [false, true] {
        for enters in [1, 3] {
            for later in 0..3 {
                let mut app = pending_folder_selection();
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let mouse = |kind, row| MouseEvent {
                    kind,
                    column: 35,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    app.mouse(mouse(kind, 3));
                }
                for _ in 0..enters {
                    app.key(
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        None,
                        false,
                    );
                }
                assert!(matches!(
                    app.visual_intents.front(),
                    Some(VisualIntent::Mouse(gesture)) if gesture.released
                ));
                assert!(matches!(
                    app.visual_intents.get(1),
                    Some(VisualIntent::ToggleCurrent)
                ));
                if later == 1 {
                    app.key(
                        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                        None,
                        false,
                    );
                } else if later == 2 {
                    for kind in [
                        MouseEventKind::Down(MouseButton::Left),
                        MouseEventKind::Up(MouseButton::Left),
                    ] {
                        app.mouse(mouse(kind, 1));
                    }
                }
                if later != 0 {
                    app.key(
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        None,
                        false,
                    );
                }
                if stale_epoch {
                    let Some(VisualIntent::Mouse(gesture)) = app.visual_intents.front_mut()
                    else {
                        panic!("the rejected click must precede its dependent toggles");
                    };
                    gesture.view = Arc::new(CachedSnapshot {
                        generation: gesture.view.generation,
                        child_epoch: gesture.view.child_epoch.wrapping_add(1),
                        ids: gesture.view.ids.clone(),
                        positions: gesture.view.positions.clone(),
                    });
                } else {
                    app.removed(5);
                }
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(if later == 0 { 4 } else { 3 }));
                assert_eq!(app.visual.contains(3), later != 1);
                assert!(app.visual.contains(4));
                assert!(!app.visual.contains(5));
                assert_eq!(app.visual.len(), if later == 1 { 1 } else { 2 });
                assert!(app.visual_intents.is_empty());
            }
        }
    }
}

#[test]
fn exhausted_deferred_mouse_intervals_do_not_toggle_repair_cursor() {
    for shift in [false, true] {
        for new_row in [false, true] {
            for later_navigation in [false, true] {
                let mut app = pending_folder_selection();
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let mouse = |kind, row| MouseEvent {
                    kind,
                    column: 35,
                    row,
                    modifiers: if shift {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                };
                app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2));
                app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
                app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
                if later_navigation {
                    for code in [KeyCode::Home, KeyCode::Enter] {
                        app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
                    }
                }
                app.removed(4);
                app.removed(5);
                if new_row {
                    app.removed(3);
                    apply(
                        &mut app,
                        ScanEvent::Entries {
                            parent: 1,
                            entries: vec![entry(6, "new-row", NodeKind::File, 0)],
                        },
                    );
                    wait_cache(&mut app, 1, &[6]);
                }
                finish_visual(&mut app);
                let survivor = if new_row { 6 } else { 3 };
                let marked = new_row == later_navigation;
                assert_eq!(app.selected, Some(survivor));
                assert_eq!(app.visual.contains(survivor), marked);
                assert_eq!(app.visual.len(), usize::from(marked));
                assert!(!app.visual.contains(4) && !app.visual.contains(5));
                assert!(app.visual_intents.is_empty());
            }
        }
    }
}

#[test]
fn deferred_mouse_surviving_intervals_bind_dependent_enter() {
    for shift in [false, true] {
        for survivor in 3..=5 {
            let mut app = pending_folder_selection();
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let mouse = |kind, row| MouseEvent {
                kind,
                column: 35,
                row,
                modifiers: if shift {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            };
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1));
            app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3));
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
            for id in 3..=5 {
                if id != survivor {
                    app.removed(id);
                }
            }
            finish_visual(&mut app);
            let marked = shift && survivor != 5;
            assert_eq!(app.selected, Some(survivor));
            assert_eq!(app.visual.contains(survivor), marked);
            assert_eq!(app.visual.len(), usize::from(marked));
            assert!(app.visual_intents.is_empty());
        }
    }
}

#[test]
fn distant_surviving_mouse_interval_cursor_keeps_dependent_enter() {
    for mode in [RangeMode::Add, RangeMode::Invert] {
        let mut app = navigation_fixture(600, 601..=601);
        app.selected = Some(300);
        app.visual.enter(ROOT, Some(300));
        app.visual
            .begin_range(Arc::clone(&app.view), 300, RangeMode::Add);
        app.enqueue_visual(VisualIntent::Mouse(PendingMouse {
            anchor: 1,
            lead: 600,
            dragged: true,
            released: true,
            mode,
            edge: 0,
            view: Arc::clone(&app.view),
            offset: app.offset,
        }));
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        for id in 1..=600 {
            if id != 300 {
                app.removed(id);
            }
        }
        finish_visual(&mut app);
        let marked = mode == RangeMode::Invert;
        assert_eq!(app.selected, Some(300));
        assert_eq!(app.visual.contains(300), marked);
        assert_eq!(app.visual.len(), usize::from(marked));
        assert!(app.visual_intents.is_empty());
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
            app.enqueue_visual(hover_intent(&app, 3));
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
            app.enqueue_visual(hover_intent(&app, 5));
            assert!(matches!(
                app.visual_intents.back(),
                Some(VisualIntent::Hover(hover)) if hover.row == app.list_area.y + 2
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
fn rejected_cursor_input_preserves_the_last_admitted_hover() {
    for input in 0..3 {
        let mut app = pending_folder_selection();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 35,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hover_position, Some((35, 2)));
        while app.visual_intents.len() < VISUAL_INTENT_LIMIT {
            app.enqueue_visual(VisualIntent::ToggleStable(3));
        }
        assert!(!app.visual_input_available());
        match input {
            0 | 1 => app.mouse(MouseEvent {
                kind: if input == 0 {
                    MouseEventKind::Moved
                } else {
                    MouseEventKind::ScrollDown
                },
                column: 35,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            _ => {
                app.key(
                    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    None,
                    false,
                );
            }
        }
        assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
        assert_eq!(app.hover_position, Some((35, 2)), "input={input}");
        finish_visual(&mut app);
        assert_eq!(app.hover_position, Some((35, 2)), "input={input}");
    }
}

#[test]
fn rejected_visual_presses_preserve_hover_without_acquiring_capture() {
    for shift in [false, true] {
        for followup in 0..8 {
            let mut app = pending_folder_selection();
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let mouse = |kind, column, row, modifiers| MouseEvent {
                kind,
                column,
                row,
                modifiers,
            };
            app.mouse(mouse(MouseEventKind::Moved, 35, 2, KeyModifiers::NONE));
            while app.visual_intents.len() < VISUAL_INTENT_LIMIT {
                app.enqueue_visual(VisualIntent::ToggleStable(3));
            }
            app.mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                35,
                1,
                if shift {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            ));
            assert_eq!(app.hover_position, Some((35, 2)));
            assert!(app.mouse_position.is_none() && !app.has_held_mouse());
            let (column, row) = match followup {
                0 => (35, 1),
                1 => (35, 3),
                2 | 3 => (1, 0),
                4 => (90, 2),
                5 => {
                    app.mouse(mouse(MouseEventKind::Moved, 90, 2, KeyModifiers::NONE));
                    (35, 2)
                }
                6 => (35, 0),
                _ => (35, 12),
            };
            if matches!(followup, 1 | 3 | 7) {
                app.mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    column,
                    row,
                    KeyModifiers::NONE,
                ));
            }
            app.mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row,
                KeyModifiers::NONE,
            ));
            let expected_hover = (followup < 2).then_some((35, 2));
            assert_eq!(app.hover_position, expected_hover);
            assert_eq!(app.back_hovered, matches!(followup, 2 | 3));
            assert_eq!(app.visual_intents.len(), VISUAL_INTENT_LIMIT);
            assert!(app.mouse_position.is_none() && app.mouse_press_view.is_none());
            assert!(app.mouse_baseline.is_none() && app.drag_anchor.is_none());
            assert!(app.drag_scroll.is_none() && !app.mouse_dragging && !app.has_held_mouse());
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(4));
            assert_eq!(app.hover_position, expected_hover);
        }
    }
}

fn ignored_mouse_kinds() -> [MouseEventKind; 8] {
    [
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Right),
        MouseEventKind::Drag(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Middle),
        MouseEventKind::Up(MouseButton::Middle),
        MouseEventKind::Drag(MouseButton::Middle),
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
    ]
}

#[test]
fn ignored_mouse_input_keeps_hovered_rows_through_live_publications() {
    for kind in ignored_mouse_kinds() {
        for row in [1, 10] {
            for mode in 0..3 {
                let mut app = navigation_fixture(40, 41..=41);
                app.selected = Some(11);
                app.offset = 10;
                if mode != 0 {
                    app.visual.enter(ROOT, Some(11));
                }
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(100, 11)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let mouse = |kind| MouseEvent {
                    kind,
                    column: 35,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
                app.mouse(mouse(MouseEventKind::Moved));
                if mode == 2 {
                    app.visual
                        .begin_range(Arc::clone(&app.view), 40, RangeMode::Add);
                    while app.visual_intents.len() < VISUAL_INTENT_LIMIT {
                        app.enqueue_visual(VisualIntent::ToggleStable(1));
                    }
                }
                let selected = app.selected;
                let painted_rows = app.painted_rows.clone();
                let marks = app.visual.snapshot();
                let intents = app.visual_intents.len();
                for resort in [false, true] {
                    let previous = Arc::clone(&app.view);
                    app.mouse(mouse(kind));
                    assert_eq!(app.hover_position, Some((35, row)), "{kind:?}");
                    apply(
                        &mut app,
                        ScanEvent::Entries {
                            parent: if resort { 41 } else { ROOT },
                            entries: vec![if resort {
                                entry(42, "large-child", NodeKind::File, 10_000)
                            } else {
                                entry(41, "new-last", NodeKind::Directory, 0)
                            }],
                        },
                    );
                    let ids: Vec<_> = if resort {
                        std::iter::once(41).chain(1..=40).collect()
                    } else {
                        (1..=41).collect()
                    };
                    wait_cache(&mut app, ROOT, &ids);
                    app.sync_order(true);
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    assert!(!Arc::ptr_eq(&previous, &app.painted_view));
                    assert_eq!(
                        (app.selected, app.offset, app.selection_screen_row()),
                        (
                            selected,
                            10 + usize::from(resort),
                            Some(usize::from(row - 1))
                        ),
                        "kind={kind:?}, row={row}, mode={mode}, resort={resort}"
                    );
                    assert_eq!(app.painted_rows, painted_rows);
                    assert_eq!(app.hover_position, Some((35, row)));
                    assert_eq!(app.visual.snapshot(), marks);
                    assert_eq!(app.visual_intents.len(), intents);
                }
            }
        }
    }
}

#[test]
fn ignored_mouse_input_preserves_only_in_list_hover() {
    for directory in [ROOT, 1] {
        for kind in ignored_mouse_kinds() {
            let mut app = browser_fixture();
            app.directory = directory;
            app.reset_order(false);
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            for (column, row, keep_hover, back) in [
                (35, 2, true, false),
                (35, 9, true, false),
                (1, 0, false, true),
                (35, 0, false, false),
                (90, 2, false, false),
                (35, 12, false, false),
            ] {
                app.mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 35,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                });
                let selected = app.selected;
                app.mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                });
                assert_eq!(app.hover_position, keep_hover.then_some((35, 1)));
                assert_eq!(app.back_hovered, back);
                assert_eq!(app.directory, directory);
                assert_eq!(app.selected, selected);
                assert!(app.mouse_press_view.is_none() && !app.has_held_mouse());
            }
            app.mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 35,
                row: 1,
                modifiers: KeyModifiers::NONE,
            });
            app.modal = Some(Modal::help(false));
            app.mouse(MouseEvent {
                kind,
                column: 35,
                row: 1,
                modifiers: KeyModifiers::NONE,
            });
            assert!(app.hover_position.is_none() && !app.back_hovered);
        }
    }
}

#[test]
fn ignored_mouse_input_does_not_interrupt_captured_gestures() {
    for deferred in [false, true] {
        for shift in [false, true] {
            let mut app = browser_fixture();
            app.directory = 1;
            app.reset_order(false);
            app.visual.enter(1, Some(3));
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            if deferred {
                app.visual
                    .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
            }
            let mouse = |kind, column, row| MouseEvent {
                kind,
                column,
                row,
                modifiers: if shift {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            };
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 35, 1));
            app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 35, 11));
            let view = app.mouse_press_view.clone();
            let selected = app.selected;
            let marks = app.visual.snapshot();
            let intents = app.visual_intents.len();
            let dragging = app.mouse_dragging;
            let started = app.drag_started;
            let scroll = app.drag_scroll;
            for kind in ignored_mouse_kinds() {
                for (column, row) in [(35, 2), (35, 9), (1, 0), (90, 2), (35, 12)] {
                    app.mouse(mouse(kind, column, row));
                    assert!(app.hover_position.is_none());
                    assert_eq!(app.mouse_position, Some((35, 11)));
                    assert_eq!(app.selected, selected);
                    assert_eq!(app.visual.snapshot(), marks);
                    assert_eq!(app.visual_intents.len(), intents);
                    assert_eq!(app.mouse_dragging, dragging);
                    assert_eq!(app.drag_started, started);
                    assert_eq!(app.drag_scroll, scroll);
                    assert_eq!(app.has_held_mouse(), deferred);
                    if let Some(view) = &view {
                        assert!(Arc::ptr_eq(app.mouse_press_view.as_ref().unwrap(), view));
                    } else {
                        assert!(app.mouse_press_view.is_none());
                        let gesture = app.held_mouse().unwrap();
                        assert_eq!((gesture.anchor, gesture.lead, gesture.edge), (3, 5, 1));
                        assert!(gesture.dragged && !gesture.released);
                    }
                }
            }
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 35, 11));
            finish_visual(&mut app);
            assert_eq!(app.selected, Some(5));
            assert_eq!(app.visual.contains(3), !shift);
            assert!(app.visual.contains(4) && app.visual.contains(5));
            assert!(app.mouse_press_view.is_none() && !app.has_held_mouse());
            assert!(app.drag_scroll.is_none() && app.mouse_position.is_none());
        }
    }
}

#[test]
fn ignored_keys_and_display_changes_preserve_hover_mode() {
    for (filter_editing, code, modifiers) in [
        (false, KeyCode::Delete, KeyModifiers::NONE),
        (true, KeyCode::Up, KeyModifiers::NONE),
        (true, KeyCode::Char('x'), KeyModifiers::ALT),
        (false, KeyCode::Char('g'), KeyModifiers::NONE),
        (false, KeyCode::Char('c'), KeyModifiers::NONE),
        (false, KeyCode::Char('n'), KeyModifiers::NONE),
        (false, KeyCode::Char('s'), KeyModifiers::NONE),
        (false, KeyCode::Char('C'), KeyModifiers::NONE),
        (false, KeyCode::Char('a'), KeyModifiers::NONE),
        (false, KeyCode::Char('t'), KeyModifiers::NONE),
    ] {
        let mut app = browser_fixture();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.filter_editing = filter_editing;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 35,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hover_position, Some((35, 1)));
        app.key(KeyEvent::new(code, modifiers), None, false);
        assert_eq!(
            app.hover_position,
            Some((35, 1)),
            "key={code:?}, filter={filter_editing}"
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            app.hover_position,
            Some((35, 1)),
            "key={code:?}, filter={filter_editing}"
        );
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
fn refresh_requests_preserve_duplicate_position_and_blocked_retry_order() {
    let mut requests = RefreshRequests::default();
    for id in [1, 2, 3, 1] {
        requests.request(id);
    }
    assert_eq!(
        requests.order.iter().copied().collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(requests.members.len(), 3);
    requests.defer_front();
    requests.request(1);
    assert_eq!(
        requests.order.iter().copied().collect::<Vec<_>>(),
        [2, 3, 1]
    );
    assert_eq!(requests.pop_front(), Some(2));
    requests.request(2);
    assert_eq!(
        requests.order.iter().copied().collect::<Vec<_>>(),
        [3, 1, 2]
    );
    for id in [3, 1, 2] {
        assert_eq!(requests.pop_front(), Some(id));
    }
    assert!(requests.is_empty() && requests.members.is_empty());
    requests.request(3);
    assert_eq!(requests.pop_front(), Some(3));
    assert!(requests.members.is_empty());
}

#[test]
fn blocked_refresh_retries_finish_one_bounded_pass_and_rearm_on_changes() {
    const COUNT: usize = MAX_BATCHES * 4 + 3;
    let mut requests = RefreshRequests::default();
    for id in 1..=COUNT as NodeId {
        requests.request(id);
    }
    let mut dependencies = RefreshDependencies {
        model: 0,
        inserted: requests.inserted,
        anchor_ready: true,
        scopes: 0,
        batch: None,
        deferred: None,
    };
    let mut retry = RefreshRetry::default();
    let mut checked = Vec::new();
    while checked.len() < COUNT {
        let before = checked.len();
        retry.pump(dependencies, &mut requests, |id| {
            checked.push(id);
            false
        });
        assert!(checked.len() - before <= MAX_BATCHES);
    }
    let expected: Vec<_> = (1..=COUNT as NodeId).collect();
    assert_eq!(checked, expected);
    assert_eq!(requests.order.iter().copied().collect::<Vec<_>>(), expected);
    requests.request(1);
    assert_eq!(requests.inserted, dependencies.inserted);
    for _ in 0..8 {
        retry.pump(dependencies, &mut requests, |_| {
            panic!("unchanged blocked requests must stay asleep")
        });
    }

    requests.request(COUNT as NodeId + 1);
    dependencies.inserted = requests.inserted;
    checked.clear();
    while checked.len() < COUNT + 1 {
        retry.pump(dependencies, &mut requests, |id| {
            checked.push(id);
            false
        });
    }
    assert_eq!(checked, (1..=COUNT as NodeId + 1).collect::<Vec<_>>());
    retry.pump(dependencies, &mut requests, |_| panic!("pass completed"));

    // Same-size queue replacement and cancellation arrive as invalidations.
    retry.invalidate();
    checked.clear();
    retry.pump(dependencies, &mut requests, |id| {
        checked.push(id);
        id % 2 == 0
    });
    assert_eq!(checked, (1..=MAX_BATCHES as NodeId).collect::<Vec<_>>());
    assert_eq!(requests.front(), Some(MAX_BATCHES as NodeId + 1));
    assert_eq!(requests.len(), COUNT + 1 - MAX_BATCHES / 2);

    // Every asynchronous admission dependency can restart a sleeping pass.
    for change in 0..6 {
        while retry.remaining != 0 {
            retry.pump(dependencies, &mut requests, |_| false);
        }
        retry.pump(dependencies, &mut requests, |_| panic!("pass completed"));
        match change {
            0 => dependencies.model += 1,
            1 => dependencies.anchor_ready = false,
            2 => dependencies.scopes = 8,
            3 => dependencies.batch = Some((ROOT, 1)),
            4 => dependencies.deferred = Some((ROOT, 2)),
            _ => dependencies.scopes = 7,
        }
        let mut attempts = 0;
        retry.pump(dependencies, &mut requests, |_| {
            attempts += 1;
            false
        });
        assert_eq!(attempts, MAX_BATCHES);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn paused_overlap_refresh_sleeps_then_admits_new_and_unblocked_scopes() {
    std::fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::create_dir(fixture.path().join("blocked")).unwrap();
    std::fs::create_dir(fixture.path().join("fresh")).unwrap();
    let options = ScanOptions {
        threads: 1,
        ..ScanOptions::default()
    };
    let scanner = scan::spawn(fixture.path().to_owned(), options.clone());
    let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.baseline_finished || !scanner.is_finished() {
        drain(&scanner, &mut app, false);
        assert!(Instant::now() < deadline, "initial scan did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    let find = |name: &str| {
        (1..app.model.len() as NodeId)
            .find(|&id| app.model.node(id).name.as_ref() == OsStr::new(name))
            .unwrap()
    };
    let blocked = find("blocked");
    let fresh = find("fresh");
    let mut queue = DeleteQueue::new().unwrap();
    queue.toggle_pause();
    let job = queue.enqueue(
        blocked,
        Arc::clone(scanner.anchor.get().unwrap()),
        "blocked".into(),
    );
    let mut scopes = Vec::new();
    let mut generation = 0;
    let mut requests = RefreshRequests::default();
    let mut retry = RefreshRetry::default();
    requests.request(blocked);
    let pump = |retry: &mut RefreshRetry,
                requests: &mut RefreshRequests,
                app: &mut App,
                scopes: &mut Vec<ScopedScan>,
                queue: &DeleteQueue,
                generation: &mut u64| {
        let dependencies = RefreshDependencies::capture(app, &scanner, scopes, None, requests);
        let mut attempts = 0;
        retry.pump(dependencies, requests, |id| {
            attempts += 1;
            begin_refresh(
                app,
                (&scanner, &options),
                scopes,
                queue,
                None,
                id,
                generation,
            )
        });
        attempts
    };
    assert_eq!(
        pump(
            &mut retry,
            &mut requests,
            &mut app,
            &mut scopes,
            &queue,
            &mut generation
        ),
        1
    );
    assert!(scopes.is_empty() && !queue.start_next());
    for _ in 0..8 {
        assert_eq!(
            pump(
                &mut retry,
                &mut requests,
                &mut app,
                &mut scopes,
                &queue,
                &mut generation
            ),
            0
        );
    }

    // A new unrelated request must pass the older blocked request promptly.
    requests.request(fresh);
    assert_eq!(
        pump(
            &mut retry,
            &mut requests,
            &mut app,
            &mut scopes,
            &queue,
            &mut generation
        ),
        2
    );
    assert_eq!(requests.front(), Some(blocked));
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].scope, fresh);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !scopes.is_empty() {
        for scope in &mut scopes {
            drain_scope(scope, &mut app, false, Instant::now() + WORK_BUDGET);
        }
        retire_scopes_with(&mut scopes, false, |ready| {
            drop(ready);
            Ok(())
        });
        assert!(
            Instant::now() < deadline,
            "unrelated refresh did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        pump(
            &mut retry,
            &mut requests,
            &mut app,
            &mut scopes,
            &queue,
            &mut generation
        ),
        1
    );
    assert_eq!(
        pump(
            &mut retry,
            &mut requests,
            &mut app,
            &mut scopes,
            &queue,
            &mut generation
        ),
        0
    );

    // The input loop invalidates retry state when a paused job is removed.
    assert!(queue.remove_pending(job.id).is_some());
    retry.invalidate();
    assert_eq!(
        pump(
            &mut retry,
            &mut requests,
            &mut app,
            &mut scopes,
            &queue,
            &mut generation
        ),
        1
    );
    assert!(requests.is_empty());
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].scope, blocked);
    assert!(fixture.path().join("blocked").is_dir());
}

#[cfg(target_os = "linux")]
#[test]
fn manual_refresh_after_cancelled_quit_restores_survivor_reconciliation() {
    std::fs::create_dir_all(".tmp").unwrap();
    for key in ['r', 'R'] {
        for blocked in [false, true] {
            let fixture = tempfile::tempdir_in(".tmp").unwrap();
            for name in ["manual", "survivors"] {
                std::fs::create_dir(fixture.path().join(name)).unwrap();
            }
            std::fs::write(fixture.path().join("survivors/removed"), b"old").unwrap();
            let options = ScanOptions {
                threads: 1,
                ..ScanOptions::default()
            };
            let scanner = scan::spawn(fixture.path().to_owned(), options.clone());
            let mut app = App::new(fixture.path().to_owned(), SizeMode::Allocated).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !app.baseline_finished || !scanner.is_finished() {
                drain(&scanner, &mut app, false);
                assert!(Instant::now() < deadline, "initial scan did not finish");
                std::thread::sleep(Duration::from_millis(1));
            }
            app.started = true;
            let find = |name: &str| {
                app.model
                    .children(ROOT)
                    .find(|&id| app.model.node(id).name.as_ref() == OsStr::new(name))
                    .unwrap()
            };
            let manual = find("manual");
            let survivors = find("survivors");
            let removed = app.model.children(survivors).next().unwrap();
            let mut queue = DeleteQueue::new().unwrap();
            queue.toggle_pause();
            let job = queue.enqueue(
                manual,
                Arc::clone(scanner.anchor.get().unwrap()),
                "manual".into(),
            );
            let mut scopes = Vec::new();
            let mut requests = RefreshRequests::default();
            let mut retired = im::Vector::new();
            requests.request(survivors);
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                    None,
                    true,
                ),
                Action::QuitWarning
            ));
            // Follow the guarded-quit event-loop branch, including its clear.
            let mut scans_stopped = true;
            stop_scanning(&scanner, &scopes);
            clear_refresh_requests_with(&mut requests, &mut retired, |_| Ok(()));
            assert!(scanner.cancel.load(Ordering::Acquire));
            assert!(requests.is_empty());
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                    None,
                    true,
                ),
                Action::Continue
            ));
            assert!(app.modal.is_none());
            let cancelled = |removed_entries| delete::DeleteResult {
                relative: "survivors".into(),
                target_removed: false,
                outcome: Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
                errors: Vec::new(),
                progress: delete::DeleteProgress {
                    removed_entries,
                    ..Default::default()
                },
                parent_metadata: None,
                metadata_error: None,
            };
            // Dismissing q alone must keep automatic reconciliation stopped.
            finish_deletion(
                &mut app,
                survivors,
                cancelled(0),
                &mut requests,
                !scans_stopped,
            );
            assert!(requests.is_empty());
            app.directory = if key == 'R' { manual } else { ROOT };
            if key == 'r' {
                // Draining the scanner alone does not publish its ordered
                // rows. Entry-targeted input requires a visible selection.
                let deadline = Instant::now() + Duration::from_secs(5);
                while !app.view.positions.contains_key(&manual) {
                    app.pump_order();
                    assert!(Instant::now() < deadline, "manual row was not ordered");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            app.selected = (key == 'r').then_some(manual);
            let Action::Refresh(id) = app.key(
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                None,
                true,
            ) else {
                panic!("manual refresh was not accepted");
            };
            assert_eq!(id, manual);
            request_manual_refresh(&mut requests, &mut scans_stopped, id);
            assert!(!scans_stopped);
            let mut generation = 0;
            assert!(!begin_refresh(
                &mut app,
                (&scanner, &options),
                &mut scopes,
                &queue,
                None,
                manual,
                &mut generation
            ));
            assert!(scopes.is_empty());
            let settle = |app: &mut App,
                          requests: &mut RefreshRequests,
                          scopes: &mut Vec<ScopedScan>,
                          queue: &DeleteQueue,
                          generation: &mut u64| {
                let mut retry = RefreshRetry::default();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !requests.is_empty()
                    || !scopes.is_empty()
                    || app.model.has_pending_removals()
                {
                    let dependencies =
                        RefreshDependencies::capture(app, &scanner, scopes, None, requests);
                    retry.pump(dependencies, requests, |id| {
                        begin_refresh(
                            app,
                            (&scanner, &options),
                            scopes,
                            queue,
                            None,
                            id,
                            generation,
                        )
                    });
                    for scope in scopes.iter_mut() {
                        drain_scope(scope, app, false, Instant::now() + WORK_BUDGET);
                    }
                    let changed = app.model.pump_removals();
                    app.changed(changed);
                    retire_scopes_with(scopes, false, |_| Ok(()));
                    assert!(Instant::now() < deadline, "resumed refresh did not settle");
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            if !blocked {
                queue.remove_pending(job.id).unwrap();
                settle(
                    &mut app,
                    &mut requests,
                    &mut scopes,
                    &queue,
                    &mut generation,
                );
                assert!(app.model.node(manual).complete);
            }
            // A subsequent partial cancellation must repair stale contents,
            // including when the user's manual request is still blocked.
            exclude_job(
                &mut app,
                &scanner,
                &scopes,
                survivors,
                std::path::Path::new("survivors"),
            );
            std::fs::remove_file(fixture.path().join("survivors/removed")).unwrap();
            std::fs::write(
                fixture.path().join("survivors/remaining"),
                b"surviving contents",
            )
            .unwrap();
            finish_deletion(
                &mut app,
                survivors,
                cancelled(1),
                &mut requests,
                !scans_stopped,
            );
            assert!(requests.members.contains(&survivors));
            if blocked {
                assert!(requests.members.contains(&manual));
                queue.remove_pending(job.id).unwrap();
            }
            settle(
                &mut app,
                &mut requests,
                &mut scopes,
                &queue,
                &mut generation,
            );
            assert!(!app.model.is_live(removed));
            let children: Vec<_> = app.model.children(survivors).collect();
            assert_eq!(children.len(), 1);
            assert_eq!(
                app.model.node(children[0]).name.as_ref(),
                OsStr::new("remaining")
            );
            assert!(app.model.node(survivors).complete);
            assert_eq!(app.model.node(survivors).items, 1);
            assert!(requests.is_empty() && !queue.is_busy());
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn failed_directory_refresh_backlog_preserves_later_confirmed_deletion() {
    const COUNT: NodeId = 8192;
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(fixture.path().join("later"), b"confirmed later job").unwrap();
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
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
            entries: (1..=COUNT)
                .map(|id| entry(id, &format!("failed-{id}"), NodeKind::Directory, 1))
                .chain(std::iter::once(entry(
                    COUNT + 1,
                    "later",
                    NodeKind::File,
                    1,
                )))
                .collect(),
        },
    );
    let failure = |id| delete::DeleteResult {
        relative: PathBuf::from(format!("failed-{id}")),
        target_removed: false,
        outcome: Err(io::Error::new(io::ErrorKind::PermissionDenied, "retained")),
        errors: Vec::new(),
        progress: Default::default(),
        parent_metadata: None,
        metadata_error: None,
    };
    let mut requests = RefreshRequests::default();
    // This is the event-loop completion path while later jobs keep refresh
    // admission deferred. Every failed directory must retain one request.
    for id in 1..=COUNT {
        finish_deletion(&mut app, id, failure(id), &mut requests, true);
    }
    assert_eq!(requests.len(), COUNT as usize);
    assert_eq!(requests.members.len(), COUNT as usize);
    assert_eq!(requests.front(), Some(1));
    assert_eq!(app.delete_notices.len(), COUNT as usize);

    let mut queue = DeleteQueue::new().unwrap();
    let target = delete::capture(&anchor, std::path::Path::new("later")).unwrap();
    queue.enqueue_confirmed(COUNT + 1, anchor, "later".into(), target);
    assert!(queue.start_next());
    let deadline = Instant::now() + Duration::from_secs(5);
    let completion = loop {
        if let Some(completion) = queue.poll() {
            break completion;
        }
        assert!(
            Instant::now() < deadline,
            "later confirmed job did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(completion.result.outcome.is_ok());
    finish_deletion(
        &mut app,
        completion.entry.node,
        completion.result,
        &mut requests,
        true,
    );
    assert!(!fixture.path().join("later").exists());
    assert!(!app.model.is_live(COUNT + 1));
    assert!(!queue.is_busy());
    assert_eq!(requests.len(), COUNT as usize);

    assert_eq!(requests.pop_front(), Some(1));
    finish_deletion(&mut app, 1, failure(1), &mut requests, false);
    assert_eq!(requests.len(), COUNT as usize - 1);
    assert!(!requests.members.contains(&1));
    finish_deletion(&mut app, 1, failure(1), &mut requests, true);
    assert_eq!(requests.len(), COUNT as usize);
    assert_eq!(requests.order.back(), Some(&1));
}

#[test]
fn guarded_quit_clear_retains_rejected_and_repeated_refresh_backlogs() {
    let mut app = browser_fixture();
    let mut requests = RefreshRequests::default();
    for id in 1..=16_384 {
        requests.request(id);
    }
    let mut retired = im::Vector::new();
    // Occupy the same bounded ownership handoff used by Retirer so clear
    // must retain its packet instead of reclaiming either collection here.
    let (sender, receiver) = bounded::<im::Vector<RefreshRequests>>(1);
    assert!(sender.send(im::Vector::new()).is_ok());
    let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(matches!(app.key(quit, None, true), Action::QuitWarning));
    clear_refresh_requests_with(&mut requests, &mut retired, |packet| {
        sender.try_send(packet).map_err(|error| error.into_inner())
    });
    assert!(requests.is_empty() && requests.members.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].len(), 16_384);
    assert_eq!(retired[0].members.len(), 16_384);

    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, true);
    requests.request(1);
    requests.request(20_000);
    assert!(matches!(app.key(quit, None, true), Action::QuitWarning));
    clear_refresh_requests_with(&mut requests, &mut retired, |packet| {
        sender.try_send(packet).map_err(|error| error.into_inner())
    });
    assert!(requests.is_empty() && requests.members.is_empty());
    assert_eq!(retired.len(), 2);
    assert_eq!(retired[0].front(), Some(1));
    assert_eq!(
        retired[1].order.iter().copied().collect::<Vec<_>>(),
        [1, 20_000]
    );
    receiver.recv().unwrap();
    retire_refresh_requests_with(&mut retired, |packet| {
        sender.try_send(packet).map_err(|error| error.into_inner())
    });
    assert!(retired.is_empty());
    let accepted = receiver.recv().unwrap();
    assert_eq!(accepted.len(), 2);
    assert_eq!(accepted[0].len(), 16_384);
    assert_eq!(accepted[0].members.len(), 16_384);
    assert_eq!(accepted[1].members.len(), 2);

    drop(receiver);
    requests.request(30_000);
    clear_refresh_requests_with(&mut requests, &mut retired, |packet| {
        sender.try_send(packet).map_err(|error| error.into_inner())
    });
    assert!(requests.is_empty() && requests.members.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].front(), Some(30_000));
    assert!(retired[0].members.contains(&30_000));
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
fn control_click_deselects_without_adding_and_wins_over_shift() {
    for deferred in [false, true] {
        for marked in [false, true] {
            for shift in [false, true] {
                let mut app = browser_fixture();
                app.directory = 1;
                app.selected = Some(3);
                app.reset_order(false);
                app.visual.enter(1, Some(3));
                app.visual.toggle(5);
                if marked {
                    app.visual.toggle(4);
                }
                app.visual.reanchor(3);
                if deferred {
                    app.visual
                        .begin_range(Arc::clone(&app.view), 3, RangeMode::Add);
                }
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                app.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 35,
                    row: 2,
                    modifiers: KeyModifiers::CONTROL
                        | if shift {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                });
                app.mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 35,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                });
                finish_visual(&mut app);
                assert_eq!(
                    app.visual.snapshot(),
                    [3u32, 5].into_iter().collect::<im::HashSet<NodeId>>(),
                    "deferred={deferred}, marked={marked}, shift={shift}"
                );
                assert_eq!(app.selected, Some(4));
                assert!(app.mouse_press_view.is_none() && !app.has_held_mouse());
            }
        }
    }
}

#[test]
fn visual_drag_modes_survive_separate_event_pumps_and_reversal() {
    for deferred in [false, true] {
        for pump_press in [false, true] {
            for subtract in [false, true] {
                let mut app = browser_fixture();
                app.directory = 1;
                app.selected = Some(3);
                app.reset_order(false);
                app.visual.enter(1, Some(3));
                app.visual.toggle(5);
                app.visual.reanchor(3);
                if deferred {
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
                    1,
                    if subtract {
                        KeyModifiers::CONTROL
                    } else {
                        KeyModifiers::NONE
                    },
                ));
                if pump_press {
                    app.pump_visual();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    assert!(app.mouse_press_view.is_some());
                }
                // Modifiers after the press must not change its operation.
                let later = if subtract {
                    KeyModifiers::NONE
                } else {
                    KeyModifiers::CONTROL
                };
                app.mouse(event(MouseEventKind::Drag(MouseButton::Left), 3, later));
                app.pump_visual();
                app.pump_visual();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.visual.len(), if subtract { 0 } else { 3 });
                assert!(app.mouse_dragging && app.drag_started);
                app.mouse(event(MouseEventKind::Drag(MouseButton::Left), 2, later));
                app.pump_visual();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.visual.contains(3), !subtract);
                assert_eq!(app.visual.contains(4), !subtract);
                assert!(
                    app.visual.contains(5),
                    "reversal restores baseline membership"
                );
                app.mouse(event(MouseEventKind::Up(MouseButton::Left), 2, later));
                finish_visual(&mut app);
                assert_eq!(app.visual.len(), if subtract { 1 } else { 3 });
                assert!(app.mouse_press_view.is_none() && app.drag_scroll.is_none());
            }
        }
    }
}

#[test]
fn control_release_movement_and_deleted_endpoints_keep_the_subtractive_range() {
    for deferred in [false, true] {
        for release_only in [false, true] {
            for removed in [None, Some(3), Some(5)] {
                let mut app = browser_fixture();
                app.directory = 1;
                app.selected = Some(3);
                app.reset_order(false);
                app.visual.enter(1, Some(3));
                app.visual.toggle(5);
                app.visual.reanchor(3);
                if deferred {
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
                    1,
                    KeyModifiers::CONTROL,
                ));
                if release_only {
                    app.mouse(event(
                        MouseEventKind::Up(MouseButton::Left),
                        3,
                        KeyModifiers::NONE,
                    ));
                } else {
                    app.mouse(event(
                        MouseEventKind::Drag(MouseButton::Left),
                        3,
                        KeyModifiers::NONE,
                    ));
                }
                if let Some(id) = removed {
                    app.removed(id);
                }
                app.pump_visual();
                app.pump_visual();
                assert!(
                    app.visual.is_empty(),
                    "deferred={deferred}, release_only={release_only}, removed={removed:?}"
                );
                if !release_only {
                    app.mouse(event(
                        MouseEventKind::Drag(MouseButton::Left),
                        2,
                        KeyModifiers::NONE,
                    ));
                    app.mouse(event(
                        MouseEventKind::Up(MouseButton::Left),
                        2,
                        KeyModifiers::NONE,
                    ));
                }
                finish_visual(&mut app);
                assert!(!app.visual.contains(3) && !app.visual.contains(4));
                assert_eq!(app.visual.contains(5), !release_only && removed != Some(5));
                assert!(app.mouse_press_view.is_none() && app.drag_scroll.is_none());
            }
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
                            app.enqueue_visual(hover_intent(&app, anchor));
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
                    positions: ids
                        .iter()
                        .enumerate()
                        .map(|(i, &id)| (id, u32::try_from(i).unwrap()))
                        .collect(),
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
                app.enqueue_visual(hover_intent(&app, 4));
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
fn normal_directory_click_navigates_on_release_and_visual_click_selects() {
    for (visual, shift) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut app = browser_fixture();
        app.selected = Some(2);
        if visual {
            app.visual.enter(ROOT, None);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row: 1,
            modifiers: if shift {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            },
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(app.directory, ROOT);
        assert_eq!(app.selected, Some(1));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        finish_visual(&mut app);
        if visual || shift {
            assert_eq!(app.directory, ROOT);
            assert!(app.visual.active() && app.visual.contains(1));
        } else {
            assert_eq!(app.directory, 1);
            assert!(!app.visual.active() && app.visual.is_empty());
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.painted_directory, 1);
            assert_eq!(app.painted_rows, vec![3, 4, 5]);
            assert!(app.hover_position.is_none());
            // A duplicate release cannot activate an entry on the new page.
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
            assert_eq!(app.directory, 1);
        }
        assert!(app.mouse_press_view.is_none() && app.normal_click.is_none());
    }
}

#[test]
fn normal_directory_click_uses_captured_identity_across_live_resorts() {
    for repaint in [false, true] {
        let mut app = browser_fixture();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let original = Arc::clone(&app.painted_view);
        let newer = Arc::new(CachedSnapshot {
            generation: original.generation,
            child_epoch: original.child_epoch,
            ids: vec![2, 1],
            positions: HashMap::from([(2, 0), (1, 1)]),
        });
        app.view = Arc::clone(&newer);
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(app.normal_click, Some(1));
        // Publish through the real worker; directly overwriting app.view
        // would bypass sync_order's held-capture admission guard.
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(6, "new-largest", NodeKind::File, 10_000)],
            },
        );
        wait_cache(&mut app, ROOT, &[6, 1, 2]);
        app.sync_order(true);
        if repaint {
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.painted_rows, vec![1, 2]);
            assert!(Arc::ptr_eq(&app.painted_view, &original));
        }
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert_eq!(app.directory, 1);
        assert!(app.mouse_press_view.is_none() && app.normal_click.is_none());
    }
}

#[test]
fn normal_directory_click_rejects_drags_stale_targets_and_interrupted_presses() {
    for interruption in 0..18 {
        let mut app = browser_fixture();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 35, 1));
        let mut release = (35, 1);
        match interruption {
            0..=2 => {
                let (column, row) = [(35, 2), (35, 1), (90, 1)][interruption];
                app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), column, row));
                app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 35, 1));
            }
            3..=5 => release = [(35, 2), (1, 0), (90, 1)][interruption - 3],
            6 => {
                app.removed(1);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
            }
            7 => {
                // A refreshed generation may arrive before its first repaint.
                let changed = app.model.begin_rescan(ROOT, 1);
                app.changed(changed);
                app.sync_order(true);
            }
            8 | 9 => {
                app.key(
                    KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
                    None,
                    false,
                );
                if interruption == 9 {
                    app.key(
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        None,
                        false,
                    );
                    assert!(app.mouse_press_view.is_some());
                }
            }
            10 => {
                app.key(
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    None,
                    false,
                );
                app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None, false);
            }
            11 => {
                app.key(
                    KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(app.mouse_press_view.is_some());
            }
            12 => {
                app.key(
                    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    None,
                    false,
                );
            }
            13 => {
                app.mouse(mouse(MouseEventKind::Moved, 35, 2));
                app.mouse(mouse(MouseEventKind::Moved, 35, 1));
            }
            14 => {
                app.key(
                    KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
                    None,
                    false,
                );
                app.key(
                    KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
                    None,
                    false,
                );
            }
            15 => {
                terminal.backend_mut().resize(40, 12);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                release = (90, 1);
            }
            16 => {
                // A release without a preceding admitted press is inert.
                app.finish_mouse_for_input();
            }
            _ => {
                // A new rejected press cannot reuse an older click target.
                app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 90, 1));
            }
        }
        app.mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            release.0,
            release.1,
        ));
        assert_eq!(app.directory, ROOT, "interruption={interruption}");
        assert!(app.mouse_press_view.is_none() && app.normal_click.is_none());
        assert!(!app.visual.active() && app.visual.is_empty());
    }
}

#[test]
fn normal_file_click_stays_in_place_and_release_modifiers_do_not_change_mode() {
    for row in [1, 2] {
        let mut app = browser_fixture();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row,
            modifiers: KeyModifiers::NONE,
        });
        // Ignored keys preserve the admitted gesture.
        app.key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            None,
            false,
        );
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 35,
            row,
            modifiers: KeyModifiers::SHIFT,
        });
        assert!(!app.visual.active() && app.visual.is_empty());
        if row == 1 {
            assert_eq!(app.directory, 1);
        } else {
            assert_eq!((app.directory, app.selected), (ROOT, Some(2)));
            assert_eq!(app.hover_position, Some((35, 2)));
        }
    }
}

#[test]
fn normal_presses_preserve_painted_rows_through_release_and_live_publication() {
    for row in [1, 10] {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(11);
        app.offset = 10;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let original = Arc::clone(&app.painted_view);
        let ids: Vec<_> = original.ids.iter().rev().copied().collect();
        app.view = Arc::new(CachedSnapshot {
            generation: original.generation,
            child_epoch: original.child_epoch,
            positions: ids
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, u32::try_from(i).unwrap()))
                .collect(),
            ids,
        });
        app.offset = 0;
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        let selected = Some(10 + u32::from(row));
        assert_eq!(app.selected, selected);
        for resort in [false, true] {
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: if resort { 41 } else { ROOT },
                    entries: vec![if resort {
                        entry(42, "large-child", NodeKind::File, 10_000)
                    } else {
                        entry(41, "new-last", NodeKind::Directory, 0)
                    }],
                },
            );
            let ids: Vec<_> = if resort {
                std::iter::once(41).chain(1..=40).collect()
            } else {
                (1..=41).collect()
            };
            wait_cache(&mut app, ROOT, &ids);
            app.sync_order(true);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(Arc::ptr_eq(&app.painted_view, &original));
            assert_eq!(app.painted_offset, 10);
            assert_eq!(app.selected, selected);
            assert_eq!(app.selection_screen_row(), Some(usize::from(row - 1)));
        }
        terminal
            .backend_mut()
            .resize(100, if row == 1 { 7 } else { 13 });
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_offset, 10);
        assert_eq!(app.selection_screen_row(), Some(usize::from(row - 1)));
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
        assert_eq!(app.hover_position, Some((35, row)));
        app.sync_order(true);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!Arc::ptr_eq(&app.painted_view, &original));
        assert_eq!(app.painted_offset, 11);
        assert_eq!(app.selection_screen_row(), Some(usize::from(row - 1)));
        for resort in [false, true] {
            apply(
                &mut app,
                ScanEvent::Entries {
                    parent: if resort { 43 } else { ROOT },
                    entries: vec![if resort {
                        entry(44, "larger-child", NodeKind::File, 20_000)
                    } else {
                        entry(43, "another-last", NodeKind::Directory, 0)
                    }],
                },
            );
            let ids: Vec<_> = if resort {
                [43, 41].into_iter().chain(1..=40).collect()
            } else {
                std::iter::once(41).chain(1..=40).chain([43]).collect()
            };
            wait_cache(&mut app, ROOT, &ids);
            app.sync_order(true);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.painted_offset, 11 + usize::from(resort));
            assert_eq!(app.selected, selected);
            assert_eq!(app.selection_screen_row(), Some(usize::from(row - 1)));
            assert_eq!(app.hover_position, Some((35, row)));
        }
        terminal.backend_mut().resize(100, 11);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_offset, 12);
        assert_eq!(app.selection_screen_row(), Some(usize::from(row - 1)));
        assert!(!app.visual.active() && app.visual.is_empty());
        assert!(app.visual_intents.is_empty() && app.drag_scroll.is_none());
    }
}

#[test]
fn normal_captured_motion_and_release_never_create_visual_gestures() {
    for directory in [ROOT, 1] {
        for (column, row) in [(35, 2), (1, 0), (90, 2), (35, 20), (35, 1)] {
            let mut app = browser_fixture();
            app.directory = directory;
            app.reset_order(false);
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let rows = app.painted_rows.clone();
            let mouse = |kind, column, row, modifiers| MouseEvent {
                kind,
                column,
                row,
                modifiers,
            };
            app.mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                35,
                1,
                KeyModifiers::NONE,
            ));
            for kind in ignored_mouse_kinds() {
                app.mouse(mouse(kind, column, row, KeyModifiers::SHIFT));
            }
            app.mouse(mouse(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                row,
                KeyModifiers::SHIFT,
            ));
            assert!(!app.back_hovered);
            app.mouse(mouse(MouseEventKind::Moved, 35, 2, KeyModifiers::NONE));
            assert!(!app.pump_mouse_scroll(Instant::now() + Duration::from_secs(1)));
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.painted_rows, rows);
            assert_eq!(app.selected, rows.first().copied());
            assert_eq!(app.directory, directory);
            assert!(!app.visual.active() && app.visual.is_empty());
            app.mouse(mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row,
                KeyModifiers::SHIFT,
            ));
            assert_eq!(app.selected, rows.first().copied());
            assert_eq!(app.directory, directory);
            assert_eq!(
                app.hover_position,
                ((column, row) == (35, 1)).then_some((35, 1))
            );
            assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
            assert!(app.mouse_baseline.is_none() && app.drag_anchor.is_none());
            assert!(!app.mouse_dragging && !app.drag_started && app.drag_scroll.is_none());
            assert!(!app.visual.active() && app.visual.is_empty());
            app.mouse(mouse(MouseEventKind::Moved, 35, 2, KeyModifiers::NONE));
            assert_eq!(app.selected, rows.get(1).copied());
        }
    }
}

#[test]
fn normal_keyboard_navigation_ends_capture_without_selecting_a_range() {
    for (code, modifiers, selected) in [
        (KeyCode::Up, KeyModifiers::NONE, 14),
        (KeyCode::Down, KeyModifiers::NONE, 16),
        (KeyCode::Char('k'), KeyModifiers::NONE, 14),
        (KeyCode::Char('j'), KeyModifiers::NONE, 16),
        (KeyCode::Home, KeyModifiers::NONE, 1),
        (KeyCode::End, KeyModifiers::NONE, 40),
        (KeyCode::PageUp, KeyModifiers::NONE, 5),
        (KeyCode::PageDown, KeyModifiers::NONE, 25),
        (KeyCode::Up, KeyModifiers::SHIFT, 5),
        (KeyCode::Down, KeyModifiers::SHIFT, 25),
        (KeyCode::Left, KeyModifiers::NONE, 15),
        (KeyCode::Backspace, KeyModifiers::NONE, 15),
        (KeyCode::Char('h'), KeyModifiers::NONE, 15),
        (KeyCode::Char('<'), KeyModifiers::NONE, 15),
        (KeyCode::Right, KeyModifiers::NONE, 15),
        (KeyCode::Char('l'), KeyModifiers::NONE, 15),
        (KeyCode::Enter, KeyModifiers::NONE, 15),
    ] {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(11);
        app.offset = 10;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        app.key(KeyEvent::new(code, modifiers), None, false);
        assert_eq!(app.selected, Some(selected), "{code:?} {modifiers:?}");
        assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert_eq!(app.selected, Some(selected));
        assert!(app.hover_position.is_none());
        assert!(!app.visual.active() && app.visual.is_empty());
        assert!(app.drag_scroll.is_none());
    }
}

#[test]
fn normal_capture_survives_display_keys_and_help_closed_before_release() {
    for code in ['g', 'c', 'n', 's', 'C', 'a', 't', 'z', '?'] {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(11);
        app.offset = 10;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let rows = app.painted_rows.clone();
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        app.key(
            KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE),
            None,
            false,
        );
        if code == '?' {
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
        }
        app.sync_order(true);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(app.mouse_press_view.is_some(), "{code}");
        assert_eq!(app.painted_rows, rows, "{code}");
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert_eq!(app.hover_position, Some((35, 1)), "{code}");
        assert!(!app.visual.active() && app.visual.is_empty());
    }
}

#[test]
fn normal_capture_clears_on_modal_release_filter_navigation_and_refresh() {
    for transition in 0..9 {
        let mut app = browser_fixture();
        app.directory = 1;
        app.selected = Some(3);
        app.reset_order(false);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind| MouseEvent {
            kind,
            column: 35,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        if matches!(transition, 4 | 5) {
            app.key(
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                None,
                false,
            );
        }
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert!(app.mouse_press_view.is_some());
        match transition {
            0..=2 => {
                let code = ['?', 'i', 'D'][transition];
                app.key(
                    KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE),
                    None,
                    false,
                );
                assert!(app.modal.is_some());
            }
            3 => {
                app.key(
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    None,
                    false,
                );
            }
            4 | 5 => {
                app.key(
                    KeyEvent::new(
                        if transition == 4 {
                            KeyCode::Enter
                        } else {
                            KeyCode::Esc
                        },
                        KeyModifiers::NONE,
                    ),
                    None,
                    false,
                );
            }
            6 => {
                app.key(
                    KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                    None,
                    false,
                );
                assert_eq!(app.directory, ROOT);
            }
            7 => app.begin_model_refresh(1, 1),
            _ => {
                app.key(
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    None,
                    false,
                );
                app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
                app.key(
                    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                    None,
                    false,
                );
            }
        }
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
        assert!(app.mouse_baseline.is_none() && app.drag_anchor.is_none());
        assert!(app.hover_position.is_none());
        assert!(!app.visual.active() && app.visual.is_empty());
        assert!(app.drag_scroll.is_none());
    }
}

#[test]
fn normal_release_does_not_restore_invalid_pressed_rows_or_geometry() {
    for invalidation in 0..5 {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(11);
        app.offset = 10;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(95, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let row = if invalidation == 1 { 10 } else { 2 };
        let column = if invalidation == 0 { 90 } else { 35 };
        let mouse = |kind| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        match invalidation {
            0 => terminal.backend_mut().resize(96, 11),
            1 => terminal.backend_mut().resize(95, 7),
            2 => app.removed(12),
            3 => app.removed(11),
            _ => {
                // Simulate an epoch invalidation arriving before repaint.
                let changed = app.model.begin_rescan(ROOT, 1);
                app.changed(changed);
                app.sync_order(true);
            }
        }
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert!(app.hover_position.is_none(), "invalidation={invalidation}");
        assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
        assert!(!app.visual.active() && app.visual.is_empty());
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
                        positions: ids
                            .iter()
                            .enumerate()
                            .map(|(i, &id)| (id, u32::try_from(i).unwrap()))
                            .collect(),
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
                assert_eq!(app.selected, Some(5));
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
fn captured_drags_and_releases_take_precedence_over_the_back_control() {
    for nested in [false, true] {
        for deferred in [false, true] {
            for shift in [false, true] {
                for direct_release in [false, true] {
                    let mut app =
                        App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
                    let directory = if nested {
                        apply(
                            &mut app,
                            ScanEvent::Entries {
                                parent: ROOT,
                                entries: vec![entry(1, "folder", NodeKind::Directory, 0)],
                            },
                        );
                        1
                    } else {
                        ROOT
                    };
                    let first = if nested { 2 } else { 1 };
                    let ids: Vec<_> = (first..first + 40).collect();
                    apply(
                        &mut app,
                        ScanEvent::Entries {
                            parent: directory,
                            entries: ids
                                .iter()
                                .map(|&id| {
                                    entry(
                                        id,
                                        &format!("item-{id}"),
                                        NodeKind::File,
                                        u64::from(first + 40 - id),
                                    )
                                })
                                .collect(),
                        },
                    );
                    wait_cache(&mut app, directory, &ids);
                    app.directory = directory;
                    app.reset_order(false);
                    app.selected = Some(ids[12]);
                    app.offset = 10;
                    app.page_size = 5;
                    app.visual.enter(directory, Some(ids[12]));
                    if deferred {
                        app.visual
                            .begin_range(Arc::clone(&app.view), ids[12], RangeMode::Add);
                    }
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(80, 6)).unwrap();
                    terminal
                        .draw(|frame| app.draw(frame, false, false))
                        .unwrap();
                    assert_eq!(app.painted_rows, ids[10..15]);
                    let mouse = |kind, column, row, modifiers| MouseEvent {
                        kind,
                        column,
                        row,
                        modifiers,
                    };
                    app.mouse(mouse(
                        MouseEventKind::Down(MouseButton::Left),
                        35,
                        3,
                        if shift {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    ));
                    app.mouse(mouse(
                        MouseEventKind::Drag(MouseButton::Left),
                        35,
                        5,
                        KeyModifiers::NONE,
                    ));
                    if deferred {
                        assert_eq!(app.held_mouse().unwrap().edge, 1);
                    } else {
                        app.pump_visual();
                        assert_eq!(app.drag_scroll.unwrap().0, 1);
                    }
                    if !direct_release {
                        app.mouse(mouse(
                            MouseEventKind::Drag(MouseButton::Left),
                            1,
                            0,
                            KeyModifiers::NONE,
                        ));
                        assert!(!app.back_hovered);
                        if deferred {
                            let gesture = app.held_mouse().unwrap();
                            assert_eq!((gesture.lead, gesture.edge), (ids[10], -1));
                        } else {
                            assert_eq!(app.selected, Some(ids[10]));
                            assert_eq!(app.drag_scroll.unwrap().0, -1);
                        }
                    }
                    app.mouse(mouse(
                        MouseEventKind::Up(MouseButton::Left),
                        1,
                        0,
                        KeyModifiers::NONE,
                    ));
                    assert_eq!(app.directory, directory);
                    assert!(!app.back_hovered);
                    assert!(app.mouse_position.is_none() && app.mouse_press_view.is_none());
                    assert!(app.drag_scroll.is_none() && !app.has_held_mouse());
                    finish_visual(&mut app);
                    assert_eq!(app.selected, Some(ids[10]));
                    assert!(app.visual.contains(ids[10]) && app.visual.contains(ids[11]));
                    assert_eq!(app.visual.contains(ids[12]), !shift);
                    assert_eq!(app.visual.len(), if shift { 2 } else { 3 });
                }
            }
        }
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
    assert!(
        app.row(1, 30, 80, false, " ", DeleteState::default())
            .to_string()
            .starts_with('✓')
    );
    assert!(
        app.row(2, 30, 80, true, " ", DeleteState::default())
            .to_string()
            .starts_with('›')
    );
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(20, 5)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(rendered(&terminal).contains("Visual 2"));
    app.visual.toggle(2);
    assert_eq!(
        app.row(2, 30, 80, true, " ", DeleteState::default())
            .style
            .bg,
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
fn removed_nested_directory_repairs_to_surviving_parent_once() {
    let mut app = browser_fixture();
    app.directory = 5;
    app.selected = None;
    app.reset_order(false);
    let changed = app.model.remove(5);
    app.changed(changed);
    app.repair_directory();
    assert_eq!(app.directory, 1);
    assert!(app.selected.is_none_or(|id| app.model.is_live(id)));
    let navigation_until = app.navigation_until;
    app.prioritize_requested = false;
    for _ in 0..8 {
        app.repair_directory();
        assert_eq!(app.directory, 1);
        assert_eq!(app.navigation_until, navigation_until);
        assert!(!app.prioritize_requested);
    }
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
    let row = app.row(1, 22, 80, false, spinner_frame(0), DeleteState::default());
    assert!(row.to_string().contains('~'));
}

#[test]
fn removed_target_with_restoration_error_updates_totals_and_keeps_report() {
    for target in [2, 3] {
        for cancelled in [false, true] {
            let mut app = browser_fixture();
            let path = app.report_path(target);
            let removed_bytes = app.size(target);
            let total = app.size(ROOT);
            let error = if cancelled {
                io::Error::new(io::ErrorKind::Interrupted, "cancelled after unlink")
            } else {
                io::Error::other("parent permissions could not be restored")
            };
            let reconcile = app.deletion_finished(
                target,
                delete::DeleteResult {
                    relative: app
                        .model
                        .path(target)
                        .strip_prefix(&app.root)
                        .unwrap()
                        .to_owned(),
                    target_removed: true,
                    outcome: Err(error),
                    errors: vec![DeleteError {
                        path: path.clone(),
                        message: "parent permissions could not be restored".to_owned(),
                    }],
                    progress: delete::DeleteProgress {
                        examined_entries: 1,
                        removed_entries: 1,
                        removed_directories: 0,
                    },
                    parent_metadata: None,
                    metadata_error: None,
                },
            );
            assert!(!reconcile && !app.model.is_live(target));
            assert_eq!(app.size(ROOT), total - removed_bytes);
            assert_eq!(app.model.node(ROOT).error_count, 0);
            assert_eq!(app.delete_notices.len(), 1);
            assert_eq!(app.delete_notices[0].target, printable(path.as_os_str()));
            assert_eq!(app.delete_notices[0].errors[0].path, path);
            assert!(matches!(app.modal, Some(Modal::DeleteErrors { .. })));
            app.key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                None,
                false,
            );
            assert!(app.modal.is_none() && app.delete_notices.is_empty());
        }
    }
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
            assert!(app.model.last_error.is_none() && app.delete_notices.is_empty());
            assert_eq!(
                app.model.has_partial_totals(parent),
                !parent_metadata_available
            );
            assert_eq!(
                app.model.has_partial_totals(ROOT),
                !parent_metadata_available
            );
            let row = app
                .row(
                    parent,
                    app.size(ROOT),
                    80,
                    false,
                    spinner_frame(0),
                    DeleteState::default(),
                )
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
    terminal.backend_mut().resize(95, 12);
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
fn preview_layout_scales_and_keeps_mouse_hits_in_the_main_pane() {
    for width in [0, 1, 95, 96, 120, 160, u16::MAX] {
        let area = Rect::new(0, 1, width, 10);
        let (main, preview) = preview_layout(area);
        if width < 96 {
            assert_eq!(main, area);
            assert!(preview.is_none());
        } else {
            let preview = preview.unwrap();
            assert_eq!(u32::from(main.width), u32::from(width) * 4 / 7);
            assert_eq!(main.right(), preview.x);
            assert_eq!(main.width + preview.width, width);
        }
    }
    let mut app = browser_fixture();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(96, 12)).unwrap();
    for selected in [Some(1), Some(2), None] {
        app.selected = selected;
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.list_area.width, 54);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 55,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, selected);
        assert!(
            app.file_preview.selected().is_none(),
            "no scan anchor means no file I/O"
        );
    }
}

#[test]
fn file_preview_is_async_clears_old_text_and_stops_on_narrow_resize() {
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(
        fixture.path().join("root-file"),
        "preview hello\nsecond line\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("binary-file"), [0, 1, 2]).unwrap();
    let mut app = browser_fixture_at(fixture.path().to_owned());
    apply(
        &mut app,
        ScanEvent::Entries {
            parent: ROOT,
            entries: vec![entry(6, "binary-file", NodeKind::File, 3)],
        },
    );
    wait_cache(&mut app, ROOT, &[1, 2, 6]);
    app.sync_order(true);
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
    app.file_anchor = Some(Arc::new(OnceLock::from(anchor)));
    app.selected = Some(2);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(95, 12)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(app.file_preview.selected().is_none());
    terminal.backend_mut().resize(120, 12);
    for (id, expected) in [(2, "preview hello"), (6, "Binary")] {
        app.selected = Some(id);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.file_preview.selected(), Some(id));
        if id == 6 {
            assert!(!rendered(&terminal).contains("preview hello"));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.file_preview.snapshot().is_none() {
            assert!(Instant::now() < deadline, "file preview did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("Preview") && screen.contains(expected));
        if id == 6 {
            assert!(screen.contains("Size") && screen.contains("Disk"));
        }
    }
    app.selected = Some(2);
    app.queue_view.pending = 1;
    app.queue_view.paused = true;
    terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
    assert_eq!(
        app.file_preview.selected(),
        Some(2),
        "paused unrelated jobs allow previews"
    );
    for blocked in 0..3 {
        match blocked {
            0 => app.queue_view.marks.active = Some(1),
            1 => app.modal = Some(Modal::Quit { reviewable: false }),
            _ => app.modal = Some(Modal::Stopping),
        }
        terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
        assert!(app.file_preview.selected().is_none() && app.file_preview.snapshot().is_none());
        app.queue_view.marks.active = None;
        app.modal = None;
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.file_preview.selected(), Some(2));
    }
    for height in [0, 1, 2] {
        terminal.backend_mut().resize(120, height);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(app.file_preview.selected().is_none() && app.file_preview.snapshot().is_none());
        terminal.backend_mut().resize(120, 3);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.file_preview.selected(), Some(2));
    }
    terminal.backend_mut().resize(95, 12);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(app.file_preview.selected().is_none() && app.file_preview.snapshot().is_none());
    assert_eq!(app.list_area.width, 95);
    terminal.backend_mut().resize(160, 12);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.file_preview.selected(), Some(2));
    assert_eq!(app.list_area.width, 91);
    app.selected = Some(1);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(app.file_preview.selected().is_none());
    assert!(rendered(&terminal).contains("Contents"));
}

#[cfg(unix)]
#[test]
fn symlink_preview_shows_only_wrapped_destination_and_obeys_request_gates() {
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(fixture.path().join("root-file"), "PRIVATE-FILE-CONTENTS").unwrap();
    std::fs::create_dir(fixture.path().join("folder")).unwrap();
    std::fs::write(
        fixture.path().join("folder/PRIVATE-DIRECTORY-CHILD"),
        "private",
    )
    .unwrap();
    let long_target = format!("../{}/VISIBLE-TARGET-END", "long-component/".repeat(7));
    let links = [
        (6, "file-link", "root-file"),
        (7, "directory-link", "folder"),
        (8, "dangling-link", "missing-target"),
        (9, "long-link", long_target.as_str()),
    ];
    for (_, name, target) in links {
        std::os::unix::fs::symlink(target, fixture.path().join(name)).unwrap();
    }
    std::fs::write(
        fixture.path().join("replaced-link"),
        "PRIVATE-REPLACEMENT-CONTENTS",
    )
    .unwrap();
    let mut app = browser_fixture_at(fixture.path().to_owned());
    apply(
        &mut app,
        ScanEvent::Entries {
            parent: ROOT,
            entries: links
                .iter()
                .map(|&(id, name, _)| entry(id, name, NodeKind::Symlink, 1))
                .chain(std::iter::once(entry(
                    10,
                    "replaced-link",
                    NodeKind::Symlink,
                    1,
                )))
                .collect(),
        },
    );
    // Newly discovered equal-size links use basename order before they
    // have a prior displayed rank.
    wait_cache(&mut app, ROOT, &[1, 2, 8, 7, 6, 9, 10]);
    app.sync_order(true);
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
    app.file_anchor = Some(Arc::new(OnceLock::from(Arc::clone(&anchor))));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
    for (id, _, target) in links {
        app.selected = Some(id);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.file_preview.selected(), Some(id));
        assert!(
            app.preview.directory.is_none(),
            "links must not request directory listings"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.file_preview.snapshot().is_none() {
            assert!(Instant::now() < deadline, "symlink preview did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        let contents = app.file_preview.snapshot().unwrap();
        assert_eq!(contents.kind, "Symlink");
        assert_eq!(contents.text.as_deref(), Some(target));
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains("Links to"));
        assert!(screen.contains(if id == 9 {
            "VISIBLE-TARGET-END"
        } else {
            target
        }));
        for forbidden in [
            "PRIVATE-FILE-CONTENTS",
            "PRIVATE-DIRECTORY-CHILD",
            "remove-me",
        ] {
            assert!(
                !screen.contains(forbidden),
                "link preview exposed {forbidden}"
            );
        }
    }
    terminal.backend_mut().resize(120, 4);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let clipped = rendered(&terminal);
    assert!(clipped.contains("Links to") && clipped.contains('…'));
    assert!(!clipped.contains("VISIBLE-TARGET-END"));
    terminal.backend_mut().resize(120, 12);
    app.selected = Some(10);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.file_preview.snapshot().is_none() {
        assert!(
            Instant::now() < deadline,
            "replaced symlink preview did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(!rendered(&terminal).contains("PRIVATE-REPLACEMENT-CONTENTS"));
    assert!(app.file_preview.snapshot().unwrap().text.is_none());

    app.selected = Some(9);
    terminal.backend_mut().resize(95, 12);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(app.file_preview.selected().is_none() && app.file_preview.snapshot().is_none());
    terminal.backend_mut().resize(120, 12);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.file_preview.selected(), Some(9));
    let mut queue = DeleteQueue::new().unwrap();
    queue.toggle_pause();
    queue.enqueue(9, anchor, "long-link".into());
    app.sync_queue(&mut queue, Rect::new(0, 0, 120, 12));
    terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
    assert!(app.file_preview.selected().is_none() && app.file_preview.snapshot().is_none());
    assert!(rendered(&terminal).contains("Marked for deletion"));
    assert!(
        fixture.path().join("long-link").is_symlink(),
        "a paused job must not run"
    );
}

#[cfg(unix)]
#[test]
fn refreshed_directory_replacement_switches_to_file_or_link_preview() {
    for kind in [NodeKind::File, NodeKind::Symlink] {
        let fixture = tempfile::tempdir_in(".tmp").unwrap();
        let path = fixture.path().join("folder");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("remove-me"), "old child").unwrap();
        let mut app = browser_fixture_at(fixture.path().to_owned());
        let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
        app.file_anchor = Some(Arc::new(OnceLock::from(anchor)));
        app.selected = Some(1);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(rendered(&terminal).contains("remove-me"));
        assert_eq!(app.preview.directory, Some(1));
        assert!(app.file_preview.selected().is_none());

        std::fs::remove_dir_all(&path).unwrap();
        let expected = if kind == NodeKind::Symlink {
            "stored-target-path"
        } else {
            "replacement regular text"
        };
        if kind == NodeKind::Symlink {
            std::os::unix::fs::symlink(expected, &path).unwrap();
        } else {
            std::fs::write(&path, expected).unwrap();
        }
        // A scoped directory refresh retains its ID, then replacement
        // metadata may turn that directory into a file or symlink.
        app.begin_model_refresh(1, 1);
        let changed = app.model.apply_scoped(
            1,
            1,
            ScanEvent::RootInfo {
                kind,
                sizes: Default::default(),
                error: None,
                file_identity: None,
            },
        );
        app.changed(changed);
        assert_eq!(app.model.node(1).kind, kind);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.selected, Some(1));
        assert!(app.file_preview.matches(1, kind));
        assert!(app.preview.directory.is_none());
        assert!(
            !rendered(&terminal).contains("remove-me"),
            "old directory listing remained visible on the first frame"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.file_preview.snapshot().is_none() {
            assert!(
                Instant::now() < deadline,
                "replacement preview did not finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let contents = app.file_preview.snapshot().unwrap();
        assert_eq!(contents.text.as_deref(), Some(expected));
        assert_eq!(
            contents.kind,
            if kind == NodeKind::Symlink {
                "Symlink"
            } else {
                "Text"
            }
        );
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        assert!(screen.contains(expected));
        assert_eq!(screen.contains("Links to"), kind == NodeKind::Symlink);
    }
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
fn deletion_rows_mark_confirmed_targets_and_descendants_in_both_panes() {
    let mut app = browser_fixture();
    let anchor = Arc::new(RootAnchor::open(std::path::Path::new(".")).unwrap());
    let mut queue = DeleteQueue::new().unwrap();
    queue.toggle_pause();
    let job = queue.enqueue(1, anchor, "folder".into()).id;
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    assert!(!app.deleting(ROOT) && !app.deleting(2));
    assert!(app.deleting(1) && app.deleting(3) && app.deleting(5));
    for (id, parent) in [(1, ROOT), (3, 1), (5, 1)] {
        let row = app.row(
            id,
            30,
            80,
            false,
            spinner_frame(0),
            app.deletion_state(parent),
        );
        assert!(row.to_string().contains("× "));
        assert!(
            row.spans
                .iter()
                .any(|span| span.content == "× " && span.style.fg == Some(Color::Red))
        );
        assert!(!row.to_string().contains(spinner_frame(0)));
    }
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
    terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
    assert!(rendered(&terminal).matches('×').count() >= 4);
    queue.remove_pending(job).unwrap();
    app.sync_queue(&mut queue, Rect::new(0, 0, 80, 18));
    assert!(!app.deleting(1) && !app.deleting(3) && !app.deleting(5));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(!rendered(&terminal).contains('×'));
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
fn ignored_delete_preserves_startup_top_follow_and_visual_delete_still_batches() {
    for before_rows in [true, false] {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        wait_cache(&mut app, ROOT, &[]);
        app.sync_order(true);
        if before_rows {
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                    None,
                    false
                ),
                Action::Continue
            ));
        }
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![
                    entry(1, "growing", NodeKind::Directory, 10),
                    entry(2, "initial-largest", NodeKind::Directory, 100),
                ],
            },
        );
        wait_cache(&mut app, ROOT, &[2, 1]);
        app.sync_order(true);
        assert_eq!(app.selected, Some(2));
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_rows.first(), Some(&2));
        if !before_rows {
            assert!(matches!(
                app.key(
                    KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                    None,
                    false
                ),
                Action::Continue
            ));
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
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_rows.first(), Some(&1));
        assert_eq!(app.selection_screen_row(), Some(0));
        assert!(app.cursor_mode == CursorMode::Top);
        assert!(!app.prioritize_requested && !app.focus_after_selection);
        app.key(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            None,
            false,
        );
        assert!(app.visual.active() && app.visual.contains(1));
        assert!(matches!(
            app.key(
                KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                None,
                false
            ),
            Action::BatchPrepare
        ));
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
fn mouse_wheel_scrolls_the_viewport_under_the_pointer_without_keyboard_margins() {
    let mut app = navigation_fixture(40, 41..=41);
    app.selected = Some(1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let mouse = |kind| MouseEvent {
        kind,
        row: 7,
        column: 35,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(mouse(MouseEventKind::ScrollDown));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (3, Some(10), Some(6))
    );
    app.mouse(mouse(MouseEventKind::Moved));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!((app.offset, app.selected), (3, Some(10)));
    app.mouse(mouse(MouseEventKind::ScrollUp));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (0, Some(7), Some(6))
    );
    for _ in 0..15 {
        app.mouse(mouse(MouseEventKind::ScrollDown));
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
    }
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (30, Some(37), Some(6))
    );
    app.mouse(MouseEvent {
        row: 10,
        ..mouse(MouseEventKind::ScrollDown)
    });
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (30, Some(40), Some(9))
    );
    // Keyboard input resumes its normal context-margin behavior.
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), None, false);
    assert!(app.hover_position.is_none());
}

#[test]
fn mouse_wheel_uses_painted_order_and_counts_only_eligible_rows() {
    let mut app = navigation_fixture(40, 5..=8);
    app.config.show_hidden = false;
    app.selected = Some(1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let painted = Arc::clone(&app.painted_view);
    let ids: Vec<_> = app.view.ids.iter().copied().rev().collect();
    app.view = Arc::new(CachedSnapshot {
        generation: app.view.generation,
        child_epoch: app.view.child_epoch,
        positions: ids
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, u32::try_from(index).unwrap()))
            .collect(),
        ids,
    });
    app.mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        row: 2,
        column: 35,
        modifiers: KeyModifiers::NONE,
    });
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(Arc::ptr_eq(&app.view, &painted));
    assert_eq!(
        (
            app.painted_rows[0],
            app.selected,
            app.selection_screen_row()
        ),
        (4, Some(9), Some(1))
    );
    app.mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        row: 2,
        column: 35,
        modifiers: KeyModifiers::NONE,
    });
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.painted_rows[0], 11);
    assert_eq!(app.selection_screen_row(), Some(1));
}

#[test]
fn deferred_visual_wheel_preserves_pointer_row_and_selected_marks() {
    let mut app = navigation_fixture(40, 41..=41);
    app.selected = Some(1);
    app.visual.enter(ROOT, Some(1));
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    app.visual
        .begin_range(Arc::clone(&app.view), 40, RangeMode::Add);
    let mouse = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        row: 7,
        column: 35,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(mouse);
    app.mouse(mouse);
    assert!(matches!(
        app.visual_intents.back(),
        Some(VisualIntent::ScrollViewport(_))
    ));
    finish_visual(&mut app);
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (6, Some(13), Some(6))
    );
    assert_eq!(app.visual.len(), 40);
}

#[test]
fn ordinary_hover_matches_immediate_input_after_pending_navigation_and_wheels() {
    for wheel in [false, true] {
        for row in [1, 10] {
            let mut outcomes = Vec::new();
            for deferred in [false, true] {
                let mut app = navigation_fixture(2048, 2049..=2049);
                app.selected = Some(1);
                app.visual.enter(ROOT, Some(1));
                app.visual
                    .begin_range(Arc::clone(&app.view), 2048, RangeMode::Add);
                if deferred {
                    app.pump_visual();
                    assert!(app.visual.is_pending());
                } else {
                    finish_visual(&mut app);
                }
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let mouse = |kind| MouseEvent {
                    kind,
                    row,
                    column: 35,
                    modifiers: KeyModifiers::NONE,
                };
                if wheel {
                    app.mouse(mouse(MouseEventKind::ScrollDown));
                } else {
                    app.key(
                        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                        None,
                        false,
                    );
                }
                app.pump_visual();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.visual.is_pending(), deferred);
                app.mouse(mouse(MouseEventKind::Moved));
                finish_visual(&mut app);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let expected_offset = if wheel { 3 } else { 0 };
                let expected_id = expected_offset as NodeId + NodeId::from(row);
                assert_eq!(
                    (app.offset, app.selected, app.selection_screen_row()),
                    (
                        expected_offset,
                        Some(expected_id),
                        Some(usize::from(row - 1))
                    )
                );
                assert_eq!(app.hover_position, Some((35, row)));
                let old_view = Arc::clone(&app.view);
                apply(
                    &mut app,
                    ScanEvent::Entries {
                        parent: ROOT,
                        entries: vec![entry(2049, "new-last", NodeKind::File, 0)],
                    },
                );
                let ids: Vec<_> = (1..=2049).collect();
                wait_cache(&mut app, ROOT, &ids);
                app.sync_order(true);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert!(!Arc::ptr_eq(&old_view, &app.view));
                assert_eq!(
                    (app.offset, app.selected, app.selection_screen_row()),
                    (
                        expected_offset,
                        Some(expected_id),
                        Some(usize::from(row - 1))
                    )
                );
                assert_eq!(app.visual.len(), 2048);
                assert!(!app.visual.contains(2049));
                outcomes.push((app.offset, app.selected, app.hover_position));
            }
            assert_eq!(outcomes[0], outcomes[1]);
        }
    }
}

#[test]
fn later_input_and_resize_do_not_reactivate_deferred_hover() {
    for later in 0..9 {
        let mut app = navigation_fixture(2048, 2049..=2049);
        app.selected = Some(1);
        app.visual.enter(ROOT, Some(1));
        app.visual
            .begin_range(Arc::clone(&app.view), 2048, RangeMode::Add);
        app.pump_visual();
        assert!(app.visual.is_pending());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::ScrollDown, 60, 10));
        app.mouse(mouse(MouseEventKind::Moved, 60, 10));
        assert_eq!(app.hover_position, Some((60, 10)));
        match later {
            0 => {
                app.key(
                    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    None,
                    false,
                );
            }
            1 => {
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
            }
            2 => app.mouse(mouse(MouseEventKind::Moved, 35, 0)),
            3 => app.mouse(mouse(MouseEventKind::Moved, 1, 0)),
            4 => app.mouse(mouse(MouseEventKind::Moved, 90, 10)),
            5 => app.modal = Some(Modal::help(true)),
            6 | 7 => {
                // An invalidated hover must not bind Enter to the old cursor.
                app.key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    None,
                    false,
                );
                if later == 6 {
                    terminal.backend_mut().resize(120, 5);
                } else {
                    terminal.backend_mut().resize(96, 11);
                }
            }
            _ => {
                app.key(
                    KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                    None,
                    false,
                );
                app.key(
                    KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                    None,
                    false,
                );
            }
        }
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(app.hover_position.is_none(), "later={later}");
        finish_visual(&mut app);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(app.hover_position.is_none(), "later={later}");
        if later == 8 {
            assert!(app.visual.is_empty() && app.visual_intents.is_empty());
        } else {
            assert_eq!(
                app.selected,
                Some(if later == 0 {
                    14
                } else if later == 6 {
                    7
                } else {
                    13
                })
            );
            assert_eq!(app.offset, if later == 0 { 7 } else { 3 });
            assert_eq!(app.visual.len(), if later == 1 { 2047 } else { 2048 });
            assert_eq!(app.visual.contains(13), later != 1);
        }
    }
}

#[test]
fn vanished_hover_rows_and_stale_epochs_do_not_toggle_the_previous_cursor() {
    for stale_epoch in [false, true] {
        let mut app = pending_folder_selection();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 35,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        app.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            None,
            false,
        );
        if stale_epoch {
            let Some(VisualIntent::Hover(hover)) = app.visual_intents.front_mut() else {
                panic!("hover must precede the dependent toggle");
            };
            hover.child_epoch = hover.child_epoch.wrapping_add(1);
        } else {
            app.removed(5);
        }
        finish_visual(&mut app);
        assert_eq!(app.selected, Some(4));
        assert!(app.visual.contains(3) && app.visual.contains(4));
        assert_eq!(app.visual.len(), 2);
        assert!(app.hover_position.is_none());
    }
}

#[test]
fn immediate_hover_keeps_the_painted_view_when_a_resort_is_not_painted() {
    let mut app = navigation_fixture(40, 41..=41);
    app.selected = Some(1);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let painted = Arc::clone(&app.painted_view);
    let ids: Vec<_> = app.view.ids.iter().copied().rev().collect();
    app.view = Arc::new(CachedSnapshot {
        generation: app.view.generation,
        child_epoch: app.view.child_epoch,
        positions: ids
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, u32::try_from(index).unwrap()))
            .collect(),
        ids,
    });
    app.mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 35,
        row: 10,
        modifiers: KeyModifiers::NONE,
    });
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert!(Arc::ptr_eq(&app.view, &painted));
    assert_eq!(
        (app.offset, app.selected, app.selection_screen_row()),
        (0, Some(10), Some(9))
    );
    assert_eq!(app.painted_rows, (1..=10).collect::<Vec<_>>());
}

#[test]
fn deferred_wheel_reversals_compose_and_clamp_each_direction() {
    use MouseEventKind::{ScrollDown, ScrollUp};
    let cases = [
        (0, vec![ScrollDown, ScrollDown, ScrollUp], 3),
        (10, vec![ScrollDown, ScrollUp], 10),
        (10, vec![ScrollUp, ScrollDown], 10),
        (0, vec![ScrollUp, ScrollDown], 3),
        (0, vec![ScrollDown, ScrollUp], 0),
        (30, vec![ScrollDown, ScrollUp], 27),
        (30, vec![ScrollUp, ScrollDown], 30),
        (10, vec![ScrollDown, ScrollDown, ScrollUp, ScrollUp], 10),
    ];
    for (offset, directions, expected) in cases {
        let mut app = navigation_fixture(40, 41..=41);
        let initial = offset as NodeId + 7;
        app.selected = Some(initial);
        app.offset = offset;
        app.visual.enter(ROOT, Some(initial));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.visual
            .begin_range(Arc::clone(&app.view), 40, RangeMode::Add);
        for kind in directions {
            app.mouse(MouseEvent {
                kind,
                row: 7,
                column: 35,
                modifiers: KeyModifiers::NONE,
            });
        }
        finish_visual(&mut app);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            (app.offset, app.selected, app.selection_screen_row()),
            (expected, Some(expected as NodeId + 7), Some(6))
        );
        assert_eq!(app.visual.len(), (41 - initial) as usize);
    }
}

#[test]
fn deferred_wheel_composes_with_interleaved_toggles_navigation_and_hover() {
    for middle in [Some(KeyCode::Enter), Some(KeyCode::Down), None] {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(7);
        app.visual.enter(ROOT, Some(7));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        app.visual
            .begin_range(Arc::clone(&app.view), 40, RangeMode::Add);
        let mouse = |kind| MouseEvent {
            kind,
            row: 7,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::ScrollDown));
        if let Some(code) = middle {
            app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
        } else {
            app.mouse(mouse(MouseEventKind::Moved));
        }
        app.mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.visual_intents.len(), 3);
        finish_visual(&mut app);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let expected = if middle == Some(KeyCode::Down) { 7 } else { 6 };
        assert_eq!(
            (app.offset, app.selected, app.selection_screen_row()),
            (expected, Some(expected as NodeId + 7), Some(6))
        );
        assert_eq!(
            app.visual.len(),
            if middle == Some(KeyCode::Enter) {
                33
            } else {
                34
            }
        );
        assert_eq!(app.visual.contains(10), middle != Some(KeyCode::Enter));
    }
}

#[test]
fn deferred_wheels_compose_across_live_appends_and_clamp_each_direction() {
    use MouseEventKind::{ScrollDown, ScrollUp};
    for (offset, first, second, expected) in [
        (0, ScrollDown, ScrollDown, 6),
        (10, ScrollDown, ScrollUp, 10),
        (10, ScrollUp, ScrollDown, 10),
        (0, ScrollUp, ScrollDown, 3),
        (0, ScrollDown, ScrollUp, 0),
        (590, ScrollDown, ScrollDown, 591),
        (590, ScrollDown, ScrollUp, 588),
        (590, ScrollUp, ScrollDown, 590),
    ] {
        let mut app = navigation_fixture(600, 601..=601);
        let initial = offset as NodeId + 7;
        let endpoint = if offset < 300 { 600 } else { 1 };
        app.selected = Some(initial);
        app.offset = offset;
        app.visual.enter(ROOT, Some(initial));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let previous = Arc::clone(&app.painted_view);
        app.visual
            .begin_range(Arc::clone(&previous), endpoint, RangeMode::Add);
        app.pump_visual();
        assert!(app.visual.is_pending());
        let mouse = |kind| MouseEvent {
            kind,
            row: 7,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(first));
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: vec![entry(601, "new-last", NodeKind::File, 0)],
            },
        );
        let ids: Vec<_> = (1..=601).collect();
        wait_cache(&mut app, ROOT, &ids);
        app.sync_order(true);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!Arc::ptr_eq(&previous, &app.painted_view));
        assert_eq!(previous.child_epoch, app.painted_view.child_epoch);
        assert_eq!(previous.generation, app.painted_view.generation);
        assert_eq!(app.offset, offset);
        assert!(app.visual.is_pending());
        app.mouse(mouse(second));
        assert_eq!(app.visual_intents.len(), 2);
        finish_visual(&mut app);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(
            (app.offset, app.selected, app.selection_screen_row()),
            (expected, Some(expected as NodeId + 7), Some(6))
        );
        let selected = initial.min(endpoint)..=initial.max(endpoint);
        for id in 1..=601 {
            assert_eq!(app.visual.contains(id), selected.contains(&id));
        }
    }
}

#[test]
fn deferred_wheels_keep_live_resorts_and_interleaved_cursor_intents() {
    for (middle, final_wheel) in [
        (Some(KeyCode::Enter), true),
        (Some(KeyCode::Down), true),
        (None, true),
        (None, false),
    ] {
        let mut app = App::new(PathBuf::from("fixture"), SizeMode::Allocated).unwrap();
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: ROOT,
                entries: (1..=600)
                    .map(|id| {
                        entry(
                            id,
                            &format!("item-{id:05}"),
                            if id == 300 {
                                NodeKind::Directory
                            } else {
                                NodeKind::File
                            },
                            u64::from(601 - id),
                        )
                    })
                    .collect(),
            },
        );
        let original: Vec<_> = (1..=600).collect();
        wait_cache(&mut app, ROOT, &original);
        app.sync_order(true);
        app.cursor_mode = CursorMode::Tracked;
        app.selected = Some(7);
        app.visual.enter(ROOT, Some(7));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let previous = Arc::clone(&app.painted_view);
        app.visual
            .begin_range(Arc::clone(&previous), 600, RangeMode::Add);
        app.pump_visual();
        assert!(app.visual.is_pending() && !app.visual.contains(300));
        let mouse = |kind| MouseEvent {
            kind,
            row: 7,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::ScrollDown));
        apply(
            &mut app,
            ScanEvent::Entries {
                parent: 300,
                entries: vec![entry(601, "large-child", NodeKind::File, 10_000)],
            },
        );
        let reordered: Vec<_> = std::iter::once(300)
            .chain((1..=600).filter(|&id| id != 300))
            .collect();
        wait_cache(&mut app, ROOT, &reordered);
        app.sync_order(true);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(!Arc::ptr_eq(&previous, &app.painted_view));
        assert_eq!(previous.child_epoch, app.painted_view.child_epoch);
        assert_eq!(previous.generation, app.painted_view.generation);
        assert_eq!(
            (app.offset, app.selected, app.selection_screen_row()),
            (1, Some(7), Some(6))
        );
        assert!(app.visual.is_pending());
        if let Some(code) = middle {
            app.key(KeyEvent::new(code, KeyModifiers::NONE), None, false);
        } else {
            app.mouse(mouse(MouseEventKind::Moved));
        }
        if final_wheel {
            app.mouse(mouse(MouseEventKind::ScrollDown));
        }
        assert_eq!(app.visual_intents.len(), if final_wheel { 3 } else { 2 });
        finish_visual(&mut app);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let expected = if middle == Some(KeyCode::Down) {
            8
        } else if final_wheel {
            7
        } else {
            4
        };
        assert_eq!(
            (app.offset, app.selected, app.selection_screen_row()),
            (expected, Some(expected as NodeId + 6), Some(6))
        );
        assert_eq!(app.view.ids, reordered);
        // Directory 300 moved outside the new endpoint interval before
        // the old range reached it; its original membership must survive.
        for id in 1..=601 {
            assert_eq!(
                app.visual.contains(id),
                (7..=600).contains(&id) && (id != 10 || middle != Some(KeyCode::Enter))
            );
        }
    }
}

#[test]
fn back_control_highlights_on_hover_and_navigates_only_on_click() {
    let mut app = browser_fixture();
    app.directory = 1;
    app.selected = Some(3);
    app.reset_order(false);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.back_area.width, 2);
    let mouse = |kind| MouseEvent {
        kind,
        row: 0,
        column: 1,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(mouse(MouseEventKind::Moved));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(app.directory, 1);
    assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Cyan);
    assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "←");
    assert_eq!(terminal.backend().buffer()[(1, 0)].symbol(), "─");
    assert_eq!(terminal.backend().buffer()[(1, 0)].bg, Color::Cyan);
    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
    assert_eq!((app.directory, app.selected), (ROOT, Some(1)));
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::DarkGray);
    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
    assert_eq!(app.directory, ROOT);
}

#[test]
fn compact_header_retains_trailing_path_help_and_integrated_queue_key() {
    let path = format!("/{}visible-tail", "long-parent-directory/".repeat(20));
    let mut app = App::new(path.into(), SizeMode::Allocated).unwrap();
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(60, 5)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, false, false))
        .unwrap();
    let screen = rendered(&terminal);
    let header = screen.lines().next().unwrap();
    assert!(
        header.starts_with("←─..")
            && header.contains("visible-tail")
            && header.contains("? help")
    );
    assert!(!header.contains("v select") && !header.contains("[D]"));
    app.queue_view.status = "Deleting · 20 removed · 3 queued".to_owned();
    terminal.draw(|frame| app.draw(frame, false, true)).unwrap();
    let screen = rendered(&terminal);
    let header = screen.lines().next().unwrap();
    assert!(header.contains("[D]") && header.contains("? help"));
    assert!(!header.contains("D queue") && !header.contains("v select"));
    assert_eq!(clipped_path_tail("/one/two/three", 10), "..wo/three");
    for width in 0..12 {
        assert!(Line::from(clipped_path_tail("/parent/界界界tail", width)).width() <= width);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn deletion_header_tracks_confirmed_work_through_batch_review_and_admission() {
    std::fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(fixture.path().join("root-file"), b"retained").unwrap();
    let anchor = Arc::new(RootAnchor::open(fixture.path()).unwrap());
    let mut app = browser_fixture_at(fixture.path().to_owned());
    app.selected = Some(2);
    app.visual.enter(ROOT, Some(2));
    let mut queue = DeleteQueue::new().unwrap();
    queue.toggle_pause();
    let mut batch = None;
    prepare_batch(
        &mut app,
        Some(Arc::clone(&anchor)),
        &mut batch,
        &VecDeque::new(),
    );
    assert!(batch.is_some());
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    let mut header =
        |app: &mut App, queue: &mut DeleteQueue, batch: Option<&BatchPreparation>| {
            terminal
                .draw(|frame| {
                    app.sync_queue(queue, frame.area());
                    app.draw(frame, false, deletion_busy(queue, batch));
                })
                .unwrap();
            rendered(&terminal).lines().next().unwrap().to_owned()
        };
    for ready in [false, true] {
        let Some(Modal::Batch { ready: state, .. }) = app.modal.as_mut() else {
            panic!("batch review must remain open");
        };
        *state = ready;
        let text = header(&mut app, &mut queue, batch.as_ref());
        assert!(text.contains("Visual") && text.contains("? help"));
        assert!(
            !text.contains("Deleting") && !text.contains("[D]"),
            "unconfirmed batch advertised deletion: {text}"
        );
    }
    let queued = queue.enqueue(2, Arc::clone(&anchor), "root-file".into());
    assert!(queued.inserted);
    let text = header(&mut app, &mut queue, batch.as_ref());
    assert!(text.contains("Deleting") && text.contains("[D]"));
    assert!(queue.remove_pending(queued.id).is_some());
    assert!(matches!(
        app.key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            None,
            deletion_busy(&queue, batch.as_ref())
        ),
        Action::BatchConfirm
    ));
    for dispatching in [false, true] {
        let preparation = batch.as_mut().unwrap();
        preparation.confirm_pending = !dispatching;
        preparation.dispatching = dispatching;
        assert!(!queue.is_busy());
        let text = header(&mut app, &mut queue, batch.as_ref());
        assert!(text.contains("Deleting") && text.contains("[D]"));
        assert!(matches!(app.modal, Some(Modal::Enqueueing)));
    }
    drop(batch.take());
    app.modal = None;
    let queued = queue.enqueue(2, anchor, "root-file".into());
    assert!(queued.inserted);
    let text = header(&mut app, &mut queue, None);
    assert!(text.contains("queued") && text.contains("Paused") && text.contains("[D]"));
    assert!(queue.remove_pending(queued.id).is_some());
    let text = header(&mut app, &mut queue, None);
    assert!(!text.contains("Deleting") && !text.contains("[D]"));
    assert!(fixture.path().join("root-file").is_file());
}

#[test]
fn visual_header_restores_controls_without_idle_selection_hints() {
    for width in [39, 40, 59, 60, 80, 160] {
        let mut app = browser_fixture();
        app.selected = Some(1);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let screen = rendered(&terminal);
        let header = screen.lines().next().unwrap();
        assert!(header.contains("? help"));
        assert!(!header.contains("d delete") && !header.contains("v select"));
        app.visual.enter(ROOT, Some(1));
        for deleting in [false, true] {
            terminal
                .draw(|frame| app.draw(frame, false, deleting))
                .unwrap();
            let screen = rendered(&terminal);
            let header = screen.lines().next().unwrap();
            assert!(header.contains("Visual") && header.contains("? help"));
            assert_eq!(header.contains("d delete"), width >= 40);
            assert_eq!(header.contains("Esc clear"), width >= 60);
            assert_eq!(header.contains("[D]"), deleting);
            assert!(!header.contains("v select") && !header.contains("D queue"));
        }
    }
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
        assert!(text.contains("× queued/deleting") && text.contains("Preview (96+ columns)"));
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
fn bottom_row_mouse_capture_preserves_viewport_through_shrink_and_growth() {
    for visual in [false, true] {
        for offset in [0, 10] {
            let mut app = navigation_fixture(40, 41..=41);
            let first = offset as NodeId + 1;
            app.selected = Some(first);
            app.offset = offset;
            if visual {
                app.visual.enter(ROOT, None);
            }
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            let original = app.painted_rows.clone();
            let pressed_view = Arc::clone(&app.painted_view);
            let mouse = |kind| MouseEvent {
                kind,
                row: 10,
                column: 35,
                modifiers: KeyModifiers::NONE,
            };
            app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
            terminal.backend_mut().resize(80, 7);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(Arc::ptr_eq(&app.painted_view, &pressed_view));
            assert_eq!(app.painted_offset, offset);
            assert_eq!(app.painted_rows, original[..6]);
            assert_eq!(app.selected, Some(first + 9));
            assert_eq!(app.selection_screen_row(), None);
            terminal.backend_mut().resize(80, 11);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.painted_offset, offset);
            assert_eq!(app.painted_rows, original);
            assert_eq!(app.selection_screen_row(), Some(9));
            app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
            assert_eq!(app.painted_offset, offset);
            assert_eq!(app.selection_screen_row(), Some(9));
            assert_eq!(app.hover_position, (!visual).then_some((35, 10)));
            assert_eq!(app.visual.len(), usize::from(visual));
            app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), None, false);
            terminal
                .draw(|frame| app.draw(frame, false, false))
                .unwrap();
            assert_eq!(app.selected, Some(first + 8));
            assert_eq!(app.painted_offset, offset + 2);
            assert_eq!(app.selection_screen_row(), Some(6));
        }
    }
}

#[test]
fn resized_visual_capture_keeps_original_drag_and_release_targets() {
    for deferred in [false, true] {
        for release_only in [false, true] {
            for offset in [0, 10] {
                let mut app = navigation_fixture(40, 41..=41);
                let first = offset as NodeId + 1;
                app.selected = Some(first);
                app.offset = offset;
                app.visual.enter(ROOT, None);
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                let original = app.painted_rows.clone();
                if deferred {
                    app.visual
                        .begin_range(Arc::clone(&app.view), first, RangeMode::Add);
                }
                let mouse = |kind, row| MouseEvent {
                    kind,
                    row,
                    column: 35,
                    modifiers: KeyModifiers::NONE,
                };
                app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10));
                assert_eq!(app.has_held_mouse(), deferred);
                terminal.backend_mut().resize(80, 7);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.painted_offset, offset);
                assert_eq!(app.painted_rows, original[..6]);
                // Resolve an earlier range while the held press is clipped.
                // Transferring a deferred press must retain its viewport too.
                app.pump_visual();
                assert!(app.mouse_press_view.is_some() && !app.has_held_mouse());
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.selected, Some(first + 9));
                assert_eq!(app.painted_offset, offset);
                assert_eq!(app.painted_rows, original[..6]);
                terminal.backend_mut().resize(80, 11);
                terminal
                    .draw(|frame| app.draw(frame, false, false))
                    .unwrap();
                assert_eq!(app.painted_offset, offset);
                assert_eq!(app.painted_rows, original);
                if !release_only {
                    app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 9));
                }
                app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 9));
                finish_visual(&mut app);
                assert_eq!(app.selected, Some(first + 8));
                assert_eq!(app.visual.len(), 2 + usize::from(deferred));
                for id in 1..=40 {
                    assert_eq!(
                        app.visual.contains(id),
                        (first + 8..=first + 9).contains(&id) || (deferred && id == first),
                        "deferred={deferred}, release_only={release_only}, offset={offset}, id={id}"
                    );
                }
                assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
            }
        }
    }
}

#[test]
fn releasing_a_resize_clipped_press_restores_visibility_and_keyboard_margin() {
    for visual in [false, true] {
        let mut app = navigation_fixture(40, 41..=41);
        app.selected = Some(1);
        if visual {
            app.visual.enter(ROOT, None);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 11)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        let mouse = |kind| MouseEvent {
            kind,
            row: 10,
            column: 35,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        terminal.backend_mut().resize(80, 7);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.painted_offset, 0);
        assert_eq!(app.selection_screen_row(), None);
        app.mouse(mouse(MouseEventKind::Up(MouseButton::Left)));
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert!(app.mouse_press_view.is_none() && app.mouse_position.is_none());
        assert!(app.hover_position.is_none());
        assert_eq!(app.selected, Some(10));
        assert_eq!(app.painted_offset, 4);
        assert_eq!(app.selection_screen_row(), Some(5));
        assert_eq!(app.visual.len(), usize::from(visual));
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), None, false);
        terminal
            .draw(|frame| app.draw(frame, false, false))
            .unwrap();
        assert_eq!(app.selected, Some(9));
        assert_eq!(app.selection_screen_row(), Some(3));
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

#[test]
fn fixed_edge_layout_matches_solver_across_terminal_sizes() {
    use ratatui::layout::{Constraint, Layout};

    let mut edges = vec![(2, 4), (3, 2), (1, 3)];
    edges.extend((0..=18).map(|height| (height, 3)));
    edges.extend(
        ["? help", "d delete  ? help", "d delete  Esc clear  ? help"]
            .map(|hint| (2, hint.len() as u16 + 2)),
    );
    let mut differences = Vec::new();
    for direction in [Direction::Horizontal, Direction::Vertical] {
        for &(leading, trailing) in &edges {
            for available in 0..=100 {
                let area = match direction {
                    Direction::Horizontal => Rect::new(7, 9, available, 3),
                    Direction::Vertical => Rect::new(7, 9, 3, available),
                };
                let reference = Layout::default()
                    .direction(direction)
                    .constraints([
                        Constraint::Length(leading),
                        Constraint::Min(0),
                        Constraint::Length(trailing),
                    ])
                    .split(area);
                let actual = split_edges(area, direction, leading, trailing);
                if actual.as_slice() != reference.as_ref() {
                    differences.push(format!(
                        "{direction:?} {leading}/{trailing} in {available}: {actual:?} != {reference:?}",
                    ));
                }
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    for height in 0..=100 {
        let area = Rect::new(7, 9, 50, height);
        let reference = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(area);
        assert_eq!(
            &split_edges(area, Direction::Vertical, 0, 2)[1..],
            reference.as_ref(),
        );
    }
}
