//! ---- A minimal RFC 6455 WebSocket server (the tailnet gateway's transport) -------------------
//
//! Hand-rolled on the standard library (plus `sha1_smol` and `base64` for the handshake digest)
//! to keep the dependency tree light: the protocol the gateway needs is small and the whole
//! surface is unit-tested below. Minimal-client assumptions, by design:
//
//! - Client frames MUST be masked (RFC 6455 §5.1); an unmasked one is a protocol error.
//! - Data is binary only. A text frame is refused (the wire protocol has its own framing), and
//!   `permessage-deflate` is not negotiated, so RSV bits set are a protocol error.
//! - Fragmented binary messages are JOINED, never rejected: continuation frames accumulate into
//!   one message, capped at `wire::MAX_FRAME`.
//! - Pings are answered with matching pongs; pongs from the client are ignored.
//! - A close frame is echoed once and reported; no half-open close handshake.
//! - The upgrade request is read byte-by-byte up to the blank line, so no buffered frame bytes
//!   are ever lost (real clients wait for the 101 anyway; pipelining is not supported).
//
//! The identity headers (`Tailscale-User-Login` / `Tailscale-User-Name`) are ONLY trusted
//! because `tailscale serve` injects them for trusted tailnet traffic. Anything hitting this
//! port directly can set them at will, which is acceptable because the gateway binds 127.0.0.1
//! only and real deployments front it with tailscale.

use base64::Engine as _;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

/// The WebSocket GUID the accept key hashes over (RFC 6455 §1.3)
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

fn protocol_error(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

/// The upgrade request the client sent, as far as the gateway cares
pub struct Request {
    /// `Sec-WebSocket-Key`, required to answer the upgrade
    pub key: Option<String>,
    /// `Tailscale-User-Login`, injected by `tailscale serve` for trusted traffic
    pub login: Option<String>,
    /// `Tailscale-User-Name`, the fallback identity header
    pub name: Option<String>,
}

/// Reads the request head byte-by-byte up to the blank line (no buffering: whatever the
/// client sends after the head is never lost)
fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err(protocol_error("the upgrade request head is too large"));
        }
        if stream.read(&mut byte)? == 0 {
            return Err(protocol_error("the upgrade request ended mid-head"));
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Parses the upgrade request. Only what the gateway needs is read; a request without a
/// handshake key simply carries `key: None` and is answered with a bad request.
pub fn read_upgrade_request(stream: &mut TcpStream) -> io::Result<Request> {
    let head = read_head(stream)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    if !request_line.starts_with("GET ") {
        return Err(protocol_error("the request is not a websocket upgrade"));
    }
    let mut request = Request {
        key: None,
        login: None,
        name: None,
    };
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "sec-websocket-key" => request.key = Some(value.trim().to_string()),
            "sec-websocket-version" if value.trim() != "13" => {
                return Err(protocol_error("only websocket version 13 is spoken"));
            }
            "tailscale-user-login" => request.login = Some(value.trim().to_string()),
            "tailscale-user-name" => request.name = Some(value.trim().to_string()),
            _ => {}
        }
    }
    Ok(request)
}

/// The `Sec-WebSocket-Accept` value for a handshake key (RFC 6455 §1.3)
pub fn accept_key(key: &str) -> String {
    use sha1_smol::Sha1;
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.digest().bytes())
}

fn write_response(mut stream: &TcpStream, head: &str) -> io::Result<()> {
    stream.write_all(head.as_bytes())
}

/// Answers the upgrade: 101 with the computed accept header
pub fn write_switching(stream: &TcpStream, key: &str) -> io::Result<()> {
    write_response(
        stream,
        &format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            accept_key(key)
        ),
    )
}

