// BlizzNux launcher — desktop app. GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tauri::{AppHandle, Manager};

const FORUM_HOST: &str = "blizznux.com";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("blizznux").join("config")
}

fn log_path() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"));
    base.join("blizznux").join("blizznux.log")
}

/// The engine: bin/blizznux-run, bundled as a resource, next to a dev build, or on PATH.
fn script_path(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(dir) = app.path().resource_dir() {
        for rel in ["_up_/bin/blizznux-run", "bin/blizznux-run", "blizznux-run"] {
            let p = dir.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().take(6) {
            let p = dir.join("bin").join("blizznux-run");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = PathBuf::from(dir).join("blizznux-run");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[tauri::command]
async fn fetch_json(url: String) -> Result<serde_json::Value, String> {
    let u: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;
    match u.host_str() {
        Some(FORUM_HOST) | Some("api.github.com") => {}
        _ => return Err("host not allowed".into()),
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let r = client
        .get(u)
        .header("Accept", "application/vnd.api+json, application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    r.json::<serde_json::Value>().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn launch(app: AppHandle, game: Option<String>) -> Result<String, String> {
    let script = script_path(&app).ok_or("blizznux-run not found (run install.sh)")?;
    let mut cmd = Command::new(&script);
    if let Some(code) = game.as_deref().filter(|g| !g.is_empty()) {
        if !code.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("invalid game code".into());
        }
        cmd.arg("--game").arg(code);
    }
    // No terminal attached: the script writes ~/.cache/blizznux/blizznux.log itself.
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(script.display().to_string())
}

fn run_script(app: &AppHandle, args: &[&str]) -> Result<String, String> {
    let script = script_path(app).ok_or("blizznux-run not found (run install.sh)")?;
    let out = Command::new(&script)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(text) } else { Err(text) }
}

#[tauri::command]
async fn doctor(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || run_script(&app, &["doctor"]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_dpi(app: AppHandle, dpi: u32) -> Result<String, String> {
    if !(72..=384).contains(&dpi) {
        return Err("DPI must be between 72 and 384".into());
    }
    let d = dpi.to_string();
    tauri::async_runtime::spawn_blocking(move || run_script(&app, &["dpi", &d]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn read_config() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = fs::read_to_string(config_path()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if matches!(k, "PREFIX" | "PROTON" | "OFFLOAD") {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

#[tauri::command]
fn write_config(values: BTreeMap<String, String>) -> Result<(), String> {
    let mut current = read_config();
    for (k, v) in values {
        if matches!(k.as_str(), "PREFIX" | "PROTON" | "OFFLOAD") {
            current.insert(k, v.trim().to_string());
        }
    }
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = format!(
        "PREFIX={}\nPROTON={}\nOFFLOAD={}\n",
        current.get("PREFIX").cloned().unwrap_or_default(),
        current.get("PROTON").cloned().unwrap_or_default(),
        current.get("OFFLOAD").cloned().unwrap_or_else(|| "auto".into()),
    );
    fs::write(&path, text).map_err(|e| e.to_string())
}

const LAUNCHER_REL: &str = "drive_c/Program Files (x86)/Battle.net/Battle.net Launcher.exe";

fn current_prefix() -> PathBuf {
    let cfg = read_config();
    match cfg.get("PREFIX").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let base = std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home().join(".local").join("share"));
            base.join("blizznux").join("prefix")
        }
    }
}

#[derive(serde::Serialize)]
struct InstallState {
    prefix: String,
    installed: bool,
    umu: bool,
}

#[tauri::command]
fn install_state() -> InstallState {
    let prefix = current_prefix();
    let umu = std::env::var("PATH")
        .map(|p| p.split(':').any(|d| PathBuf::from(d).join("umu-run").is_file()))
        .unwrap_or(false);
    InstallState {
        installed: prefix.join(LAUNCHER_REL).is_file(),
        prefix: prefix.display().to_string(),
        umu,
    }
}

/// Downloads Blizzard's installer and starts it (the script does the work; this returns at once).
#[tauri::command]
fn install(app: AppHandle) -> Result<(), String> {
    let script = script_path(&app).ok_or("blizznux-run not found (run install.sh)")?;
    let mut child = Command::new(&script)
        .arg("install")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[tauri::command]
async fn import_prefix(app: AppHandle, path: String) -> Result<String, String> {
    let mut p = path.trim().to_string();
    if p.is_empty() {
        return Err("enter the path to the prefix".into());
    }
    if let Some(rest) = p.strip_prefix("~/") {
        p = home().join(rest).display().to_string();
    }
    tauri::async_runtime::spawn_blocking(move || run_script(&app, &["import", &p]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    let u: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;
    if u.scheme() != "https" {
        return Err("only https links can be opened".into());
    }
    Command::new("xdg-open")
        .arg(u.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn read_log() -> String {
    let text = fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(200);
    lines[start..].join("\n")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK renders with hardware acceleration by default. If the forum view shows
    // glitches on your GPU, start with WEBKIT_DISABLE_DMABUF_RENDERER=1 to use the fallback path.
    tauri::Builder::default()
        .setup(|app| {
            // blizznux.com is shown in a frame inside our own page. WebKitGTK refuses
            // third-party cookies by default, which would log the user out of the forum on
            // every visit, so allow cookies for the embedded site.
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.set_title(&format!("BlizzNux Launcher v{}", env!("CARGO_PKG_VERSION")));
                let _ = main.with_webview(|platform| {
                    #[cfg(target_os = "linux")]
                    {
                        use webkit2gtk::{CookieManagerExt, WebContextExt, WebViewExt};
                        if let Some(ctx) = platform.inner().context() {
                            if let Some(cm) = ctx.cookie_manager() {
                                cm.set_accept_policy(webkit2gtk::CookieAcceptPolicy::Always);
                            }
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            fetch_json,
            launch,
            doctor,
            set_dpi,
            read_config,
            write_config,
            install_state,
            install,
            import_prefix,
            app_version,
            open_external,
            read_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running BlizzNux");
}
