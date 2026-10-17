//! Our Free Model (OFM) 聚合桥接上游 —— 第 7 个上游，把 OFM 的账号渠道摆渡进来。
//!
//! 背景（逆向 `Our Free Model\service` 实测确认）：OFM 自带一个 OpenAI 兼容
//! 转发口（`src/forward.js` 的 `startForwardServer`），standalone 服务默认在
//! `127.0.0.1:18900` 监听，`forwardKey`（`ofm-…` Bearer）持久化在
//! `~/.our-free-model/settings.json`。它把 12 个「账号积分池 + 每日签到」型
//! 渠道（codearts/buddy/workbuddy/lobsterai/qoder/qodercn/trae/cline/loomy/
//! raccoon/minimax/zcode）连同匿名免费车道聚合成标准 `/v1/models` +
//! `/v1/chat/completions`。这些渠道的凭据锁在 OFM 自己的加密 vault 里，
//! CreditFerry 无法直接读明文，所以走「桥接」而非「逐渠道原生实现」：
//! 保活、签到、账号轮转全部交给 OFM 本体，本模块只做透传。
//!
//! 去重原则：loomy / raccoon / codearts 三个渠道 CreditFerry 已有原生上游，
//! 匿名免费车道（无 `channel` 字段的裸 id、以及 kilo/openrouter/nvidia 等
//! 公共 key 渠道）由 Kilo/Zen 上游覆盖 —— 这些一律从 OFM 目录里剔除，避免
//! 同一模型在面板出现两次。cline 有 490+ 个订阅模型，只保留 `cline-free/`
//! 免费通道且限量，防止淹没选择器。
//!
//! 降级：OFM 服务没在跑时 `catalog()` 直接返回 Err，面板显示「OFM 服务未运行」
//! 的黄条提示，不影响其余六个上游。

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::ModelEntry;

pub const OFM_SOURCE: &str = "ofm";
const OFM_UA: &str = "credit-ferry";
const LISTING_TIMEOUT: Duration = Duration::from_secs(15);
/// OFM standalone 转发口的默认端口（`service.mjs` 的 `requestedPort`）。
const OFM_DEFAULT_PORT: u16 = 18_900;
/// cline 免费通道最多带多少个进面板（其余 490+ 订阅模型丢弃）。
const CLINE_FREE_CAP: usize = 24;

/// 只桥接这些「账号渠道」——其余（已原生覆盖的、匿名车道、公共 key 渠道）剔除。
pub const OFM_CHANNELS: &[&str] = &[
    "buddy",
    "workbuddy",
    "lobsterai",
    "qoder",
    "qodercn",
    "trae",
    "minimax",
    "zcode",
    "cline",
];

