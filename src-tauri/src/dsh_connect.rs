//! DSH connector platforms: the two community plugins that bring WorkBuddy and
//! Qoder models *into* DeepSeek Harness, managed from here.
//!
//! A plugin is active in a profile when its package is a dependency of
//! `<home>/profiles/<name>/package.json` **and** its name sits in that file's
//! `dsh.profile.bundles` list — the host then applies the plugin's own
//! `cordis.patch.yml` (loader insert `llm-workbuddy` / `llm-qoder`). So we only
//! ever touch those two places plus the dependency install itself; the loader
//! tree and every credential stay the plugin's business.
//!
//! Plugin state is read-only here, and never a secret: the WorkBuddy connector
//! keeps its files under `$DSH_HOME` (`~/.dsh`), the Qoder connector per profile
//! under `.dsh-qoder-connect/`. We report presence, counts and timestamps only.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::store::{now_ms, Store};

pub struct Platform {
    pub key: &'static str,
    pub package: &'static str,
    pub repo: &'static str,
    pub title: &'static str,
    pub doc: &'static str,
}

pub const PLATFORMS: [Platform; 2] = [
    Platform {
        key: "workbuddy",
        package: "dsh-workbuddy-connect",
        repo: "corrinehu/dsh-workbuddy-connect",
        title: "WorkBuddy 连接器",
        doc: "复用 WorkBuddy 桌面 App 的登录，把它的模型接入 DSH（国内版/国际版各一组，积分跟随 App）",
    },
    Platform {
        key: "qoder",
        package: "dsh-qoder-connect",
        repo: "masknull/dsh-qoder-connect",
        title: "Qoder 连接器",
        doc: "用 Qoder 个人访问令牌（PAT）把 Qoder CN / 国际版模型接入 DSH，含每日签到与侧栏额度卡",
    },
];

pub fn platform(key: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|p| p.key == key)
}

// ------------------------------------------------------------ version rules

/// `0.2.0-rc.2` → `(0, 2, 0, "rc.2")`. A pre-release ranks below its release.
fn parse_ver(text: &str) -> Option<(u32, u32, u32, String)> {
    let t = text.trim().trim_start_matches('v');
    if t.is_empty() {
        return None;
    }
    let (core, pre) = match t.split_once('-') {
        Some((a, b)) => (a, b.to_string()),
        None => (t, String::new()),
    };
    let mut nums = core.split('.').map(|p| p.parse::<u32>().ok());
    let major = nums.next()??;
    let minor = nums.next().flatten().unwrap_or(0);
    let patch = nums.next().flatten().unwrap_or(0);
    Some((major, minor, patch, pre))
}

fn cmp_ver(a: &str, b: &str) -> Option<Ordering> {
    let (am, an, ap, apre) = parse_ver(a)?;
    let (bm, bn, bp, bpre) = parse_ver(b)?;
    let base = am.cmp(&bm).then(an.cmp(&bn)).then(ap.cmp(&bp));
    Some(match base {
        Ordering::Equal => match (apre.is_empty(), bpre.is_empty()) {
            (true, true) => Ordering::Equal,
            // `0.2.0` ships after `0.2.0-rc.2`.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => apre.cmp(&bpre),
        },
        other => other,
    })
}

fn at_least(kernel: &str, min: &str) -> bool {
    cmp_ver(kernel, min).map(|o| o != Ordering::Less).unwrap_or(false)
}

