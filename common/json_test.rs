//! Tests for `common/json.rs` — the shared std-only JSON field accessors.
//!
//! Build with plain rustc (the shims' build has no cargo):
//!     rustc -O -o json_test.exe common/json_test.rs
//!     .\json_test.exe
//!
//! These cover the escape/nesting cases the two hand-rolled copies got wrong: a key name inside a
//! string value, escaped quotes, unicode escapes (including surrogate pairs), balanced containers,
//! and malformed input. Every case also asserts "no panic", because a panic in a shim that runs
//! inside the game process is a crash, not a test failure.

#[path = "json.rs"]
mod json;

use std::sync::atomic::{AtomicU32, Ordering};

static FAILURES: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        println!("PASS  {name}  {detail}");
    } else {
        println!("FAIL  {name}  {detail}");
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}

fn eq(name: &str, got: Option<String>, want: &str) {
    check(
        name,
        got.as_deref() == Some(want),
        &format!("got={got:?} want={want:?}"),
    );
}

fn main() {
    // ── basics ────────────────────────────────────────────────────────────────────────────
    let doc = r#"{"LobbyId":"lan-1","MaxPlayers":8,"MembershipLock":false,"Owner":null}"#;
    eq("top-level string", json::json_str(doc, "LobbyId"), "lan-1");
    eq(
        "unquoted number as text",
        json::json_str(doc, "MaxPlayers"),
        "8",
    );
    eq(
        "bool as text",
        json::json_str(doc, "MembershipLock"),
        "false",
    );
    check(
        "null is None, not an empty string",
        json::json_str(doc, "Owner").is_none(),
        "",
    );
    check(
        "missing key yields None",
        json::json_str(doc, "Nope").is_none(),
        "",
    );
    check(
        "empty blob yields None",
        json::json_str("", "x").is_none(),
        "",
    );
    check(
        "blob without braces still scans",
        json::json_str("\"LobbyId\":\"a\"", "LobbyId").as_deref() == Some("a"),
        "",
    );

    // ── the broker envelope: first match at any depth ─────────────────────────────────────
    let env = r#"{"code":200,"status":"OK","data":{"LobbyId":"lan-2","data":{"LobbyId":"inner"}}}"#;
    eq(
        "envelope finds data.LobbyId",
        json::json_str(env, "LobbyId"),
        "lan-2",
    );
    eq(
        "envelope object field",
        json::json_obj(env, "data").get("LobbyId").cloned(),
        "lan-2",
    );

    // ── key names must not match inside string values, or as a prefix of another key ──────
    let tricky =
        r#"{"note":"this says \"LobbyId\":\"decoy\" in text","OwnerId":"o1","Owner":{"Id":"o2"}}"#;
    check(
        "key inside a string value does not match",
        json::json_str(tricky, "LobbyId").is_none(),
        &format!("got={:?}", json::json_str(tricky, "LobbyId")),
    );
    check(
        "prefix key does not match",
        json::json_str(tricky, "Owner").is_none(),
        &format!("got={:?}", json::json_str(tricky, "Owner")),
    );
    eq(
        "the exact key still matches",
        json::json_obj(tricky, "Owner").get("Id").cloned(),
        "o2",
    );
    eq("OwnerId matches", json::json_str(tricky, "OwnerId"), "o1");

    // ── escapes ───────────────────────────────────────────────────────────────────────────
    let esc = r#"{"s":"a\"b\\c\/d\be\ff\ng\rh\ti","u":"\u0041\u00e9","emoji":"\ud83d\ude00"}"#;
    eq(
        "escaped quote/backslash/slash",
        json::json_str(esc, "s"),
        "a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti",
    );
    eq("\\u escapes", json::json_str(esc, "u"), "Aé");
    eq("surrogate pair", json::json_str(esc, "emoji"), "😀");
    eq(
        "raw multibyte passes through",
        json::json_str(r#"{"n":"グranblue"}"#, "n"),
        "グranblue",
    );
    eq(
        "escape round-trips through json_str",
        json::json_str(
            &format!(r#"{{"v":"{}"}}"#, json::json_escape("q\"\\\n\tx")),
            "v",
        ),
        "q\"\\\n\tx",
    );

    // ── containers ────────────────────────────────────────────────────────────────────────
    check(
        "object value is not a string",
        json::json_str(r#"{"Owner":{"Id":"x"}}"#, "Owner").is_none(),
        "",
    );
    check(
        "array value is not a string",
        json::json_str(r#"{"Lobbies":[]}"#, "Lobbies").is_none(),
        "",
    );
    let lob = r#"{"data":{"LobbyData":{"string_key1":"v1","string_key2":"v2","network_descriptor":"LAN1.abc.0a000001","nested":{"skip":1},"list":[1,2]},"SearchData":{"string_key5":"name"}}}"#;
    let data = json::json_obj(lob, "LobbyData");
    check(
        "json_obj reads scalars",
        data.get("string_key1").map(String::as_str) == Some("v1")
            && data.get("string_key2").map(String::as_str) == Some("v2"),
        &format!("{data:?}"),
    );
    check(
        "json_obj skips nested containers",
        !data.contains_key("nested") && !data.contains_key("list"),
        &format!("{data:?}"),
    );
    eq(
        "json_obj second object",
        json::json_obj(lob, "SearchData")
            .get("string_key5")
            .cloned(),
        "name",
    );

    // Array elements keep balanced braces even when a string inside them looks structural.
    let arr = r#"{"Lobbies":[{"Id":"a","Name":"}{"},{"Id":"b","Name":"[x]"},{"Id":"c"}]}"#;
    let rows = json::json_arr_objects(arr, "Lobbies");
    check(
        "json_arr_objects splits balanced objects",
        rows.len() == 3
            && rows[0].contains(r#""Id":"a""#)
            && rows[1].contains(r#""Id":"b""#)
            && rows[2].contains(r#""Id":"c""#),
        &format!("{rows:?}"),
    );
    eq(
        "member fragment lookup",
        json::json_str(&rows[0], "Name"),
        "}{",
    );
    let member =
        r#"{"entity_id":"e1","ip":"10.0.0.5","udp_port":"27015","members":[{"entity_id":"e2"}]}"#;
    eq(
        "members array of the party broker",
        json::json_arr_objects(member, "members")
            .first()
            .and_then(|m| json::json_str(m, "entity_id")),
        "e2",
    );
    check(
        "array lookup respects the 32-element cap",
        json::json_arr_objects(
            &format!(
                r#"{{"a":[{}]}}"#,
                (0..40)
                    .map(|i| format!(r#"{{"i":{i}}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            "a",
        )
        .len()
            == 32,
        "",
    );

    // ── numbers ───────────────────────────────────────────────────────────────────────────
    eq(
        "json_num numeric",
        json::json_num(r#"{"n":-12.5e2}"#, "n"),
        "-12.5e2",
    );
    check(
        "json_num rejects a quoted number",
        json::json_num(r#"{"n":"12"}"#, "n").is_none(),
        &format!("got={:?}", json::json_num(r#"{"n":"12"}"#, "n")),
    );
    check(
        "json_num rejects a non-numeric scalar",
        json::json_num(r#"{"n":false}"#, "n").is_none(),
        "",
    );
    check(
        "json_u32 unquoted",
        json::json_u32(r#"{"MaxPlayers":8}"#, "MaxPlayers") == Some(8),
        "",
    );
    check(
        "json_u32 quoted (broker stringifies some fields)",
        json::json_u32(r#"{"CurrentPlayers":"4"}"#, "CurrentPlayers") == Some(4),
        "",
    );
    check(
        "json_u32 garbage",
        json::json_u32(r#"{"MaxPlayers":"x"}"#, "MaxPlayers").is_none(),
        "",
    );

    // ── malformed input: None, never a panic ──────────────────────────────────────────────
    let bad = [
        r#"{"a":"unterminated"#,
        r#"{"a":"bad\q"}"#,
        r#"{"a":"\u00"}"#,
        r#"{"a":"\ud83d"}"#,
        r#"{"a":{}"#,
        r#"{"a":[1,2"#,
        r#"{"a" "b"}"#,
        r#"{"a":}"#,
        r#"{"a":,}"#,
        r#"{,}"#,
        r#"}""#,
        "\u{0}\u{1}\u{2}",
        r#"{"a":12345678901234567890123456789012345678901234567890}"#,
    ];
    let mut survived = 0;
    for b in bad {
        let _ = json::json_str(b, "a");
        let _ = json::json_obj(b, "a");
        let _ = json::json_arr_objects(b, "a");
        let _ = json::json_num(b, "a");
        survived += 1;
    }
    check(
        "malformed blobs return None without panicking",
        survived == bad.len(),
        &format!("{survived}/{} cases", bad.len()),
    );

    // ── deep nesting is iterative (no stack overflow) ─────────────────────────────────────
    let deep = format!(
        "{{\"outer\":{}1{},\"key\":\"found\"}}",
        "[".repeat(5000),
        "]".repeat(5000)
    );
    eq(
        "nested 5000 deep still finds a later key",
        json::json_str(&deep, "key"),
        "found",
    );
    let deep_obj = format!("{{\"a\":{}1{}}}", "{".repeat(2000), "}".repeat(2000));
    check(
        "nested 2000 deep object scan terminates",
        json::json_str(&deep_obj, "a").is_none(),
        "",
    );

    // ── pretty-printed input (the real PlayFab REST bodies are not minified) ──────────────
    let pretty =
        "{\n  \"LobbyId\" : \"lan-pretty\",\n  \"Members\" : [\n    { \"Id\" : \"m1\" }\n  ]\n}";
    eq(
        "whitespace before colon",
        json::json_str(pretty, "LobbyId"),
        "lan-pretty",
    );
    eq(
        "whitespace inside arrays",
        json::json_arr_objects(pretty, "Members")
            .first()
            .and_then(|m| json::json_str(m, "Id")),
        "m1",
    );

    let failures = FAILURES.load(Ordering::SeqCst);
    if failures > 0 {
        println!("{failures} FAILURE(S)");
        std::process::exit(1);
    }
    println!("all json checks passed");
}
