//! Std-only JSON field accessors shared by both shims and their tests.
//!
//! The shims never need to build a document tree; they need "read this field" from three kinds of
//! blob: the broker's `{"code":..,"data":{..}}` envelope, the game's request bodies, and lobby /
//! member fragments. `serde_json` is not available to the rustc-built shims, so this module keeps a
//! small scanner with the properties the previous hand-rolled lookups lacked:
//!
//! * a key is a real `"key"` token followed by `:` — a key name appearing inside a *string value*
//!   can never match, and `"Owner"` can never match `"OwnerId"`;
//! * string values are unescaped (`\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`, `\uXXXX`,
//!   including surrogate pairs);
//! * objects and arrays are walked with balanced nesting, so a `}` or `]` inside a string or a
//!   nested container cannot truncate them;
//! * the scan is iterative and bounds-checked, so a truncated or hostile blob returns `None`
//!   rather than panicking (the previous `find`/`split` versions could not fail this way, and the
//!   shims must not panic across the FFI boundary).
//!
//! Lookup semantics are "first match in document order at any depth", matching the call sites that
//! were written against the broker envelope. `json_obj` / `json_arr_objects` return values in the
//! same shape as the copies this replaces, so the call sites stay unchanged.

#![allow(dead_code)]

use std::collections::HashMap;

