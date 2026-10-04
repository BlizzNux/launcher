// Bug and run reports: collect a system profile, the Battle.net and game builds, let the
// user see the report, and send it to blizznux.com. See REPORTING.md for the contract.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

use crate::{current_prefix, home, load_config, save_config};

pub const DEFAULT_REPORT_URL: &str = "https://blizznux.com/api/launcher/reports";
pub const DEFAULT_LINK_URL: &str = "https://blizznux.com/api/launcher/link";
/// The page a launcher that is not logged in opens: the forum's front page. With the pairing cookie
/// in place the site shows its login box there once, and it can be closed; nobody is made to log in.
pub const LINK_PAGE: &str = "https://blizznux.com/";

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
pub async fn link_start(app: AppHandle) -> Result<LinkStart, String> {
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
    set_pair_cookie(&app, Some(pair_id.clone()))?;
    *PAIRING.lock().map_err(|e| e.to_string())? = Some(Pairing { pair_id, pair_secret, started: Instant::now() });
    // The page gets no pairing parameter and no secret: it learns the pairing only from the
    // cookie above, which exists only in this launcher's own cookie store.
    let page = std::env::var("BLIZZNUX_LINK_PAGE").unwrap_or_else(|_| LINK_PAGE.to_string());
    Ok(LinkStart { url: page, expires_in })
}

const PAIR_COOKIE: &str = "bz_pair";

fn site_host() -> String {
    std::env::var("BLIZZNUX_LINK_PAGE")
        .ok()
        .and_then(|p| p.parse::<url::Url>().ok())
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "blizznux.com".into())
}

/// Put the pairing id in the launcher's own cookie store for the site (HttpOnly, Secure,
/// SameSite=None so the embedded site sends it). Only this launcher can set it, so the site
/// can trust it when a logged-in page asks to approve; a crafted link cannot plant it.
fn set_pair_cookie(app: &AppHandle, value: Option<String>) -> Result<(), String> {
    let main = app.get_webview_window("main").ok_or("main window missing")?;
    let host = site_host();
    main.with_webview(move |platform| {
        #[cfg(target_os = "linux")]
        {
            use webkit2gtk::{CookieManagerExt, WebContextExt, WebViewExt};
            let Some(ctx) = platform.inner().context() else { return };
            let Some(cm) = ctx.cookie_manager() else { return };
            let (val, max_age) = match value { Some(v) => (v, 600), None => (String::new(), 0) };
            let mut cookie = soup::Cookie::new(PAIR_COOKIE, &val, &host, "/", max_age);
            cookie.set_secure(true);
            cookie.set_http_only(true);
            cookie.set_same_site_policy(soup::SameSitePolicy::None);
            if max_age == 0 {
                cm.delete_cookie(&mut cookie, None::<&webkit2gtk::gio::Cancellable>, |_| {});
            } else {
                cm.add_cookie(&mut cookie, None::<&webkit2gtk::gio::Cancellable>, |_| {});
            }
        }
    })
    .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
pub struct LinkPoll {
    status: String,
    username: String,
    /// Seconds to wait before asking again when `status` is "busy".
    retry_after: u64,
    /// The same account as before on this launcher, with its sharing choices still in place.
    returning: bool,
}

fn poll_answer(status: &str, retry_after: u64) -> LinkPoll {
    LinkPoll { status: status.into(), username: String::new(), retry_after, returning: false }
}

/// How long to wait when the site cannot answer a poll right now, or None when the answer is not
/// one to wait out. "Too many requests" and server errors are waited out, for as long as the site
/// asks (Retry-After, in seconds) or half a minute. Giving up for the session instead left every
/// launcher behind one shared address unable to notice a login.
fn busy_wait(code: u16, retry_after: Option<u64>) -> Option<u64> {
    match code {
        429 | 500..=599 => Some(retry_after.filter(|s| *s > 0).unwrap_or(30).min(600)),
        _ => None,
    }
}

/// Ask the site whether the user has finished logging in; stores the token when they have.
#[tauri::command]
pub async fn link_poll(app: AppHandle) -> Result<LinkPoll, String> {
    let (pair_id, pair_secret, age) = {
        let g = PAIRING.lock().map_err(|e| e.to_string())?;
        let Some(p) = g.as_ref() else { return Ok(poll_answer("none", 0)) };
        (p.pair_id.clone(), p.pair_secret.clone(), p.started.elapsed())
    };
    if age > Duration::from_secs(600) {
        *PAIRING.lock().map_err(|e| e.to_string())? = None;
        return Ok(poll_answer("expired", 0));
    }
    let sent = http()?
        .post(format!("{}/poll", link_url()))
        .json(&serde_json::json!({ "pair_id": pair_id, "pair_secret": pair_secret }))
        .send()
        .await;
    let Ok(resp) = sent else { return Ok(poll_answer("busy", 30)) };   // site unreachable right now
    let code = resp.status().as_u16();
    let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if let Some(wait) = busy_wait(code, retry_after) {
        return Ok(poll_answer("busy", wait));
    }
    match code {
        202 => Ok(poll_answer("pending", 0)),
        201 | 200 => {
            let token = body["token"].as_str().unwrap_or("").trim().to_string();
            let username = body["username"].as_str().unwrap_or("").trim().to_string();
            if token.len() < 16 || token.len() > 512 || !token.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) || username.is_empty() {
                return Err("the site returned an invalid token".into());
            }
            let mut c = load_config();
            // The link was ended from the site's side and the same person is back: nothing to ask again.
            let returning = c.remove("LAST_USERNAME").map_or(false, |last| last == username) && c.contains_key("REPORTS_SHARE");
            if !returning {
                // Someone else, or a first login: nothing is shared until they have been asked.
                c.insert("REPORTS_SHARE".into(), "0".into());
                c.insert("BUG_AUTO".into(), "0".into());
            }
            c.insert("USER_TOKEN".into(), token);
            c.insert("USERNAME".into(), username.clone());
            save_config(&c)?;
            *PAIRING.lock().map_err(|e| e.to_string())? = None;
            let _ = set_pair_cookie(&app, None);
            Ok(LinkPoll { status: "linked".into(), username, retry_after: 0, returning })
        }
        410 => {
            *PAIRING.lock().map_err(|e| e.to_string())? = None;
            Ok(poll_answer("expired", 0))
        }
        _ => Err(body["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {code}"))),
    }
}

