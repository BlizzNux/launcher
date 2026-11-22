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
        // The pairing API answers on blizznux.com itself; a redirect is not an answer.
        .redirect(reqwest::redirect::Policy::none())
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
    // The page must not load before the cookie is in place: without it the site sees no pairing
    // (or the previous one, whose login box was already shown) and stays quiet.
    let stored = set_pair_cookie(&app, Some(pair_id.clone()))?;
    let _ = tauri::async_runtime::spawn_blocking(move || stored.recv_timeout(Duration::from_secs(2))).await;
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
/// can trust it when a logged-in page asks to approve; a crafted link cannot plant it. The
/// cookie store works on its own time: the receiver hears once the change has been made (or
/// hangs up when it could not be).
fn set_pair_cookie(app: &AppHandle, value: Option<String>) -> Result<std::sync::mpsc::Receiver<()>, String> {
    let main = app.get_webview_window("main").ok_or("main window missing")?;
    let host = site_host();
    let (done, stored) = std::sync::mpsc::channel();
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
                cm.delete_cookie(&mut cookie, None::<&webkit2gtk::gio::Cancellable>, move |_| { let _ = done.send(()); });
            } else {
                cm.add_cookie(&mut cookie, None::<&webkit2gtk::gio::Cancellable>, move |_| { let _ = done.send(()); });
            }
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(stored)
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
    runtime: String,
    prefix_kind: String,
}

/// The graphics cards, looked up once per launcher run. Asking lspci wakes a sleeping NVIDIA
/// card on a laptop, which took seconds every time a report was built; the cards do not change
/// while the launcher runs.
fn gpus() -> Vec<Gpu> {
    static GPUS: std::sync::OnceLock<Vec<Gpu>> = std::sync::OnceLock::new();
    GPUS.get_or_init(probe_gpus).clone()
}

fn probe_gpus() -> Vec<Gpu> {
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

/// The Proton build in a folder of builds: the newest one that is complete. The wrapper unpacks
/// the launcher's own build there and removes the one before it.
fn proton_under(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("proton").is_file())
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// What the wrapper is doing in that folder right now, if it is fetching a build: it downloads
/// to a `.part` file and unpacks in `.unpack`. One that was left behind long ago does not count.
fn proton_arriving(dir: &Path) -> Option<String> {
    let fresh = |p: &Path, secs: u64| fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(false, |age| age < Duration::from_secs(secs));
    if fresh(&dir.join(".unpack"), 600) {
        return Some("Unpacking Proton. Battle.net starts when that is done.".into());
    }
    let part = fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().map_or(false, |x| x == "part") && fresh(p, 60))?;
    let mb = fs::metadata(&part).map(|m| m.len() / 1_048_576).unwrap_or(0);
    Some(format!("Downloading Proton, {mb} MB so far. Battle.net starts when that is done."))
}

fn proton_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/share")).join("blizznux").join("proton")
}

/// The launcher's own Proton, which the wrapper downloads into the data folder on first use.
pub(crate) fn own_proton() -> Option<PathBuf> {
    proton_under(&proton_dir())
}

/// The Steam Linux Runtime in a folder of runtimes, as its own VERSIONS.txt names it:
/// "steamrt4 4.0.20260928.262390". With several there, the one that was set up or updated last.
fn runtime_under(dir: &Path) -> String {
    let newest = fs::read_dir(dir).ok().and_then(|rd| {
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("steamrt")) && p.join("VERSIONS.txt").is_file())
            .max_by_key(|p| fs::metadata(p.join("VERSIONS.txt")).and_then(|m| m.modified()).ok())
    });
    let Some(rt) = newest else { return String::new() };
    let text = fs::read_to_string(rt.join("VERSIONS.txt")).unwrap_or_default();
    let version = text
        .lines()
        .find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next() == Some("depot")).then(|| words.next().unwrap_or("").to_string())
        })
        .unwrap_or_default();
    format!("{} {version}", rt.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).trim().to_string()
}

/// The runtime umu runs Proton in. umu fetches the newest one by itself, so unlike Proton and
/// umu it is not the same on every machine; the reports say which one a machine has.
fn runtime_in_use() -> String {
    let base = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".local/share"));
    runtime_under(&base.join("umu"))
}

/// A Proton build chosen in Labs (a folder) or by the packager (BLIZZNUX_PROTON, which wins).
pub(crate) fn custom_proton() -> Option<PathBuf> {
    custom_proton_from(&load_config(), std::env::var("BLIZZNUX_PROTON").ok().as_deref())
}

fn custom_proton_from(config: &BTreeMap<String, String>, env: Option<&str>) -> Option<PathBuf> {
    let pick = |s: &str| Some(s.trim().trim_end_matches('/')).filter(|s| !s.is_empty()).map(PathBuf::from);
    env.and_then(pick).or_else(|| config.get("LAB_PROTON_DIR").and_then(|v| pick(v)))
}

/// How a Proton folder is named in reports: its version file's last word (the build name), else the
/// folder's name.
fn proton_label(dir: &Path) -> String {
    let ver = fs::read_to_string(dir.join("version")).unwrap_or_default();
    let ver = ver.split_whitespace().last().unwrap_or("").to_string();
    if ver.is_empty() { dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { ver }
}

/// The build Battle.net is started with: the Proton chosen in Labs if any, else the launcher's own.
fn proton_in_use() -> String {
    if let Some(d) = custom_proton() {
        return if d.join("proton").is_file() { proton_label(&d) } else { "chosen Proton not found".into() };
    }
    match own_proton() {
        Some(d) => proton_label(&d),
        None => "not downloaded yet".into(),
    }
}

/// MemTotal is what is left after firmware and iGPU reservations (a 32 GB
/// machine reports ~31.3 GiB), so round up to the next common module size.
fn installed_ram_gb(mem_total_kb: u64) -> u64 {
    const SIZES: [u64; 12] = [2, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128];
    let gib = mem_total_kb as f64 / (1024.0 * 1024.0);
    SIZES
        .iter()
        .copied()
        .find(|&s| s as f64 >= gib)
        .unwrap_or_else(|| (gib / 64.0).ceil() as u64 * 64)
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
        session: session_label(std::env::var("XDG_SESSION_TYPE").unwrap_or_default(), std::env::var_os("BLIZZNUX_GL_FALLBACK").is_some()),
        cpu,
        ram_gb: installed_ram_gb(ram_kb),
        gpus: gpus(),
        proton: proton_in_use(),
        umu,
        runtime: runtime_in_use(),
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

/// The folder a World of Warcraft product is installed in: the reverse of `wow_product`.
fn wow_folder(product: &str) -> String {
    match product {
        "wow" => "_retail_".into(),
        "wowt" => "_ptr_".into(),
        "wowxptr" => "_xptr_".into(),
        other => other.strip_prefix("wow_").map(|inner| format!("_{inner}_")).unwrap_or_default(),
    }
}

/// Which World of Warcraft version a report is about: (folder, product). `asked` is the folder
/// the game ran from and `recorded` what that folder's `.flavor.info` says. When the folder is
/// not known (an answer saved by an older launcher, a command line), only an install with a
/// single version leaves no doubt; with several, nothing is chosen and no build is reported.
fn resolve_wow(rows: &[BTreeMap<String, String>], asked: &str, recorded: Option<String>) -> (String, String) {
    if !asked.is_empty() {
        return (asked.to_string(), recorded.or_else(|| wow_product("WoW", asked)).unwrap_or_default());
    }
    match rows {
        [only] => {
            let product = only.get("Product").cloned().unwrap_or_default();
            (wow_folder(&product), product)
        }
        _ => (String::new(), String::new()),
    }
}

/// The product a World of Warcraft folder belongs to, as Blizzard records it in the folder's own
/// `.flavor.info` (a header line, then the product name).
fn flavor_product(root: &Path, flavor: &str) -> Option<String> {
    let text = fs::read_to_string(root.join(flavor).join(".flavor.info")).ok()?;
    text.lines().map(str::trim).filter(|l| !l.is_empty()).nth(1).map(String::from)
}

/// The `.build.info` row for what actually ran. World of Warcraft keeps every version in one
/// folder with one row per product, so with a product the row is that product's; when it has no
/// row there is no version, rather than another version's. Other games have a single product.
fn build_row<'a>(rows: &'a [BTreeMap<String, String>], product: Option<&str>) -> Option<&'a BTreeMap<String, String>> {
    match product {
        Some(product) => rows.iter().find(|r| r.get("Product").map_or(false, |p| p == product)),
        None => rows.iter().find(|r| r.get("Active").map_or(false, |a| a == "1")).or_else(|| rows.first()),
    }
}

/// Is a session of this game reported at all? Everything is, except World of Warcraft's test and
/// beta clients. Which of the live versions get a thread is the site's decision, not the
/// launcher's: it files the ones its update bot follows and only records the rest.
fn is_reported(code: &str, flavor: &str) -> bool {
    code != "WoW" || !(flavor.contains("ptr") || flavor.contains("beta"))
}

/// The name the site files a game under. World of Warcraft's other versions are named after
/// their folder: `_classic_era_` → "WoW Classic Era", `_anniversary_` → "WoW Anniversary".
fn game_name(g: &(&'static str, &'static str, &'static str, &'static [&'static str]), flavor: &str) -> String {
    if g.0 != "WoW" || matches!(flavor, "" | "_retail_") {
        return g.1.to_string();
    }
    let words: Vec<String> = flavor
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut rest = w.chars();
            rest.next().map(|first| first.to_uppercase().chain(rest).collect()).unwrap_or_default()
        })
        .collect();
    format!("WoW {}", words.join(" "))
}

pub fn game_info(id: &str) -> Option<GameInfo> {
    let (code, ran_from) = split_game(id);
    let g = game_by_code(code)?;
    let root = game_root(&current_prefix(), g);
    let rows = build_info(&root.join(".build.info")).unwrap_or_default();
    // World of Warcraft has several versions in one folder; every other game has one product.
    let wow = (g.0 == "WoW").then(|| resolve_wow(&rows, ran_from, flavor_product(&root, ran_from)));
    let row = build_row(&rows, wow.as_ref().map(|(_, product)| product.as_str()));
    let version = row.and_then(|r| r.get("Version")).filter(|v| !v.is_empty()).cloned();
    // Hearthstone has no `.build.info`; its version only lives in `.product.db`. That file is the
    // folder's main product, so it cannot stand in for a World of Warcraft version.
    let version = match version {
        Some(v) => v,
        None if wow.is_some() => String::new(),
        None => fs::read(root.join(".product.db")).ok().and_then(|b| product_db_version(&b)).unwrap_or_default(),
    };
    // Only World of Warcraft has flavours as far as reports go; the field stays empty for the rest.
    let flavor = wow.map(|(folder, _)| folder).unwrap_or_default();
    Some(GameInfo { code: g.0.into(), name: game_name(g, &flavor), flavor, version })
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
        if let Ok(body) = report_body("bug".into(), None, None, Some(comment)) {
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
        forget_link()?;
        return Ok(LinkCheck { status: status.into(), username: String::new() });
    }
    Ok(LinkCheck { status: status.into(), username })
}

/// Drop a link the site has ended. The sharing choices stay, so logging in again brings
/// everything back without a question.
fn forget_link() -> Result<(), String> {
    let mut c = load_config();
    if let Some(u) = c.remove("USERNAME") {
        c.insert("LAST_USERNAME".into(), u);
    }
    c.remove("USER_TOKEN");
    save_config(&c)
}

fn user_token() -> String {
    load_config().get("USER_TOKEN").cloned().unwrap_or_default()
}

/// The account the link belongs to, by name. The outbox records only this, never the token, and
/// nothing is recorded while no token is stored (logged out, or a link ended by the site).
fn linked_account() -> Option<String> {
    account_in(&load_config())
}

/// The account linked now, and the one whose link was forgotten before it.
fn linked_accounts() -> (Option<String>, Option<String>) {
    let c = load_config();
    (account_in(&c), last_account_in(&c))
}

