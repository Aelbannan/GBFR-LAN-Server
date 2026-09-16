//! GBFR LAN broker: Cygames HTTP/WS + PlayFab lobby REST + Party peer list.
//! HTTP only. Clients read [server] host/port from lan.ini.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

#[path = "../../common/lan_cfg.rs"]
mod lan_cfg;
mod lobby_filter;

const TITLE_DEFAULT: &str = "1AC1AD";
const BLOB_KEY: &[u8; 32] = b"kdfg8kojildksuie23jsdfg8fg7klsdx";
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MIN_PLAYERS: i64 = 2;
const MAX_PLAYERS_CEILING: i64 = 32;
const DEFAULT_MAX_PLAYERS: i64 = 8;
/// A party member is live for this long after its last `/party/join` or entity-tagged
/// `/party/peers` (LS-2).
const PARTY_SEEN_TTL_SECS: f64 = 30.0;
/// Minimum spacing between repeats of the same new diagnostic line.
const RATE_LIMIT_SECS: f64 = 30.0;

struct Player {
    playfab_id: String,
    entity_id: String,
    entity_type: String,
    entity_token: String,
    session_ticket: String,
    token_expiration: String,
}

struct Lobby {
    id: String,
    connection: String,
    owner: Value,
    /// PlayFab `PFLobbyOwnerMigrationPolicy` chosen by the title at create (0 = None, the
    /// value this game sends; 1 = Automatic, 2 = Manual, 3 = Server). It cannot change.
    /// When the owner leaves, Automatic migrates the owner and None/Manual clears it; the
    /// lobby itself only dies with its last member (client-owned lobby semantics).
    owner_migration_policy: i64,
    max_players: i64,
    lobby_data: Map<String, Value>,
    search_data: Map<String, Value>,
    access_policy: String,
    /// Service-assigned membership lock: "Unlocked" (default) or "Locked". The genuine
    /// UpdateLobby request carries `MembershipLock` and FindLobbies filters on
    /// `lobby/membershipLock`, so it is stored rather than synthesised per response.
    membership_lock: String,
    members: Vec<Value>,
    created: f64,
}

struct PartyMember {
    entity_id: String,
    ip: String,
    udp_port: u16,
    seen: f64,
    /// Client-supplied monotonic registration counter (`member_seq` / `join_epoch`). 0 means
    /// the client sent none (every shim today), so a leave cannot be compared and keeps its
    /// old unconditional semantics (LS-6).
    member_seq: u64,
}

struct App {
    title_id: String,
    http_port: u16,
    ws_port: u16,
    max_players: i64,
    override_game_max: bool,
    sessions: HashMap<String, Arc<Player>>,
    lobbies: HashMap<String, Lobby>,
    party: HashMap<String, HashMap<String, PartyMember>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_unix() -> f64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    d.as_secs() as f64 + d.subsec_nanos() as f64 / 1e9
}

fn utc_iso(_hours: f64) -> String {
    "2027-01-01T00:00:00.000Z".into()
}

fn clamp_max_players(n: i64) -> i64 {
    n.clamp(MIN_PLAYERS, MAX_PLAYERS_CEILING)
}

fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn playfab_ok(data: Value) -> (u16, &'static str, Vec<u8>) {
    let body = json!({"code": 200, "status": "OK", "data": data});
    (
        200,
        "application/json",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn playfab_err(error: &str, message: &str) -> (u16, &'static str, Vec<u8>) {
    // Real HTTP status, so a client can tell failure from success without parsing the body.
    // The body keeps the PlayFab-style code/error fields for clients that parse the envelope.
    let (status, code, status_text) = match error {
        "LobbyNotFound" => (404u16, 404u32, "NotFound"),
        "LobbyMemberLimitExceeded" | "PartyMemberLimitExceeded" => (409, 409, "Conflict"),
        _ => (400, 400, "BadRequest"),
    };
    let body = json!({
        "code": code,
        "status": status_text,
        "error": error,
        "errorCode": 1114,
        "errorMessage": message,
    });
    (
        status,
        "application/json",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// Char-boundary-safe truncation. Byte slicing (`&s[..n]`) panics when n lands inside a
/// multi-byte character, and a panic while the App mutex is held poisons it for good.
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

fn cygames_ok(data: Value) -> (u16, &'static str, Vec<u8>) {
    let body = json!({
        "meta": {"error_code": 0},
        "common": [],
        "data": data,
        "responseCode": 0,
    });
    (
        200,
        "application/json",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn encode_boot_blob(plaintext: &[u8]) -> Vec<u8> {
    let key = Key::from_slice(BLOB_KEY);
    let cipher = ChaCha20Poly1305::new(key);
    let n = now_secs().to_le_bytes();
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes[..8].copy_from_slice(&n);
    nonce_bytes[8..].copy_from_slice(&n[..4]);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, plaintext).unwrap_or_default();
    let mut packed = Vec::with_capacity(12 + ct.len());
    packed.extend_from_slice(&nonce_bytes);
    packed.extend_from_slice(&ct);
    B64.encode(packed).into_bytes()
}

fn decode_boot_blob(body: &[u8]) -> Result<Vec<u8>, String> {
    let packed = B64.decode(body).map_err(|e| e.to_string())?;
    if packed.len() < 28 {
        return Err("boot blob shorter than nonce+tag".into());
    }
    let key = Key::from_slice(BLOB_KEY);
    let cipher = ChaCha20Poly1305::new(key);
    let nonce = Nonce::from_slice(&packed[..12]);
    cipher
        .decrypt(nonce, packed[12..].as_ref())
        .map_err(|e| e.to_string())
}

fn json_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn json_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_u64().map(|n| n as i64))
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    })
}

/// Read an array-of-strings field under any of its aliases (the genuine Lobby model spells the
/// *ToDelete fields in PascalCase; accept the camelCase wire spelling too).
fn json_str_array(v: &Value, keys: &[&str]) -> Vec<String> {
    for key in keys {
        if let Some(arr) = v.get(*key).and_then(|x| x.as_array()) {
            return arr
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
        }
    }
    Vec::new()
}

/// Client monotonic join counter. Accepts `member_seq` (or `join_epoch`/`join_seq`) from the
/// JSON body or the query string. 0 = absent.
fn member_seq_of(body: &Value, qs: &str) -> u64 {
    for key in ["member_seq", "join_epoch", "join_seq"] {
        if let Some(n) = json_i64(body, key) {
            if n > 0 {
                return n as u64;
            }
        }
        let pat = format!("{key}=");
        if let Some(v) = qs
            .split('&')
            .find_map(|kv| kv.strip_prefix(pat.as_str()))
            .and_then(|s| s.parse::<u64>().ok())
        {
            if v > 0 {
                return v;
            }
        }
    }
    0
}

fn member_id(member: &Value) -> String {
    if let Some(id) = json_str(member, "Id") {
        return id.to_string();
    }
    member
        .get("MemberEntity")
        .and_then(|e| json_str(e, "Id"))
        .unwrap_or("")
        .to_string()
}

fn decimal_account_id(entity_id: &str) -> String {
    if !entity_id.is_empty() && entity_id.chars().all(|c| c.is_ascii_digit()) {
        return entity_id.to_string();
    }
    let mut n: u64 = 2166136261;
    for b in entity_id.as_bytes() {
        n ^= *b as u64;
        n = n.wrapping_mul(16777619);
    }
    if n == 0 {
        n = 1;
    }
    n.to_string()
}

fn value_as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
        _ => None,
    }
}

/// Read-only diagnostic: sorted key list of a lobby data/search object.
fn map_keys(m: &Map<String, Value>) -> String {
    let mut keys: Vec<String> = m.keys().cloned().collect();
    keys.sort();
    keys.join(",")
}

fn fill_member_data(data: &mut Map<String, Value>, eid: &str) {
    for key in [
        "member_platform",
        "member_platform_account_id",
        "member_platform_user_name",
    ] {
        if let Some(v) = data.get(key) {
            if !v.is_string() {
                if let Some(s) = value_as_string(v) {
                    data.insert(key.into(), Value::String(s));
                }
            }
        }
    }
    let plat = data
        .get("member_platform")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // "Steam" is the PC property string; "1" is the Steam enum and fails FUN_140ad6eb0.
    if plat.is_empty() || plat == "1" {
        data.insert("member_platform".into(), json!("Steam"));
    }
    let acct_ok = data
        .get("member_platform_account_id")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if !acct_ok {
        data.insert(
            "member_platform_account_id".into(),
            json!(decimal_account_id(eid)),
        );
    }
    if !data.contains_key("member_platform_user_name") {
        data.insert("member_platform_user_name".into(), json!("LAN"));
    }
}

