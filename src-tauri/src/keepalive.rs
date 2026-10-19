//! P1-3 账号保活 —— 把 OFM（Our Free Model）的凭据续期纪律移植过来。
//!
//! OFM 保活六件套，逐条对应：
//! 1. 批量调度：每 30 分钟一轮巡检（启动后首轮延迟 15s，等 refresh_all 落定）。
//! 2. lead 窗：`REFRESH_LEAD_MS = 1h`，只有会话进入过期前 1 小时才真正发
//!    续期请求；窗外什么都不做（只对账写 note），避免无谓地烧单次消费型 RT。
//! 3. 串行闸：`refresh_gate` 全局互斥。码道 STS 与小浣熊的 refresh_token
//!    都是**一次性**的——两条路径并发轮换，后消费的那条必然 invalid_grant
//!    （OFM CodeArts 401 风暴的根因教训）。转发口与探针的 ensure_fresh
//!    也走同一把闸（见 server.rs / probe.rs / commands.rs 的接线）。
//! 4. 闸内重读幂等：拿到闸之后才从 store 重读凭据。若另一路径刚续成功，
//!    这里读到的是新凭据 → 窗外 → 直接跳过，绝不重复发请求。
//! 5. 错误分类：终态（invalid_grant / 401 / 403 / 缺凭据材料）才落
//!    `blocks`（停止自动续期直到重新登录），且告警去重；瞬态（5xx / 429 /
//!    上游拒绝）10 分钟重试，网络异常 60 秒重试——瞬态永不落 block，
//!    一次抖动不能误杀在线会话（refreshable 镜像的是凭据材料，非案底）。
//! 6. 终态自愈：block 24 小时后自动过期重查；任何写入新凭据的路径
//!    （扫码成功 / 登录成功 / 导入客户端 / 注销）立即清除对应 block。
//!
//! Loomy 没有可续期凭据（桌面客户端 JWT），但每轮重扫客户端
//! `auth-session.json` 把新会话补进池子，等价于 OFM 的客户端登录态同步。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::AppState;
use crate::store::{now_ms, BlockInfo, Store};

/// 批量巡检周期（OFM: 30min）。
pub const REFRESH_INTERVAL_MS: i64 = 30 * 60 * 1000;
/// lead 窗：进窗才续期（OFM: REFRESH_LEAD_MS = 3600000）。
pub const REFRESH_LEAD_MS: i64 = 60 * 60 * 1000;
/// 瞬态失败的重试间隔（OFM: 600000ms）。
pub const TRANSIENT_RETRY_MS: i64 = 10 * 60 * 1000;
/// 网络异常的重试间隔（OFM: 60000ms）。
pub const NETWORK_RETRY_MS: i64 = 60 * 1000;
/// 启动首轮延迟：等 refresh_all 与 OpenCode 运行时先落定。
pub const FIRST_ROUND_DELAY_MS: u64 = 15_000;
/// 终态标记的自愈期限：误判最迟一天后重查。
pub const BLOCK_AUTO_EXPIRE_MS: i64 = 24 * 60 * 60 * 1000;

/// 续期失败的分类（决定是落 block 还是退避重试）。
#[derive(Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// 上游拒绝凭据本身：停止自动续期，等重新登录。
    Terminal,
    /// 上游/网关瞬态故障：10 分钟后重试。
    Transient,
    /// 本机网络异常：60 秒后重试。
    Network,
}