/// `LAST_USERNAME` is written only when a link is forgotten, so an empty one means no other
/// account was linked here before.
fn last_account_in(config: &BTreeMap<String, String>) -> Option<String> {
    config.get("LAST_USERNAME").map(|n| n.trim()).filter(|n| !n.is_empty()).map(String::from)
}

/// A link this launcher made always stores the name with the token. A token without one (an
/// edited config, or a link from before names were kept) reads as no account: saved reports then
/// wait rather than go out under a name nobody can check.
fn account_in(config: &BTreeMap<String, String>) -> Option<String> {
    let linked = config.get("USER_TOKEN").map_or(false, |t| !t.is_empty());
    let name = config.get("USERNAME").map(|n| n.trim()).unwrap_or_default();
    if linked && !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

/// Reports about something that went wrong. They carry the end of the launcher log.
fn is_problem(kind: &str, outcome: Option<&str>) -> bool {
    match kind {
        "bug" => true,
        "launch" => outcome == Some("failed"),
        "run" => matches!(outcome, Some("issues" | "broken")),
        _ => false,
    }
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

/// A session with fewer lines than this gets the one before it as context.
const SHORT_SESSION: usize = 40;

/// Each start of the launcher writes `=== <time> ===` to the log.
fn is_header(line: &str) -> bool {
    let t = line.trim();
    t.len() > 6 && t.starts_with("=== ") && t.ends_with(" ===")
}

/// The part of the log that matters: from the last start header to the end, plus the session
/// before it when the last one is short. A log with no header is returned as it is.
pub(crate) fn recent_sessions(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let heads: Vec<usize> = (0..lines.len()).filter(|&i| is_header(lines[i])).collect();
    let Some(&last) = heads.last() else { return log.to_string() };
    let filled = |from: usize, to: usize| lines[from..to].iter().filter(|l| !l.trim().is_empty()).count();
    let from = if filled(last, lines.len()) < SHORT_SESSION && heads.len() > 1 { heads[heads.len() - 2] } else { last };
    lines[from..].join("\n")
}

/// The end of the launcher log as attached to a report: the last session (and the one before
/// it when short), folded, then cut, then scrubbed.
fn log_tail(max_lines: usize) -> String {
    let log = fs::read_to_string(crate::log_path()).unwrap_or_default();
    let lines = condense_log(&recent_sessions(&log));
    scrub(&lines[lines.len().saturating_sub(max_lines)..].join("\n"))
}

/// How far back a crash file may be dated when the session's start is not known, and how much
/// of one rides along.
const CRASH_WINDOW_SECS: u64 = 6 * 3600;
const CRASH_LINES: usize = 150;
const CRASH_BYTES: usize = 12 * 1024;

/// Takes the Windows user and computer name out of a crash file: lines that state them, and the
/// user folder in any path.
pub(crate) fn redact_crash(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            let key = l.split_once(':').map(|(k, _)| k.trim().to_ascii_lowercase()).unwrap_or_default();
            if matches!(key.as_str(), "computer" | "computer name" | "computername" | "user" | "user name" | "username" | "machine" | "host" | "host name") {
                return format!("{}: (removed)", l.split_once(':').map(|(k, _)| k.trim()).unwrap_or_default());
            }
            let mut out = String::with_capacity(l.len());
            let mut rest = l;
            while let Some(i) = rest.to_ascii_lowercase().find("users\\") {
                let after = i + "users\\".len();
                out.push_str(&rest[..after]);
                let name_end = rest[after..].find(|c: char| c == '\\' || c == '/' || c.is_whitespace() || c == '"' || c == '\'').map_or(rest.len(), |n| after + n);
                out.push('~');
                rest = &rest[name_end..];
            }
            out.push_str(rest);
            out
        })
        .collect();
    lines.join("\n")
}

/// The last CRASH_LINES lines and CRASH_BYTES bytes of a crash file, redacted and scrubbed.
pub(crate) fn crash_excerpt(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = scrub(&redact_crash(&lines[lines.len().saturating_sub(CRASH_LINES)..].join("\n")));
    if out.len() > CRASH_BYTES {
        let mut cut = out.len() - CRASH_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = out[cut..].to_string();
    }
    out
}

/// The newest `.txt` in a WoW `Errors` folder that is no older than `max_age` seconds at `now`.
pub(crate) fn newest_crash(errors: &Path, now: SystemTime, max_age: u64) -> Option<PathBuf> {
    fs::read_dir(errors)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(m, _)| now.duration_since(*m).map_or(true, |age| age.as_secs() <= max_age))
        .max_by_key(|(m, _)| *m)
        .map(|(_, p)| p)
}

/// WoW's own crash report for the version a report is about, when one is fresh.
fn crash_file(code: &str, flavor: &str) -> Option<String> {
    if code != "WoW" {
        return None;
    }
    let flavor = if flavor.is_empty() { "_retail_" } else { flavor };
    let g = game_by_code(code)?;
    let errors = game_root(&current_prefix(), g).join(flavor).join("Errors");
    let path = newest_crash(&errors, SystemTime::now(), CRASH_WINDOW_SECS)?;
    let text = String::from_utf8_lossy(&fs::read(path).ok()?).to_string();
    let out = crash_excerpt(&text);
    (!out.trim().is_empty()).then_some(out)
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
#[derive(Debug)]
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

/// The Labs settings that are on, as the keys the site knows them by (sorted).
fn labs_in(config: &BTreeMap<String, String>) -> Vec<String> {
    labs_from(config, std::env::var("BLIZZNUX_PROTON").ok().as_deref(), std::env::var("PROTON_USE_WOW64").ok().as_deref())
}

/// `env_wow64` is PROTON_USE_WOW64 from the environment: set to 1 by hand it is the wow64 Labs
/// setting too, whether or not the Labs toggle is on.
fn labs_from(config: &BTreeMap<String, String>, env_proton: Option<&str>, env_wow64: Option<&str>) -> Vec<String> {
    let mut lab = Vec::new();
    if config.get("LAB_BNET_FPS").is_some_and(|v| v == "1") {
        lab.push("bnetfps".to_string());
    }
    if config.get("LAB_DIRECT_PROTON").is_some_and(|v| v == "1") {
        lab.push("direct".to_string());
    }
    if custom_proton_from(config, env_proton).is_some() {
        lab.push("proton".to_string());
    }
    if config.get("LAB_WOW64").is_some_and(|v| v == "1") || env_wow64 == Some("1") {
        lab.push("wow64".to_string());
    }
    lab
}

/// The keys in a BLIZZNUX_LAB value (comma separated), sorted.
fn parse_lab(value: &str) -> Vec<String> {
    let mut lab: Vec<String> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    lab.sort();
    lab.dedup();
    lab
}

/// Adds `lab` to a report body, only when a Labs setting was on: other reports stay as they were.
fn mark_lab(body: &mut serde_json::Value, lab: &[String]) {
    if !lab.is_empty() {
        body["lab"] = serde_json::json!(lab);
    }
}

/// The Labs settings in effect for the latest Battle.net launch or game session, captured when it
/// started: a run or launch report is about that session, whatever the setting is when it is sent.
static SESSION_LAB: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

fn set_session_lab(lab: Vec<String>) {
    if let Ok(mut s) = SESSION_LAB.lock() {
        *s = Some(lab);
    }
}

/// The Labs settings a running program was started with: the wrapper puts them in BLIZZNUX_LAB
/// (comma separated). A program whose environment cannot be read falls back to the setting now.
fn lab_of_pid(pid: u32) -> Vec<String> {
    match fs::read(format!("/proc/{pid}/environ")) {
        Ok(env) => env
            .split(|b| *b == 0)
            .find_map(|kv| kv.strip_prefix(b"BLIZZNUX_LAB="))
            .map(|v| parse_lab(&String::from_utf8_lossy(v)))
            .unwrap_or_default(),
        Err(_) => labs_in(&load_config()),
    }
}

/// The Labs settings a report of this kind is about: the session's for run and launch reports.
fn report_lab(kind: &str) -> Vec<String> {
    match (kind, SESSION_LAB.lock().ok().and_then(|s| s.clone())) {
        ("run" | "launch", Some(session)) => session,
        _ => labs_in(&load_config()),
    }
}

/// The once-a-day stamp for a build: a Labs session gets its own, so it and a normal success
/// on the same day do not swallow each other. Without Labs the stamp is the plain one.
fn stamp_for(kind: &str, build: &str, date: &str, lab: &[String]) -> String {
    let mut stamp = format!("{kind}:{build}:{date}");
    if !lab.is_empty() {
        let mut keys = lab.to_vec();
        keys.sort();
        stamp.push_str(&format!(":lab={}", keys.join(",")));
    }
    stamp
}

/// Build the JSON body for a report; the UI shows it before sending. Off the window's thread:
/// the system profile runs a few programs, and the window must not freeze meanwhile.
#[tauri::command]
pub async fn build_report(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || report_body(kind, game, outcome, comment)).await.map_err(|e| e.to_string())?
}

