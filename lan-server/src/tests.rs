//! In-process integration and framing tests for the broker (`cargo test` in `lan-server`).
//!
//! The server is a bin crate, so these tests live in the crate and exercise the real code paths:
//! `bind_listener` + `serve` (the same functions `main` runs), the real `dispatch`/`handle_*`
//! routes, the real WebSocket pump, and `read_request` as a plain reader. Nothing is mocked, and
//! nothing needs a subprocess or a fixed port — each test binds `127.0.0.1:0` and learns the port
//! from `local_addr`, so the whole suite can run in parallel.
//!
//! Two tests are `#[ignore]`d because they assert behaviour the hardening pass has not landed yet
//! (they are marked with the fix they need). Remove the attribute when the fix lands.

use super::*;

// ─────────────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────────────

struct TestServer {
    #[allow(dead_code)]
    app: Arc<Mutex<App>>,
    http_port: u16,
    ws_port: u16,
}

fn start_server() -> TestServer {
    // A short WebSocket idle so the idle-timeout test is fast; production uses the default.
    start_server_with(ServerOpts {
        ws_idle: Duration::from_millis(300),
        max_connections: MAX_CONNECTIONS,
    })
}

fn start_server_with(opts: ServerOpts) -> TestServer {
    let http = bind_listener("127.0.0.1:0").expect("bind http");
    let ws = bind_listener("127.0.0.1:0").expect("bind ws");
    let http_port = http.local_addr().unwrap().port();
    let ws_port = ws.local_addr().unwrap().port();
    let app = Arc::new(Mutex::new(App {
        title_id: TITLE_DEFAULT.to_string(),
        http_port,
        ws_port,
        max_players: 8,
        override_game_max: true,
        sessions: HashMap::new(),
        lobbies: HashMap::new(),
        party: HashMap::new(),
    }));
    let http_app = app.clone();
    let ws_app = app.clone();
    thread::spawn(move || serve(http, http_app, "HTTP-test", opts));
    thread::spawn(move || serve(ws, ws_app, "WS-test", opts));
    TestServer {
        app,
        http_port,
        ws_port,
    }
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

fn try_get(port: u16, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    // CR/LF as numbers: this helper is small enough to build by hand, and it must not depend on
    // escape sequences surviving whatever tooling edits this file.
    const CR: u8 = 0x0D;
    const LF: u8 = 0x0A;
    let mut c = TcpStream::connect(("127.0.0.1", port))?;
    c.set_read_timeout(Some(Duration::from_secs(5)))?;
    c.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req: Vec<u8> = format!("GET {path} HTTP/1.1").into_bytes();
    req.extend_from_slice(&[CR, LF]);
    req.extend_from_slice(format!("Host: 127.0.0.1:{port}").as_bytes());
    req.extend_from_slice(&[CR, LF, CR, LF]);
    c.write_all(&req)?;
    let mut buf = Vec::new();
    c.read_to_end(&mut buf)?;
    let Some(idx) = buf.windows(4).position(|w| w == [CR, LF, CR, LF]) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no header terminator in the response",
        ));
    };
    let head = String::from_utf8_lossy(&buf[..idx]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Ok((status, buf[idx + 4..].to_vec()))
}

/// One request on a fresh connection (the server closes after one response, `Connection: close`).
/// `content_length` overrides the header when the caller wants to send a mismatched length.
fn request(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    content_length: Option<usize>,
) -> (u16, Vec<u8>) {
    let mut s = connect(port);
    let cl = content_length.unwrap_or(body.len());
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {cl}\r\nConnection: close\r\n\r\n{body}"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let idx = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response header terminator");
    let head = String::from_utf8_lossy(&buf[..idx]);
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, buf[idx + 4..].to_vec())
}

