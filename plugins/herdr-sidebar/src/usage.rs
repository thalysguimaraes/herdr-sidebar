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
