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

/// 开发（`tauri dev` / debug 构建）用 6665，打包发布（release）用 6666——
/// 正在跑的生产实例与开发实例互不抢端口（旧进程占端口直接退出的保护也
/// 不会误杀）。settings.json 里显式写过 port 的用户设置仍然优先。
fn default_port() -> u16 {
    if cfg!(debug_assertions) {
        6665
    } else {
        6666
    }
}
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

/// CodeBuddy (腾讯, 国内) / WorkBuddy (国际) 凭据 —— 同一套 OAuth 协议，
/// 仅 endpoint / productCode / UA / domain 不同。
/// refresh_token 是**单次消费**的：每次刷新都轮换，故所有续期路径必须走
/// 同一把闸（server.rs 的 refresh_gate）。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuddyCreds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// ms since epoch（绝对时间；上游给 expiresAt/expiresIn 两种形态，见 buddy.rs）。
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub refresh_expires_at: i64,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub scope: String,
    /// 上游下发的权威域名（X-Domain），空串时回退产品 apiDomain。
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub phone: String,
}

impl BuddyCreds {
    pub fn ready(&self) -> bool {
        !self.access_token.is_empty()
    }
    pub fn refreshable(&self) -> bool {
        !self.refresh_token.is_empty()
    }
    pub fn expired(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= crate::store::now_ms()
    }
}

/// 一路通用 Bearer-token 渠道的凭据（lobsterai / trae / minimax … 共用形状：
/// access_token + 单次消费 refresh_token + 到期时间 + 账号标识）。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenCreds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 渠道特有的原始字段（device_id / machine_id / security_oauth_token …），
    /// 原样保存——TRAE 的设备指纹头要从这里派生。
    #[serde(default)]
    pub extra: serde_json::Value,
}

impl TokenCreds {
    pub fn ready(&self) -> bool {
        !self.access_token.is_empty()
    }
    pub fn refreshable(&self) -> bool {
        !self.refresh_token.is_empty()
    }
    pub fn expired(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= crate::store::now_ms()
    }
}

/// 一路 native 账号渠道（buddy / workbuddy / …）的登录态。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuddyState {
    #[serde(default)]
    pub buddy: BuddyCreds,
    #[serde(default)]
    pub workbuddy: BuddyCreds,
}

/// Bookkeeping for the background keepalive loop (P1-3, OFM-style).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveState {
    /// source ("maodao" | "raccoon" | "buddy" | "workbuddy") → terminal block.
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
    /// Native Tencent-family channels (buddy / workbuddy).
    #[serde(default)]
    pub buddy: BuddyState,
    /// Native LobsterAI (有道).
    #[serde(default)]
    pub lobsterai: TokenCreds,
    /// Native TRAE (字节).
    #[serde(default)]
    pub trae: TokenCreds,
    /// Native MiniMax Code.
    #[serde(default)]
    pub minimax: TokenCreds,
    /// Native Cline (OpenAI-compatible).
    #[serde(default)]
    pub cline: TokenCreds,
    /// Native Qoder (国际版) — login/catalog/quota only; inference needs a WASM signer.
    #[serde(default)]
    pub qoder: TokenCreds,
    /// Native Qoder CN (中国版).
    #[serde(default)]
    pub qodercn: TokenCreds,
    /// Freebuff (CodeBuff) — login import + catalog + daily freebucks only;
    /// free-tier inference is CLI-gated upstream, so chat is not relayed.
    #[serde(default)]
    pub freebuff: TokenCreds,
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
    /// 渠道侧的“通道/function”归属（TRAE batch_get_detail_param 的
    /// function_configs[].function）。chat 时必须带上匹配的通道名，
    /// 否则 TRAE 在 SSE 流内回 event:error code=4001。
    #[serde(default)]
    pub channel: Option<String>,
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
    /// CodeBuddy (腾讯 CodeBuddy 国内版) — native OAuth channel.
    #[serde(default)]
    pub buddy: Vec<ModelEntry>,
    /// WorkBuddy (国际版) — same OAuth protocol, different endpoint/UA.
    #[serde(default)]
    pub workbuddy: Vec<ModelEntry>,
    /// Native LobsterAI (有道).
    #[serde(default)]
    pub lobsterai: Vec<ModelEntry>,
    /// Native TRAE (字节).
    #[serde(default)]
    pub trae: Vec<ModelEntry>,
    /// Native MiniMax Code.
    #[serde(default)]
    pub minimax: Vec<ModelEntry>,
    /// Native Cline (OpenAI-compatible, device-code login).
    #[serde(default)]
    pub cline: Vec<ModelEntry>,
    /// Freebuff (CodeBuff) roster — 目录仅供面板展示，推理 gated 不发布。
    #[serde(default)]
    pub freebuff: Vec<ModelEntry>,
    /// Freebuff 每日 freebucks 额度快照（GET /session 提取，仅供展示）。
    #[serde(default)]
    pub freebuff_quota: Option<serde_json::Value>,
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
    /// "available" (6h) | "timeout" (10min) | "rate_limited" (2min) | "unavailable" (1h)
    pub category: String,
}