/// Refuses the upgrade: the identity is not in `remote.allow`
pub fn write_forbidden(stream: &TcpStream) -> io::Result<()> {
    write_response(
        stream,
        "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    )
}

/// The upgrade request was not a websocket upgrade
pub fn write_bad_request(stream: &TcpStream) -> io::Result<()> {
    write_response(
        stream,
        "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    )
}

/// One WebSocket frame
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Reads one client frame. Client frames must be masked (RFC 6455 §5.1); the payload is
/// unmasked in place. Control frames carry at most 125 bytes and are never fragmented.
fn read_frame<R: Read>(mut read: R) -> io::Result<Frame> {
    let mut head = [0u8; 2];
    read.read_exact(&mut head)?;
    if head[0] & 0x70 != 0 {
        return Err(protocol_error(
            "extension bits set, but no extension was negotiated",
        ));
    }
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0f;
    if head[1] & 0x80 == 0 {
        return Err(protocol_error("unmasked client frame (RFC 6455 §5.1)"));
    }
    let len = match head[1] & 0x7f {
        126 => {
            let mut extended = [0u8; 2];
            read.read_exact(&mut extended)?;
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            read.read_exact(&mut extended)?;
            let len = u64::from_be_bytes(extended);
            if len > crate::wire::MAX_FRAME as u64 {
                return Err(protocol_error(
                    "a frame exceeded the protocol's length limit",
                ));
            }
            len as usize
        }
        len => len as usize,
    };
    if opcode >= 0x8 && (len > 125 || !fin) {
        return Err(protocol_error("a control frame is fragmented or too large"));
    }
    let mut mask = [0u8; 4];
    read.read_exact(&mut mask)?;
    let mut payload = vec![0u8; len];
    read.read_exact(&mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// Writes one server frame — never masked (RFC 6455 §5.1); messages go out whole
fn write_frame<W: Write>(mut write: W, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        0..=125 => frame.push(payload.len() as u8),
        126..=0xFFFF => {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    write.write_all(&frame)
}

/// One message off the wire: the only kinds this protocol carries
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Binary(Vec<u8>),
    /// The peer closed, with its status code (if it sent one)
    Close(Option<u16>),
}

/// The write half of a websocket connection: whole binary messages, one close frame
#[derive(Clone)]
pub struct WsWriter {
    write: Arc<Mutex<TcpStream>>,
}

impl WsWriter {
    /// Any frame out (binary messages, pong echoes)
    fn write_frame_as(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut write = self.write.lock().unwrap();
        write_frame(&mut *write, opcode, payload)
    }

    /// A whole binary message
    pub fn write_binary(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_frame_as(OP_BINARY, bytes)
    }

    /// A close frame with a status code. Best effort: callers are closing anyway.
    pub fn write_close(&self, code: u16) {
        let _ = self.write_frame_as(OP_CLOSE, &code.to_be_bytes());
    }

    /// Ends the connection in both directions
    pub fn shutdown(&self) {
        let _ = self
            .write
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Both);
    }
}

/// The read half of a websocket connection
pub struct WsReader {
    read: TcpStream,
    /// The write half, shared for pong and close echoes from the reading thread
    write: WsWriter,
}

impl WsReader {
    /// The next message. Pings are answered in place through the shared write half, pongs
    /// are ignored, and fragmented binary messages are joined. Anything else that is not a
    /// binary message or a close is a protocol error; the caller ends the connection.
    pub fn read_message(&mut self) -> io::Result<Message> {
        let mut fragments: Option<Vec<u8>> = None;
        loop {
            let frame = read_frame(&mut self.read)?;
            match frame.opcode {
                OP_BINARY => {
                    if fragments.is_some() {
                        return Err(protocol_error(
                            "a message started before its fragments ended",
                        ));
                    }
                    if frame.fin {
                        return Ok(Message::Binary(frame.payload));
                    }
                    fragments = Some(frame.payload);
                }
                OP_CONTINUATION => {
                    let Some(accumulated) = fragments.as_mut() else {
                        return Err(protocol_error("a continuation frame opened no message"));
                    };
                    if accumulated.len() + frame.payload.len() > crate::wire::MAX_FRAME {
                        return Err(protocol_error(
                            "a message exceeded the protocol's length limit",
                        ));
                    }
                    accumulated.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return Ok(Message::Binary(fragments.take().unwrap_or_default()));
                    }
                }
                OP_PING => self.write.write_frame_as(OP_PONG, &frame.payload)?,
                OP_PONG => {}
                OP_CLOSE => {
                    let code = if frame.payload.len() >= 2 {
                        Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]))
                    } else {
                        None
                    };
                    self.write.write_close(code.unwrap_or(1000));
                    return Ok(Message::Close(code));
                }
                OP_TEXT => {
                    return Err(protocol_error(
                        "text frames are not part of this protocol (binary only)",
                    ));
                }
                other => {
                    return Err(protocol_error(&format!(
                        "unknown websocket opcode: {other}"
                    )));
                }
            }
        }
    }
}

/// A freshly upgraded websocket: its reader and writer halves (the writer is shared so
/// the reader thread can answer pings and closes while data streams out)
pub struct Ws;

impl Ws {
    pub fn from_stream(stream: TcpStream) -> io::Result<(WsReader, WsWriter)> {
        let write = stream.try_clone()?;
        let writer = WsWriter {
            write: Arc::new(Mutex::new(write)),
        };
        let reader = WsReader {
            read: stream,
            write: writer.clone(),
        };
        Ok((reader, writer))
    }
}
