//! OpenCode upstream adapter — the third ferry source.
//!
//! Wire facts (from the OC Ferry reference implementation and its design doc):
//! - `opencode serve --pure --hostname 127.0.0.1 --port <free>` exposes a
//!   Basic-auth HTTP API (`/global/health`, `/provider`, `/session`,
//!   `/session/{id}/message`, `/permission`, `/event`).
//! - The catalog lives behind `/provider`: free means cost.input/output and
//!   cost.cache.read/write are ALL zero and the model is not deprecated —
//!   the name saying `free` does not count.
//! - The chat API is NOT OpenAI-compatible: a session carries a permission
//!   table (all native actions `ask`), a message carries
//!   `{model:{providerID,modelID}, system, parts, format}` and the answer
//!   arrives as `parts` (text and/or a completed `StructuredOutput` tool).
//! - The envelope protocol: with `format:{type:'json_schema'}` the model can
//!   only answer `{"content":..,"calls":[..]}` — calls are PROPOSALS, the
//!   external client owns execution. Native actions are auto-rejected here;
//!   StructuredOutput is not permission-gated, so the envelope survives.
//! - Usage comes back on the assistant message info (`tokens`); OpenCode's
//!   free pool reports no points, so the ledger records tokens only.
//! - The runtime is isolated: managed copy under the data dir, env whitelist,
//!   four XDG dirs redirected, random Basic password, autoupdate/share/
//!   project-config/external-skills all disabled. Download goes through the
//!   npm registry (opencode-ai) with sha512 verification; npmmirror is the
//!   fallback mirror and its bytes must match the official checksum.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use base64::Engine as _;
use reqwest::Client;
use serde_json::{json, Value};

use crate::store::{ModelEntry, OPENCODE_PKG, OPENCODE_BINARY};

pub const OPENCODE_PROVIDER: &str = "opencode";
const REGISTRIES: [&str; 2] = ["https://registry.npmjs.org", "https://registry.npmmirror.com"];
const SESSION_TTL: Duration = Duration::from_secs(300);
const MESSAGE_TTL: Duration = Duration::from_secs(600);

/// The envelope system prompt: the model may only PROPOSE actions as JSON.
pub const ENVELOPE_PROMPT: &str = "You are the reasoning component of an external assistant. \
Never invoke native tools. The only way to express an action is the JSON envelope \
{\"content\":\"a string, empty if only calling tools\",\"calls\":[{\"name\":\"an allowed tool name\",\"arguments\":{}}]}. \
Pick tool names and argument names verbatim from the tool list in this request. \
The external client owns execution and supplies tool results on the next request; your calls are proposals only. \
Ignore the local environment, working directory and file system; resolve paths only from the external conversation. \
Do not place dependent operations in the same calls array. Do not wrap the JSON in markdown fences.";

/// A running isolated OpenCode server.
pub struct OpenCodeRuntime {
    pub child: std::process::Child,
    pub base: String,
    pub password: String,
    pub version: String,
}

impl OpenCodeRuntime {
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn auth(password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", password))
    )
}

// ---------------------------------------------------------------- runtime