/// 从错误文案里抠出 `HTTP <code>`（上游两处格式：
/// `upstream HTTP 401: …` / `refused (HTTP 403 code …)`）。
pub fn http_code(msg: &str) -> Option<u16> {
    let idx = msg.find("HTTP ")?;
    let rest = &msg[idx + 5..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

pub fn classify(msg: &str) -> ErrorClass {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("invalid_grant")
        || lower.contains("invalid refresh")
        || lower.contains("invalid token")
        || msg.contains("请重新登录")
    {
        return ErrorClass::Terminal;
    }
    if let Some(code) = http_code(msg) {
        // 401/403 发生在 token 端点 = RT 本身被判死；其余按瞬态处理，
        // 429/5xx/未知业务码都不该误杀在线会话。
        return if matches!(code, 401 | 403) {
            ErrorClass::Terminal
        } else {
            ErrorClass::Transient
        };
    }
    // reqwest 发送失败的错误串只出现在 send 分支的前缀里。
    if lower.contains("token refresh:") || lower.contains("raccoon refresh:") {
        return ErrorClass::Network;
    }
    ErrorClass::Transient
}

/// lead 窗判据：`expires_at - now <= lead` 才续期。
/// `expires_at == 0`（未知）视为已过期 → 续。
pub fn should_renew(expires_at: i64, now: i64, lead_ms: i64) -> bool {
    expires_at - now <= lead_ms
}

/// block 是否仍然生效（未满自愈期限）。
pub fn block_active(block: Option<&BlockInfo>, now: i64) -> bool {
    match block {
        Some(b) => now - b.at_ms < BLOCK_AUTO_EXPIRE_MS,
        None => false,
    }
}

/// 面板/命令共用的保活快照。
pub fn keepalive_json(store: &Store) -> Value {
    let ka = &store.credentials.keepalive;
    json!({
        "lastRunMs": ka.last_run_ms,
        "intervalMinutes": REFRESH_INTERVAL_MS / 60000,
        "leadMinutes": REFRESH_LEAD_MS / 60000,
        "notes": ka
            .notes
            .iter()
            .map(|(k, v)| json!({ "source": k, "note": v }))
            .collect::<Vec<_>>(),
        "blocks": ka
            .blocks
            .iter()
            .map(|(k, b)| json!({ "source": k, "reason": b.reason, "atMs": b.at_ms }))
            .collect::<Vec<_>>(),
    })
}

/// 拉起保活循环。与转发口启停无关——登录态续命在服务器停止时也要继续。
pub fn spawn(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(FIRST_ROUND_DELAY_MS)).await;
        loop {
            let (delay, _) = run_round(&state).await;
            let sleep_ms = delay.clamp(60_000, REFRESH_INTERVAL_MS) as u64;
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        }
    });
}

/// 一轮巡检。返回（下一轮建议延迟, 保活快照）。
pub async fn run_round(state: &Arc<AppState>) -> (i64, Value) {
    let mut next = REFRESH_INTERVAL_MS;
    tick_loomy(state).await;
    next = next.min(tick_raccoon(state).await);
    next = next.min(tick_maodao(state).await);
    // 模型判定自愈：过期的失败判定重探一次。上游恢复的模型会自动重新
    // 上架，不依赖用户手动刷新目录；只重探失败项，成功项不烧积分。
    for src in [
        "loomy", "raccoon", "opencode", "maodao", "kilo", "zen",
        "buddy", "workbuddy", "lobsterai", "trae", "minimax", "cline",
    ] {
        crate::probe::reprobe_stale_failures(state, src).await;
    }
    let snapshot = {
        let mut store = state.store.lock().await;
        store.credentials.keepalive.last_run_ms = now_ms();
        store.save_credentials();
        keepalive_json(&store)
    };
    (next, snapshot)
}

async fn set_note(state: &Arc<AppState>, source: &str, note: &str) {
    let mut store = state.store.lock().await;
    let changed = store
        .credentials
        .keepalive
        .notes
        .get(source)
        .map(|v| v.as_str())
        != Some(note);
    if changed {
        store
            .credentials
            .keepalive
            .notes
            .insert(source.to_string(), note.to_string());
        store.save_credentials();
    }
}

