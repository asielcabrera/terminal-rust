use super::*;
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use terminal_protocol::pane_io::IoEvent;

fn shell(program: &str, args: &[&str]) -> ShellConfig {
    ShellConfig {
        program: Some(program.to_string()),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        integration: false,
        ..ShellConfig::default()
    }
}

/// A program that runs `script` (a `sh -c` script, or a `cmd /C` line on Windows)
fn running(unix: &str, windows: &str) -> ShellConfig {
    if cfg!(windows) {
        shell("cmd", &["/C", windows])
    } else {
        shell("sh", &["-c", unix])
    }
}

fn printing(text: &str) -> ShellConfig {
    running(&format!("echo {text}"), &format!("echo {text}"))
}

fn waiting() -> ShellConfig {
    running("sleep 30", "ping -n 30 127.0.0.1 >NUL")
}

fn events() -> (Notify, Receiver<IoEvent>) {
    let (tx, rx) = channel();
    let tx = std::sync::Mutex::new(tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    (notify, rx)
}

fn create(
    server: &ServerHandle,
    shell: &ShellConfig,
    explicit: bool,
) -> Result<(ServerBlock, Option<String>, Receiver<IoEvent>), String> {
    let (notify, rx) = events();
    let (block, notice) = server.create_block(shell, explicit, 80, 24, 1000, notify)?;
    Ok((block, notice, rx))
}

/// Reads output (raw bytes) until `pending` returns false or 10 s pass, answering ConPTY's
/// cursor position query as libghostty does in the app
fn read_until(io: &mut ServerBlock, mut pending: impl FnMut(&str) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    while pending(&String::from_utf8_lossy(&output)) && Instant::now() < deadline {
        io.rearm();
        match io.try_read() {
            Some(chunk) => {
                if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                    io.write(b"\x1b[1;1R");
                }
                output.extend(chunk);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    output
}

fn wait_for(rx: &Receiver<IoEvent>, wanted: IoEvent) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(event) if event == wanted => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Waits for the next layout event, skipping pane output and exit notices
fn wait_layout(events: &Receiver<LayoutEvent>) -> LayoutEvent {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(LayoutEvent::PaneOutput(_) | LayoutEvent::PaneExited(_)) => continue,
            Ok(event) => return event,
            Err(_) => break,
        }
    }
    panic!("no layout event arrived");
}

/// Waits for a state event and splits it into its parts
fn wait_state(events: &Receiver<LayoutEvent>) -> (LayoutState, Option<CreatedPane>) {
    match wait_layout(events) {
        LayoutEvent::State(state, created) => (state, created),
        other => panic!("expected a state event, got {other:?}"),
    }
}

/// A split of the first pane at `area`, starting `shell` in the new pane
fn split(server: &ServerHandle, pane: PaneId, shell: ShellConfig, area: Rect) {
    server.send(Request::LayoutSplit(SplitOp {
        pane,
        dir: Dir::Right,
        shell,
        explicit: false,
        cols: 40,
        rows: 20,
        scrollback: 1000,
        area,
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    }));
}

fn init(server: &ServerHandle) -> (LayoutState, CreatedPane) {
    server
        .init(&waiting(), 80, 24, 1000)
        .expect("the first pane starts")
}

fn plain_text(terminal: &Terminal<'static, 'static>) -> String {
    let options = FormatterOptions::new().with_format(Format::Plain);
    let mut formatter = Formatter::new(terminal, options).unwrap();
    String::from_utf8_lossy(&formatter.format_alloc(None).unwrap()).into_owned()
}

fn terminal() -> Terminal<'static, 'static> {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 1000,
    })
    .unwrap()
}

#[test]
fn a_split_creates_the_block_and_reports_it() {
    let (server, events) = Server::start();
    let (state, first) = init(&server);
    assert_eq!(first.pane, PaneId(0), "the server allocated the pane id");
    assert_eq!(state.focus, PaneId(0));
    assert_eq!(first.notice, None);
    assert_eq!((first.cols, first.rows), (80, 24));

    let program = running(
        "echo split-marker; sleep 30",
        "echo split-marker & ping -n 30 127.0.0.1 >NUL",
    );
    split(&server, PaneId(0), program, Rect::new(0, 0, 800, 600));
    let (state, created) = wait_state(&events);
    let created = created.expect("the split reports its new pane");
    assert_eq!(created.pane, PaneId(1), "the server allocated the next id");
    assert_eq!(created.notice, None);

    let projection = Layout::from_state(state);
    assert_eq!(projection.tiled_ids(), vec![PaneId(0), PaneId(1)]);
    assert_eq!(projection.focus(), PaneId(1), "the new pane gets the focus");

    // The block behind the new pane really runs: its output reaches the window's block
    let mut block = created.block;
    let output = read_until(&mut block, |text| !text.contains("split-marker"));
    assert!(
        String::from_utf8_lossy(&output).contains("split-marker"),
        "{output:?}"
    );
}

#[test]
#[cfg(unix)]
fn closing_a_pane_kills_its_block() {
    let (server, events) = Server::start();
    init(&server);
    let program = running("echo $$; exec sleep 30", "unused on unix");
    split(&server, PaneId(0), program, Rect::new(0, 0, 800, 600));
    let (_, created) = wait_state(&events);
    let mut block = created.expect("the split reports its pane").block;
    let output = read_until(&mut block, |text| !text.contains('\n'));
    let pid: i32 = String::from_utf8_lossy(&output)
        .trim()
        .parse()
        .expect("the shell printed its pid");

    server.send(Request::LayoutClose { pane: PaneId(1) });
    let (state, created) = wait_state(&events);
    assert!(created.is_none(), "closing reports no new pane");
    assert_eq!(Layout::from_state(state).tiled_ids(), vec![PaneId(0)]);

    // The program behind the closed pane is really dead
    let alive = || {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(true)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(), "the block's program was killed");
}

#[test]
fn closing_the_last_pane_emits_last_pane_closed() {
    let (server, events) = Server::start();
    let (_state, first) = init(&server);
    assert_eq!(first.pane, PaneId(0));
    server.send(Request::LayoutClose { pane: PaneId(0) });
    match wait_layout(&events) {
        LayoutEvent::LastPaneClosed => {}
        other => panic!("expected LastPaneClosed, got {other:?}"),
    }
}

#[test]
fn a_rolled_back_init_burns_the_pane_id() {
    let (server, _events) = Server::start();
    // A 0×0 grid cannot start a block: the init rolls back
    assert!(
        server.init(&waiting(), 0, 0, 1000).is_err(),
        "a 0×0 grid does not start a block"
    );
    // The rolled-back id is burned, never handed out again (advisory A-T2b-2)
    let (state, created) = init(&server);
    assert_ne!(
        created.pane,
        PaneId(0),
        "the id was reused after the rollback"
    );
    assert_eq!(state.focus, created.pane);
    assert_eq!(Layout::from_state(state).tiled_ids(), vec![created.pane]);
}

#[test]
fn a_failed_spawn_rolls_the_layout_back_and_reports_an_error() {
    let (server, events) = Server::start();
    init(&server);
    // An explicit program that cannot start is an error, like a floating pane's own program
    server.send(Request::LayoutSplit(SplitOp {
        pane: PaneId(0),
        dir: Dir::Right,
        shell: shell("no-such-program-layout", &[]),
        explicit: true,
        cols: 40,
        rows: 20,
        scrollback: 1000,
        area: Rect::new(0, 0, 800, 600),
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    }));
    match wait_layout(&events) {
        LayoutEvent::Error(error) => assert!(
            error.starts_with("could not start \"no-such-program-layout\": "),
            "{error}"
        ),
        other => panic!("expected an error event, got {other:?}"),
    }

    // The layout rolled back: the following split finds only the original pane, and the
    // rolled-back id is never handed out again
    split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
    let (state, created) = wait_state(&events);
    let created = created.expect("the good split reports its pane");
    assert_eq!(created.pane, PaneId(2));
    assert_eq!(
        Layout::from_state(state).tiled_ids(),
        vec![PaneId(0), PaneId(2)]
    );
}

#[test]
fn an_exit_never_precedes_the_state_event_for_a_spawned_pane() {
    let (server, events) = Server::start();
    let _ = init(&server);
    // The program exits immediately: its output and exit are queued while the split is
    // applied, so the event order proves State always announces a pane first (State, then
    // the pane's output, then its exit)
    split(
        &server,
        PaneId(0),
        printing("bye"),
        Rect::new(0, 0, 800, 600),
    );

    // The test plays the client's role for the spawned pane, including answering ConPTY's
    // cursor-position query: an unanswered one keeps the child's teardown from completing on
    // Windows, so the exit would never be reported
    let mut created: Option<CreatedPane> = None;
    let mut state_tick = None;
    let mut output_tick = None;
    let mut exit_tick = None;
    // 10s: a cold Windows CI takes a while to spawn cmd.exe and tear down ConPTY; the
    // assertion only needs the order, not the timing
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut tick = 0usize;
    while exit_tick.is_none() && Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(LayoutEvent::State(_, mut pane)) => match pane.take() {
                Some(created_pane) => {
                    assert_eq!(created_pane.pane, PaneId(1), "the split creates pane 1");
                    created = Some(created_pane);
                    state_tick = Some(tick);
                }
                None => panic!("the split reports its pane"),
            },
            Ok(LayoutEvent::PaneOutput(PaneId(1))) => output_tick = Some(tick),
            Ok(LayoutEvent::PaneExited(PaneId(1))) => exit_tick = Some(tick),
            Ok(_) => {}
            Err(_) => {}
        }
        tick += 1;
        // Drain the pane's output and answer the cursor query, as the app does
        if let Some(block) = created.as_mut().map(|created| &mut created.block) {
            #[cfg(unix)]
            block.rearm();
            while let Some(chunk) = block.try_read() {
                if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                    block.write(b"\x1b[1;1R");
                }
            }
        }
    }
    drop(created);

    let state_tick = state_tick.expect("the split reports its pane");
    if let Some(output_tick) = output_tick {
        assert!(output_tick > state_tick, "output arrived before the state");
    }
    let exit_tick = exit_tick.expect("the exit is reported");
    assert!(exit_tick > state_tick, "the exit arrived before the state");
}

#[test]
fn a_closed_twice_pane_stays_silent() {
    let (server, events) = Server::start();
    let _ = init(&server);
    split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
    let _ = wait_state(&events);

    server.send(Request::LayoutClose { pane: PaneId(1) });
    let _ = wait_state(&events);
    // Drain whatever the kill still has in flight before the silence check
    while events.try_recv().is_ok() {}

    // Closing an unknown pane again answers nothing
    server.send(Request::LayoutClose { pane: PaneId(1) });
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second close is silent"
    );
}