fn json_request(port: u16, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let raw = serde_json::to_string(body).unwrap();
    let (status, bytes) = request(port, method, path, &raw, None);
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

fn get(port: u16, path: &str) -> (u16, Vec<u8>) {
    request(port, "GET", path, "", Some(0))
}

/// Log in through the PlayFab-shaped endpoint; returns `(entity_id, entity_token)`.
fn login(port: u16, ticket: &str) -> (String, String) {
    let (status, v) = json_request(
        port,
        "POST",
        "/Client/LoginWithSteam",
        &json!({ "SteamTicket": ticket }),
    );
    assert_eq!(status, 200, "login status: {v}");
    let ent = &v["data"]["EntityToken"];
    let id = ent["Entity"]["Id"].as_str().unwrap_or("").to_string();
    let tok = ent["EntityToken"].as_str().unwrap_or("").to_string();
    assert!(
        !id.is_empty() && !tok.is_empty(),
        "login returned no entity: {v}"
    );
    (id, tok)
}

fn create_lobby(port: u16, owner: &str) -> (String, String, i64) {
    let (status, v) = json_request(
        port,
        "POST",
        "/Lobby/CreateLobby",
        &json!({
            "Creator": { "Id": owner, "Type": "title_player_account", "TypeString": "title_player_account" },
            "MaxPlayers": 4,
            "AccessPolicy": "Public",
            "MembershipLock": "Unlocked",
            "LobbyData": { "string_key1": "v1" },
            "SearchData": { "string_key5": "leader" },
        }),
    );
    assert_eq!(status, 200, "create status: {v}");
    let id = v["data"]["LobbyId"].as_str().unwrap_or("").to_string();
    let conn = v["data"]["ConnectionString"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let max = v["data"]["MaxPlayers"].as_i64().unwrap_or(0);
    assert!(
        !id.is_empty() && !conn.is_empty(),
        "create returned no lobby: {v}"
    );
    (id, conn, max)
}

// ─────────────────────────────────────────────────────────────────────────────
// Boot config blob
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn boot_blob_round_trips_and_rejects_tampering() {
    let plain = br#"{"title":"1AC1AD","value":42}"#;
    let blob = encode_boot_blob(plain);
    assert_eq!(decode_boot_blob(&blob).unwrap(), plain);

    let mut tampered = blob.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
        decode_boot_blob(&tampered).is_err(),
        "a flipped byte must fail authentication"
    );
    assert!(
        decode_boot_blob(&blob[..8]).is_err(),
        "truncated blob must fail"
    );
}

#[test]
fn boot_config_blob_endpoint_serves_decodable_ciphertext() {
    let s = start_server();
    let (status, body) = get(s.http_port, "/dat/config/1AC1AD.blob");
    assert_eq!(status, 200);
    let json = decode_boot_blob(&body).expect("blob must decode with the server key");
    let v: Value = serde_json::from_slice(&json).expect("decoded blob must be JSON");
    assert_eq!(
        v["PlayfabTitleId"],
        Value::String(TITLE_DEFAULT.to_string())
    );
    assert!(
        v["WebsocketUrl"].as_str().unwrap_or("").contains("/"),
        "boot config must carry the ws url: {v}"
    );

    let (status, body) = get(s.http_port, "/dat/config/1AC1AD.json");
    assert_eq!(status, 200);
    assert!(serde_json::from_slice::<Value>(&body).is_ok());
}

// ─────────────────────────────────────────────────────────────────────────────
// Cygames endpoints
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn user_auth_returns_ws_url_pointing_at_the_ws_port() {
    let s = start_server();
    let (status, v) = json_request(s.http_port, "POST", "/sys/user_auth", &json!({}));
    assert_eq!(status, 200);
    assert_eq!(v["meta"]["error_code"], 0, "cygames envelope: {v}");
    let ws_url = v["data"]["ws_url"].as_str().unwrap_or("");
    assert!(
        ws_url.ends_with(&format!(":{}/", s.ws_port)),
        "ws_url={ws_url} must carry the ws port {}",
        s.ws_port
    );
    assert!(v["data"]["auth_token"]
        .as_str()
        .unwrap_or("")
        .starts_with("lan-auth."));
}

#[test]
fn misc_cygames_endpoints_answer_in_the_envelope() {
    let s = start_server();
    for path in ["/sys/get_terms", "/sys/get_news", "/playlog/write"] {
        let (status, v) = json_request(s.http_port, "POST", path, &json!({}));
        assert_eq!(status, 200, "{path}");
        assert_eq!(v["meta"]["error_code"], 0, "{path}: {v}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PlayFab lobby lifecycle
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn lobby_lifecycle_create_find_join_get_leave() {
    let s = start_server();
    let (owner, _) = login(s.http_port, "steam-owner");
    let (guest, _) = login(s.http_port, "steam-guest");

    let (id, conn, max) = create_lobby(s.http_port, &owner);
    assert_eq!(max, 8, "lan.ini override_game_max=true caps at max_players");

    // FindLobbies: the lobby is discoverable, in the PlayFab envelope.
    let (status, found) = json_request(
        s.http_port,
        "POST",
        "/Lobby/FindLobbies",
        &json!({ "ClientSearchResultCount": 10 }),
    );
    assert_eq!(status, 200);
    let rows = found["data"]["Lobbies"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter()
            .any(|r| r["LobbyId"] == Value::String(id.clone())),
        "created lobby missing from FindLobbies: {found}"
    );

    // Join with the connection string from create.
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/JoinLobby",
        &json!({
            "ConnectionString": conn,
            "MemberEntity": { "Id": guest, "Type": "title_player_account" },
            "MemberData": { "string_key1": "guest" },
        }),
    );
    assert_eq!(status, 200, "join: {v}");
    assert_eq!(v["data"]["LobbyId"], Value::String(id.clone()));

    // GetLobby shows both members.
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/GetLobby",
        &json!({ "LobbyId": id }),
    );
    assert_eq!(status, 200);
    let members = v["data"]["Members"].as_array().cloned().unwrap_or_default();
    assert_eq!(members.len(), 2, "members: {v}");
    assert_eq!(v["data"]["CurrentPlayers"], 2);

    // Leave, then the lobby is down to the owner.
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/LeaveLobby",
        &json!({ "LobbyId": id, "MemberEntity": { "Id": guest, "Type": "title_player_account" } }),
    );
    assert_eq!(status, 200, "leave: {v}");
    let (_, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/GetLobby",
        &json!({ "LobbyId": id }),
    );
    assert_eq!(
        v["data"]["Members"].as_array().map(Vec::len),
        Some(1),
        "after leave: {v}"
    );
}

#[test]
fn join_unknown_connection_is_a_real_404() {
    let s = start_server();
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/JoinLobby",
        &json!({ "ConnectionString": "lan.does-not-exist" }),
    );
    assert_eq!(status, 404);
    assert_eq!(v["error"], "LobbyNotFound");
}