pub fn report_body(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, String> {
    build(kind, game, outcome, comment).map_err(BuildError::text)
}

/// What the machine itself contributes to a report body: the profile, the builds, the log and
/// WoW's crash file. `live` fills it from the machine; a test stands in for it, so the fields a
/// report carries (REPORTING.md) can be checked without a prefix, a log or a graphics card.
struct Parts {
    install_id: String,
    user_token: String,
    created_at: String,
    system: SystemProfile,
    battlenet: BattlenetInfo,
    game: Option<GameInfo>,
    lab: Vec<String>,
    log: Option<String>,
    crash: Option<String>,
}

/// The machine's own part of a report: probed here, and only what the type needs. The log and the
/// crash file go with problem reports only, so an `ok` report never reads them.
fn live(kind: &str, game: Option<&str>, outcome: Option<&str>) -> Parts {
    let game = game.filter(|c| !c.is_empty()).and_then(game_info);
    let problem = is_problem(kind, outcome);
    // A failed launch carries the last 120 log lines, other problem reports the last 200.
    let log = problem.then(|| log_tail(if kind == "launch" { 120 } else { 200 }));
    let crash = match &game {
        Some(g) if problem => crash_file(&g.code, &g.flavor),
        _ => None,
    };
    Parts {
        install_id: install_id(),
        user_token: user_token(),
        created_at: now_iso(),
        system: system_profile(),
        battlenet: battlenet_info(),
        game,
        lab: report_lab(kind),
        log,
        crash,
    }
}

/// Fill a report body from the request and the machine's part of it. Every field here is in
/// REPORTING.md.
fn assemble(kind: &str, outcome: Option<&str>, comment: Option<&str>, parts: Parts) -> Result<serde_json::Value, BuildError> {
    let invalid = |why: &str| BuildError::Invalid(why.into());
    if !matches!(kind, "bug" | "run" | "launch") {
        return Err(invalid("kind must be bug, run or launch"));
    }
    let mut body = serde_json::json!({
        "schema": 1,
        "type": kind,
        "install_id": parts.install_id,
        "user_token": parts.user_token,
        "launcher_version": env!("CARGO_PKG_VERSION"),
        "created_at": parts.created_at,
        "system": parts.system,
        "battlenet": parts.battlenet,
    });
    mark_lab(&mut body, &parts.lab);
    if let Some(g) = &parts.game {
        body["game"] = serde_json::to_value(g).map_err(|e| invalid(&e.to_string()))?;
    }
    // The log and the crash file belong to problem reports: `bug`, and `run` with `issues` or
    // `broken`. A launch has no game, so it never carries a crash file.
    let problem = is_problem(kind, outcome);
    if kind == "launch" {
        let o = outcome.unwrap_or_default();
        if !matches!(o, "ok" | "failed") {
            return Err(invalid("launch outcome must be ok or failed"));
        }
        body["target"] = serde_json::Value::String("battlenet".into());
        body["outcome"] = serde_json::Value::String(o.into());
        if problem {
            body["log"] = serde_json::Value::String(parts.log.unwrap_or_default());
        }
    } else if kind == "run" {
        let o = outcome.unwrap_or_default();
        if !matches!(o, "perfect" | "issues" | "broken" | "ok") {
            return Err(invalid("outcome must be perfect, issues, broken or ok"));
        }
        let (code, flavor) = (body["game"]["code"].as_str().unwrap_or_default(), body["game"]["flavor"].as_str().unwrap_or_default());
        if !is_reported(code, flavor) {
            return Err(invalid("sessions on test and beta realms are not reported"));
        }
        if body["game"]["version"].as_str().map_or(true, str::is_empty) {
            let name = parts.game.as_ref().map(|g| g.name.clone()).unwrap_or_else(|| "the game".to_string());
            return Err(BuildError::UnknownBuild(format!(
                "The launcher cannot tell which build of {name} is installed, so the site has nothing to file this report under. Please tell BlizzNux through \"Report a problem\" in Settings."
            )));
        }
        if problem {
            body["log"] = serde_json::Value::String(parts.log.unwrap_or_default());
            if let Some(c) = parts.crash {
                body["crash"] = serde_json::Value::String(c);
            }
        }
        body["outcome"] = serde_json::Value::String(o.into());
        body["comment"] = serde_json::Value::String(scrub(comment.unwrap_or_default()).chars().take(2000).collect());
    } else {
        body["log"] = serde_json::Value::String(parts.log.unwrap_or_default());
        if let Some(c) = parts.crash {
            body["crash"] = serde_json::Value::String(c);
        }
        if let Some(c) = comment.filter(|c| !c.is_empty()) {
            body["comment"] = serde_json::Value::String(scrub(c).chars().take(2000).collect());
        }
    }
    Ok(body)
}

fn build(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<serde_json::Value, BuildError> {
    let parts = live(&kind, game.as_deref(), outcome.as_deref());
    assemble(&kind, outcome.as_deref(), comment.as_deref(), parts)
}

/// What a request does with what `build` produced: send the body, or tell the user why nothing
/// was built. A report that cannot be built is never kept: one that names no game build would be
/// dropped by the next flush anyway, so saving it would only promise something that cannot come.
#[derive(Debug, PartialEq)]
enum BuildOutcome {
    Send(serde_json::Value),
    Error(String),
}

fn build_outcome(built: Result<serde_json::Value, BuildError>) -> BuildOutcome {
    match built {
        Ok(body) => BuildOutcome::Send(body),
        Err(BuildError::UnknownBuild(_)) => BuildOutcome::Error(UNKNOWN_BUILD_NOT_SAVED.into()),
        Err(BuildError::Invalid(why)) => BuildOutcome::Error(why),
    }
}

/// What the user is told when the launcher cannot tell which build a report is about.
const UNKNOWN_BUILD_NOT_SAVED: &str = "The game build could not be read, so this report was not saved. Try again after the game has started once.";

/// Why a report was not delivered: the site refused this report, the site takes reports only
/// from a logged-in launcher, the site could not take any right now (offline, rate limit,
/// server trouble), which is worth another try later, or the report is too large for it even
/// with its log cut down.
enum SendError {
    Refused(String),
    LoginRequired(String),
    Unreachable(String),
    TooLarge(String),
}

impl SendError {
    fn why(&self) -> &str {
        match self {
            SendError::Refused(why) | SendError::LoginRequired(why) | SendError::Unreachable(why) | SendError::TooLarge(why) => why,
        }
    }
}

const LOGIN_NEEDED: &str = "Log in to BlizzNux to send reports.";

#[tauri::command]
pub async fn send_report(mut report: serde_json::Value) -> Result<String, String> {
    post_report(&mut report).await.result.map_err(|e| e.why().to_string())
}

/// The client that posts reports. A redirect is an error, never a request to wherever it points:
/// a report goes to the one address the launcher was given.
fn reports_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

/// What one try at the site came back with, and how long it asked to wait before another one
/// (`Retry-After`, in seconds).
struct Answer {
    result: Result<String, SendError>,
    retry_after: Option<u64>,
}

/// Posts a report once. One the site calls too large is cut down once and posted again: the site
/// takes as much of it as it will, and the answer says so when it still will not.
async fn post_report(report: &mut serde_json::Value) -> Answer {
    let first = post_once(report).await;
    if matches!(&first.result, Err(SendError::TooLarge(_))) && trim_large(report) {
        let again = post_once(report).await;
        if let Err(SendError::TooLarge(why)) = &again.result {
            eprintln!("[report] still too large with no crash file and the log cut to its last {TRIM_LOG_LINES} lines: {why}");
        }
        return again;
    }
    first
}

async fn post_once(report: &serde_json::Value) -> Answer {
    let offline = || Answer { result: Err(SendError::Unreachable("The site could not be reached.".into())), retry_after: None };
    let client = match reports_client() {
        Ok(client) => client,
        Err(_) => return offline(),
    };
    let resp = match client.post(report_url()).json(report).send().await {
        Ok(resp) => resp,
        Err(_) => return offline(),
    };
    let status = resp.status().as_u16();
    let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    Answer { result: answer(status, &resp.text().await.unwrap_or_default()), retry_after }
}

/// What the site's answer to a report means: delivered (with the thread, when there is one),
/// log in first, try again later, refused, or too large to take at all.
fn answer(status: u16, text: &str) -> Result<String, SendError> {
    let field = |name: &str| serde_json::from_str::<serde_json::Value>(text).ok().and_then(|v| v[name].as_str().map(String::from));
    match status {
        200..=299 => Ok(field("url").unwrap_or_default()),
        300..=399 => Err(SendError::Refused(format!("The site redirected this report (HTTP {status}), so it was not delivered."))),
        401 => Err(SendError::LoginRequired(field("error").unwrap_or_else(|| LOGIN_NEEDED.into()))),
        413 => Err(SendError::TooLarge(field("error").unwrap_or_else(|| format!("The site cannot take a report this large (HTTP {status}).")))),
        429 => Err(SendError::Unreachable("The site is not taking more reports right now.".into())),
        500..=599 => Err(SendError::Unreachable(format!("The site had a problem (HTTP {status})."))),
        _ => {
            let msg = field("error").unwrap_or_else(|| text.to_string());
            Err(SendError::Refused(format!("The site did not accept this report (HTTP {status}: {}).", msg.chars().take(300).collect::<String>())))
        }
    }
}

/// The most log lines a report keeps once the site has called it too large.
const TRIM_LOG_LINES: usize = 60;

/// Makes a report smaller for a site that calls it too large: the crash file goes first, then the
/// log down to its last TRIM_LOG_LINES lines. False when there is nothing left to cut.
fn trim_large(report: &mut serde_json::Value) -> bool {
    let Some(fields) = report.as_object_mut() else { return false };
    let mut cut = fields.remove("crash").is_some();
    let log = fields.get("log").and_then(|v| v.as_str()).map(String::from);
    if let Some(log) = log {
        let lines: Vec<&str> = log.lines().collect();
        if lines.len() > TRIM_LOG_LINES {
            fields.insert("log".into(), serde_json::Value::String(lines[lines.len() - TRIM_LOG_LINES..].join("\n")));
            cut = true;
        }
    }
    cut
}

/// Whether a report saved earlier may go out now: only against the game build it was written
/// for, and only when it names one. A saved report without a build cannot say what it is about,
/// so it is never sent later against whatever is installed by then.
fn build_matches(saved: &str, body_version: Option<&str>) -> bool {
    !saved.is_empty() && body_version == Some(saved)
}

/// How long to wait before posting a report again: a report gets three tries in one flush, a
/// second apart and then two, or what the site asked for in `Retry-After`. A site that names a
/// longer wait than CAP_SECS gets no more tries in this flush: the report stays saved for the
/// next start or login instead of the launcher hammering it. None once there is no try left.
fn retry_delay(attempt: u32, retry_after: Option<u64>) -> Option<Duration> {
    const CAP_SECS: u64 = 2;
    if attempt >= 3 {
        return None;
    }
    match retry_after {
        Some(secs) if secs > CAP_SECS => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(Duration::from_secs((attempt as u64).min(CAP_SECS))),
    }
}

/// Waits between two tries without holding up the rest of the launcher.
async fn pause(secs: Duration) {
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(secs)).await;
}

/// Posts a report with up to three tries: a timeout, a rate limit or a server error is worth
/// another try after a short wait, unless the site asked to be left alone for longer than that.
/// Anything else is the site's final answer.
async fn send_with_retry(report: &mut serde_json::Value) -> Result<String, SendError> {
    let mut attempt = 1;
    loop {
        let Answer { result, retry_after } = post_report(report).await;
        let e = match result {
            Ok(url) => return Ok(url),
            Err(e) => e,
        };
        let wait = match retry_delay(attempt, retry_after) {
            Some(wait) if matches!(e, SendError::Unreachable(_)) => wait,
            _ => return Err(e),
        };
        eprintln!("[report] {}; trying again in {} s", e.why(), wait.as_secs());
        pause(wait).await;
        attempt += 1;
    }
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
    /// Kept until the launcher is logged in: the site takes reports only from a logged-in launcher.
    #[serde(default)]
    needs_login: bool,
    /// The account the report was written for, by name (never the token): it goes out only while
    /// that same account is linked again.
    #[serde(default)]
    account: Option<String>,
    /// The Labs settings that were on for the session, kept so a report sent later says so.
    #[serde(default)]
    lab: Vec<String>,
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
    /// Kept until the user logs in; it goes out as soon as they do.
    pub needs_login: bool,
    /// Kept to be tried again on later starts (not when the site refused it).
    pub retry: bool,
}

/// Send a report the user asked for. Every report needs a logged-in launcher: without one the
/// report is kept and goes out once the user logs in. One that cannot go out now for another
/// reason (site unreachable or refusing) is kept in the outbox and tried again on later starts.
/// One that cannot be built at all is not kept: the user is told why instead.
#[tauri::command]
pub async fn submit_report(kind: String, game: Option<String>, outcome: Option<String>, comment: Option<String>) -> Result<Submitted, String> {
    let linked = !user_token().is_empty();
    let mut pending = Pending {
        kind: kind.clone(),
        game: game.clone(),
        outcome: outcome.clone(),
        comment: comment.clone(),
        saved_at: now_secs(),
        build: String::new(),
        report: None,
        refused_by: None,
        needs_login: false,
        // Whose report this is: it goes out only under the same account, never under whoever
        // happens to be logged in when it is tried again.
        account: linked_account(),
        lab: Vec::new(),
    };
    let mut body = match build_outcome(build(kind, game, outcome, comment)) {
        BuildOutcome::Send(body) => body,
        BuildOutcome::Error(why) => return Err(why),
    };
    // Without a login the site can only answer "log in first": skip that round trip.
    let sent = if linked { post_report(&mut body).await.result } else { Err(SendError::LoginRequired(LOGIN_NEEDED.into())) };
    let e = match sent {
        Ok(url) => return Ok(Submitted { sent: true, url, reason: String::new(), needs_login: false, retry: false }),
        Err(e) => e,
    };
    pending.lab = body["lab"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    pending.build = body["game"]["version"].as_str().unwrap_or_default().to_string();
    let reason = match e {
        SendError::LoginRequired(why) => {
            // A launcher that thought it was logged in holds a link the site has ended.
            if linked {
                forget_link()?;
            }
            pending.needs_login = true;
            pending.report = Some(without_token(body));
            why
        }
        SendError::Unreachable(why) => {
            pending.report = Some(without_token(body));
            why
        }
        // A refused answer waits for a launcher whose report the site takes; a refused
        // bug report would carry another log by then. A report the site calls too large
        // after its log was cut is no different: it is not sent again as it stands.
        SendError::Refused(why) | SendError::TooLarge(why) if pending.kind == "run" => {
            pending.refused_by = Some(env!("CARGO_PKG_VERSION").into());
            why
        }
        SendError::Refused(why) | SendError::TooLarge(why) => return Err(why),
    };
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    save_pending(&outbox_dir().join(format!("{nanos}.json")), &pending)?;
    Ok(Submitted { sent: false, url: String::new(), reason, needs_login: pending.needs_login, retry: pending.refused_by.is_none() })
}

/// A report as kept on disk: the account token is added again when it is sent.
fn without_token(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = body.as_object_mut() {
        fields.remove("user_token");
    }
    body
}

/// What stays on disk for a report the site found too large: the body as it was posted, so a
/// report cut down for the site is stored cut down, without the account token.
fn keep_posted(pending: &mut Pending, body: serde_json::Value) {
    pending.report = Some(without_token(body));
}

/// What a start, or a fresh login, does with a saved report.
#[derive(Debug, PartialEq)]
enum Held {
    Expired,
    Wait,
    /// Written for another account than the one linked now: kept, and never sent as this one.
    OtherAccount,
    Send,
}

/// Two names are the same account whatever the case: the link keeps the name the site shows, and
/// a site may spell it back differently.
fn same_account(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// True when the report may go out as the account linked now: the one it was written for, or, for
/// a file that names none, one this install has not replaced with someone else since.
fn written_for(pending: &Pending, linked: &str, last: Option<&str>) -> bool {
    match pending.account.as_deref() {
        Some(account) => same_account(account, linked),
        // Saved before the account was written down. It may only be sent while this install has
        // not forgotten another account's link: with one, the report could be someone else's.
        None => last.map_or(true, |last| same_account(last, linked)),
    }
}

/// Every report needs a login, so nothing goes out while the launcher is logged out; a report
/// the site refused waits for the next launcher version. A report also belongs to the account it
/// was written for: with another account linked it stays put until that one is back.
fn held(pending: &Pending, linked: Option<&str>, last: Option<&str>, now: u64) -> Held {
    if now.saturating_sub(pending.saved_at) > PENDING_SECS {
        Held::Expired
    } else if linked.is_none() || pending.refused_by.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        Held::Wait
    } else if linked.map_or(false, |linked| written_for(pending, linked, last)) {
        Held::Send
    } else {
        Held::OtherAccount
    }
}

fn outbox() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(outbox_dir())
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map_or(false, |x| x == "json")).collect())
        .unwrap_or_default();
    files.sort();
    files
}

