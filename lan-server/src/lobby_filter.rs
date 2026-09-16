//! OData-like `FindLobbies` filter and sort evaluation for the LAN broker.
//!
//! The real Lobby service evaluates `Filter`/`OrderBy` server-side ("Find lobbies",
//! learn.microsoft.com, last updated 2025-05-01). This module implements the subset the
//! exe can emit plus the documented operators:
//!
//! * clauses joined by `and`; operators `eq ne le lt ge gt` (case sensitive);
//! * `string_keyN` / `number_keyN`, N = 1..30 — numeric values are floats (the service
//!   contract in MS_DLL_GROUND_TRUTH §4.3: "Numeric values are floats");
//! * the predefined keys `lobby/memberCount`, `lobby/maxMemberCount`,
//!   `lobby/memberCountRemaining`, `lobby/membershipLock`, `lobby/amOwner`,
//!   `lobby/amMember`, `lobby/amServer`;
//! * `lobbyId` / `lobby/lobbyId` as a broker extension. The service has no documented
//!   lobby-id filter; the exe's join-by-code path filters `string_key1`/`string_key2`
//!   (disassembly of the second PFMultiplayerFindLobbies call site, 0x143B52E2F).
//!
//! Anything outside the subset is *reported*, never silently dropped: `unsupported()`
//! lists the offending text, the caller logs it and puts it in the response, and
//! `matches()` fails closed so a constraint we did not apply can never look like a
//! successful search.

use serde_json::{Map, Value};
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    StringKey(u32),
    NumberKey(u32),
    MemberCount,
    MaxMemberCount,
    MemberCountRemaining,
    MembershipLock,
    AmOwner,
    AmMember,
    AmServer,
    LobbyId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    Le,
    Lt,
    Ge,
    Gt,
}

#[derive(Clone, Debug)]
pub struct Clause {
    pub key: Key,
    pub op: Op,
    pub value: String,
    pub raw: String,
}

/// A parsed filter. `clauses` are the clauses we can evaluate; `unsupported` is the text of
/// every clause (or whole tail) we cannot.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    clauses: Vec<Clause>,
    unsupported: Vec<String>,
}

impl Filter {
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// True only when every parsed clause matched. An empty filter matches everything.
    /// If any clause was unsupported, nothing matches (fail closed) — the caller logs it.
    pub fn matches(&self, lobby: &LobbyView) -> bool {
        if !self.unsupported.is_empty() {
            return false;
        }
        self.clauses
            .iter()
            .all(|c| matches!(eval_clause(c, lobby), Ok(true)))
    }
}

/// The broker-side view of one lobby that the filter/sort can read.
pub struct LobbyView<'a> {
    pub id: &'a str,
    pub max_players: f64,
    pub current_members: f64,
    pub membership_lock: &'a str,
    pub search_data: &'a Map<String, Value>,
    pub owner_id: &'a str,
    pub viewer_id: Option<&'a str>,
    pub viewer_is_member: bool,
    /// The broker has no joined-server concept; every client-owned lobby is amServer=false.
    pub has_server: bool,
}

#[derive(Clone, Debug)]
pub struct SortSpec {
    pub key: Key,
    pub desc: bool,
    /// `distance{number_keyN = V}`: sort by |stored - V| ascending.
    pub distance: Option<f64>,
}

// ---------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------

struct Scanner<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, i: 0 }
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s.as_bytes()[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.i >= self.s.len()
    }

    fn take_bare(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.s.len() && !self.s.as_bytes()[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        if start == self.i {
            None
        } else {
            Some(&self.s[start..self.i])
        }
    }

    /// A value is either a single-quoted string (with `''` as an escaped quote) or a bare
    /// token (numbers, `true`, `Unlocked`, ...).
    fn take_value(&mut self) -> Option<String> {
        self.skip_ws();
        if self.s[self.i..].starts_with('\'') {
            let mut out = String::new();
            let mut it = self.s[self.i + 1..].char_indices();
            while let Some((off, c)) = it.next() {
                if c == '\'' {
                    if let Some((_, '\'')) = it.clone().next() {
                        out.push('\'');
                        it.next();
                        continue;
                    }
                    self.i = self.i + 1 + off + c.len_utf8();
                    return Some(out);
                }
                out.push(c);
            }
            None // unterminated quote
        } else {
            self.take_bare().map(str::to_string)
        }
    }
}

