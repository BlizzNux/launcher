// Bug and run reports: collect a system profile, the Battle.net and game builds, let the
// user see the report, and send it to blizznux.com. See REPORTING.md for the contract.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::{current_prefix, home, load_config, save_config};

pub const DEFAULT_REPORT_URL: &str = "https://blizznux.com/api/launcher/reports";
pub const DEFAULT_LINK_URL: &str = "https://blizznux.com/api/launcher/link";
pub const LINK_PAGE: &str = "https://blizznux.com/launcher/link";

fn link_url() -> String {
    std::env::var("BLIZZNUX_LINK_URL").unwrap_or_else(|_| DEFAULT_LINK_URL.to_string())
}

/// The pairing in progress (pair_secret never leaves the process).
struct Pairing {
    pair_id: String,
    pair_secret: String,
    started: Instant,
}

static PAIRING: std::sync::Mutex<Option<Pairing>> = std::sync::Mutex::new(None);

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct LinkStart {
    url: String,
    expires_in: u64,
}

/// Register a pairing with the site and return the login page URL to open.
#[tauri::command]
pub async fn link_start() -> Result<LinkStart, String> {
    let resp = http()?
        .post(format!("{}/start", link_url()))
        .json(&serde_json::json!({ "install_id": install_id(), "launcher_version": env!("CARGO_PKG_VERSION"), "distro": os_release("PRETTY_NAME") }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Err(body["error"].as_str().unwrap_or("the site did not accept the pairing request").to_string());
    }
    let pair_id = body["pair_id"].as_str().unwrap_or("").to_string();
    let pair_secret = body["pair_secret"].as_str().unwrap_or("").to_string();
    if pair_id.is_empty() || pair_secret.len() < 16 {
        return Err("the site returned an invalid pairing".into());
    }
    let expires_in = body["expires_in"].as_u64().unwrap_or(600);
    *PAIRING.lock().map_err(|e| e.to_string())? = Some(Pairing { pair_id, pair_secret, started: Instant::now() });
    // The page gets no pairing parameter: approval is triggered by the launcher itself
    // from inside its own login window, so a crafted link can never approve a pairing.
    let page = std::env::var("BLIZZNUX_LINK_PAGE").unwrap_or_else(|_| LINK_PAGE.to_string());
    Ok(LinkStart { url: page, expires_in })
}

const LOGIN_WINDOW: &str = "login";

/// Open blizznux.com's login page as the top-level document of a dedicated window, so the
/// launcher can run script in it (impossible in the cross-origin frame of the main window).
#[tauri::command]
pub fn login_open(app: AppHandle, url: String) -> Result<(), String> {
    let u: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;
    let allowed = std::env::var("BLIZZNUX_LINK_PAGE").ok().and_then(|p| p.parse::<url::Url>().ok()).and_then(|p| p.host_str().map(String::from));
    if u.host_str() != Some("blizznux.com") && u.host_str().map(String::from) != allowed {
        return Err("login window only opens blizznux.com".into());
    }
    if let Some(w) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, LOGIN_WINDOW, WebviewUrl::External(u))
        .title("Log in with BlizzNux.com")
        .inner_size(560.0, 720.0)
        .center()
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn login_close(app: AppHandle) {
    if let Some(w) = app.get_webview_window(LOGIN_WINDOW) {
        let _ = w.close();
    }
}

/// Run the approval step inside the login window: once Flarum reports a logged-in user,
/// POST the pairing id to /link/approve with the page's own session and CSRF token.
#[tauri::command]
pub fn login_approve_tick(app: AppHandle) -> Result<bool, String> {
    let pair_id = { PAIRING.lock().map_err(|e| e.to_string())?.as_ref().map(|p| p.pair_id.clone()) };
    let Some(pair_id) = pair_id else { return Ok(false) };
    let Some(w) = app.get_webview_window(LOGIN_WINDOW) else { return Ok(false) };
    if !pair_id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err("invalid pairing id".into());
    }
    let script = format!(r#"(function () {{
  if (window.__bzApproved) return;
  if (!(window.app && app.session && app.session.user)) return;
  window.__bzApproved = true;
  fetch("/api/launcher/link/approve", {{
    method: "POST", credentials: "same-origin",
    headers: {{ "Content-Type": "application/json", "X-CSRF-Token": app.session.csrfToken }},
    body: JSON.stringify({{ pair_id: "{pair_id}" }})
  }}).then(function (r) {{ if (!r.ok) window.__bzApproved = false; }}).catch(function () {{ window.__bzApproved = false; }});
}})();"#);
    w.eval(&script).map_err(|e| e.to_string())?;
    Ok(true)
}

