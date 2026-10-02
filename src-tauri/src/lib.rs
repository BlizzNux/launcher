// BlizzNux launcher — desktop app. GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tauri::webview::WebviewBuilder;
use tauri::window::WindowBuilder;
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WindowEvent};

const FORUM: &str = "https://blizznux.com/";
const FORUM_HOST: &str = "blizznux.com";
const UI: &str = "ui";
const COMMUNITY: &str = "community";
/// Height of the control bar at the bottom of the window, in logical pixels.
const BAR: f64 = 64.0;

/// Whether the control webview is the bottom bar or covers the window (settings, disclaimer).
#[derive(Clone, Copy, PartialEq)]
enum UiMode {
    Bar,
    Full,
}

struct UiState(Mutex<UiMode>);

fn layout(app: &AppHandle, window: &tauri::Window) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let Ok(size) = window.inner_size() else { return };
    let size = size.to_logical::<f64>(scale);
    let mode = app.state::<UiState>().0.lock().map(|m| *m).unwrap_or(UiMode::Bar);
    let bar_h = BAR.min(size.height);
    if let Some(c) = app.get_webview(COMMUNITY) {
        let _ = c.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = c.set_size(LogicalSize::new(size.width, (size.height - bar_h).max(1.0)));
    }
    if let Some(ui) = app.get_webview(UI) {
        match mode {
            UiMode::Bar => {
                let _ = ui.set_position(LogicalPosition::new(0.0, size.height - bar_h));
                let _ = ui.set_size(LogicalSize::new(size.width, bar_h));
            }
            UiMode::Full => {
                let _ = ui.set_position(LogicalPosition::new(0.0, 0.0));
                let _ = ui.set_size(LogicalSize::new(size.width, size.height));
            }
        }
    }
}

#[tauri::command]
fn set_ui_mode(app: AppHandle, mode: String) -> Result<(), String> {
    let m = match mode.as_str() {
        "bar" => UiMode::Bar,
        "full" => UiMode::Full,
        _ => return Err("mode must be bar or full".into()),
    };
    *app.state::<UiState>().0.lock().map_err(|e| e.to_string())? = m;
    let window = app.get_window("main").ok_or("main window missing")?;
    layout(&app, &window);
    Ok(())
}

#[tauri::command]
fn community_navigate(app: AppHandle, url: String) -> Result<(), String> {
    let u: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;
    if u.host_str() != Some(FORUM_HOST) {
        return Err("only blizznux.com can be shown in the community view".into());
    }
    let c = app.get_webview(COMMUNITY).ok_or("community webview missing")?;
    c.navigate(u).map_err(|e| e.to_string())
}

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
        .manage(UiState(Mutex::new(UiMode::Bar)))
        .setup(|app| {
            let window = WindowBuilder::new(app, "main")
                .title("BlizzNux")
                .inner_size(1180.0, 820.0)
                .min_inner_size(900.0, 600.0)
                .build()?;
            let scale = window.scale_factor()?;
            let size = window.inner_size()?.to_logical::<f64>(scale);
            window.add_child(
                WebviewBuilder::new(COMMUNITY, WebviewUrl::External(FORUM.parse().unwrap())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(size.width, (size.height - BAR).max(1.0)),
            )?;
            window.add_child(
                WebviewBuilder::new(UI, WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, size.height - BAR),
                LogicalSize::new(size.width, BAR),
            )?;
            let handle = app.handle().clone();
            let w = window.clone();
            window.on_window_event(move |e| {
                if matches!(e, WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }) {
                    layout(&handle, &w);
                }
            });
            // Resize events can arrive before the handler exists (e.g. the compositor maximizes
            // the window as it appears), so also reconcile whenever the size actually changes.
            let handle = app.handle().clone();
            let w = window.clone();
            std::thread::spawn(move || {
                let mut last = (0u32, 0u32, 0u64);
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let Ok(size) = w.inner_size() else { break };
                    let scale = (w.scale_factor().unwrap_or(1.0) * 1000.0) as u64;
                    let now = (size.width, size.height, scale);
                    if now != last {
                        last = now;
                        layout(&handle, &w);
                    }
                }
            });
            layout(app.handle(), &window);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_ui_mode,
            community_navigate,
            fetch_json,
            launch,
            doctor,
            set_dpi,
            read_config,
            write_config,
            app_version,
            open_external,
            read_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running BlizzNux");
}