/// Windows: spawn a console-subsystem child without allocating a console
/// window. Without this, every opencode.exe launch flashes a black window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Apply the no-console-window flag (no-op off Windows).
pub fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Windows refuses to CreateProcess a non-PE file with a GUI error box
/// ("不支持的 16 位应用程序") — and the npm tarball's `bin/opencode.exe` is
/// exactly that: a JS stub. Probe the header before ever executing a
/// candidate; text stubs are rejected silently.
#[cfg(windows)]
fn looks_executable(file: &Path) -> bool {
    match std::fs::File::open(file) {
        Ok(mut f) => {
            let mut buf = [0u8; 2];
            f.read_exact(&mut buf).is_ok() && buf == *b"MZ"
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn looks_executable(file: &Path) -> bool {
    // Mach-O / ELF have no `MZ` header, so the Windows gate cannot be reused
    // here. `version_of` still runs the candidate and only trusts a well-formed
    // version string, so a shell shim fails harmlessly.
    file.is_file()
}

fn version_of(file: &Path) -> Option<String> {
    if !looks_executable(file) {
        return None;
    }
    let out = quiet(std::process::Command::new(file)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null()))
    .output()
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.split('.').count() == 3 {
        Some(text)
    } else {
        None
    }
}

/// Where a usable runtime may already exist: our managed copy first, then
/// another ferry's managed copy (OC Ferry), then the user's own installs.
fn local_candidates(data_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(read) = std::fs::read_dir(data_dir.join("runtime")) {
        let mut dirs: Vec<PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            out.push(dir.join(OPENCODE_BINARY));
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        // OC Ferry keeps its own managed runtime here.
        let oc = Path::new(&appdata).join("OC Ferry").join("runtime");
        if let Ok(read) = std::fs::read_dir(oc) {
            let mut dirs: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
            for dir in dirs {
                out.push(dir.join(OPENCODE_BINARY));
            }
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        out.push(Path::new(&home).join(".opencode").join("bin").join(OPENCODE_BINARY));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        out.push(
            Path::new(&appdata)
                .join("npm")
                .join("node_modules")
                .join(OPENCODE_PKG)
                .join("bin")
                .join(OPENCODE_BINARY),
        );
    }
    out
}

fn valid_metadata(metadata: &Value) -> bool {
    let dist = match metadata.get("dist") {
        Some(d) => d,
        None => return false,
    };
    let tarball = match dist.get("tarball").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    let version = metadata.get("version").and_then(|v| v.as_str()).unwrap_or("");
    let ok_version = version.split('.').count() == 3 && !version.is_empty();
    let integrity = dist.get("integrity").and_then(|v| v.as_str()).unwrap_or("");
    let url = reqwest::Url::parse(tarball).ok();
    let ok_url = url
        .map(|u| {
            u.path().starts_with(&format!("/{}/-/", OPENCODE_PKG))
                && REGISTRIES.iter().any(|r| u.origin().ascii_serialization() == *r)
        })
        .unwrap_or(false);
    metadata.get("name").and_then(|n| n.as_str()) == Some(OPENCODE_PKG)
        && ok_version
        && integrity.starts_with("sha512-")
        && ok_url
}

/// Acquire a runtime binary: reuse a valid local copy, else download the
/// official npm tarball with sha512 verification (mirror fallback allowed,
/// but the bytes must match the official checksum).
pub async fn find_runtime(data_dir: &Path, status: &(dyn Fn(&str) + Sync)) -> Result<PathBuf, String> {
    let client = http();
    // 1. local copies (newest version wins)
    let mut local: Vec<(String, PathBuf)> = Vec::new();
    for file in local_candidates(data_dir) {
        if let Some(v) = version_of(&file) {
            local.push((v, file));
        }
    }
    local.sort_by(|a, b| b.0.cmp(&a.0));
    let best = local.first().cloned();

    // 2. official latest
    let mut metadata = None;
    let mut failures = Vec::new();
    for registry in REGISTRIES {
        status(&format!("正在从 {} 获取 OpenCode 版本信息", registry));
        match client
            .get(format!("{}/{}/latest", registry, OPENCODE_PKG))
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Value>().await {
                    Ok(m) if valid_metadata(&m) => {
                        metadata = Some(m);
                        break;
                    }
                    Ok(_) => failures.push(format!("{}: 安装信息不可信", registry)),
                    Err(e) => failures.push(format!("{}: {}", registry, e)),
                }
            }
            Ok(resp) => failures.push(format!("{}: HTTP {}", registry, resp.status())),
            Err(e) => failures.push(format!("{}: {}", registry, e)),
        }
    }

    let metadata = match metadata {
        Some(m) => m,
        None => {
            // No official metadata: a good local copy is still enough.
            if let Some((v, file)) = best {
                return Ok(file);
            }
            return Err(format!(
                "无法获取 OpenCode 版本信息，官方源和国内镜像均失败（{}）",
                failures.join("；")
            ));
        }
    };
    let latest = metadata.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if let Some((v, _)) = best.as_ref() {
        if v >= &latest {
            if let Some((_, file)) = best {
                return Ok(file);
            }
        }
    }

    // 3. download + verify + extract
    let target = data_dir.join("runtime").join(&latest).join(OPENCODE_BINARY);
    if version_of(&target).as_deref() == Some(latest.as_str()) {
        return Ok(target);
    }
    status(&format!("正在下载 OpenCode {}（首次启动需要一两分钟）", latest));
    let dist = metadata.get("dist").unwrap();
    let official = dist.get("tarball").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let integrity = dist.get("integrity").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mirrors: Vec<String> = REGISTRIES
        .iter()
        .map(|r| format!("{}/{}/-/{}-{}.tgz", r, OPENCODE_PKG, OPENCODE_PKG, latest))
        .collect();
    let mut tarballs = vec![official];
    tarballs.extend(mirrors);

    let mut downloaded: Option<Vec<u8>> = None;
    let mut checksum_failed = false;
    for url in tarballs {
        match client.get(&url).timeout(Duration::from_secs(300)).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => {
                    use sha2::{Digest, Sha512};
                    let digest = Sha512::digest(&bytes);
                    let computed = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(digest));
                    if computed != integrity {
                        checksum_failed = true;
                        continue;
                    }
                    downloaded = Some(bytes.to_vec());
                    break;
                },
                Err(e) => failures.push(format!("{}: {}", url, e)),
            },
            Ok(resp) => failures.push(format!("{}: HTTP {}", url, resp.status())),
            Err(e) => failures.push(format!("{}: {}", url, e)),
        }
    }
    let bytes = match downloaded {
        Some(b) => b,
        None => {
            // The npm tarball's `bin/opencode.exe` is only a JS stub on
            // Windows — the real binary arrives via the postinstall script,
            // which we never run. Whenever a fresh copy cannot be obtained or
            // verified, fall back to the best LOCAL copy instead of failing:
            // a stale-but-real runtime beats no runtime.
            if let Some((v, file)) = best {
                eprintln!("opencode: 下载/校验失败，回退本机已有运行时 {}", v);
                return Ok(file);
            }
            if checksum_failed {
                return Err("OpenCode 下载完整性校验失败".into());
            }
            return Err(format!("OpenCode 下载失败（{}）", failures.join("；")));
        }
    };

    // extract only package/bin/<binary>
    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(decoder);
    let member = format!("package/bin/{}", OPENCODE_BINARY);
    let staging = data_dir.join("runtime").join("extract-tmp");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut extracted = None;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        if path.to_string_lossy() == member && entry.header().entry_type().is_file() {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            extracted = Some(buf);
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    let binary = extracted.ok_or("OpenCode 包内未找到可执行文件")?;
    // On Windows the tarball ships a JS stub under bin/opencode.exe (the real
    // binary is fetched by a postinstall we deliberately do not run). Never
    // write a non-PE stub to disk — fall back straight away.
    if !binary.starts_with(b"MZ") {
        if let Some((v, file)) = best {
            eprintln!("opencode: 下载包不含真实二进制（stub），回退本机已有运行时 {}", v);
            return Ok(file);
        }
        return Err("下载的 OpenCode 包不含可执行二进制（仅 JS stub）".into());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&target, &binary).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
    }
    if version_of(&target).as_deref() != Some(latest.as_str()) {
        // The npm `bin/opencode.exe` is a JS stub on Windows (the real
        // binary is fetched by a postinstall script we deliberately do not
        // run). A failed version check almost always means "stub": drop the
        // poisoned copy and fall back to the best local runtime rather than
        // leaving the upstream dead.
        let _ = std::fs::remove_file(&target);
        if let Some((v, file)) = best {
            eprintln!("opencode: 下载包不含真实二进制（stub），回退本机已有运行时 {}", v);
            return Ok(file);
        }
        return Err("下载的 OpenCode 版本校验失败".into());
    }
    Ok(target)
}