/// Raw-element cap for `json_arr_objects`, matching the previous copies (the broker trims search
/// results to the client's requested count long before this is reachable).
const ARRAY_CAP: usize = 32;
/// Key cap for `json_obj`; documented lobby/member maps are far below it.
const OBJECT_CAP: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Str,
    Scalar,
    Object,
    Array,
    Null,
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scan<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        if self.i + 4 > self.b.len() {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.b[self.i];
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return None,
            };
            v = (v << 4) | d;
            self.i += 1;
        }
        Some(v)
    }

    /// Decode the string whose opening quote is at `self.i`; always leaves `self.i` one past the
    /// closing quote on success. Returns `None` on malformed input (never panics).
    fn string(&mut self) -> Option<String> {
        if self.peek() != Some(b'"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: a paired low surrogate must follow, or the
                                // document is malformed and the whole value is unusable.
                                if self.peek() != Some(b'\\') {
                                    return None;
                                }
                                self.i += 1;
                                if self.peek() != Some(b'u') {
                                    return None;
                                }
                                self.i += 1;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return None;
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp)?);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Copy the whole UTF-8 sequence for this byte.
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    if len == 0 || start + len > self.b.len() {
                        return None;
                    }
                    self.i = start + len;
                    out.push_str(std::str::from_utf8(&self.b[start..self.i]).ok()?);
                }
            }
        }
    }

    /// Advance past one value without allocating. Leaves `self.i` one past the value.
    fn skip_value(&mut self) -> Option<()> {
        self.skip_ws();
        match self.peek()? {
            b'"' => {
                self.string()?;
                Some(())
            }
            b'{' => {
                self.i += 1;
                loop {
                    self.skip_ws();
                    match self.peek()? {
                        b'}' => {
                            self.i += 1;
                            return Some(());
                        }
                        b',' => self.i += 1,
                        _ => {
                            self.string()?;
                            self.skip_ws();
                            if self.peek() != Some(b':') {
                                return None;
                            }
                            self.i += 1;
                            self.skip_value()?;
                        }
                    }
                }
            }
            b'[' => {
                self.i += 1;
                loop {
                    self.skip_ws();
                    match self.peek()? {
                        b']' => {
                            self.i += 1;
                            return Some(());
                        }
                        b',' => self.i += 1,
                        _ => self.skip_value()?,
                    }
                }
            }
            _ => {
                // Scalar literal: number, true, false, null. Ends at a structural delimiter.
                let start = self.i;
                while let Some(c) = self.peek() {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
                Some(())
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0, // continuation byte or invalid lead
    }
}

/// First `"key"` token followed by `:` in document order, at any depth.
fn find_value_start(blob: &str, key: &str) -> Option<usize> {
    let mut sc = Scan::new(blob);
    while let Some(c) = sc.peek() {
        if c == b'"' {
            let text = sc.string()?;
            sc.skip_ws();
            if sc.peek() == Some(b':') && text == key {
                sc.i += 1;
                // The value starts after any whitespace; callers inspect that first byte to
                // classify the value, so it must not be a space.
                sc.skip_ws();
                return Some(sc.i);
            }
        } else {
            sc.i += 1;
        }
    }
    None
}

/// Classify a value's first byte. `t`/`f`/`n` literals arrive here as `Scalar`; `span` corrects
/// a bare `null` to `Kind::Null`.
fn kind_at(c: u8) -> Kind {
    match c {
        b'"' => Kind::Str,
        b'{' => Kind::Object,
        b'[' => Kind::Array,
        _ => Kind::Scalar,
    }
}

/// `(kind, start, end)` of the value of the first `key`, scanning containers with balanced nesting.
fn span(blob: &str, key: &str) -> Option<(Kind, usize, usize)> {
    // `find_value_start` has already skipped whitespace after the colon.
    let start = find_value_start(blob, key)?;
    let b = blob.as_bytes();
    let first = *b.get(start)?;
    let mut sc = Scan { b, i: start };
    sc.skip_value()?;
    let end = sc.i;
    let kind = match first {
        b't' | b'f' => Kind::Scalar,
        b'n' => {
            if &b[start..end] == b"null" {
                Kind::Null
            } else {
                Kind::Scalar
            }
        }
        c => kind_at(c),
    };
    Some((kind, start, end))
}

/// String-valued field: decoded JSON strings, plus raw text for numbers/bools. Objects, arrays and
/// `null` yield `None` (callers use `json_obj` / `json_arr_objects` for those).
pub fn json_str(blob: &str, key: &str) -> Option<String> {
    let (kind, start, end) = span(blob, key)?;
    match kind {
        Kind::Str => {
            let mut sc = Scan {
                b: blob.as_bytes(),
                i: start,
            };
            sc.string()
        }
        Kind::Scalar => Some(blob[start..end].to_string()),
        _ => None,
    }
}

/// Numeric field as written in the document (no quotes), for callers that format it back out.
pub fn json_num(blob: &str, key: &str) -> Option<String> {
    let (kind, start, end) = span(blob, key)?;
    if kind != Kind::Scalar {
        return None;
    }
    let raw = &blob[start..end];
    if raw.is_empty()
        || !raw
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return None;
    }
    Some(raw.to_string())
}

pub fn json_u32(blob: &str, key: &str) -> Option<u32> {
    json_str(blob, key)?.parse().ok()
}

pub fn json_i64(blob: &str, key: &str) -> Option<i64> {
    json_str(blob, key)?.parse().ok()
}

/// Object field as a `key -> scalar text` map. Nested objects/arrays are skipped, matching the
/// copies this replaces (their callers only read scalar properties).
pub fn json_obj(blob: &str, key: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some((kind, start, _end)) = span(blob, key) else {
        return out;
    };
    if kind != Kind::Object {
        return out;
    }
    let mut sc = Scan {
        b: blob.as_bytes(),
        i: start + 1,
    };
    loop {
        sc.skip_ws();
        match sc.peek() {
            None | Some(b'}') => break,
            Some(b',') => {
                sc.i += 1;
                continue;
            }
            _ => {
                let Some(k) = sc.string() else { break };
                sc.skip_ws();
                if sc.peek() != Some(b':') {
                    break;
                }
                sc.i += 1;
                sc.skip_ws();
                let Some(vfirst) = sc.peek() else { break };
                if vfirst == b'{' || vfirst == b'[' {
                    // Nested container: skip it without recording a bogus string value.
                    let mut skip = Scan { b: sc.b, i: sc.i };
                    if skip.skip_value().is_none() {
                        break;
                    }
                    sc.i = skip.i;
                } else {
                    let vstart = sc.i;
                    if sc.skip_value().is_none() {
                        break;
                    }
                    let raw = &blob[vstart..sc.i];
                    let v = if vfirst == b'"' {
                        // Re-read the string to unescape it exactly once.
                        let mut inner = Scan { b: sc.b, i: vstart };
                        match inner.string() {
                            Some(s) => s,
                            None => break,
                        }
                    } else if raw == "null" {
                        String::new()
                    } else {
                        raw.to_string()
                    };
                    if !k.is_empty() && out.len() < OBJECT_CAP {
                        out.insert(k, v);
                    }
                }
            }
        }
    }
    out
}

/// Array field as raw element text for object elements (`{...}`), matching the copies this
/// replaces. Scalar elements are returned as their raw text too, so callers that only look for
/// objects still behave identically (they will simply not find their keys).
pub fn json_arr_objects(blob: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some((kind, start, _end)) = span(blob, key) else {
        return out;
    };
    if kind != Kind::Array {
        return out;
    }
    let mut sc = Scan {
        b: blob.as_bytes(),
        i: start + 1,
    };
    loop {
        sc.skip_ws();
        match sc.peek() {
            None | Some(b']') => break,
            Some(b',') => {
                sc.i += 1;
                continue;
            }
            _ => {
                let estart = sc.i;
                if sc.skip_value().is_none() {
                    break;
                }
                out.push(blob[estart..sc.i].to_string());
                if out.len() >= ARRAY_CAP {
                    break;
                }
            }
        }
    }
    out
}

/// JSON string escaping for request bodies (control characters are emitted as `\u00XX`).
pub fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}
