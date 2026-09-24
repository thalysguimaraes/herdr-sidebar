//! Quota data for the Usage view, read from a tg-usage server
//! (`GET /v1/snapshot`). Connection details come from the owner-only file
//! tg-usage writes when a device is paired:
//! `~/.local/state/tg-usage/tui-connection.json`
//! `{ origin, deviceToken, access?: { clientId, clientSecret } }`.
//!
//! Contract used by `usage_app` (do not change signatures without updating it):
//! - [`load_connection`] -> `Option<Connection>` (None = host not paired)
//! - [`fetch`] -> `Result<Snapshot, String>` blocking, bounded by a timeout
//! - [`Snapshot`], [`Account`], [`Window`] are plain data, already filtered to
//!   accounts that have at least one measurable window.

#[derive(Clone, Debug)]
pub struct Connection {
    pub origin: String,
    pub device_token: String,
    pub access: Option<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Horizon {
    Session,
    Weekly,
    Monthly,
    Other,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub label: String,
    pub horizon: Horizon,
    /// 0.0..=1.0 share USED.
    pub used: f64,
    /// Window length; None when the server does not report one.
    pub duration_ms: Option<i64>,
    /// Epoch ms of the next reset, if known.
    pub resets_at_ms: Option<i64>,
    /// True for model-restricted windows (e.g. "Weekly Fable").
    pub model_scoped: bool,
}

#[derive(Clone, Debug)]
pub struct Account {
    /// e.g. "claude", "codex", "opencode-go", "glm"
    pub provider: String,
    /// e.g. "Claude · Kanastra"
    pub display_name: String,
    /// false when the account's data is stale/unavailable.
    pub fresh: bool,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Server clock at snapshot time (epoch ms); use for countdowns.
    pub server_now_ms: i64,
    pub accounts: Vec<Account>,
}

/// Reads the pairing file tg-usage writes. Missing or unparseable => None.
pub fn load_connection() -> Option<Connection> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home).join(".local/state/tg-usage/tui-connection.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let origin = json.get("origin")?.as_str()?.to_string();
    let device_token = json.get("deviceToken")?.as_str()?.to_string();
    let access = json.get("access").and_then(|a| {
        Some((
            a.get("clientId")?.as_str()?.to_string(),
            a.get("clientSecret")?.as_str()?.to_string(),
        ))
    });
    Some(Connection {
        origin,
        device_token,
        access,
    })
}