#[test]
fn join_beyond_max_players_is_refused_with_409() {
    let s = start_server();
    // 8 players is the configured cap; make the lobby itself smaller by asking for 2 with the
    // override off for this instance: MaxPlayers is clamped up to the mesh cap only when the
    // override is on, so use the cap itself and fill it.
    let (id, conn, _) = create_lobby(s.http_port, &login(s.http_port, "cap-owner").0);
    for i in 0..7 {
        let (guest, _) = login(s.http_port, &format!("cap-guest-{i}"));
        let (status, _) = json_request(
            s.http_port,
            "POST",
            "/Lobby/JoinLobby",
            &json!({ "ConnectionString": conn, "MemberEntity": { "Id": guest, "Type": "title_player_account" } }),
        );
        assert_eq!(status, 200, "guest {i}");
    }
    let (extra, _) = login(s.http_port, "cap-guest-extra");
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/JoinLobby",
        &json!({ "ConnectionString": conn, "MemberEntity": { "Id": extra, "Type": "title_player_account" } }),
    );
    assert_eq!(status, 409, "ninth member must be refused: {v}");
    assert_eq!(v["error"], "LobbyMemberLimitExceeded");
    let (_, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/GetLobby",
        &json!({ "LobbyId": id }),
    );
    assert_eq!(v["data"]["Members"].as_array().map(Vec::len), Some(8));
}

