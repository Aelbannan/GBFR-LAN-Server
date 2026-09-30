//! Tests for `common/http.rs`: the bounded blocking HTTP client both shims use.
//!
//! Build with plain rustc (the shims' build has no cargo/deps):
//!     rustc --edition 2021 -O -o http_test.exe common/http_test.rs
//!     .\http_test.exe
//!
//! The socket tests run a one-shot HTTP server on a loopback port, so the request line, headers,
//! body, status parsing and the two failure modes (oversized response, missing header terminator)
//! are all exercised against real bytes rather than a mock. CR/LF are written as numbers here for
//! the same reason as in the module: escape sequences in generated code are not trustworthy.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

#[path = "http.rs"]
mod http;

const CR: u8 = 0x0D;
const LF: u8 = 0x0A;

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

/// One-shot server: accepts a connection, reads the request head, and answers with `response`.
/// Returns the port and a handle carrying the request bytes the client actually sent.
fn serve_once(response: Vec<u8>) -> (u16, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until the head terminator is in, then drain whatever body bytes arrived with it.
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    seen.extend_from_slice(&chunk[..n]);
                    if seen.windows(4).any(|w| w == [CR, LF, CR, LF]) && seen.len() >= 4 {
                        // Give the client a moment to finish writing its body.
                        thread::sleep(Duration::from_millis(20));
                        let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                        if let Ok(n) = s.read(&mut chunk) {
                            seen.extend_from_slice(&chunk[..n]);
                        }
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = s.write_all(&response);
        let _ = s.flush();
        let _ = s.shutdown(Shutdown::Both);
        seen
    });
    (port, handle)
}

fn response(status: &str, body: &str) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}").into_bytes();
    r.extend_from_slice(&[CR, LF]);
    r.extend_from_slice(b"Content-Type: application/json");
    r.extend_from_slice(&[CR, LF]);
    r.extend_from_slice(format!("Content-Length: {}", body.len()).as_bytes());
    r.extend_from_slice(&[CR, LF, CR, LF]);
    r.extend_from_slice(body.as_bytes());
    r
}