/// Start the isolated server: fresh XDG dirs, random password, config that
/// disables autoupdate / share / project config / external skills, and the
/// catalog refreshed once before the resident server starts.
pub async fn start(data_dir: &Path, status: &(dyn Fn(&str) + Sync)) -> Result<OpenCodeRuntime, String> {
    let binary = find_runtime(data_dir, status).await?;
    let actual_version = version_of(&binary).ok_or("无法读取 OpenCode 版本")?;
    let root = data_dir.join("opencode");
    for sub in ["config", "data", "cache", "state", "project"] {
        std::fs::create_dir_all(root.join(sub)).map_err(|e| e.to_string())?;
    }
    let mut env: Vec<(String, String)> = Vec::new();
    for key in [
        "PATH", "HOME", "USER", "LANG", "TMPDIR", "SHELL", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS",
        "SystemRoot", "WINDIR", "TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "PATHEXT", "COMSPEC",
    ] {
        if let Ok(v) = std::env::var(key) {
            env.push((key.into(), v));
        }
    }
    for sub in ["config", "data", "cache", "state"] {
        env.push((format!("XDG_{}_HOME", sub.to_uppercase()), root.join(sub).to_string_lossy().to_string()));
    }
    let password = crate::store::new_hex(24);
    env.push(("OPENCODE_SERVER_PASSWORD".into(), password.clone()));
    env.push(("OPENCODE_SERVER_USERNAME".into(), "opencode".into()));
    env.push(("OPENCODE_DISABLE_AUTOUPDATE".into(), "true".into()));
    env.push(("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "true".into()));
    env.push(("OPENCODE_DISABLE_CLAUDE_CODE".into(), "true".into()));
    env.push(("OPENCODE_DISABLE_EXTERNAL_SKILLS".into(), "true".into()));
    env.push((
        "OPENCODE_CONFIG_CONTENT".into(),
        json!({
            "autoupdate": false,
            "share": "disabled",
            "agent": {
                "credit-ferry": {
                    "mode": "primary",
                    "description": "External client inference only (envelope protocol)",
                    "prompt": ENVELOPE_PROMPT,
                    "permission": { "*": "ask" }
                }
            }
        })
        .to_string(),
    ));

    // Refresh the catalog once before the resident server starts.
    status("正在刷新 OpenCode 模型目录…");
    let _ = quiet(Command::new(&binary)
        .args(["models", OPENCODE_PROVIDER, "--refresh", "--pure"])
        .current_dir(root.join("project"))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::null())
        .stderr(Stdio::null()))
    .output();

    // Free loopback port.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.local_addr().map_err(|e| e.to_string())?.port()
    };
    let child = quiet(Command::new(&binary)
        .args([
            "serve",
            "--pure",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(root.join("project"))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::null())
        .stderr(Stdio::null()))
    .spawn()
    .map_err(|e| format!("启动 OpenCode 失败：{}", e))?;

    let runtime = OpenCodeRuntime {
        child,
        base: format!("http://127.0.0.1:{}", port),
        password,
        version: actual_version.clone(),
    };
    let client = http();
    // Ready means healthy AND the self-reported version matches the binary.
    for _ in 0..120 {
        match client
            .get(format!("{}/global/health", runtime.base))
            .header("authorization", auth(&runtime.password))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let health: Value = resp.json().await.unwrap_or(json!({}));
                if health.get("healthy") == Some(&Value::Bool(true)) {
                    let seen = health.get("version").and_then(|v| v.as_str()).unwrap_or("");
                    if seen.is_empty() || seen == actual_version {
                        return Ok(runtime);
                    }
                }
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut runtime = runtime;
    runtime.stop();
    Err("OpenCode 启动超时".into())
}

// ---------------------------------------------------------------- client

pub struct OpenCodeClient {
    http: Client,
}

impl OpenCodeClient {
    pub fn new() -> Self {
        Self { http: http() }
    }

    fn headers(&self, password: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, "application/json".parse().unwrap());
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(AUTHORIZATION, auth(password).parse().unwrap());
        headers
    }

    /// Free models from `/provider` — price, not the name, decides.
    pub async fn models(&self, base: &str, password: &str) -> Result<Vec<ModelEntry>, String> {
        let resp = self
            .http
            .get(format!("{}/provider", base))
            .headers(self.headers(password))
            .timeout(SESSION_TTL)
            .send()
            .await
            .map_err(|e| format!("opencode /provider: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("opencode /provider HTTP {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let provider = body
            .get("all")
            .and_then(|a| a.as_array())
            .and_then(|list| list.iter().find(|p| p.get("id").and_then(|v| v.as_str()) == Some(OPENCODE_PROVIDER)))
            .ok_or("OpenCode provider missing")?;
        let models = provider.get("models").and_then(|m| m.as_object()).ok_or("catalog empty")?;
        let mut out = Vec::new();
        for (id, m) in models {
            let cost = m.get("cost");
            let free = cost
                .map(|c| {
                    num(c.get("input")) == Some(0.0)
                        && num(c.get("output")) == Some(0.0)
                        && num(c.get("cache").and_then(|x| x.get("read"))).unwrap_or(0.0) == 0.0
                        && num(c.get("cache").and_then(|x| x.get("write"))).unwrap_or(0.0) == 0.0
                })
                .unwrap_or(false);
            if !free {
                continue;
            }
            if m.get("status").and_then(|v| v.as_str()) == Some("deprecated") {
                continue;
            }
            if m.get("capabilities")
                .and_then(|c| c.get("output"))
                .and_then(|o| o.get("text"))
                == Some(&Value::Bool(false))
            {
                continue;
            }
            out.push(ModelEntry {
                upstream_id: id.clone(),
                name: m.get("name").and_then(|v| v.as_str()).unwrap_or(id).to_string(),
                source: "opencode".into(),
                vision: m
                    .get("capabilities")
                    .and_then(|c| c.get("input"))
                    .and_then(|i| i.get("image"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                context: m.get("limit").and_then(|l| l.get("context")).and_then(|v| v.as_u64()).unwrap_or(0),
                max_output: m.get("limit").and_then(|l| l.get("output")).and_then(|v| v.as_u64()).unwrap_or(0),
                multiplier: None,
                billing_status: None,
        channel: None,
            });
        }
        out.sort_by(|a, b| a.upstream_id.cmp(&b.upstream_id));
        Ok(out)
    }

    /// One OpenAI-shaped chat/completions call, translated to the envelope.
    pub async fn chat(&self, base: &str, password: &str, body: &Value) -> Result<Value, String> {
        let messages = body.get("messages").and_then(|m| m.as_array()).ok_or("messages must be an array")?;
        let tools: Vec<Value> = body
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("function")).cloned().collect())
            .unwrap_or_default();
        let model_id = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

        // Flatten the conversation into one text part (role-labelled).
        let mut text = String::new();
        for m in messages {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| {
                        if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                            p.get("text").and_then(|v| v.as_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if !content.trim().is_empty() {
                text.push_str(&format!("[{}]\n{}\n\n", role, content));
            }
        }
        if !tools.is_empty() {
            text.push_str("[available tools]\n");
            text.push_str(&serde_json::to_string_pretty(&tools).unwrap_or_default());
            text.push_str("\n\n");
        }

        // Session with the fence: every native action needs approval, and the
        // watcher below auto-rejects all of them.
        let session: Value = self
            .http
            .post(format!("{}/session", base))
            .headers(self.headers(password))
            .json(&json!({
                "title": "CreditFerry",
                "permission": [{ "permission": "*", "pattern": "*", "action": "ask" }]
            }))
            .timeout(SESSION_TTL)
            .send()
            .await
            .map_err(|e| format!("opencode /session: {}", e))?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let session_id = session.get("id").and_then(|v| v.as_str()).ok_or("session id missing")?.to_string();

        // The watcher: nothing native ever executes here.
        let watcher = {
            let http = self.http.clone();
            let base = base.to_string();
            let password = password.to_string();
            let sid = session_id.clone();
            tokio::spawn(async move {
                let headers = {
                    use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
                    let mut h = HeaderMap::new();
                    h.insert(ACCEPT, "application/json".parse().unwrap());
                    h.insert(CONTENT_TYPE, "application/json".parse().unwrap());
                    h.insert(AUTHORIZATION, auth(&password).parse().unwrap());
                    h
                };
                loop {
                    if let Ok(resp) = http
                        .get(format!("{}/permission", base))
                        .headers(headers.clone())
                        .timeout(Duration::from_secs(5))
                        .send()
                        .await
                    {
                        if let Ok(list) = resp.json::<Value>().await {
                            if let Some(items) = list.as_array() {
                                for p in items {
                                    if p.get("sessionID").and_then(|v| v.as_str()) == Some(sid.as_str()) {
                                        if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
                                            let _ = http
                                                .post(format!("{}/permission/{}/reply", base, id))
                                                .headers(headers.clone())
                                                .json(&json!({ "reply": "reject", "message": "This native action is executed by the external client instead." }))
                                                .timeout(Duration::from_secs(5))
                                                .send()
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
        };

        let calls_schema = calls_schema(&tools, body.get("parallel_tool_calls") == Some(&Value::Bool(true)));
        let payload = json!({
            "model": { "providerID": OPENCODE_PROVIDER, "modelID": model_id },
            "agent": "credit-ferry",
            "system": ENVELOPE_PROMPT,
            "parts": [{ "type": "text", "text": text }],
            "format": {
                "type": "json_schema",
                "retryCount": 0,
                "schema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "calls": calls_schema
                    },
                    "required": ["content", "calls"],
                    "additionalProperties": false
                }
            }
        });

        let result = self
            .http
            .post(format!("{}/session/{}/message", base, session_id))
            .headers(self.headers(password))
            .json(&payload)
            .timeout(MESSAGE_TTL)
            .send()
            .await;
        watcher.abort();
        let _ = self
            .http
            .delete(format!("{}/session/{}", base, session_id))
            .headers(self.headers(password))
            .timeout(Duration::from_secs(5))
            .send()
            .await;

        let response: Value = match result {
            Ok(resp) if resp.status().is_success() => resp.json().await.map_err(|e| e.to_string())?,
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("opencode message HTTP {}: {}", status.as_u16(), text.chars().take(300).collect::<String>()));
            }
            Err(e) => return Err(format!("opencode message: {}", e)),
        };

        // The envelope arrives as info.structured, a completed StructuredOutput
        // part, or plain text — accept all three, in that order.
        let structured = response
            .get("info")
            .and_then(|i| i.get("structured"))
            .cloned()
            .or_else(|| {
                response
                    .get("parts")
                    .and_then(|p| p.as_array())
                    .and_then(|parts| {
                        parts
                            .iter()
                            .find(|p| {
                                p.get("type").and_then(|v| v.as_str()) == Some("tool")
                                    && p.get("tool").and_then(|v| v.as_str()) == Some("StructuredOutput")
                                    && p.get("state").and_then(|s| s.get("status")).and_then(|v| v.as_str()) == Some("completed")
                            })
                            .and_then(|p| p.get("state").and_then(|s| s.get("input")).cloned())
                    })
            });
        let text_answer = structured.is_none().then(|| {
            response
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default()
        });

        let envelope: Value = match structured {
            Some(s) => s,
            None => {
                let raw = text_answer.unwrap_or_default();
                if raw.trim().is_empty() {
                    return Err("模型没有返回信封：structured、StructuredOutput、文本三者都为空".into());
                }
                match serde_json::from_str::<Value>(&raw) {
                    Ok(v) => v,
                    Err(_) => json!({ "content": raw, "calls": [] }),
                }
            }
        };

        let content = envelope.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let calls = envelope.get("calls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let tool_calls: Vec<Value> = calls
            .iter()
            .filter_map(|c| {
                let name = c.get("name").and_then(|v| v.as_str())?;
                let args = c.get("arguments").cloned().unwrap_or(json!({}));
                Some(json!({
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }))
            })
            .collect();

        let tokens = response.get("info").and_then(|i| i.get("tokens")).cloned().unwrap_or(json!({}));
        let usage = json!({
            "prompt_tokens": num(tokens.get("input")).map(|n| n as u64),
            "completion_tokens": num(tokens.get("output")).map(|n| n as u64),
            "total_tokens": num(tokens.get("input")).unwrap_or(0.0) as u64 + num(tokens.get("output")).unwrap_or(0.0) as u64,
            "prompt_tokens_details": { "cached_tokens": num(tokens.get("cache").and_then(|c| c.get("read"))).map(|n| n as u64).unwrap_or(0) },
            "completion_tokens_details": { "reasoning_tokens": num(tokens.get("reasoning")).map(|n| n as u64).unwrap_or(0) }
        });

        let finish = if tool_calls.is_empty() { "stop" } else { "tool_calls" };
        Ok(json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion",
            "created": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "model": body.get("model").cloned().unwrap_or(json!(model_id)),
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": if content.is_empty() { Value::Null } else { Value::String(content) },
                    "tool_calls": tool_calls
                },
                "finish_reason": finish
            }],
            "usage": usage
        }))
    }
}

/// Schema for the calls array, welded to this request's tool table.
fn calls_schema(tools: &[Value], parallel: bool) -> Value {
    let any_of: Vec<Value> = tools
        .iter()
        .filter_map(|t| {
            let function = t.get("function")?;
            let name = function.get("name").and_then(|v| v.as_str())?;
            Some(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "const": name },
                    "arguments": function.get("parameters").cloned().unwrap_or(json!({ "type": "object" }))
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }))
        })
        .collect();
    let mut schema = json!({ "type": "array" });
    if let Some(obj) = schema.as_object_mut() {
        if !parallel {
            obj.insert("maxItems".into(), json!(1));
        }
        if any_of.is_empty() {
            obj.insert("maxItems".into(), json!(0));
            obj.insert("items".into(), json!({ "type": "object" }));
        } else {
            obj.insert("items".into(), json!({ "anyOf": any_of }));
        }
    }
    schema
}

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| v.as_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The npm Windows stub is a JS text file; executing it makes Windows pop
    /// the "不支持的 16 位应用程序" GUI error. looks_executable must reject it
    /// without ever spawning a process.
    #[test]
    fn js_stub_is_not_executable_but_pe_headers_are() {
        let dir = std::env::temp_dir().join(format!("cf-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("opencode.exe");
        std::fs::write(&stub, b"#!/usr/bin/env node\nrequire('./cli.js')\n").unwrap();
        assert!(!looks_executable(&stub));
        let pe = dir.join("real.exe");
        std::fs::write(&pe, b"MZ\x90\x00fake").unwrap();
        assert!(looks_executable(&pe));
        assert!(!looks_executable(&dir.join("missing.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