#[test]
fn pane_ids_are_allocated_by_the_server_and_never_reused() {
    let (server, events) = Server::start();
    let (_state, first) = init(&server);
    assert_eq!(first.pane, PaneId(0));

    for expected in [1u32, 2] {
        split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
        let (_, created) = wait_state(&events);
        assert_eq!(
            created.expect("the split reports its pane").pane,
            PaneId(expected)
        );
    }

    server.send(Request::LayoutClose { pane: PaneId(1) });
    let (state, _) = wait_state(&events);
    assert_eq!(
        Layout::from_state(state).tiled_ids(),
        vec![PaneId(0), PaneId(2)]
    );

    // The closed pane's id is not handed out again
    split(&server, PaneId(2), waiting(), Rect::new(0, 0, 800, 600));
    let (state, created) = wait_state(&events);
    assert_eq!(created.expect("the split reports its pane").pane, PaneId(3));
    assert_eq!(
        Layout::from_state(state).tiled_ids(),
        vec![PaneId(0), PaneId(2), PaneId(3)]
    );
}

#[test]
fn a_refused_split_changes_nothing_and_stays_silent() {
    let (server, events) = Server::start();
    init(&server);
    // Both halves would fall below the minimum: the window previews the same check and
    // never sees an event
    split(&server, PaneId(0), waiting(), Rect::new(0, 0, 150, 600));
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match events.try_recv() {
            Ok(LayoutEvent::State(..) | LayoutEvent::LastPaneClosed | LayoutEvent::Error(_)) => {
                panic!("a refused split must stay silent")
            }
            Ok(_) => {} // pane output noise (ConPTY) is fine
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn the_program_output_arrives_and_its_exit_is_reported() {
    let (server, _events) = Server::start();
    let (mut block, notice, rx) =
        create(&server, &printing("hello-server"), true).expect("the program starts");
    assert_eq!(notice, None);
    let output = read_until(&mut block, |text| !text.contains("hello-server"));
    assert!(
        String::from_utf8_lossy(&output).contains("hello-server"),
        "{output:?}"
    );
    assert!(wait_for(&rx, IoEvent::Exited), "the exit is reported");
}

#[test]
fn output_wakes_the_client() {
    let (server, _events) = Server::start();
    let (_block, _, rx) = create(&server, &printing("wake"), true).unwrap();
    assert!(wait_for(&rx, IoEvent::Output));
}

#[test]
fn killing_the_program_reports_its_exit() {
    let (server, _events) = Server::start();
    let (mut block, _, rx) = create(&server, &waiting(), true).unwrap();
    block.kill();
    assert!(wait_for(&rx, IoEvent::Exited));
}

#[test]
fn an_explicit_program_that_cannot_start_is_an_error() {
    let (server, _events) = Server::start();
    let error = create(&server, &shell("no-such-program-server", &[]), true)
        .err()
        .expect("it does not start");
    assert!(
        error.starts_with("could not start \"no-such-program-server\": "),
        "{error}"
    );
}

#[test]
fn a_configured_shell_that_cannot_start_falls_back_to_the_default_shell() {
    let (server, _events) = Server::start();
    let (mut block, notice, _rx) = create(&server, &shell("no-such-program-server", &[]), false)
        .expect("the default shell starts");
    let notice = notice.expect("a notice explains the fallback");
    assert!(
        notice.starts_with("could not start \"no-such-program-server\": "),
        "{notice}"
    );
    assert!(notice.ends_with("; using the default shell"), "{notice}");
    block.kill();
}

#[test]
fn a_snapshot_keeps_the_cursor_with_origin_mode_and_a_scrolling_region() {
    // DECOM plus a scroll region makes the final CUP relative to the region's origin, so the
    // restored cursor would land somewhere else than where the client saw it
    let mut expected = terminal();
    expected.vt_write(b"hello\x1b[5;20r\x1b[?6h\x1b[3;7Hmark");
    assert!(
        expected
            .mode(libghostty_vt::terminal::Mode::ORIGIN)
            .unwrap(),
        "the repro actually sets origin mode"
    );

    let mut restored = terminal();
    restored.vt_write(&snapshot(&expected));

    assert_eq!(
        (restored.cursor_x().unwrap(), restored.cursor_y().unwrap()),
        (expected.cursor_x().unwrap(), expected.cursor_y().unwrap())
    );
    assert!(
        restored
            .mode(libghostty_vt::terminal::Mode::ORIGIN)
            .unwrap(),
        "origin mode survives the snapshot"
    );
}

#[test]
fn a_snapshot_reproduces_the_screen_and_the_cursor() {
    let (server, _events) = Server::start();
    let program = running(
        "printf 'first line\\n\\tplain \\033[31mred\\033[0m end-of-snapshot'; sleep 30",
        "echo first line & echo plain red end-of-snapshot & ping -n 30 127.0.0.1 >NUL",
    );
    let (mut block, _, _rx) = create(&server, &program, true).unwrap();
    let output = read_until(&mut block, |text| !text.contains("end-of-snapshot"));

    // What the client saw, fed into its own terminal
    let mut expected = terminal();
    expected.vt_write(&output);

    let snapshot = block.snapshot();
    if cfg!(unix) {
        assert!(
            String::from_utf8_lossy(&snapshot).contains("31m")
                || String::from_utf8_lossy(&snapshot).contains("38;5;1m"),
            "the color is kept: {snapshot:?}"
        );
    }
    let mut restored = terminal();
    restored.vt_write(&snapshot);

    assert_eq!(plain_text(&restored), plain_text(&expected));
    assert!(plain_text(&restored).contains("end-of-snapshot"));
    assert_eq!(
        (restored.cursor_x().unwrap(), restored.cursor_y().unwrap()),
        (expected.cursor_x().unwrap(), expected.cursor_y().unwrap())
    );
    block.kill();
}

#[test]
fn a_client_that_does_not_read_does_not_stall_other_blocks() {
    let (server, _events) = Server::start();
    // Floods output and is never drained. Each line goes back to the top-left corner: the
    // screen never scrolls, which is slow in debug builds of libghostty.
    // windows: PowerShell's console writes only moved ~13KB in 10s through ConPTY, so flood
    // with plain cmd echoes of one 8KB literal: at ~1600 echoes/s, 200 iterations (~1.6MB)
    // finish quickly, and the raw ESC[H before each echo keeps the screen from scrolling.
    let windows_flood = format!(
        "for /L %i in (1,1,200) do @echo \u{1b}[H{}",
        "y".repeat(8000)
    );
    let flood = running("yes \"$(printf 'flood\\033[H')\"", &windows_flood);
    let (mut flooding, _, flood_events) = create(&server, &flood, true).unwrap();
    assert!(wait_for(&flood_events, IoEvent::Output));
    std::thread::sleep(Duration::from_millis(300));

    let (mut quiet, _, _rx) = create(&server, &printing("still-flowing"), true).unwrap();
    let output = read_until(&mut quiet, |text| !text.contains("still-flowing"));
    assert!(
        String::from_utf8_lossy(&output).contains("still-flowing"),
        "{output:?}"
    );

    // The flooding block resumes once its client reads
    let resumed = read_until(&mut flooding, |text| text.len() < 512 * 1024);
    assert!(resumed.len() >= 512 * 1024, "{}", resumed.len());
    flooding.kill();
}

/// Reads what is available right now, without waiting for more
#[cfg(unix)] // only the exit-ordering test below uses it
fn read_available(io: &mut ServerBlock) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        io.rearm();
        match io.try_read() {
            Some(chunk) => output.extend(chunk),
            None => break,
        }
    }
    output
}

#[test]
#[cfg(unix)] // Windows floods through ConPTY too slowly for this volume in the test window
fn the_exit_is_notified_after_all_the_output_was_forwarded() {
    let (server, _events) = Server::start();
    // 30000 fixed-width lines (~360KB, ~88 chunks) overrun the 16-chunks-per-drain cap and
    // the 64-chunk client channel: when the program exits, chunks are still queued, so the
    // exit must wait until every line was forwarded to the client. Each line homes the cursor
    // first: scrolling is slow in debug builds of libghostty.
    let program = running(
        "awk 'BEGIN{for(i=0;i<30000;i++) printf \"\\033[Hline %06d\\n\", i}'",
        "echo unused on unix",
    );
    let (mut block, _, rx) = create(&server, &program, true).unwrap();

    // A client that treats the exit as the end: polls for the exit, and reads a pass only
    // after each wait. The exit must come only when every chunk was already forwarded.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(IoEvent::Exited) => break,
            Ok(_) => {}
            Err(_) if Instant::now() < deadline => {}
            Err(_) => panic!("the exit was never reported"),
        }
        output.extend(read_available(&mut block));
    }
    output.extend(read_available(&mut block));

    // The first and the last line must be there
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("line 000000"), "the first line is kept");
    assert!(
        text.contains("line 029999"),
        "the last line is not lost: got {} bytes",
        text.len()
    );

    // And the exit is the last notification: no output follows it
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        rx.try_recv().is_err(),
        "no output notification follows the exit"
    );
    block.kill();
}

// ---- Wire format (stage 3) ------------------------------------------------------------
// The same ops and events travel over the wire as cross the in-process links today; these
// tests pin the JSON round trip so stage 3 can reuse the mapping without behavior drift.

use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use terminal_protocol::layout::Axis;
use wire::{
    Deframed, IpcCreatedPane, WIRE_VERSION, WireEvent, WireEventKind, WireOp, WireReply,
    WireReplyKind, WireRequest, WireShell, deframe, deframe_json, frame, frame_json,
};

/// Encode, decode, and check nothing changed
fn round_trip<T>(original: T) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_vec(&original).expect("serializes");
    serde_json::from_slice(&json).expect("deserializes")
}

/// Shell config exercising every field
fn full_shell() -> ShellConfig {
    ShellConfig {
        program: Some("zsh".to_string()),
        args: vec!["-l".to_string()],
        cwd: Some(PathBuf::from("/tmp")),
        env: BTreeMap::from([("TERM".to_string(), "xterm".to_string())]),
        integration: true,
    }
}