/// Kernel → plugin version, from each plugin's published compatibility table.
/// An unknown kernel is treated as the current line (callers show a warning).
pub fn recommended(key: &str, kernel: Option<&str>) -> Option<&'static str> {
    let k = kernel.unwrap_or("0.2.0");
    match key {
        "workbuddy" => {
            if at_least(k, "0.2.0-rc.2") {
                Some("0.7.5")
            } else if at_least(k, "0.2.0-rc.1") {
                Some("0.7.0")
            } else if at_least(k, "0.1.5") {
                Some("0.6.5")
            } else if at_least(k, "0.1.2-rc.1") {
                Some("0.3.1")
            } else if at_least(k, "0.1.1-rc.2") {
                Some("0.2.6")
            } else {
                None
            }
        }
        "qoder" => {
            if at_least(k, "0.1.7-rc.1") {
                Some("0.2.3")
            } else if at_least(k, "0.1.5-rc.2") {
                Some("0.2.0")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// What the panel should offer for one (platform, profile) pair.
pub fn plan(installed: Option<&str>, enabled: bool, want: Option<&str>) -> (&'static str, &'static str) {
    let Some(want) = want else {
        return ("unsupported", "本机 DSH 内核没有可用的插件版本");
    };
    match installed {
        None => ("install", "安装并启用"),
        Some(have) if have.trim_start_matches(['^', '~', 'v']) != want => {
            ("upgrade", "升级到与内核匹配的版本")
        }
        Some(_) if !enabled => ("enable", "启用（已安装但未加入 bundle）"),
        Some(_) => ("none", "已安装并启用"),
    }
}

// -------------------------------------------------------------- dsh layout

/// `~/.dsh`, honouring the profile dir the user pointed the export at.
pub fn dsh_home(store: &Store) -> Option<PathBuf> {
    if let Some(saved) = &store.settings.dsh_file {
        if !saved.is_empty() {
            let dir = PathBuf::from(saved);
            if let Some(home) = dir.parent().and_then(|p| p.parent()) {
                return Some(home.to_path_buf());
            }
        }
    }
    if let Ok(v) = std::env::var("DSH_HOME") {
        if !v.is_empty() && Path::new(&v).is_dir() {
            return Some(PathBuf::from(v));
        }
    }
    let user = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let cand = Path::new(&user).join(".dsh");
    cand.is_dir().then_some(cand)
}

/// `desktopVersion` of the runtime the desktop App materialised.
fn runtime_kernel(home: &Path) -> Option<String> {
    let root = home.join("dsh-runtimes");
    for entry in fs::read_dir(&root).ok()?.flatten() {
        let file = entry.path().join("runtime.json");
        if let Some(v) = read_json(&file) {
            if let Some(s) = v.get("desktopVersion").and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Kernel line resolved by pnpm for a profile. Importers list ranges
/// (`'>=0.1.7-rc.1'`); only the concrete resolution reads as a version.
fn lock_kernel(lock: &Path) -> Option<String> {
    let text = fs::read_to_string(lock).ok()?;
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("'@deepseek-ai/dsh-llm':") else {
            continue;
        };
        let v = rest.trim().trim_matches('\'').trim();
        if v.is_empty() || v.contains(['^', '>', '<', '|', '*']) {
            continue;
        }
        return Some(v.to_string());
    }
    None
}

fn read_json(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

fn dep_version(dir: &Path, pkg: &str) -> Option<String> {
    let doc = read_json(&dir.join("package.json"))?;
    ["dependencies", "devDependencies"]
        .iter()
        .find_map(|sec| doc.get(*sec)?.get(pkg)?.as_str())
        .map(|s| s.to_string())
}

pub fn bundles(dir: &Path) -> Vec<String> {
    read_json(&dir.join("package.json"))
        .and_then(|d| {
            d.pointer("/dsh/profile/bundles")
                .and_then(|b| b.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        })
        .unwrap_or_default()
}

/// Add or drop one bundle name in the profile's package.json.
pub fn set_bundle(dir: &Path, pkg: &str, on: bool) -> Result<bool, String> {
    let file = dir.join("package.json");
    let old = fs::read_to_string(&file).map_err(|e| format!("读取 {} 失败：{}", file.display(), e))?;
    let mut doc: Value = serde_json::from_str(&old).map_err(|e| format!("package.json 不是合法 JSON：{}", e))?;
    if doc.get("dsh").is_none() {
        doc["dsh"] = json!({});
    }
    if doc["dsh"].get("profile").is_none() {
        doc["dsh"]["profile"] = json!({});
    }
    if doc["dsh"]["profile"].get("bundles").is_none() {
        doc["dsh"]["profile"]["bundles"] = json!([]);
    }
    let list = doc["dsh"]["profile"]["bundles"]
        .as_array_mut()
        .ok_or_else(|| "dsh.profile.bundles 不是数组".to_string())?;
    let names: Vec<String> = list.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    let mut changed = false;
    if on {
        if !names.iter().any(|n| n == pkg) {
            list.push(Value::String(pkg.to_string()));
            changed = true;
        }
    } else if let Some(at) = names.iter().position(|n| n == pkg) {
        list.remove(at);
        changed = true;
    }
    if !changed {
        return Ok(false);
    }
    write_with_backup(&file, &old, &serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)
}

fn write_with_backup(file: &Path, old: &str, new: &str) -> Result<bool, String> {
    let mut text = new.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if !old.trim().is_empty() {
        let backup = format!("{}.credit-ferry-{}.bak", file.display(), now_ms());
        fs::write(&backup, old).map_err(|e| e.to_string())?;
        crate::export::prune_backups(file, 5);
    }
    let tmp = format!("{}.cf-{}.tmp", file.display(), std::process::id());
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, file).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(true)
}

// ------------------------------------------------------------ pnpm access

pub enum Pnpm {
    /// The Node + pnpm the DSH desktop App ships and uses for its own installs.
    Bundled { node: PathBuf, script: PathBuf },
    /// pnpm from PATH.
    External(PathBuf),
    None,
}

pub fn pnpm_tool(home: &Path) -> Pnpm {
    if let Ok(read) = fs::read_dir(home.join("dsh-runtimes")) {
        for entry in read.flatten() {
            let deps = entry.path().join("dependencies");
            let node = deps.join("node").join("bin").join(if cfg!(windows) { "node.exe" } else { "node" });
            let script = deps.join("pnpm").join("bin").join("pnpm.cjs");
            if node.is_file() && script.is_file() {
                return Pnpm::Bundled { node, script };
            }
        }
    }
    let names: &[&str] = if cfg!(windows) { &["pnpm.cmd", "pnpm"] } else { &["pnpm"] };
    for name in names {
        if let Ok(path) = which(name) {
            return Pnpm::External(path);
        }
    }
    Pnpm::None
}

fn which(name: &str) -> Result<PathBuf, ()> {
    let joined = std::env::var("PATH").map_err(|_| ())?;
    for dir in joined.split(PATH_SEP).filter(|d| !d.is_empty()) {
        let cand = Path::new(dir).join(name);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(())
}

#[cfg(windows)]
const PATH_SEP: char = ';';
#[cfg(not(windows))]
const PATH_SEP: char = ':';

impl Pnpm {
    fn describe(&self) -> Value {
        match self {
            Pnpm::Bundled { node, script } => json!({
                "kind": "bundled",
                "path": script.display().to_string(),
                "node": node.display().to_string(),
            }),
            Pnpm::External(p) => json!({ "kind": "external", "path": p.display().to_string() }),
            Pnpm::None => json!({ "kind": "none", "path": "" }),
        }
    }

    fn args(&self, profile: &Path, sub: &str, spec: &str) -> std::process::Command {
        let mut cmd = match self {
            Pnpm::Bundled { node, script } => {
                let mut c = std::process::Command::new(node);
                c.arg(script);
                c
            }
            Pnpm::External(p) => std::process::Command::new(p),
            Pnpm::None => std::process::Command::new(""),
        };
        cmd.current_dir(profile).arg(sub).arg(spec);
        crate::opencode::quiet(&mut cmd);
        cmd
    }

    fn run(&self, profile: &Path, sub: &str, spec: &str) -> Result<String, String> {
        if matches!(self, Pnpm::None) {
            return Err("本机没有可用的 pnpm（DSH 自带的运行时里也没找到）".to_string());
        }
        let out = self
            .args(profile, sub, spec)
            .output()
            .map_err(|e| format!("启动 pnpm 失败：{}", e))?;
        let mut log = String::from_utf8_lossy(&out.stdout).to_string();
        log.push_str(&String::from_utf8_lossy(&out.stderr));
        let tail: String = log.chars().rev().take(1200).collect::<Vec<_>>().into_iter().rev().collect();
        if out.status.success() {
            return Ok(tail);
        }
        Err(format!("pnpm {} 失败：{}", sub, tail.trim()))
    }
}

// ----------------------------------------------------------------- status

pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
}

pub fn list_profiles(home: &Path) -> Vec<Profile> {
    let mut out = Vec::new();
    if let Ok(read) = fs::read_dir(home.join("profiles")) {
        for entry in read.flatten() {
            let dir = entry.path();
            if dir.join("package.json").is_file() {
                out.push(Profile {
                    name: entry.file_name().to_string_lossy().to_string(),
                    dir,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Newest catalog entry: where the list came from, how many models, when.
fn catalog_summary(file: &Path) -> Value {
    let Some(doc) = read_json(file) else {
        return json!({ "present": false });
    };
    let entries = doc.get("entries").and_then(|e| e.as_object());
    let best = entries
        .map(|map| {
            map.values()
                .filter_map(|v| Some((v.get("fetchedAtMs")?.as_i64()?, v)))
                .max_by_key(|(ts, _)| *ts)
        })
        .flatten();
    match best {
        Some((ts, e)) => json!({
            "present": true,
            "source": e.get("source").and_then(|s| s.as_str()).unwrap_or(""),
            "models": e.get("models").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0),
            "fetchedAtMs": ts,
        }),
        None => json!({ "present": true, "source": "", "models": 0, "fetchedAtMs": 0 }),
    }
}

fn json_len(file: &Path, pointer: &str) -> usize {
    read_json(file)
        .and_then(|v| {
            v.pointer(pointer).and_then(|n| match n {
                Value::Array(a) => Some(a.len()),
                Value::Object(o) => Some(o.len()),
                _ => None,
            })
        })
        .unwrap_or(0)
}

/// The WorkBuddy connector keeps its copies under `$DSH_HOME` (shared by every
/// profile); CN and International differ only by the file-name infix.
fn workbuddy_state(home: &Path) -> Value {
    let variant = |stem: &str| -> Value {
        let desktop = workbuddy_desktop_auth(stem);
        json!({
            "ownAuth": home.join(format!("{}-auth.json", stem)).is_file(),
            "desktopAuth": desktop.is_some(),
            "desktopAuthPath": desktop.map(|p| p.display().to_string()).unwrap_or_default(),
            "catalog": catalog_summary(&home.join(format!("{}-catalog.json", stem))),
            "probe": json_len(&home.join(format!("{}-probe.json", stem)), "/records"),
            "hiddenAccounts": json_len(&home.join(format!("{}-model-visibility.json", stem)), "/accounts"),
        })
    };
    json!({ "scope": "home", "cn": variant(".workbuddy"), "intl": variant(".workbuddy-ai") })
}

/// Where the WorkBuddy desktop App stores its own sign-in (never read here —
/// only probed for existence, since the connector follows it automatically).
fn workbuddy_desktop_auth(stem: &str) -> Option<PathBuf> {
    let user = std::env::var("USERPROFILE").ok()?;
    let file = if stem == ".workbuddy-ai" { "workbuddy-desktop-ai.info" } else { "workbuddy-desktop.info" };
    for base in ["LocalAppData", "APPDATA"] {
        let root = std::env::var(base).unwrap_or_else(|_| Path::new(&user).join("AppData").display().to_string());
        let cand = Path::new(&root)
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")
            .join(file);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// The Qoder connector stores per profile: `<profile>/.dsh-qoder-connect/`.
fn qoder_state(profile: &Path) -> Value {
    let dir = profile.join(".dsh-qoder-connect");
    let variant = |file: &str, cat: &str| -> Value {
        // `pat` is the secret: reported as presence only, never echoed.
        let auth = read_json(&dir.join(file)).unwrap_or(Value::Null);
        json!({
            "patSaved": auth.get("pat").and_then(|p| p.as_str()).map(|s| !s.is_empty()).unwrap_or(false),
            "region": auth.get("region").and_then(|r| r.as_str()).unwrap_or(""),
            "savedAtMs": auth.get("savedAt").and_then(|s| s.as_i64()).unwrap_or(0),
            "catalog": catalog_summary(&dir.join("state").join(cat)),
        })
    };
    let checkin = read_json(&dir.join("checkin-status.json")).unwrap_or(Value::Null);
    let one = |key: &str| -> Value {
        let row = checkin.get(key);
        json!({
            "lastDate": row.and_then(|r| r.get("lastDate")).and_then(|v| v.as_str()).unwrap_or(""),
            "status": row.and_then(|r| r.get("status")).and_then(|v| v.as_str()).unwrap_or(""),
            "amount": row.and_then(|r| r.get("amount")).and_then(|v| v.as_f64()).unwrap_or(0.0),
            "logs": row.and_then(|r| r.get("logs")).and_then(|l| l.as_array()).map(|a| a.len()).unwrap_or(0),
        })
    };
    let settings = read_json(&dir.join("settings.json"));
    let flag = |name: &str| settings.as_ref().and_then(|s| s.get(name)).and_then(|v| v.as_bool()).unwrap_or(false);
    json!({
        "scope": "profile",
        "dataDir": dir.display().to_string(),
        "present": dir.is_dir(),
        "cn": variant(".qoder-auth.json", ".qoder-catalog.json"),
        "intl": variant(".qoder-global-auth.json", ".qoder-global-catalog.json"),
        "checkin": { "cn": one("qoder"), "intl": one("qoder-global") },
        "autoCheckIn": { "cn": flag("autoCheckInCN"), "intl": flag("autoCheckInGlobal") },
        "sidebarQuota": { "cn": flag("sidebarQuotaCN"), "intl": flag("sidebarQuotaGlobal") },
        "maxContext": {
            "cn": settings.as_ref().and_then(|s| s.get("useMaximumContextWindowCN")).and_then(|v| v.as_bool()),
            "intl": settings.as_ref().and_then(|s| s.get("useMaximumContextWindow")).and_then(|v| v.as_bool()),
        },
    })
}

fn profile_state(key: &str, profile: &Path) -> Value {
    match key {
        "qoder" => qoder_state(profile),
        // Home-scoped: exposed once at the top level instead of per profile.
        _ => Value::Null,
    }
}

pub fn status(store: &Store) -> Value {
    let Some(home) = dsh_home(store) else {
        return json!({ "found": false, "profiles": [], "workbuddyHome": null });
    };
    let app_kernel = runtime_kernel(&home);
    let tool = pnpm_tool(&home);
    let profiles = list_profiles(&home);
    let mut out = Vec::new();
    for p in &profiles {
        let kernel = if p.name == "desktop" { app_kernel.clone() } else { None }.or_else(|| lock_kernel(&p.dir.join("pnpm-lock.yaml")));
        let mut plats = Vec::new();
        for plat in &PLATFORMS {
            let installed = dep_version(&p.dir, plat.package);
            let enabled = bundles(&p.dir).iter().any(|b| b == plat.package);
            let want = recommended(plat.key, kernel.as_deref());
            let (action, note) = plan(installed.as_deref(), enabled, want);
            let state = profile_state(plat.key, &p.dir);
            plats.push(json!({
                "key": plat.key,
                "title": plat.title,
                "package": plat.package,
                "repo": plat.repo,
                "doc": plat.doc,
                "installed": installed,
                "enabled": enabled,
                "recommended": want,
                "action": action,
                "note": note,
                "state": state,
            }));
        }
        out.push(json!({
            "name": p.name,
            "dir": p.dir.display().to_string(),
            "kernel": kernel,
            "patchFile": p.dir.join("cordis.patch.yml").is_file(),
            "platforms": plats,
        }));
    }
    json!({
        "found": true,
        "home": home.display().to_string(),
        "runtimeKernel": app_kernel,
        "pnpm": tool.describe(),
        "workbuddyHome": workbuddy_state(&home),
        "profiles": out,
    })
}

// ------------------------------------------------------------------ apply

pub fn apply(store: &Store, key: &str, profile_name: &str, action: &str) -> Result<Value, String> {
    let plat = platform(key).ok_or_else(|| format!("未知连接器：{}", key))?;
    let home = dsh_home(store).ok_or_else(|| "未找到 DeepSeek Harness 目录（~/.dsh）".to_string())?;
    let profile = list_profiles(&home)
        .into_iter()
        .find(|p| p.name == profile_name)
        .ok_or_else(|| format!("未找到 DSH profile：{}", profile_name))?;
    let kernel = if profile.name == "desktop" { runtime_kernel(&home) } else { None }
        .or_else(|| lock_kernel(&profile.dir.join("pnpm-lock.yaml")));
    let want = recommended(plat.key, kernel.as_deref()).ok_or_else(|| {
        format!(
            "DSH 内核 {} 没有匹配的 {} 版本",
            kernel.clone().unwrap_or_else(|| "未知".to_string()),
            plat.package
        )
    })?;
    let tool = pnpm_tool(&home);
    let mut log = Vec::new();
    let mut changed = false;

    match action {
        "install" | "upgrade" => {
            let installed = dep_version(&profile.dir, plat.package);
            let version_ok = installed.as_deref().map(|have| plan_some(have, want)).unwrap_or(false);
            if !version_ok {
                let spec = format!("{}@{}", plat.package, want);
                let manual = format!("dsh plugin --profile {} add {}", profile.name, spec);
                let tail = tool.run(&profile.dir, "add", &spec).map_err(|e| format!("{}（也可以手动执行：{}）", e, manual))?;
                log.push(format!("pnpm add {} 完成", spec));
                log.push(tail);
                changed = true;
            }
            if dep_version(&profile.dir, plat.package).is_none() {
                return Err(format!(
                    "安装后仍没有 {} 依赖，请手动执行：{}",
                    plat.package,
                    manual_hint(&profile.name, plat.package, want)
                ));
            }
            if set_bundle(&profile.dir, plat.package, true)? {
                log.push(format!("已加入 dsh.profile.bundles：{}", plat.package));
                changed = true;
            }
        }
        "enable" => {
            if dep_version(&profile.dir, plat.package).is_none() {
                return Err(format!("尚未安装 {}，请先点「安装并启用」", plat.package));
            }
            if set_bundle(&profile.dir, plat.package, true)? {
                log.push(format!("已启用 {}", plat.package));
                changed = true;
            }
        }
        "disable" => {
            if set_bundle(&profile.dir, plat.package, false)? {
                log.push(format!("已从 bundle 摘除 {}（依赖仍在）", plat.package));
                changed = true;
            }
        }
        "uninstall" => {
            let manual = format!("dsh plugin --profile {} remove {}", profile.name, plat.package);
            let tail = tool
                .run(&profile.dir, "remove", plat.package)
                .map_err(|e| format!("{}（也可以手动执行：{}）", e, manual))?;
            log.push(format!("pnpm remove {} 完成", plat.package));
            log.push(tail);
            // A bundle name pointing at a package that is gone breaks DSH startup.
            set_bundle(&profile.dir, plat.package, false)?;
            changed = true;
        }
        other => return Err(format!("未知操作：{}", other)),
    }

    if !changed {
        log.push("没有需要改动的地方".to_string());
    }
    log.push("重启 DSH 后生效".to_string());
    Ok(json!({
        "changed": changed,
        "platform": plat.key,
        "profile": profile.name,
        "kernel": kernel,
        "version": want,
        "log": log,
    }))
}

/// `^0.7.5` / `0.7.5` both count as "this exact line is present".
fn plan_some(have: &str, want: &str) -> bool {
    have.trim_start_matches(['^', '~', 'v', ' ']) == want
}

fn manual_hint(profile: &str, pkg: &str, ver: &str) -> String {
    format!("dsh plugin --profile {} add {}@{}", profile, pkg, ver)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cf-dshc-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn version_ordering_respects_prerelease() {
        assert_eq!(cmp_ver("0.2.0-rc.1", "0.2.0-rc.2"), Some(Ordering::Less));
        assert_eq!(cmp_ver("0.1.9", "0.2.0-rc.1"), Some(Ordering::Less));
        assert!(at_least("0.2.0", "0.2.0-rc.2"));
        assert!(!at_least("0.2.0-rc.1", "0.2.0-rc.2"));
    }

    #[test]
    fn kernel_maps_to_plugin_version() {
        assert_eq!(recommended("workbuddy", Some("0.2.0-rc.2")), Some("0.7.5"));
        assert_eq!(recommended("workbuddy", Some("0.2.0-rc.1")), Some("0.7.0"));
        assert_eq!(recommended("workbuddy", Some("0.1.7")), Some("0.6.5"));
        assert_eq!(recommended("workbuddy", Some("0.1.2-rc.1")), Some("0.3.1"));
        assert_eq!(recommended("workbuddy", Some("0.1.1-rc.2")), Some("0.2.6"));
        assert_eq!(recommended("workbuddy", Some("0.1.1")), Some("0.2.6"));
        assert_eq!(recommended("workbuddy", Some("0.1.0")), None);
        assert_eq!(recommended("qoder", Some("0.1.7-rc.1")), Some("0.2.3"));
        assert_eq!(recommended("qoder", Some("0.1.5-rc.2")), Some("0.2.0"));
        assert_eq!(recommended("qoder", Some("0.1.5-rc.1")), None);
    }

    #[test]
    fn plan_covers_install_upgrade_enable_none() {
        assert_eq!(plan(None, false, Some("0.7.5")).0, "install");
        assert_eq!(plan(Some("0.7.4"), true, Some("0.7.5")).0, "upgrade");
        assert_eq!(plan(Some("^0.7.5"), false, Some("0.7.5")).0, "enable");
        assert_eq!(plan(Some("0.7.5"), true, Some("0.7.5")).0, "none");
        assert_eq!(plan(Some("0.7.5"), true, None).0, "unsupported");
    }

    #[test]
    fn lock_kernel_skips_range_lines() {
        let dir = temp_dir("lock");
        let lock = dir.join("pnpm-lock.yaml");
        fs::write(
            &lock,
            "importers:\n  .:\n    dependencies:\n      '@deepseek-ai/dsh-llm': '>=0.1.7-rc.1'\nsnapshots:\n      '@deepseek-ai/dsh-llm': 0.2.0-rc.2\n",
        )
        .unwrap();
        assert_eq!(lock_kernel(&lock).as_deref(), Some("0.2.0-rc.2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundle_toggle_is_idempotent_and_keeps_other_keys() {
        let dir = temp_dir("bundle");
        fs::write(
            &dir.join("package.json"),
            "{\n  \"name\": \"dsh-profile-desktop\",\n  \"private\": true,\n  \"dependencies\": {\n    \"dsh-qoder-connect\": \"^0.2.3\"\n  },\n  \"dsh\": {\n    \"profile\": {\n      \"bundles\": [\n        \"@deepseek-ai/dsh-base\"\n      ]\n    }\n  }\n}\n",
        )
        .unwrap();
        assert!(set_bundle(&dir, "dsh-qoder-connect", true).unwrap());
        assert!(!set_bundle(&dir, "dsh-qoder-connect", true).unwrap());
        let text = fs::read_to_string(dir.join("package.json")).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["name"], "dsh-profile-desktop");
        assert_eq!(doc["dependencies"]["dsh-qoder-connect"], "^0.2.3");
        assert_eq!(
            bundles(&dir),
            vec!["@deepseek-ai/dsh-base".to_string(), "dsh-qoder-connect".to_string()]
        );
        assert!(set_bundle(&dir, "dsh-qoder-connect", false).unwrap());
        assert_eq!(bundles(&dir), vec!["@deepseek-ai/dsh-base".to_string()]);
        assert_eq!(dep_version(&dir, "dsh-qoder-connect").as_deref(), Some("^0.2.3"));
        assert!(has_backup(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_bundle_section_is_created() {
        let dir = temp_dir("nobundle");
        fs::write(&dir.join("package.json"), "{\"name\":\"p\",\"dependencies\":{}}").unwrap();
        assert!(set_bundle(&dir, "dsh-workbuddy-connect", true).unwrap());
        assert_eq!(bundles(&dir), vec!["dsh-workbuddy-connect".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn qoder_state_reports_presence_not_pat() {
        let dir = temp_dir("qoderstate");
        let data = dir.join(".dsh-qoder-connect");
        fs::create_dir_all(data.join("state")).unwrap();
        fs::write(
            data.join(".qoder-auth.json"),
            "{\"version\":2,\"pat\":\"pt-secret-token\",\"region\":\"china\",\"savedAt\":1791999870654}",
        )
        .unwrap();
        fs::write(
            data.join("state").join(".qoder-catalog.json"),
            "{\"version\":1,\"entries\":{\"a\":{\"source\":\"live\",\"fetchedAtMs\":5,\"models\":[1,2,3]}}}",
        )
        .unwrap();
        fs::write(
            data.join("checkin-status.json"),
            "{\"qoder\":{\"lastDate\":\"2026-10-15\",\"status\":\"ok\",\"amount\":100,\"logs\":[{\"id\":1}]}}",
        )
        .unwrap();
        fs::write(data.join("settings.json"), "{\"autoCheckInCN\":true,\"sidebarQuotaCN\":false}").unwrap();
        let state = qoder_state(&dir);
        assert_eq!(state["cn"]["patSaved"], true);
        assert_eq!(state["cn"]["region"], "china");
        assert_eq!(state["cn"]["catalog"]["source"], "live");
        assert_eq!(state["cn"]["catalog"]["models"], 3);
        assert_eq!(state["checkin"]["cn"]["amount"], 100.0);
        assert_eq!(state["autoCheckIn"]["cn"], true);
        assert!(!state.to_string().contains("pt-secret-token"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_are_listed_only_with_package_json() {
        let dir = temp_dir("profiles");
        let prof = dir.join("profiles");
        fs::create_dir_all(prof.join("desktop")).unwrap();
        fs::create_dir_all(prof.join("node_modules")).unwrap();
        fs::write(prof.join("desktop").join("package.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("dsh-runtimes").join("r1").join("dependencies").join("node").join("bin")).unwrap();
        let names: Vec<String> = list_profiles(&dir).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["desktop".to_string()]);
        // A runtime with node + pnpm is picked up as the bundled tool.
        let node_dir = dir.join("dsh-runtimes").join("r1").join("dependencies");
        fs::write(node_dir.join("node").join("bin").join(if cfg!(windows) { "node.exe" } else { "node" }), "").unwrap();
        let pnpm_bin = node_dir.join("pnpm").join("bin");
        fs::create_dir_all(&pnpm_bin).unwrap();
        fs::write(pnpm_bin.join("pnpm.cjs"), "").unwrap();
        assert!(matches!(pnpm_tool(&dir), Pnpm::Bundled { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Live smoke check against the machine's own `~/.dsh` (read-only).
    #[test]
    #[ignore]
    fn real_dsh_status_is_secret_free() {
        let store = Store::open(std::env::temp_dir().join("credit-ferry-smoke"));
        let doc = status(&store);
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
        let text = doc.to_string();
        assert!(text.contains("\"profiles\""), "{}", text);
        for secret in ["\"pat\":", "accessToken", "refreshToken", "securityToken"] {
            assert!(!text.contains(secret), "状态里漏出了 {}", secret);
        }
    }

    /// Real install round-trip: bundled pnpm + bundle registration, in a
    /// throwaway profile so nobody's DSH gets touched. Needs the network.
    #[test]
    #[ignore]
    fn pnpm_install_and_uninstall_in_scratch_profile() {
        let home = dsh_home(&Store::open(std::env::temp_dir().join("credit-ferry-smoke")))
            .expect("需要本机 ~/.dsh 才能拿到 DSH 自带的 pnpm");
        let tool = pnpm_tool(&home);
        assert!(!matches!(tool, Pnpm::None), "本机没有可用 pnpm");

        let scratch = std::env::temp_dir().join(format!("cf-dshc-scratch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).unwrap();
        fs::write(
            scratch.join("package.json"),
            "{\n  \"name\": \"dsh-profile-selftest\",\n  \"private\": true,\n  \"dependencies\": {},\n  \"dsh\": { \"profile\": { \"bundles\": [\"@deepseek-ai/dsh-base\"] } }\n}\n",
        )
        .unwrap();
        fs::write(scratch.join("pnpm-workspace.yaml"), "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n").unwrap();

        let pkg = "dsh-workbuddy-connect";
        let ver = recommended("workbuddy", Some("0.2.0-rc.2")).unwrap();
        tool.run(&scratch, "add", &format!("{}@{}", pkg, ver)).expect("pnpm add 失败");
        assert_eq!(dep_version(&scratch, pkg).as_deref(), Some(ver));
        assert!(set_bundle(&scratch, pkg, true).unwrap());
        assert!(bundles(&scratch).iter().any(|b| b == pkg));
        assert_eq!(plan(dep_version(&scratch, pkg).as_deref(), true, Some(ver)).0, "none");

        tool.run(&scratch, "remove", pkg).expect("pnpm remove 失败");
        assert_eq!(dep_version(&scratch, pkg), None);
        assert!(set_bundle(&scratch, pkg, false).unwrap());
        let _ = fs::remove_dir_all(&scratch);
    }

    fn has_backup(dir: &Path) -> bool {
        fs::read_dir(dir)
            .map(|read| {
                read.flatten()
                    .any(|e| e.file_name().to_string_lossy().contains("credit-ferry-"))
            })
            .unwrap_or(false)
    }
}