impl StoredVerdict {
    pub fn fresh(&self) -> bool {
        let ttl_ms = match self.category.as_str() {
            "available" => 6 * 3600 * 1000,
            "timeout" => 10 * 60 * 1000,
            "rate_limited" => 2 * 60 * 1000,
            _ => 3600 * 1000,
        };
        now_ms() - self.at_ms < ttl_ms
    }
}

impl ModelCache {
    pub fn verdict(&self, source: &str, upstream_id: &str) -> Option<&StoredVerdict> {
        self.probes.get(&format!("{}/{}", source, upstream_id))
    }
    /// 发布口径（2026-10-19 收紧）：只保留**验证可调用**的模型——必须有过
    /// 探针成功判定；未验证、失败（含限流/超时）都不提供给客户端，等待
    /// 「统一验证」或 keepalive 重探成功后自动重新上架。
    pub fn publishable(&self, source: &str, upstream_id: &str) -> bool {
        match self.verdict(source, upstream_id) {
            Some(v) => v.ok,
            None => false,
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
    /// 多把接入密钥（2026-10-19 新增）：`api-keys.json`。主密钥（primary）
    /// 始终等于 `api-key` 文件内容，导出配置只写主密钥；额外密钥同样能过
    /// 转发口鉴权，可随时吊销。
    pub api_keys: ApiKeyList,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyInfo {
    pub id: String,
    pub label: String,
    /// 明文密钥。本机盘上已有 credentials.json 明文凭据的同等保护口径。
    pub key: String,
    pub created_at_ms: i64,
    /// 主密钥不可吊销；首次打开时从 `api-key` 文件同步进来。
    pub primary: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyList {
    #[serde(default)]
    pub keys: Vec<ApiKeyInfo>,
}

impl ApiKeyList {
    /// 保证主密钥在列表首位且与 `api-key` 文件一致；缺额外结构时补空表。
    pub fn ensure_primary(&mut self, primary: &str) {
        if self.keys.iter().any(|k| k.primary) {
            for k in self.keys.iter_mut() {
                if k.primary {
                    k.key = primary.to_string();
                }
            }
        } else {
            self.keys.insert(0, ApiKeyInfo {
                id: "primary".into(),
                label: "主密钥".into(),
                key: primary.to_string(),
                created_at_ms: crate::store::now_ms(),
                primary: true,
            });
        }
    }

    pub fn authorize(&self, candidate: &str) -> bool {
        !candidate.is_empty() && self.keys.iter().any(|k| k.key == candidate)
    }
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
        let api_keys: ApiKeyList = read_json(&dir.join("api-keys.json"), ApiKeyList::default());
        Self {
            settings: read_json(&dir.join("settings.json"), Settings::default()),
            credentials: read_json(&dir.join("credentials.json"), Credentials::default()),
            models: read_json(&dir.join("models-cache.json"), ModelCache::default()),
            ledger: read_json(&dir.join("ledger.json"), Ledger::default()),
            dir,
            api_keys,
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

    pub fn save_api_keys(&self) {
        let _ = write_json(&self.dir.join("api-keys.json"), &self.api_keys);
    }

    /// 新增一把额外密钥，返回新条目。
    pub fn add_api_key(&mut self, label: &str) -> ApiKeyInfo {
        let info = ApiKeyInfo {
            id: uuid::Uuid::new_v4().simple().to_string(),
            label: if label.trim().is_empty() { "未命名".into() } else { label.trim().chars().take(40).collect() },
            key: new_hex(32),
            created_at_ms: now_ms(),
            primary: false,
        };
        self.api_keys.keys.push(info.clone());
        self.save_api_keys();
        info
    }

    /// 吊销一把非主密钥。主密钥返回 Err。
    pub fn revoke_api_key(&mut self, id: &str) -> Result<(), String> {
        let Some(pos) = self.api_keys.keys.iter().position(|k| k.id == id) else {
            return Err("密钥不存在".into());
        };
        if self.api_keys.keys[pos].primary {
            return Err("主密钥不可吊销（导出配置依赖它）".into());
        }
        self.api_keys.keys.remove(pos);
        self.save_api_keys();
        Ok(())
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

/// Decode one base64url segment of a JWT to raw bytes (tolerates padded input).
pub fn base64_url_decode(segment: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segment).ok()
}

/// Decode a JWT `iat` claim to milliseconds; `None` when absent.
pub fn jwt_iat_ms(token: &str) -> Option<i64> {
    if token.is_empty() {
        return None;
    }
    let payload = token.split('.').nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    Some(value.get("iat")?.as_i64()? * 1000)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_builds_default_to_the_dev_port_and_release_to_prod() {
        // Same assertion compiles differently per profile — that IS the
        // contract: `tauri dev` must never fight the installed 6666 instance.
        if cfg!(debug_assertions) {
            assert_eq!(Settings::default().port, 6665);
        } else {
            assert_eq!(Settings::default().port, 6666);
        }
    }

    #[test]
    fn api_keys_ensure_primary_and_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("cf-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut store = Store::open(tmp.clone());
        let primary = store.load_or_create_api_key();
        store.api_keys.ensure_primary(&primary);
        // 主密钥入列且唯一
        assert_eq!(store.api_keys.keys.len(), 1);
        assert!(store.api_keys.keys[0].primary);
        assert_eq!(store.api_keys.keys[0].key, primary);
        assert!(store.api_keys.authorize(&primary));
        assert!(!store.api_keys.authorize(""));
        assert!(!store.api_keys.authorize("nope"));

        // 新增额外密钥
        let extra = store.add_api_key("我的笔记本");
        assert_eq!(extra.key.len(), 64);
        assert!(!extra.primary);
        assert!(store.api_keys.authorize(&extra.key));
        // 再开一个 store 读回持久化结果
        let reloaded = Store::open(tmp.clone());
        assert_eq!(reloaded.api_keys.keys.len(), 2);
        assert!(reloaded.api_keys.authorize(&extra.key));

        // 主密钥不可吊销；额外密钥可吊销
        let pid = store.api_keys.keys[0].id.clone();
        assert!(store.revoke_api_key(&pid).is_err());
        assert!(store.revoke_api_key(&extra.id).is_ok());
        assert!(!store.api_keys.authorize(&extra.key));
        assert_eq!(Store::open(tmp.clone()).api_keys.keys.len(), 1);

        // ensure_primary 幂等：主密钥内容变化会同步而不是追加第二条
        store.api_keys.ensure_primary(&primary);
        assert_eq!(store.api_keys.keys.iter().filter(|k| k.primary).count(), 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn mask_key_keeps_tail() {
        let masked = crate::server::mask_key("0123456789abcdef");
        assert!(masked.ends_with("cdef"));
        assert!(masked.starts_with('•'));
        assert_eq!(crate::server::mask_key("short"), "••••");
    }
}