fn members_for_client(lobby: &Lobby) -> Vec<Value> {
    lobby
        .members
        .iter()
        .map(|m| {
            let mut m = m.clone();
            let eid = member_id(&m);
            let mut data = m
                .get("MemberData")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            fill_member_data(&mut data, &eid);
            if let Some(obj) = m.as_object_mut() {
                obj.insert("MemberData".into(), Value::Object(data));
            }
            m
        })
        .collect()
}

fn is_loopback_ip(s: &str) -> bool {
    let s = s.trim();
    s == "127.0.0.1" || s == "::1" || s == "0.0.0.0" || s.starts_with("127.")
}

fn lobby_owner_id(lobby: &Lobby) -> String {
    json_str(&lobby.owner, "Id")
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            lobby
                .owner
                .as_str()
                .unwrap_or("")
                .to_string()
        })
}

fn lobby_lan1_network_id(lobby: &Lobby) -> Option<String> {
    let desc = lobby
        .lobby_data
        .get("network_descriptor")
        .and_then(|v| v.as_str())?;
    let rest = desc.strip_prefix("LAN1.")?;
    let nid = rest.split('.').next().unwrap_or("");
    if nid.len() >= 8 {
        Some(nid.to_string())
    } else {
        None
    }
}

fn lobby_owner_in_party(app: &App, lobby: &Lobby, now: f64) -> bool {
    let Some(nid) = lobby_lan1_network_id(lobby) else {
        return false;
    };
    let owner = lobby_owner_id(lobby);
    if owner.is_empty() {
        return false;
    }
    app.party
        .get(&nid)
        .and_then(|net| net.get(&owner))
        .map(|m| now - m.seen <= PARTY_SEEN_TTL_SECS)
        .unwrap_or(false)
}

fn drop_owner_lobbies(app: &mut App, owner_id: &str) {
    if owner_id.is_empty() {
        return;
    }
    let stale: Vec<String> = app
        .lobbies
        .iter()
        .filter(|(_, l)| lobby_owner_id(l) == owner_id)
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        app.lobbies.remove(&id);
        request_log(&format!(
            "CreateLobby drop stale {id} owner={owner_id}"
        ));
    }
}

fn request_entity_id(app: &App, body: &Value, headers: &HashMap<String, String>) -> Option<String> {
    for hk in ["x-entitytoken", "x-authorization", "x-authentication"] {
        if let Some(tok) = headers.get(hk) {
            if let Some(p) = app.sessions.get(tok) {
                return Some(p.entity_id.clone());
            }
        }
    }
    if let Some(s) = json_str(body, "EntityId").or_else(|| json_str(body, "entityId")) {
        return Some(s.to_string());
    }
    for key in ["MemberEntity", "Owner", "SearchingEntity", "Entity"] {
        if let Some(id) = body.get(key).and_then(|e| json_str(e, "Id")) {
            return Some(id.to_string());
        }
    }
    None
}