/// OFM 的凭据落点：`~/.our-free-model/settings.json` 的 `forwardKey` +
/// `standalonePort`。返回 (base_url, key)。找不到（OFM 未装/未启）即 Err。
pub fn read_ofm_credentials() -> Result<(String, String), String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "无法定位用户主目录".to_string())?;
    let path = std::path::Path::new(&home).join(".our-free-model").join("settings.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "未找到 OFM 配置（~/.our-free-model/settings.json）——OFM 可能未安装或未运行".to_string())?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("OFM 配置解析失败：{}", e))?;
    let key = value
        .get("forwardKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "OFM 配置里没有 forwardKey（转发口从未启用过）".to_string())?
        .to_string();
    let port = value
        .get("standalonePort")
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(OFM_DEFAULT_PORT);
    Ok((format!("http://127.0.0.1:{}", port), key))
}

/// id 的渠道前缀：`buddy/hy4` → `buddy`；裸 id（`beam-free`）→ 空串。
pub fn channel_of(row: &Value) -> String {
    if let Some(c) = row.get("channel").and_then(|v| v.as_str()) {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    match id.find('/') {
        Some(pos) => id[..pos].to_string(),
        None => String::new(),
    }
}

/// 这一行是否值得桥接进 CreditFerry（白名单渠道 + cline 只留免费通道）。
pub fn is_bridgeable(row: &Value) -> bool {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
    if id.is_empty() {
        return false;
    }
    let channel = channel_of(row);
    if channel.is_empty() || !OFM_CHANNELS.contains(&channel.as_str()) {
        return false;
    }
    // cline 只保留免费通道（cline-free/…），订阅模型（cline-pass/…）不接。
    if channel == "cline" {
        return id.contains("cline-free");
    }
    true
}

/// 从展示名里剥出倍率：`Hy4 preview · x0.29` → ("Hy4 preview", Some(0.29))。
/// 解析不到倍率就原样返回、倍率为 None。分时计价那种「· x0.05；高峰…」也
/// 只取第一个 xN。按 char 迭代匹配 " x<数字>"（`·` 是 U+00B7 多字节，不能用
/// 字节偏移硬算），彻底不依赖分隔符的具体码点。
pub fn split_multiplier(name: &str) -> (String, Option<f64>) {
    let mut mult = None;
    let mut bare = name.trim().to_string();
    let chars: Vec<char> = bare.chars().collect();
    let mut cut: Option<usize> = None; // char 索引，指向 " x" 的空格
    let mut i = 0;
    while i + 2 < chars.len() {
        if chars[i] == ' ' && chars[i + 1] == 'x' && chars[i + 2].is_ascii_digit() {
            cut = Some(i);
            break;
        }
        i += 1;
    }
    if let Some(c) = cut {
        let tail: String = chars[c + 2..]
            .iter()
            .take_while(|ch| ch.is_ascii_digit() || **ch == '.')
            .collect();
        if let Ok(v) = tail.parse::<f64>() {
            mult = Some(v);
            let head: String = chars[..c].iter().collect();
            bare = head.trim_end_matches(['·', ' ']).trim_end().to_string();
        }
    }
    (bare, mult)
}

/// OFM 展示名：`渠道 名称`（渠道前缀避免跨渠道重名，如 buddy 与 workbuddy
/// 都有 "Auto"）。没有 name 时用 id 去掉渠道前缀。倍率后缀 `· xN` 已剥离。
fn display_name(row: &Value, channel: &str) -> String {
    let raw = row.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
    let base = if raw.is_empty() {
        let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("model");
        id.strip_prefix(&format!("{}/", channel)).unwrap_or(id).to_string()
    } else {
        raw.to_string()
    };
    let (bare, _mult) = split_multiplier(&base);
    let bare = if bare.is_empty() { base } else { bare };
    if channel.is_empty() {
        bare
    } else {
        format!("{} {}", channel, bare)
    }
}

fn num(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64())
}

/// 一行 OFM 模型 → ModelEntry（source=ofm，upstream_id 保留完整带前缀的 id，
/// 因为回传给 OFM 时 model 字段要用它）。
pub fn ofm_entry_of(row: &Value) -> ModelEntry {
    let channel = channel_of(row);
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let raw_name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let multiplier = split_multiplier(&raw_name).1;
    let vision = row
        .get("vision")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| {
            row.get("input")
                .and_then(|v| v.as_array())
                .map(|mods| mods.iter().any(|m| m.as_str() == Some("image")))
                .unwrap_or(false)
        });
    let context = num(row.get("context_window")).unwrap_or(131_072);
    let max_output = num(row.get("max_tokens")).unwrap_or(32_768);
    ModelEntry {
        upstream_id: id,
        name: display_name(row, &channel),
        source: OFM_SOURCE.to_string(),
        vision,
        context,
        max_output,
        multiplier,
        billing_status: Some(format!("账号渠道 · {}（OFM 聚合）", channel)),
    }
}

/// 过滤 + 去重 + cline 限量，得到桥接目录。
/// 两道去重：upstream_id 完全重复；以及**展示名冲突**（如 `buddy/hy3` 与
/// `buddy/hy3-x` 的 name 都是 "Hy3"，会给客户端两个一模一样的 clientId，
/// resolve 时永远命中第一个、第二个成哑弹）——按 name 去重，首个胜出。
pub fn build_ofm_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut named = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut cline_count = 0usize;
    for row in rows {
        if !is_bridgeable(row) {
            continue;
        }
        let entry = ofm_entry_of(row);
        if entry.upstream_id.is_empty() || !seen.insert(entry.upstream_id.clone()) {
            continue;
        }
        if !named.insert(entry.name.clone()) {
            continue;
        }
        if channel_of(row) == "cline" {
            if cline_count >= CLINE_FREE_CAP {
                continue;
            }
            cline_count += 1;
        }
        out.push(entry);
    }
    out
}

pub struct OfmClient {
    http: Client,
    listing_http: Client,
}

impl OfmClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(OFM_UA)
            .no_proxy()
            .build()
            .unwrap_or_default();
        let listing_http = Client::builder()
            .timeout(LISTING_TIMEOUT)
            .connect_timeout(Duration::from_secs(8))
            .user_agent(OFM_UA)
            .no_proxy()
            .build()
            .unwrap_or_default();
        Self { http, listing_http }
    }

    /// 一轮目录：读凭据 → `GET {base}/v1/models` → 过滤桥接渠道。
    pub async fn catalog(&self) -> Result<Vec<ModelEntry>, String> {
        let (base, key) = read_ofm_credentials()?;
        let resp = self
            .listing_http
            .get(format!("{}/v1/models", base))
            .header("authorization", format!("Bearer {}", key))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("OFM 服务未响应（standalone 未运行？）：{}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("OFM 目录解析失败：{}", e))?;
        if !status.is_success() {
            let note = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(format!("OFM HTTP {}: {}", status.as_u16(), note));
        }
        let rows: Vec<Value> = value
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(build_ofm_catalog(&rows))
    }

    /// 转发一轮：`POST {base}/v1/chat/completions`，Bearer key，纯透传
    /// （OFM 对 stream true/false 都按标准 OpenAI 协议应答，无需折叠）。
    pub async fn chat(&self, body: &Value) -> Result<reqwest::Response, String> {
        let (base, key) = read_ofm_credentials()?;
        self.http
            .post(format!("{}/v1/chat/completions", base))
            .header("authorization", format!("Bearer {}", key))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| format!("OFM chat 转发失败：{}", e))
    }
}