fn parse_key(tok: &str) -> Option<Key> {
    if let Some(rest) = tok.strip_prefix("string_key") {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=30).contains(&n) {
                return Some(Key::StringKey(n));
            }
        }
    }
    if let Some(rest) = tok.strip_prefix("number_key") {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=30).contains(&n) {
                return Some(Key::NumberKey(n));
            }
        }
    }
    match tok {
        "lobby/memberCount" | "lobby/currentMemberCount" => Some(Key::MemberCount),
        "lobby/maxMemberCount" => Some(Key::MaxMemberCount),
        "lobby/memberCountRemaining" => Some(Key::MemberCountRemaining),
        "lobby/membershipLock" => Some(Key::MembershipLock),
        "lobby/amOwner" => Some(Key::AmOwner),
        "lobby/amMember" => Some(Key::AmMember),
        "lobby/amServer" => Some(Key::AmServer),
        "lobbyId" | "lobby/lobbyId" | "LobbyId" => Some(Key::LobbyId),
        _ => None,
    }
}

fn parse_op(tok: &str) -> Option<Op> {
    match tok {
        "eq" => Some(Op::Eq),
        "ne" => Some(Op::Ne),
        "le" => Some(Op::Le),
        "lt" => Some(Op::Lt),
        "ge" => Some(Op::Ge),
        "gt" => Some(Op::Gt),
        _ => None,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// A clause is supported only if its key accepts the operator and its value has the right
/// type. This is what keeps `unsupported()` exact: nothing reaches evaluation unchecked.
fn clause_supported(c: &Clause) -> bool {
    match c.key {
        // The service documents ordering for numeric properties only; strings are eq/ne.
        Key::StringKey(_) | Key::LobbyId | Key::MembershipLock => {
            matches!(c.op, Op::Eq | Op::Ne)
        }
        Key::AmOwner | Key::AmMember | Key::AmServer => {
            matches!(c.op, Op::Eq | Op::Ne) && parse_bool(&c.value).is_some()
        }
        Key::NumberKey(_) => c.value.trim().parse::<f64>().is_ok(),
        Key::MemberCount | Key::MaxMemberCount | Key::MemberCountRemaining => {
            c.value.trim().parse::<f64>().is_ok()
        }
    }
}

pub fn parse_filter(s: &str) -> Filter {
    let mut out = Filter::default();
    let mut sc = Scanner::new(s);
    loop {
        if sc.at_end() {
            break;
        }
        let start = sc.i;
        let Some(key_tok) = sc.take_bare() else {
            break;
        };
        if key_tok == "or" {
            out.unsupported.push(s[start..].trim().to_string());
            break;
        }
        let Some(op_tok) = sc.take_bare() else {
            out.unsupported.push(s[start..].trim().to_string());
            break;
        };
        let Some(op) = parse_op(op_tok) else {
            out.unsupported.push(s[start..].trim().to_string());
            break;
        };
        let Some(value) = sc.take_value() else {
            out.unsupported.push(s[start..].trim().to_string());
            break;
        };
        let raw = s[start..sc.i].trim().to_string();
        match parse_key(key_tok) {
            Some(key) => {
                let clause = Clause { key, op, value, raw };
                if clause_supported(&clause) {
                    out.clauses.push(clause);
                } else {
                    out.unsupported.push(clause.raw);
                }
            }
            None => out.unsupported.push(raw),
        }
        if sc.at_end() {
            break;
        }
        let Some(join) = sc.take_bare() else {
            break;
        };
        if join != "and" {
            out.unsupported.push(s[start..].trim().to_string());
            break;
        }
    }
    out
}

/// Parse an `OrderBy` string. Returns the specs we can apply and the text of the terms we
/// cannot. Sorting cannot hide or invent rows, so an unsupported term only degrades order.
pub fn parse_sort(s: &str) -> (Vec<SortSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut bad = Vec::new();
    for term in s.split(',') {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        if let Some(inner) = term.strip_prefix("distance{").and_then(|t| t.strip_suffix('}')) {
            match inner.split_once('=') {
                Some((k, v)) => {
                    let key = parse_key(k.trim());
                    let target = v.trim().parse::<f64>();
                    match (key, target) {
                        (Some(k @ Key::NumberKey(_)), Ok(t)) => specs.push(SortSpec {
                            key: k,
                            desc: false,
                            distance: Some(t),
                        }),
                        _ => bad.push(term.to_string()),
                    }
                }
                None => bad.push(term.to_string()),
            }
            continue;
        }
        let mut it = term.split_whitespace();
        let key = it.next().and_then(parse_key);
        let dir = it.next();
        let extra = it.next();
        if extra.is_some() {
            bad.push(term.to_string());
            continue;
        }
        let desc = match dir {
            Some("asc") => false,
            Some("desc") => true,
            _ => {
                bad.push(term.to_string());
                continue;
            }
        };
        match key {
            Some(k @ (Key::NumberKey(_) | Key::MemberCount | Key::MaxMemberCount | Key::MemberCountRemaining)) => {
                specs.push(SortSpec { key: k, desc, distance: None });
            }
            // The docs restrict OrderBy to the numeric search keys; strings are unsupported.
            _ => bad.push(term.to_string()),
        }
    }
    (specs, bad)
}

// ---------------------------------------------------------------------------
// evaluation
// ---------------------------------------------------------------------------

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn value_as_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        _ => None,
    }
}