#[derive(serde::Serialize)]
pub struct LinkPoll {
    status: String,
    username: String,
}

/// Ask the site whether the user has finished logging in; stores the token when they have.
#[tauri::command]
pub async fn link_poll() -> Result<LinkPoll, String> {
    let (pair_id, pair_secret, age) = {
        let g = PAIRING.lock().map_err(|e| e.to_string())?;
        let Some(p) = g.as_ref() else { return Ok(LinkPoll { status: "none".into(), username: String::new() }) };
        (p.pair_id.clone(), p.pair_secret.clone(), p.started.elapsed())
    };
    if age > Duration::from_secs(600) {
        *PAIRING.lock().map_err(|e| e.to_string())? = None;
        return Ok(LinkPoll { status: "expired".into(), username: String::new() });
    }
    let resp = http()?
        .post(format!("{}/poll", link_url()))
        .json(&serde_json::json!({ "pair_id": pair_id, "pair_secret": pair_secret }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    match code {
        202 => Ok(LinkPoll { status: "pending".into(), username: String::new() }),
        201 | 200 => {
            let token = body["token"].as_str().unwrap_or("").trim().to_string();
            let username = body["username"].as_str().unwrap_or("").trim().to_string();
            if token.len() < 16 || token.len() > 512 || !token.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) || username.is_empty() {
                return Err("the site returned an invalid token".into());
            }
            let mut c = load_config();
            c.insert("USER_TOKEN".into(), token);
            c.insert("USERNAME".into(), username.clone());
            save_config(&c)?;
            *PAIRING.lock().map_err(|e| e.to_string())? = None;
            Ok(LinkPoll { status: "linked".into(), username })
        }
        410 => {
            *PAIRING.lock().map_err(|e| e.to_string())? = None;
            Ok(LinkPoll { status: "expired".into(), username: String::new() })
        }
        _ => Err(body["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {code}"))),
    }
}

#[tauri::command]
pub fn link_cancel() {
    if let Ok(mut g) = PAIRING.lock() {
        *g = None;
    }
}

pub fn report_url() -> String {
    std::env::var("BLIZZNUX_REPORT_URL").unwrap_or_else(|_| DEFAULT_REPORT_URL.to_string())
}

/// Game codes the launcher knows: (code, name, install folder, process name fragments).
pub const GAMES: [(&str, &str, &str, &[&str]); 9] = [
    ("WoW", "World of Warcraft", "World of Warcraft", &["Wow.exe", "WowClassic.exe", "WowT.exe", "WowB.exe"]),
    ("WTCG", "Hearthstone", "Hearthstone", &["Hearthstone.exe"]),
    ("Hero", "Heroes of the Storm", "Heroes of the Storm", &["HeroesOfTheStorm"]),
    ("Pro", "Overwatch 2", "Overwatch", &["Overwatch.exe"]),
    ("Fen", "Diablo IV", "Diablo IV", &["Diablo IV.exe"]),
    ("D3", "Diablo III", "Diablo III", &["Diablo III64.exe", "Diablo III.exe"]),
    ("OSI", "Diablo II: Resurrected", "Diablo II Resurrected", &["D2R.exe"]),
    ("S2", "StarCraft II", "StarCraft II", &["SC2_x64.exe", "SC2.exe"]),
    ("W3", "Warcraft III: Reforged", "Warcraft III", &["Warcraft III.exe"]),
];