async fn handle_refresh_error(state: &Arc<AppState>, source: &str, err: &str) -> i64 {
    let short: String = err.chars().take(160).collect();
    match classify(err) {
        ErrorClass::Terminal => {
            let mut store = state.store.lock().await;
            let already = block_active(store.credentials.keepalive.blocks.get(source), now_ms());
            if !already {
                // 告警去重：只在第一次落 block 时打日志。
                eprintln!("[keepalive] {} 续期被上游判死（终态），暂停自动续期：{}", source, short);
                store.credentials.keepalive.blocks.insert(
                    source.to_string(),
                    BlockInfo { reason: short.clone(), at_ms: now_ms() },
                );
            }
            store
                .credentials
                .keepalive
                .notes
                .insert(source.to_string(), format!("终态，需重新登录：{}", short));
            store.save_credentials();
            REFRESH_INTERVAL_MS
        }
        ErrorClass::Transient => {
            set_note(state, source, &format!("瞬态失败，稍后重试：{}", short)).await;
            TRANSIENT_RETRY_MS
        }
        ErrorClass::Network => {
            set_note(state, source, &format!("网络异常，稍后重试：{}", short)).await;
            NETWORK_RETRY_MS
        }
    }
}

/// 码道：STS 临时凭据 ~1h 过期、RT ~30d 单次消费轮换。
async fn tick_maodao(state: &Arc<AppState>) -> i64 {
    // 单消费 RT：任何续期路径都在闸后（含转发口/探针的 ensure_fresh）。
    let _gate = state.refresh_gate.lock().await;
    let (mut creds, block) = {
        let store = state.store.lock().await;
        (
            store.credentials.maodao.clone(),
            store.credentials.keepalive.blocks.get("maodao").cloned(),
        )
    };
    if block_active(block.as_ref(), now_ms()) {
        return REFRESH_INTERVAL_MS;
    }
    if block.is_some() {
        // 自愈期限已过：清掉旧标记重新判定（误标自动改回）。
        let mut store = state.store.lock().await;
        store.credentials.keepalive.blocks.remove("maodao");
        store.save_credentials();
    }
    if !creds.ready() || creds.refresh_token.is_empty() || creds.dpop_private.is_empty() {
        set_note(state, "maodao", "未登录或缺可续期材料（RT/DPoP），跳过").await;
        return REFRESH_INTERVAL_MS;
    }
    if !should_renew(creds.expires_at, now_ms(), REFRESH_LEAD_MS) {
        set_note(state, "maodao", "窗外等待（过期前 1 小时内才续期）").await;
        return REFRESH_INTERVAL_MS;
    }
    match crate::maodao::refresh_session(&mut creds).await {
        Ok(()) => {
            {
                let mut store = state.store.lock().await;
                store.credentials.maodao = creds.clone();
                store.save_credentials();
            }
            eprintln!("[keepalive] maodao 已自动续期，新到期 {}", creds.expires_at);
            set_note(state, "maodao", "已自动续期").await;
            REFRESH_INTERVAL_MS
        }
        Err(e) => handle_refresh_error(state, "maodao", &e).await,
    }
}

/// 小浣熊：JWT access + 单次消费 refresh_token。
async fn tick_raccoon(state: &Arc<AppState>) -> i64 {
    let _gate = state.refresh_gate.lock().await;
    let (mut creds, block) = {
        let store = state.store.lock().await;
        (
            store.credentials.raccoon.clone(),
            store.credentials.keepalive.blocks.get("raccoon").cloned(),
        )
    };
    if block_active(block.as_ref(), now_ms()) {
        return REFRESH_INTERVAL_MS;
    }
    if block.is_some() {
        let mut store = state.store.lock().await;
        store.credentials.keepalive.blocks.remove("raccoon");
        store.save_credentials();
    }
    if creds.access_token.is_empty() {
        set_note(state, "raccoon", "未登录，跳过").await;
        return REFRESH_INTERVAL_MS;
    }
    if creds.refresh_token.is_empty() {
        set_note(state, "raccoon", "没有刷新令牌，过期后需重新扫码").await;
        return REFRESH_INTERVAL_MS;
    }
    let exp = if creds.expires_at_ms > 0 {
        creds.expires_at_ms
    } else {
        crate::store::jwt_exp_ms(&creds.access_token).unwrap_or(0)
    };
    if !should_renew(exp, now_ms(), REFRESH_LEAD_MS) {
        set_note(state, "raccoon", "窗外等待（过期前 1 小时内才续期）").await;
        return REFRESH_INTERVAL_MS;
    }
    match state.raccoon.refresh(&creds.refresh_token).await {
        Ok((access, refresh, new_exp)) => {
            creds.access_token = access;
            creds.refresh_token = refresh;
            creds.expires_at_ms = new_exp;
            {
                let mut store = state.store.lock().await;
                store.credentials.raccoon = creds.clone();
                store.save_credentials();
            }
            eprintln!("[keepalive] raccoon 已自动续期，新到期 {}", new_exp);
            set_note(state, "raccoon", "已自动续期").await;
            REFRESH_INTERVAL_MS
        }
        Err(e) => handle_refresh_error(state, "raccoon", &e).await,
    }
}