fn search_number(l: &LobbyView, n: u32) -> Option<f64> {
    l.search_data
        .get(&format!("number_key{n}"))
        .and_then(value_as_f64)
}

fn search_string(l: &LobbyView, n: u32) -> Option<String> {
    l.search_data
        .get(&format!("string_key{n}"))
        .and_then(value_as_str)
}

fn cmp_num(a: f64, b: f64, op: Op) -> bool {
    match op {
        Op::Eq => a == b,
        Op::Ne => a != b,
        Op::Le => a <= b,
        Op::Lt => a < b,
        Op::Ge => a >= b,
        Op::Gt => a > b,
    }
}

fn eval_clause(c: &Clause, l: &LobbyView) -> Result<bool, String> {
    match c.key {
        Key::StringKey(n) => {
            let Some(stored) = search_string(l, n) else {
                // A lobby that never published the property cannot satisfy a constraint on it.
                return Ok(false);
            };
            Ok(match c.op {
                Op::Eq => stored == c.value,
                Op::Ne => stored != c.value,
                _ => return Err(c.raw.clone()),
            })
        }
        Key::NumberKey(n) => {
            let want = c.value.trim().parse::<f64>().map_err(|_| c.raw.clone())?;
            match search_number(l, n) {
                Some(stored) => Ok(cmp_num(stored, want, c.op)),
                None => Ok(false),
            }
        }
        Key::MemberCount => {
            let want = c.value.trim().parse::<f64>().map_err(|_| c.raw.clone())?;
            Ok(cmp_num(l.current_members, want, c.op))
        }
        Key::MaxMemberCount => {
            let want = c.value.trim().parse::<f64>().map_err(|_| c.raw.clone())?;
            Ok(cmp_num(l.max_players, want, c.op))
        }
        Key::MemberCountRemaining => {
            let want = c.value.trim().parse::<f64>().map_err(|_| c.raw.clone())?;
            Ok(cmp_num(l.max_players - l.current_members, want, c.op))
        }
        Key::MembershipLock => Ok(match c.op {
            Op::Eq => l.membership_lock.eq_ignore_ascii_case(&c.value),
            Op::Ne => !l.membership_lock.eq_ignore_ascii_case(&c.value),
            _ => return Err(c.raw.clone()),
        }),
        Key::AmOwner => {
            let want = parse_bool(&c.value).ok_or_else(|| c.raw.clone())?;
            let is_owner = l.viewer_id.is_some_and(|v| !v.is_empty() && v == l.owner_id);
            Ok(match c.op {
                Op::Eq => is_owner == want,
                Op::Ne => is_owner != want,
                _ => return Err(c.raw.clone()),
            })
        }
        Key::AmMember => {
            let want = parse_bool(&c.value).ok_or_else(|| c.raw.clone())?;
            Ok(match c.op {
                Op::Eq => l.viewer_is_member == want,
                Op::Ne => l.viewer_is_member != want,
                _ => return Err(c.raw.clone()),
            })
        }
        Key::AmServer => {
            let want = parse_bool(&c.value).ok_or_else(|| c.raw.clone())?;
            Ok(match c.op {
                Op::Eq => l.has_server == want,
                Op::Ne => l.has_server != want,
                _ => return Err(c.raw.clone()),
            })
        }
        Key::LobbyId => Ok(match c.op {
            Op::Eq => l.id == c.value,
            Op::Ne => l.id != c.value,
            _ => return Err(c.raw.clone()),
        }),
    }
}

