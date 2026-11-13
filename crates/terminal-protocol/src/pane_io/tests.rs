use super::*;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

fn shell(program: &str, args: &[&str]) -> ShellConfig {
    ShellConfig {
        program: Some(program.to_string()),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        integration: false,
        ..ShellConfig::default()
    }
}

/// A program that prints `text` and exits, on every system
fn printing(text: &str) -> ShellConfig {
    if cfg!(windows) {
        shell("cmd", &["/C", &format!("echo {text}")])
    } else {
        shell("sh", &["-c", &format!("echo {text}")])
    }
}

/// A program that waits until it is killed
fn waiting() -> ShellConfig {
    if cfg!(windows) {
        shell("cmd", &["/C", "ping -n 30 127.0.0.1 >NUL"])
    } else {
        shell("sh", &["-c", "sleep 30"])
    }
}

fn events() -> (Notify, Receiver<IoEvent>) {
    let (tx, rx) = channel();
    let tx = std::sync::Mutex::new(tx);
    let notify: Notify = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    (notify, rx)
}

/// Reads output until `pending` returns false or 10 s pass
fn read_until(io: &mut LocalPty, mut pending: impl FnMut(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    while pending(&String::from_utf8_lossy(&output)) && Instant::now() < deadline {
        io.rearm();
        match io.try_read() {
            Some(chunk) => {
                // ConPTY asks for the cursor position at startup and shows nothing until the
                // terminal answers; in the app libghostty answers, here the test does
                if chunk.windows(4).any(|query| query == b"\x1b[6n") {
                    io.write(b"\x1b[1;1R");
                }
                output.extend(chunk);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    String::from_utf8_lossy(&output).into_owned()
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

#[test]
fn the_program_output_arrives_and_its_exit_is_reported() {
    let (notify, rx) = events();
    let (mut io, notice) = LocalPty::spawn(&printing("hello-pane-io"), true, 80, 24, notify)
        .expect("the program starts");
    assert_eq!(notice, None);
    let output = read_until(&mut io, |text| !text.contains("hello-pane-io"));
    assert!(output.contains("hello-pane-io"), "{output:?}");
    assert!(wait_for(&rx, IoEvent::Exited), "the exit is reported");
}

#[test]
fn output_wakes_the_event_loop() {
    let (notify, rx) = events();
    let (_io, _) = LocalPty::spawn(&printing("wake"), true, 80, 24, notify).unwrap();
    assert!(wait_for(&rx, IoEvent::Output));
}

#[test]
fn notify_again_sends_another_output_event() {
    let (notify, rx) = events();
    let (io, _) = LocalPty::spawn(&waiting(), true, 80, 24, notify).unwrap();
    while rx.try_recv().is_ok() {}
    io.notify_again();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(IoEvent::Output));
}

#[test]
fn killing_the_program_reports_its_exit() {
    let (notify, rx) = events();
    let (mut io, _) = LocalPty::spawn(&waiting(), true, 80, 24, notify).unwrap();
    io.kill();
    assert!(wait_for(&rx, IoEvent::Exited));
}

#[test]
fn an_explicit_program_that_cannot_start_is_an_error() {
    let (notify, _rx) = events();
    let error = LocalPty::spawn(&shell("no-such-program-pane-io", &[]), true, 80, 24, notify)
        .err()
        .expect("it does not start");
    assert!(
        error.starts_with("could not start \"no-such-program-pane-io\": "),
        "{error}"
    );
}

#[test]
fn a_configured_shell_that_cannot_start_falls_back_to_the_default_shell() {
    let (notify, _rx) = events();
    let (mut io, notice) = LocalPty::spawn(
        &shell("no-such-program-pane-io", &[]),
        false,
        80,
        24,
        notify,
    )
    .expect("the default shell starts");
    let notice = notice.expect("a notice explains the fallback");
    assert!(
        notice.starts_with("could not start \"no-such-program-pane-io\": "),
        "{notice}"
    );
    assert!(notice.ends_with("; using the default shell"), "{notice}");
    io.kill();
}
