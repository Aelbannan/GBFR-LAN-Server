//! Small blocking HTTP/1.1 client shared by the shims and their tests.
//!
//! Both shims talk to the broker the same way (`POST /…` with a JSON body, one response, close),
//! and both used to carry their own copy of the connect/read code. This module is that code once,
//! with the two properties the copies lacked:
//!
//! * **bounded responses** — a `Content-Length` the broker never fills (or a malicious peer on the
//!   LAN) cannot grow the client's buffer without limit: past `max_bytes` the read stops with
//!   `HttpError::TooLarge` instead of `read_to_end`;
//! * **bounded connect** — the OS default connect timeout is ~21 s, which would freeze the game's
//!   tick thread on a dead broker host; the connect is capped at one second.
//!
//! `std`-only, like everything the shims build. The bytes that frame a request are written as
//! numbers, never as Rust escape sequences, so an editing/generation layer cannot turn `\r\n` into
//! a real newline and silently change the protocol.
//!
//! A read error mid-body is *not* fatal: whatever arrived is used, matching the previous
//! behaviour (a broker that answered the status line and then stalled still yields a partial body
//! for the caller to inspect). Only an oversized body or a missing header terminator fail.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Client-side cap on one response body. The broker's largest response is a lobby list or a boot
/// config blob, both far below this.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Connect cap: the OS default (~21 s) is unacceptable on the game's tick thread.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Per-read/per-write cap, as before.
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

const CR: u8 = 0x0D;
const LF: u8 = 0x0A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// DNS/connect/address failure, or the connect cap expired.
    Connect,
    /// The request could not be written.
    Write,
    /// No `CRLFCRLF` header terminator arrived.
    NoHead,
    /// The response body exceeded the caller's cap.
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// HTTP status, or `0` when the status line could not be parsed.
    pub status: u16,
    pub body: String,
}

/// Connect with the one-second cap, for callers that need the socket itself (the Party shim uses
/// it to discover the NIC address the broker is reachable on).
pub fn connect(host: &str, port: u16) -> Option<TcpStream> {
    let addr: SocketAddr = (host, port).to_socket_addrs().ok()?.next()?;
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()
}

/// Read one response, stopping at `max_bytes + 1` accumulated bytes.
pub fn read_response<R: Read>(r: &mut R, max_bytes: usize) -> Result<Response, HttpError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max_bytes {
                    return Err(HttpError::TooLarge);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            // Keep what arrived; a stalled body is still a usable partial response.
            Err(_) => break,
        }
    }
    let Some(idx) = buf.windows(4).position(|w| w == [CR, LF, CR, LF]) else {
        return Err(HttpError::NoHead);
    };
    let head = String::from_utf8_lossy(&buf[..idx]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    Ok(Response {
        status,
        body: String::from_utf8_lossy(&buf[idx + 4..]).into_owned(),
    })
}

/// One request on a fresh connection. `auth`, when given, is a complete header line *without* the
/// trailing CRLF (for example `X-EntityToken: abc`).
pub fn request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    auth: Option<&str>,
) -> Result<Response, HttpError> {
    request_capped(host, port, method, path, body, auth, MAX_RESPONSE_BYTES)
}

pub fn request_capped(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    auth: Option<&str>,
    max_bytes: usize,
) -> Result<Response, HttpError> {
    let mut stream = connect(host, port).ok_or(HttpError::Connect)?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut req = format!("{method} {path} HTTP/1.1").into_bytes();
    req.extend_from_slice(&[CR, LF]);
    req.extend_from_slice(format!("Host: {host}:{port}").as_bytes());
    req.extend_from_slice(&[CR, LF]);
    req.extend_from_slice(b"Content-Type: application/json");
    req.extend_from_slice(&[CR, LF]);
    if let Some(a) = auth {
        if !a.is_empty() {
            req.extend_from_slice(a.as_bytes());
            req.extend_from_slice(&[CR, LF]);
        }
    }
    req.extend_from_slice(format!("Content-Length: {}", body.len()).as_bytes());
    req.extend_from_slice(&[CR, LF]);
    req.extend_from_slice(b"Connection: close");
    req.extend_from_slice(&[CR, LF, CR, LF]);
    req.extend_from_slice(body.as_bytes());

    stream.write_all(&req).map_err(|_| HttpError::Write)?;
    read_response(&mut stream, max_bytes)
}
