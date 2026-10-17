//! Local persistence: settings, credentials, model cache, points ledger.
//!
//! Everything lives in the app data directory (`%APPDATA%\CreditFerry`):
//! `settings.json`, `credentials.json`, `models-cache.json`, `ledger.json`,
//! `api-key`. All writes are atomic (tmp + rename).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const OWNER: &str = "credit-ferry-v1";

/// The npm package / binary name of the isolated OpenCode runtime.
pub const OPENCODE_PKG: &str = "opencode-ai";
pub const OPENCODE_BINARY: &str = "opencode.exe";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_port")]
    pub port: u16,
    /// Explicit WorkBuddy models.json path (Windows auto-resolves otherwise).
    #[serde(default)]
    pub models_file: Option<String>,
    /// Explicit WorkBuddy International models.json path.
    #[serde(default)]
    pub models_file_intl: Option<String>,
    /// Explicit Qoder CN settings.json path.
    #[serde(default)]
    pub qoder_file: Option<String>,
    /// Explicit Qoder International settings.json path.
    #[serde(default)]
    pub qoder_file_intl: Option<String>,
    /// Explicit DeepSeek Harness desktop profile dir (~/.dsh/profiles/desktop).
    #[serde(default)]
    pub dsh_file: Option<String>,
    #[serde(default = "default_true")]
    pub auto_start_server: bool,
}

fn default_port() -> u16 { 6666 }
fn default_true() -> bool { true }

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: default_port(),
            models_file: None,
            models_file_intl: None,
            qoder_file: None,
            qoder_file_intl: None,
            dsh_file: None,
            auto_start_server: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LoomySession {
    pub name: String,
    pub session: String,
    #[serde(default)]
    pub userid: String,
    #[serde(default)]
    pub phone: String,
    /// ms since epoch; 0 = unknown.
    #[serde(default)]
    pub expires_at_ms: i64,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LoomyCreds {
    #[serde(default)]
    pub sessions: Vec<LoomySession>,
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RaccoonCreds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: i64,
    #[serde(default)]
    pub nickname: String,
    /// Personal accounts send an empty org code; the header is always present.
    #[serde(default)]
    pub org_code: String,
    #[serde(default)]
    pub device_id: String,
}

/// A Maodao session: APIG temporary credentials plus the long-lived refresh token.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MaodaoCreds {
    /// Temporary access key (AK) — goes into the `Authorization` header.
    #[serde(default)]
    pub access_key: String,
    /// Temporary secret key (SK) — signs the request.
    #[serde(default)]
    pub secret_key: String,
    /// IAM security token (STS) — `X-Security-Token` / `X-Auth-Token`.
    #[serde(default)]
    pub security_token: String,
    /// Long-lived refresh token (JWT, ~30 days).
    #[serde(default)]
    pub refresh_token: String,
    /// ms since epoch.
    #[serde(default)]
    pub expires_at: i64,
    /// DPoP key pair (JWK JSON strings) used to prove possession.
    #[serde(default)]
    pub dpop_private: String,
    #[serde(default)]
    pub dpop_public: String,
    /// Huawei account display name (from an imported client session, if any).
    #[serde(default)]
    pub user_name: String,
}

impl MaodaoCreds {
    pub fn ready(&self) -> bool {
        !self.security_token.is_empty() || !self.refresh_token.is_empty()
    }
}

/// A terminal refresh failure: the refresh token itself was rejected
/// (invalid_grant / auth 4xx), so auto-renewal stops until re-login.
/// Transient failures (network, 5xx, 429) never land here — a false mark
/// would strand a live session (OFM lesson: refreshable mirrors the
/// credential material, not the case history).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockInfo {
    pub reason: String,
    pub at_ms: i64,
}