/// A representative request for each op that travels over the wire
fn wire_ops() -> Vec<WireOp> {
    let limits = Limits {
        min_w: 80,
        min_h: 60,
    };
    vec![
        WireOp::Init {
            shell: WireShell::from_config(&full_shell()),
            cols: 80,
            rows: 24,
            scrollback: 1000,
        },
        WireOp::Split {
            pane: PaneId(3),
            dir: Dir::Down,
            shell: WireShell::from_config(&full_shell()),
            explicit: true,
            cols: 40,
            rows: 20,
            scrollback: 500,
            area: Rect::new(1, 2, 300, 400),
            limits,
        },
        WireOp::Close { pane: PaneId(1) },
        WireOp::SetFocus { pane: PaneId(2) },
        WireOp::FocusDir {
            dir: Dir::Left,
            area: Rect::new(0, 0, 800, 600),
        },
        WireOp::FocusCycle { forward: false },
        WireOp::Resize {
            pane: PaneId(4),
            dir: Dir::Up,
            delta: 33,
            area: Rect::new(0, 0, 800, 600),
            limits,
        },
        WireOp::SetDivider {
            path: vec![false, true, true],
            boundary: 512,
            area: Rect::new(0, 0, 800, 600),
            limits,
        },
        WireOp::ToggleZoom,
        WireOp::AddFloat {
            shell: WireShell::from_config(&full_shell()),
            explicit: false,
            env: [(
                "ASPNETCORE_ENVIRONMENT".to_string(),
                "Development".to_string(),
            )]
            .into(),
            width: 0.5,
            height: 0.25,
            cols: 60,
            rows: 10,
            scrollback: 0,
        },
        WireOp::ToggleFloats,
        WireOp::MoveFloat {
            pane: PaneId(5),
            dx: -10,
            dy: 24,
            window: Rect::new(0, 0, 800, 600),
        },
        WireOp::ResizeFloatEdge {
            pane: PaneId(6),
            edge: Edge {
                left: true,
                right: true,
                top: false,
                bottom: true,
            },
            dx: 5,
            dy: -7,
            window: Rect::new(0, 0, 800, 600),
            limits,
        },
        WireOp::Write {
            block: BlockId(9),
            bytes: vec![0x1b, 0xFF, 0x00, b'a'],
        },
        WireOp::ResizeBlock {
            block: BlockId(9),
            cols: 100,
            rows: 40,
        },
        WireOp::Kill { block: BlockId(9) },
        WireOp::CloseBlock { block: BlockId(9) },
        WireOp::ForegroundDir { block: BlockId(2) },
        WireOp::Snapshot { block: BlockId(2) },
    ]
}

#[test]
fn wire_requests_carry_the_same_ops_the_in_process_channel_does() {
    // A real request maps to exactly the wire op a client would have written
    let request = Request::LayoutSplit(SplitOp {
        pane: PaneId(0),
        dir: Dir::Right,
        shell: full_shell(),
        explicit: true,
        cols: 40,
        rows: 20,
        scrollback: 500,
        area: Rect::new(1, 2, 300, 400),
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    });
    assert_eq!(
        WireOp::from_request(&request).expect("a layout split travels"),
        WireOp::Split {
            pane: PaneId(0),
            dir: Dir::Right,
            shell: WireShell::from_config(&full_shell()),
            explicit: true,
            cols: 40,
            rows: 20,
            scrollback: 500,
            area: Rect::new(1, 2, 300, 400),
            limits: Limits {
                min_w: 80,
                min_h: 60,
            },
        }
    );

    for request in [
        Request::LayoutSetFocus { pane: PaneId(2) },
        Request::LayoutToggleZoom,
        Request::LayoutToggleFloats,
        Request::LayoutFocusCycle { forward: true },
        Request::Write(BlockId(3), vec![0xFF]),
        Request::Resize(BlockId(3), 90, 30),
        Request::Kill(BlockId(3)),
        Request::Close(BlockId(3)),
    ] {
        WireOp::from_request(&request).unwrap_or_else(|| panic!("{request:?} travels"));
    }

    // Server-internal traffic does not travel
    assert!(
        WireOp::from_request(&Request::Shutdown).is_none(),
        "shutdown stays in the process"
    );

    // The float's env entries land in the spawned shell, over the global shell.env
    let request = WireOp::AddFloat {
        shell: WireShell::from_config(&ShellConfig {
            env: BTreeMap::from([("GLOBAL".to_string(), "one".to_string())]),
            ..full_shell()
        }),
        explicit: false,
        env: BTreeMap::from([
            (
                "ASPNETCORE_ENVIRONMENT".to_string(),
                "Development".to_string(),
            ),
            ("GLOBAL".to_string(), "two".to_string()),
        ]),
        width: 0.5,
        height: 0.25,
        cols: 60,
        rows: 10,
        scrollback: 0,
    }
    .into_request()
    .expect("a float add travels");
    let Request::LayoutAddFloat(op) = request else {
        panic!("{request:?}")
    };
    assert_eq!(
        op.shell
            .env
            .get("ASPNETCORE_ENVIRONMENT")
            .map(String::as_str),
        Some("Development")
    );
    assert_eq!(op.shell.env.get("GLOBAL").map(String::as_str), Some("two"));

    // Every op that travels survives JSON wrapped in the versioned envelope
    for op in wire_ops() {
        let request = WireRequest {
            v: WIRE_VERSION,
            rid: None,
            op,
        };
        let json = serde_json::to_vec(&request).expect("serializes");
        assert!(
            String::from_utf8_lossy(&json).contains(&format!("\"v\":{WIRE_VERSION}")),
            "the protocol version is pinned: {:?}",
            String::from_utf8_lossy(&json)
        );
        assert_eq!(round_trip(request.clone()), request, "{request:?}");
    }
}

#[test]
fn wire_requests_name_their_op_and_a_rid_survives_the_round_trip() {
    let request = WireRequest {
        v: WIRE_VERSION,
        rid: Some(42),
        op: WireOp::Snapshot { block: BlockId(1) },
    };
    let json = serde_json::to_string(&request).expect("serializes");
    assert!(json.starts_with("{\"v\":1"), "{json}");
    assert!(json.contains("\"op\":\"snapshot\""), "{json}");
    assert_eq!(round_trip(request.clone()), request);
}

#[test]
fn the_layout_state_survives_the_wire() {
    // A nested tree with zoom, floats and history: every field of LayoutState
    let mut layout = Layout::new(PaneId(1));
    assert!(layout.split(
        PaneId(1),
        Axis::Horizontal,
        PaneId(2),
        Rect::new(0, 0, 800, 600),
        Limits {
            min_w: 80,
            min_h: 60,
        }
    ));
    assert!(layout.split(
        PaneId(2),
        Axis::Vertical,
        PaneId(3),
        Rect::new(0, 0, 800, 600),
        Limits {
            min_w: 80,
            min_h: 60,
        }
    ));
    assert!(layout.toggle_zoom());
    layout.add_float(PaneId(4), 0.5, 0.5);
    assert!(layout.toggle_floats());
    assert!(layout.set_focus(PaneId(3)));

    let state = layout.state();
    assert_eq!(state.zoomed, Some(PaneId(3)));
    assert_eq!(state.floats.len(), 1);
    assert!(!state.floats_visible);

    let json = serde_json::to_string(&state).expect("serializes");
    assert!(json.contains("\"root\""), "{json}");
    let back: LayoutState = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(back, state, "the state round trips field for field");
    // And a projection rebuilt from a decoded state answers exactly like the original
    assert_eq!(Layout::from_state(back).state(), state);
}

#[test]
fn wire_events_map_layout_events_without_the_block() {
    // A created pane carries its block in-process; on the wire only pane, size and notice go
    let (server, _events) = Server::start();
    let (block, notice, _rx) =
        create(&server, &shell("no-such-program-wire", &[]), false).expect("the fallback");
    let created = CreatedPane {
        pane: PaneId(7),
        block,
        cols: 80,
        rows: 24,
        notice,
    };
    let layout = Layout::new(PaneId(7));
    let state = layout.state();

    let event = WireEvent::from_layout_event(&LayoutEvent::State(state.clone(), Some(created)))
        .expect("a state event travels");
    let WireEventKind::State {
        state: wire_state,
        created: wire_created,
    } = &event.event
    else {
        panic!("mapped to a state event: {event:?}");
    };
    assert_eq!(wire_state, &state, "the state travels whole");
    let wire_created = wire_created.as_ref().expect("the created pane travels");
    assert_eq!(wire_created.pane, PaneId(7));
    assert_eq!(wire_created.cols, 80);
    assert_eq!(wire_created.rows, 24);
    let notice = wire_created
        .notice
        .clone()
        .expect("the fallback notice travels");
    assert!(notice.ends_with("; using the default shell"), "{notice}");
    assert_eq!(round_trip(event.clone()), event);
    assert!(
        serde_json::to_string(&event)
            .unwrap()
            .contains("\"event\":\"state\""),
        "events name their kind"
    );

    // A state event with no created pane, and the other kinds, map and round trip too
    for event in [
        WireEvent::from_layout_event(&LayoutEvent::State(layout.state(), None)).expect("travels"),
        WireEvent::from_layout_event(&LayoutEvent::LastPaneClosed).expect("travels"),
        WireEvent::from_layout_event(&LayoutEvent::Error("boom".to_string())).expect("travels"),
        WireEvent::from_layout_event(&LayoutEvent::PaneOutput(PaneId(1))).expect("travels"),
        WireEvent::from_layout_event(&LayoutEvent::PaneExited(PaneId(1))).expect("travels"),
    ] {
        assert_eq!(round_trip(event.clone()), event, "{event:?}");
    }
}

#[test]
fn wire_replies_carry_the_state_with_the_pane_token() {
    let (server, _events) = Server::start();
    let (block, notice, _rx) =
        create(&server, &shell("no-such-program-wire-reply", &[]), false).expect("the fallback");
    let created = CreatedPane {
        pane: PaneId(7),
        block,
        cols: 80,
        rows: 24,
        notice,
    };
    // What the wire should carry for this pane: pane, its block token, size and notice;
    // the block view itself still does not travel
    let expected = IpcCreatedPane::from_created(&created);
    let reply = WireReply::init(11, Ok((Layout::new(PaneId(7)).state(), Some(created))));
    assert_eq!(reply.rid, 11);
    assert_eq!(reply.v, WIRE_VERSION);
    let json = serde_json::to_string(&reply).expect("serializes");
    assert!(json.contains("\"reply\":\"init\""), "{json}");
    assert_eq!(round_trip(reply.clone()), reply);
    // The decoded created pane is exactly what the window needs to build its pane
    let WireReplyKind::Init {
        state,
        created: Some(wire_created),
    } = reply.reply
    else {
        panic!("an init reply");
    };
    assert_eq!(state.focus, PaneId(7));
    assert_eq!(wire_created, expected);
    assert_eq!(
        wire_created.block, expected.block,
        "the pane's block token travels: it names the block over the data channel"
    );
    let notice = wire_created.notice.expect("the fallback notice travels");
    assert!(notice.ends_with("; using the default shell"), "{notice}");

    // The failure of an init, the foreground directory and bytes that are not UTF-8 too
    let failed = WireReply::init(12, Err("did not start".to_string()));
    assert_eq!(round_trip(failed.clone()), failed);
    match failed.reply {
        WireReplyKind::InitFailed { error } => assert_eq!(error, "did not start"),
        other => panic!("an init failure: {other:?}"),
    }
    let dir = WireReply::foreground_dir(13, Some(PathBuf::from("/home")));
    assert_eq!(round_trip(dir.clone()), dir);
    let snapshot = WireReply::snapshot(14, vec![0xFF, b'\x1b', 0x00]);
    assert_eq!(round_trip(snapshot.clone()), snapshot);
}