fn map_nonempty(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(value_as_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn string_key5_leader_name(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object()?;
    obj.get("leader_name")
        .and_then(value_as_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn owner_member_user_name(lobby: &Lobby) -> Option<String> {
    let oid = lobby_owner_id(lobby);
    if oid.is_empty() {
        return None;
    }
    lobby.members.iter().find_map(|m| {
        if member_id(m) != oid {
            return None;
        }
        m.get("MemberData")
            .and_then(|d| d.as_object())
            .and_then(|d| map_nonempty(d, "member_platform_user_name"))
    })
}

fn lobby_display_name(lobby: &Lobby) -> String {
    map_nonempty(&lobby.lobby_data, "leader_name")
        .or_else(|| map_nonempty(&lobby.lobby_data, "leader_platform_user_name"))
        .or_else(|| owner_member_user_name(lobby))
        .filter(|s| s != "LAN")
        .unwrap_or_else(|| "LAN".into())
}

/// Session search reads FindLobbies `string_key5` as a JSON object and copies
/// `leader_name`. The exe often posts a formatted int (or JSON with an empty
/// name) in that slot; rewrite it from lobby properties when it is not usable.
fn ensure_search_leader_name(sd: &mut Map<String, Value>, lobby: &Lobby) {
    let usable = match sd.get("string_key5") {
        Some(Value::String(s)) => string_key5_leader_name(s).is_some(),
        Some(Value::Object(o)) => o
            .get("leader_name")
            .and_then(value_as_string)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        _ => false,
    };
    if usable {
        if matches!(sd.get("string_key5"), Some(Value::Object(_))) {
            if let Some(obj) = sd.remove("string_key5") {
                sd.insert(
                    "string_key5".into(),
                    Value::String(serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into())),
                );
            }
        }
        return;
    }
    let leader = lobby_display_name(lobby);
    let nested = json!({
        "leader_name": leader,
        "leader_platform": map_nonempty(&lobby.lobby_data, "leader_platform").unwrap_or_else(|| "1".into()),
        "leader_platform_account_id": map_nonempty(&lobby.lobby_data, "leader_platform_account_id").unwrap_or_else(|| "1".into()),
        "leader_platform_user_name": map_nonempty(&lobby.lobby_data, "leader_platform_user_name").unwrap_or_else(|| leader.clone()),
    });
    sd.insert(
        "string_key5".into(),
        Value::String(serde_json::to_string(&nested).unwrap_or_else(|_| "{}".into())),
    );
}

fn search_data_for_find(lobby: &Lobby) -> Map<String, Value> {
    let mut sd = lobby.search_data.clone();
    ensure_search_leader_name(&mut sd, lobby);
    if !sd.contains_key("string_key8") {
        let oid = lobby_owner_id(lobby);
        if !oid.is_empty() {
            sd.insert("string_key8".into(), Value::String(oid));
        }
    }
    sd
}

fn lobby_has_entity(lobby: &Lobby, eid: &str) -> bool {
    if eid.is_empty() {
        return false;
    }
    if lobby.members.iter().any(|m| member_id(m) == eid) {
        return true;
    }
    lobby_owner_id(lobby) == eid
}

fn lobby_public(lobby: &Lobby) -> Value {
    json!({
        "LobbyId": lobby.id,
        "ConnectionString": lobby.connection,
        "Owner": lobby.owner,
        "OwnerMigrationPolicy": lobby.owner_migration_policy,
        "MaxPlayers": lobby.max_players,
        "CurrentPlayers": lobby.members.len(),
        "MembershipLock": lobby.membership_lock,
        "AccessPolicy": lobby.access_policy,
        "LobbyData": lobby.lobby_data,
        "SearchData": lobby.search_data,
        "Members": members_for_client(lobby),
    })
}

fn player_from_ticket(app: &mut App, ticket: &str) -> Arc<Player> {
    let digest = sha1_hex(if ticket.is_empty() { "empty" } else { ticket });
    let playfab_id = digest[..16].to_ascii_uppercase();
    let entity_id = sha1_hex(&format!("tpa:{playfab_id}"))[..16].to_string();
    let token = format!(
        "STUB.{}",
        B64.encode(entity_id.as_bytes())
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_")
    );
    let player = Arc::new(Player {
        playfab_id: playfab_id.clone(),
        entity_id: entity_id.clone(),
        entity_type: "title_player_account".into(),
        entity_token: token.clone(),
        session_ticket: format!("STUB-SESSION-{playfab_id}"),
        token_expiration: utc_iso(12.0),
    });
    app.sessions.insert(token, player.clone());
    app.sessions
        .insert(player.session_ticket.clone(), player.clone());
    player
}

fn login_result(p: &Player) -> Value {
    json!({
        "SessionTicket": p.session_ticket,
        "PlayFabId": p.playfab_id,
        "NewlyCreated": true,
        "SettingsForUser": {
            "NeedsAttribution": false,
            "GatherDeviceInfo": false,
            "GatherFocusInfo": false,
        },
        "LastLoginTime": utc_iso(0.0),
        "EntityToken": {
            "EntityToken": p.entity_token,
            "TokenExpiration": p.token_expiration,
            "Entity": {
                "Id": p.entity_id,
                "Type": p.entity_type,
                "TypeString": p.entity_type,
            },
        },
        "TreatmentAssignment": {"Variants": [], "Variables": []},
    })
}

fn entity_obj(p: &Player) -> Value {
    json!({
        "Id": p.entity_id,
        "Type": p.entity_type,
        "TypeString": p.entity_type,
    })
}

/// The one configured mesh cap: lan.ini `[lobby] max_players`, clamped to the protocol range.
/// The lobby maximum and the `party/join` refusal both derive from this, so the broker's cap
/// can no longer drift from the setting (LS-10). The Party shim's own 8-remote ceiling is the
/// other half of that finding and is out of reach from here.
fn configured_mesh_cap(app: &App) -> i64 {
    clamp_max_players(app.max_players)
}

fn resolve_lobby_max(app: &App, requested: Option<i64>) -> i64 {
    let cfg = configured_mesh_cap(app);
    if app.override_game_max {
        if let Some(req) = requested {
            let req = clamp_max_players(req);
            if req != cfg {
                // LS-10: the shipped lan.ini silently replaces the exe's request (4 -> 8). Say so.
                rate_limited_log(
                    "lobby_max_override",
                    &format!(
                        "lobby max override requested={req} served={cfg} (lan.ini max_players={} override_game_max=true)",
                        app.max_players
                    ),
                );
            }
        }
        return cfg;
    }
    requested.map(clamp_max_players).unwrap_or(cfg)
}

fn as_obj_or_empty(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(|x| x.as_object().cloned()).unwrap_or_default()
}

/// `PFLobbyOwnerMigrationPolicy`: 0 None (the exe's create value), 1 Automatic, 2 Manual,
/// 3 Server. The shim forwards the exe's dword; string forms are accepted too so a
/// hand-written body cannot silently pick the wrong policy.
fn parse_owner_migration_policy(body: &Value) -> i64 {
    match body
        .get("OwnerMigrationPolicy")
        .or_else(|| body.get("ownerMigrationPolicy"))
    {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0).clamp(0, 3),
        Some(Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "automatic" => 1,
            "manual" => 2,
            "server" => 3,
            _ => 0,
        },
        _ => 0,
    }
}

fn create_lobby(app: &mut App, body: &Value, player: &Player) -> String {
    let lobby_id = format!("lan-{}", &sha1_hex(&format!("{}{}", now_unix(), player.entity_id))[..12]);
    let owner = body
        .get("Owner")
        .or_else(|| body.get("owner"))
        .cloned()
        .unwrap_or_else(|| entity_obj(player));
    let mut member_entity = owner.clone();
    let mut member_data = Map::new();
    if let Some(arr) = body
        .get("Members")
        .or_else(|| body.get("members"))
        .and_then(|v| v.as_array())
    {
        if let Some(first) = arr.first() {
            member_data = as_obj_or_empty(
                first
                    .get("MemberData")
                    .or_else(|| first.get("memberData")),
            );
            if let Some(ent) = first.get("MemberEntity").or_else(|| first.get("memberEntity")) {
                member_entity = ent.clone();
            }
        }
    }
    if let Some(join) = body.get("MemberData").and_then(|v| v.as_object()) {
        for (k, v) in join {
            member_data.insert(k.clone(), v.clone());
        }
    }
    let mid = json_str(&member_entity, "Id")
        .map(|s| s.to_string())
        .unwrap_or_else(|| player.entity_id.clone());
    fill_member_data(&mut member_data, &mid);
    drop_owner_lobbies(app, &player.entity_id);
    let lobby = Lobby {
        id: lobby_id.clone(),
        connection: format!("lan.{}.{}", app.title_id, lobby_id),
        owner,
        owner_migration_policy: parse_owner_migration_policy(body),
        max_players: resolve_lobby_max(app, json_i64(body, "MaxPlayers").or_else(|| json_i64(body, "maxPlayers"))),
        lobby_data: as_obj_or_empty(
            body.get("LobbyData")
                .or_else(|| body.get("lobbyData"))
                .or_else(|| body.get("lobbyProperties")),
        ),
        search_data: as_obj_or_empty(
            body.get("SearchData")
                .or_else(|| body.get("searchData"))
                .or_else(|| body.get("searchProperties")),
        ),
        access_policy: json_str(body, "AccessPolicy")
            .unwrap_or("Public")
            .to_string(),
        membership_lock: json_str(body, "MembershipLock")
            .or_else(|| json_str(body, "membershipLock"))
            .unwrap_or("Unlocked")
            .to_string(),
        members: vec![json!({
            "Id": mid,
            "MemberEntity": member_entity,
            "MemberData": member_data,
        })],
        created: now_unix(),
    };
    println!(
        "[{}] CreateLobby id={} max={}",
        ts(),
        lobby.id,
        lobby.max_players
    );
    app.lobbies.insert(lobby_id.clone(), lobby);
    lobby_id
}

fn ts() -> String {
    let s = now_secs() % 86400;
    format!(" {:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60).replacen(' ', "", 1)
}

static LOG_TRUNCATED: AtomicBool = AtomicBool::new(false);

/// Serialises the whole "append one line" operation. `writeln!` issues the message and the
/// newline as separate writes on a per-line handle, so concurrent request threads interleaved
/// them (3 merged lines in 20,045 observed) and made automated request counts from this log
/// unreliable.
/// Cached log handle, re-opened periodically: the old code opened and closed the file for
/// every line, on top of `println!`, while the request thread held a global lock.
const LOG_REOPEN_MS: u128 = 5000;
static LOG_FILE: std::sync::OnceLock<Mutex<Option<(std::fs::File, std::time::Instant)>>> =
    std::sync::OnceLock::new();

fn write_log_file(line: &str) {
    let m = LOG_FILE.get_or_init(|| Mutex::new(None));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let stale = match g.as_ref() {
        Some((_, opened)) => opened.elapsed().as_millis() >= LOG_REOPEN_MS,
        None => true,
    };
    if stale {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else {
            return;
        };
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true);
        if !LOG_TRUNCATED.swap(true, Ordering::SeqCst) {
            opts.write(true).truncate(true);
        } else {
            opts.append(true);
        }
        match opts.open(dir.join("gbfr-lan-server.log")) {
            Ok(f) => *g = Some((f, std::time::Instant::now())),
            Err(_) => return,
        }
    }
    if let Some((f, _)) = g.as_mut() {
        use std::io::Write;
        // One buffer, one write: the whole line including its terminator.
        if f.write_all(format!("{line}\n").as_bytes()).is_err() {
            *g = None;
        }
    }
}

/// Errors and warnings: always written to the log file and the console.
fn log_line(msg: &str) {
    let line = format!("[{}] {msg}", ts());
    println!("{line}");
    write_log_file(&line);
}

/// Broker activity (startup, requests, WS traffic): visible on the console while the broker
/// runs, written to the file only in `[debug]` mode. No periodic output when debug is off.
fn request_log(msg: &str) {
    let line = format!("[{}] {msg}", ts());
    println!("{line}");
    if lan_cfg::debug_enabled() {
        write_log_file(&line);
    }
}

/// Per-request logging. Non-poll requests go to the console always (file only in `[debug]`).
/// The two hot poll routes are silent unless `[debug]` is on; there is no rollup.
fn log_request(method: &str, host: &str, path: &str) {
    if path == "/party/peers" || path == "/Lobby/GetLobby" {
        if lan_cfg::debug_enabled() {
            log_line(&format!("{method} host={host} path={path}"));
        }
        return;
    }
    request_log(&format!("{method} host={host} path={path}"));
}

/// Emit `msg` at most once per `RATE_LIMIT_SECS` for a given key. The key set is capped, so a
/// long run cannot grow it without bound.
fn rate_limited_log(key: &str, msg: &str) {
    static RATE: std::sync::OnceLock<Mutex<HashMap<String, f64>>> = std::sync::OnceLock::new();
    let now = now_unix();
    let due = {
        let mut map = RATE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if map.len() > 64 {
            map.clear();
        }
        let due = map
            .get(key)
            .map(|t| now - *t >= RATE_LIMIT_SECS)
            .unwrap_or(true);
        if due {
            map.insert(key.to_string(), now);
        }
        due
    };
    if due {
        log_line(msg);
    }
}

fn strip_api_prefix(path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    for prefix in [
        "/v1/api/index.php/",
        "/v1/api/",
        "/index.php/",
        "/api/",
    ] {
        if let Some(rest) = p.strip_prefix(prefix) {
            return format!("/{}", rest.trim_start_matches('/'));
        }
    }
    p.to_string()
}

fn advert_host(headers: &HashMap<String, String>, fallback: &str) -> String {
    headers
        .get("host")
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn boot_config(app: &App, headers: &HashMap<String, String>) -> Value {
    let host = advert_host(headers, "127.0.0.1");
    let now = now_secs() as i64;
    json!({
        "PlayfabTitleId": app.title_id,
        "PlaylogUrl": format!("http://{}:{}/playlog", host, app.http_port),
        "NetworkStatsUrl": format!("http://{}:{}/playlog/network-stats", host, app.http_port),
        "GameapiUrl": format!("http://{}:{}", host, app.http_port),
        "WebsocketUrl": format!("ws://{}:{}/", host, app.ws_port),
        "CidpUrl": "x",
        "CidpOfficialUrl": "x",
        "CidpClientId": "x",
        "PrivacyPolicyUrl": "x",
        "SurveyUrl": "x",
        "ActivateDateSec": now - 86400,
        "DeactivateDateSec": now + 86400 * 3650,
        // Guest join work-item pump (FUN_1429010f0 / Ghidra FUN_1429057e0) uses
        // MatchingSearchWaitTimeMaxSec as work+0xb8. Unpatched +0xa0==3 waits that
        // many seconds then writes +0xa0=6 → 1C even if Party mesh is already up
        // (shim auto-connect never fills work+0x98). Min 0 / max 3600; SearchWaitTime*
        // is CBT-only and unused by this exe.
        "MatchingSearchWaitTimeMinSec": 0,
        "MatchingSearchWaitTimeMaxSec": 3600,
        "MatchingRemainingMaxCounter": 10,
        "MatchingDefaultSearchLimit": 20,
        "MatchingEnableFindListSort": 1,
        "SearchWaitTimeMinSec": 0,
        "SearchWaitTimeMaxSec": 3600,
        "RemainingMaxCounter": 10,
    })
}

fn handle_cygames(
    path: &str,
    app: &App,
    headers: &HashMap<String, String>,
) -> Option<(u16, &'static str, Vec<u8>)> {
    let p = path.trim_start_matches('/');
    if p == "sys/get_terms" || p == "get_terms" {
        return Some(cygames_ok(json!({"pp_version": 1, "playlog": 1, "sc": 0})));
    }
    if p == "sys/get_news" || p == "get_news" {
        return Some(cygames_ok(json!({"list_size": 0, "list": []})));
    }
    if matches!(
        p,
        "sys/user_auth" | "user_auth" | "sys/external_user_auth" | "external_user_auth"
    ) {
        let host = advert_host(headers, "127.0.0.1");
        return Some(cygames_ok(json!({
            "auth_token": format!("lan-auth.{}", &sha1_hex(&format!("{}", now_unix()))[..16]),
            "ws_url": format!("ws://{}:{}/", host, app.ws_port),
            "ws_api_key": "lan-ws-api-key",
            "cygames_id_linked": false,
        })));
    }
    if p == "activity/get_invite_list" || p == "activity/get_presence_list" {
        return Some(cygames_ok(json!({})));
    }
    if p == "cygames_id/link_status" || p == "cygames_id/link" || p == "cygames_id/unlink" || p.starts_with("cygames_id/") && p != "cygames_id/check_reward"
    {
        return Some(cygames_ok(json!({"is_pending": false, "link_status": 0})));
    }
    if p == "cygames_id/check_reward" {
        return Some(cygames_ok(
            json!({"has_link_reward": false, "has_cross_reward": false}),
        ));
    }
    if p.starts_with("playlog") {
        return Some(cygames_ok(json!({})));
    }
    None
}

fn handle_playfab(
    app: &mut App,
    path: &str,
    body: &Value,
    headers: &HashMap<String, String>,
) -> Option<(u16, &'static str, Vec<u8>)> {
    let p = path.trim_start_matches('/');
    let lower = p.to_ascii_lowercase();

    if lower.ends_with("client/loginwithsteam") || lower.ends_with("loginwithsteam") {
        let ticket = json_str(body, "SteamTicket")
            .or_else(|| json_str(body, "steamTicket"))
            .unwrap_or("");
        let player = player_from_ticket(app, ticket);
        request_log(&format!("LoginWithSteam PlayFabId={}", player.playfab_id));
        return Some(playfab_ok(login_result(&player)));
    }

    if lower.replace('_', "").contains("getentitytoken") {
        let player = request_entity_id(app, body, headers)
            .and_then(|id| {
                app.sessions
                    .values()
                    .find(|p| p.entity_id == id)
                    .cloned()
            })
            .unwrap_or_else(|| {
                let t = json_str(body, "SteamTicket").unwrap_or("anonymous");
                player_from_ticket(app, t)
            });
        return Some(playfab_ok(json!({
            "EntityToken": player.entity_token,
            "TokenExpiration": player.token_expiration,
            "Entity": entity_obj(&player),
        })));
    }

    if lower.contains("createandjoinlobby") || lower.ends_with("lobby/createlobby") {
        let player = request_entity_id(app, body, headers)
            .and_then(|id| {
                app.sessions
                    .values()
                    .find(|p| p.entity_id == id)
                    .cloned()
            })
            .unwrap_or_else(|| player_from_ticket(app, "anonymous"));
        let id = create_lobby(app, body, &player);
        let lobby = app.lobbies.get(&id).unwrap();
        request_log(&format!(
            "CreateLobby {id} search_keys=[{}] lobby_keys=[{}]",
            map_keys(&lobby.search_data),
            map_keys(&lobby.lobby_data)
        ));
        return Some(playfab_ok(json!({
            "LobbyId": lobby.id,
            "ConnectionString": lobby.connection,
            "MaxPlayers": lobby.max_players,
        })));
    }

    if lower.ends_with("lobby/joinlobby") || lower.ends_with("joinlobby") {
        let player = request_entity_id(app, body, headers)
            .and_then(|id| {
                app.sessions
                    .values()
                    .find(|p| p.entity_id == id)
                    .cloned()
            })
            .unwrap_or_else(|| player_from_ticket(app, "anonymous"));
        let conn = json_str(body, "ConnectionString")
            .or_else(|| json_str(body, "connectionString"))
            .unwrap_or("");
        let found = app
            .lobbies
            .values()
            .find(|l| l.connection == conn)
            .map(|l| l.id.clone());
        if found.is_none() && !conn.is_empty() {
            log_line(&format!("JoinLobby unknown connection={conn}"));
            return Some(playfab_err("LobbyNotFound", "Unknown connection string"));
        }
        if found.is_none() {
            log_line("JoinLobby missing ConnectionString");
            return Some(playfab_err("LobbyNotFound", "Missing connection string"));
        }
        let lid = found.unwrap();
        let lobby = app.lobbies.get_mut(&lid).unwrap();
        let ent = body
            .get("MemberEntity")
            .cloned()
            .unwrap_or_else(|| entity_obj(&player));
        let eid = json_str(&ent, "Id")
            .unwrap_or(&player.entity_id)
            .to_string();
        if let Some(existing) = lobby.members.iter_mut().find(|m| member_id(m) == eid) {
            let mut member_data = as_obj_or_empty(existing.get("MemberData"));
            if let Some(join) = body.get("MemberData").and_then(|v| v.as_object()) {
                for (k, v) in join {
                    member_data.insert(k.clone(), v.clone());
                }
            }
            fill_member_data(&mut member_data, &eid);
            if let Some(obj) = existing.as_object_mut() {
                obj.insert("MemberData".into(), Value::Object(member_data));
            }
        } else {
            if lobby.members.len() as i64 >= lobby.max_players {
                request_log(&format!(
                    "JoinLobby full id={} members={} max={}",
                    lobby.id,
                    lobby.members.len(),
                    lobby.max_players
                ));
                return Some(playfab_err("LobbyMemberLimitExceeded", "Lobby is full"));
            }
            let mut member_data = as_obj_or_empty(body.get("MemberData"));
            fill_member_data(&mut member_data, &eid);
            lobby.members.push(json!({
                "Id": eid,
                "MemberEntity": ent,
                "MemberData": member_data,
            }));
        }
        request_log(&format!(
            "JoinLobby id={} members={}",
            lobby.id,
            lobby.members.len()
        ));
        return Some(playfab_ok(json!({
            "LobbyId": lobby.id,
            "MaxPlayers": lobby.max_players,
        })));
    }

    if lower.contains("findlobbies") {
        let viewer = request_entity_id(app, body, headers);
        let filter_text = json_str(body, "Filter")
            .or_else(|| json_str(body, "filter"))
            .unwrap_or("")
            .to_string();
        let sort_text = json_str(body, "Sort")
            .or_else(|| json_str(body, "sort"))
            .or_else(|| json_str(body, "OrderBy"))
            .or_else(|| json_str(body, "orderBy"))
            .unwrap_or("")
            .to_string();
        let count = json_i64(body, "ClientSearchResultCount")
            .or_else(|| json_i64(body, "clientSearchResultCount"))
            .or_else(|| {
                body.get("Pagination")
                    .and_then(|p| json_i64(p, "PageSizeRequested"))
            });
        // The real service filters, sorts and paginates in one pass. We do the same, and
        // anything outside the implemented subset is reported (log + response `Warnings`)
        // instead of being silently dropped; an unsupported filter clause fails closed.
        let filter = lobby_filter::parse_filter(&filter_text);
        let (sort_specs, sort_bad) = lobby_filter::parse_sort(&sort_text);
        let mut warnings: Vec<String> = filter.unsupported().to_vec();
        warnings.extend(sort_bad);
        let friends_present = body
            .get("FriendsFilter")
            .or_else(|| body.get("friendsFilter"))
            .is_some();
        if friends_present {
            warnings.push(
                "friendsFilter present: broker has no friend graph (fail closed)".to_string(),
            );
        }
        let now = now_unix();
        let mut candidates: Vec<&Lobby> = app
            .lobbies
            .values()
            .filter(|lobby| {
                if friends_present {
                    return false;
                }
                let sd = search_data_for_find(lobby);
                let owner_id = lobby_owner_id(lobby);
                let view = lobby_filter::LobbyView {
                    id: &lobby.id,
                    max_players: lobby.max_players as f64,
                    current_members: lobby.members.len() as f64,
                    // The service field; create/UpdateLobby store it and lobby_public serves it.
                    membership_lock: &lobby.membership_lock,
                    search_data: &sd,
                    owner_id: &owner_id,
                    viewer_id: viewer.as_deref(),
                    viewer_is_member: viewer
                        .as_deref()
                        .map(|v| lobby_has_entity(lobby, v))
                        .unwrap_or(false),
                    has_server: false,
                };
                filter.matches(&view)
            })
            .collect();
        // LS-2: the 30 s owner check no longer removes a row from discovery. It used to strip
        // every LAN1 lobby on a >30 s host stall (loading screen / blocked tick), so a healthy
        // session vanished and nothing retried. Keep the row; JoinLobby reports the truth.
        for lobby in &candidates {
            if lobby_lan1_network_id(lobby).is_some() && !lobby_owner_in_party(app, lobby, now) {
                rate_limited_log(
                    &format!("find_owner_stale:{}", lobby.id),
                    &format!(
                        "FindLobbies keeps id={} (owner party row lapsed; JoinLobby will decide)",
                        lobby.id
                    ),
                );
            }
        }
        // OrderBy: the exe's sort string first, then the documented default tiebreak
        // (creation time descending).
        candidates.sort_by(|a, b| {
            let sa = search_data_for_find(a);
            let sb = search_data_for_find(b);
            let oa = lobby_owner_id(a);
            let ob = lobby_owner_id(b);
            let va = lobby_filter::LobbyView {
                id: &a.id,
                max_players: a.max_players as f64,
                current_members: a.members.len() as f64,
                membership_lock: a.membership_lock.as_str(),
                search_data: &sa,
                owner_id: &oa,
                viewer_id: viewer.as_deref(),
                viewer_is_member: false,
                has_server: false,
            };
            let vb = lobby_filter::LobbyView {
                id: &b.id,
                max_players: b.max_players as f64,
                current_members: b.members.len() as f64,
                membership_lock: b.membership_lock.as_str(),
                search_data: &sb,
                owner_id: &ob,
                viewer_id: viewer.as_deref(),
                viewer_is_member: false,
                has_server: false,
            };
            lobby_filter::cmp_lobbies(&va, &vb, &sort_specs).then_with(|| {
                b.created
                    .partial_cmp(&a.created)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        if let Some(c) = count {
            if c > 0 {
                candidates.truncate(c.min(1000) as usize);
            }
        }
        let ids: Vec<&str> = candidates.iter().map(|l| l.id.as_str()).collect();
        let mut sk5_notes = Vec::new();
        let mut out = Vec::new();
        for lobby in candidates {
            let sd = search_data_for_find(lobby);
            let sk5 = sd
                .get("string_key5")
                .and_then(value_as_string)
                .unwrap_or_default();
            let shown = if sk5.chars().count() > 96 {
                format!("{}…", truncate_chars(&sk5, 96))
            } else {
                sk5
            };
            sk5_notes.push(format!("{}:{}", lobby.id, shown));
            out.push(json!({
                "LobbyId": lobby.id,
                "ConnectionString": lobby.connection,
                "CurrentPlayers": lobby.members.len(),
                "MaxPlayers": lobby.max_players,
                "MembershipLock": "Unlocked",
                "SearchData": sd,
                "Owner": lobby.owner,
            }));
        }
        request_log(&format!(
            "FindLobbies n={} ids=[{}] filter=\"{}\" sort=\"{}\" count={} warnings=[{}] sk5=[{}]",
            ids.len(),
            ids.join(","),
            truncate_chars(&filter_text, 1024),
            truncate_chars(&sort_text, 128),
            count.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
            warnings.join(" | "),
            sk5_notes.join(" | ")
        ));
        let mut data = json!({"Lobbies": out, "Pagination": {}});
        if !warnings.is_empty() {
            data["Warnings"] = json!(warnings);
        }
        return Some(playfab_ok(data));
    }

    if lower.contains("leavelobby") {
        let lid = json_str(body, "LobbyId")
            .or_else(|| json_str(body, "lobbyId"))
            .unwrap_or("");
        if let Some(mut lobby) = app.lobbies.remove(lid) {
            let pid = request_entity_id(app, body, headers);
            let owner = lobby_owner_id(&lobby);
            let owner_left = pid.as_ref().map(|p| p == &owner).unwrap_or(false);
            let policy = lobby.owner_migration_policy;
            let before = lobby.members.len();
            if let Some(ref pid) = pid {
                lobby.members.retain(|m| member_id(m) != *pid);
            }
            // An unresolved caller leaving a one-member lobby is that member leaving.
            if lobby.members.len() == before && before <= 1 {
                lobby.members.clear();
            }
            let closed = lobby.members.is_empty();
            let mut owner_note = String::new();
            if !closed && owner_left {
                // PlayFab keeps a client-owned lobby alive after its owner leaves
                // (ownership-changes): Automatic hands it to another member, while
                // None/Manual clear the owner so a remaining member can claim it.
                if policy == 1 {
                    let next = lobby.members[0].clone();
                    let eid = member_id(&next);
                    owner_note = format!(" owner->{eid}(Automatic)");
                    lobby.owner = next
                        .get("MemberEntity")
                        .cloned()
                        .filter(|v| {
                            json_str(v, "Id")
                                .map(|s| !s.is_empty())
                                .unwrap_or(false)
                        })
                        .unwrap_or_else(|| {
                            json!({"Id": eid, "Type": "title_player_account"})
                        });
                } else {
                    owner_note = format!(" owner->none(policy={policy})");
                    lobby.owner = Value::Null;
                }
            }
            let remain = lobby.members.len();
            if !closed {
                app.lobbies.insert(lid.to_string(), lobby);
            }
            request_log(&format!(
                "LeaveLobby {lid} members={} closed={closed} owner_left={owner_left}{owner_note}",
                if closed { 0 } else { remain }
            ));
        }
        return Some(playfab_ok(json!({})));
    }

    if lower.contains("updatelobby") || lower.contains("postlobby") {
        let lid = json_str(body, "LobbyId")
            .or_else(|| json_str(body, "lobbyId"))
            .unwrap_or("");
        let updater = request_entity_id(app, body, headers);
        if let Some(lobby) = app.lobbies.get_mut(lid) {
            // PFLobbyDataUpdate → service mapping: null member values become the documented
            // *ToDelete arrays (Lobby REST UpdateLobby) and the scalar fields become
            // MaxPlayers/AccessPolicy/MembershipLock/Owner. Deletes run before sets; a key in
            // both is a bad request upstream, so the order only decides our local tie-break.
            let search_del = json_str_array(body, &["SearchDataToDelete", "searchDataToDelete"]);
            let lobby_del = json_str_array(body, &["LobbyDataToDelete", "lobbyDataToDelete"]);
            for k in &search_del {
                lobby.search_data.remove(k);
            }
            for k in &lobby_del {
                lobby.lobby_data.remove(k);
            }
            if let Some(lock) =
                json_str(body, "MembershipLock").or_else(|| json_str(body, "membershipLock"))
            {
                lobby.membership_lock = lock.to_string();
            }
            if let Some(mx) = json_i64(body, "MaxPlayers").or_else(|| json_i64(body, "maxPlayers")) {
                lobby.max_players = clamp_max_players(mx).max(lobby.members.len() as i64);
            }
            if let Some(ap) =
                json_str(body, "AccessPolicy").or_else(|| json_str(body, "accessPolicy"))
            {
                lobby.access_policy = ap.to_string();
            }
            if let Some(ow) = body.get("Owner").or_else(|| body.get("owner")) {
                lobby.owner = ow.clone();
            }
            let member_set_target = json_str(body, "MemberEntity")
                .map(|s| s.to_string())
                .or_else(|| updater.clone());
            let member_del = json_str_array(body, &["MemberDataToDelete", "memberDataToDelete"]);
            if !member_del.is_empty() {
                if let Some(target) = member_set_target.as_ref() {
                    if let Some(m) = lobby.members.iter_mut().find(|m| member_id(m) == *target) {
                        if let Some(obj) = m.as_object_mut() {
                            if let Some(md) = obj.get_mut("MemberData").and_then(|v| v.as_object_mut()) {
                                for k in &member_del {
                                    md.remove(k);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ld) = body
                .get("LobbyData")
                .or_else(|| body.get("lobbyData"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in ld {
                    lobby.lobby_data.insert(k.clone(), v.clone());
                }
            }
            if let Some(sd) = body
                .get("SearchData")
                .or_else(|| body.get("searchData"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in sd {
                    lobby.search_data.insert(k.clone(), v.clone());
                }
            }
            request_log(&format!(
                "UpdateLobby {lid} search_keys=[{}] lobby_keys=[{}] search_del=[{}] lobby_del=[{}] member_del=[{}] lock={}",
                map_keys(&lobby.search_data),
                map_keys(&lobby.lobby_data),
                search_del.join(","),
                lobby_del.join(","),
                member_del.join(","),
                lobby.membership_lock
            ));
            if let (Some(md), Some(eid)) = (
                body.get("MemberData").and_then(|v| v.as_object()),
                member_set_target.as_ref(),
            ) {
                if let Some(m) = lobby.members.iter_mut().find(|m| member_id(m) == *eid) {
                    let mut data = as_obj_or_empty(m.get("MemberData"));
                    for (k, v) in md {
                        data.insert(k.clone(), v.clone());
                    }
                    fill_member_data(&mut data, eid);
                    if let Some(obj) = m.as_object_mut() {
                        obj.insert("MemberData".into(), Value::Object(data));
                    }
                }
            }
            if let Some(arr) = body.get("Members").and_then(|v| v.as_array()) {
                for mem in arr {
                    let eid = member_id(mem);
                    if eid.is_empty() {
                        continue;
                    }
                    let mut data = as_obj_or_empty(mem.get("MemberData"));
                    fill_member_data(&mut data, &eid);
                    let ent = mem
                        .get("MemberEntity")
                        .cloned()
                        .unwrap_or_else(|| json!({"Id": eid, "Type": "title_player_account"}));
                    if let Some(existing) = lobby.members.iter_mut().find(|m| member_id(m) == eid) {
                        if let Some(obj) = existing.as_object_mut() {
                            obj.insert("MemberData".into(), Value::Object(data));
                            obj.insert("MemberEntity".into(), ent);
                        }
                    } else if (lobby.members.len() as i64) < lobby.max_players {
                        lobby.members.push(json!({
                            "Id": eid,
                            "MemberEntity": ent,
                            "MemberData": data,
                        }));
                    }
                }
            }
        }
        return Some(playfab_ok(json!({})));
    }

    if lower.contains("getlobby") {
        let lid = json_str(body, "LobbyId")
            .or_else(|| json_str(body, "lobbyId"))
            .unwrap_or("");
        return Some(match app.lobbies.get(lid) {
            Some(l) => playfab_ok(lobby_public(l)),
            None => {
                // A miss is the event that precedes every guest's PFLobbyDisconnected; keep it
                // even though the successful poll lines are rolled up.
                rate_limited_log(
                    &format!("getlobby_miss:{lid}"),
                    &format!("GetLobby miss id={lid}"),
                );
                playfab_err("LobbyNotFound", "Lobby not found")
            }
        });
    }

    if lower.starts_with("client/")
        || lower.starts_with("lobby/")
        || lower.starts_with("authentication/")
    {
        return Some(playfab_ok(json!({})));
    }
    None
}

fn handle_party(
    app: &mut App,
    path: &str,
    body: &Value,
    headers: &HashMap<String, String>,
) -> Option<(u16, &'static str, Vec<u8>)> {
    let (p, qs) = match path.split_once('?') {
        Some((a, b)) => (a.trim_start_matches('/'), b),
        None => (path.trim_start_matches('/'), ""),
    };
    let qs_nid = qs
        .split('&')
        .find_map(|kv| kv.strip_prefix("network_id=").map(|s| s.to_string()));
    let qs_entity = qs
        .split('&')
        .find_map(|kv| kv.strip_prefix("entity_id=").map(|s| s.to_string()));
    let now = now_unix();
    let cap = configured_mesh_cap(app);

    if p == "party/join" || p == "party/create" {
        let nid = json_str(body, "network_id")
            .map(|s| s.to_string())
            .or(qs_nid)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("net-{}", &sha1_hex(&format!("{now}"))[..8]));
        let eid = json_str(body, "entity_id").unwrap_or("anon").to_string();
        let mut ip = json_str(body, "ip")
            .map(|s| s.to_string())
            .or_else(|| headers.get("x-forwarded-for").cloned())
            .or_else(|| headers.get("x-peer-ip").cloned())
            .unwrap_or_else(|| "127.0.0.1".into());
        if is_loopback_ip(&ip) {
            if let Some(peer) = headers.get("x-peer-ip") {
                if !is_loopback_ip(peer) {
                    request_log(&format!(
                        "party join rewrote loopback {ip} -> {peer} entity={eid}"
                    ));
                    ip = peer.clone();
                }
            }
        }
        let port = json_i64(body, "udp_port").unwrap_or(0) as u16;
        let seq = member_seq_of(body, qs);
        let net = app.party.entry(nid.clone()).or_default();
        net.retain(|_, m| now - m.seen <= PARTY_SEEN_TTL_SECS);
        let new_member = !net.contains_key(&eid);
        if new_member && net.len() as i64 >= cap {
            log_line(&format!(
                "party join refused network={nid} entity={eid} members={} max={cap}",
                net.len()
            ));
            return Some(playfab_err("PartyMemberLimitExceeded", "Party mesh is full"));
        }
        if new_member {
            net.insert(
                eid.clone(),
                PartyMember {
                    entity_id: eid.clone(),
                    ip: ip.clone(),
                    udp_port: port,
                    seen: now,
                    member_seq: seq,
                },
            );
        } else if let Some(m) = net.get_mut(&eid) {
            if seq != 0 && m.member_seq > seq {
                // LS-6: a reordered older join must not downgrade a fresh registration.
                m.seen = now;
                rate_limited_log(
                    &format!("party_join_stale:{nid}:{eid}"),
                    &format!(
                        "party join stale ignored network={nid} entity={eid} seq={seq} have={}",
                        m.member_seq
                    ),
                );
            } else {
                m.ip = ip.clone();
                m.udp_port = port;
                m.seen = now;
                m.member_seq = m.member_seq.max(seq);
            }
        }
        let members: Vec<Value> = net
            .values()
            .map(|m| {
                json!({
                    "entity_id": m.entity_id,
                    "ip": m.ip,
                    "udp_port": m.udp_port,
                    "seen": m.seen,
                    "member_seq": m.member_seq,
                })
            })
            .collect();
        request_log(&format!(
            "party join network={nid} entity={eid} udp={ip}:{port} members={}",
            members.len()
        ));
        return Some(playfab_ok(json!({"network_id": nid, "members": members})));
    }

    if p == "party/leave" {
        let nid = json_str(body, "network_id").unwrap_or("");
        let eid = json_str(body, "entity_id").unwrap_or("");
        let seq = member_seq_of(body, qs);
        if let Some(net) = app.party.get_mut(nid) {
            let have = net.get(eid).map(|m| m.member_seq).unwrap_or(0);
            if seq != 0 && have > seq {
                // LS-6: a leave answering an older join must not delete the fresh registration.
                rate_limited_log(
                    &format!("party_leave_stale:{nid}:{eid}"),
                    &format!(
                        "party leave ignored (stale) network={nid} entity={eid} seq={seq} have={have}"
                    ),
                );
            } else {
                net.remove(eid);
            }
        }
        return Some(playfab_ok(json!({})));
    }

    if p == "party/peers" {
        let nid = json_str(body, "network_id")
            .map(|s| s.to_string())
            .or(qs_nid)
            .unwrap_or_default();
        let eid = json_str(body, "entity_id")
            .map(|s| s.to_string())
            .or(qs_entity)
            .unwrap_or_default();
        let members = if let Some(net) = app.party.get_mut(&nid) {
            // LS-2: a peer that is polling is alive even when its 10 s /party/join keep-alive is
            // late (loading screen / blocked tick). Refresh it before the expiry sweep and log
            // the once-per-peer case the old code answered by deleting it.
            if !eid.is_empty() {
                if let Some(m) = net.get_mut(&eid) {
                    let age = now - m.seen;
                    m.seen = now;
                    if age > PARTY_SEEN_TTL_SECS {
                        rate_limited_log(
                            &format!("party_peers_refresh:{nid}:{eid}"),
                            &format!(
                                "party peers kept expired entity={eid} network={nid} age={age:.1}s (would have been dropped)"
                            ),
                        );
                    }
                }
            }
            net.retain(|_, m| now - m.seen <= PARTY_SEEN_TTL_SECS);
            net.values()
                .map(|m| {
                    json!({
                        "entity_id": m.entity_id,
                        "ip": m.ip,
                        "udp_port": m.udp_port,
                        "seen": m.seen,
                        "member_seq": m.member_seq,
                    })
                })
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        return Some(playfab_ok(json!({"network_id": nid, "members": members})));
    }

    if p == "party/msg" {
        return Some(playfab_ok(json!({})));
    }
    None
}

fn is_boot_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".json") || p.ends_with(".dat") || p.ends_with(".blob") || p.contains("config")
}

/// Lock the shared state, recovering the guard if a previous holder panicked. Continuing with
/// possibly-inconsistent state beats refusing to answer every request.
fn lock_app(app: &Mutex<App>) -> std::sync::MutexGuard<'_, App> {
    app.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn dispatch(
    app: &Mutex<App>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &Value,
) -> (u16, &'static str, Vec<u8>) {
    let stripped = strip_api_prefix(path);
    // Normalise once, then match exactly: a prefixed call (/v1/api/index.php/party/join) is
    // routed as /party/join, so it can no longer fall through to the success-shaped catch-all
    // for a call that did nothing (LS-9).
    let party = stripped.trim_start_matches('/');
    let party_path = match path.split_once('?') {
        Some((_, q)) if !q.is_empty() => format!("/{party}?{q}"),
        _ => format!("/{party}"),
    };
    let host = headers
        .get("host")
        .cloned()
        .unwrap_or_default();
    log_request(method, &host, &stripped);

    // Poison-tolerant locking: a panic anywhere while the state lock is held would otherwise
    // make every later request panic forever (total outage that only a restart clears).
    {
        let g = lock_app(app);
        if let Some(r) = handle_cygames(&stripped, &g, headers) {
            return r;
        }
    }

    if party.starts_with("party/") {
        let mut g = lock_app(app);
        if let Some(r) = handle_party(&mut g, &party_path, body, headers) {
            return r;
        }
    }

    {
        let mut g = lock_app(app);
        if let Some(r) = handle_playfab(&mut g, &stripped, body, headers) {
            return r;
        }
    }

    if method == "GET" && (stripped == "/" || stripped == "/health") {
        let g = lock_app(app);
        let body = json!({"ok": true, "title": g.title_id});
        return (
            200,
            "application/json",
            serde_json::to_vec(&body).unwrap_or_default(),
        );
    }

    if method == "GET" && (is_boot_path(&stripped) || stripped == "/" || stripped.is_empty()) {
        let g = lock_app(app);
        let cfg = boot_config(&g, headers);
        let raw = serde_json::to_vec(&cfg).unwrap_or_default();
        if stripped.ends_with(".blob") {
            let blob = encode_boot_blob(&raw);
            request_log(&format!("boot .blob {} json -> {} b64", raw.len(), blob.len()));
            return (200, "application/octet-stream", blob);
        }
        return (200, "application/json", raw);
    }

    if method == "POST" {
        // Every catch-all hit is logged with its method and normalised path: the LS-9 failure
        // mode was a prefixed /party/join answering success here while doing nothing.
        log_line(&format!("catch-all POST {stripped} — no handler, empty PlayFab OK"));
        return playfab_ok(json!({}));
    }
    log_line(&format!("catch-all {method} {stripped} — no handler, empty cygames OK"));
    cygames_ok(json!({}))
}

fn send_http(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
        let reason = match code {
        200 => "OK",
        101 => "Switching Protocols",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let hdr = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
}

fn ws_send_binary(stream: &mut TcpStream, payload: &[u8]) -> bool {
    let n = payload.len();
    let mut hdr = Vec::with_capacity(10 + n);
    hdr.push(0x82);
    if n < 126 {
        hdr.push(n as u8);
    } else if n < 65536 {
        hdr.push(126);
        hdr.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        hdr.push(127);
        hdr.extend_from_slice(&(n as u64).to_be_bytes());
    }
    stream.write_all(&hdr).is_ok() && stream.write_all(payload).is_ok()
}

fn handle_ws_payload(stream: &mut TcpStream, data: &[u8]) {
    let preview = String::from_utf8_lossy(data);
    let shown = if preview.chars().count() > 240 {
        format!("{}...", truncate_chars(&preview, 240))
    } else {
        preview.into_owned()
    };
    request_log(&format!("ws frame len={} {shown}", data.len()));
    // Exe FUN_14290b160 requires root command (string) + params (object). Extra keys are
    // skip-safe; lobby_search_id lives under params, not at the root.
    let reply = if let Ok(v) = serde_json::from_slice::<Value>(data) {
        let cmd = v.get("command").and_then(|c| c.as_str()).unwrap_or("");
        request_log(&format!("ws command={cmd}"));
        let mut params = match v.get("params") {
            Some(Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if !params.contains_key("lobby_search_id") {
            if let Some(id) = v.get("lobby_search_id") {
                if !id.is_null() {
                    params.insert("lobby_search_id".into(), id.clone());
                }
            }
        }
        json!({
            "command": cmd,
            "params": Value::Object(params),
        })
    } else if data.windows(10).any(|w| w == b"join_lobby") {
        json!({
            "command": "join_lobby",
            "params": {}
        })
    } else {
        return;
    };
    if let Ok(body) = serde_json::to_vec(&reply) {
        if ws_send_binary(stream, &body) {
            request_log(&format!(
                "ws reply command={} len={}",
                reply.get("command").and_then(|c| c.as_str()).unwrap_or(""),
                body.len()
            ));
        }
    }
}

fn pump_websocket(stream: &mut TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(None);
    loop {
        let mut hdr = [0u8; 2];
        if stream.read_exact(&mut hdr).is_err() {
            break;
        }
        let opcode = hdr[0] & 0x0f;
        let masked = hdr[1] & 0x80 != 0;
        let mut length = (hdr[1] & 0x7f) as u64;
        if length == 126 {
            let mut ext = [0u8; 2];
            if stream.read_exact(&mut ext).is_err() {
                break;
            }
            length = u16::from_be_bytes(ext) as u64;
        } else if length == 127 {
            let mut ext = [0u8; 8];
            if stream.read_exact(&mut ext).is_err() {
                break;
            }
            length = u64::from_be_bytes(ext);
        }
        if length > 1_000_000 {
            break;
        }
        let mut mask = [0u8; 4];
        if masked && stream.read_exact(&mut mask).is_err() {
            break;
        }
        let mut data = vec![0u8; length as usize];
        if stream.read_exact(&mut data).is_err() {
            break;
        }
        if masked {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        if opcode == 0x8 {
            break;
        }
        if opcode == 0x9 {
            let mut out = vec![0x8a, data.len() as u8];
            if data.len() < 126 {
                out.extend_from_slice(&data);
                let _ = stream.write_all(&out);
            }
            continue;
        }
        if opcode == 0x1 || opcode == 0x2 {
            handle_ws_payload(stream, &data);
        } else {
            request_log(&format!(
                "ws frame opcode={opcode} len={} head={}",
                data.len(),
                data.iter().take(32).map(|b| format!("{b:02x}")).collect::<String>()
            ));
        }
    }
    request_log("websocket closed");
}

fn handle_ws_upgrade(stream: &mut TcpStream, headers: &HashMap<String, String>) -> bool {
    let Some(key) = headers.get("sec-websocket-key") else {
        send_http(stream, 400, "text/plain", b"missing Sec-WebSocket-Key");
        return true;
    };
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let accept = B64.encode(hasher.finalize());
    let proto = headers
        .get("sec-websocket-protocol")
        .map(|p| format!("Sec-WebSocket-Protocol: {p}\r\n"))
        .unwrap_or_default();
    let hdr = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{proto}\r\n"
    );
    if stream.write_all(hdr.as_bytes()).is_err() {
        return true;
    }
    request_log("websocket connected");
    pump_websocket(stream);
    true
}

fn read_request(
    stream: &mut TcpStream,
    header_timeout: Option<Duration>,
    local_port: u16,
) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(header_timeout);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() >= 3 && buf[0] == 0x16 && buf[1] == 0x03 {
                    log_line(&format!(
                        "ws TLS ClientHello local={local_port} have={} — Relink used WSS on plaintext 8081",
                        buf.len()
                    ));
                    return None;
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 1024 * 1024 {
                    log_line(&format!("http read too large local={local_port}"));
                    return None;
                }
            }
            Err(e)
                if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock =>
            {
                let head: String = buf.iter().take(32).map(|b| format!("{b:02x}")).collect();
                log_line(&format!(
                    "http read timeout local={local_port} have={} head={head}",
                    buf.len()
                ));
                return None;
            }
            Err(e) => {
                let head: String = buf.iter().take(32).map(|b| format!("{b:02x}")).collect();
                let tls = buf.len() >= 3 && buf[0] == 0x16 && buf[1] == 0x03;
                log_line(&format!(
                    "http read err local={local_port} {e} have={} head={head}{}",
                    buf.len(),
                    if tls {
                        " (TLS ClientHello — WSS on plaintext WS port)"
                    } else {
                        ""
                    }
                ));
                return None;
            }
        }
    }
    if buf.is_empty() {
        log_line(&format!("http read empty local={local_port}"));
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    let Some((head, rest)) = text.split_once("\r\n\r\n") else {
        let head: String = buf.iter().take(32).map(|b| format!("{b:02x}")).collect();
        log_line(&format!(
            "http read truncated local={local_port} have={} head={head}",
            buf.len()
        ));
        return None;
    };
    let mut lines = head.split("\r\n");
    let req = lines.next()?;
    let mut parts = req.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = rest.as_bytes().to_vec();
    // If we over-read into the header buffer split, rest is already the start of body.
    // Original buf may have extra after headers as binary-safe:
    if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        body = buf[idx + 4..].to_vec();
    }
    let want = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    while body.len() < want {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(want);
    Some((method, path, headers, body))
}

fn is_websocket_upgrade(headers: &HashMap<String, String>) -> bool {
    headers
        .get("upgrade")
        .map(|s| {
            s.to_ascii_lowercase()
                .split(',')
                .any(|p| p.trim() == "websocket")
        })
        .unwrap_or(false)
}

fn handle_client(mut stream: TcpStream, app: Arc<Mutex<App>>) {
    let local_port = stream.local_addr().map(|a| a.port()).unwrap_or(0);
    let app_ws = app.lock().ok().map(|g| g.ws_port).unwrap_or(0);
    // WinHTTP Connects to 8081 at user_auth, then SendRequest (upgrade GET) only
    // later at lobby join. A 15s header timeout closed that socket (1C / 14C).
    let header_timeout = if app_ws != 0 && local_port == app_ws {
        None
    } else {
        Some(Duration::from_secs(15))
    };
    let Some((method, path, mut headers, body_raw)) =
        read_request(&mut stream, header_timeout, local_port)
    else {
        return;
    };
    if let Ok(addr) = stream.peer_addr() {
        let ip = match addr {
            SocketAddr::V4(v) => v.ip().to_string(),
            SocketAddr::V6(v) => v.ip().to_string(),
        };
        headers.entry("x-peer-ip".into()).or_insert(ip);
    }
    if is_websocket_upgrade(&headers) {
        let _ = handle_ws_upgrade(&mut stream, &headers);
        return;
    }
    if local_port != 0 {
        let app_ws = app.lock().ok().map(|g| g.ws_port).unwrap_or(0);
        if app_ws != 0 && local_port == app_ws {
            log_line(&format!(
                "ws port {local_port} non-upgrade {method} {path} upgrade={:?}",
                headers.get("upgrade")
            ));
        }
    }
    if method == "OPTIONS" {
        let hdr = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Methods: GET,POST,PUT,DELETE,OPTIONS\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(hdr.as_bytes());
        return;
    }
    let json_body: Value = if body_raw.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body_raw).unwrap_or_else(|_| {
            json!({"_raw": String::from_utf8_lossy(&body_raw)})
        })
    };
    let (code, ctype, out) = dispatch(&app, &method, &path, &headers, &json_body);
    send_http(&mut stream, code, ctype, &out);
}

fn listen(addr: &str, app: Arc<Mutex<App>>, name: &str) {
    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        eprintln!("bind {addr} failed: {e}");
        std::process::exit(1);
    });
    request_log(&format!("{name} {addr}"));
    for s in listener.incoming() {
        match s {
            Ok(stream) => {
                let app = app.clone();
                thread::spawn(move || handle_client(stream, app));
            }
            Err(e) => log_line(&format!("accept error {e}")),
        }
    }
}

fn load_lobby_ini(path: &str) -> (i64, bool) {
    let mut max = DEFAULT_MAX_PLAYERS;
    let mut override_game = true;
    if let Ok(text) = std::fs::read_to_string(path) {
        let mut section = String::new();
        for line in text.lines() {
            let line = line.split(';').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            if section != "lobby" {
                continue;
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "max_players" => max = v.trim().parse().unwrap_or(max),
                "override_game_max" => {
                    override_game = matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
                }
                _ => {}
            }
        }
        request_log(&format!("loaded ini {path}"));
    }
    (clamp_max_players(max), override_game)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut title = TITLE_DEFAULT.to_string();
    let mut ini = {
        let mut p = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("lan.ini")));
        if p.as_ref().map(|x| !x.exists()).unwrap_or(true) {
            p = Some(std::path::PathBuf::from("lan.ini"));
        }
        p.unwrap().to_string_lossy().into_owned()
    };
    if let Ok(e) = std::env::var("GBFR_LAN_INI") {
        ini = e;
    }
    // `--ini` must be visible to the shared config loader (server/party/debug sections),
    // not only to `load_lobby_ini`, so pre-scan it before the loader is first called.
    let mut pre = 1;
    while pre < args.len() {
        if args[pre] == "--ini" {
            if let Some(p) = args.get(pre + 1) {
                ini = p.clone();
            }
        }
        pre += 1;
    }
    std::env::set_var("GBFR_LAN_INI", &ini);
    let cfg = lan_cfg::lan_cfg();
    let mut http_port = cfg.port;
    let mut ws_port = cfg.ws_port;
    let mut max_players: Option<i64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--title-id" => {
                i += 1;
                title = args.get(i).cloned().unwrap_or(title);
            }
            "--http-port" => {
                i += 1;
                http_port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(http_port);
            }
            "--ws-port" => {
                i += 1;
                ws_port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(ws_port);
            }
            "--ini" => {
                i += 1;
                ini = args.get(i).cloned().unwrap_or(ini);
            }
            "--max-players" => {
                i += 1;
                max_players = args.get(i).and_then(|s| s.parse().ok());
            }
            "--decode-blob" => {
                i += 1;
                let path = args.get(i).cloned().unwrap_or_default();
                let raw = std::fs::read(&path).expect("read blob");
                let json = decode_boot_blob(&raw).expect("decode");
                println!("{}", String::from_utf8_lossy(&json));
                return;
            }
            _ => {}
        }
        i += 1;
    }
    let (mut max, override_game_max) = load_lobby_ini(&ini);
    if let Some(n) = max_players {
        max = clamp_max_players(n);
    }
    let app = Arc::new(Mutex::new(App {
        title_id: title.clone(),
        http_port,
        ws_port,
        max_players: max,
        override_game_max,
        sessions: HashMap::new(),
        lobbies: HashMap::new(),
        party: HashMap::new(),
    }));
    request_log(&format!(
        "lobby max_players={max} override_game_max={override_game_max} title={title} (broker lobby+party cap={max})"
    ));
    request_log("HTTP only (no TLS). Clients use lan.ini [server] host/port.");
    let http_app = app.clone();
    let ws_app = app;
    let http_addr = format!("0.0.0.0:{http_port}");
    let ws_addr = format!("0.0.0.0:{ws_port}");
    thread::spawn(move || listen(&ws_addr, ws_app, "WS"));
    listen(&http_addr, http_app, "HTTP");
}
