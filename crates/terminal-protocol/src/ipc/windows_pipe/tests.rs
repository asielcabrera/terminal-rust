use super::*;
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

/// A pipe name no other test (or server) uses
fn unique_name(tag: &str) -> String {
    format!(r"\\.\pipe\terminal-rs-test-{}-{tag}", std::process::id())
}

/// Connects to `name` once the accepting thread has created its instance
fn connect_when_listening(name: &str) -> (PipeReader, PipeWriter) {
    let deadline = Instant::now() + LIMIT;
    loop {
        match connect_client(name) {
            Ok(halves) => return halves,
            Err(error) if Instant::now() >= deadline => {
                panic!("the client could not connect: {error}")
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A connected pair: the client's halves and the server's
fn connected_pair(tag: &str) -> ((PipeReader, PipeWriter), (PipeReader, PipeWriter)) {
    let name = unique_name(tag);
    let listener = PipeListener::bind(&name).expect("the name is free");
    let server = thread::spawn(move || listener.accept().expect("a client connects"));
    let client = connect_when_listening(&name);
    (client, server.join().expect("the accept thread ends"))
}

/// Reads exactly `len` bytes on its own thread, so a deadlock fails the test instead of
/// hanging it
fn read_in_background(mut read: PipeReader, len: usize) -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut bytes = vec![0u8; len];
        if read.read_exact(&mut bytes).is_ok() {
            let _ = tx.send(bytes);
        }
    });
    rx
}

#[test]
fn a_parked_read_does_not_hold_back_a_write_on_the_same_end() {
    let ((client_read, mut client_write), (mut server_read, mut server_write)) =
        connected_pair("duplex");
    // The session's reader parks on the server end, waiting for the next request...
    let parked = thread::spawn(move || {
        let mut request = [0u8; 4];
        server_read.read_exact(&mut request).map(|()| request)
    });
    thread::sleep(Duration::from_millis(200));
    // ...and the server must still answer, or the client's ask would time out
    let reply = read_in_background(client_read, 5);
    thread::spawn(move || {
        server_write
            .write_all(b"reply")
            .expect("the reply is written");
        server_write.flush().expect("the reply is flushed");
        server_write
    });
    assert_eq!(
        reply.recv_timeout(LIMIT).expect("the reply arrives"),
        b"reply"
    );
    // The same holds for the client's own end, which parks in its reader loop
    client_write
        .write_all(b"ping")
        .expect("the request is written");
    client_write.flush().expect("the request is flushed");
    assert_eq!(
        parked
            .join()
            .expect("the parked read ends")
            .expect("it reads the request"),
        *b"ping"
    );
}

#[test]
fn a_closed_peer_ends_the_pending_read_instead_of_hanging_it() {
    let (client, (mut server_read, _server_write)) = connected_pair("closed");
    let parked = thread::spawn(move || {
        let mut byte = [0u8; 1];
        server_read.read(&mut byte)
    });
    thread::sleep(Duration::from_millis(200));
    drop(client);
    assert!(
        parked.join().expect("the parked read ends").is_err(),
        "a broken pipe is reported as an error"
    );
}

#[test]
fn a_second_client_connects_while_the_first_stays_attached() {
    let name = unique_name("two");
    let listener = PipeListener::bind(&name).expect("the name is free");
    let (accepted_tx, accepted_rx) = channel();
    thread::spawn(move || {
        // Like the accept loop: an instance per connection, one after another
        for _ in 0..2 {
            accepted_tx
                .send(listener.accept().expect("a client connects"))
                .expect("the test still listens");
        }
    });
    let first = connect_when_listening(&name);
    let second = connect_when_listening(&name);
    let _first_server = accepted_rx
        .recv_timeout(LIMIT)
        .expect("the first is accepted");
    let (_, mut second_server_write) = accepted_rx
        .recv_timeout(LIMIT)
        .expect("the second is accepted");
    second_server_write
        .write_all(b"hi")
        .expect("the second client is served");
    assert_eq!(
        read_in_background(second.0, 2)
            .recv_timeout(LIMIT)
            .expect("the second client hears the server"),
        b"hi"
    );
    drop(first);
}