#[tauri::command]
pub fn link_cancel(app: AppHandle) {
    if let Ok(mut g) = PAIRING.lock() {
        *g = None;
    }
    let _ = set_pair_cookie(&app, None);
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
    let mut child = crate::host_command(cmd)
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
    let umu = crate::umu_run()
        .and_then(|p| run_capture(&p.to_string_lossy(), &["--version"], Duration::from_secs(5)))
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

/// The length-delimited fields of a protobuf message, in order (no schema, no parser crate);
/// stops at the first thing it cannot read.
fn pb_fields(mut buf: &[u8]) -> Vec<(u64, &[u8])> {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = buf.split_first()?;
            *buf = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 { return Some(v); }
        }
        None
    }
    fn next<'a>(buf: &mut &'a [u8]) -> Option<(u64, &'a [u8])> {
        let key = varint(buf)?;
        let len = match key & 7 {
            0 => { varint(buf)?; 0 }
            1 => 8,
            2 => usize::try_from(varint(buf)?).ok()?,
            5 => 4,
            _ => return None,
        };
        let whole: &'a [u8] = buf;
        if len > whole.len() { return None; }
        let (value, rest) = whole.split_at(len);
        *buf = rest;
        Some((key, value))
    }
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some((key, value)) = next(&mut buf) else { break };
        if key & 7 == 2 { out.push((key >> 3, value)); }
    }
    out
}

/// First length-delimited value of `field` in a protobuf message.
fn pb_field(buf: &[u8], field: u64) -> Option<&[u8]> {
    pb_fields(buf).into_iter().find(|(f, _)| *f == field).map(|(_, value)| value)
}

/// Installed version from Battle.net's `.product.db` install record (protobuf:
/// cached_product_state → base_product_state → current_version_str).
fn product_db_version(bytes: &[u8]) -> Option<String> {
    let version = pb_field(pb_field(pb_field(bytes, 4)?, 1)?, 7)?;
    let version = std::str::from_utf8(version).ok()?.trim();
    if version.is_empty() { None } else { Some(version.to_string()) }
}

/// What the Battle.net Agent has installed, from its `product.db` (protobuf: product_installs →
/// product_code, settings → install_path): (product code, Windows install path).
fn agent_installs(bytes: &[u8]) -> Vec<(String, String)> {
    let text = |b: &[u8]| std::str::from_utf8(b).ok().map(str::to_string);
    pb_fields(bytes)
        .into_iter()
        .filter(|(field, _)| *field == 1)
        .filter_map(|(_, install)| Some((text(pb_field(install, 2)?)?, text(pb_field(pb_field(install, 3)?, 1)?)?)))
        .collect()
}

/// The Windows install path of a game among the Agent's installs. The product code is the
/// launch code in lower case except for Hearthstone and Diablo IV; test and classic clients
/// (`herot`, `wow_classic`, …) only count when the main product is not installed.
fn install_path<'a>(installs: &'a [(String, String)], code: &str) -> Option<&'a str> {
    let product = match code {
        "WTCG" => "hsb".to_string(),
        "Fen" => "fenris".to_string(),
        c => c.to_lowercase(),
    };
    installs
        .iter()
        .find(|(p, _)| *p == product)
        .or_else(|| installs.iter().find(|(p, _)| p.starts_with(&product)))
        .map(|(_, path)| path.as_str())
}

/// A Windows path as the host sees it, through the prefix's drive links
/// (`D:\Games\Hearthstone` → `<prefix>/dosdevices/d:/Games/Hearthstone`).
fn host_path(prefix: &Path, windows: &str) -> Option<PathBuf> {
    let (drive, rest) = windows.split_once(':')?;
    if drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest = rest.replace('\\', "/");
    Some(prefix.join("dosdevices").join(format!("{}:", drive.to_ascii_lowercase())).join(rest.trim_start_matches('/')))
}

/// The game's install folder: where the Agent put it, else the default location.
fn game_root(prefix: &Path, g: &(&str, &str, &str, &[&str])) -> PathBuf {
    let installs = fs::read(prefix.join("drive_c/ProgramData/Battle.net/Agent/product.db")).map(|b| agent_installs(&b)).unwrap_or_default();
    install_path(&installs, g.0)
        .and_then(|path| host_path(prefix, path))
        .filter(|root| root.is_dir())
        .unwrap_or_else(|| prefix.join("drive_c/Program Files (x86)").join(g.2))
}

/// A game as it travels through the report code: its launch code, optionally followed by the
/// flavour folder it ran from (`WoW`, `WoW/_classic_era_`).
fn split_game(id: &str) -> (&str, &str) {
    id.split_once('/').unwrap_or((id, ""))
}

fn game_id(code: &str, flavor: &str) -> String {
    if flavor.is_empty() { code.to_string() } else { format!("{code}/{flavor}") }
}

/// The flavour folder a game ran from, taken from the Windows path of its program
/// (`…\World of Warcraft\_classic_era_\WowClassic.exe` → `_classic_era_`); empty when there is none.
fn flavor_of(exe_path: &str) -> String {
    let mut parts = exe_path.rsplit(|c| c == '\\' || c == '/');
    parts.next();
    match parts.next() {
        Some(dir) if dir.len() > 2 && dir.starts_with('_') && dir.ends_with('_') => dir.to_lowercase(),
        _ => String::new(),
    }
}