/// 面板黄条：桥接语义 + 依赖提示。
pub fn bridge_note() -> Value {
    json!({
        "text": "本上游桥接 Our Free Model 的账号渠道（CodeBuddy/WorkBuddy/LobsterAI/Qoder/Qoder中国版/TRAE/Cline免费/MiniMax/ZCode）；保活与每日签到由 OFM 本体负责，需 OFM standalone 服务（127.0.0.1:18900）在运行。Loomy/小浣熊/码道已由各自原生上游覆盖，此处不重复。",
        "severity": "info"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, channel: &str) -> Value {
        json!({ "id": id, "name": name, "channel": channel,
                "input": ["text", "image"], "context_window": 200_000, "max_tokens": 8192 })
    }

    #[test]
    fn only_whitelisted_channels_bridge() {
        let rows = vec![
            row("buddy/hy4", "Hy4", "buddy"),
            row("loomy/spark", "Spark", "loomy"),        // 原生已覆盖，剔除
            row("raccoon/x", "Rx", "raccoon"),           // 原生已覆盖，剔除
            json!({ "id": "beam-free" }),                // 匿名车道，无 channel，剔除
            json!({ "id": "nvidia/nemotron", "owned_by": "nvidia" }), // 公共 key 渠道，剔除
        ];
        let cat = build_ofm_catalog(&rows);
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0].upstream_id, "buddy/hy4");
        assert_eq!(cat[0].source, "ofm");
        assert!(cat[0].vision);
    }

    #[test]
    fn cline_keeps_only_free_and_caps() {
        let mut rows = Vec::new();
        for i in 0..40 {
            // 真实 cline 免费模型名各不相同（name 唯一），只测「只留 free + 限量」。
            rows.push(row(&format!("cline/cline-free/m{}", i), &format!("Free M{}", i), "cline"));
        }
        for i in 0..10 {
            rows.push(row(&format!("cline/cline-pass/p{}", i), &format!("Paid P{}", i), "cline"));
        }
        let cat = build_ofm_catalog(&rows);
        assert_eq!(cat.len(), CLINE_FREE_CAP);
        assert!(cat.iter().all(|e| e.upstream_id.contains("cline-free")));
    }

    #[test]
    fn multiplier_parsed_from_name() {
        assert_eq!(split_multiplier("Hy4 preview · x0.29"), ("Hy4 preview".into(), Some(0.29)));
        assert_eq!(split_multiplier("DeepSeek-V4.1-Flash · x0.05；高峰时段"), ("DeepSeek-V4.1-Flash".into(), Some(0.05)));
        assert_eq!(split_multiplier("Auto"), ("Auto".into(), None));
        let entry = ofm_entry_of(&row("qoder/auto", "Auto · x0.5", "qoder"));
        assert_eq!(entry.multiplier, Some(0.5));
        assert_eq!(entry.name, "qoder Auto");
    }

    #[test]
    fn channel_prefers_field_then_id_prefix() {
        assert_eq!(channel_of(&row("zcode/GLM-5.3", "GLM", "zcode")), "zcode");
        assert_eq!(channel_of(&json!({ "id": "trae/DeepSeek" })), "trae");
        assert_eq!(channel_of(&json!({ "id": "beam-free" })), "");
    }

    #[test]
    fn missing_name_falls_back_to_id_tail() {
        let entry = ofm_entry_of(&json!({ "id": "minimax/MiniMax-M3.1", "channel": "minimax" }));
        assert_eq!(entry.name, "minimax MiniMax-M3.1");
        assert_eq!(entry.context, 131_072); // 无 context_window → 兜底
        assert_eq!(entry.max_output, 32_768);
    }

    #[test]
    fn dedup_by_upstream_id() {
        let rows = vec![row("buddy/hy4", "Hy4", "buddy"), row("buddy/hy4", "Hy4 dup", "buddy")];
        assert_eq!(build_ofm_catalog(&rows).len(), 1);
    }

    #[test]
    fn same_display_name_deduped_but_cross_channel_kept() {
        // buddy/hy3 与 buddy/hy3-x 展示名都是 "buddy Hy3" → 去重留一个；
        // qoder/auto 与 qodercn/auto 展示名带各自渠道前缀 → 都保留。
        let rows = vec![
            row("buddy/hy3", "Hy3", "buddy"),
            row("buddy/hy3-x", "Hy3", "buddy"),
            row("qoder/auto", "Auto", "qoder"),
            row("qodercn/auto", "Auto", "qodercn"),
        ];
        let cat = build_ofm_catalog(&rows);
        assert_eq!(cat.len(), 3);
        assert_eq!(cat.iter().filter(|e| e.name == "buddy Hy3").count(), 1);
        assert!(cat.iter().any(|e| e.name == "qoder Auto"));
        assert!(cat.iter().any(|e| e.name == "qodercn Auto"));
    }
}
