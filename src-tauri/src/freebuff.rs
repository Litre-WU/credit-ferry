//! Freebuff (CodeBuff) —— 每日免费额度上游。
//!
//! 协议来自对本机桌面客户端 `orchestrator.js`（23 万行 bundle）的逆向，
//! 实测校准（2026-10-19，探针 `freebuff-api-probe.py`）：
//! - API host：`https://www.codebuff.com`（打包常量 `NEXT_PUBLIC_CODEBUFF_APP_URL`）。
//! - 鉴权：`Authorization: Bearer <token>`；token 在桌面客户端的
//!   `~/.config/freebuff-desktop/state.json` 的 `authSessions[host].token`。
//!   基础头 `x-freebuff-client: desktop`、`x-freebuff-install-id`、
//!   `x-freebuff-catalog-protocol: 1`、`x-freebuff-acting-user-id`。
//! - **三个只读端点无需 Ed25519 设备签名即可 200**（实测）：
//!   GET /api/v1/freebuff/models（目录 rows[]）、
//!   GET /api/v1/freebuff/session（freebucks 每日额度 + prices 表）、
//!   GET /api/v1/freebuff/usage-summary。
//! - freebucks 机制：`limited` 档每日 25 分（`daily.limit/spent/remaining`，
//!   0 点 Asia/Shanghai 刷新）；`freebucks.prices{modelKey: 0~250}` 为单次
//!   会话扣分，price=0 即纯免费（Solar Pro 4 / Solar Mini 4 / Glyph Cluster /
//!   Nemotron 3.5 Lightning）。
//! - ⚠️ **chat 不可转发**：托管推理端点 `POST /api/v1/chat/completions` 的
//!   `cost_mode:"free"` 被服务端强制只允许官方 CLI（`403 free_mode_cli_required`，
//!   响应并附「Calling the API directly may get your account banned」封号警告）；
//!   去掉 cost_mode 走付费档则 402。桌面 bundle 无 CLI 独有的 `x-freebuff-env`
//!   指纹头，CLI 本体是交互式 TUI、无 -p 非交互模式，无法作无头子进程。
//!   因此本模块的 `chat` 诚实返回 gated——只接入**登录态导入 + 模型目录 +
//!   每日免费额度展示**，模型不发布到 /v1/models，不转发推理（与 qoder.rs 同法）。

use std::path::PathBuf;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::{ModelEntry, TokenCreds};

pub const FREEBUFF_SOURCE: &str = "freebuff";
pub const API_HOST: &str = "https://www.codebuff.com";

const MODELS_PATH: &str = "/api/v1/freebuff/models";
const SESSION_PATH: &str = "/api/v1/freebuff/session";
const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 30_000;

/// 从 state.json 解析出的本机登录态。
#[derive(Clone, Debug)]
pub struct Imported {
    pub token: String,
    pub install_id: String,
    pub user_id: String,
    pub nickname: String,
}

impl Imported {
    /// 落成通用 TokenCreds；渠道特有字段进 extra（installId / userId）。
    pub fn to_creds(&self) -> TokenCreds {
        TokenCreds {
            access_token: self.token.clone(),
            refresh_token: String::new(),
            expires_at: 0,
            uid: self.user_id.clone(),
            nickname: self.nickname.clone(),
            extra: json!({ "installId": self.install_id, "userId": self.user_id }),
        }
    }
}

/// 桌面客户端 state.json 的路径（Windows: %USERPROFILE%\.config\freebuff-desktop）。
pub fn state_file() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let p = PathBuf::from(home).join(".config").join("freebuff-desktop").join("state.json");
    p.exists().then_some(p)
}