/// Blocking snapshot fetch. Secrets travel in curl's config on stdin, never argv.
/// Any transport/HTTP failure (or a spawn failure) is reported as `"offline"`.
pub fn fetch(conn: &Connection) -> Result<Snapshot, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let url = format!("{}/v1/snapshot", conn.origin.trim_end_matches('/'));
    let mut child = Command::new("curl")
        .args(["-sfm", "5", "-K", "-"])
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "offline".to_string())?;

    let mut config = format!(
        "header = {}\n",
        curl_quote(&format!("Authorization: Bearer {}", conn.device_token))
    );
    if let Some((client_id, client_secret)) = &conn.access {
        config.push_str(&format!(
            "header = {}\n",
            curl_quote(&format!("CF-Access-Client-Id: {client_id}"))
        ));
        config.push_str(&format!(
            "header = {}\n",
            curl_quote(&format!("CF-Access-Client-Secret: {client_secret}"))
        ));
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|_| "offline".to_string())?;
    }
    let out = child.wait_with_output().map_err(|_| "offline".to_string())?;
    if !out.status.success() {
        return Err("offline".to_string());
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// Quotes a curl config value. Control characters are dropped: they would end
/// the line and let a secret inject further config entries.
fn curl_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => {}
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Maps a `/v1/snapshot` body. Pure, so the mapping rules are unit-testable.
fn parse(json: &str) -> Result<Snapshot, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("bad response: {e}"))?;
    let server_now_ms = value
        .get("serverNowMs")
        .and_then(|n| n.as_i64())
        .ok_or("bad response: serverNowMs")?;
    let accounts_json = value
        .get("accounts")
        .and_then(|a| a.as_array())
        .ok_or("bad response: accounts")?;

    let mut accounts = Vec::new();
    for account in accounts_json {
        let provider = account
            .get("providerId")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let fresh = account.get("status").and_then(|s| s.as_str()) == Some("fresh");
        let mut windows = Vec::new();
        for window in account
            .get("windows")
            .and_then(|w| w.as_array())
            .into_iter()
            .flatten()
        {
            // Unmeasurable windows carry no bar to draw.
            let Some(used) = window.get("usedFraction").and_then(|n| n.as_f64()) else {
                continue;
            };
            let horizon = match window.get("horizon").and_then(|h| h.as_str()) {
                Some("session") => Horizon::Session,
                Some("weekly") => Horizon::Weekly,
                Some("monthly") => Horizon::Monthly,
                // codex reports its weekly window as a bare "Session".
                _ if provider == "codex" => Horizon::Weekly,
                _ => Horizon::Other,
            };
            windows.push(Window {
                label: window
                    .get("label")
                    .and_then(|l| l.as_str())
                    .unwrap_or_default()
                    .to_string(),
                horizon,
                used,
                duration_ms: window.get("durationMs").and_then(|n| n.as_i64()),
                resets_at_ms: window.get("resetsAtMs").and_then(|n| n.as_i64()),
                model_scoped: window
                    .get("modelScoped")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            });
        }
        if windows.is_empty() {
            continue;
        }
        accounts.push(Account {
            provider: provider.to_string(),
            display_name: account
                .get("displayName")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            fresh,
            windows,
        });
    }
    Ok(Snapshot {
        server_now_ms,
        accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "serverNowMs": 1790264438470,
        "accounts": [
            {
                "providerId": "claude",
                "displayName": "Claude \u00b7 Test",
                "status": "fresh",
                "windows": [
                    {"label": "Session (5h)", "usedFraction": 0.25, "durationMs": 18000000,
                     "resetsAtMs": 1790276399644, "horizon": "session"},
                    {"label": "Balance (USD)", "usedFraction": null}
                ]
            },
            {"providerId": "elevenlabs", "displayName": "ElevenLabs", "status": "unsupported",
             "windows": []},
            {
                "providerId": "codex",
                "displayName": "Codex",
                "status": "stale",
                "windows": [{"label": "Session", "usedFraction": 0.5, "modelScoped": true}]
            }
        ]
    }"#;

    #[test]
    fn parse_maps_windows_and_accounts() {
        let snap = parse(FIXTURE).expect("fixture parses");
        assert_eq!(snap.server_now_ms, 1790264438470);
        // elevenlabs left with zero windows is dropped entirely.
        assert_eq!(snap.accounts.len(), 2);

        let claude = &snap.accounts[0];
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.display_name, "Claude \u{b7} Test");
        assert!(claude.fresh);
        // the usedFraction:null window is dropped.
        assert_eq!(claude.windows.len(), 1);
        assert_eq!(claude.windows[0].horizon, Horizon::Session);
        assert_eq!(claude.windows[0].used, 0.25);
        assert_eq!(claude.windows[0].duration_ms, Some(18000000));
        assert_eq!(claude.windows[0].resets_at_ms, Some(1790276399644));
        assert!(!claude.windows[0].model_scoped);

        let codex = &snap.accounts[1];
        assert_eq!(codex.provider, "codex");
        assert!(!codex.fresh);
        assert_eq!(codex.windows.len(), 1);
        // horizon absent + provider codex => weekly, not Other.
        assert_eq!(codex.windows[0].horizon, Horizon::Weekly);
        assert!(codex.windows[0].model_scoped);
        assert_eq!(codex.windows[0].duration_ms, None);
    }

    #[test]
    fn parse_rejects_malformed_bodies() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"accounts":[]}"#).is_err());
        assert!(parse(r#"{"serverNowMs":1}"#).is_err());
    }

    #[test]
    fn curl_quote_keeps_secrets_on_one_line() {
        assert_eq!(curl_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(curl_quote("x\ny"), "\"xy\"");
    }
}
