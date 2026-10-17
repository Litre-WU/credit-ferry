//! Client config export: WorkBuddy models.json merge, Qwen Code / DSH snippets.
//!
//! WorkBuddy writes follow the same discipline as OC Ferry:
//! only entries carrying our owner mark are added or removed, the user's own
//! entries are never touched, every write is backed up (last 5) and atomic.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::server::client_model_id;
use crate::store::{ModelEntry, Store};

pub const OWNER_FIELD: &str = "creditFerryOwner";

/// The output size a client may safely ask for.
///
/// Catalogs report a model's theoretical maximum (Maodao says 393216) while the
/// gateway's own range validator accepts far less, so exporting the raw number
/// hands every client a request that comes back as a 400/502. Zero means the
/// upstream never told us, and is left alone for each client's own default.
fn advertised_max_output(entry: &ModelEntry) -> u64 {
    if entry.max_output == 0 {
        return 0;
    }
    entry.max_output.min(crate::server::MAX_TOKENS_CAP)
}

/// Resolve the WorkBuddy models.json path on Windows.
pub fn resolve_models_file(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.models_file {
        if !saved.is_empty() && Path::new(saved).is_file() {
            return Some(PathBuf::from(saved));
        }
    }
    if let Ok(dir) = std::env::var("WORKBUDDY_CONFIG_DIR") {
        let candidate = Path::new(&dir).join("models.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".workbuddy").join("models.json"))
}

fn is_owned(entry: &Value) -> bool {
    entry
        .get(OWNER_FIELD)
        .and_then(|v| v.as_str())
        .map(|v| v == crate::store::OWNER)
        .unwrap_or(false)
}

/// Merge our entries into a models.json document (array or {models:[…]}).
pub fn merge_models(document: &Value, models: &[crate::store::ModelEntry], endpoint: &str, key: &str) -> Value {
    let list = if document.is_array() {
        document.as_array().cloned().unwrap_or_default()
    } else {
        document
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let kept: Vec<Value> = list.iter().filter(|m| !is_owned(m)).cloned().collect();
    let conflicts: std::collections::HashSet<String> = kept
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    let mut entries = Vec::new();
    for entry in models {
        let id = client_model_id(entry);
        if conflicts.contains(&id) {
            continue; // never shadow a user entry with the same id
        }
        entries.push(json!({
            "id": id,
            "name": id,
            "vendor": "Custom",
            "url": format!("{}/chat/completions", endpoint),
            "apiKey": key,
            "supportsToolCall": true,
            "supportsImages": entry.vision,
            "supportsReasoning": entry.multiplier.is_some(),
            "maxInputTokens": entry.context,
            "maxOutputTokens": advertised_max_output(entry),
            OWNER_FIELD: crate::store::OWNER,
        }));
    }
    let combined: Vec<Value> = kept.into_iter().chain(entries.into_iter()).collect();
    if document.is_array() {
        Value::Array(combined)
    } else {
        let mut updated = document.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("models".into(), Value::Array(combined));
        }
        updated
    }
}

pub(crate) fn prune_backups(file: &Path, keep: usize) {
    let dir = match file.parent() {
        Some(d) => d,
        None => return,
    };
    let prefix = format!(
        "{}.credit-ferry-",
        file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()
    );
    if let Ok(read) = fs::read_dir(dir) {
        let mut names: Vec<String> = read
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) && name.ends_with(".bak") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        for name in names.iter().take(names.len().saturating_sub(keep)) {
            let _ = fs::remove_file(dir.join(name));
        }
    }
}

/// Write the merged document to models.json (backup + atomic replace).
pub fn export_workbuddy(store: &Store, endpoint: &str, key: &str) -> Result<Value, String> {
    let file = resolve_models_file(store).ok_or_else(|| "未找到 WorkBuddy models.json".to_string())?;
    export_models_json(store, &file, endpoint, key)
}

/// WorkBuddy International keeps its own config home: ~/.workbuddy-ai.
pub fn resolve_models_file_intl(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.models_file_intl {
        if !saved.is_empty() {
            return Some(PathBuf::from(saved));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".workbuddy-ai").join("models.json"))
}

/// Merge our entries into the International edition's models.json.
pub fn export_workbuddy_intl(store: &Store, endpoint: &str, key: &str) -> Result<Value, String> {
    let file = resolve_models_file_intl(store)
        .ok_or_else(|| "未找到 ~/.workbuddy-ai/models.json".to_string())?;
    if !file.parent().map(|p| p.is_dir()).unwrap_or(false) {
        return Err("WorkBuddy 国际版配置目录不存在（~/.workbuddy-ai）".to_string());
    }
    export_models_json(store, &file, endpoint, key)
}

pub fn cleanup_workbuddy_intl(store: &Store) -> Result<Value, String> {
    let file = resolve_models_file_intl(store)
        .ok_or_else(|| "未找到 ~/.workbuddy-ai/models.json".to_string())?;
    cleanup_models_json(&file)
}

/// Shared models.json writer for both WorkBuddy editions.
fn export_models_json(store: &Store, file: &Path, endpoint: &str, key: &str) -> Result<Value, String> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let old = fs::read_to_string(file).unwrap_or_default();
    let document: Value = if old.trim().is_empty() {
        Value::Array(vec![])
    } else {
        serde_json::from_str(&old).map_err(|_| "models.json 不是合法 JSON，未改动".to_string())?
    };
    let mut models = store.models.loomy.clone();
    models.extend(store.models.raccoon.clone());
    models.extend(store.models.opencode.clone());
    models.extend(store.models.maodao.clone());
    models.extend(store.models.kilo.clone());
    models.extend(store.models.zen.clone());
    models.extend(store.models.ofm.clone());
    // Models with a fresh failing verdict are not exported.
    models.retain(|m| store.models.publishable(&m.source, &m.upstream_id));
    let merged = merge_models(&document, &models, endpoint, key);
    let owned = if merged.is_array() {
        merged.as_array().map(|a| a.iter().filter(|m| is_owned(m)).count()).unwrap_or(0)
    } else {
        merged
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().filter(|m| is_owned(m)).count())
            .unwrap_or(0)
    };
    if merged == document {
        return Ok(json!({ "changed": false, "count": owned, "path": file }));
    }
    if !old.trim().is_empty() {
        let backup = format!(
            "{}.credit-ferry-{}.bak",
            file.display(),
            crate::store::now_ms()
        );
        fs::write(&backup, &old).map_err(|e| e.to_string())?;
        prune_backups(file, 5);
    }
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, serde_json::to_string_pretty(&merged).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(json!({ "changed": true, "count": owned, "path": file }))
}

