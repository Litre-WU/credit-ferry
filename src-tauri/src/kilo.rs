//! Kilo (kilo.ai) free-pool upstream — the keyless lane.
//!
//! Wire facts (ported from the Our Free Model service, `src/kilo.js` +
//! `src/catalog.js`, and re-verified live from this machine):
//! - Gateway `https://api.kilo.ai/api/gateway`, OpenAI-compatible, **no
//!   credential at all** — no key, no login, no fingerprint gate. That is
//!   the whole auth story for the `isFree: true` slice of the listing.
//! - `GET {base}/models` returns `{ data: [...] }` (~400 rows); only rows
//!   with `isFree === true` answer keyless — every other id on this gateway
//!   is a paid model that returns `PAID_MODEL_AUTH_REQUIRED` (a 401-class
//!   refusal), so a picker full of them would be worse than a short roster.
//! - `POST {base}/chat/completions` streams SSE; while the model is being
//!   scheduled the gateway emits `: KILO PROCESSING` comment lines, which a
//!   spec-compliant SSE reader skips (and which keep idle timeouts honest).
//! - Listing fields → ModelEntry: `context_length` ?? `top_provider.context_length`
//!   ?? 131072; output `top_provider.max_completion_tokens` ?? 32768;
//!   vision when `architecture.input_modalities` contains `image`.
//! - Privacy trade stated on Kilo's own model cards: free-pool prompts MAY
//!   be logged by the upstream provider and used to improve its services.
//!   This lane is for throwaway work, not secrets — the panel surfaces the
//!   warning next to every Kilo model.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::ModelEntry;

pub const KILO_BASE: &str = "https://api.kilo.ai/api/gateway";
pub const KILO_SOURCE: &str = "kilo";
const KILO_UA: &str = "credit-ferry";
const LISTING_TIMEOUT: Duration = Duration::from_secs(15);

pub struct KiloClient {
    http: Client,
    listing_http: Client,
}

/// True when the listing row belongs to the free pool: `isFree === true`
/// and a non-empty id. Everything else is a paid id this lane cannot serve.
pub fn is_free_row(row: &Value) -> bool {
    row.get("isFree").and_then(|v| v.as_bool()) == Some(true)
        && row.get("id").and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false)
}

fn num(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64())
}

/// The picker name: drop the `Vendor: ` prefix and the trailing `(free)`
/// qualifier. (`NVIDIA: Nemotron 3 Ultra (free)` → `Nemotron 3 Ultra`; the
/// `Kilo · ` source prefix is added by `client_model_id`, and the `org/`
/// part of the id is routing detail, never a shown name.)
pub fn kilo_display_name(row: &Value) -> String {
    let raw = row.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let mut bare = raw.clone();
    if bare.is_empty() {
        bare = row.get("id").and_then(|v| v.as_str()).unwrap_or("kilo").to_string();
    }
    // `^[^:]{1,40}:\s+` vendor prefix — only when the colon is early enough.
    if let Some(pos) = bare.find(": ") {
        if pos > 0 && pos <= 40 {
            bare = bare[pos + 2..].trim().to_string();
        }
    }
    if bare.to_lowercase().ends_with("(free)") {
        let len = bare.len() - "(free)".len();
        bare = bare[..len].trim().to_string();
    }
    if bare.is_empty() {
        bare = row.get("id").and_then(|v| v.as_str()).unwrap_or("model").to_string();
    }
    bare
}

/// One listing row → ModelEntry (field mapping mirrored from OFM's
/// `kiloEntryOf`, with its fallbacks: context 131072, output 32768).
pub fn kilo_entry_of(row: &Value) -> ModelEntry {
    let context = num(row.get("context_length"))
        .or_else(|| num(row.get("top_provider").and_then(|t| t.get("context_length"))))
        .unwrap_or(131_072);
    let output = num(row.get("top_provider").and_then(|t| t.get("max_completion_tokens")))
        .unwrap_or(32_768);
    let vision = row
        .get("architecture")
        .and_then(|a| a.get("input_modalities"))
        .and_then(|m| m.as_array())
        .map(|mods| mods.iter().any(|m| m.as_str() == Some("image")))
        .unwrap_or(false);
    ModelEntry {
        upstream_id: row.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
        name: kilo_display_name(row),
        source: KILO_SOURCE.to_string(),
        vision,
        context,
        max_output: output,
        multiplier: Some(0.0),
        billing_status: Some("free-pool（提示词可能被上游记录）".to_string()),
    }
}