/// Blizzard's product name for a World of Warcraft flavour folder (`_classic_era_` →
/// `wow_classic_era`). Other games have one product per folder and need no such choice.
fn wow_product(code: &str, flavor: &str) -> Option<String> {
    if code != "WoW" {
        return None;
    }
    let inner = flavor.strip_prefix('_')?.strip_suffix('_')?;
    Some(match inner {
        "retail" => "wow".into(),
        "ptr" => "wowt".into(),
        "xptr" => "wowxptr".into(),
        "beta" => "wow_beta".into(),
        other => format!("wow_{other}"),
    })
}

/// The `.build.info` row for what actually ran. World of Warcraft keeps every flavour in one
/// folder with one row per product, so the row is the one of the flavour the game ran from; when
/// that flavour has no row there is no version, rather than another flavour's.
fn build_row<'a>(rows: &'a [BTreeMap<String, String>], code: &str, flavor: &str) -> Option<&'a BTreeMap<String, String>> {
    match wow_product(code, flavor) {
        Some(product) => rows.iter().find(|r| r.get("Product").map_or(false, |p| *p == product)),
        None => rows.iter().find(|r| r.get("Active").map_or(false, |a| a == "1")).or_else(|| rows.first()),
    }
}

/// Does the site file sessions of this game? It follows the three live World of Warcraft products;
/// a test realm, beta or Anniversary session has no thread to go to and would read as a Retail
/// patch there, so those sessions are not reported and nothing is asked about them.
fn is_reported(code: &str, flavor: &str) -> bool {
    code != "WoW" || matches!(flavor, "" | "_retail_" | "_classic_" | "_classic_era_")
}

/// The name the site files a game under; World of Warcraft's classic clients have their own.
fn game_name(g: &(&'static str, &'static str, &'static str, &'static [&'static str]), flavor: &str) -> &'static str {
    match (g.0, flavor) {
        ("WoW", "_classic_era_") => "WoW Classic Era",
        ("WoW", "_classic_") => "WoW Classic",
        _ => g.1,
    }
}

pub fn game_info(id: &str) -> Option<GameInfo> {
    let (code, ran_from) = split_game(id);
    let g = game_by_code(code)?;
    let root = game_root(&current_prefix(), g);
    let rows = build_info(&root.join(".build.info")).unwrap_or_default();
    let row = build_row(&rows, g.0, ran_from);
    let version = row.and_then(|r| r.get("Version")).filter(|v| !v.is_empty()).cloned();
    // Hearthstone has no `.build.info`; its version only lives in `.product.db`. That file is the
    // folder's main product, so it cannot stand in for a World of Warcraft flavour.
    let version = match version {
        Some(v) => v,
        None if wow_product(g.0, ran_from).is_some() => String::new(),
        None => fs::read(root.join(".product.db")).ok().and_then(|b| product_db_version(&b)).unwrap_or_default(),
    };
    // Only World of Warcraft has flavours as far as reports go; the field stays empty for the rest.
    let flavor = if wow_product(g.0, ran_from).is_some() {
        ran_from
    } else {
        match row.and_then(|r| r.get("Product")).map(String::as_str).unwrap_or_default() {
            "wow" => "_retail_",
            "wow_classic" => "_classic_",
            "wow_classic_era" => "_classic_era_",
            "wowt" => "_ptr_",
            _ => "",
        }
    };
    Some(GameInfo { code: g.0.into(), name: game_name(g, flavor).into(), flavor: flavor.into(), version })
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

/// What the site's answer to a link check means. Only a clear "no" ends the link; a site that is
/// down, busy or does not know the question yet leaves it alone.
fn check_outcome(code: u16) -> &'static str {
    match code {
        200 => "linked",
        401 => "revoked",
        _ => "unknown",
    }
}

#[derive(serde::Serialize)]
pub struct LinkCheck {
    status: String,
    username: String,
}

/// Ask the site whether the stored link still stands, so the launcher never shows a login it no
/// longer has. When the site has ended it (revoked on the profile, or logged out in the embedded
/// view), the token is forgotten here too. The sharing choices stay, so logging in again brings
/// everything back without a question.
#[tauri::command]
pub async fn link_check() -> Result<LinkCheck, String> {
    let c = load_config();
    let token = c.get("USER_TOKEN").cloned().unwrap_or_default();
    let username = c.get("USERNAME").cloned().unwrap_or_default();
    if token.is_empty() {
        return Ok(LinkCheck { status: "unlinked".into(), username: String::new() });
    }
    let sent = http()?
        .post(format!("{}/check", link_url()))
        .json(&serde_json::json!({ "install_id": install_id(), "user_token": token }))
        .send()
        .await;
    let Ok(resp) = sent else { return Ok(LinkCheck { status: "unknown".into(), username }) };
    let status = check_outcome(resp.status().as_u16());
    if status == "revoked" {
        let mut c = load_config();
        if let Some(u) = c.remove("USERNAME") {
            c.insert("LAST_USERNAME".into(), u);
        }
        c.remove("USER_TOKEN");
        save_config(&c)?;
        return Ok(LinkCheck { status: status.into(), username: String::new() });
    }
    Ok(LinkCheck { status: status.into(), username })
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

/// How many raw log lines are looked at, and the longest repeating block that gets folded.
const LOG_WINDOW: usize = 4000;
const MAX_BLOCK: usize = 8;

/// What two log lines have in common when they differ only in numbers and paths.
fn line_shape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            if c == '0' && chars.peek() == Some(&'x') {
                chars.next();
                while chars.peek().is_some_and(|n| n.is_ascii_hexdigit()) {
                    chars.next();
                }
            } else {
                while chars.peek().is_some_and(|n| n.is_ascii_digit()) {
                    chars.next();
                }
            }
            out.push('#');
        } else if c == '/' {
            while chars.peek().is_some_and(|n| !n.is_whitespace() && *n != '\'' && *n != '"') {
                chars.next();
            }
            out.push('/');
        } else {
            out.push(c);
        }
    }
    out
}

/// Folds log noise so the tail of a report holds distinct lines: blank lines go, the harmless
/// GStreamer plugin notices get a plain wording, and a line (or a block of up to MAX_BLOCK
/// lines) repeated three or more times in a row is kept once with a count.
pub(crate) fn condense_log(text: &str) -> Vec<String> {
    let all: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    let lines: Vec<String> = all[all.len().saturating_sub(LOG_WINDOW)..]
        .iter()
        .map(|l| {
            // Proton lists its 32-bit and 64-bit plugin folders for both kinds of process, so
            // each one warns once per plugin of the other kind. Nothing failed.
            if l.contains("GStreamer-WARNING") && l.contains("wrong ELF class") {
                "GStreamer skipped a plugin built for the other CPU architecture (harmless)".to_string()
            } else {
                l.to_string()
            }
        })
        .collect();
    let shapes: Vec<String> = lines.iter().map(|l| line_shape(l)).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut folded = false;
        for p in 1..=MAX_BLOCK.min((lines.len() - i) / 3) {
            let mut reps = 1;
            while i + (reps + 1) * p <= lines.len() && shapes[i + reps * p..i + (reps + 1) * p] == shapes[i..i + p] {
                reps += 1;
            }
            if reps >= 3 {
                if p == 1 {
                    out.push(format!("(×{reps}) {}", lines[i]));
                } else {
                    out.push(format!("(×{reps}, the next {p} lines)"));
                    out.extend(lines[i..i + p].iter().cloned());
                }
                i += reps * p;
                folded = true;
                break;
            }
        }
        if !folded {
            out.push(lines[i].clone());
            i += 1;
        }
    }
    out
}