fn game_by_code(code: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static [&'static str])> {
    GAMES.iter().find(|g| g.0.eq_ignore_ascii_case(code))
}

fn run_capture(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

fn os_release(key: &str) -> String {
    crate::host_os_release()
        .lines()
        .find(|l| l.starts_with(&format!("{key}=")))
        .map(|l| l[key.len() + 1..].trim_matches('"').to_string())
        .unwrap_or_default()
}

#[derive(serde::Serialize, Clone)]
pub struct Gpu {
    vendor: String,
    name: String,
    driver: String,
}

#[derive(serde::Serialize, Clone)]
pub struct SystemProfile {
    distro: String,
    kernel: String,
    desktop: String,
    session: String,
    cpu: String,
    ram_gb: u64,
    gpus: Vec<Gpu>,
    proton: String,
    umu: String,
    prefix_kind: String,
}

fn gpus() -> Vec<Gpu> {
    let mut out: Vec<Gpu> = Vec::new();
    // Names from lspci when present; vendor from sysfs so it works without lspci too.
    let lspci = run_capture("lspci", &["-nn"], Duration::from_secs(3)).unwrap_or_default();
    let names: Vec<String> = lspci
        .lines()
        .filter(|l| l.contains("VGA compatible controller") || l.contains("3D controller") || l.contains("Display controller"))
        .map(|l| l.splitn(2, ": ").nth(1).unwrap_or(l).to_string())
        .collect();
    let nvidia_driver = fs::read_to_string("/sys/module/nvidia/version")
        .map(|s| s.trim().to_string())
        .or_else(|_| fs::read_to_string("/proc/driver/nvidia/version").map(|t| t.split_whitespace().find(|w| w.chars().next().map_or(false, |c| c.is_ascii_digit()) && w.contains('.')).unwrap_or("").to_string()))
        .unwrap_or_default();
    let mesa = run_capture("vulkaninfo", &["--summary"], Duration::from_secs(5))
        .and_then(|t| t.lines().find(|l| l.contains("driverInfo") && l.contains("Mesa")).map(|l| l.split('=').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| "Mesa".into());
    if let Ok(rd) = fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with("card") || n.contains('-') {
                continue;
            }
            let Ok(vid) = fs::read_to_string(e.path().join("device/vendor")) else { continue };
            let vendor = match vid.trim() { "0x10de" => "nvidia", "0x1002" => "amd", "0x8086" => "intel", _ => "other" };
            let name = names
                .iter()
                .find(|l| match vendor { "nvidia" => l.contains("NVIDIA"), "amd" => l.contains("AMD") || l.contains("ATI"), "intel" => l.contains("Intel"), _ => true })
                .cloned()
                .unwrap_or_else(|| vendor.to_string());
            let driver = match vendor { "nvidia" => nvidia_driver.clone(), "amd" | "intel" => mesa.clone(), _ => String::new() };
            if !out.iter().any(|g| g.vendor == vendor && g.name == name) {
                out.push(Gpu { vendor: vendor.into(), name, driver });
            }
        }
    }
    out
}

fn proton_in_use() -> String {
    let cfg = load_config();
    let mut dir: Option<PathBuf> = cfg.get("PROTON").filter(|p| !p.is_empty()).map(PathBuf::from);
    if dir.is_none() {
        if let Ok(rd) = fs::read_dir("/usr/share/steam/compatibilitytools.d") {
            dir = rd.flatten().map(|e| e.path()).find(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("proton-cachyos")) && p.join("proton").is_file());
        }
    }
    if dir.is_none() {
        let ge = home().join(".local/share/Steam/compatibilitytools.d");
        if let Ok(rd) = fs::read_dir(&ge) {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("GE-Proton"))).collect();
            v.sort();
            dir = v.pop();
        }
    }
    match dir {
        Some(d) => {
            let ver = fs::read_to_string(d.join("version")).unwrap_or_default();
            let ver = ver.split_whitespace().last().unwrap_or("").to_string();
            if ver.is_empty() { d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { ver }
        }
        None => "GE-Proton (downloaded by umu)".into(),
    }
}