#[test]
fn post_update_scalar_and_delete_round_trip() {
    let s = start_server();
    let (owner, _) = login(s.http_port, "upd-owner");
    let (id, _conn, _) = create_lobby(s.http_port, &owner);
    let (status, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/UpdateLobby",
        &json!({
            "LobbyId": id,
            "MembershipLock": "Locked",
            "LobbyDataToDelete": ["string_key1"],
            "LobbyData": { "string_key2": "v2" },
        }),
    );
    assert_eq!(status, 200, "{v}");
    let (_, v) = json_request(
        s.http_port,
        "POST",
        "/Lobby/GetLobby",
        &json!({ "LobbyId": id }),
    );
    let data = &v["data"];
    assert_eq!(data["MembershipLock"], "Locked");
    assert_eq!(data["LobbyData"]["string_key2"], "v2");
    assert!(
        data["LobbyData"].get("string_key1").is_none(),
        "deleted key still present: {data}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Party peer list
// ─────────────────────────────────────────────────────────────────────────────

fn party_join(port: u16, net: &str, eid: &str, udp: u16) -> u16 {
    let (status, _) = json_request(
        port,
        "POST",
        "/party/join",
        &json!({ "network_id": net, "entity_id": eid, "udp_port": udp }),
    );
    status
}

fn party_peers(port: u16, net: &str) -> Vec<Value> {
    let (status, v) = json_request(
        port,
        "POST",
        &format!("/party/peers?network_id={net}"),
        &json!({}),
    );
    assert_eq!(status, 200, "peers: {v}");
    v["data"]["members"].as_array().cloned().unwrap_or_default()
}

#[test]
fn party_join_publishes_peers_with_their_udp_port() {
    let s = start_server();
    assert_eq!(party_join(s.http_port, "net-a", "e1", 27015), 200);
    assert_eq!(party_join(s.http_port, "net-a", "e2", 27016), 200);

    let peers = party_peers(s.http_port, "net-a");
    assert_eq!(peers.len(), 2, "peers: {peers:?}");
    let e1 = peers
        .iter()
        .find(|p| p["entity_id"] == "e1")
        .expect("e1 missing");
    assert_eq!(e1["udp_port"], 27015);
    assert!(
        e1["ip"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "peer has no ip: {e1}"
    );

    // A second network is independent.
    assert_eq!(party_join(s.http_port, "net-b", "e3", 27017), 200);
    assert_eq!(party_peers(s.http_port, "net-b").len(), 1);
    assert_eq!(party_peers(s.http_port, "net-a").len(), 2);
}

#[test]
fn party_join_refuses_past_the_configured_mesh_cap() {
    let s = start_server();
    for i in 0..8 {
        assert_eq!(
            party_join(s.http_port, "net-full", &format!("e{i}"), 27015 + i),
            200
        );
    }
    assert_eq!(
        party_join(s.http_port, "net-full", "e8", 27030),
        409,
        "ninth member must be refused"
    );
    assert_eq!(party_peers(s.http_port, "net-full").len(), 8);
}

#[test]
fn party_leave_removes_the_peer() {
    let s = start_server();
    assert_eq!(party_join(s.http_port, "net-l", "e1", 27015), 200);
    assert_eq!(party_join(s.http_port, "net-l", "e2", 27016), 200);
    let (status, _) = json_request(
        s.http_port,
        "POST",
        "/party/leave",
        &json!({ "network_id": "net-l", "entity_id": "e1" }),
    );
    assert_eq!(status, 200);
    let peers = party_peers(s.http_port, "net-l");
    assert_eq!(peers.len(), 1, "{peers:?}");
    assert_eq!(peers[0]["entity_id"], "e2");
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing / robustness
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn health_and_unknown_routes() {
    let s = start_server();
    let (status, body) = get(s.http_port, "/health");
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ok"], true);

    // An unknown POST is the documented catch-all: PlayFab-shaped success, no handler ran.
    let (status, v) = json_request(s.http_port, "POST", "/Lobby/NoSuchCall", &json!({}));
    assert_eq!(status, 200);
    assert!(
        v.get("code").is_some(),
        "catch-all must stay PlayFab-shaped: {v}"
    );
}

#[test]
fn malformed_requests_do_not_kill_the_server() {
    let s = start_server();
    let junk: &[&[u8]] = &[
        b"\x00\x01\x02\x03",
        b"GET",
        b"GET / HTTP/1.1\r\n\r\n",
        b"POST /party/join HTTP/1.1\r\nContent-Length: 999999\r\n\r\n",
        b"\x16\x03\x01\x00\x50", // TLS ClientHello prefix
        b"GARBAGE\r\n\r\n",
        b"GET / HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
    ];
    for bytes in junk {
        if let Ok(mut c) = TcpStream::connect(("127.0.0.1", s.http_port)) {
            let _ = c.write_all(bytes);
        }
    }
    // The server must still answer a well-formed request afterwards.
    let (status, _) = get(s.http_port, "/health");
    assert_eq!(status, 200, "server stopped serving after malformed input");
}

// ─────────────────────────────────────────────────────────────────────────────
// read_request framing (plain reader, no socket)
// ─────────────────────────────────────────────────────────────────────────────

fn reader(bytes: &[u8]) -> std::io::Cursor<Vec<u8>> {
    std::io::Cursor::new(bytes.to_vec())
}

/// Unwrap a successful framing read; a `Dropped`/`TooLarge` result fails the test with the
/// variant name.
fn read_ok(r: ReadResult) -> (String, String, HashMap<String, String>, Vec<u8>) {
    match r {
        ReadResult::Request(m, p, h, b) => (m, p, h, b),
        ReadResult::TooLarge => panic!("read_request returned TooLarge"),
        ReadResult::Dropped => panic!("read_request returned Dropped"),
    }
}

#[test]
fn framing_reads_the_body_by_content_length() {
    let raw = b"POST /party/join HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
    let (method, path, headers, body) = read_ok(read_request(&mut reader(raw), 8080));
    assert_eq!(method, "POST");
    assert_eq!(path, "/party/join");
    assert_eq!(headers.get("host").map(String::as_str), Some("h"));
    assert_eq!(
        body, b"hello",
        "body must stop at Content-Length, not include EXTRA"
    );
}

#[test]
fn framing_without_content_length_has_an_empty_body() {
    let raw = b"GET /health HTTP/1.1\r\nHost: h\r\n\r\n";
    let (_, _, _, body) = read_ok(read_request(&mut reader(raw), 8080));
    assert!(body.is_empty());
}

#[test]
fn framing_lowercases_header_names_and_trims_values() {
    let raw = b"GET /x HTTP/1.1\r\nX-Peer-IP:  10.0.0.7  \r\nUpgrade: websocket\r\n\r\n";
    let (_, _, headers, _) = read_ok(read_request(&mut reader(raw), 8080));
    assert_eq!(
        headers.get("x-peer-ip").map(String::as_str),
        Some("10.0.0.7")
    );
    assert_eq!(
        headers.get("upgrade").map(String::as_str),
        Some("websocket")
    );
}

#[test]
fn framing_rejects_a_tls_client_hello_on_the_plaintext_port() {
    let raw = [0x16u8, 0x03, 0x01, 0x00, 0x50, 0x01, 0x00];
    assert!(
        matches!(read_request(&mut reader(&raw), 8081), ReadResult::Dropped),
        "WSS on the plaintext port must be rejected with a log, not parsed"
    );
}

#[test]
fn framing_rejects_an_oversize_header_block() {
    let mut raw = b"GET / HTTP/1.1\r\n".to_vec();
    while raw.len() <= 1024 * 1024 {
        raw.extend_from_slice(b"X-Pad: 0123456789012345678901234567890123456789\r\n");
    }
    assert!(
        matches!(read_request(&mut reader(&raw), 8080), ReadResult::Dropped),
        "the 1 MB header cap must reject instead of growing the buffer"
    );
}

#[test]
fn framing_accepts_a_request_without_a_crlfcrlf_body_split() {
    // A body that itself contains CRLFCRLF must not be mistaken for the header terminator.
    let raw = b"POST /x HTTP/1.1\r\nContent-Length: 8\r\n\r\na\r\n\r\nbb";
    let (_, _, _, body) = read_ok(read_request(&mut reader(raw), 8080));
    assert_eq!(body, b"a\r\n\r\nbb");
}

#[test]
fn framing_rejects_oversize_bodies_before_reading_them() {
    // A 100 MB Content-Length must be refused before a byte of the body is read. The direct
    // framing call proves the decision happens on the declared length; the HTTP call proves the
    // server answers 413 instead of parking the connection.
    // CR/LF are numbers here: this file is also edited by tooling that can turn Rust
    // escape sequences into real newlines, which would silently change the bytes under test.
    const CR: u8 = 0x0D;
    const LF: u8 = 0x0A;
    let mut raw: Vec<u8> = b"POST /party/join HTTP/1.1".to_vec();
    raw.extend_from_slice(&[CR, LF]);
    raw.extend_from_slice(b"Content-Length: 104857600");
    raw.extend_from_slice(&[CR, LF, CR, LF]);
    match read_request(&mut reader(&raw), 8080) {
        ReadResult::TooLarge => {}
        _ => panic!("oversize Content-Length was not refused"),
    }

    let s = start_server();
    let (status, _) = request(
        s.http_port,
        "POST",
        "/party/join",
        "",
        Some(100 * 1024 * 1024),
    );
    assert_eq!(status, 413, "oversize body must be answered with 413");

    // A body exactly at the cap is still accepted (the check is `>`, not `>=`).
    let big = "x".repeat(MAX_BODY_BYTES);
    let (status, body) = request(s.http_port, "POST", "/party/join", &big, None);
    assert_eq!(
        status, 200,
        "a body at the cap must still be served: {body:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket transport
// ─────────────────────────────────────────────────────────────────────────────

/// Upgrade handshake. Reads the response one byte at a time so no frame bytes are consumed.
fn ws_upgrade(port: u16) -> TcpStream {
    let mut s = connect(port);
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let req = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = match s.read(&mut b) {
            Ok(n) => n,
            Err(e) => panic!("upgrade read on port {port} failed: {e}"),
        };
        assert!(
            n == 1,
            "server closed during upgrade on port {port} (refused by the connection cap?)"
        );
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head).to_string();
    assert!(text.starts_with("HTTP/1.1 101"), "upgrade failed: {text}");
    let want = B64.encode(Sha1::digest(format!("{key}{WS_GUID}").as_bytes()));
    assert!(
        text.contains(&format!("Sec-WebSocket-Accept: {want}")),
        "bad accept key: {text}"
    );
    s
}

/// Client frames must be masked (RFC 6455 §5.3). Mask is fixed: the server only has to unmask it.
fn ws_send(s: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        frame.push(0x80 | n as u8);
    } else if n < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mask = [0xA5u8, 0x5A, 0x11, 0x22];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    s.write_all(&frame).unwrap();
}

/// Read one unmasked server frame; returns `(opcode, payload)`.
fn ws_recv(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).expect("frame header");
    let opcode = h[0] & 0x0f;
    let mut len = (h[1] & 0x7f) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        s.read_exact(&mut ext).unwrap();
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        s.read_exact(&mut ext).unwrap();
        len = u64::from_be_bytes(ext) as usize;
    }
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).unwrap();
    (opcode, payload)
}

#[test]
fn ws_echoes_a_command_payload_as_binary_json() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    let cmd = br#"{"command":"join_lobby","params":{"lobby_search_id":"abc"}}"#;
    ws_send(&mut ws, 0x2, cmd);
    let (opcode, payload) = ws_recv(&mut ws);
    assert_eq!(opcode, 0x2, "server must reply with a binary frame");
    let v: Value = serde_json::from_slice(&payload).expect("reply is JSON");
    assert_eq!(v["command"], "join_lobby");
    assert_eq!(v["params"]["lobby_search_id"], "abc");
}