/// The end of the launcher log as attached to a report: folded, then cut, then scrubbed.
fn log_tail(max_lines: usize) -> String {
    let log = fs::read_to_string(crate::log_path()).unwrap_or_default();
    let lines = condense_log(&log);
    scrub(&lines[lines.len().saturating_sub(max_lines)..].join("\n"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn now_iso() -> String {
    iso(now_secs())
}

fn iso(secs: u64) -> String {
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

/// Why a report could not be built: a request that makes no sense, or a game whose build the
/// launcher cannot read (the site files run reports under the build and refuses one without).
enum BuildError {
    Invalid(String),
    UnknownBuild(String),
}

impl BuildError {
    fn text(self) -> String {
        match self {
            BuildError::Invalid(why) | BuildError::UnknownBuild(why) => why,
        }
    }
}

/// Build the JSON body for a report; the UI shows it before sending.
#[tauri::command]
pub fn build_report(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, String> {
    build(kind, game, outcome, comment).map_err(BuildError::text)
}

fn build(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, BuildError> {
    let invalid = |why: &str| BuildError::Invalid(why.into());
    if !matches!(kind.as_str(), "bug" | "run" | "launch") {
        return Err(invalid("kind must be bug, run or launch"));
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
            body["game"] = serde_json::to_value(g).map_err(|e| invalid(&e.to_string()))?;
        }
    }
    if kind == "launch" {
        let o = outcome.unwrap_or_default();
        if !matches!(o.as_str(), "ok" | "failed") {
            return Err(invalid("launch outcome must be ok or failed"));
        }
        body["target"] = serde_json::Value::String("battlenet".into());
        body["outcome"] = serde_json::Value::String(o.clone());
        if o == "failed" {
            body["log"] = serde_json::Value::String(log_tail(120));
        }
    } else if kind == "run" {
        let o = outcome.unwrap_or_default();
        if !matches!(o.as_str(), "perfect" | "issues" | "broken" | "ok") {
            return Err(invalid("outcome must be perfect, issues, broken or ok"));
        }
        if let Some((code, flavor)) = game.as_deref().map(split_game) {
            if !is_reported(code, flavor) {
                return Err(invalid("sessions on test, beta and Anniversary realms are not reported"));
            }
        }
        if body["game"]["version"].as_str().map_or(true, str::is_empty) {
            let name = game.as_deref().map(split_game).and_then(|(code, flavor)| game_by_code(code).map(|g| game_name(g, flavor))).unwrap_or("the game");
            return Err(BuildError::UnknownBuild(format!(
                "The launcher cannot tell which build of {name} is installed, so the site has nothing to file this report under. Please tell BlizzNux through \"Report a problem\" in Settings."
            )));
        }
        body["outcome"] = serde_json::Value::String(o);
        body["comment"] = serde_json::Value::String(scrub(&comment.unwrap_or_default()).chars().take(2000).collect());
    } else {
        body["log"] = serde_json::Value::String(log_tail(200));
        if let Some(c) = comment.filter(|c| !c.is_empty()) {
            body["comment"] = serde_json::Value::String(scrub(&c).chars().take(2000).collect());
        }
    }
    Ok(body)
}

/// Why a report was not delivered: the site refused this report, or the site could not take
/// any right now (offline, rate limit, server trouble), which is worth another try later.
enum SendError {
    Refused(String),
    Unreachable(String),
}

#[tauri::command]
pub async fn send_report(report: serde_json::Value) -> Result<String, String> {
    post_report(&report).await.map_err(|e| match e {
        SendError::Refused(why) | SendError::Unreachable(why) => why,
    })
}

async fn post_report(report: &serde_json::Value) -> Result<String, SendError> {
    let offline = |_| SendError::Unreachable("The site could not be reached.".into());
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(offline)?;
    let resp = client.post(report_url()).json(report).send().await.map_err(offline)?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        return Err(SendError::Unreachable("The site is not taking more reports right now.".into()));
    }
    if status.is_server_error() {
        return Err(SendError::Unreachable(format!("The site had a problem (HTTP {}).", status.as_u16())));
    }
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text);
        return Err(SendError::Refused(format!("The site did not accept the report (HTTP {}: {}).", status.as_u16(), msg.chars().take(300).collect::<String>())));
    }
    let url = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["url"].as_str().map(String::from)).unwrap_or_default();
    Ok(url)
}

/// A report the user chose to send that has not reached the site yet: their answer, and the
/// report as built at the time (without the account token) when it only has to be sent again.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
    kind: String,
    game: Option<String>,
    outcome: Option<String>,
    comment: Option<String>,
    saved_at: u64,
    /// The game build the answer was about; empty when the launcher could not read it.
    build: String,
    report: Option<serde_json::Value>,
    /// The launcher version whose report the site refused; a later version builds it afresh.
    refused_by: Option<String>,
}