/// Every saved report as it stands on disk now; a file that no longer reads as one is left to
/// the flush, which knows what to do with it.
fn saved_reports() -> Vec<Pending> {
    outbox()
        .iter()
        .filter_map(|p| fs::read(p).ok().and_then(|b| serde_json::from_slice::<Pending>(&b).ok()))
        .collect()
}

/// How many saved reports wait for the user to log in.
#[tauri::command]
pub fn waiting_reports() -> usize {
    // Logged out on purpose: every report kept for a login counts here, whatever its account.
    saved_reports().iter().filter(|p| p.needs_login && held(p, None, None, now_secs()) != Held::Expired).count()
}

/// How many saved reports belong to another account than the one linked now: they wait for that
/// account to come back, or expire, and are never sent under the current one.
#[tauri::command]
pub fn other_account_reports() -> usize {
    let (linked, last) = linked_accounts();
    saved_reports().iter().filter(|p| held(p, linked.as_deref(), last.as_deref(), now_secs()) == Held::OtherAccount).count()
}

/// What a flush did: how many reports got through, and how many the site found too large. Those
/// stay saved, and the user is told instead of them quietly never arriving.
#[derive(serde::Serialize)]
pub struct Flushed {
    pub sent: usize,
    pub too_large: usize,
}

/// Try the outbox again (the UI asks once per start and once after a login); returns how many
/// reports got through, and how many the site found too large to take.
#[tauri::command]
pub async fn flush_reports() -> Flushed {
    let (linked, last) = linked_accounts();
    let mut sent = 0;
    let mut too_large = 0;
    for path in outbox() {
        let discard = || { let _ = fs::remove_file(&path); };
        let Some(mut pending) = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Pending>(&b).ok()) else { discard(); continue };
        match held(&pending, linked.as_deref(), last.as_deref(), now_secs()) {
            Held::Expired => { discard(); continue }
            Held::Wait | Held::OtherAccount => continue,
            Held::Send => {}
        }
        let mut body = match pending.report.clone() {
            Some(mut body) => {
                let Some(fields) = body.as_object_mut() else { discard(); continue };
                fields.insert("user_token".into(), user_token().into());
                body
            }
            None => match build(pending.kind.clone(), pending.game.clone(), pending.outcome.clone(), pending.comment.clone()) {
                // A report that names no build is not sent against one it was never written
                // for: it is dropped, and the log says why.
                Ok(_) if pending.build.is_empty() => {
                    eprintln!("[report] saved {} report names no game build: dropped, so it is not sent against another", pending.kind);
                    discard();
                    continue;
                }
                // The answer was about the build installed then; a game patched since is another one.
                Ok(body) if !build_matches(&pending.build, body["game"]["version"].as_str()) => { discard(); continue }
                Ok(mut body) => {
                    body["created_at"] = iso(pending.saved_at).into();
                    if !pending.lab.is_empty() {
                        mark_lab(&mut body, &pending.lab);
                    }
                    body
                }
                Err(BuildError::UnknownBuild(_)) => continue,
                Err(BuildError::Invalid(_)) => { discard(); continue }
            },
        };
        match send_with_retry(&mut body).await {
            Ok(_) => {
                discard();
                sent += 1;
            }
            Err(SendError::Unreachable(_)) => break,
            // The site has ended this link: keep the report for the next login and stop, the
            // rest would get the same answer. No retry until then, so this never loops.
            Err(SendError::LoginRequired(why)) => {
                eprintln!("[report] saved {} report needs a login: {why}", pending.kind);
                pending.needs_login = true;
                let _ = save_pending(&path, &pending);
                let _ = forget_link();
                break;
            }
            // Kept as it is, and the user is told: a report the site cannot take as it stands is
            // not thrown away. The body as posted (cut down, when it was) is what stays saved,
            // so the next start sends the trimmed one instead of the full one again.
            Err(SendError::TooLarge(why)) => {
                eprintln!("[report] saved {} report is too large for the site: {why}", pending.kind);
                keep_posted(&mut pending, body);
                let _ = save_pending(&path, &pending);
                too_large += 1;
            }
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
    Flushed { sent, too_large }
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
    let stamp = stamp_for(kind, &build, &now_iso()[..10], &report_lab(kind));
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
        match report_body(kind.clone(), game, Some(outcome.clone()), Some(comment)) {
            Ok(body) => { let lab = body.get("lab").is_some(); match send_report(body).await {
                Ok(url) => {
                    mark_sent(&kind, &stamp);   // only an accepted report counts for today
                    let _ = h.emit("report-sent", serde_json::json!({ "kind": kind, "outcome": outcome, "url": url, "lab": lab }));
                }
                Err(e) => eprintln!("[report] {kind} not sent: {e}"),
            } },
            Err(e) => eprintln!("[report] {kind} not built: {e}"),
        }
    });
}