/// Bookkeeping for the background keepalive loop (P1-3, OFM-style).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveState {
    /// source ("maodao" | "raccoon") → terminal block.
    #[serde(default)]
    pub blocks: BTreeMap<String, BlockInfo>,
    #[serde(default)]
    pub last_run_ms: i64,
    /// source → last round verdict, surfaced in the panel.
    #[serde(default)]
    pub notes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Credentials {
    #[serde(default)]
    pub loomy: LoomyCreds,
    #[serde(default)]
    pub raccoon: RaccoonCreds,
    #[serde(default)]
    pub maodao: MaodaoCreds,
    #[serde(default)]
    pub keepalive: KeepaliveState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelEntry {
    pub upstream_id: String,
    pub name: String,
    /// "loomy" | "raccoon"
    pub source: String,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub max_output: u64,
    #[serde(default)]
    pub multiplier: Option<f64>,
    #[serde(default)]
    pub billing_status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ModelCache {
    #[serde(default)]
    pub loomy: Vec<ModelEntry>,
    #[serde(default)]
    pub raccoon: Vec<ModelEntry>,
    #[serde(default)]
    pub opencode: Vec<ModelEntry>,
    #[serde(default)]
    pub maodao: Vec<ModelEntry>,
    /// Kilo free-pool roster — keyless lane, refreshed like any other source.
    #[serde(default)]
    pub kilo: Vec<ModelEntry>,
    /// Zen / OpenCode free-lane roster — anonymous `opencode.ai` gateway.
    #[serde(default)]
    pub zen: Vec<ModelEntry>,
    /// OFM 聚合桥接：把 Our Free Model 的账号渠道（buddy/workbuddy/…）透传进来。
    #[serde(default)]
    pub ofm: Vec<ModelEntry>,
    #[serde(default)]
    pub fetched_at_ms: i64,
    /// Callability verdicts, keyed by "<source>/<upstream_id>".
    #[serde(default)]
    pub probes: BTreeMap<String, StoredVerdict>,
}

/// A "can this model actually answer" verdict, with OC-Ferry-style TTLs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredVerdict {
    pub ok: bool,
    pub note: String,
    pub at_ms: i64,
    /// "available" (6h) | "timeout" (10min) | "unavailable" (1h)
    pub category: String,
}

impl StoredVerdict {
    pub fn fresh(&self) -> bool {
        let ttl_ms = match self.category.as_str() {
            "available" => 6 * 3600 * 1000,
            "timeout" => 10 * 60 * 1000,
            _ => 3600 * 1000,
        };
        now_ms() - self.at_ms < ttl_ms
    }
}

impl ModelCache {
    pub fn verdict(&self, source: &str, upstream_id: &str) -> Option<&StoredVerdict> {
        self.probes.get(&format!("{}/{}", source, upstream_id))
    }
    /// A fresh FAILING verdict hides the model; anything else publishes it.
    pub fn publishable(&self, source: &str, upstream_id: &str) -> bool {
        match self.verdict(source, upstream_id) {
            Some(v) => !(v.fresh() && !v.ok),
            None => true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    pub ts_ms: i64,
    pub source: String,
    pub model: String,
    /// Points actually reported by the upstream (Loomy does; Raccoon does not).
    #[serde(default)]
    pub points: Option<f64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// "reported" | "tokens-only" | "balance"
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Ledger {
    #[serde(default)]
    pub entries: Vec<LedgerEntry>,
    #[serde(default)]
    pub loomy_balance: Option<f64>,
    #[serde(default)]
    pub raccoon_balance: Option<f64>,
    /// Maodao reports its quota in tokens (total_balance), not points.
    #[serde(default)]
    pub maodao_balance: Option<f64>,
}

impl Ledger {
    pub fn record(&mut self, entry: LedgerEntry) {
        self.entries.push(entry);
        if self.entries.len() > 500 {
            let excess = self.entries.len() - 500;
            self.entries.drain(0..excess);
        }
    }
}

pub struct Store {
    pub dir: PathBuf,
    pub settings: Settings,
    pub credentials: Credentials,
    pub models: ModelCache,
    pub ledger: Ledger,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, default: T) -> T {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or(default),
        Err(_) => default,
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(value).unwrap_or_default())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

impl Store {
    pub fn open(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            settings: read_json(&dir.join("settings.json"), Settings::default()),
            credentials: read_json(&dir.join("credentials.json"), Credentials::default()),
            models: read_json(&dir.join("models-cache.json"), ModelCache::default()),
            ledger: read_json(&dir.join("ledger.json"), Ledger::default()),
            dir,
        }
    }

    pub fn api_key_path(&self) -> PathBuf { self.dir.join("api-key") }

    pub fn load_or_create_api_key(&self) -> String {
        if let Ok(text) = fs::read_to_string(self.api_key_path()) {
            let key = text.trim();
            if !key.is_empty() {
                return key.to_string();
            }
        }
        let key = new_hex(32);
        let _ = fs::write(self.api_key_path(), &key);
        key
    }

    pub fn save_settings(&self) { let _ = write_json(&self.dir.join("settings.json"), &self.settings); }
    pub fn save_credentials(&self) { let _ = write_json(&self.dir.join("credentials.json"), &self.credentials); }
    pub fn save_models(&self) { let _ = write_json(&self.dir.join("models-cache.json"), &self.models); }
    pub fn save_ledger(&self) { let _ = write_json(&self.dir.join("ledger.json"), &self.ledger); }
}

/// OS-CSPRNG random bytes (UUID v4 backed, same source as `new_hex`).
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    fill_random(&mut buf);
    buf
}

/// Random lowercase hex string of `bytes` random bytes.
pub fn new_hex(bytes: usize) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes * 2);
    let mut buf = vec![0u8; bytes];
    fill_random(&mut buf);
    for b in buf {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

fn fill_random(buf: &mut [u8]) {
    // UUID v4 is backed by the OS CSPRNG on every supported platform.
    let mut filled = 0;
    while filled < buf.len() {
        let u = uuid::Uuid::new_v4();
        let bytes = u.as_bytes();
        let take = (buf.len() - filled).min(16);
        buf[filled..filled + take].copy_from_slice(&bytes[..take]);
        filled += take;
    }
}

/// Decode a JWT `exp` claim to milliseconds; `None` on any failure.
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp = value.get("exp")?.as_i64()?;
    Some(exp * 1000)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