/// How long an undelivered report is kept.
const PENDING_SECS: u64 = 7 * 86400;

fn outbox_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/share")).join("blizznux").join("outbox")
}

fn save_pending(path: &Path, pending: &Pending) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(path, serde_json::to_vec_pretty(pending).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct Submitted {
    pub sent: bool,
    pub url: String,
    /// Why the report was kept for later instead.
    pub reason: String,
}

/// Send a report the user asked for. One that cannot go out now (site unreachable or refusing,
/// game build unknown) is kept in the outbox and tried again on later starts.
#[tauri::command]
pub async fn submit_report(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<Submitted, String> {
    let mut pending = Pending {
        kind: kind.clone(),
        game: game.clone(),
        outcome: outcome.clone(),
        comment: comment.clone(),
        saved_at: now_secs(),
        build: String::new(),
        report: None,
        refused_by: None,
    };
    let reason = match build(kind, game, outcome, comment) {
        Ok(mut body) => match post_report(&body).await {
            Ok(url) => return Ok(Submitted { sent: true, url, reason: String::new() }),
            Err(e) => {
                pending.build = body["game"]["version"].as_str().unwrap_or_default().to_string();
                match e {
                    SendError::Unreachable(why) => {
                        if let Some(fields) = body.as_object_mut() {
                            fields.remove("user_token");
                        }
                        pending.report = Some(body);
                        why
                    }
                    // A refused answer waits for a launcher whose report the site takes; a
                    // refused bug report would carry another log by then.
                    SendError::Refused(why) if pending.kind == "run" => {
                        pending.refused_by = Some(env!("CARGO_PKG_VERSION").into());
                        why
                    }
                    SendError::Refused(why) => return Err(why),
                }
            }
        },
        Err(BuildError::UnknownBuild(why)) => why,
        Err(BuildError::Invalid(why)) => return Err(why),
    };
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    save_pending(&outbox_dir().join(format!("{nanos}.json")), &pending)?;
    Ok(Submitted { sent: false, url: String::new(), reason })
}

/// Try the outbox again (the UI asks once per start); returns how many reports got through.
#[tauri::command]
pub async fn flush_reports() -> usize {
    let mut files: Vec<PathBuf> = fs::read_dir(outbox_dir())
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map_or(false, |x| x == "json")).collect())
        .unwrap_or_default();
    files.sort();
    let mut sent = 0;
    for path in files {
        let discard = || { let _ = fs::remove_file(&path); };
        let Some(mut pending) = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Pending>(&b).ok()) else { discard(); continue };
        if now_secs().saturating_sub(pending.saved_at) > PENDING_SECS {
            discard();
            continue;
        }
        if pending.refused_by.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
            continue;
        }
        let body = match pending.report.take() {
            Some(mut body) => {
                let Some(fields) = body.as_object_mut() else { discard(); continue };
                fields.insert("user_token".into(), load_config().get("USER_TOKEN").cloned().unwrap_or_default().into());
                body
            }
            None => match build(pending.kind.clone(), pending.game.clone(), pending.outcome.clone(), pending.comment.clone()) {
                // The answer was about the build installed then; a game patched since is another one.
                Ok(body) if !pending.build.is_empty() && body["game"]["version"].as_str() != Some(pending.build.as_str()) => { discard(); continue }
                Ok(mut body) => {
                    body["created_at"] = iso(pending.saved_at).into();
                    body
                }
                Err(BuildError::UnknownBuild(_)) => continue,
                Err(BuildError::Invalid(_)) => { discard(); continue }
            },
        };
        match post_report(&body).await {
            Ok(_) => {
                discard();
                sent += 1;
            }
            Err(SendError::Unreachable(_)) => break,
            Err(SendError::Refused(why)) => {
                eprintln!("[report] saved {} report refused: {why}", pending.kind);
                if pending.kind == "run" {
                    pending.refused_by = Some(env!("CARGO_PKG_VERSION").into());
                    let _ = save_pending(&path, &pending);
                } else {
                    discard();
                }
            }
        }
    }
    sent
}

/// Successful automatic reports are sent at most once per day per build; failures always.
/// Returns the stamp to record once the site has accepted the report, or None when it was sent already.
fn daily_stamp(kind: &str, game: Option<&str>, outcome: &str) -> Option<String> {
    if outcome != "ok" {
        return Some(String::new());
    }
    let build = match (kind, game) {
        ("launch", _) => battlenet_info().version,
        ("run", Some(code)) => game_info(code).map(|g| format!("{}:{}", g.code, g.version)).unwrap_or_default(),
        _ => String::new(),
    };
    let stamp = format!("{kind}:{build}:{}", &now_iso()[..10]);
    let key = if kind == "launch" { "LAST_LAUNCH_OK" } else { "LAST_RUN_OK" };
    let c = load_config();
    if c.get(key).map_or(false, |v| v.split(';').any(|s| s == stamp)) {
        return None;
    }
    Some(stamp)
}