/// True from the moment Play is accepted until `watch_launch` finishes. For the first stretch of
/// a start (Proton/umu setup, up to two minutes) no Battle.net process exists yet, so
/// `battlenet_running` alone would let a second click start a second launch.
static LAUNCH_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Holds a flag while alive and clears it on drop, so a panic or an early return can never leave
/// it stuck on.
pub struct LaunchGuard(&'static std::sync::atomic::AtomicBool);

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn take_flag(flag: &'static std::sync::atomic::AtomicBool) -> Option<LaunchGuard> {
    use std::sync::atomic::Ordering::SeqCst;
    // compare_exchange so two fast clicks cannot both pass.
    flag.compare_exchange(false, true, SeqCst, SeqCst).ok().map(|_| LaunchGuard(flag))
}

/// Claim the right to start Battle.net, or None while another start is still pending.
pub fn begin_launch() -> Option<LaunchGuard> {
    take_flag(&LAUNCH_PENDING)
}

/// After Launch: did the Battle.net client actually come up? Reports ok/failed when sharing.
/// Takes the guard from `begin_launch` and keeps it until the start has succeeded or timed out.
pub fn watch_launch(app: AppHandle, guard: LaunchGuard) {
    set_session_lab(labs_in(&load_config()));
    std::thread::spawn(move || {
        let mut start = Instant::now();
        let mut ok = false;
        while start.elapsed() < Duration::from_secs(120) {
            if game_running(&["Battle.net.exe", "Battle.net Launcher.exe", "Battle.net-Setup.exe"]) {
                ok = true;
                break;
            }
            // The first start downloads the launcher's own Proton before anything else. That is
            // not Battle.net failing to start: the two minutes count from the end of it.
            if let Some(doing) = proton_arriving(&proton_dir()).filter(|_| custom_proton().is_none()) {
                start = Instant::now();
                let _ = app.emit("launch-progress", doing);
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        // Free the launch flag before the result goes out, so the retry offered with it is never refused.
        drop(guard);
        let leftovers = if ok { 0 } else { prefix_leftovers(&current_prefix()).len() };
        if leftovers > 0 {
            note(&format!("{} Battle.net did not start; {leftovers} processes of an earlier session still run in the prefix", now_iso()));
        }
        let _ = app.emit("launch-result", serde_json::json!({ "ok": ok, "leftovers": leftovers }));
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

/// The paths a process's environment names as its Wine prefix: WINEPREFIX (set by Proton and
/// umu for everything Wine starts) and STEAM_COMPAT_DATA_PATH (set earlier, for Proton's own
/// setup scripts, which stall before any Wine process exists).
fn prefix_env_paths(env: &[u8]) -> Vec<PathBuf> {
    env.split(|b| *b == 0)
        .filter_map(|kv| kv.strip_prefix(b"WINEPREFIX=").or_else(|| kv.strip_prefix(b"STEAM_COMPAT_DATA_PATH=")))
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(String::from_utf8_lossy(v).to_string()))
        .collect()
}

/// Is any of these paths the prefix, once both sides are resolved? umu's `<prefix>/pfx/` is a
/// link back to the prefix itself.
fn names_prefix(paths: &[PathBuf], prefix: &Path) -> bool {
    let Ok(ours) = fs::canonicalize(prefix) else { return false };
    paths.iter().any(|p| fs::canonicalize(p).map_or(false, |theirs| theirs == ours))
}

/// Everything still running in the launcher's own prefix: wineserver, explorer, services,
/// winedevice, Battle.net, the Agent, and Proton's setup scripts. Found by the prefix in each
/// process's environment, not by name, so nothing from another prefix or another Wine
/// (Steam, Lutris) is ever included. The launcher itself is left out.
fn prefix_leftovers(prefix: &Path) -> Vec<u32> {
    let mut found = Vec::new();
    let me = std::process::id();
    let Ok(rd) = fs::read_dir("/proc") else { return found };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me { continue }
        let Ok(env) = fs::read(e.path().join("environ")) else { continue };
        if names_prefix(&prefix_env_paths(&env), prefix) {
            found.push(pid);
        }
    }
    found
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
/// Blizzard's Agent, which installs and updates the games (the path ends in `\Agent.exe` or `/Agent.exe`).
const AGENT: [&str; 2] = ["\\Agent.exe", "/Agent.exe"];

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

/// The game running in this prefix right now, if any, read from the processes rather than the
/// watcher so a game started in the last few seconds still counts.
fn game_in_prefix(prefix: &Path) -> Option<&'static str> {
    GAMES.iter().find(|g| !pids_of(g.3, Some(prefix)).is_empty()).map(|g| g.1)
}

/// Battle.net's processes and its Agent in this prefix. When the last of them exits, Wine's
/// helpers (explorer, wineserver) end on their own, and with them the wrapper's sleep inhibitor.
fn battlenet_session_pids(prefix: &Path) -> Vec<u32> {
    let mut pids = pids_of(&BATTLENET, Some(prefix));
    pids.extend(pids_of(&AGENT, Some(prefix)));
    pids
}

fn signal_all(pids: &[u32], sig: i32) {
    for pid in pids {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::kill(*pid as libc::pid_t, sig);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (pid, sig);
    }
}

/// Close Battle.net for good: its window may be gone (closed to the tray) while it and the Agent
/// keep running and keep the computer from sleeping. Never while a game runs in the prefix, so
/// it cannot take a game down. Asks politely first (SIGTERM), and only forces (SIGKILL) what is
/// still there after ten seconds. Only touches processes in the launcher's own prefix.
#[tauri::command]
pub async fn close_battlenet() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let prefix = current_prefix();
        if let Some(game) = game_in_prefix(&prefix) {
            return Err(format!("{game} is running. Close the game first, then close Battle.net."));
        }
        let pids = battlenet_session_pids(&prefix);
        if pids.is_empty() {
            return Ok("Battle.net is not running.".to_string());
        }
        note(&format!("{} closing Battle.net ({} processes)", now_iso(), pids.len()));
        #[cfg(target_os = "linux")]
        signal_all(&pids, libc::SIGTERM);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            if battlenet_session_pids(&prefix).is_empty() {
                return Ok("Battle.net is closed.".to_string());
            }
        }
        if game_in_prefix(&prefix).is_some() {
            return Err("A game started while Battle.net was closing, so it was left running.".to_string());
        }
        #[cfg(target_os = "linux")]
        signal_all(&battlenet_session_pids(&prefix), libc::SIGKILL);
        std::thread::sleep(Duration::from_secs(1));
        if battlenet_session_pids(&prefix).is_empty() {
            note(&format!("{} Battle.net did not exit on request and was stopped", now_iso()));
            Ok("Battle.net is closed.".to_string())
        } else {
            Err("Battle.net did not close. Try again in a moment.".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// End everything an earlier session left running in the launcher's prefix, so a start that
/// stalled in Proton's setup can be tried again. Same rules as `close_battlenet`: never while a
/// game runs in the prefix, SIGTERM first, SIGKILL after ten seconds, only this prefix.
#[tauri::command]
pub async fn reset_prefix_processes() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let prefix = current_prefix();
        if let Some(game) = game_in_prefix(&prefix) {
            return Err(format!("{game} is running. Close the game first, then try again."));
        }
        let pids = prefix_leftovers(&prefix);
        if pids.is_empty() {
            return Ok("Nothing is left running.".to_string());
        }
        note(&format!("{} closing {} leftover processes in the prefix", now_iso(), pids.len()));
        #[cfg(target_os = "linux")]
        signal_all(&pids, libc::SIGTERM);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            if prefix_leftovers(&prefix).is_empty() {
                return Ok("Closed what was stuck.".to_string());
            }
        }
        if game_in_prefix(&prefix).is_some() {
            return Err("A game started while this was closing, so it was left running.".to_string());
        }
        #[cfg(target_os = "linux")]
        signal_all(&prefix_leftovers(&prefix), libc::SIGKILL);
        std::thread::sleep(Duration::from_secs(1));
        if prefix_leftovers(&prefix).is_empty() {
            note(&format!("{} leftover processes did not exit on request and were stopped", now_iso()));
            Ok("Closed what was stuck.".to_string())
        } else {
            Err("Some processes did not close. Try again in a moment.".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Blizzard's Agent keeps the local port it listens on in this file. Battle.net and the games
/// read it to find the Agent that is already running.
fn agent_port_file(prefix: &Path) -> PathBuf {
    prefix.join("drive_c/ProgramData/Battle.net/Agent/Agent.dat")
}

fn port_in(file: &Path) -> Option<u16> {
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// Does a Blizzard Agent answer on this local port? It answers a request for its own state with
/// the state, or with "not authorised" when no session key comes with it, as here.
fn agent_answers(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(700)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(300)));
    if s.write_all(b"GET /agent HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    s.read_exact(&mut head).is_ok() && (&head == b"HTTP/1.1 401" || &head == b"HTTP/1.1 200")
}

/// Keeps the Agent's port file pointing at the Agent of this Battle.net session. Heroes of the
/// Storm starts an Agent of its own beside the running one; that copy takes another port and
/// writes it to the file. When it leaves with the game, the file names a port nobody listens on,
/// the next game finds no Agent and starts yet another copy, and whichever copy closes last
/// saves its own, older list of installed games: games installed in that session are forgotten.
/// So once the port in the file has gone quiet while the session's Agent still answers, the
/// file gets that Agent's port back. `main` is the port this session's Agent was found on, and
/// the new value of it is returned; it is learnt while a single Agent runs.
fn keep_agent_findable(file: &Path, battlenet: bool, agents: usize, main: Option<u16>) -> Option<u16> {
    if !battlenet {
        return None;
    }
    let now = port_in(file);
    let Some(main) = main else {
        return now.filter(|p| agents == 1 && agent_answers(*p));
    };
    if now == Some(main) {
        return Some(main);
    }
    if !agent_answers(main) {
        return None; // the session's Agent has gone or moved: learn again
    }
    if now.map_or(true, |p| !agent_answers(p)) && fs::write(file, main.to_string()).is_ok() {
        note(&format!("the Agent's port file named port {}, where no Agent answers; set back to {main}", now.map_or("none".into(), |p| p.to_string())));
    }
    Some(main)
}

/// A line in the launcher's log, next to what the wrapper writes there. The unit tests go
/// through the same code and must not write into the log of the machine they run on.
fn note(text: &str) {
    use std::io::Write;
    if cfg!(test) {
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(crate::log_path()) {
        let _ = writeln!(f, "[blizznux] {text}");
    }
}

/// What the launcher sees of the Agent in its prefix (for `--cli agent-state`).
pub fn agent_state() -> serde_json::Value {
    let prefix = current_prefix();
    let port = port_in(&agent_port_file(&prefix));
    serde_json::json!({
        "port_file": agent_port_file(&prefix),
        "port": port,
        "answers": port.map_or(false, agent_answers),
        "agents_running": pids_of(&AGENT, Some(&prefix)).len(),
        "battlenet_running": !pids_of(&BATTLENET, Some(&prefix)).is_empty(),
    })
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
        // The port this Battle.net session's Agent was found on.
        let mut agent_port: Option<u16> = None;
        // A game that has already been counted as closed while its process still finishes
        // shutting down; it is not picked up again as a new session.
        let mut closing: Option<u32> = None;
        loop {
            let prefix = current_prefix();
            let battlenet = !pids_of(&BATTLENET, Some(&prefix)).is_empty();
            agent_port = keep_agent_findable(&agent_port_file(&prefix), battlenet, pids_of(&AGENT, Some(&prefix)).len(), agent_port);
            let pids = current.as_ref().map(|(g, ..)| pids_of(g.3, Some(&prefix))).unwrap_or_default();
            let window_closed = matches!(&current, Some((g, flavor, seen, age)) if wow_window_closed(&prefix, g, flavor, *seen, *age));
            let ended = current.is_some() && (pids.is_empty() || window_closed);
            if ended {
                closing = pids.into_iter().min();
                if let Some((g, flavor, seen, age)) = current.take() {
                    let secs = age + seen.elapsed().as_secs();
                    let id = game_id(g.0, &flavor);
                    let (event, record_ok) = session_end(secs);
                    note(&format!("{} {} closed after {secs} s", now_iso(), game_name(g, &flavor)));
                    if is_reported(g.0, &flavor) {
                        if record_ok {
                            auto_report(&app, "run", Some(id.clone()), "ok", format!("ran for {} min", secs / 60));
                        }
                        let _ = app.emit(event, serde_json::json!({ "code": id, "name": game_name(g, &flavor), "seconds": secs }));
                        #[cfg(target_os = "linux")]
                        crate::notify::game_ended(&app, &game_name(g, &flavor));
                    }
                }
            } else if current.is_none() && closing.map_or(true, |pid| !Path::new(&format!("/proc/{pid}")).exists()) {
                closing = None;
                current = GAMES.iter().find_map(|g| {
                    let pid = pids_of(g.3, Some(&prefix)).into_iter().min()?;
                    Some((g, flavor_of(&process_path(pid)), Instant::now(), process_age(pid).unwrap_or(0)))
                });
                if let Some(pid) = current.as_ref().and_then(|(g, ..)| pids_of(g.3, Some(&prefix)).into_iter().min().map(|p| (g, p))).map(|(_, p)| p) {
                    set_session_lab(lab_of_pid(pid));
                }
                if current.is_some() {
                    // The game keeps the graphics card awake: look the cards up now, so the report
                    // at the end of the session does not have to wake it again.
                    std::thread::spawn(gpus);
                }
            }
            let now = SessionState { battlenet, game: current.as_ref().map(|c| game_name(c.0, &c.1)) };
            let changed = SESSION.lock().map(|mut s| if *s != now { *s = now.clone(); true } else { false }).unwrap_or(false);
            if changed {
                let _ = app.emit("session-state", &now);
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Has World of Warcraft closed its window in this session? It shuts its graphics down first,
/// then spends a while closing its connection to Blizzard before the process exits; asking only
/// once the process is gone left the user waiting about 20 seconds for the question. WoW starts
/// a fresh `Logs/gx.log` every run, so a `GxShutdown` in one written since the game started
/// belongs to this session.
fn wow_window_closed(prefix: &Path, g: &(&str, &str, &str, &[&str]), flavor: &str, seen: Instant, age: u64) -> bool {
    if g.0 != "WoW" || flavor.is_empty() {
        return false;
    }
    let log = game_root(prefix, g).join(flavor).join("Logs/gx.log");
    let started = SystemTime::now() - seen.elapsed() - Duration::from_secs(age);
    let fresh = fs::metadata(&log).and_then(|m| m.modified()).map_or(false, |t| t > started);
    fresh && fs::read_to_string(&log).map_or(false, |t| t.contains("GxShutdown"))
}

/// What the end of a game session means: the event the UI gets, and whether a successful run is
/// recorded automatically (when sharing). A session of under a minute proves nothing either way:
/// a lost connection to Blizzard, a login queue and the user quitting all look the same as a
/// crash. So it is never reported by itself; the UI asks instead.
fn session_end(secs: u64) -> (&'static str, bool) {
    if secs < 60 { ("game-closed-early", false) } else { ("game-ended", true) }
}

/// The session type for reports: a launcher restarted under XWayland says so (the site keeps 20 characters).
fn session_label(xdg: String, xwayland_fallback: bool) -> String {
    if xwayland_fallback {
        "wayland (xwayland)".into()
    } else {
        xdg
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_report_log_starts_at_the_last_session() {
        let long: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let log = format!("=== 2026-11-01T10:00:00-05:00 ===\nold\n\n=== 2026-11-02T10:00:00-05:00 ===\n{long}");
        let got = recent_sessions(&log);
        assert!(got.starts_with("=== 2026-11-02"));
        assert!(!got.contains("old"));
        assert!(got.ends_with("line 49"));
    }

    #[test]
    fn a_short_last_session_brings_the_one_before() {
        let log = "=== a ===\nfirst\n=== b ===\nsecond\n=== c ===\nthird\n";
        let got = recent_sessions(log);
        assert!(got.starts_with("=== b ==="));
        assert!(got.contains("second") && got.contains("third") && !got.contains("first"));
    }

    #[test]
    fn a_log_without_headers_or_with_one_session_is_whole() {
        assert_eq!(recent_sessions("just\nlines"), "just\nlines");
        let one = "=== a ===\nonly\n";
        assert!(recent_sessions(one).contains("only"));
    }

    #[test]
    fn a_crash_file_loses_the_user_and_computer_name() {
        let text = "Exe: C:\\Users\\Jane\\wow.exe\nComputer: DESKTOP-ABC\nUser: Jane\nException: ACCESS_VIOLATION";
        let got = redact_crash(text);
        assert!(!got.contains("Jane") && !got.contains("DESKTOP-ABC"));
        assert!(got.contains("Users\\~\\wow.exe") && got.contains("ACCESS_VIOLATION"));
    }

    #[test]
    fn a_crash_excerpt_is_capped() {
        let text: String = (0..1000).map(|i| format!("frame {i}\n")).collect();
        let got = crash_excerpt(&text);
        assert_eq!(got.lines().count(), CRASH_LINES);
        assert!(got.ends_with("frame 999"));
        assert!(crash_excerpt(&"x".repeat(50_000)).len() <= CRASH_BYTES);
    }

    #[test]
    fn only_a_fresh_text_crash_file_is_picked() {
        let dir = std::env::temp_dir().join(format!("blizznux-crash-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.dmp"), "bin").unwrap();
        assert!(newest_crash(&dir, SystemTime::now(), 3600).is_none());
        fs::write(dir.join("b.txt"), "crash").unwrap();
        assert_eq!(newest_crash(&dir, SystemTime::now(), 3600), Some(dir.join("b.txt")));
        let later = SystemTime::now() + Duration::from_secs(7200);
        assert!(newest_crash(&dir, later, 3600).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn the_prefix_is_read_from_either_variable() {
        use std::path::PathBuf;
        let env = b"HOME=/h\0WINEPREFIX=/p/pfx/\0STEAM_COMPAT_DATA_PATH=/p\0WINEPREFIX=\0";
        assert_eq!(super::prefix_env_paths(env), vec![PathBuf::from("/p/pfx/"), PathBuf::from("/p")]);
        assert!(super::prefix_env_paths(b"HOME=/h\0PATH=/bin").is_empty());
    }

    #[test]
    fn a_prefix_is_matched_through_its_pfx_link_and_never_by_prefix_of_the_name() {
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!("bnx-prefix-{}", std::process::id()));
        let other = std::env::temp_dir().join(format!("bnx-prefix-{}-other", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(&other);
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&dir, dir.join("pfx"));
            assert!(super::names_prefix(&[dir.join("pfx")], &dir));
        }
        assert!(super::names_prefix(&[PathBuf::from("/nonexistent"), dir.clone()], &dir));
        assert!(!super::names_prefix(&[other.clone()], &dir));
        assert!(!super::names_prefix(&[], &dir));
        assert!(!super::names_prefix(&[dir.clone()], &dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    use super::*;

    #[test]
    fn a_restart_under_xwayland_is_reported_as_such() {
        assert_eq!(session_label("wayland".into(), true), "wayland (xwayland)");
        assert!("wayland (xwayland)".len() <= 20);
        assert_eq!(session_label("x11".into(), false), "x11");
    }

    #[test]
    fn a_second_launch_is_refused_until_the_first_guard_drops() {
        static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let first = take_flag(&FLAG).expect("first start is allowed");
        assert!(take_flag(&FLAG).is_none());
        drop(first);
        assert!(take_flag(&FLAG).is_some());
    }

    #[test]
    fn a_panicking_start_does_not_leave_the_flag_on() {
        static FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let _ = std::thread::spawn(|| {
            let _g = take_flag(&FLAG).unwrap();
            panic!("start died");
        })
        .join();
        assert!(take_flag(&FLAG).is_some());
    }

    #[test]
    fn a_labs_session_has_its_own_daily_stamp() {
        let plain = stamp_for("run", "wow:1.2", "2026-11-08", &[]);
        assert_eq!(plain, "run:wow:1.2:2026-11-08");
        let lab = stamp_for("run", "wow:1.2", "2026-11-08", &["wow64".to_string()]);
        assert_eq!(lab, "run:wow:1.2:2026-11-08:lab=wow64");
        assert_ne!(plain, lab);
        let two = stamp_for("run", "b", "d", &["b".to_string(), "a".to_string()]);
        assert_eq!(two, "run:b:d:lab=a,b");
    }

    #[test]
    fn the_runtime_is_named_as_its_own_folder_records_it() {
        let dir = std::env::temp_dir().join(format!("blizznux-runtime-test-{}", std::process::id()));
        assert_eq!(runtime_under(&dir), "");
        fs::create_dir_all(dir.join("steamrt4")).unwrap();
        fs::create_dir_all(dir.join("umu-shim")).unwrap();
        assert_eq!(runtime_under(&dir), "");
        fs::write(
            dir.join("steamrt4/VERSIONS.txt"),
            "#Name\tVersion\t\tRuntime\tRuntime_Version\tComment\ndepot\t4.0.20260928.262390\t\t\t# Overall version number\npressure-vessel\t0.20260925.0\t\t\t\n",
        )
        .unwrap();
        assert_eq!(runtime_under(&dir), "steamrt4 4.0.20260928.262390");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A stand-in for a program listening on a local port: answers every request with this
    /// status line until it is dropped.
    struct Listener(u16, std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Listener {
        fn answering(status: &'static str) -> Listener {
            use std::io::{Read, Write};
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopped = stop.clone();
            std::thread::spawn(move || {
                for s in l.incoming() {
                    if stopped.load(std::sync::atomic::Ordering::SeqCst) { break }
                    let Ok(mut s) = s else { continue };
                    let _ = s.read(&mut [0u8; 256]);
                    let _ = s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes());
                }
            });
            Listener(port, stop)
        }
    }
    impl Drop for Listener {
        fn drop(&mut self) {
            self.1.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(("127.0.0.1", self.0)); // wakes the thread so it closes the port
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn an_agent_is_told_from_other_programs_and_from_a_quiet_port() {
        let agent = Listener::answering("401 Unauthorized");
        let other = Listener::answering("404 Not Found");
        assert!(agent_answers(agent.0));
        assert!(!agent_answers(other.0));
        let port = agent.0;
        drop(agent);
        assert!(!agent_answers(port));
    }

    #[test]
    fn the_port_file_is_set_back_once_a_games_own_agent_has_gone() {
        let dir = std::env::temp_dir().join(format!("blizznux-agent-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Agent.dat");
        let port = || fs::read_to_string(&file).unwrap();

        // Battle.net starts its Agent: the launcher learns the port, and only while Battle.net runs.
        let main = Listener::answering("401 Unauthorized");
        fs::write(&file, main.0.to_string()).unwrap();
        assert_eq!(keep_agent_findable(&file, false, 1, None), None);
        assert_eq!(keep_agent_findable(&file, true, 2, None), None);
        let known = keep_agent_findable(&file, true, 1, None);
        assert_eq!(known, Some(main.0));

        // A game starts an Agent of its own, which writes its port: left alone while it runs.
        let copy = Listener::answering("401 Unauthorized");
        fs::write(&file, copy.0.to_string()).unwrap();
        assert_eq!(keep_agent_findable(&file, true, 2, known), known);
        assert_eq!(port(), copy.0.to_string());

        // The game and its Agent are gone: the file names the session's Agent again.
        drop(copy);
        assert_eq!(keep_agent_findable(&file, true, 1, known), known);
        assert_eq!(port(), main.0.to_string());

        // The session's own Agent is gone: nothing is written, and the port is learnt afresh.
        let gone = main.0;
        drop(main);
        fs::write(&file, "1").unwrap();
        assert_eq!(keep_agent_findable(&file, true, 1, Some(gone)), None);
        assert_eq!(port(), "1");
        assert_eq!(keep_agent_findable(&file, false, 0, Some(gone)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_launchers_own_proton_is_the_complete_build_in_its_folder() {
        let dir = std::env::temp_dir().join(format!("blizznux-proton-test-{}", std::process::id()));
        assert_eq!(proton_under(&dir), None);
        // A download and an unpacking that were cut off are not a build.
        fs::create_dir_all(dir.join(".unpack/proton-cachyos-11.0-20260703-slr-x86_64")).unwrap();
        fs::write(dir.join("proton-cachyos-11.0-20260703-slr-x86_64.tar.xz.part"), "").unwrap();
        assert_eq!(proton_under(&dir), None);
        let build = dir.join("proton-cachyos-11.0-20260703-slr-x86_64");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("proton"), "").unwrap();
        assert_eq!(proton_under(&dir), Some(build));
        // The files just written there are an unpacking and a download under way.
        assert!(proton_arriving(&dir).unwrap().starts_with("Unpacking Proton"));
        fs::remove_dir_all(dir.join(".unpack")).unwrap();
        assert!(proton_arriving(&dir).unwrap().starts_with("Downloading Proton, 0 MB"));
        fs::remove_file(dir.join("proton-cachyos-11.0-20260703-slr-x86_64.tar.xz.part")).unwrap();
        assert_eq!(proton_arriving(&dir), None);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(proton_arriving(&dir), None);
    }

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
        let version = |flavor| build_row(&rows, wow_product("WoW", flavor).as_deref()).and_then(|r| r.get("Version")).cloned();
        assert_eq!(version("_retail_").as_deref(), Some("12.1.0.69933"));
        assert_eq!(version("_classic_era_").as_deref(), Some("1.15.9.70003"));
        assert_eq!(version("_classic_"), None);   // not installed: no build, not Retail's
        assert_eq!(wow_product("WoW", "_ptr_").as_deref(), Some("wowt"));
        assert_eq!(wow_product("WoW", "_classic_era_ptr_").as_deref(), Some("wow_classic_era_ptr"));
        assert_eq!(wow_product("Pro", "_retail_"), None);
    }

    #[test]
    fn test_and_beta_realms_are_not_reported() {
        for live in ["", "_retail_", "_classic_", "_classic_era_", "_anniversary_", "_classic_titan_"] {
            assert!(is_reported("WoW", live));
        }
        for test in ["_ptr_", "_xptr_", "_beta_", "_classic_ptr_", "_classic_era_ptr_", "_classic_beta_"] {
            assert!(!is_reported("WoW", test));
        }
        assert!(is_reported("Pro", "_retail_"));
        assert!(is_reported("WTCG", ""));
    }

    #[test]
    fn an_unknown_wow_version_is_never_guessed() {
        let rows = wow_rows();
        // Known folder: that folder, with the product its own record names, else by its name.
        assert_eq!(resolve_wow(&rows, "_classic_era_", Some("wow_classic_era".into())), ("_classic_era_".into(), "wow_classic_era".into()));
        assert_eq!(resolve_wow(&rows, "_anniversary_", None), ("_anniversary_".into(), "wow_anniversary".into()));
        // Unknown folder, several versions installed: nothing is chosen, so no build is found.
        let (folder, product) = resolve_wow(&rows, "", None);
        assert_eq!((folder.as_str(), product.as_str()), ("", ""));
        assert!(build_row(&rows, Some(&product)).is_none());
        // Unknown folder, a single version installed: it can only have been that one.
        let only = &rows[1..];
        assert_eq!(resolve_wow(only, "", None), ("_classic_era_".into(), "wow_classic_era".into()));
        assert_eq!(wow_folder("wow"), "_retail_");
        assert_eq!(wow_folder("wowt"), "_ptr_");
        assert_eq!(wow_folder("wow_anniversary"), "_anniversary_");
        assert_eq!(wow_folder("hsb"), "");
    }

    #[test]
    fn a_name_is_made_from_any_folder_without_failing() {
        let wow = game_by_code("WoW").unwrap();
        assert_eq!(game_name(wow, "_\u{e9}t\u{e9}_"), "WoW \u{c9}t\u{e9}");
        assert_eq!(game_name(wow, "___"), "WoW ");
    }

    #[test]
    fn classic_clients_are_named_as_the_site_files_them() {
        let wow = game_by_code("WoW").unwrap();
        assert_eq!(game_name(wow, "_classic_era_"), "WoW Classic Era");
        assert_eq!(game_name(wow, "_classic_"), "WoW Classic");
        assert_eq!(game_name(wow, "_retail_"), "World of Warcraft");
        assert_eq!(game_name(wow, "_anniversary_"), "WoW Anniversary");
        assert_eq!(game_name(wow, ""), "World of Warcraft");
        assert_eq!(game_name(game_by_code("Pro").unwrap(), "_retail_"), "Overwatch 2");
        assert_eq!(split_game("WoW/_classic_era_"), ("WoW", "_classic_era_"));
        assert_eq!(split_game("Pro"), ("Pro", ""));
        assert_eq!(game_id("WoW", "_classic_era_"), "WoW/_classic_era_");
        assert_eq!(game_id("WTCG", ""), "WTCG");
    }

    #[test]
    fn the_lab_field_is_there_only_when_a_lab_setting_is_on() {
        let mut config = BTreeMap::new();
        let mut body = serde_json::json!({ "schema": 1 });
        mark_lab(&mut body, &labs_in(&config));
        assert_eq!(body, serde_json::json!({ "schema": 1 }));
        config.insert("LAB_WOW64".to_string(), "0".to_string());
        assert!(labs_in(&config).is_empty());
        config.insert("LAB_WOW64".to_string(), "1".to_string());
        mark_lab(&mut body, &labs_in(&config));
        assert_eq!(body["lab"], serde_json::json!(["wow64"]));
    }

    #[test]
    fn labs_in_knows_each_key_and_sorts_them() {
        let mut c = BTreeMap::new();
        assert!(labs_from(&c, None, None).is_empty());
        c.insert("LAB_DIRECT_PROTON".to_string(), "1".to_string());
        assert_eq!(labs_from(&c, None, None), ["direct"]);
        c.insert("LAB_DIRECT_PROTON".to_string(), "0".to_string());
        c.insert("LAB_PROTON_DIR".to_string(), "/opt/proton/".to_string());
        assert_eq!(labs_from(&c, None, None), ["proton"]);
        c.insert("LAB_PROTON_DIR".to_string(), "".to_string());
        assert!(labs_from(&c, None, None).is_empty());
        assert_eq!(labs_from(&c, Some("/nix/store/x"), None), ["proton"]);
        assert!(labs_from(&c, Some(""), None).is_empty());
        c.insert("LAB_WOW64".to_string(), "1".to_string());
        c.insert("LAB_DIRECT_PROTON".to_string(), "1".to_string());
        c.insert("LAB_PROTON_DIR".to_string(), "/opt/p".to_string());
        assert_eq!(labs_from(&c, None, None), ["direct", "proton", "wow64"]);
    }

    #[test]
    fn labs_in_knows_the_fps_cap_and_sorts_all_four() {
        let mut c = BTreeMap::new();
        c.insert("LAB_BNET_FPS".to_string(), "1".to_string());
        assert_eq!(labs_from(&c, None, None), ["bnetfps"]);
        c.insert("LAB_WOW64".to_string(), "1".to_string());
        c.insert("LAB_DIRECT_PROTON".to_string(), "1".to_string());
        c.insert("LAB_PROTON_DIR".to_string(), "/opt/p".to_string());
        assert_eq!(labs_from(&c, None, None), ["bnetfps", "direct", "proton", "wow64"]);
    }

    #[test]
    fn a_hand_set_proton_use_wow64_counts_as_wow64() {
        let mut c = BTreeMap::new();
        assert_eq!(labs_from(&c, None, Some("1")), ["wow64"]);
        assert!(labs_from(&c, None, Some("0")).is_empty());
        assert!(labs_from(&c, None, Some("")).is_empty());
        assert!(labs_from(&c, None, None).is_empty());
        c.insert("LAB_WOW64".to_string(), "1".to_string());
        c.insert("LAB_DIRECT_PROTON".to_string(), "1".to_string());
        assert_eq!(labs_from(&c, None, Some("1")), ["direct", "wow64"]);
    }

    #[test]
    fn the_wrappers_lab_list_is_split_into_keys() {
        assert_eq!(parse_lab("direct,proton,wow64"), ["direct", "proton", "wow64"]);
        assert_eq!(parse_lab("wow64,direct"), ["direct", "wow64"]);
        assert_eq!(parse_lab("wow64"), ["wow64"]);
        assert!(parse_lab("").is_empty());
    }

    #[test]
    fn the_env_proton_wins_over_the_chosen_folder() {
        let mut c = BTreeMap::new();
        c.insert("LAB_PROTON_DIR".to_string(), "/chosen/".to_string());
        assert_eq!(custom_proton_from(&c, None), Some(PathBuf::from("/chosen")));
        assert_eq!(custom_proton_from(&c, Some("/env")), Some(PathBuf::from("/env")));
        assert_eq!(custom_proton_from(&c, Some("")), Some(PathBuf::from("/chosen")));
        assert_eq!(custom_proton_from(&BTreeMap::new(), None), None);
    }

    #[test]
    fn proton_is_named_by_its_version_file_else_its_folder() {
        let dir = std::env::temp_dir().join(format!("blizznux-protonlabel-test-{}", std::process::id()));
        let build = dir.join("GE-Proton9-27");
        fs::create_dir_all(&build).unwrap();
        assert_eq!(proton_label(&build), "GE-Proton9-27");
        fs::write(build.join("version"), "1735000000 GE-Proton9-27-custom\n").unwrap();
        assert_eq!(proton_label(&build), "GE-Proton9-27-custom");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_report_from_an_older_launcher_has_no_lab() {
        let p: Pending = serde_json::from_str(r#"{"kind":"run","game":null,"outcome":null,"comment":null,"saved_at":1,"build":"","report":null,"refused_by":null}"#).unwrap();
        assert!(p.lab.is_empty());
    }

    #[test]
    fn problem_reports_are_the_ones_that_carry_the_log() {
        assert!(is_problem("bug", None));
        assert!(is_problem("launch", Some("failed")));
        assert!(!is_problem("launch", Some("ok")));
        assert!(is_problem("run", Some("issues")));
        assert!(is_problem("run", Some("broken")));
        assert!(!is_problem("run", Some("perfect")));
        assert!(!is_problem("run", Some("ok")));
        assert!(!is_problem("run", None));
    }

    /// The fields of a body, sorted: the key set REPORTING.md documents for its type.
    fn fields(body: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = body.as_object().expect("a report body is an object").keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The machine's part of a body, filled in by the test: nothing is probed, so the fields each
    /// report type carries can be checked on any machine. Every optional part is there, so a type
    /// that must not carry one shows it by leaving it out.
    fn machine() -> Parts {
        Parts {
            install_id: "0a4e1c52-7ab0-4a5c-9d34-0f7f5a3e9c11".into(),
            user_token: "token".into(),
            created_at: "2026-11-21T10:00:00Z".into(),
            system: SystemProfile {
                distro: "CachyOS Linux".into(),
                kernel: "7.2.8-1-cachyos".into(),
                desktop: "KDE".into(),
                session: "wayland".into(),
                cpu: "AMD Ryzen 7 7840HS".into(),
                ram_gb: 16,
                gpus: vec![Gpu { vendor: "amd".into(), name: "Radeon 780M".into(), driver: "mesa 26.2.4".into() }],
                proton: "cachyos-11.0-20260703-slr".into(),
                umu: "1.4.4".into(),
                runtime: "steamrt4 4.0.20260928.262390".into(),
                prefix_kind: "blizznux".into(),
            },
            battlenet: BattlenetInfo { build: "17896".into(), version: "2.53.4.17896".into() },
            game: Some(GameInfo { code: "WoW".into(), name: "World of Warcraft".into(), flavor: "_retail_".into(), version: "12.0.1.65432".into() }),
            lab: Vec::new(),
            log: Some("=== 2026-11-21T09:59:00Z ===\nException: ACCESS_VIOLATION".into()),
            crash: Some("Exception: ACCESS_VIOLATION".into()),
        }
    }

    #[test]
    fn a_bug_report_carries_the_documented_fields() {
        let body = assemble("bug", None, Some("it broke"), machine()).unwrap();
        assert_eq!(
            fields(&body),
            ["battlenet", "comment", "crash", "created_at", "game", "install_id", "launcher_version", "log", "schema", "system", "type", "user_token"]
        );
        // No game to attach a crash file to, no comment, no fresh crash file: none of them is sent.
        let mut p = machine();
        p.game = None;
        p.crash = None;
        let body = assemble("bug", None, Some(""), p).unwrap();
        assert_eq!(fields(&body), ["battlenet", "created_at", "install_id", "launcher_version", "log", "schema", "system", "type", "user_token"]);
    }

    #[test]
    fn a_run_report_carries_the_log_and_crash_only_on_problems() {
        let run = |outcome| assemble("run", Some(outcome), Some(""), machine()).unwrap();
        for outcome in ["perfect", "ok"] {
            let body = run(outcome);
            assert_eq!(
                fields(&body),
                ["battlenet", "comment", "created_at", "game", "install_id", "launcher_version", "outcome", "schema", "system", "type", "user_token"]
            );
            assert_eq!(body["comment"], "");
        }
        for outcome in ["issues", "broken"] {
            let body = run(outcome);
            assert_eq!(
                fields(&body),
                ["battlenet", "comment", "crash", "created_at", "game", "install_id", "launcher_version", "log", "outcome", "schema", "system", "type", "user_token"]
            );
        }
    }

    #[test]
    fn a_launch_report_carries_the_log_only_when_it_failed() {
        let launch = |outcome| {
            let mut p = machine();
            p.game = None;
            assemble("launch", Some(outcome), Some("ignored"), p).unwrap()
        };
        let ok = launch("ok");
        assert_eq!(fields(&ok), ["battlenet", "created_at", "install_id", "launcher_version", "outcome", "schema", "system", "target", "type", "user_token"]);
        let failed = launch("failed");
        // No game, so no crash file, and a launch carries no comment.
        assert_eq!(fields(&failed), ["battlenet", "created_at", "install_id", "launcher_version", "log", "outcome", "schema", "system", "target", "type", "user_token"]);
    }

    #[test]
    fn a_labs_setting_adds_the_lab_field_and_nothing_else() {
        let mut p = machine();
        p.lab = vec!["wow64".into()];
        let body = assemble("bug", None, None, p).unwrap();
        assert_eq!(body["lab"], serde_json::json!(["wow64"]));
        let plain = assemble("bug", None, None, machine()).unwrap();
        assert!(plain.get("lab").is_none());
        assert_eq!(fields(&body).len(), fields(&plain).len() + 1);
    }

    #[test]
    fn a_report_the_launcher_never_sends_is_still_refused() {
        assert!(matches!(assemble("bogus", None, None, machine()), Err(BuildError::Invalid(_))));
        assert!(matches!(assemble("launch", Some("perfect"), None, machine()), Err(BuildError::Invalid(_))));
        assert!(matches!(assemble("run", Some("failed"), None, machine()), Err(BuildError::Invalid(_))));
        let mut ptr = machine();
        if let Some(g) = &mut ptr.game {
            g.flavor = "_ptr_".into();
        }
        assert!(matches!(assemble("run", Some("ok"), None, ptr), Err(BuildError::Invalid(_))));
        let mut unknown = machine();
        if let Some(g) = &mut unknown.game {
            g.version = String::new();
        }
        match assemble("run", Some("ok"), None, unknown) {
            Err(BuildError::UnknownBuild(why)) => assert!(why.contains("World of Warcraft")),
            _ => panic!("a run report without a build must be refused"),
        }
    }

    #[test]
    fn the_sites_answer_to_a_report_is_read_by_its_status() {
        assert!(matches!(answer(201, r#"{"url":"https://blizznux.com/d/1"}"#), Ok(u) if u == "https://blizznux.com/d/1"));
        assert!(matches!(answer(202, r#"{"status":"not tracked"}"#), Ok(u) if u.is_empty()));
        assert!(matches!(answer(401, r#"{"error":"Log in to BlizzNux to send reports","code":"login_required"}"#), Err(SendError::LoginRequired(w)) if w == "Log in to BlizzNux to send reports"));
        assert!(matches!(answer(401, ""), Err(SendError::LoginRequired(w)) if w == LOGIN_NEEDED));
        assert!(matches!(answer(400, r#"{"error":"bad body"}"#), Err(SendError::Refused(w)) if w.contains("bad body") && w.contains("did not accept")));
        assert!(matches!(answer(429, ""), Err(SendError::Unreachable(_))));
        assert!(matches!(answer(503, ""), Err(SendError::Unreachable(_))));
    }

    #[test]
    fn a_report_too_large_is_its_own_answer_and_kept() {
        assert!(matches!(answer(413, r#"{"error":"body over 256 KB"}"#), Err(SendError::TooLarge(w)) if w == "body over 256 KB"));
        assert!(matches!(answer(413, ""), Err(SendError::TooLarge(w)) if w.contains("413")));
        // Any other 4xx is still a plain refusal.
        assert!(matches!(answer(422, ""), Err(SendError::Refused(_))));
    }

    #[test]
    fn a_redirect_is_a_refusal_and_never_a_delivery() {
        assert!(matches!(answer(302, ""), Err(SendError::Refused(w)) if w.contains("302") && w.contains("redirect")));
        assert!(matches!(answer(307, ""), Err(SendError::Refused(w)) if w.contains("307")));
    }

    #[test]
    fn a_report_the_site_finds_too_large_loses_its_crash_then_its_old_log() {
        let log: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let mut body = serde_json::json!({ "type": "bug", "crash": "Exception: ACCESS_VIOLATION", "log": log });
        assert!(trim_large(&mut body));
        assert!(body.get("crash").is_none(), "the crash file goes before the log is cut");
        let lines: Vec<&str> = body["log"].as_str().unwrap().lines().collect();
        assert_eq!(lines.len(), TRIM_LOG_LINES);
        assert!(lines.last().unwrap().ends_with("line 199"), "the newest lines stay");
        // Nothing left to cut: no crash file, and the log is short enough already.
        assert!(!trim_large(&mut body));
        let mut short = serde_json::json!({ "type": "launch", "crash": "Exception", "log": "one\ntwo" });
        assert!(trim_large(&mut short));
        assert!(short.get("crash").is_none() && short["log"] == "one\ntwo");
        assert!(!trim_large(&mut serde_json::json!({ "type": "launch" })));
    }

    #[test]
    fn a_saved_report_naming_no_build_is_not_sent_against_one() {
        // An empty saved build used to match every build, so the report went out anyway.
        assert!(!build_matches("", Some("12.0.1.65432")));
        assert!(!build_matches("", None));
        // A game patched since the answer is another build, and a body without one is no match.
        assert!(!build_matches("12.0.1.65432", Some("12.1.0.69933")));
        assert!(!build_matches("12.0.1.65432", None));
        assert!(build_matches("12.0.1.65432", Some("12.0.1.65432")));
    }

    #[test]
    fn a_report_kept_as_too_large_is_stored_trimmed_and_without_the_token() {
        let log: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let mut body = serde_json::json!({ "type": "bug", "user_token": "secret", "crash": "Exception: ACCESS_VIOLATION", "log": log });
        assert!(trim_large(&mut body));
        let mut pending = saved(1, None, false, Some("karl"));
        keep_posted(&mut pending, body);
        let stored = pending.report.expect("the posted body is what stays saved");
        assert!(stored.get("user_token").is_none(), "the token never goes to disk");
        assert!(stored.get("crash").is_none());
        assert_eq!(stored["log"].as_str().unwrap().lines().count(), TRIM_LOG_LINES);
        assert!(stored["log"].as_str().unwrap().ends_with("line 199"), "the newest lines are kept");
    }

    #[test]
    fn a_report_with_no_game_build_is_not_saved() {
        let got = build_outcome(Err(BuildError::UnknownBuild("the launcher cannot tell which build of World of Warcraft is installed".into())));
        // Nothing is kept: the old code saved it and said so, and the next flush dropped it.
        assert_eq!(got, BuildOutcome::Error(UNKNOWN_BUILD_NOT_SAVED.into()));
        assert!(UNKNOWN_BUILD_NOT_SAVED.contains("not saved") && UNKNOWN_BUILD_NOT_SAVED.contains("the game has started once"));
        // Other build errors reach the user as they are, and a body that builds is sent.
        assert_eq!(build_outcome(Err(BuildError::Invalid("kind must be bug, run or launch".into()))), BuildOutcome::Error("kind must be bug, run or launch".into()));
        assert!(matches!(build_outcome(Ok(serde_json::json!({ "type": "bug" }))), BuildOutcome::Send(_)));
    }

    #[test]
    fn a_rate_limited_report_is_tried_three_times_with_a_short_capped_wait() {
        assert_eq!(retry_delay(1, None), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(2, None), Some(Duration::from_secs(2)));
        // A wait the site names is honoured while it is short ...
        assert_eq!(retry_delay(1, Some(1)), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(1, Some(2)), Some(Duration::from_secs(2)));
        // ... and a longer one ends the tries for this flush; the report stays saved.
        assert_eq!(retry_delay(1, Some(30)), None);
        // There is no fourth try.
        assert_eq!(retry_delay(3, None), None);
        assert_eq!(retry_delay(3, Some(1)), None);
    }

    fn saved(saved_at: u64, refused_by: Option<&str>, needs_login: bool, account: Option<&str>) -> Pending {
        Pending {
            kind: "run".into(),
            game: Some("WoW".into()),
            outcome: Some("perfect".into()),
            comment: None,
            saved_at,
            build: String::new(),
            report: None,
            refused_by: refused_by.map(String::from),
            needs_login,
            account: account.map(String::from),
            lab: Vec::new(),
        }
    }

    #[test]
    fn a_saved_report_waits_for_a_login_and_is_never_retried_in_a_loop() {
        let now = 10 * PENDING_SECS;
        // Logged out: everything waits, nothing is sent and nothing is marked refused.
        assert_eq!(held(&saved(now, None, true, Some("alice")), None, None, now), Held::Wait);
        assert_eq!(held(&saved(now, None, false, Some("alice")), None, None, now), Held::Wait);
        // Once logged in as that account it goes out.
        assert_eq!(held(&saved(now, None, true, Some("alice")), Some("alice"), None, now), Held::Send);
        // A report the site refused waits for the next launcher version.
        assert_eq!(held(&saved(now, Some(env!("CARGO_PKG_VERSION")), false, Some("alice")), Some("alice"), None, now), Held::Wait);
        assert_eq!(held(&saved(now, Some("0.0.1"), false, Some("alice")), Some("alice"), None, now), Held::Send);
        // A week later it is gone, logged in or not.
        assert_eq!(held(&saved(now - PENDING_SECS - 1, None, true, Some("alice")), None, None, now), Held::Expired);
        assert_eq!(held(&saved(now - PENDING_SECS - 1, None, true, Some("alice")), Some("alice"), None, now), Held::Expired);
    }

    #[test]
    fn a_saved_report_goes_out_only_for_the_account_it_was_written_for() {
        let now = 10 * PENDING_SECS;
        let report = saved(now, None, false, Some("alice"));
        // The same account linked again, whatever the case of the name: it is sent.
        assert_eq!(held(&report, Some("alice"), None, now), Held::Send);
        assert_eq!(held(&report, Some("Alice"), None, now), Held::Send);
        // Another account: kept as it is, so it is never sent as, or attributed to, that one.
        assert_eq!(held(&report, Some("bob"), None, now), Held::OtherAccount);
        // Names are compared whole: no partial match.
        for other in ["alice2", "alic", "bob"] {
            assert_eq!(held(&report, Some(other), None, now), Held::OtherAccount);
        }
        // It still expires a week after it was written, whichever account is linked by then.
        let old = saved(now - PENDING_SECS - 1, None, false, Some("alice"));
        assert_eq!(held(&old, Some("bob"), None, now), Held::Expired);
    }

    #[test]
    fn a_report_saved_before_accounts_were_written_down_goes_out_unless_another_link_was_forgotten() {
        let old = r#"{"kind":"run","game":"WoW","outcome":"perfect","comment":null,"saved_at":10,"build":"","report":null,"refused_by":null}"#;
        let p: Pending = serde_json::from_str(old).unwrap();
        assert_eq!(p.account, None);
        // No link was ever forgotten here: it goes out under the account linked, as it always did.
        assert_eq!(held(&p, Some("alice"), None, 10), Held::Send);
        assert_eq!(held(&p, Some("bob"), None, 10), Held::Send);
        // A link was forgotten and the same person is back: still theirs to send, whatever the case.
        assert_eq!(held(&p, Some("alice"), Some("alice"), 10), Held::Send);
        assert_eq!(held(&p, Some("Alice"), Some("ALICE"), 10), Held::Send);
        // Another account linked since: the report may be the forgotten one's, so it stays.
        assert_eq!(held(&p, Some("bob"), Some("alice"), 10), Held::OtherAccount);
        // Logged out it waits, whichever account came before.
        assert_eq!(held(&p, None, Some("alice"), 10), Held::Wait);
    }

    #[test]
    fn only_a_stored_link_with_a_name_is_an_account() {
        let mut c = BTreeMap::new();
        assert_eq!(account_in(&c), None);
        c.insert("USERNAME".into(), "alice".into());   // a name without a link is not a login
        assert_eq!(account_in(&c), None);
        c.insert("USER_TOKEN".into(), "token".into());
        assert_eq!(account_in(&c), Some("alice".into()));
        c.insert("USERNAME".into(), " alice ".into());   // the name the site gave, without space
        assert_eq!(account_in(&c), Some("alice".into()));
        c.insert("USER_TOKEN".into(), String::new());    // logged out: nothing to attribute
        assert_eq!(account_in(&c), None);
        c.insert("USER_TOKEN".into(), "token".into());
        c.remove("USERNAME");                            // a token alone names nobody
        assert_eq!(account_in(&c), None);
    }

    #[test]
    fn a_forgotten_link_names_the_account_it_was_forgotten_for() {
        let mut c = BTreeMap::new();
        assert_eq!(last_account_in(&c), None);            // nothing was forgotten on this install
        c.insert("LAST_USERNAME".into(), "  ".into());
        assert_eq!(last_account_in(&c), None);            // a blank name is no name
        c.insert("LAST_USERNAME".into(), " alice ".into());
        assert_eq!(last_account_in(&c), Some("alice".into()));
    }

    #[test]
    fn a_saved_report_file_names_the_account_and_carries_no_token() {
        let dir = std::env::temp_dir().join(format!("blizznux-outbox-test-{}", std::process::id()));
        let path = dir.join("1.json");
        let mut p = saved(10, None, false, Some("alice"));
        p.report = Some(serde_json::json!({ "schema": 1, "type": "run" }));
        save_pending(&path, &p).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(text.contains("\"account\": \"alice\""), "{text}");
        assert!(!text.contains("user_token"), "{text}");
        // It reads back as the same report, and stays put under another account.
        let back: Pending = serde_json::from_str(&text).unwrap();
        assert_eq!(back.account.as_deref(), Some("alice"));
        assert_eq!(held(&back, Some("bob"), None, 10), Held::OtherAccount);
        assert_eq!(held(&back, Some("ALICE"), None, 10), Held::Send);
    }

    #[test]
    fn a_report_saved_by_an_older_launcher_reads_as_not_waiting_for_a_login() {
        let old = r#"{"kind":"run","game":"WoW","outcome":"perfect","comment":null,"saved_at":1,"build":"","report":null,"refused_by":null}"#;
        let p: Pending = serde_json::from_str(old).unwrap();
        assert!(!p.needs_login);
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
    fn ram_rounds_up_to_the_installed_size() {
        assert_eq!(installed_ram_gb(32_822_000), 32); // 31.3 GiB on a 32 GB box
        assert_eq!(installed_ram_gb(15_524_700), 16); // 16 GB laptop minus iGPU carve-out
        assert_eq!(installed_ram_gb(16 * 1024 * 1024), 16);
        assert_eq!(installed_ram_gb(7_900_000), 8);
        assert_eq!(installed_ram_gb(200 * 1024 * 1024), 256);
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

    #[test]
    fn wow_counts_as_closed_once_this_sessions_graphics_log_says_so() {
        let prefix = std::env::temp_dir().join(format!("blizznux-gxlog-test-{}", std::process::id()));
        let logs = prefix.join("drive_c/Program Files (x86)/World of Warcraft/_retail_/Logs");
        fs::create_dir_all(&logs).unwrap();
        let wow = GAMES.iter().find(|g| g.0 == "WoW").unwrap();
        let hots = GAMES.iter().find(|g| g.0 == "Hero").unwrap();
        let gx = logs.join("gx.log");

        // The log of the previous run, written before this session started, does not count.
        fs::write(&gx, "LogOpen\nGxShutdown\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let seen = Instant::now();
        assert!(!wow_window_closed(&prefix, wow, "_retail_", seen, 0));

        // File times come from the kernel's coarse clock, which can lag real time by a tick, so a
        // log written right after `seen` could look older than the session. Real WoW writes its
        // log seconds later; give this run's log the same clear margin.
        std::thread::sleep(Duration::from_millis(50));

        // This run's log: still playing, then the window closes.
        fs::write(&gx, "LogOpen\nD3d12 Device Created\n").unwrap();
        assert!(!wow_window_closed(&prefix, wow, "_retail_", seen, 0));
        fs::write(&gx, "LogOpen\nD3d12 Device Created\nGxShutdown\n").unwrap();
        assert!(wow_window_closed(&prefix, wow, "_retail_", seen, 0));

        // Only World of Warcraft is judged by its log.
        assert!(!wow_window_closed(&prefix, hots, "", seen, 0));
        fs::remove_dir_all(&prefix).unwrap();
    }
}