#[test]
fn frames_carry_whole_messages_and_reject_corrupt_lengths() {
    // A frame round trips
    let framed = frame(b"hello wire");
    assert_eq!(&framed[..4], &10u32.to_le_bytes(), "4-byte LE length first");
    match deframe(&framed) {
        Deframed::Message { bytes, consumed } => {
            assert_eq!(bytes, b"hello wire");
            assert_eq!(consumed, framed.len());
        }
        other => panic!("decodes the frame: {other:?}"),
    }

    // Two frames in one buffer decode one at a time
    let two = [framed.clone(), frame(b"{}")].concat();
    let (first, used) = match deframe(&two) {
        Deframed::Message { bytes, consumed } => (bytes, consumed),
        other => panic!("decodes the first: {other:?}"),
    };
    assert_eq!(first, b"hello wire");
    match deframe(&two[used..]) {
        Deframed::Message { bytes, consumed } => {
            assert_eq!(bytes, b"{}");
            assert_eq!(consumed, two.len() - used);
        }
        other => panic!("decodes the second: {other:?}"),
    }

    // Incomplete frames wait for more bytes
    assert_eq!(deframe(&framed[..3]), Deframed::Incomplete);
    assert_eq!(deframe(&framed[..framed.len() - 1]), Deframed::Incomplete);

    // A length over the protocol cap is refused, never waited for
    assert_eq!(deframe(&u32::MAX.to_le_bytes()), Deframed::Invalid);

    // And the JSON helpers compose them
    let request = WireRequest {
        v: WIRE_VERSION,
        rid: Some(1),
        op: WireOp::FocusCycle { forward: true },
    };
    let mut wire = frame_json(&request).expect("serializes");
    wire.extend_from_slice(&frame_json(&request).expect("serializes"));
    let (first, used) = deframe_json::<WireRequest>(&wire)
        .expect("decodes")
        .expect("a whole message");
    assert_eq!(first, request);
    let (second, rest) = deframe_json::<WireRequest>(&wire[used..])
        .expect("decodes")
        .expect("a whole message");
    assert_eq!(second, request);
    assert_eq!(used + rest, wire.len());
}

#[test]
fn an_attach_on_a_fresh_server_behaves_like_init() {
    let (server, _events) = Server::start();
    let (reply, answer) = channel();
    server.send(Request::Attach {
        shell: waiting(),
        cols: 80,
        rows: 24,
        scrollback: 1000,
        reply,
    });
    match answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the attach answers")
    {
        AttachOutcome::Fresh(Ok((state, created))) => {
            assert_eq!(created.pane, PaneId(0), "a fresh start inits the layout");
            assert_eq!(state.focus, PaneId(0));
            assert_eq!((created.cols, created.rows), (80, 24));
        }
        other => panic!("expected a fresh attach, got {other:?}"),
    }
}

#[test]
fn an_attach_on_an_existing_layout_lists_its_live_panes() {
    let (server, events) = Server::start();
    init(&server);
    split(&server, PaneId(0), waiting(), Rect::new(0, 0, 800, 600));
    let (split_state, created) = wait_state(&events);
    let created = created.expect("the split reports its pane");
    let second_block = created.block.block_id();

    let (reply, answer) = channel();
    server.send(Request::Attach {
        shell: waiting(),
        cols: 80,
        rows: 24,
        scrollback: 1000,
        reply,
    });
    match answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the attach answers")
    {
        AttachOutcome::Existing(attach_state, panes) => {
            assert_eq!(
                attach_state, split_state,
                "the attach carries the current state"
            );
            let ids: Vec<PaneId> = panes.iter().map(|pane| pane.pane).collect();
            assert_eq!(ids, vec![PaneId(0), PaneId(1)], "both live panes travel");
            assert_eq!((panes[0].cols, panes[0].rows), (80, 24));
            assert_eq!(
                (panes[1].cols, panes[1].rows),
                (40, 20),
                "the split's spawn size travels"
            );
            assert_ne!(
                panes[0].block, panes[1].block,
                "each pane names its own block"
            );
            assert_eq!(panes[1].block, second_block);
            assert_eq!(
                Layout::from_state(attach_state).tiled_ids(),
                vec![PaneId(0), PaneId(1)]
            );
        }
        other => panic!("expected an existing attach, got {other:?}"),
    }
}

#[test]
fn the_last_pane_closing_leaves_a_fresh_server_for_the_next_attach() {
    let (server, events) = Server::start();
    init(&server);
    server.send(Request::LayoutClose { pane: PaneId(0) });
    match wait_layout(&events) {
        LayoutEvent::LastPaneClosed => {}
        other => panic!("expected LastPaneClosed, got {other:?}"),
    }

    let (reply, answer) = channel();
    server.send(Request::Attach {
        shell: waiting(),
        cols: 80,
        rows: 24,
        scrollback: 1000,
        reply,
    });
    match answer
        .recv_timeout(Duration::from_secs(10))
        .expect("the attach answers")
    {
        AttachOutcome::Fresh(Ok((state, created))) => {
            // A fresh layout, and a fresh pane id: ids are never reused, not even now
            assert_ne!(created.pane, PaneId(0));
            assert_eq!(state.focus, created.pane);
            assert_eq!(Layout::from_state(state).tiled_ids(), vec![created.pane]);
        }
        other => panic!("expected a fresh attach after the last pane closed, got {other:?}"),
    }
}

#[test]
fn a_panicking_request_does_not_stop_the_server() {
    let (server, _events) = Server::start();
    server.send(Request::Panic);
    // The run loop survives the panic and keeps answering
    let (state, created) = init(&server);
    assert_eq!(created.pane, PaneId(0));
    assert_eq!(state.focus, PaneId(0));
}