pub fn system_profile() -> SystemProfile {
    let cpu = fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let ram_kb: u64 = fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("MemTotal"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let umu = run_capture("umu-run", &["--version"], Duration::from_secs(5))
        .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
        .map(|l| l.replace("umu-launcher version ", ""))
        .map(|l| l.split(' ').next().unwrap_or("").to_string())
        .unwrap_or_default();
    let prefix = current_prefix().display().to_string();
    let prefix_kind = if prefix.contains("/Steam/steamapps/compatdata/") {
        "steam"
    } else if prefix.to_lowercase().contains("lutris") || prefix.contains("/Games/") {
        "lutris"
    } else if prefix.contains("/blizznux/") {
        "blizznux"
    } else {
        "other"
    };
    SystemProfile {
        distro: os_release("PRETTY_NAME"),
        kernel: fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default(),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        session: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        cpu,
        ram_gb: (ram_kb + 512 * 1024) / (1024 * 1024),
        gpus: gpus(),
        proton: proton_in_use(),
        umu,
        prefix_kind: prefix_kind.into(),
    }
}

/// ProductVersion from a Windows executable's VERSIONINFO resource (UTF-16 scan; no parser).
fn pe_product_version(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let key: Vec<u8> = "ProductVersion".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let pos = bytes.windows(key.len()).position(|w| w == key.as_slice())?;
    let mut i = pos + key.len() + 2; // skip the terminating NUL
    while i + 1 < bytes.len() && bytes[i] == 0 && bytes[i + 1] == 0 {
        i += 2; // padding
    }
    let mut out = String::new();
    while i + 1 < bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if u == 0 { break; }
        out.push(char::from_u32(u as u32)?);
        i += 2;
        if out.len() > 40 { break; }
    }
    let out = out.trim().replace(", ", ".");
    if out.chars().all(|c| c.is_ascii_digit() || c == '.') && !out.is_empty() { Some(out) } else { None }
}

#[derive(serde::Serialize, Clone)]
pub struct BattlenetInfo {
    pub build: String,
    pub version: String,
}

pub fn battlenet_info() -> BattlenetInfo {
    let root = current_prefix().join("drive_c/Program Files (x86)/Battle.net");
    let mut builds: Vec<(u64, PathBuf)> = fs::read_dir(&root)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter_map(|p| {
                    let n = p.file_name()?.to_string_lossy().to_string();
                    let b = n.strip_prefix("Battle.net.")?.parse::<u64>().ok()?;
                    Some((b, p))
                })
                .collect()
        })
        .unwrap_or_default();
    builds.sort();
    // The client's main executable lives at the top of the Battle.net folder; the versioned
    // subfolders hold helpers. Try the top-level one first.
    let version = pe_product_version(&root.join("Battle.net.exe"))
        .or_else(|| builds.last().and_then(|(_, dir)| pe_product_version(&dir.join("Battle.net.exe"))))
        .unwrap_or_default();
    let build = builds.last().map(|(b, _)| b.to_string()).or_else(|| version.rsplit('.').next().map(String::from)).unwrap_or_default();
    BattlenetInfo { build, version }
}

#[derive(serde::Serialize, Clone)]
pub struct GameInfo {
    code: String,
    name: String,
    flavor: String,
    version: String,
}

/// Parse Blizzard's `.build.info` (pipe-separated, `Name!TYPE:len` header) → (product, version).
fn build_info(path: &Path) -> Option<Vec<BTreeMap<String, String>>> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Vec<String> = lines.next()?.split('|').map(|h| h.split('!').next().unwrap_or("").to_string()).collect();
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| header.iter().cloned().zip(l.split('|').map(|v| v.to_string())).collect::<BTreeMap<_, _>>())
        .collect();
    Some(rows)
}