/// Filter + dedupe a raw listing into the free roster (same discipline as
/// `buildKiloCatalog`: keep first-seen ids, drop everything else).
pub fn build_kilo_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        if !is_free_row(row) {
            continue;
        }
        let entry = kilo_entry_of(row);
        if entry.upstream_id.is_empty() || !seen.insert(entry.upstream_id.clone()) {
            continue;
        }
        out.push(entry);
    }
    out
}

impl KiloClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .user_agent(KILO_UA)
            .build()
            .unwrap_or_default();
        let listing_http = Client::builder()
            .timeout(LISTING_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(KILO_UA)
            .build()
            .unwrap_or_default();
        Self { http, listing_http }
    }

    /// One listing round: `GET {base}/models`, no auth. Returns free entries.
    pub async fn catalog(&self) -> Result<Vec<ModelEntry>, String> {
        let resp = self
            .listing_http
            .get(format!("{}/models", KILO_BASE))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("kilo listing: {}", e))?;
        let status = resp.status();
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("kilo listing body: {}", e))?;
        if !status.is_success() {
            let note = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(format!("upstream HTTP {}: {}", status.as_u16(), note));
        }
        let rows: Vec<Value> = value
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(build_kilo_catalog(&rows))
    }

    /// POST one turn; the response is passed through untouched (SSE or JSON,
    /// whatever the client asked for with `stream`).
    pub async fn chat(&self, body: &Value) -> Result<reqwest::Response, String> {
        self.http
            .post(format!("{}/chat/completions", KILO_BASE))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| format!("kilo chat: {}", e))
    }
}

/// The free-pool privacy disclaimer, surfaced in the panel and in exports.
pub fn privacy_note() -> Value {
    json!({
        "text": "Kilo 免费池无需登录，但上游声明免费车道提示词可能被记录并用于改进其服务——请勿在此车道发送敏感内容。",
        "severity": "warning"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, free: bool) -> Value {
        json!({ "id": id, "name": name, "isFree": free,
                "context_length": 200_000,
                "top_provider": { "max_completion_tokens": 8192 },
                "architecture": { "input_modalities": ["text", "image"] } })
    }

    #[test]
    fn free_filter_keeps_only_is_free_rows_with_ids() {
        let rows = vec![
            row("a:free", "Vendor A: Alpha (free)", true),
            row("b", "Vendor B: Beta", false),
            json!({ "id": "  ", "isFree": true }),
        ];
        let cat = build_kilo_catalog(&rows);
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0].upstream_id, "a:free");
        assert_eq!(cat[0].source, "kilo");
        assert!(cat[0].vision);
        assert_eq!(cat[0].context, 200_000);
        assert_eq!(cat[0].max_output, 8192);
    }

    #[test]
    fn display_name_strips_vendor_prefix_and_free_tag() {
        assert_eq!(kilo_display_name(&row("x", "NVIDIA: Nemotron 3 Ultra (free)", true)),
                   "Nemotron 3 Ultra");
        // no vendor colon → keep the name whole
        assert_eq!(kilo_display_name(&row("x", "Auto Free", true)), "Auto Free");
        // name missing → fall back to the id
        assert_eq!(kilo_display_name(&json!({ "id": "stealth/glyph", "isFree": true })),
                   "stealth/glyph");
    }

    #[test]
    fn field_fallbacks_match_ofm_mapping() {
        let entry = kilo_entry_of(&json!({ "id": "openrouter/free", "isFree": true }));
        assert_eq!(entry.context, 131_072);
        assert_eq!(entry.max_output, 32_768);
        assert!(!entry.vision);
        // top_provider.context_length also feeds the context when the
        // top-level field is absent.
        let entry2 = kilo_entry_of(&json!({
            "id": "l:free", "isFree": true,
            "top_provider": { "context_length": 1_000_000 }
        }));
        assert_eq!(entry2.context, 1_000_000);
    }

    #[test]
    fn duplicates_collapse_to_first() {
        let rows = vec![row("a:free", "A (free)", true), row("a:free", "A dup", true)];
        assert_eq!(build_kilo_catalog(&rows).len(), 1);
    }
}