/// Loomy：无可续期凭据，等价动作是重扫桌面客户端登录态补会话。
async fn tick_loomy(state: &Arc<AppState>) {
    let found = crate::loomy::import_client_sessions();
    let mut added = 0usize;
    let mut touched = false;
    {
        let mut store = state.store.lock().await;
        for session in found {
            if let Some(existing) = store
                .credentials
                .loomy
                .sessions
                .iter_mut()
                .find(|s| s.session == session.session)
            {
                // 客户端把同一会话续期了：跟进过期时间。
                if existing.expires_at_ms != session.expires_at_ms && session.expires_at_ms > 0 {
                    existing.expires_at_ms = session.expires_at_ms;
                    touched = true;
                }
                continue;
            }
            store.credentials.loomy.sessions.push(session);
            added += 1;
        }
        if added > 0 || touched {
            store.save_credentials();
        }
    }
    if added > 0 {
        eprintln!("[keepalive] loomy 客户端重扫：新增 {} 个会话", added);
        set_note(state, "loomy", &format!("客户端重扫新增 {} 个会话", added)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_only_for_auth_killed_credentials() {
        assert_eq!(
            classify("upstream HTTP 400: {\"error\":\"invalid_grant\"}"),
            ErrorClass::Terminal
        );
        assert_eq!(classify("upstream HTTP 401: unauthorized"), ErrorClass::Terminal);
        assert_eq!(classify("upstream HTTP 403: forbidden"), ErrorClass::Terminal);
        assert_eq!(classify("码道没有刷新令牌，请重新登录"), ErrorClass::Terminal);
    }

    #[test]
    fn transient_and_network_never_block() {
        // 429 与 5xx 是瞬态——一次抖动不得误杀在线会话。
        assert_eq!(classify("upstream HTTP 429: too many"), ErrorClass::Transient);
        assert_eq!(classify("upstream HTTP 503: busy"), ErrorClass::Transient);
        assert_eq!(classify("raccoon refresh refused (HTTP 200 code 500 internal)"), ErrorClass::Transient);
        assert_eq!(
            classify("maodao token refresh: error sending request for url"),
            ErrorClass::Network
        );
        assert_eq!(classify("raccoon refresh: dns failure"), ErrorClass::Network);
    }

    #[test]
    fn http_code_parses_both_shapes() {
        assert_eq!(http_code("upstream HTTP 401: x"), Some(401));
        assert_eq!(http_code("refused (HTTP 403 code 1002 msg)"), Some(403));
        assert_eq!(http_code("no code here"), None);
    }

    #[test]
    fn renew_fires_only_inside_the_lead_window() {
        let now = 1_000_000i64;
        assert!(should_renew(now + 30 * 60 * 1000, now, REFRESH_LEAD_MS));
        assert!(should_renew(now, now, REFRESH_LEAD_MS)); // 未知到期 = 续
        assert!(should_renew(now - 1, now, REFRESH_LEAD_MS));
        assert!(!should_renew(now + 2 * 3600 * 1000, now, REFRESH_LEAD_MS));
    }

    #[test]
    fn blocks_self_expire() {
        let now = 2_000_000i64;
        let fresh = BlockInfo { reason: "x".into(), at_ms: now - 1000 };
        let old = BlockInfo { reason: "x".into(), at_ms: now - BLOCK_AUTO_EXPIRE_MS - 1 };
        assert!(block_active(Some(&fresh), now));
        assert!(!block_active(Some(&old), now));
        assert!(!block_active(None, now));
    }
}