pub fn game_info(code: &str) -> Option<GameInfo> {
    let g = game_by_code(code)?;
    let root = current_prefix().join("drive_c/Program Files (x86)").join(g.2);
    let rows = build_info(&root.join(".build.info")).unwrap_or_default();
    let row = rows.iter().find(|r| r.get("Active").map(|a| a == "1").unwrap_or(false)).or_else(|| rows.first());
    let version = row.and_then(|r| r.get("Version")).cloned().unwrap_or_default();
    let product = row.and_then(|r| r.get("Product")).cloned().unwrap_or_default();
    let flavor = match product.as_str() {
        "wow" => "_retail_",
        "wow_classic" => "_classic_",
        "wow_classic_era" => "_classic_era_",
        "wowt" => "_ptr_",
        _ => "",
    };
    Some(GameInfo { code: g.0.into(), name: g.1.into(), flavor: flavor.into(), version })
}

fn install_id() -> String {
    let mut cfg = load_config();
    if let Some(id) = cfg.get("INSTALL_ID").filter(|s| s.len() >= 32) {
        return id.clone();
    }
    let mut b = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let id = format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]);
    cfg.insert("INSTALL_ID".into(), id.clone());
    let _ = crate::save_config(&cfg);
    id
}

#[derive(serde::Serialize)]
pub struct AccountStatus {
    linked: bool,
    username: String,
    sharing: bool,
    bug_auto: bool,
}

#[tauri::command]
pub fn account_status() -> AccountStatus {
    let c = load_config();
    AccountStatus {
        linked: c.get("USER_TOKEN").map_or(false, |t| !t.is_empty()),
        username: c.get("USERNAME").cloned().unwrap_or_default(),
        sharing: c.get("REPORTS_SHARE").map_or(false, |v| v == "1"),
        bug_auto: c.get("BUG_AUTO").map_or(false, |v| v == "1"),
    }
}

/// Automatic bug report for a failed launch, when the user opted in.
pub fn auto_bug_report(app: &AppHandle, comment: String) {
    let s = account_status();
    if !(s.linked && s.bug_auto) {
        return;
    }
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(body) = build_report("bug".into(), None, None, Some(comment)) {
            match send_report(body).await {
                Ok(url) => { let _ = h.emit("report-sent", serde_json::json!({ "kind": "bug", "outcome": "sent", "url": url })); }
                Err(e) => eprintln!("[report] bug not sent: {e}"),
            }
        }
    });
}