/// 纯解析：state.json → Imported（独立出来便于单测）。
pub fn parse_state(value: &Value) -> Result<Imported, String> {
    let sessions = value
        .get("authSessions")
        .and_then(|v| v.as_object())
        .ok_or("state.json 无 authSessions（桌面客户端未登录？）")?;
    // 优先 codebuff.com 主会话，其次任意一个带 token 的会话。
    let preferred = sessions.get(API_HOST).or_else(|| {
        sessions.values().find(|s| s.get("token").and_then(|t| t.as_str()).map(|t| !t.is_empty()).unwrap_or(false))
    });
    let session = preferred.ok_or("authSessions 为空")?;
    let token = session.get("token").and_then(|t| t.as_str()).unwrap_or("");
    if token.is_empty() {
        return Err("会话里没有 token".into());
    }
    let user = session.get("user").cloned().unwrap_or(json!({}));
    Ok(Imported {
        token: token.to_string(),
        install_id: value.get("installId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        user_id: user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        nickname: user.get("name").and_then(|v| v.as_str())
            .or_else(|| user.get("email").and_then(|v| v.as_str()))
            .unwrap_or("Freebuff").to_string(),
    })
}

/// 本机客户端导入：读 state.json 并解析。
pub fn import_local_state() -> Result<Imported, String> {
    let path = state_file().ok_or("未找到 ~/.config/freebuff-desktop/state.json（Freebuff 桌面客户端未安装或未登录）")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败：{}", path.display(), e))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("解析 state.json 失败：{}", e))?;
    parse_state(&value)
}

pub struct FreebuffClient {
    http: Client,
}

impl FreebuffClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_millis(TIMEOUT_MS))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(UA)
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// 只读 GET 的基础头集（实测无签名即可 200）。
    fn decorate(&self, req: reqwest::RequestBuilder, creds: &TokenCreds) -> reqwest::RequestBuilder {
        let install_id = creds.extra.get("installId").and_then(|v| v.as_str()).unwrap_or("");
        let user_id = creds.extra.get("userId").and_then(|v| v.as_str()).unwrap_or("");
        let mut req = req
            .header("x-freebuff-client", "desktop")
            .header("x-freebuff-catalog-protocol", "1")
            .header("Authorization", format!("Bearer {}", creds.access_token));
        if !install_id.is_empty() {
            req = req.header("x-freebuff-install-id", install_id);
        }
        if !user_id.is_empty() {
            req = req.header("x-freebuff-acting-user-id", user_id);
        }
        req
    }

    async fn get_json(&self, path: &str, creds: &TokenCreds) -> Result<Value, String> {
        let req = self.decorate(self.http.get(format!("{}{}", API_HOST, path)), creds);
        let resp = req.send().await.map_err(|e| format!("freebuff {}: {}", path, e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("freebuff {} body: {}", path, e))?;
        if !status.is_success() {
            let msg = value.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str())
                .or_else(|| value.as_str())
                .unwrap_or("");
            return Err(format!("freebuff {} HTTP {} {}", path, status.as_u16(), msg));
        }
        Ok(value)
    }

    /// 目录：GET /models（rows[]，key/handle/displayName/multimodal/access）。
    pub async fn fetch_catalog(&self, creds: &TokenCreds) -> Result<Value, String> {
        self.get_json(MODELS_PATH, creds).await
    }

    /// 额度：GET /session（freebucks.daily + prices 表）。
    pub async fn fetch_session(&self, creds: &TokenCreds) -> Result<Value, String> {
        self.get_json(SESSION_PATH, creds).await
    }

    /// 目录 × prices 合并成面板行。price 来自 session.freebucks.prices[key]：
    /// - access=="locked" → 需付费计划（付费档专属，免费 tier 不可用）
    /// - price==0        → 免费（不扣每日 freebucks）
    /// - 其余            → 每日免费额度（每次会话扣 price 分，25 分/日）
    pub fn build_models(catalog: &Value, session: &Value) -> Vec<ModelEntry> {
        let prices = session.pointer("/freebucks/prices").cloned().unwrap_or(json!({}));
        let plan_required = session.pointer("/freebucks/planRequiredModelIds").cloned().unwrap_or(json!([]));
        let rows = catalog.get("rows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        rows.iter().filter_map(|row| {
            let key = row.get("key").and_then(|v| v.as_str())?;
            let name = row.get("displayName").and_then(|v| v.as_str()).unwrap_or(key);
            let access = row.get("access").and_then(|v| v.as_str()).unwrap_or("open");
            let in_plan_list = plan_required.as_array().map(|a| a.iter().any(|x| x.as_str() == Some(key))).unwrap_or(false);
            let locked = access == "locked" || in_plan_list;
            let price = prices.get(key).and_then(|v| v.as_f64());
            let context = row.get("contextWindow").and_then(|v| v.as_u64())
                .or_else(|| row.pointer("/compaction/maxContextLength").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let billing = if locked {
                "需付费计划"
            } else if price == Some(0.0) {
                "免费"
            } else {
                "每日免费额度"
            }.to_string();
            Some(ModelEntry {
                upstream_id: key.to_string(),
                name: name.to_string(),
                source: FREEBUFF_SOURCE.to_string(),
                vision: row.get("multimodal").and_then(|v| v.as_bool()).unwrap_or(false),
                context,
                max_output: 0, // 目录未公布 maxOutput，诚实置 0
                multiplier: price,
                billing_status: Some(billing),
        channel: None,
            })
        }).collect()
    }

    /// session → 面板用的每日额度摘要。
    pub fn extract_quota(session: &Value) -> Value {
        json!({
            "accessTier": session.get("accessTier").cloned().unwrap_or(Value::Null),
            "balance": session.pointer("/freebucks/balance").cloned().unwrap_or(Value::Null),
            "limit": session.pointer("/freebucks/daily/limit").cloned().unwrap_or(Value::Null),
            "spent": session.pointer("/freebucks/daily/spent").cloned().unwrap_or(Value::Null),
            "remaining": session.pointer("/freebucks/daily/remaining").cloned().unwrap_or(Value::Null),
            "resetAt": session.pointer("/freebucks/daily/resetAt").cloned().unwrap_or(Value::Null),
            "resetTimeZone": session.pointer("/freebucks/daily/resetTimeZone").cloned().unwrap_or(Value::Null),
        })
    }

    /// chat：**不可转发**——免费档推理被服务端强门控到官方 CLI
    /// （`403 free_mode_cli_required` + 封号警告），伪造 CLI 指纹有账号风险。
    pub fn chat(&self, _creds: &TokenCreds, _body: &Value) -> Result<reqwest::Response, String> {
        Err("Freebuff 免费档推理被服务端强制要求官方 CLI（cost_mode:\"free\" 直连 API 返回 403 free_mode_cli_required 并附封号警告），本工具不伪装 CLI 身份转发；已原生支持登录态导入/模型目录/每日免费额度展示（目录仅供查看，不发布到 /v1/models）".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_session() -> Value {
        json!({
            "accessTier": "limited",
            "freebucks": {
                "balance": 25,
                "daily": { "limit": 25, "spent": 3, "remaining": 22,
                           "resetAt": "2026-10-19T16:00:00.000Z", "resetTimeZone": "Asia/Shanghai" },
                "prices": { "m-a": 0, "m-b": 15, "m-c": 250 },
                "planRequiredModelIds": ["m-c"],
                "priceNotices": {}
            }
        })
    }

    fn fixture_catalog() -> Value {
        json!({
            "rows": [
                { "key": "m-a", "displayName": "Solar Pro 4", "multimodal": false, "access": "open", "contextWindow": 500000 },
                { "key": "m-b", "displayName": "DeepSeek V4.1 Flash", "multimodal": true, "access": "open", "compaction": { "maxContextLength": 400000 } },
                { "key": "m-c", "displayName": "GPT-6.1 Sol", "multimodal": true, "access": "locked" }
            ]
        })
    }

    #[test]
    fn build_models_marks_free_locked_and_daily() {
        let models = FreebuffClient::build_models(&fixture_catalog(), &fixture_session());
        assert_eq!(models.len(), 3);
        let a = models.iter().find(|m| m.upstream_id == "m-a").unwrap();
        assert_eq!(a.billing_status.as_deref(), Some("免费"));
        assert_eq!(a.multiplier, Some(0.0));
        let b = models.iter().find(|m| m.upstream_id == "m-b").unwrap();
        assert_eq!(b.billing_status.as_deref(), Some("每日免费额度"));
        assert_eq!(b.context, 400000); // compaction.maxContextLength 兜底
        assert!(b.vision);
        let c = models.iter().find(|m| m.upstream_id == "m-c").unwrap();
        assert_eq!(c.billing_status.as_deref(), Some("需付费计划"));
        assert!(models.iter().all(|m| m.source == FREEBUFF_SOURCE));
    }

    #[test]
    fn extract_quota_maps_daily_fields() {
        let q = FreebuffClient::extract_quota(&fixture_session());
        assert_eq!(q["limit"], 25);
        assert_eq!(q["spent"], 3);
        assert_eq!(q["remaining"], 22);
        assert_eq!(q["resetTimeZone"], "Asia/Shanghai");
    }

    #[test]
    fn parse_state_fills_preferred_host_session() {
        let state = json!({
            "installId": "inst-1",
            "authSessions": {
                API_HOST: {
                    "token": "tok-1",
                    "user": { "id": "u-1", "name": "Litre", "email": "a@b.c" }
                }
            }
        });
        let got = parse_state(&state).unwrap();
        assert_eq!(got.token, "tok-1");
        assert_eq!(got.install_id, "inst-1");
        assert_eq!(got.user_id, "u-1");
        assert_eq!(got.nickname, "Litre");
        let creds = got.to_creds();
        assert_eq!(creds.extra["installId"], "inst-1");
    }

    #[test]
    fn parse_state_rejects_empty_sessions() {
        assert!(parse_state(&json!({ "authSessions": {} })).is_err());
        assert!(parse_state(&json!({})).is_err());
    }
}