/// Remove our entries (used when the user asks to clean up).
pub fn cleanup_workbuddy(store: &Store) -> Result<Value, String> {
    let file = resolve_models_file(store).ok_or_else(|| "未找到 WorkBuddy models.json".to_string())?;
    cleanup_models_json(&file)
}

/// Shared models.json pruner for both WorkBuddy editions.
fn cleanup_models_json(file: &Path) -> Result<Value, String> {
    let old = fs::read_to_string(file).unwrap_or_default();
    if old.trim().is_empty() {
        return Ok(json!({ "changed": false, "count": 0, "path": file }));
    }
    let document: Value = serde_json::from_str(&old).map_err(|_| "models.json 不是合法 JSON".to_string())?;
    let list = if document.is_array() {
        document.as_array().cloned().unwrap_or_default()
    } else {
        document.get("models").and_then(|m| m.as_array()).cloned().unwrap_or_default()
    };
    let kept: Vec<Value> = list.iter().filter(|m| !is_owned(m)).cloned().collect();
    let removed = list.len() - kept.len();
    if removed == 0 {
        return Ok(json!({ "changed": false, "count": 0, "path": file }));
    }
    let merged = if document.is_array() {
        Value::Array(kept)
    } else {
        let mut updated = document.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.insert("models".into(), Value::Array(kept));
        }
        updated
    };
    let backup = format!("{}.credit-ferry-{}.bak", file.display(), crate::store::now_ms());
    let _ = fs::write(&backup, &old);
    prune_backups(file, 5);
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, serde_json::to_string_pretty(&merged).unwrap_or_default()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| e.to_string())?;
    Ok(json!({ "changed": true, "count": removed, "path": file }))
}

// ---------------------------------------------------------------- DeepSeek Harness

/// DSH (DeepSeek Harness desktop) reads its model providers from
/// `~/.dsh/profiles/desktop/cordis.patch.yml`, under the `llm-pi-ai` loader
/// patch → `config.providers`. Each provider is an OpenAI-completions entry
/// whose key comes from an env var declared in `~/.dsh/.credentials.yaml`
/// under `refs`. We keep the user's file byte-for-byte and only splice in a
/// marker-delimited `credit-ferry` block, so "摘除" removes exactly our lines.
pub const DSH_API_KEY_ENV: &str = "CREDITFERRY_API_KEY";
const DSH_BLOCK_HEAD: &str = "# >>> credit-ferry >>>";
const DSH_BLOCK_TAIL: &str = "# <<< credit-ferry <<<";