/// Reads a block's output until `wanted` shows up (answering ConPTY's cursor query), like
/// the window drains its panes
#[cfg(unix)]
fn read_until_marker(block: &mut dyn PaneIo, wanted: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains(wanted) && Instant::now() < deadline {
        block.rearm();
        match block.try_read() {
            Some(chunk) => {
                if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                    block.write(b"\x1b[1;1R");
                }
                output.extend(chunk);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(
        String::from_utf8_lossy(&output).contains(wanted),
        "{wanted:?} never came back over the socket"
    );
    output
}

#[test]
#[cfg(unix)]
fn the_block_writer_thread_serializes_writes_and_never_blocks_the_server() {
    let (server, _events) = Server::start();
    // Echo off: the kernel echo and the program's own print would produce two interleaved
    // copies of every line; with echo off each write comes back exactly once, in order
    let echo_off = running("stty -echo; cat", "more");
    let (mut block, _notice, _rx) = create(&server, &echo_off, false).expect("the program starts");
    let id = block.block_id();

    // Each sender writes its pair back to back; the single writer thread per block applies
    // each sender's pair in order and every marker stays one whole write
    const PAIRS: usize = 12;
    let mut senders = Vec::new();
    for sender in 0..3 {
        let handle = server.clone();
        senders.push(thread::spawn(move || {
            for round in 0..PAIRS {
                let first = format!("s{sender}-a{round}-{}", "x".repeat(40));
                let second = format!("s{sender}-b{round}-{}", "y".repeat(40));
                handle.send(Request::Write(id, format!("{first}\n").into_bytes()));
                handle.send(Request::Write(id, format!("{second}\n").into_bytes()));
            }
        }));
    }

    // A snapshot must answer while the writes queue: before the per-block writer thread
    // existed, a deep enough PTY write backlog blocked the server loop itself
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    let (reply, answer) = channel();
    server.send(Request::Snapshot(id, reply));
    let screen = answer
        .recv_timeout(Duration::from_secs(2))
        .expect("the server answers a snapshot while writes are pending");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the server answered in {:?} with a write backlog pending",
        started.elapsed()
    );
    assert!(!screen.is_empty());
    let _ = screen;

    for sender in senders {
        sender.join().expect("the writers finished");
    }
    // Every marker arrives whole, and each sender's pairs stay in order
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let complete = |text: &str| {
        (0..3).all(|sender| {
            (0..PAIRS).all(|round| {
                text.contains(&format!("s{sender}-a{round}"))
                    && text.contains(&format!("s{sender}-b{round}"))
            })
        })
    };
    while !complete(&String::from_utf8_lossy(&output)) && Instant::now() < deadline {
        block.rearm();
        match block.try_read() {
            Some(chunk) => {
                if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                    block.write(b"\x1b[1;1R");
                }
                output.extend(chunk);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let text = String::from_utf8_lossy(&output).into_owned();
    for sender in 0..3 {
        for round in 0..PAIRS {
            let first = format!("s{sender}-a{round}-{}", "x".repeat(40));
            let second = format!("s{sender}-b{round}-{}", "y".repeat(40));
            let (a, b) = (
                text.find(&first)
                    .expect("the marker arrived whole; it is one write"),
                text.find(&second)
                    .expect("the marker arrived whole; it is one write"),
            );
            assert!(
                a < b,
                "sender {sender} round {round}: the pair got reordered: {text}"
            );
        }
    }
    block.kill();
}

#[test]
fn typed_frames_dispatch_by_their_type_byte() {
    let request = WireRequest {
        v: WIRE_VERSION,
        rid: Some(3),
        op: WireOp::ToggleZoom,
    };
    let json = wire::frame_json(&request).expect("the request serializes");
    let mut stream: Vec<u8> = Vec::new();
    stream.push(FRAME_CONTROL);
    stream.extend_from_slice(&wire::frame(&json));
    stream.append(&mut data_frame(
        FRAME_DATA,
        BlockId(9),
        b"live-bytes".to_vec(),
    ));
    stream.append(&mut data_frame(
        FRAME_SNAPSHOT,
        BlockId(9),
        b"screen-bits".to_vec(),
    ));
    stream.push(7); // an unknown type byte ends the connection

    let mut read = &stream[..];
    match read_typed_frame(&mut read).expect("the control frame") {
        WireFrame::Control(bytes) => {
            assert_eq!(bytes, json, "the control frame carries the JSON whole");
        }
        frame => panic!("expected a control frame, got {frame:?}"),
    }
    match read_typed_frame(&mut read).expect("the data frame") {
        WireFrame::Data(token, bytes) => {
            assert_eq!(token, 9, "the block token travels");
            assert_eq!(bytes, b"live-bytes");
        }
        frame => panic!("expected a data frame, got {frame:?}"),
    }
    match read_typed_frame(&mut read).expect("the snapshot frame") {
        WireFrame::Snapshot(token, bytes) => {
            assert_eq!(token, 9);
            assert_eq!(bytes, b"screen-bits");
        }
        frame => panic!("expected a snapshot frame, got {frame:?}"),
    }
    assert!(
        read_typed_frame(&mut read).is_err(),
        "an unknown type byte is an error, not a frame"
    );
}

#[test]
#[cfg(unix)]
fn a_real_socket_client_attaches_writes_and_reads_back() {
    // The socket path must be shorter than SUN_LEN: a short /tmp name
    let dir = std::env::temp_dir().join(format!(
        "t{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock works")
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("the test directory is created");
    let path = dir.join("server.sock");

    let (server, layout_events) = Server::start();
    let shared = Arc::new(ProcessShared::default());
    let router_shared = shared.clone();
    thread::spawn(move || route_events(layout_events, router_shared));
    let listener =
        IpcListener::bind(&IpcEndpoint::Socket(path.clone())).expect("the endpoint binds");
    let accept_shared = shared.clone();
    let accept_handle = server.clone();
    thread::spawn(move || accept_sessions(listener, accept_handle, accept_shared));

    // The client connects, spawns its reader, and attaches
    let (client, read) =
        IpcConn::connect(&IpcEndpoint::Socket(path.clone())).expect("the client connects");
    let (layout_tx, layout_rx) = channel::<WireEventKind>();
    {
        let client = client.clone();
        thread::spawn(move || {
            client.read_loop(read, move |down| match down {
                ControlDown::Event(event) => {
                    let _ = layout_tx.send(event);
                }
                ControlDown::Reply(reply) => {
                    panic!("an unexpected reply arrived over the wire: {reply:?}")
                }
            });
        });
    }
    // An echoing program, so the written bytes come over the data channel back
    let echo = running("cat", "more");
    let answer = client
        .attach(&echo, 80, 24, 1000)
        .expect("the attach answers");
    let AttachAnswer::Fresh { state, pane } = answer else {
        panic!("a fresh server attaches like init");
    };
    assert_eq!(state.focus, pane.pane);

    // The pane is built exactly as the window would: an IpcBlock over the connection
    let (event_tx, _event_rx) = channel::<IoEvent>();
    let event_tx = std::sync::Mutex::new(event_tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = event_tx.lock().unwrap().send(event);
    });
    let mut block = client.open_block(pane.block, notify);
    assert_eq!(pane.block, BlockId(0), "the first block is token zero");

    // The echoed write comes back over the data channel, exactly like a pane drains
    block.write(b"echo-after-attach\n");
    read_until_marker(&mut block, "echo-after-attach");

    // Layout events travel as wire events too: a mutation answers with a state event. A
    // split is the guaranteed change with a single pane (it also reports its created pane)
    client.send_op(WireOp::Split {
        pane: pane.pane,
        dir: terminal_protocol::layout::Dir::Right,
        shell: wire::WireShell::from_config(&waiting()),
        explicit: false,
        cols: 40,
        rows: 20,
        scrollback: 1000,
        area: Rect::new(0, 0, 800, 600),
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_state = false;
    while Instant::now() < deadline {
        match layout_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(WireEventKind::State { .. }) => {
                saw_state = true;
                break;
            }
            // A created pane followed the state event; keep collecting
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_state, "a state event traveled over the wire");

    // A second connection attaches now: the most recent attach takes over as driver and
    // the first session is demoted (and promoted again when the new driver leaves)
    let (second, read) =
        IpcConn::connect(&IpcEndpoint::Socket(path.clone())).expect("the second client connects");
    let (second_tx, _second_rx) = channel::<WireEventKind>();
    {
        let second = second.clone();
        thread::spawn(move || {
            second.read_loop(read, move |down| match down {
                ControlDown::Event(event) => {
                    let _ = second_tx.send(event);
                }
                ControlDown::Reply(reply) => {
                    panic!("an unexpected reply arrived over the wire: {reply:?}")
                }
            });
        });
    }
    let answer = second
        .attach(&waiting(), 80, 24, 1000)
        .expect("the second attach answers");
    let AttachAnswer::Existing {
        panes: second_panes,
        exited: second_exited,
        ..
    } = answer
    else {
        panic!("an existing layout hands itself over");
    };
    assert_eq!(second_panes.len(), 2, "both live panes travel");
    assert!(second_exited.is_empty(), "nothing exited while attached");
    // The first session was demoted: it receives the DriverChanged event
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut demoted = false;
    while Instant::now() < deadline {
        match layout_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(WireEventKind::DriverChanged { is_driver: false }) => {
                demoted = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        demoted,
        "the first session was told it lost the driver role"
    );

    // The new driver drives: its write reaches the pane
    let (event_tx, _event_rx) = channel::<IoEvent>();
    let event_tx = std::sync::Mutex::new(event_tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = event_tx.lock().unwrap().send(event);
    });
    let mut driver_block = second.open_block(second_panes[0].block, notify);
    driver_block.write(b"echo-from-the-new-driver\n");
    read_until_marker(&mut driver_block, "echo-from-the-new-driver");

    // The demoted session's writes are dropped server-side now
    block.write(b"echo-from-demoted\n");
    {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut seen = String::new();
        while Instant::now() < deadline {
            block.rearm();
            if let Some(chunk) = block.try_read() {
                seen.push_str(&String::from_utf8_lossy(&chunk));
            }
            assert!(
                !seen.contains("echo-from-demoted"),
                "a viewer's write reached the pane: {seen:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    // The new driver leaves: the oldest remaining session is promoted again
    drop(driver_block);
    second.close();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut promoted = false;
    while Instant::now() < deadline {
        match layout_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(WireEventKind::DriverChanged { is_driver: true }) => {
                promoted = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        promoted,
        "the remaining session was promoted when the driver left"
    );
    block.write(b"echo-after-busy\n");
    read_until_marker(&mut block, "echo-after-busy");

    // The working directory ask answers over the wire too
    let _ = client.foreground_dir(pane.block);

    // The client goes away: the session ends, the block keeps running headless, and the
    // next session re-attaches to the same block with its screen
    drop(block);
    client.close();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !shared.sessions.lock().unwrap().is_empty() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        shared.sessions.lock().unwrap().is_empty(),
        "the session ended when the client went away"
    );

    let (client, read) =
        IpcConn::connect(&IpcEndpoint::Socket(path.clone())).expect("the client reconnects");
    {
        let client = client.clone();
        thread::spawn(move || {
            client.read_loop(read, move |down| match down {
                ControlDown::Event(_) => {}
                ControlDown::Reply(reply) => {
                    panic!("an unexpected reply arrived over the wire: {reply:?}")
                }
            });
        });
    }
    let answer = client
        .attach(&waiting(), 80, 24, 1000)
        .expect("the re-attach answers");
    let AttachAnswer::Existing {
        state,
        panes,
        exited,
    } = answer
    else {
        panic!("an existing layout hands itself over");
    };
    assert!(exited.is_empty(), "no pane exited while the sessions ran");
    // Both panes of the split layout are live, in pane order
    assert_eq!(
        Layout::from_state(state).tiled_ids(),
        vec![PaneId(0), PaneId(1)]
    );
    assert_eq!(panes.len(), 2, "both live panes travel");
    assert_eq!(
        panes[0].block,
        BlockId(0),
        "the first block keeps its token"
    );
    assert_eq!(
        panes[1].block,
        BlockId(1),
        "the split's block keeps its token"
    );

    // The snapshot for the re-attached pane arrives on the data channel, before any live
    // chunk, and the first read serves it
    let (event_tx, _event_rx) = channel::<IoEvent>();
    let event_tx = std::sync::Mutex::new(event_tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = event_tx.lock().unwrap().send(event);
    });
    let mut second_block = client.open_block(panes[0].block, notify);
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        second_block.rearm();
        if let Some(bytes) = second_block.try_read() {
            break bytes;
        }
        assert!(
            Instant::now() < deadline,
            "the snapshot never arrived on the data channel"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !snapshot.is_empty(),
        "the re-attached pane replays its screen first"
    );

    // The shell behind the block is still alive (the window never killed it) and answers
    second_block.write(b"echo-after-reattach\n");
    read_until_marker(&mut second_block, "echo-after-reattach");

    drop(second_block);
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Several sessions (stage 4) -------------------------------------------------------
// Two and three clients over one socket: the most recently attached one drives, the
// others watch; take-over, gating and promotion are all server-side. The socket harness
// is unix-only; Windows exercises the same policies through the transport abstraction.

/// The wire over a real named pipe: a window attaches to a fresh server and a second
/// connection (like the automation CLI) attaches to the same session. Every ask needs the
/// server to write while its session reader is parked, which a synchronous pipe handle
/// never allowed (the asks timed out and the window panicked).
#[cfg(windows)]
#[test]
fn a_window_and_a_second_client_attach_over_a_named_pipe() {
    let endpoint = IpcEndpoint::Pipe(format!(
        r"\\.\pipe\terminal-rs-test-{}-attach",
        std::process::id()
    ));
    let (server, layout_events) = Server::start();
    let shared = Arc::new(ProcessShared::default());
    let router_shared = shared.clone();
    thread::spawn(move || route_events(layout_events, router_shared));
    let listener = IpcListener::bind(&endpoint).expect("the endpoint binds");
    let accept_handle = server.clone();
    thread::spawn(move || accept_sessions(listener, accept_handle, shared));

    // The accepting thread creates its first instance on its own schedule
    let connect = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match IpcConn::connect(&endpoint) {
                Ok((conn, read)) => {
                    let reader = conn.clone();
                    thread::spawn(move || reader.read_loop(read, |_| {}));
                    return conn;
                }
                Err(error) if Instant::now() >= deadline => {
                    panic!("the client could not connect: {error}")
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    };

    let window = connect();
    match window
        .attach(&waiting(), 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { .. } => {}
        other => panic!("expected a fresh server, got {other:?}"),
    }
    let second = connect();
    match second
        .attach(&waiting(), 80, 24, 1000)
        .expect("the second attach answers")
    {
        AttachAnswer::Existing { panes, .. } => assert_eq!(panes.len(), 1),
        other => panic!("expected the existing layout, got {other:?}"),
    }
}

#[cfg(unix)]
static HARNESS_SEQ: AtomicU64 = AtomicU64::new(0);

/// A server process for the multi-session tests: its socket endpoint and shared state.
/// The scratch directory is removed on drop.
#[cfg(unix)]
struct Harness {
    path: PathBuf,
    dir: PathBuf,
    shared: Arc<ProcessShared>,
    _server: ServerHandle,
}

#[cfg(unix)]
impl Harness {
    fn start() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "t4-{}-{}",
            HARNESS_SEQ.fetch_add(1, Ordering::Relaxed),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("the test directory is created");
        let path = dir.join("server.sock");
        let (server, layout_events) = Server::start();
        let shared = Arc::new(ProcessShared::default());
        let router_shared = shared.clone();
        thread::spawn(move || route_events(layout_events, router_shared));
        let listener =
            IpcListener::bind(&IpcEndpoint::Socket(path.clone())).expect("the endpoint binds");
        let accept_shared = shared.clone();
        let accept_handle = server.clone();
        thread::spawn(move || accept_sessions(listener, accept_handle, accept_shared));
        Self {
            path,
            dir,
            shared,
            _server: server,
        }
    }

    /// Connects a client and spawns its reader thread: the events land on the channel
    fn client(&self) -> (IpcConn, Receiver<WireEventKind>) {
        let (conn, read) =
            IpcConn::connect(&IpcEndpoint::Socket(self.path.clone())).expect("the client connects");
        let (tx, rx) = channel();
        {
            let conn = conn.clone();
            thread::spawn(move || {
                conn.read_loop(read, move |down| match down {
                    ControlDown::Event(event) => {
                        let _ = tx.send(event);
                    }
                    ControlDown::Reply(reply) => {
                        panic!("an unexpected reply arrived over the wire: {reply:?}")
                    }
                });
            });
        }
        (conn, rx)
    }

    /// Waits until every session ended
    fn wait_no_sessions(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !self.shared.sessions.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            self.shared.sessions.lock().unwrap().is_empty(),
            "the sessions ended"
        );
    }
}

#[cfg(unix)]
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Attaches to a fresh server and returns the created pane
#[cfg(unix)]
fn attach_fresh(conn: &IpcConn) -> wire::IpcCreatedPane {
    match conn
        .attach(&waiting(), 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => pane,
        other => panic!("expected a fresh server, got {other:?}"),
    }
}

/// Attaches to an existing layout and returns the live panes and the exited list
#[cfg(unix)]
fn attach_existing(conn: &IpcConn) -> (Vec<wire::IpcCreatedPane>, Vec<PaneId>) {
    match conn
        .attach(&waiting(), 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Existing { panes, exited, .. } => (panes, exited),
        other => panic!("expected an existing layout, got {other:?}"),
    }
}

/// Builds a pane's block feed over the connection, as the window would
#[cfg(unix)]
fn open_feed(conn: &IpcConn, token: BlockId) -> IpcBlock {
    let (tx, _rx) = channel::<IoEvent>();
    let tx = std::sync::Mutex::new(tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    conn.open_block(token, notify)
}

/// Polls a block's output, failing if `marker` ever shows up (a write the server must
/// have dropped)
#[cfg(unix)]
fn assert_output_never(block: &mut dyn PaneIo, marker: &str) {
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut seen = String::new();
    while Instant::now() < deadline {
        block.rearm();
        if let Some(chunk) = block.try_read() {
            seen.push_str(&String::from_utf8_lossy(&chunk));
            assert!(!seen.contains(marker), "dropped output came back: {seen:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for a `DriverChanged` event with the wanted role, skipping the other events and
/// the opposite role (a session that was demoted earlier still has that event buffered)
#[cfg(unix)]
fn wait_driver_changed(events: &Receiver<WireEventKind>, is_driver: bool) -> bool {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(WireEventKind::DriverChanged { is_driver: got }) if got == is_driver => {
                return true;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    false
}

/// Fails if a promotion (`is_driver: true`) arrives within the window; an already-buffered
/// demotion is skipped
#[cfg(unix)]
fn assert_no_promotion(events: &Receiver<WireEventKind>, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(WireEventKind::DriverChanged { is_driver: true }) => {
                panic!("an unexpected promotion arrived")
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// Waits for the next state event, skipping the other kinds
#[cfg(unix)]
fn wait_wire_state(events: &Receiver<WireEventKind>) -> LayoutState {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(WireEventKind::State { state, .. }) => return state,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    panic!("no state event arrived");
}

/// Fails if a state event arrives within the window (a mutation the server must have
/// dropped); the other kinds are skipped
#[cfg(unix)]
fn assert_no_state_event(events: &Receiver<WireEventKind>, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(WireEventKind::State { .. }) => {
                panic!("a mutation the server should have dropped was applied")
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

/// A layout resize carrying `area`, the op that stores the driver's window area
#[cfg(unix)]
fn resize_op(pane: PaneId, area: Rect) -> WireOp {
    WireOp::Resize {
        pane,
        dir: Dir::Right,
        delta: 10,
        area,
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    }
}

#[test]
#[cfg(unix)]
fn a_second_client_takes_over_as_driver_and_the_first_watches() {
    let harness = Harness::start();
    let (first, first_events) = harness.client();
    let _pane = attach_fresh(&first);

    // The most recent attach takes over: the second client drives, the first is demoted
    let (second, _second_events) = harness.client();
    let (panes, exited) = attach_existing(&second);
    assert_eq!(panes.len(), 1);
    assert!(
        exited.is_empty(),
        "nothing exited while the first was attached"
    );
    assert!(
        wait_driver_changed(&first_events, false),
        "the first session was told it lost the driver role"
    );

    // The driver's write reaches the pane
    let mut driver_block = open_feed(&second, panes[0].block);
    driver_block.write(b"echo-from-the-driver\n");
    read_until_marker(&mut driver_block, "echo-from-the-driver");

    // The viewer's write is dropped: nothing echoes back
    let mut viewer_block = open_feed(&first, panes[0].block);
    viewer_block.write(b"echo-from-the-viewer\n");
    assert_output_never(&mut viewer_block, "echo-from-the-viewer");

    // And the driver still gets its echo afterwards
    driver_block.write(b"echo-from-the-driver-2\n");
    read_until_marker(&mut driver_block, "echo-from-the-driver-2");
    drop(viewer_block);
    drop(driver_block);
    // An explicit close: dropping the connection cannot end the session, the client's own
    // reader thread still holds a clone of it
    first.close();
    second.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn taking_over_drops_the_previous_driver_window_area() {
    let harness = Harness::start();
    let (first, _first_events) = harness.client();
    attach_fresh(&first);

    // The driver's layout op stores its window area
    let area_a = Rect::new(1, 2, 300, 400);
    first.send_op(resize_op(PaneId(0), area_a));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && *harness.shared.driver_area.lock().unwrap() != Some(area_a) {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        *harness.shared.driver_area.lock().unwrap(),
        Some(area_a),
        "the driver's layout op stored its window area"
    );

    // The take-over drops it, until the new driver sends its own
    let (second, _second_events) = harness.client();
    attach_existing(&second);
    assert_eq!(
        *harness.shared.driver_area.lock().unwrap(),
        None,
        "the demoted driver's area was dropped"
    );

    // The new driver stores its own; the viewer's op changes nothing
    let area_b = Rect::new(0, 0, 500, 500);
    second.send_op(resize_op(PaneId(0), area_b));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && *harness.shared.driver_area.lock().unwrap() != Some(area_b) {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(*harness.shared.driver_area.lock().unwrap(), Some(area_b));
    first.send_op(resize_op(PaneId(0), Rect::new(9, 9, 999, 999)));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        *harness.shared.driver_area.lock().unwrap(),
        Some(area_b),
        "a viewer's layout op was dropped"
    );
}

#[test]
#[cfg(unix)]
fn layout_changes_reach_every_session_and_viewer_mutations_are_dropped() {
    let harness = Harness::start();
    let (first, first_events) = harness.client();
    attach_fresh(&first);
    let (second, second_events) = harness.client();
    attach_existing(&second);

    // The driver splits: both sessions rebuild from the same state event
    second.send_op(WireOp::Split {
        pane: PaneId(0),
        dir: Dir::Right,
        shell: WireShell::from_config(&waiting()),
        explicit: false,
        cols: 40,
        rows: 20,
        scrollback: 1000,
        area: Rect::new(0, 0, 800, 600),
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    });
    let first_state = wait_wire_state(&first_events);
    assert_eq!(
        Layout::from_state(first_state.clone()).tiled_ids(),
        vec![PaneId(0), PaneId(1)]
    );
    let second_state = wait_wire_state(&second_events);
    assert_eq!(
        second_state, first_state,
        "both sessions saw the same change"
    );

    // The viewer's split is dropped: no session sees another state event
    first.send_op(WireOp::Split {
        pane: PaneId(0),
        dir: Dir::Right,
        shell: WireShell::from_config(&waiting()),
        explicit: false,
        cols: 40,
        rows: 20,
        scrollback: 1000,
        area: Rect::new(0, 0, 800, 600),
        limits: Limits {
            min_w: 80,
            min_h: 60,
        },
    });
    assert_no_state_event(&first_events, Duration::from_millis(600));
    assert_no_state_event(&second_events, Duration::from_millis(600));
}

#[test]
#[cfg(unix)]
fn a_driver_disconnect_promotes_the_oldest_remaining_session() {
    let harness = Harness::start();
    let (first, first_events) = harness.client();
    let pane = attach_fresh(&first);
    let (second, second_events) = harness.client();
    attach_existing(&second);
    let (third, _third_events) = harness.client();
    attach_existing(&third);

    // The driver leaves: the OLDEST remaining session (the first) is promoted, not the
    // second, and nobody else is told
    third.close();
    assert!(
        wait_driver_changed(&first_events, true),
        "the oldest remaining session was promoted"
    );
    assert_no_promotion(&second_events, Duration::from_millis(500));

    // The promoted session drives: its write reaches the pane
    let mut block = open_feed(&first, pane.block);
    block.write(b"echo-after-promotion\n");
    read_until_marker(&mut block, "echo-after-promotion");
    drop(block);
    // An explicit close: dropping the connection cannot end the session (see the take-over
    // test)
    third.close();
    second.close();
    first.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn the_attach_reply_names_panes_that_exited_while_detached() {
    let harness = Harness::start();
    // The first client attaches with a program that exits right away: it sees the exit
    // live, and the pane stays (its block is kept)
    let (first, first_events) = harness.client();
    let dying = running("echo dying-marker", "echo dying-marker");
    match first
        .attach(&dying, 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => assert_eq!(pane.pane, PaneId(0)),
        other => panic!("expected a fresh server, got {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_exit = false;
    while Instant::now() < deadline {
        match first_events.recv_timeout(Duration::from_millis(200)) {
            Ok(WireEventKind::PaneExited { .. }) => {
                saw_exit = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_exit, "the exit reached the attached session live");

    first.close();
    harness.wait_no_sessions();

    // The next attach names the exited pane, and clears the list once delivered
    let (second, _second_events) = harness.client();
    let (panes, exited) = attach_existing(&second);
    assert_eq!(panes.len(), 1, "the held pane still travels");
    assert_eq!(
        exited,
        vec![PaneId(0)],
        "the exit while detached is delivered once"
    );
    let (third, _third_events) = harness.client();
    let (_, exited) = attach_existing(&third);
    assert!(exited.is_empty(), "the list is cleared once delivered");
}

// ---- WebSocket gateway (stage 5a) ------------------------------------------------------

/// A loopback TCP pair: the server end wrapped in a `Ws`, the client end raw (tests write
/// masked frames on it, exactly what a WebSocket client must send)
fn ws_pair() -> (ws::WsReader, ws::WsWriter, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
    let addr = listener.local_addr().expect("the address is known");
    let client = TcpStream::connect(addr).expect("the loopback client connects");
    let (server, _) = listener.accept().expect("the loopback server accepts");
    let (reader, writer) = ws::Ws::from_stream(server).expect("the websocket opens");
    (reader, writer, client)
}

/// A masked client frame (opcode, fin, payload): what a WebSocket client must send
fn masked_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![(if fin { 0x80 } else { 0x00 }) | opcode];
    match payload.len() {
        0..=125 => frame.push(0x80 | payload.len() as u8),
        126..=0xFFFF => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[index % 4]);
    }
    frame
}

/// Reads one unmasked server frame off the client end: its opcode and payload
fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream
        .read_exact(&mut head)
        .expect("the frame head arrives");
    let opcode = head[0] & 0x0f;
    let len = match head[1] & 0x7f {
        126 => {
            let mut extended = [0u8; 2];
            stream
                .read_exact(&mut extended)
                .expect("the length arrives");
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            stream
                .read_exact(&mut extended)
                .expect("the length arrives");
            u64::from_be_bytes(extended) as usize
        }
        len => len as usize,
    };
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .expect("the payload arrives");
    (opcode, payload)
}

/// Sends a WebSocket binary message from the test client: a masked frame over `bytes`
fn ws_send(stream: &mut TcpStream, bytes: &[u8]) {
    stream
        .write_all(&masked_frame(0x2, true, bytes))
        .expect("the frame goes out");
}

/// Sends a control message from the test client: the 1-byte type prefix, then the JSON
#[cfg(unix)]
fn ws_send_control(stream: &mut TcpStream, message: &impl serde::Serialize) {
    let json = serde_json::to_vec(message).expect("serializes");
    let mut message = vec![FRAME_CONTROL];
    message.extend_from_slice(&json);
    ws_send(stream, &message);
}

/// Reads one WebSocket binary message off the test client (the server never fragments out);
/// `None` when nothing arrived within the read timeout
#[cfg(unix)]
fn ws_recv_maybe(stream: &mut TcpStream) -> Option<Vec<u8>> {
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("the read timeout is set");
    let mut head = [0u8; 2];
    match stream.read_exact(&mut head) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return None;
        }
        Err(error) => panic!("the server frame failed: {error}"),
    }
    let opcode = head[0] & 0x0f;
    let len = match head[1] & 0x7f {
        126 => {
            let mut extended = [0u8; 2];
            stream
                .read_exact(&mut extended)
                .expect("the length arrives");
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            stream
                .read_exact(&mut extended)
                .expect("the length arrives");
            u64::from_be_bytes(extended) as usize
        }
        len => len as usize,
    };
    // Once the head arrived the payload follows whole: clear the timeout so a slow chunk
    // cannot lose the frame's tail
    stream
        .set_read_timeout(None)
        .expect("the read timeout is cleared");
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .expect("the payload arrives");
    assert_eq!(opcode, 0x2, "the server sends binary frames");
    Some(payload)
}

/// Reads WebSocket messages until one satisfies `wanted`, with a deadline
#[cfg(unix)]
fn ws_recv_until(stream: &mut TcpStream, mut wanted: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(payload) = ws_recv_maybe(stream)
            && wanted(&payload)
        {
            return payload;
        }
    }
    panic!("the websocket message never arrived");
}

/// Reads the reply with correlation id `rid`, skipping anything else
#[cfg(unix)]
fn control_reply(stream: &mut TcpStream, rid: u64) -> wire::WireReply {
    let payload = ws_recv_until(stream, |payload| {
        payload.first() == Some(&FRAME_CONTROL)
            && serde_json::from_slice::<wire::WireReply>(&payload[1..])
                .map(|reply| reply.rid == rid)
                .unwrap_or(false)
    });
    serde_json::from_slice(&payload[1..]).expect("the reply decodes")
}

/// Sends the upgrade request (with an identity header when `login` is set) and returns
/// the client stream plus the response head
fn ws_upgrade(addr: std::net::SocketAddr, login: Option<&str>) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).expect("the remote client connects");
    let mut request = String::from(
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n",
    );
    if let Some(login) = login {
        request.push_str(&format!("Tailscale-User-Login: {login}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request goes out");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("the response arrives");
        head.push(byte[0]);
    }
    (stream, String::from_utf8_lossy(&head).into_owned())
}

/// A gateway accept loop in front of the harness server: loopback port and allow list
#[cfg(unix)]
fn start_gateway(harness: &Harness, allow: &[&str]) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the gateway listener binds");
    let addr = listener.local_addr().expect("the gateway address is known");
    let allow: Vec<String> = allow.iter().map(|login| login.to_string()).collect();
    let endpoint = IpcEndpoint::Socket(harness.path.clone());
    thread::spawn(move || accept_gateway(listener, endpoint, allow));
    addr
}

#[test]
fn the_websocket_accept_matches_the_rfc_example() {
    assert_eq!(
        ws::accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
fn ws_frames_round_trip_masked_in_and_unmasked_out() {
    let (mut reader, writer, mut client) = ws_pair();
    writer.write_binary(b"out").expect("the write goes out");
    let (opcode, payload) = read_server_frame(&mut client);
    assert_eq!((opcode, payload.as_slice()), (0x2, b"out".as_slice()));

    ws_send(&mut client, b"in");
    assert_eq!(
        reader.read_message().expect("the message arrives"),
        ws::Message::Binary(b"in".to_vec())
    );

    // A client frame that is not masked is a protocol error (RFC 6455 5.1)
    client
        .write_all(&[0x82, 0x01, b'x'])
        .expect("the raw frame goes out");
    assert!(
        reader.read_message().is_err(),
        "unmasked frames are refused"
    );
}

#[test]
fn ws_joins_fragmented_binary_messages_and_refuses_strays() {
    let (mut reader, _writer, mut client) = ws_pair();
    client
        .write_all(&masked_frame(0x2, false, b"frag"))
        .expect("the first fragment goes out");
    client
        .write_all(&masked_frame(0x0, false, b"ment"))
        .expect("the continuation goes out");
    client
        .write_all(&masked_frame(0x0, true, b"ed"))
        .expect("the last fragment goes out");
    assert_eq!(
        reader.read_message().expect("the joined message arrives"),
        ws::Message::Binary(b"fragmented".to_vec())
    );

    // A continuation with no message in progress is a protocol error
    client
        .write_all(&masked_frame(0x0, true, b"stray"))
        .expect("the stray goes out");
    assert!(
        reader.read_message().is_err(),
        "a stray continuation is refused"
    );
}

#[test]
fn ws_answers_pings_and_echoes_closes() {
    let (mut reader, _writer, mut client) = ws_pair();
    client
        .write_all(&masked_frame(0x9, true, b"xy"))
        .expect("the ping goes out");
    ws_send(&mut client, b"data");
    // Reading the next message answers the ping in place (before it returns the binary)
    assert_eq!(
        reader.read_message().expect("the message arrives"),
        ws::Message::Binary(b"data".to_vec())
    );
    let (opcode, payload) = read_server_frame(&mut client);
    assert_eq!((opcode, payload.as_slice()), (0xA, b"xy".as_slice()));

    // A close is echoed once and reported
    client
        .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
        .expect("the close goes out");
    assert_eq!(
        reader.read_message().expect("the close arrives"),
        ws::Message::Close(Some(1000))
    );
    let (opcode, payload) = read_server_frame(&mut client);
    assert_eq!(
        (opcode, payload.as_slice()),
        (0x8, 1000u16.to_be_bytes().as_slice())
    );
}

#[test]
fn ws_refuses_text_frames() {
    let (mut reader, _writer, mut client) = ws_pair();
    client
        .write_all(&masked_frame(0x1, true, b"text"))
        .expect("the text frame goes out");
    let error = reader
        .read_message()
        .expect_err("text frames are refused (binary only)");
    assert!(error.to_string().contains("text"), "{error}");
}

#[test]
fn the_ws_handshake_computes_the_accept_and_reads_the_identity_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
    let addr = listener.local_addr().expect("the address is known");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the handshake connects");
        let request = ws::read_upgrade_request(&mut stream).expect("the request parses");
        assert_eq!(request.login.as_deref(), Some("dev@example.com"));
        let key = request.key.expect("the key travels");
        ws::write_switching(&stream, &key).expect("the switch goes out");
    });
    let (client, head) = ws_upgrade(addr, Some("dev@example.com"));
    assert!(head.contains("101 Switching Protocols"), "{head}");
    assert!(
        head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{head}"
    );
    let _ = client.shutdown(std::net::Shutdown::Both);
}

#[test]
#[cfg(unix)]
fn a_remote_client_over_websocket_attaches_writes_and_reads_back() {
    let harness = Harness::start();
    let addr = start_gateway(&harness, &["dev@example.com"]);

    let (mut stream, head) = ws_upgrade(addr, Some("dev@example.com"));
    assert!(head.contains("101 Switching Protocols"), "{head}");
    // The gateway names who authenticated (rid 0: gateway-initiated, no ask)
    let hello = control_reply(&mut stream, 0);
    assert_eq!(
        hello.reply,
        WireReplyKind::Hello {
            login: "dev@example.com".to_string()
        }
    );

    // Attach exactly as a local window does
    ws_send_control(
        &mut stream,
        &WireRequest {
            v: WIRE_VERSION,
            rid: Some(1),
            op: WireOp::Attach {
                shell: WireShell::from_config(&running("cat", "more")),
                cols: 80,
                rows: 24,
                scrollback: 1000,
            },
        },
    );
    let reply = control_reply(&mut stream, 1);
    let WireReplyKind::Init { state, created } = reply.reply else {
        panic!("a fresh server attaches like init: {:?}", reply.reply);
    };
    let pane = created.expect("the first pane travels");
    assert_eq!(state.focus, pane.pane);

    // The write reaches the pane, and the echo streams back as a data frame
    ws_send_control(
        &mut stream,
        &WireRequest {
            v: WIRE_VERSION,
            rid: None,
            op: WireOp::Write {
                block: pane.block,
                bytes: b"echo-over-ws\n".to_vec(),
            },
        },
    );
    let echo = ws_recv_until(&mut stream, |payload| {
        payload.first() == Some(&FRAME_DATA)
            && payload.get(1..5) == Some(&(pane.block.0 as u32).to_le_bytes())
            && String::from_utf8_lossy(&payload[5..]).contains("echo-over-ws")
    });
    assert!(
        String::from_utf8_lossy(&echo[5..]).contains("echo-over-ws"),
        "{echo:?}"
    );

    // Events fan out too: a split answers with a state event over the websocket
    ws_send_control(
        &mut stream,
        &WireRequest {
            v: WIRE_VERSION,
            rid: Some(2),
            op: WireOp::Split {
                pane: pane.pane,
                dir: terminal_protocol::layout::Dir::Right,
                shell: WireShell::from_config(&waiting()),
                explicit: false,
                cols: 40,
                rows: 20,
                scrollback: 1000,
                area: Rect::new(0, 0, 800, 600),
                limits: Limits {
                    min_w: 80,
                    min_h: 60,
                },
            },
        },
    );
    let event = ws_recv_until(&mut stream, |payload| {
        payload.first() == Some(&FRAME_CONTROL)
            && serde_json::from_slice::<wire::WireEvent>(&payload[1..])
                .map(|event| matches!(event.event, WireEventKind::State { .. }))
                .unwrap_or(false)
    });
    let event: wire::WireEvent = serde_json::from_slice(&event[1..]).expect("decodes");
    assert!(matches!(event.event, WireEventKind::State { .. }));

    // The bridge ends with the websocket: the close handshake ends the server session
    stream
        .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
        .expect("the close goes out");
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn the_gateway_refuses_logins_that_remote_allow_does_not_name() {
    let harness = Harness::start();
    let addr = start_gateway(&harness, &["dev@example.com"]);

    for login in [Some("stranger@example.com"), None] {
        let (_stream, head) = ws_upgrade(addr, login);
        assert!(head.contains("403 Forbidden"), "{login:?}: {head}");
        assert!(!head.contains("101"), "{login:?}: {head}");
    }
    harness.wait_no_sessions();

    // An empty allow list denies everyone
    let addr = start_gateway(&harness, &[]);
    let (_stream, head) = ws_upgrade(addr, Some("dev@example.com"));
    assert!(head.contains("403 Forbidden"), "{head}");
}

#[test]
#[cfg(unix)]
fn a_second_remote_client_is_refused_while_one_is_active() {
    let harness = Harness::start();
    let addr = start_gateway(&harness, &["dev@example.com"]);

    let (mut first, first_head) = ws_upgrade(addr, Some("dev@example.com"));
    assert!(first_head.contains("101"), "{first_head}");
    let _hello = control_reply(&mut first, 0);

    // The second connection upgrades, gets the Busy event, and closes right after
    let (mut second, second_head) = ws_upgrade(addr, Some("dev@example.com"));
    assert!(second_head.contains("101"), "{second_head}");
    let busy = ws_recv_until(&mut second, |payload| {
        payload.first() == Some(&FRAME_CONTROL)
    });
    let busy: WireEvent = serde_json::from_slice(&busy[1..]).expect("the busy event decodes");
    assert!(matches!(busy.event, WireEventKind::Busy));
    let (opcode, _payload) = read_server_frame(&mut second);
    assert_eq!(opcode, 0x8, "the busy client is closed right after");
    assert_eq!(
        second.read(&mut [0u8; 1]).expect("the socket closes"),
        0,
        "nothing follows the close"
    );

    // The first remote client is unaffected
    ws_send_control(
        &mut first,
        &WireRequest {
            v: WIRE_VERSION,
            rid: Some(1),
            op: WireOp::Attach {
                shell: WireShell::from_config(&waiting()),
                cols: 80,
                rows: 24,
                scrollback: 1000,
            },
        },
    );
    let reply = control_reply(&mut first, 1);
    assert!(matches!(reply.reply, WireReplyKind::Init { .. }));
    // The gateway slot frees up when the websocket closes
    first
        .write_all(&masked_frame(0x8, true, &1000u16.to_be_bytes()))
        .expect("the close goes out");
    harness.wait_no_sessions();
}

// --- Stage 6: the automation CLI ---------------------------------------------------------

#[test]
#[cfg(unix)]
fn the_automation_cli_lists_panes() {
    let harness = Harness::start();
    // A fresh server has no panes to list
    let (cli, _cli_events) = harness.client();
    assert!(
        cli.automation_list().expect("the list answers").is_empty(),
        "a fresh server lists no panes"
    );
    // After the window attaches, the pane is listed with its block token, grid and state
    let (window, _window_events) = harness.client();
    let pane = attach_fresh(&window);
    let panes = cli.automation_list().expect("the list answers");
    assert_eq!(panes.len(), 1, "the live pane is listed");
    assert_eq!(panes[0].pane, pane.pane);
    assert_eq!(panes[0].block, pane.block);
    assert_eq!((panes[0].cols, panes[0].rows), (80, 24));
    assert!(panes[0].alive, "the held program is alive");
    // The CLI never attached: the ask must not disturb the window's session
    window.close();
    cli.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn the_automation_cli_send_bypasses_the_driver_gate_and_reaches_the_program() {
    let harness = Harness::start();
    let (window, _window_events) = harness.client();
    let echo = running("cat", "more");
    let pane = match window
        .attach(&echo, 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => pane,
        other => panic!("expected a fresh server, got {other:?}"),
    };
    let mut block = open_feed(&window, pane.block);
    // The CLI session never attaches, so it is never the driver; its automation send is
    // served anyway (a plain Write from the same session would be dropped by the gate)
    let (cli, _cli_events) = harness.client();
    cli.automation_send_checked(pane.block, b"echo-from-cli\n")
        .expect("the send is confirmed");
    read_until_marker(&mut block, "echo-from-cli");
    // An unknown block is refused with an error naming it
    let error = cli
        .automation_send_checked(BlockId(999), b"echo-nowhere\n")
        .expect_err("an unknown block is refused");
    assert!(error.contains("999"), "the error names the block: {error}");
    // The window still drives: its write reaches the pane after the CLI traffic
    block.write(b"echo-still-driver\n");
    read_until_marker(&mut block, "echo-still-driver");
    cli.close();
    window.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn a_dead_pane_reports_dead_and_refuses_sends() {
    let harness = Harness::start();
    let (window, window_events) = harness.client();
    let dying = running("echo dying-marker", "echo dying-marker");
    let pane = match window
        .attach(&dying, 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => pane,
        other => panic!("expected a fresh server, got {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_exit = false;
    while Instant::now() < deadline {
        match window_events.recv_timeout(Duration::from_millis(200)) {
            Ok(WireEventKind::PaneExited { .. }) => {
                saw_exit = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_exit, "the exit reached the window");
    // The list reports the pane dead, and sends to it are refused
    let (cli, _cli_events) = harness.client();
    let panes = cli.automation_list().expect("the list answers");
    assert_eq!(panes.len(), 1, "the held pane is still listed");
    assert!(!panes[0].alive, "the exited pane reports dead");
    let error = cli
        .automation_send_checked(panes[0].block, b"echo-ghost\n")
        .expect_err("a dead pane refuses sends");
    assert!(error.contains("exited"), "the error explains why: {error}");
    // The held screen is still readable: capture answers the plain text (the CLI checks
    // alive client-side and errors there; the server keeps serving the held screen)
    let text = cli
        .automation_capture(pane.block)
        .expect("the capture answers");
    assert!(
        text.contains("dying-marker"),
        "the screen is still captured: {text:?}"
    );
    cli.close();
    window.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn the_automation_cli_capture_matches_the_plain_screen() {
    let harness = Harness::start();
    let (window, _window_events) = harness.client();
    let program = running(
        "printf 'capture-line\\ncapture-marker'",
        "echo capture-line & echo capture-marker",
    );
    let pane = match window
        .attach(&program, 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => pane,
        other => panic!("expected a fresh server, got {other:?}"),
    };
    let mut block = open_feed(&window, pane.block);
    let output = read_until_marker(&mut block, "capture-marker");
    // The expectation: the same output, rendered in a fresh terminal, plain-formatted
    let mut expected = terminal();
    expected.vt_write(&output);
    let (cli, _cli_events) = harness.client();
    let text = cli
        .automation_capture(pane.block)
        .expect("the capture answers");
    assert_eq!(
        text,
        plain_text(&expected),
        "the capture is the plain exported screen"
    );
    // The raw snapshot (`capture --vt`) rebuilds the same screen
    let vt = cli.snapshot(pane.block).expect("the snapshot answers");
    let mut restored = terminal();
    restored.vt_write(&vt);
    assert_eq!(plain_text(&restored), plain_text(&expected));
    cli.close();
    window.close();
    harness.wait_no_sessions();
}

#[test]
#[cfg(unix)]
fn attach_streams_the_snapshot_then_live_bytes() {
    let harness = Harness::start();
    let (window, _window_events) = harness.client();
    let echo = running("cat", "more");
    let pane = match window
        .attach(&echo, 80, 24, 1000)
        .expect("the attach answers")
    {
        AttachAnswer::Fresh { pane, .. } => pane,
        other => panic!("expected a fresh server, got {other:?}"),
    };
    let mut win_block = open_feed(&window, pane.block);
    win_block.write(b"seed-screen\n");
    read_until_marker(&mut win_block, "seed-screen");
    // The CLI subscribes to the block's data channel: the snapshot is served first
    let (cli, _cli_events) = harness.client();
    cli.automation_subscribe(pane.block)
        .expect("the subscribe answers");
    let mut cli_block = open_feed(&cli, pane.block);
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        cli_block.rearm();
        match cli_block.try_read() {
            Some(chunk) => break chunk,
            None if Instant::now() > deadline => panic!("the snapshot never arrived"),
            None => thread::sleep(Duration::from_millis(20)),
        }
    };
    assert!(
        String::from_utf8_lossy(&snapshot).contains("seed-screen"),
        "the snapshot is served first: {:?}",
        String::from_utf8_lossy(&snapshot)
    );
    // ...then live bytes follow, and the window still sees its own data
    cli.automation_send(pane.block, b"echo-live\n");
    read_until_marker(&mut cli_block, "echo-live");
    read_until_marker(&mut win_block, "echo-live");
    cli.close();
    window.close();
    harness.wait_no_sessions();
}