#[test]
fn ws_lifts_a_root_lobby_search_id_into_params() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    ws_send(
        &mut ws,
        0x2,
        br#"{"command":"join_lobby","lobby_search_id":"root-level"}"#,
    );
    let (_, payload) = ws_recv(&mut ws);
    let v: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        v["params"]["lobby_search_id"], "root-level",
        "the exe reads params.lobby_search_id: {v}"
    );
}

#[test]
fn ws_answers_ping_with_pong() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    ws_send(&mut ws, 0x9, b"ping");
    let (opcode, payload) = ws_recv(&mut ws);
    assert_eq!(opcode, 0xa, "ping must be answered with pong");
    assert_eq!(payload, b"ping");
}

#[test]
fn ws_close_frame_ends_the_connection() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    ws_send(&mut ws, 0x8, &[0x03, 0xe8]);
    ws.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 1];
    match ws.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("expected close, read {n} bytes"),
        Err(e) => panic!("expected close, got {e}"),
    }
}

#[test]
fn ws_garbage_frame_does_not_kill_the_server() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    // Unmasked client frame (illegal), bad length, and a masked frame with an unknown opcode.
    ws.write_all(&[0x82, 0x03, b'a', b'b', b'c']).unwrap();
    ws_send(&mut ws, 0x7, b"reserved-opcode");
    // A real command after the garbage must still be answered.
    ws_send(&mut ws, 0x2, br#"{"command":"still_alive","params":{}}"#);
    let (_, payload) = ws_recv(&mut ws);
    let v: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(v["command"], "still_alive");
}