/// Exchange the one-time code shown on blizznux.com/launcher/link for a launcher token.
#[tauri::command]
pub async fn link_account(code: String) -> Result<AccountStatus, String> {
    let code: String = code.trim().to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if code.len() < 6 || code.len() > 12 {
        return Err("enter the code shown on blizznux.com".into());
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(link_url())
        .json(&serde_json::json!({ "code": code, "install_id": install_id(), "launcher_version": env!("CARGO_PKG_VERSION") }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status.as_u16() == 429 {
        return Err("too many attempts; wait a minute and try again".into());
    }
    if !status.is_success() {
        return Err(body["error"].as_str().unwrap_or("the code was not accepted").to_string());
    }
    let token = body["token"].as_str().unwrap_or("").trim().to_string();
    let username = body["username"].as_str().unwrap_or("").trim().to_string();
    if token.len() < 16 || token.len() > 512 || !token.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err("the site returned an invalid token".into());
    }
    if username.is_empty() || username.len() > 64 {
        return Err("the site returned no username".into());
    }
    let mut c = load_config();
    c.insert("USER_TOKEN".into(), token);
    c.insert("USERNAME".into(), username);
    save_config(&c)?;
    Ok(account_status())
}

#[tauri::command]
pub fn unlink_account() -> Result<AccountStatus, String> {
    let mut c = load_config();
    c.remove("USER_TOKEN");
    c.remove("USERNAME");
    c.insert("REPORTS_SHARE".into(), "0".into());
    c.insert("BUG_AUTO".into(), "0".into());
    save_config(&c)?;
    Ok(account_status())
}

/// True when the user linked an account and opted into community reports.
fn sharing_enabled() -> bool {
    let s = account_status();
    s.linked && s.sharing
}

fn scrub(text: &str) -> String {
    let h = home().display().to_string();
    text.replace(&h, "~")
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // civil-from-days (Howard Hinnant), good enough for a timestamp without a chrono dependency
    let days = (secs / 86400) as i64;
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Build the JSON body for a report; the UI shows it before sending.
#[tauri::command]
pub fn build_report(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, String> {
    if !matches!(kind.as_str(), "bug" | "run" | "launch") {
        return Err("kind must be bug, run or launch".into());
    }
    let mut body = serde_json::json!({
        "schema": 1,
        "type": kind,
        "install_id": install_id(),
        "user_token": load_config().get("USER_TOKEN").cloned().unwrap_or_default(),
        "launcher_version": env!("CARGO_PKG_VERSION"),
        "created_at": now_iso(),
        "system": system_profile(),
        "battlenet": battlenet_info(),
    });
    if let Some(code) = game.as_deref().filter(|c| !c.is_empty()) {
        if let Some(g) = game_info(code) {
            body["game"] = serde_json::to_value(g).map_err(|e| e.to_string())?;
        }
    }
    if kind == "launch" {
        let o = outcome.unwrap_or_default();
        if !matches!(o.as_str(), "ok" | "failed") {
            return Err("launch outcome must be ok or failed".into());
        }
        body["target"] = serde_json::Value::String("battlenet".into());
        body["outcome"] = serde_json::Value::String(o.clone());
        if o == "failed" {
            let log = fs::read_to_string(crate::log_path()).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            body["log"] = serde_json::Value::String(scrub(&lines[lines.len().saturating_sub(120)..].join("\n")));
        }
    } else if kind == "run" {
        let o = outcome.unwrap_or_default();
        if !matches!(o.as_str(), "perfect" | "issues" | "broken" | "ok") {
            return Err("outcome must be perfect, issues, broken or ok".into());
        }
        body["outcome"] = serde_json::Value::String(o);
        body["comment"] = serde_json::Value::String(scrub(&comment.unwrap_or_default()).chars().take(2000).collect());
    } else {
        let log = fs::read_to_string(crate::log_path()).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(200)..].join("\n");
        body["log"] = serde_json::Value::String(scrub(&tail));
        if let Some(c) = comment.filter(|c| !c.is_empty()) {
            body["comment"] = serde_json::Value::String(scrub(&c).chars().take(2000).collect());
        }
    }
    Ok(body)
}

#[tauri::command]
pub async fn send_report(report: serde_json::Value) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.post(report_url()).json(&report).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        return Err("the site is rate-limiting reports right now; try again later".into());
    }
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text);
        return Err(format!("HTTP {}: {}", status.as_u16(), msg.chars().take(300).collect::<String>()));
    }
    let url = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["url"].as_str().map(String::from)).unwrap_or_default();
    Ok(url)
}