fn sort_number(l: &LobbyView, key: &Key) -> Option<f64> {
    match key {
        Key::NumberKey(n) => search_number(l, *n),
        Key::MemberCount => Some(l.current_members),
        Key::MaxMemberCount => Some(l.max_players),
        Key::MemberCountRemaining => Some(l.max_players - l.current_members),
        _ => None,
    }
}

/// Order two lobbies by the sort string. Missing values sort last; a caller that needs a
/// deterministic tail should add the documented creation-time-descending tiebreak.
pub fn cmp_lobbies(a: &LobbyView, b: &LobbyView, specs: &[SortSpec]) -> Ordering {
    for spec in specs {
        let av = sort_number(a, &spec.key);
        let bv = sort_number(b, &spec.key);
        let ord = match (av, bv) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => {
                let (x, y) = match spec.distance {
                    Some(t) => ((x - t).abs(), (y - t).abs()),
                    None => (x, y),
                };
                let o = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
                if spec.desc {
                    o.reverse()
                } else {
                    o
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view<'a>(id: &'a str, sd: &'a Map<String, Value>, lock_: &'a str) -> LobbyView<'a> {
        LobbyView {
            id,
            max_players: 8.0,
            current_members: 2.0,
            membership_lock: lock_,
            search_data: sd,
            owner_id: "owner",
            viewer_id: Some("owner"),
            viewer_is_member: true,
            has_server: false,
        }
    }

    fn sd(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn empty_filter_matches_all() {
        let f = parse_filter("");
        let data = sd(json!({}));
        assert!(f.matches(&view("a", &data, "Unlocked")));
        assert!(f.unsupported().is_empty());
    }

    #[test]
    fn exe_number_key9_filter() {
        let f = parse_filter("number_key21 eq 0 and number_key9 eq 1 and number_key10 eq 2");
        assert!(f.unsupported().is_empty(), "{:?}", f.unsupported());
        let hit = sd(json!({"number_key21": 0, "number_key9": "1", "number_key10": 2}));
        let miss = sd(json!({"number_key21": 0, "number_key9": "0", "number_key10": 2}));
        assert!(f.matches(&view("a", &hit, "Unlocked")));
        assert!(!f.matches(&view("b", &miss, "Unlocked")));
    }

    #[test]
    fn string_ne_and_eq() {
        let f = parse_filter("string_key2 ne 'pw' and string_key3 eq '1'");
        assert!(f.unsupported().is_empty());
        let hit = sd(json!({"string_key2": "other", "string_key3": "1"}));
        let miss = sd(json!({"string_key2": "pw", "string_key3": "1"}));
        assert!(f.matches(&view("a", &hit, "Unlocked")));
        assert!(!f.matches(&view("b", &miss, "Unlocked")));
    }

    #[test]
    fn float_comparisons_on_number_keys() {
        let f = parse_filter("number_key1 le 2.5");
        let a = sd(json!({"number_key1": 1.5}));
        let b = sd(json!({"number_key1": 2.5}));
        let c = sd(json!({"number_key1": 100.5}));
        assert!(f.matches(&view("a", &a, "Unlocked")));
        assert!(f.matches(&view("b", &b, "Unlocked")));
        assert!(!f.matches(&view("c", &c, "Unlocked")));
        let g = parse_filter("number_key1 gt 2.4 and number_key1 lt 2.6");
        assert!(g.matches(&view("b", &b, "Unlocked")));
    }

    #[test]
    fn membership_lock_is_operator_aware() {
        // The old bug: a substring test for "locked" matched "unlocked".
        let unlocked = parse_filter("lobby/membershipLock eq 'Unlocked'");
        let locked = parse_filter("lobby/membershipLock eq 'Locked'");
        let not_locked = parse_filter("lobby/membershipLock ne 'Locked'");
        let data = sd(json!({}));
        assert!(unlocked.matches(&view("a", &data, "Unlocked")));
        assert!(!locked.matches(&view("a", &data, "Unlocked")));
        assert!(not_locked.matches(&view("a", &data, "Unlocked")));
        assert!(locked.matches(&view("a", &data, "Locked")));
        assert!(!not_locked.matches(&view("a", &data, "Locked")));
    }

    #[test]
    fn predefined_counts_and_am_keys() {
        let f = parse_filter(
            "lobby/memberCount eq 2 and lobby/maxMemberCount gt 4 and lobby/memberCountRemaining ge 6 and lobby/amOwner eq 'true' and lobby/amMember eq 'true' and lobby/amServer eq 'false'",
        );
        assert!(f.unsupported().is_empty(), "{:?}", f.unsupported());
        let data = sd(json!({}));
        assert!(f.matches(&view("a", &data, "Unlocked")));
    }

    #[test]
    fn unsupported_clauses_are_reported_and_fail_closed() {
        for bad in [
            "or",
            "number_key31 eq 1",
            "string_key1 gt 'x'",
            "number_key1 eq abc",
            "lobby/amOwner eq 'maybe'",
            "number_key9 eq 1 or number_key9 eq 0",
        ] {
            let f = parse_filter(bad);
            assert!(!f.unsupported().is_empty(), "not reported: {bad}");
            let data = sd(json!({"number_key9": "1"}));
            assert!(!f.matches(&view("a", &data, "Unlocked")), "did not fail closed: {bad}");
        }
    }

    #[test]
    fn sort_asc_desc_and_distance() {
        let (specs, bad) = parse_sort("number_key1 desc");
        assert!(bad.is_empty());
        let a = sd(json!({"number_key1": 1}));
        let b = sd(json!({"number_key1": 2}));
        let va = view("a", &a, "Unlocked");
        let vb = view("b", &b, "Unlocked");
        assert_eq!(cmp_lobbies(&va, &vb, &specs), Ordering::Greater);

        let (specs, bad) = parse_sort("distance{number_key1 = 3}");
        assert!(bad.is_empty());
        let c = sd(json!({"number_key1": 4}));
        let vc = view("c", &c, "Unlocked");
        // |2-3| = 1 beats |4-3| = 1? tie -> Equal; use 5 to distinguish.
        let d = sd(json!({"number_key1": 5}));
        let vd = view("d", &d, "Unlocked");
        assert_eq!(cmp_lobbies(&vb, &vc, &specs), Ordering::Equal);
        assert_eq!(cmp_lobbies(&vb, &vd, &specs), Ordering::Less);
    }

    #[test]
    fn string_sort_is_unsupported() {
        let (specs, bad) = parse_sort("string_key1 asc");
        assert!(specs.is_empty());
        assert_eq!(bad.len(), 1);
    }
}