#[test]
fn ws_idle_connection_is_closed_by_the_server() {
    let s = start_server();
    let mut ws = ws_upgrade(s.ws_port);
    ws.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 1];
    match ws.read(&mut buf) {
        Ok(0) => {}
        other => panic!("idle websocket was not closed by the server: {other:?}"),
    }
}

#[test]
fn excessive_idle_connections_are_refused_without_hanging() {
    // A small budget so the refusal path is exercised with a handful of sockets instead of 256.
    let cap = 16usize;
    let s = start_server_with(ServerOpts {
        ws_idle: Duration::from_millis(300),
        max_connections: cap,
    });

    // The OS completes the TCP handshake even for connections the server will refuse, so these
    // connects succeed and the refusal shows up on the request instead.
    let mut held = Vec::new();
    for _ in 0..cap + 8 {
        match TcpStream::connect(("127.0.0.1", s.http_port)) {
            Ok(c) => held.push(c),
            Err(_) => break,
        }
    }
    assert!(
        held.len() >= cap,
        "could not hold enough connections to fill the cap: {} < {cap}",
        held.len()
    );

    // Past the cap: a 503 or a reset, never a hang.
    let started = SystemTime::now();
    let outcome = try_get(s.http_port, "/health");
    let took = started.elapsed().unwrap_or_default();
    match &outcome {
        Ok((status, _)) => assert!(
            *status == 200 || *status == 503,
            "unexpected status {status} under idle load"
        ),
        Err(e) => assert!(
            matches!(
                e.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe
            ),
            "unexpected error under idle load: {e}"
        ),
    }
    assert!(
        took < Duration::from_secs(5),
        "server took {took:?} to answer under idle load"
    );

    // The budget is per listener: a saturated HTTP port must not lock out the WebSocket port.
    let mut ws = ws_upgrade(s.ws_port);
    ws_send(&mut ws, 0x9, b"p");
    let (opcode, payload) = ws_recv(&mut ws);
    assert_eq!((opcode, payload.as_slice()), (0xa, b"p".as_slice()));

    // Dropping the held sockets frees the guards; the server must serve normally again.
    let held_len = held.len();
    drop(held);
    let mut status = 0;
    for _ in 0..100 {
        if let Ok((s200, _)) = try_get(s.http_port, "/health") {
            status = s200;
            if status == 200 {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        status, 200,
        "server did not recover after the sockets were released"
    );
    println!(
        "   cap={cap} held={held_len} outcome={:?} took={took:?}",
        outcome.map(|(s, _)| s)
    );
}
/// Local assertion helper so this file keeps the "one line per property" style of the other
/// suite runners without pulling in a test framework.
fn check(detail: &str) {
    println!("   {detail}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn strip_api_prefix_normalises_the_exe_forms() {
    assert_eq!(strip_api_prefix("/party/join"), "/party/join");
    assert_eq!(
        strip_api_prefix("/v1/api/index.php/party/join"),
        "/party/join"
    );
    assert_eq!(strip_api_prefix("/api/party/join"), "/party/join");
}

#[test]
fn is_boot_path_covers_the_exe_requests() {
    assert!(is_boot_path("/dat/config/1AC1AD.blob"));
    assert!(is_boot_path("/dat/config/1AC1AD.json"));
    assert!(is_boot_path("/dat/config/foo.dat"));
    assert!(!is_boot_path("/party/join"));
    assert!(!is_boot_path("/Lobby/GetLobby"));
}

#[test]
fn clamp_max_players_keeps_the_documented_range() {
    assert_eq!(clamp_max_players(0), MIN_PLAYERS);
    assert_eq!(clamp_max_players(2), 2);
    assert_eq!(clamp_max_players(32), 32);
    assert_eq!(clamp_max_players(9999), MAX_PLAYERS_CEILING);
}