fn main() {
    // ── read_response ────────────────────────────────────────────────────────────────────
    {
        let raw = response("200 OK", "{\"a\":1}");
        let r = http::read_response(&mut raw.as_slice(), http::MAX_RESPONSE_BYTES).unwrap();
        check(
            "status and body are split at the header terminator",
            r.status == 200 && r.body == "{\"a\":1}",
            &format!("{r:?}"),
        );
    }
    {
        let raw = response("404 Not Found", "");
        let r = http::read_response(&mut raw.as_slice(), http::MAX_RESPONSE_BYTES).unwrap();
        check(
            "empty body",
            r.status == 404 && r.body.is_empty(),
            &format!("{r:?}"),
        );
    }
    {
        let mut raw = b"HTTP/1.1 xxx Not A Number".to_vec();
        raw.extend_from_slice(&[CR, LF, CR, LF]);
        let r = http::read_response(&mut raw.as_slice(), http::MAX_RESPONSE_BYTES).unwrap();
        check(
            "an unparseable status line yields status 0, not an error",
            r.status == 0,
            &format!("{r:?}"),
        );
    }
    {
        let raw = b"HTTP/1.1 200 OK".to_vec(); // no terminator
        check(
            "a missing header terminator is NoHead",
            http::read_response(&mut raw.as_slice(), http::MAX_RESPONSE_BYTES)
                == Err(http::HttpError::NoHead),
            "",
        );
    }
    {
        // Exactly at the cap is fine; one past it is TooLarge.
        let body = "x".repeat(1000);
        let raw = response("200 OK", &body);
        let at_cap = http::read_response(&mut raw.as_slice(), raw.len());
        check(
            "a response exactly at the cap is accepted",
            at_cap.is_ok(),
            "",
        );
        let over = http::read_response(&mut raw.as_slice(), raw.len() - 1);
        check(
            "a response one byte past the cap is TooLarge",
            over == Err(http::HttpError::TooLarge),
            &format!("{over:?}"),
        );
    }
    {
        // A reader that errors mid-body: the partial response is still returned.
        struct Flaky {
            head: Vec<u8>,
            sent: bool,
        }
        impl Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.sent {
                    self.sent = true;
                    let n = self.head.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.head[..n]);
                    return Ok(n);
                }
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled"))
            }
        }
        let mut raw = b"HTTP/1.1 200 OK".to_vec();
        raw.extend_from_slice(&[CR, LF, CR, LF]);
        raw.extend_from_slice(b"partial");
        let mut f = Flaky {
            head: raw,
            sent: false,
        };
        let r = http::read_response(&mut f, http::MAX_RESPONSE_BYTES).unwrap();
        check(
            "a stalled body still yields the bytes received",
            r.status == 200 && r.body == "partial",
            &format!("{r:?}"),
        );
    }

    // ── request over a real socket ───────────────────────────────────────────────────────
    {
        let (port, handle) = serve_once(response("200 OK", "{\"ok\":true}"));
        let r = http::request("127.0.0.1", port, "POST", "/party/join", "{\"n\":1}", None).unwrap();
        let seen = String::from_utf8_lossy(&handle.join().unwrap()).into_owned();
        check(
            "POST returns the parsed response",
            r.status == 200 && r.body == "{\"ok\":true}",
            &format!("{r:?}"),
        );
        check(
            "the request line is well formed",
            seen.starts_with("POST /party/join HTTP/1.1"),
            &seen.lines().next().unwrap_or(""),
        );
        check(
            "Host carries the port",
            seen.contains(&format!("Host: 127.0.0.1:{port}")),
            "",
        );
        check(
            "Content-Length matches the body",
            seen.contains("Content-Length: 7") && seen.ends_with("{\"n\":1}"),
            "",
        );
        check(
            "Connection: close is sent",
            seen.contains("Connection: close"),
            "",
        );
        check(
            "no auth header when none was given",
            !seen.contains("X-EntityToken"),
            "",
        );
    }
    {
        let (port, handle) = serve_once(response("200 OK", "{}"));
        let _ = http::request(
            "127.0.0.1",
            port,
            "POST",
            "/Lobby/GetLobby",
            "{}",
            Some("X-EntityToken: tok-1"),
        );
        let seen = String::from_utf8_lossy(&handle.join().unwrap()).into_owned();
        check(
            "the auth header is sent verbatim on its own line",
            seen.contains(&format!("X-EntityToken: tok-1{}{}", CR as char, LF as char))
                || seen.contains("X-EntityToken: tok-1\n"),
            &seen,
        );
    }
    {
        // The response cap is enforced on the socket path too.
        let big = "y".repeat(2000);
        let (port, _handle) = serve_once(response("200 OK", &big));
        let r = http::request_capped("127.0.0.1", port, "POST", "/x", "{}", None, 500);
        check(
            "request_capped rejects an oversized response",
            r == Err(http::HttpError::TooLarge),
            &format!("{r:?}"),
        );
    }
    {
        // A server that accepts and immediately closes: a write may succeed and the read yields
        // nothing, so the client reports NoHead rather than panicking or hanging.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let _ = s.shutdown(Shutdown::Both);
        });
        let r = http::request("127.0.0.1", port, "POST", "/x", "{}", None);
        let _ = h.join();
        check(
            "a closed connection reports NoHead",
            r == Err(http::HttpError::NoHead),
            &format!("{r:?}"),
        );
    }
    {
        // Nothing listening: Connect, promptly (the cap is one second).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let started = std::time::Instant::now();
        let r = http::request("127.0.0.1", port, "POST", "/x", "{}", None);
        check(
            "a dead port reports Connect without hanging",
            r == Err(http::HttpError::Connect) && started.elapsed() < Duration::from_secs(3),
            &format!("{r:?} in {:?}", started.elapsed()),
        );
    }
    {
        let started = std::time::Instant::now();
        let r = http::request("127.0.0.1", 1, "POST", "/x", "{}", None);
        check(
            "a dead broker host returns quickly",
            r.is_err() && started.elapsed() < Duration::from_secs(3),
            &format!("{r:?} in {:?}", started.elapsed()),
        );
    }

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all http checks passed");
}
