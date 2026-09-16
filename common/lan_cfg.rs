// Shared by rustc shims (include!) and the lan-server crate.
// Clients: [server] host/port in lan.ini next to the game exe, or GBFR_LAN_STUB=host:port.

#[derive(Clone, Debug)]
pub struct LanCfg {
    pub host: String,
    pub port: u16,
    pub ws_port: u16,
    /// Optional IPv4 the Party mesh should advertise. Empty = auto-detect.
    #[allow(dead_code)]
    pub advertise_ip: Option<String>,
    /// Party UDP bind. 0 = ephemeral.
    #[allow(dead_code)]
    pub udp_port: u16,
    /// Verbose diagnostics: per-message Party traffic, payload hex samples, quest/queue probes,
    /// per-request broker logging. Off by default (`[debug] enabled = true` in lan.ini or
    /// `GBFR_LAN_DEBUG=1`); lifecycle, error and rollup lines are always on.
    #[allow(dead_code)]
    pub debug: bool,
}

impl LanCfg {
    #[allow(dead_code)]
    pub fn origin(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    #[allow(dead_code)]
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    #[allow(dead_code)]
    pub fn ws_url(&self) -> String {
        format!("ws://{}:{}/", self.host, self.ws_port)
    }
}

pub fn lan_cfg() -> LanCfg {
    static C: std::sync::OnceLock<LanCfg> = std::sync::OnceLock::new();
    C.get_or_init(load_lan_cfg).clone()
}

fn load_lan_cfg() -> LanCfg {
    let mut host = "127.0.0.1".to_string();
    let mut port = 8080u16;
    let mut ws_port = 8081u16;
    let mut advertise_ip = None;
    let mut udp_port = 0u16;
    let mut debug = false;
    if let Some(text) = read_lan_ini() {
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
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match (section.as_str(), k.as_str()) {
                ("server", "host") => host = v.to_string(),
                ("server", "port") => port = v.parse().unwrap_or(port),
                ("server", "ws_port") => ws_port = v.parse().unwrap_or(ws_port),
                ("party", "advertise_ip") => {
                    if !v.is_empty() {
                        advertise_ip = Some(v.to_string());
                    }
                }
                ("party", "udp_port") => udp_port = v.parse().unwrap_or(udp_port),
                // `[debug] enabled = true` or a bare `debug = true` in any section.
                ("debug", "enabled") | (_, "debug") => {
                    debug = parse_bool(v).unwrap_or(debug)
                }
                _ => {}
            }
        }
    }
    if let Ok(raw) = std::env::var("GBFR_LAN_DEBUG") {
        if let Some(b) = parse_bool(&raw) {
            debug = b;
        }
    }
    if let Ok(raw) = std::env::var("GBFR_LAN_STUB") {
        let raw = raw.trim();
        if let Some((h, p)) = raw.rsplit_once(':') {
            if !h.is_empty() {
                host = h.to_string();
            }
            if let Ok(n) = p.parse() {
                port = n;
            }
        } else if !raw.is_empty() {
            host = raw.to_string();
        }
    }
    LanCfg {
        host,
        port,
        ws_port,
        advertise_ip,
        udp_port,
        debug,
    }
}

/// Ini/env boolean: 1/true/yes/on or 0/false/no/off.
pub fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Cached verbose-logging flag (the config itself is loaded once at first use).
#[allow(dead_code)]
pub fn debug_enabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| lan_cfg().debug)
}

fn read_lan_ini() -> Option<String> {
    if let Ok(p) = std::env::var("GBFR_LAN_INI") {
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s);
        }
    }
    for dir in lan_ini_dirs() {
        let p = dir.join("lan.ini");
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s);
        }
    }
    None
}

fn lan_ini_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            out.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd);
    }
    out
}

#[allow(dead_code)]
pub fn rewrite_http_url_to_stub(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return url.to_string();
    }
    let rest = url.splitn(2, "://").nth(1).unwrap_or("");
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
    format!("{}{path}", lan_cfg().origin())
}