/// Successful automatic reports are sent at most once per day per build; failures always.
fn already_sent_today(kind: &str, game: Option<&str>, outcome: &str) -> bool {
    if outcome != "ok" {
        return false;
    }
    let build = match (kind, game) {
        ("launch", _) => battlenet_info().version,
        ("run", Some(code)) => game_info(code).map(|g| format!("{}:{}", g.code, g.version)).unwrap_or_default(),
        _ => String::new(),
    };
    let stamp = format!("{kind}:{build}:{}", &now_iso()[..10]);
    let key = if kind == "launch" { "LAST_LAUNCH_OK" } else { "LAST_RUN_OK" };
    let mut c = load_config();
    if c.get(key).map_or(false, |v| v.split(';').any(|s| s == stamp)) {
        return true;
    }
    // keep the last few stamps so several games on one day are each sent once
    let mut stamps: Vec<String> = c.get(key).map(|v| v.split(';').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
    stamps.push(stamp);
    while stamps.len() > 12 { stamps.remove(0); }
    c.insert(key.into(), stamps.join(";"));
    let _ = save_config(&c);
    false
}

/// Send a report in the background, only when the user linked an account and opted in.
fn auto_report(app: &AppHandle, kind: &str, game: Option<String>, outcome: &str, comment: String) {
    if !sharing_enabled() || already_sent_today(kind, game.as_deref(), outcome) {
        return;
    }
    let (kind, outcome) = (kind.to_string(), outcome.to_string());
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        match build_report(kind.clone(), game, Some(outcome.clone()), Some(comment)) {
            Ok(body) => match send_report(body).await {
                Ok(url) => { let _ = h.emit("report-sent", serde_json::json!({ "kind": kind, "outcome": outcome, "url": url })); }
                Err(e) => eprintln!("[report] {kind} not sent: {e}"),
            },
            Err(e) => eprintln!("[report] {kind} not built: {e}"),
        }
    });
}

/// After Launch: did the Battle.net client actually come up? Reports ok/failed when sharing.
pub fn watch_launch(app: AppHandle) {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut ok = false;
        while start.elapsed() < Duration::from_secs(120) {
            if game_running(&["Battle.net.exe", "Battle.net Launcher.exe", "Battle.net-Setup.exe"]) {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        let _ = app.emit("launch-result", serde_json::json!({ "ok": ok }));
        auto_report(&app, "launch", None, if ok { "ok" } else { "failed" }, if ok { String::new() } else { "Battle.net did not start within two minutes".into() });
        if !ok {
            auto_bug_report(&app, "Battle.net did not start within two minutes".into());
        }
    });
}

/// Is one of the game's processes running? Looks at /proc/*/cmdline (Wine shows the .exe path).
fn game_running(fragments: &[&str]) -> bool {
    let Ok(rd) = fs::read_dir("/proc") else { return false };
    for e in rd.flatten() {
        let n = e.file_name();
        if !n.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = fs::read(e.path().join("cmdline")) else { continue };
        let first = cmd.split(|b| *b == 0).next().unwrap_or(&[]);
        let first = String::from_utf8_lossy(first).to_lowercase();
        if fragments.iter().any(|f| first.ends_with(&f.to_lowercase()) || first.contains(&format!("{}\\", f.to_lowercase())) || first.contains(&f.to_lowercase())) {
            return true;
        }
    }
    false
}

/// After a launch: wait for the game to appear (up to 15 min), then for it to end, then tell
/// the UI so it can ask how the session went.
pub fn watch_game(app: AppHandle, code: Option<String>) {
    std::thread::spawn(move || {
        let candidates: Vec<&(&str, &str, &str, &[&str])> = match code.as_deref().and_then(game_by_code) {
            Some(g) => vec![g],
            None => GAMES.iter().collect(),
        };
        let start = Instant::now();
        let mut running: Option<&(&str, &str, &str, &[&str])> = None;
        while start.elapsed() < Duration::from_secs(15 * 60) {
            if let Some(g) = candidates.iter().find(|g| game_running(g.3)) {
                running = Some(g);
                break;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        let Some(g) = running else { return };
        let began = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if !game_running(g.3) {
                break;
            }
        }
        let secs = began.elapsed().as_secs();
        if secs < 60 {
            // Gone within a minute: treat as a failed start and record it (when sharing).
            auto_report(&app, "run", Some(g.0.to_string()), "broken", format!("{} exited after {secs} s", g.1));
            let _ = app.emit("game-crashed", serde_json::json!({ "code": g.0, "name": g.1, "seconds": secs }));
            return;
        }
        // A real session: record the successful run (when sharing), then ask for details.
        auto_report(&app, "run", Some(g.0.to_string()), "ok", format!("ran for {} min", secs / 60));
        let _ = app.emit("game-ended", serde_json::json!({ "code": g.0, "name": g.1, "seconds": secs }));
    });
}