/// The desktop profile dir DSH actually reads.
fn dsh_profile_dir(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.dsh_file {
        if !saved.is_empty() {
            return Some(PathBuf::from(saved));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".dsh").join("profiles").join("desktop"))
}

/// Public probe for the panel: does the DSH desktop patch file exist?
pub fn resolve_dsh_patch(store: &Store) -> Option<PathBuf> {
    dsh_profile_dir(store).map(|d| d.join("cordis.patch.yml"))
}

fn dsh_credentials_path(store: &Store) -> PathBuf {
    match dsh_profile_dir(store) {
        Some(dir) => dir
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join(".credentials.yaml"))
            .unwrap_or_else(|| PathBuf::from(".credentials.yaml")),
        None => PathBuf::from(".credentials.yaml"),
    }
}

/// Remove every previously-written credit-ferry provider block.
/// Marker lines are indented in the written file, so compare with trim().
/// Tolerant of duplicates: an extra HEAD only restarts skipping, an unmatched
/// tail never eats user lines.
fn strip_dsh_block(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == DSH_BLOCK_HEAD {
            skipping = true;
            continue;
        }
        if line.trim() == DSH_BLOCK_TAIL {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// Count `- id:` lines inside all our marker blocks (excludes user models).
fn count_dsh_models(text: &str) -> usize {
    let mut count = 0usize;
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t == DSH_BLOCK_HEAD {
            inside = true;
            continue;
        }
        if t == DSH_BLOCK_TAIL {
            inside = false;
            continue;
        }
        if inside && line.trim_start().starts_with("- id: ") {
            count += 1;
        }
    }
    count
}

/// Render the credit-ferry provider block at the given indent.
/// Returns (yaml text, model count).
fn dsh_provider_block(store: &Store, endpoint: &str, indent: usize) -> (String, usize) {
    let pad = " ".repeat(indent);
    let mut models = store.models.loomy.clone();
    models.extend(store.models.raccoon.clone());
    models.extend(store.models.opencode.clone());
    models.extend(store.models.maodao.clone());
    models.extend(store.models.kilo.clone());
    models.extend(store.models.zen.clone());
    models.extend(store.models.ofm.clone());
    models.retain(|m| store.models.publishable(&m.source, &m.upstream_id));
    if models.is_empty() {
        return (String::new(), 0);
    }
    let mut s = String::new();
    s.push_str(&format!("{}{}\n", pad, DSH_BLOCK_HEAD));
    s.push_str(&format!("{}credit-ferry:\n", pad));
    s.push_str(&format!("{}  displayName: CreditFerry\n", pad));
    s.push_str(&format!("{}  apiKeyEnv: {}\n", pad, DSH_API_KEY_ENV));
    s.push_str(&format!("{}  api: openai-completions\n", pad));
    s.push_str(&format!("{}  baseURL: {}\n", pad, endpoint));
    s.push_str(&format!("{}  models:\n", pad));
    for entry in &models {
        let id = client_model_id(entry);
        let ctx = entry.context.max(128000);
        s.push_str(&format!("{}    - id: \"{}\"\n", pad, id));
        s.push_str(&format!("{}      name: \"{}\"\n", pad, id));
        s.push_str(&format!("{}      contextWindow: {}\n", pad, ctx));
        let out = advertised_max_output(entry);
        if out > 0 {
            s.push_str(&format!("{}      maxTokens: {}\n", pad, out));
        }
    }
    s.push_str(&format!("{}{}\n", pad, DSH_BLOCK_TAIL));
    (s, models.len())
}

/// Insert the block right after the first `providers:` line that belongs to
/// the `llm-pi-ai` loader patch (falling back to any `providers:` if the
/// loader block is absent), re-indented to wherever that key actually lives.
fn splice_dsh_patch(text: &str, block: &str, block_indent: usize) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let anchor = lines
        .iter()
        .position(|l| l.trim() == "- id: llm-pi-ai")
        .unwrap_or(0);
    let mut insert_at = None;
    let mut target_indent = 6usize;
    for (i, line) in lines.iter().enumerate().skip(anchor) {
        let trimmed = line.trim_end();
        if trimmed.ends_with("providers:") && !trimmed.contains('#') {
            target_indent = (trimmed.len() - trimmed.trim_start().len()) + 2;
            insert_at = Some(i + 1);
            break;
        }
    }
    let idx = insert_at.ok_or_else(|| {
        "cordis.patch.yml 里没有 providers: 段，请先在 DSH 里添加任意自定义模型再试".to_string()
    })?;
    let delta = target_indent as isize - block_indent as isize;
    let reindented: String = block
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if delta >= 0 {
                format!("{}{}", " ".repeat(delta as usize), line)
            } else {
                let remove = (-delta) as usize;
                let lead = line.len() - line.trim_start().len();
                line[lead.min(remove)..].to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push('\n');
        if i + 1 == idx {
            out.push_str(&reindented);
            if !reindented.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// Add or update one `refs:` entry in `.credentials.yaml`.
fn set_dsh_ref(text: &str, env: &str, value: &str) -> String {
    let entry = format!("  {}: {}", env, value);
    let mut out: Vec<String> = Vec::new();
    let mut in_refs = false;
    let mut replaced = false;
    for line in text.lines() {
        if line.trim_end() == "refs:" {
            in_refs = true;
            out.push(line.to_string());
            continue;
        }
        // A new top-level key (no leading space, non-empty) ends the refs map.
        if in_refs && !line.starts_with(' ') && !line.trim().is_empty() {
            if !replaced {
                out.push(entry.clone());
                replaced = true;
            }
            in_refs = false;
        }
        if in_refs && line.trim_start().starts_with(&format!("{}:", env)) {
            out.push(entry.clone());
            replaced = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        if !in_refs {
            out.push("refs:".to_string());
        }
        out.push(entry);
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Drop our key from `.credentials.yaml`.
fn remove_dsh_ref(text: &str, env: &str) -> String {
    let prefix = format!("  {}:", env);
    let out: Vec<String> = text
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(|l| l.to_string())
        .collect();
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// One-click configure DeepSeek Harness desktop.
pub fn export_dsh(store: &Store, endpoint: &str, key: &str) -> Result<Value, String> {
    let dir = dsh_profile_dir(store).ok_or_else(|| "无法定位用户目录".to_string())?;
    let patch = dir.join("cordis.patch.yml");
    if !patch.is_file() {
        return Err("未找到 DeepSeek Harness 桌面配置（~/.dsh/profiles/desktop/cordis.patch.yml），请先安装并启动一次 DSH".to_string());
    }
    let (block, count) = dsh_provider_block(store, endpoint, 6);
    if block.is_empty() {
        return Err("没有可导出的模型（先刷新上游并通过验证）".to_string());
    }
    let old = fs::read_to_string(&patch).map_err(|e| e.to_string())?;
    let stripped = strip_dsh_block(&old);
    let merged = splice_dsh_patch(&stripped, &block, 6)?;
    if merged != old {
        write_file_backed_up(&patch, &old, &merged)?;
    }
    // Inject the API key into the credentials refs.
    let cred = dsh_credentials_path(store);
    if let Ok(old_cred) = fs::read_to_string(&cred) {
        let new_cred = set_dsh_ref(&old_cred, DSH_API_KEY_ENV, key);
        if new_cred != old_cred {
            write_file_backed_up(&cred, &old_cred, &new_cred)?;
        }
    }
    Ok(json!({
        "changed": true,
        "count": count,
        "path": patch,
        "note": "重启 DeepSeek Harness 后，在模型选择器里选 CreditFerry 提供方",
    }))
}

/// Remove our provider + key from DeepSeek Harness.
pub fn cleanup_dsh(store: &Store) -> Result<Value, String> {
    let dir = match dsh_profile_dir(store) {
        Some(d) => d,
        None => return Ok(json!({ "changed": false, "count": 0 })),
    };
    let patch = dir.join("cordis.patch.yml");
    let mut removed = 0usize;
    if patch.is_file() {
        let old = fs::read_to_string(&patch).map_err(|e| e.to_string())?;
        if old.contains(DSH_BLOCK_HEAD) {
            // Count only the models inside our own marker blocks.
            removed = count_dsh_models(&old);
            let stripped = strip_dsh_block(&old);
            if stripped != old {
                write_file_backed_up(&patch, &old, &stripped)?;
            }
        }
    }
    let cred = dsh_credentials_path(store);
    if let Ok(old_cred) = fs::read_to_string(&cred) {
        let new_cred = remove_dsh_ref(&old_cred, DSH_API_KEY_ENV);
        if new_cred != old_cred {
            write_file_backed_up(&cred, &old_cred, &new_cred)?;
        }
    }
    Ok(json!({ "changed": removed > 0, "count": removed, "path": patch }))
}

/// Backup + atomic replace for a plain text file.
fn write_file_backed_up(file: &Path, old: &str, new: &str) -> Result<(), String> {
    let backup = format!("{}.credit-ferry-{}.bak", file.display(), crate::store::now_ms());
    let _ = fs::write(&backup, old);
    prune_backups(file, 5);
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, new).map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

// ---------------------------------------------------------------- Qoder CN / Intl

pub const QODER_PROVIDER_PREFIX: &str = "qoder-custom-credit-ferry";
pub const QODER_OWNER_FIELD: &str = "creditFerryOwner";

/// Qoder (CN and International) store custom providers in
/// `<config>/settings.json` under `providers.qoder-custom-<id>`. We write one
/// provider carrying every publishable model; the owner mark makes removal
/// safe. CN home is `~/.qoder-cn`, International is `~/.qoder`.
pub fn resolve_qoder_settings(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.qoder_file {
        if !saved.is_empty() {
            return Some(PathBuf::from(saved));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".qoder-cn").join("settings.json"))
}

/// Qoder International's config home: `~/.qoder`.
pub fn resolve_qoder_settings_intl(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.qoder_file_intl {
        if !saved.is_empty() {
            return Some(PathBuf::from(saved));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".qoder").join("settings.json"))
}

pub fn qoder_provider_id(port: u16) -> String {
    // Deterministic key so repeated exports update in place instead of piling up.
    format!("{}-{:x}", QODER_PROVIDER_PREFIX, port)
}

/// Shared Qoder provider writer (CN and International share the schema).
fn export_qoder_settings(store: &Store, file: &Path, endpoint: &str, key: &str) -> Result<Value, String> {
    if !file.is_file() {
        return Err(format!(
            "Qoder 配置不存在（{}），请先安装并启动一次 Qoder",
            file.display()
        ));
    }
    let old = fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut document: Value =
        serde_json::from_str(&old).map_err(|_| "settings.json 不是合法 JSON，未改动".to_string())?;
    if !document.is_object() {
        return Err("settings.json 顶层不是对象，未改动".to_string());
    }

    let mut models = store.models.loomy.clone();
    models.extend(store.models.raccoon.clone());
    models.extend(store.models.opencode.clone());
    models.extend(store.models.maodao.clone());
    models.extend(store.models.kilo.clone());
    models.extend(store.models.zen.clone());
    models.extend(store.models.ofm.clone());
    models.retain(|m| store.models.publishable(&m.source, &m.upstream_id));
    if models.is_empty() {
        return Err("没有可导出的模型（先刷新上游并通过验证）".to_string());
    }

    let model_list: Vec<Value> = models
        .iter()
        .map(|entry| {
            // `model` goes on the wire, so it must be an id resolve_model
            // accepts — the prefixed client id, same as WorkBuddy exports.
            let id = client_model_id(entry);
            let out = advertised_max_output(entry);
            json!({
                "model": id.clone(),
                "displayName": id,
                "contextWindow": entry.context.max(128000) as u64,
                "maxOutputTokens": if out == 0 { 8192 } else { out },
                "capabilities": {
                    "vision": entry.vision
                }
            })
        })
        .collect();
    let first = client_model_id(&models[0]);
    let provider = json!({
        "baseUrl": endpoint,
        "apiKey": key,
        "type": "openai-compatible",
        "protocol": "openai",
        "authType": "bearer",
        "model": first,
        "models": model_list.clone(),
        QODER_OWNER_FIELD: crate::store::OWNER,
    });

    let providers = document
        .as_object_mut()
        .unwrap()
        .entry("providers")
        .or_insert_with(|| json!({}));
    let providers = providers.as_object_mut().ok_or_else(|| "providers 不是对象".to_string())?;
    let id = qoder_provider_id(store.settings.port);
    let changed = providers.get(&id) != Some(&provider);
    providers.insert(id.clone(), provider);

    if !changed {
        return Ok(json!({ "changed": false, "count": model_list.len(), "path": file }));
    }
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, serde_json::to_string_pretty(&document).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    let backup = format!("{}.credit-ferry-{}.bak", file.display(), crate::store::now_ms());
    let _ = fs::write(&backup, &old);
    prune_backups(file, 5);
    Ok(json!({ "changed": true, "count": model_list.len(), "path": file, "providerId": id }))
}

pub fn export_qoder(store: &Store, endpoint: &str, key: &str) -> Result<Value, String> {
    let file = resolve_qoder_settings(store)
        .ok_or_else(|| "未找到 ~/.qoder-cn/settings.json".to_string())?;
    export_qoder_settings(store, &file, endpoint, key)
}

pub fn export_qoder_intl(store: &Store, endpoint: &str, key: &str) -> Result<Value, String> {
    let file = resolve_qoder_settings_intl(store)
        .ok_or_else(|| "未找到 ~/.qoder/settings.json".to_string())?;
    export_qoder_settings(store, &file, endpoint, key)
}

/// Shared Qoder provider pruner.
fn cleanup_qoder_settings(file: &Path) -> Result<Value, String> {
    if !file.is_file() {
        return Ok(json!({ "changed": false, "count": 0, "path": file }));
    }
    let old = fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut document: Value =
        serde_json::from_str(&old).map_err(|_| "settings.json 不是合法 JSON".to_string())?;
    let mut removed = 0usize;
    if let Some(providers) = document.get_mut("providers").and_then(|p| p.as_object_mut()) {
        let stale: Vec<String> = providers
            .iter()
            .filter(|(_, value)| {
                value
                    .get(QODER_OWNER_FIELD)
                    .and_then(|o| o.as_str())
                    .map(|o| o == crate::store::OWNER)
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            providers.remove(&key);
            removed += 1;
        }
    }
    if removed == 0 {
        return Ok(json!({ "changed": false, "count": 0, "path": file }));
    }
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, serde_json::to_string_pretty(&document).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    let backup = format!("{}.credit-ferry-{}.bak", file.display(), crate::store::now_ms());
    let _ = fs::write(&backup, &old);
    prune_backups(file, 5);
    Ok(json!({ "changed": true, "count": removed, "path": file }))
}

pub fn cleanup_qoder(store: &Store) -> Result<Value, String> {
    let file = match resolve_qoder_settings(store) {
        Some(f) => f,
        None => return Ok(json!({ "changed": false, "count": 0 })),
    };
    cleanup_qoder_settings(&file)
}

pub fn cleanup_qoder_intl(store: &Store) -> Result<Value, String> {
    let file = match resolve_qoder_settings_intl(store) {
        Some(f) => f,
        None => return Ok(json!({ "changed": false, "count": 0 })),
    };
    cleanup_qoder_settings(&file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ModelEntry;

    fn entry(id: &str, name: &str, source: &str) -> ModelEntry {
        ModelEntry {
            upstream_id: id.into(),
            name: name.into(),
            source: source.into(),
            vision: false,
            context: 128000,
            max_output: 8192,
            multiplier: Some(1.0),
            billing_status: None,
        }
    }

    #[test]
    fn advertised_max_output_respects_the_upstream_ceiling() {
        let mut maodao = entry("deepseek-v4.1-flash", "Maodao", "maodao");
        maodao.max_output = 393216;
        assert_eq!(advertised_max_output(&maodao), crate::server::MAX_TOKENS_CAP);
        // An unknown output size stays unknown: each client has its own default.
        maodao.max_output = 0;
        assert_eq!(advertised_max_output(&maodao), 0);
        // Every source is capped, because every gateway rejects the catalog size.
        let mut raccoon = entry("sn-glm-5-3", "Raccoon", "raccoon");
        raccoon.max_output = 100000;
        assert_eq!(advertised_max_output(&raccoon), crate::server::MAX_TOKENS_CAP);
        assert_eq!(advertised_max_output(&entry("m", "Loomy", "loomy")), 8192);
    }

    #[test]
    fn merge_keeps_user_entries_and_marks_own() {
        let document = json!([
            { "id": "my-own-model", "name": "mine", "vendor": "Custom" }
        ]);
        let models = vec![entry("spark-x", "Spark X2.5", "loomy")];
        let merged = merge_models(&document, &models, "http://127.0.0.1:41990/v1", "k");
        let list = merged.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].get("creditFerryOwner").is_none());
        assert_eq!(list[1]["creditFerryOwner"], "credit-ferry-v1");
        assert_eq!(list[1]["id"], "Loomy · Spark X2.5");
    }

    #[test]
    fn merge_never_shadows_conflicting_user_id() {
        let document = json!([
            { "id": "Loomy · Spark X2.5", "name": "mine" }
        ]);
        let models = vec![entry("spark-x", "Spark X2.5", "loomy")];
        let merged = merge_models(&document, &models, "http://x/v1", "k");
        assert_eq!(merged.as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_object_form_preserves_other_keys() {
        let document = json!({ "version": 2, "models": [{ "id": "u" }] });
        let merged = merge_models(&document, &[entry("sn-a", "A", "raccoon")], "http://x/v1", "k");
        assert_eq!(merged["version"], 2);
        assert_eq!(merged["models"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn owned_detection_rejects_foreign_values() {
        assert!(is_owned(&json!({ "creditFerryOwner": "credit-ferry-v1" })));
        assert!(!is_owned(&json!({ "creditFerryOwner": "someone-else" })));
        assert!(!is_owned(&json!({ "id": "x" })));
    }

    #[test]
    fn strip_dsh_block_removes_even_indented_duplicates() {
        let text = format!(
            "- id: llm-pi-ai\n  config:\n    providers:\n      {}\n      credit-ferry:\n        models:\n        - id: \"a\"\n      {}\n      {}\n      credit-ferry:\n        models:\n        - id: \"b\"\n      {}\n      sensenova:\n        api: openai-completions\n",
            DSH_BLOCK_HEAD, DSH_BLOCK_TAIL, DSH_BLOCK_HEAD, DSH_BLOCK_TAIL
        );
        let stripped = strip_dsh_block(&text);
        assert!(!stripped.contains(DSH_BLOCK_HEAD));
        assert!(!stripped.contains(DSH_BLOCK_TAIL));
        assert!(!stripped.contains("credit-ferry:"));
        assert!(!stripped.contains("- id: \"a\""));
        assert!(stripped.contains("sensenova:"));
        assert_eq!(count_dsh_models(&text), 2);
        assert_eq!(count_dsh_models(&stripped), 0);
    }

    #[test]
    fn splice_dsh_patch_anchors_after_llm_pi_ai() {
        let text = "- id: other\n  config:\n    providers:\n      foo:\n- id: llm-pi-ai\n  config:\n    providers:\n      atria:\n";
        let block = format!(
            "{}\ncredit-ferry:\n  models:\n  - id: \"x\"\n{}\n",
            DSH_BLOCK_HEAD, DSH_BLOCK_TAIL
        );
        let merged = splice_dsh_patch(text, &block, 0).expect("splice");
        // our block must land under the llm-pi-ai providers, not the other loader's
        let at = merged.find(DSH_BLOCK_HEAD).unwrap();
        let pi = merged.find("- id: llm-pi-ai").unwrap();
        assert!(at > pi);
        assert!(merged.find("foo:").unwrap() < pi);
    }

    #[test]
    fn dsh_refs_roundtrip_adds_updates_and_removes() {
        // Fresh credentials file with an unrelated ref we must preserve.
        let base = "creds:\n  token: abc\n";

        // 1) add our ref under a new top-level refs: map
        let added = set_dsh_ref(base, DSH_API_KEY_ENV, "K1");
        assert!(added.contains("refs:"));
        assert!(added.contains(&format!("  {}: {}", DSH_API_KEY_ENV, "K1")));
        assert!(added.contains("  token: abc")); // foreign entry kept

        // 2) update the same ref, stays a single entry
        let updated = set_dsh_ref(&added, DSH_API_KEY_ENV, "K2");
        assert_eq!(updated.matches(DSH_API_KEY_ENV).count(), 1);
        assert!(updated.contains(&format!("  {}: {}", DSH_API_KEY_ENV, "K2")));
        assert!(!updated.contains("K1"));

        // 3) idempotent add of an identical value
        let again = set_dsh_ref(&updated, DSH_API_KEY_ENV, "K2");
        assert_eq!(again.matches(DSH_API_KEY_ENV).count(), 1);

        // 4) remove our ref, keep the foreign one
        let removed = remove_dsh_ref(&updated, DSH_API_KEY_ENV);
        assert!(!removed.contains(DSH_API_KEY_ENV));
        assert!(removed.contains("  token: abc"));
    }

    #[test]
    fn dsh_refs_appends_into_existing_refs_map() {
        // An existing top-level refs: with a foreign key, our line slots in.
        let text = "refs:\n  OTHER_KEY: xyz\ncreds:\n  a: b\n";
        let out = set_dsh_ref(text, DSH_API_KEY_ENV, "V");
        assert!(out.contains(&format!("  {}: V", DSH_API_KEY_ENV)));
        assert!(out.contains("  OTHER_KEY: xyz"));
        // and removing it returns to the foreign-only map
        let back = remove_dsh_ref(&out, DSH_API_KEY_ENV);
        assert!(!back.contains(DSH_API_KEY_ENV));
        assert!(back.contains("  OTHER_KEY: xyz"));
    }

    #[test]
    fn dsh_export_cleanup_roundtrip_in_tempdir() {
        use crate::store::Store;
        let tmp = std::env::temp_dir().join(format!("cf-dsh-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let profile = tmp.join("profiles").join("desktop");
        std::fs::create_dir_all(&profile).unwrap();
        let patch = profile.join("cordis.patch.yml");
        let patch_text = "- id: llm-pi-ai\n  name: x\n  config:\n    providers:\n      atria:\n        api: openai-completions\n";
        std::fs::write(&patch, patch_text).unwrap();
        let cred = tmp.join(".credentials.yaml");
        std::fs::write(&cred, "creds:\n  token: abc\n").unwrap();

        let mut store = Store::open(tmp.join("store"));
        store.settings.dsh_file = Some(profile.to_string_lossy().to_string());
        store.models.loomy.push(entry("spark-x", "Spark X", "loomy"));

        // export inserts our block + refs key, preserving the user provider.
        let r = export_dsh(&store, "http://127.0.0.1:6666/v1", "KEY123").expect("export");
        assert_eq!(r["count"], 1);
        let after_export = std::fs::read_to_string(&patch).unwrap();
        assert!(after_export.contains(DSH_BLOCK_HEAD));
        assert!(after_export.contains("credit-ferry:"));
        assert!(after_export.contains("atria:"));
        let after_cred = std::fs::read_to_string(&cred).unwrap();
        assert!(after_cred.contains("CREDITFERRY_API_KEY: KEY123"));

        // cleanup removes our block + refs key, keeps the user provider.
        let c = cleanup_dsh(&store).expect("cleanup");
        assert_eq!(c["count"], 1);
        let after_clean = std::fs::read_to_string(&patch).unwrap();
        assert!(!after_clean.contains(DSH_BLOCK_HEAD));
        assert!(!after_clean.contains("credit-ferry:"));
        assert!(after_clean.contains("atria:"));
        let after_cred2 = std::fs::read_to_string(&cred).unwrap();
        assert!(!after_cred2.contains("CREDITFERRY_API_KEY"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dsh_export_graceful_error_without_anchor() {
        use crate::store::Store;
        let tmp = std::env::temp_dir().join(format!("cf-dsh-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let profile = tmp.join("profiles").join("desktop");
        std::fs::create_dir_all(&profile).unwrap();
        // a clean patch with NO llm-pi-ai / providers anchor
        let patch = profile.join("cordis.patch.yml");
        std::fs::write(&patch, "- id: ui-chat\n  config:\n    x: 1\n").unwrap();

        let mut store = Store::open(tmp.join("store"));
        store.settings.dsh_file = Some(profile.to_string_lossy().to_string());
        store.models.loomy.push(entry("spark-x", "Spark X", "loomy"));

        let err = export_dsh(&store, "http://127.0.0.1:6666/v1", "K").unwrap_err();
        assert!(err.contains("providers:"), "unexpected error: {err}");
        // the user file must be left byte-for-byte untouched
        assert_eq!(std::fs::read_to_string(&patch).unwrap(), "- id: ui-chat\n  config:\n    x: 1\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// One-shot harness (ignored by default): run with
    /// `cargo test --release -- --ignored export_all_clients_once`
    /// to write the real exports into ~/.workbuddy-ai and ~/.qoder-cn.
    #[test]
    #[ignore]
    fn export_all_clients_once() {
        let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap()).join("CreditFerry");
        let store = Store::open(dir.clone());
        let key = store.load_or_create_api_key();
        let endpoint = format!("http://127.0.0.1:{}/v1", store.settings.port);
        let a = export_workbuddy_intl(&store, &endpoint, &key).expect("intl export failed");
        println!("intl: {}", a);
        let b = export_qoder(&store, &endpoint, &key).expect("qoder export failed");
        println!("qoder: {}", b);
        let c = export_workbuddy(&store, &endpoint, &key).expect("cn export failed");
        println!("cn: {}", c);
    }
}