/// Record a sent report so the same success is not repeated today; keeps the last few stamps
/// so several games on one day are each sent once.
fn mark_sent(kind: &str, stamp: &str) {
    if stamp.is_empty() {
        return;
    }
    let key = if kind == "launch" { "LAST_LAUNCH_OK" } else { "LAST_RUN_OK" };
    let mut c = load_config();
    let mut stamps: Vec<String> = c.get(key).map(|v| v.split(';').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
    stamps.push(stamp.to_string());
    while stamps.len() > 12 { stamps.remove(0); }
    c.insert(key.into(), stamps.join(";"));
    let _ = save_config(&c);
}

/// Send a report in the background, only when the user linked an account and opted in.
fn auto_report(app: &AppHandle, kind: &str, game: Option<String>, outcome: &str, comment: String) {
    if !sharing_enabled() {
        return;
    }
    let Some(stamp) = daily_stamp(kind, game.as_deref(), outcome) else { return };
    let (kind, outcome) = (kind.to_string(), outcome.to_string());
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        match build_report(kind.clone(), game, Some(outcome.clone()), Some(comment)) {
            Ok(body) => match send_report(body).await {
                Ok(url) => {
                    mark_sent(&kind, &stamp);   // only an accepted report counts for today
                    let _ = h.emit("report-sent", serde_json::json!({ "kind": kind, "outcome": outcome, "url": url }));
                }
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

/// Does this command line name one of the programs? Wine shows the Windows path of the .exe.
fn names_program(first_arg: &str, fragments: &[&str]) -> bool {
    let first = first_arg.to_lowercase();
    fragments.iter().any(|f| first.contains(&f.to_lowercase()))
}

/// Was this process started in the given Wine prefix? umu passes `<prefix>/pfx/`, a link back to
/// the prefix itself, so both sides are resolved before they are compared.
fn runs_in_prefix(pid: u32, prefix: &Path) -> bool {
    let Ok(env) = fs::read(format!("/proc/{pid}/environ")) else { return false };
    let Some(theirs) = env.split(|b| *b == 0).find_map(|kv| kv.strip_prefix(b"WINEPREFIX=")) else { return false };
    let theirs = PathBuf::from(String::from_utf8_lossy(theirs).to_string());
    matches!((fs::canonicalize(theirs), fs::canonicalize(prefix)), (Ok(a), Ok(b)) if a == b)
}

/// Process ids of the given programs, from /proc/*/cmdline. With a prefix, only the ones running
/// in that Wine prefix: a Battle.net started by Steam or Lutris elsewhere is not the launcher's.
fn pids_of(fragments: &[&str], prefix: Option<&Path>) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(rd) = fs::read_dir("/proc") else { return found };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(cmd) = fs::read(e.path().join("cmdline")) else { continue };
        let first = String::from_utf8_lossy(cmd.split(|b| *b == 0).next().unwrap_or(&[])).to_string();
        if names_program(&first, fragments) && prefix.map_or(true, |p| runs_in_prefix(pid, p)) {
            found.push(pid);
        }
    }
    found
}

/// Is one of these programs running, in any prefix?
fn game_running(fragments: &[&str]) -> bool {
    !pids_of(fragments, None).is_empty()
}

/// The start time of a process in clock ticks after boot, from the text of /proc/<pid>/stat.
/// The program name sits in brackets and may itself contain spaces and brackets.
fn start_ticks(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// The program a process runs, as its command line names it (for Wine, the Windows path).
fn process_path(pid: u32) -> String {
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    String::from_utf8_lossy(cmd.split(|b| *b == 0).next().unwrap_or(&[])).to_string()
}

/// Seconds since the process started. The kernel reports 100 clock ticks per second to user
/// space on Linux.
fn process_age(pid: u32) -> Option<u64> {
    let ticks = start_ticks(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let uptime: f64 = fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    Some((uptime as u64).saturating_sub(ticks / 100))
}

const BATTLENET: [&str; 2] = ["Battle.net.exe", "Battle.net Launcher.exe"];

/// What the launcher sees running in its prefix right now.
#[derive(Clone, PartialEq, serde::Serialize)]
pub struct SessionState {
    battlenet: bool,
    /// Name of the game that is running, if any.
    game: Option<String>,
}

static SESSION: std::sync::Mutex<SessionState> = std::sync::Mutex::new(SessionState { battlenet: false, game: None });

#[tauri::command]
pub fn session_state() -> SessionState {
    SESSION.lock().map(|s| s.clone()).unwrap_or(SessionState { battlenet: false, game: None })
}

/// Is Battle.net running in the launcher's prefix, as far as the watcher has seen?
pub fn battlenet_running() -> bool {
    session_state().battlenet
}

/// Watches the launcher's prefix for as long as the launcher is open: is Battle.net running, and
/// which game. Who started them, and when, does not matter. Battle.net left open after a game,
/// or started before the launcher was, is followed just the same, and every game session in it
/// ends with the same question and the same record as the first one. Only looks at local
/// processes; nothing is asked of the site.
pub fn watch_session(app: AppHandle) {
    std::thread::spawn(move || {
        // The game being followed: its entry, the flavour folder it runs from, when it was first
        // seen, and how long it had run by then.
        let mut current: Option<(&(&str, &str, &str, &[&str]), String, Instant, u64)> = None;
        loop {
            let prefix = current_prefix();
            let battlenet = !pids_of(&BATTLENET, Some(&prefix)).is_empty();
            let ended = matches!(&current, Some((g, ..)) if pids_of(g.3, Some(&prefix)).is_empty());
            if ended {
                if let Some((g, flavor, seen, age)) = current.take() {
                    let secs = age + seen.elapsed().as_secs();
                    let id = game_id(g.0, &flavor);
                    let (event, record_ok) = session_end(secs);
                    if is_reported(g.0, &flavor) {
                        if record_ok {
                            auto_report(&app, "run", Some(id.clone()), "ok", format!("ran for {} min", secs / 60));
                        }
                        let _ = app.emit(event, serde_json::json!({ "code": id, "name": game_name(g, &flavor), "seconds": secs }));
                    }
                }
            } else if current.is_none() {
                current = GAMES.iter().find_map(|g| {
                    let pid = pids_of(g.3, Some(&prefix)).into_iter().min()?;
                    Some((g, flavor_of(&process_path(pid)), Instant::now(), process_age(pid).unwrap_or(0)))
                });
            }
            let now = SessionState { battlenet, game: current.as_ref().map(|c| game_name(c.0, &c.1).to_string()) };
            let changed = SESSION.lock().map(|mut s| if *s != now { *s = now.clone(); true } else { false }).unwrap_or(false);
            if changed {
                let _ = app.emit("session-state", &now);
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// What the end of a game session means: the event the UI gets, and whether a successful run is
/// recorded automatically (when sharing). A session of under a minute proves nothing either way:
/// a lost connection to Blizzard, a login queue and the user quitting all look the same as a
/// crash. So it is never reported by itself; the UI asks instead.
fn session_end(secs: u64) -> (&'static str, bool) {
    if secs < 60 { ("game-closed-early", false) } else { ("game-ended", true) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_time_is_read_past_an_awkward_program_name() {
        let stat = "4242 (Battle.net (x86) .exe) S 1 4242 4242 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 9 0 987654 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(start_ticks(stat), Some(987654));
        assert_eq!(start_ticks("garbage"), None);
    }

    #[test]
    fn programs_are_recognised_by_their_windows_path() {
        let overwatch = GAMES.iter().find(|g| g.0 == "Pro").unwrap().3;
        assert!(names_program(r"C:\Program Files (x86)\Overwatch\_retail_\Overwatch.exe", overwatch));
        assert!(names_program(r"C:\Program Files (x86)\Battle.net\Battle.net.exe", &BATTLENET));
        assert!(!names_program(r"C:\Program Files (x86)\Battle.net\Battle.net-Setup.exe", &BATTLENET));
        assert!(!names_program("/usr/bin/sleep", overwatch));
    }

    fn wow_rows() -> Vec<BTreeMap<String, String>> {
        [("wow", "12.1.0.69933"), ("wow_classic_era", "1.15.9.70003")]
            .iter()
            .map(|(product, version)| BTreeMap::from([("Active".to_string(), "1".to_string()), ("Product".to_string(), product.to_string()), ("Version".to_string(), version.to_string())]))
            .collect()
    }

    #[test]
    fn the_flavour_comes_from_the_folder_the_game_ran_from() {
        assert_eq!(flavor_of(r"C:\Program Files (x86)\World of Warcraft\_classic_era_\WowClassic.exe"), "_classic_era_");
        assert_eq!(flavor_of(r"C:\Program Files (x86)\World of Warcraft\_retail_\WoW.exe"), "_retail_");
        assert_eq!(flavor_of(r"C:\Program Files (x86)\Hearthstone\Hearthstone.exe"), "");
        assert_eq!(flavor_of("Overwatch.exe"), "");
    }

    #[test]
    fn each_wow_flavour_gets_its_own_build_and_never_anothers() {
        let rows = wow_rows();
        let version = |flavor| build_row(&rows, "WoW", flavor).and_then(|r| r.get("Version")).cloned();
        assert_eq!(version("_retail_").as_deref(), Some("12.1.0.69933"));
        assert_eq!(version("_classic_era_").as_deref(), Some("1.15.9.70003"));
        assert_eq!(version("_classic_"), None);   // not installed: no build, not Retail's
        assert_eq!(version("").as_deref(), Some("12.1.0.69933"));   // flavour unknown: as before
        assert_eq!(wow_product("WoW", "_ptr_").as_deref(), Some("wowt"));
        assert_eq!(wow_product("WoW", "_classic_era_ptr_").as_deref(), Some("wow_classic_era_ptr"));
        assert_eq!(wow_product("Pro", "_retail_"), None);
    }

    #[test]
    fn only_the_live_wow_products_are_reported() {
        for live in ["", "_retail_", "_classic_", "_classic_era_"] {
            assert!(is_reported("WoW", live));
        }
        for other in ["_ptr_", "_xptr_", "_beta_", "_classic_ptr_", "_classic_era_ptr_", "_anniversary_"] {
            assert!(!is_reported("WoW", other));
        }
        assert!(is_reported("Pro", "_retail_"));
        assert!(is_reported("WTCG", ""));
    }

    #[test]
    fn classic_clients_are_named_as_the_site_files_them() {
        let wow = game_by_code("WoW").unwrap();
        assert_eq!(game_name(wow, "_classic_era_"), "WoW Classic Era");
        assert_eq!(game_name(wow, "_classic_"), "WoW Classic");
        assert_eq!(game_name(wow, "_retail_"), "World of Warcraft");
        assert_eq!(split_game("WoW/_classic_era_"), ("WoW", "_classic_era_"));
        assert_eq!(split_game("Pro"), ("Pro", ""));
        assert_eq!(game_id("WoW", "_classic_era_"), "WoW/_classic_era_");
        assert_eq!(game_id("WTCG", ""), "WTCG");
    }

    #[test]
    fn only_a_clear_no_ends_the_link() {
        assert_eq!(check_outcome(200), "linked");
        assert_eq!(check_outcome(401), "revoked");
        for other in [404, 429, 500, 503] {
            assert_eq!(check_outcome(other), "unknown");
        }
    }

    #[test]
    fn a_busy_site_is_waited_out() {
        assert_eq!(busy_wait(429, Some(12)), Some(12));
        assert_eq!(busy_wait(429, None), Some(30));
        assert_eq!(busy_wait(429, Some(0)), Some(30));
        assert_eq!(busy_wait(429, Some(86_400)), Some(600));
        assert_eq!(busy_wait(503, None), Some(30));
        for answered in [200, 201, 202, 404, 410] {
            assert_eq!(busy_wait(answered, Some(5)), None);
        }
    }

    #[test]
    fn a_short_session_is_never_reported_by_itself() {
        assert_eq!(session_end(0), ("game-closed-early", false));
        assert_eq!(session_end(59), ("game-closed-early", false));
        assert_eq!(session_end(60), ("game-ended", true));
    }

    #[test]
    fn product_db_gives_the_installed_version() {
        let version = b"36.6.3.253932.253216";
        let mut base = vec![0x08, 1, 0x10, 1, 0x18, 1, 0x20, 0, 0x28, 0, 0x3a, version.len() as u8];
        base.extend_from_slice(version);
        let mut cached = vec![0x0a, base.len() as u8];
        cached.extend_from_slice(&base);
        let mut db = b"\x0a\x07hs_beta\x12\x03hsb".to_vec();
        db.extend_from_slice(&[0x22, cached.len() as u8]);
        db.extend_from_slice(&cached);
        assert_eq!(product_db_version(&db).as_deref(), Some("36.6.3.253932.253216"));
        assert_eq!(product_db_version(&db[..db.len() - 5]), None);
        assert_eq!(product_db_version(b"\x0a\x07hs_beta"), None);
    }

    /// A protobuf length-delimited field (short payloads only).
    fn pb(field: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![field << 3 | 2, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn agent_db_lists_products_with_their_install_paths() {
        let install = |uid: &str, product: &str, path: &str| pb(1, &[pb(1, uid.as_bytes()), pb(2, product.as_bytes()), pb(3, &pb(1, path.as_bytes()))].concat());
        let mut db = install("heroes_ptr", "herot", "C:/Program Files (x86)/Heroes of the Storm Public Test");
        db.extend(install("heroes", "hero", "D:\\Games\\Heroes of the Storm"));
        db.extend(install("hs_beta", "hsb", "C:/Games/Hearthstone"));
        db.extend(install("wow_classic", "wow_classic", "C:/Games/World of Warcraft"));
        db.extend([0x30, 0x01]); // a varint field after the installs
        let installs = agent_installs(&db);
        assert_eq!(installs.len(), 4);
        assert_eq!(install_path(&installs, "Hero"), Some("D:\\Games\\Heroes of the Storm"));
        assert_eq!(install_path(&installs, "WTCG"), Some("C:/Games/Hearthstone"));
        assert_eq!(install_path(&installs, "WoW"), Some("C:/Games/World of Warcraft"));
        assert_eq!(install_path(&installs, "Pro"), None);
    }

    #[test]
    fn windows_paths_go_through_the_drive_links() {
        let prefix = Path::new("/pfx");
        assert_eq!(host_path(prefix, "C:/Games/Hearthstone"), Some(PathBuf::from("/pfx/dosdevices/c:/Games/Hearthstone")));
        assert_eq!(host_path(prefix, "D:\\Games\\Heroes of the Storm"), Some(PathBuf::from("/pfx/dosdevices/d:/Games/Heroes of the Storm")));
        assert_eq!(host_path(prefix, "Games/Hearthstone"), None);
    }

    #[test]
    fn identical_lines_fold_into_one() {
        let out = condense_log("start\nsame\nsame\nsame\nsame\nend\n");
        assert_eq!(out, vec!["start", "(×4) same", "end"]);
    }

    #[test]
    fn a_pair_is_left_alone() {
        let out = condense_log("a\nb\nb\nc\n");
        assert_eq!(out, vec!["a", "b", "b", "c"]);
    }

    #[test]
    fn gstreamer_burst_becomes_one_plain_line() {
        let mut log = String::from("Proton: starting\n");
        for (i, name) in ["accurip", "adaptivedemux2", "alaw", "app"].iter().enumerate() {
            log.push_str(&format!(
                "(wine:253103): GStreamer-WARNING **: 19:32:01.98{i}: Failed to load plugin '/p/x86_64-linux-gnu/gstreamer-1.0/libgst{name}.so': /p/x86_64-linux-gnu/gstreamer-1.0/libgst{name}.so: wrong ELF class: ELFCLASS64\r\n\r\n"
            ));
        }
        log.push_str("err: the real problem\n");
        let out = condense_log(&log);
        assert_eq!(
            out,
            vec![
                "Proton: starting",
                "(×4) GStreamer skipped a plugin built for the other CPU architecture (harmless)",
                "err: the real problem",
            ]
        );
    }

    #[test]
    fn lines_differing_in_numbers_and_paths_fold() {
        let out = condense_log("fixme at 0x00a1 in /a/b.so pid 12\nfixme at 0x7f in /c/d.so pid 3456\nfixme at 0x0 in /e.so pid 7\n");
        assert_eq!(out, vec!["(×3) fixme at 0x00a1 in /a/b.so pid 12"]);
    }

    #[test]
    fn repeated_block_is_kept_once() {
        let block = "Unhandled exception in Xalia:\nTaskCanceledException: A task was canceled.\n  at Xalia.Utils.DoRunTask\n";
        let out = condense_log(&format!("before\n{block}{block}{block}after\n"));
        assert_eq!(
            out,
            vec![
                "before",
                "(×3, the next 3 lines)",
                "Unhandled exception in Xalia:",
                "TaskCanceledException: A task was canceled.",
                "  at Xalia.Utils.DoRunTask",
                "after",
            ]
        );
    }
}
