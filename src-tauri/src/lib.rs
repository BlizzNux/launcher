// BlizzNux launcher — desktop app. GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tauri::{AppHandle, Manager};

mod addons;
mod reports;
mod updates;

const FORUM_HOST: &str = "blizznux.com";

pub(crate) fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("blizznux").join("config")
}

pub(crate) fn log_path() -> PathBuf {
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
    reports::watch_launch(app.clone());
    reports::watch_game(app.clone(), game.clone());
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

pub(crate) fn load_config() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = fs::read_to_string(config_path()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if matches!(k, "PREFIX" | "PROTON" | "OFFLOAD" | "INSTALL_ID" | "REPORTS_AUTO" | "USER_TOKEN" | "USERNAME" | "REPORTS_SHARE" | "BUG_AUTO" | "LAST_LAUNCH_OK" | "LAST_RUN_OK") {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

#[tauri::command]
fn read_config() -> BTreeMap<String, String> {
    load_config()
}

pub(crate) fn save_config(current: &BTreeMap<String, String>) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = format!(
        "PREFIX={}\nPROTON={}\nOFFLOAD={}\n",
        current.get("PREFIX").cloned().unwrap_or_default(),
        current.get("PROTON").cloned().unwrap_or_default(),
        current.get("OFFLOAD").cloned().unwrap_or_else(|| "auto".into()),
    );
    for k in ["INSTALL_ID", "REPORTS_AUTO", "USER_TOKEN", "USERNAME", "REPORTS_SHARE", "BUG_AUTO", "LAST_LAUNCH_OK", "LAST_RUN_OK"] {
        if let Some(v) = current.get(k).filter(|v| !v.is_empty()) {
            text.push_str(&format!("{k}={v}\n"));
        }
    }
    fs::write(&path, text).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[tauri::command]
fn write_config(values: BTreeMap<String, String>) -> Result<(), String> {
    let mut current = load_config();
    for (k, v) in values {
        if matches!(k.as_str(), "PREFIX" | "PROTON" | "OFFLOAD" | "REPORTS_AUTO" | "REPORTS_SHARE" | "BUG_AUTO") {
            current.insert(k, v.trim().to_string());
        }
    }
    save_config(&current)
}

const LAUNCHER_REL: &str = "drive_c/Program Files (x86)/Battle.net/Battle.net Launcher.exe";

pub(crate) fn current_prefix() -> PathBuf {
    let cfg = load_config();
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

#[derive(serde::Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
    hint: String,
    blocking: bool,
}

fn on_path(bin: &str) -> bool {
    std::env::var("PATH")
        .map(|p| p.split(':').any(|d| PathBuf::from(d).join(bin).is_file()))
        .unwrap_or(false)
}

/// True if the file is a 32-bit ELF (used to tell lib32 drivers from 64-bit ones).
fn is_elf32(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else { return false };
    let mut head = [0u8; 5];
    f.read_exact(&mut head).is_ok() && &head[..4] == b"\x7fELF" && head[4] == 1
}

fn gpu_vendors() -> Vec<&'static str> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            if let Ok(id) = fs::read_to_string(e.path().join("device/vendor")) {
                let tag = match id.trim() {
                    "0x10de" => "nvidia",
                    "0x1002" => "amd",
                    "0x8086" => "intel",
                    _ => continue,
                };
                if !v.contains(&tag) {
                    v.push(tag);
                }
            }
        }
    }
    v
}

pub(crate) fn host_os_release() -> String {
    // Inside Flatpak /etc/os-release describes the runtime; the host's copy is at /run/host.
    fs::read_to_string("/run/host/os-release").or_else(|_| fs::read_to_string("/etc/os-release")).unwrap_or_default()
}

fn distro_family() -> &'static str {
    let text = host_os_release();
    let line = |k: &str| {
        text.lines()
            .find(|l| l.starts_with(k))
            .map(|l| l[k.len()..].trim_matches('"').to_lowercase())
            .unwrap_or_default()
    };
    let id = line("ID=");
    let like = line("ID_LIKE=");
    let s = format!("{id} {like}");
    if s.contains("arch") {
        "arch"
    } else if s.contains("fedora") || s.contains("rhel") {
        "fedora"
    } else if s.contains("debian") || s.contains("ubuntu") {
        "debian"
    } else {
        "other"
    }
}

fn pkg_hint(vendor: &str, family: &str) -> String {
    let (arch, fedora, debian) = match vendor {
        "nvidia" => ("lib32-nvidia-utils", "nvidia-driver-libs.i686 (RPM Fusion)", "libnvidia-gl-<version>:i386"),
        "amd" => ("lib32-vulkan-radeon", "mesa-vulkan-drivers.i686", "mesa-vulkan-drivers:i386"),
        "intel" => ("lib32-vulkan-intel", "mesa-vulkan-drivers.i686", "mesa-vulkan-drivers:i386"),
        _ => ("the 32-bit Vulkan driver for your GPU", "", ""),
    };
    match family {
        "arch" => arch.to_string(),
        "fedora" => fedora.to_string(),
        "debian" => format!("{debian} (after `dpkg --add-architecture i386`)"),
        _ => arch.to_string(),
    }
}

#[tauri::command]
fn readiness() -> Vec<Check> {
    let family = distro_family();
    let vendors = gpu_vendors();
    let mut out = Vec::new();

    let umu = on_path("umu-run");
    out.push(Check {
        name: "umu-launcher".into(),
        ok: umu,
        detail: if umu { "found".into() } else { "not found on this system".into() },
        hint: match family {
            "arch" => "sudo pacman -S umu-launcher".into(),
            "fedora" => "enable the umu COPR, then sudo dnf install umu-launcher".into(),
            _ => "install umu-launcher from https://github.com/Open-Wine-Components/umu-launcher/releases".into(),
        },
        blocking: true,
    });

    let curl = on_path("curl");
    out.push(Check {
        name: "curl".into(),
        ok: curl,
        detail: if curl { "found".into() } else { "needed to download the installer".into() },
        hint: "install the curl package".into(),
        blocking: true,
    });

    // 64-bit Vulkan: any ICD manifest at all.
    let icd_dirs = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d", "/usr/local/share/vulkan/icd.d"];
    let mut icds: Vec<String> = Vec::new();
    for d in icd_dirs {
        if let Ok(rd) = fs::read_dir(d) {
            for e in rd.flatten() {
                icds.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let vk64 = !icds.is_empty();
    out.push(Check {
        name: "Vulkan driver (64-bit)".into(),
        ok: vk64,
        detail: if vk64 { format!("{} driver manifest(s) found", icds.len()) } else { "no Vulkan driver manifests found".into() },
        hint: "install your GPU's Vulkan driver (mesa / nvidia-utils)".into(),
        blocking: true,
    });

    // 32-bit Vulkan: a Mesa i686 manifest, a 32-bit NVIDIA GLX library, or (Flatpak) any
    // manifest inside the 32-bit GL extension mount.
    let in_flatpak = std::env::var_os("FLATPAK_ID").is_some();
    let ext32 = ["/app/lib/i386-linux-gnu/GL/vulkan/icd.d", "/usr/lib/i386-linux-gnu/GL/vulkan/icd.d", "/usr/lib/i386-linux-gnu/GL/default/share/vulkan/icd.d"]
        .iter()
        .any(|d| fs::read_dir(d).map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".json"))).unwrap_or(false));
    let mesa32 = icds.iter().any(|n| n.contains("i686") || n.contains("i386")) || (in_flatpak && ext32);
    let nvidia32 = [
        "/usr/lib32/libGLX_nvidia.so.0",
        "/usr/lib/i386-linux-gnu/libGLX_nvidia.so.0",
        "/usr/lib/libGLX_nvidia.so.0",
    ]
    .iter()
    .any(|p| is_elf32(std::path::Path::new(p)));
    let needs_nvidia32 = vendors.contains(&"nvidia");
    let needs_mesa32 = vendors.iter().any(|v| *v == "amd" || *v == "intel");
    let vk32 = if in_flatpak { ext32 } else { (!needs_nvidia32 || nvidia32) && (!needs_mesa32 || mesa32) && (nvidia32 || mesa32) };
    let hint = vendors
        .iter()
        .filter(|v| match **v { "nvidia" => !nvidia32, _ => !mesa32 })
        .map(|v| pkg_hint(v, family))
        .collect::<Vec<_>>()
        .join(" and ");
    out.push(Check {
        name: "Vulkan driver (32-bit)".into(),
        ok: vk32,
        detail: if vk32 {
            "found".into()
        } else {
            format!("missing for {} — Battle.net and older games need it", if vendors.is_empty() { "your GPU".into() } else { vendors.join(" + ") })
        },
        hint: if in_flatpak { "flatpak install org.freedesktop.Platform.GL32.default and the GL32 extension matching your driver".into() } else if hint.is_empty() { pkg_hint("", family) } else { hint },
        blocking: false,
    });
    // Debug aid: BLIZZNUX_FAKE_MISSING="umu-launcher,curl" makes those checks fail on purpose.
    if let Ok(fake) = std::env::var("BLIZZNUX_FAKE_MISSING") {
        for c in out.iter_mut() {
            if fake.split(',').any(|f| f.trim() == c.name) {
                c.ok = false;
                c.detail = format!("{} (simulated)", c.detail);
            }
        }
    }
    out
}

/// NVIDIA driver major version from the kernel module, digits only (e.g. "595").
fn nvidia_major() -> Option<String> {
    let text = fs::read_to_string("/proc/driver/nvidia/version").ok()?;
    let token = text.split_whitespace().find(|t| t.chars().next().map_or(false, |c| c.is_ascii_digit()) && t.contains('.'))?;
    let major: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
    if major.is_empty() { None } else { Some(major) }
}

fn multilib_enabled() -> bool {
    fs::read_to_string("/etc/pacman.conf")
        .map(|t| t.lines().any(|l| l.trim() == "[multilib]"))
        .unwrap_or(false)
}

/// The exact root commands that would fix a failed readiness check, or None if we don't know how.
fn fix_steps(check: &str) -> Option<Vec<String>> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return None; // no pkexec inside the sandbox; the hint text explains what to install
    }
    let family = distro_family();
    let vendors = gpu_vendors();
    let mut steps: Vec<String> = Vec::new();
    match (check, family) {
        ("umu-launcher", "arch") => steps.push("pacman -S --needed --noconfirm umu-launcher".into()),
        ("umu-launcher", "fedora") => steps.push("dnf -y install umu-launcher".into()),
        ("umu-launcher", "debian") => {
            steps.push("apt-get update".into());
            steps.push("apt-get install -y umu-launcher".into());
        }
        ("curl", "arch") => steps.push("pacman -S --needed --noconfirm curl".into()),
        ("curl", "fedora") => steps.push("dnf -y install curl".into()),
        ("curl", "debian") => {
            steps.push("apt-get update".into());
            steps.push("apt-get install -y curl".into());
        }
        ("Vulkan driver (32-bit)", fam) => {
            let mut pkgs: Vec<String> = Vec::new();
            for v in &vendors {
                let pkg = match (*v, fam) {
                    ("nvidia", "arch") => "lib32-nvidia-utils".to_string(),
                    ("amd", "arch") => "lib32-vulkan-radeon".to_string(),
                    ("intel", "arch") => "lib32-vulkan-intel".to_string(),
                    ("nvidia", "fedora") => "nvidia-driver-libs.i686".to_string(),
                    (_, "fedora") => "mesa-vulkan-drivers.i686".to_string(),
                    ("nvidia", "debian") => format!("libnvidia-gl-{}:i386", nvidia_major()?),
                    (_, "debian") => "mesa-vulkan-drivers:i386".to_string(),
                    _ => return None,
                };
                if !pkgs.contains(&pkg) {
                    pkgs.push(pkg);
                }
            }
            if pkgs.is_empty() {
                return None;
            }
            match fam {
                "arch" => {
                    if !multilib_enabled() {
                        steps.push("sed -i '/^#\\[multilib\\]/,/^#Include/ s/^#//' /etc/pacman.conf".into());
                        steps.push("pacman -Sy".into());
                    }
                    steps.push(format!("pacman -S --needed --noconfirm {}", pkgs.join(" ")));
                }
                "fedora" => steps.push(format!("dnf -y install {}", pkgs.join(" "))),
                "debian" => {
                    steps.push("dpkg --add-architecture i386".into());
                    steps.push("apt-get update".into());
                    steps.push(format!("apt-get install -y {}", pkgs.join(" ")));
                }
                _ => return None,
            }
        }
        _ => return None,
    }
    Some(steps)
}

#[tauri::command]
fn fix_plan(check: String) -> Option<Vec<String>> {
    fix_steps(&check)
}

/// Runs the fix plan as root through the system's polkit prompt (pkexec). Returns the output.
#[tauri::command]
async fn fix_apply(check: String) -> Result<String, String> {
    let steps = fix_steps(&check).ok_or("no automatic fix is known for this system")?;
    if !on_path("pkexec") {
        return Err("pkexec (polkit) is not available; run the commands shown in a terminal with sudo".into());
    }
    let script = steps.iter().map(|s| format!("echo \"+ {s}\"; {s}")).collect::<Vec<_>>().join(" && ");
    tauri::async_runtime::spawn_blocking(move || {
        let out = Command::new("pkexec")
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            Ok(text)
        } else {
            Err(if text.trim().is_empty() { format!("exit status {}", out.status) } else { text })
        }
    })
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

/// Headless commands: `blizznux --cli wow-installs|wowup-status|wowup-install|wowup-seed|readiness`
pub fn cli(cmd: &str, _rest: &[String]) -> i32 {
    let out = match cmd {
        "wow-installs" => serde_json::to_string_pretty(&addons::wow_installs()),
        "wowup-status" => serde_json::to_string_pretty(&addons::wowup_status()),
        "wowup-install" => match tauri::async_runtime::block_on(addons::wowup_install()) {
            Ok(v) => Ok(format!("installed WowUp {v}")),
            Err(e) => { eprintln!("error: {e}"); return 1; }
        },
        "wowup-seed" => match addons::seed_wowup_installs() {
            Ok(n) => Ok(format!("registered {n} install(s) in {}", addons::wowup_config_dir().display())),
            Err(e) => { eprintln!("error: {e}"); return 1; }
        },
        "readiness" => serde_json::to_string_pretty(&readiness()),
        "battlenet-update" => match tauri::async_runtime::block_on(updates::battlenet_update_check()) {
            Ok(v) => serde_json::to_string_pretty(&v),
            Err(e) => { eprintln!("error: {e}"); return 1; }
        },
        "report-bug" | "report-run" => {
            let kind = if cmd == "report-bug" { "bug" } else { "run" };
            let game = _rest.first().cloned();
            let outcome = _rest.get(1).cloned();
            let comment = _rest.get(2).cloned();
            match reports::build_report(kind.into(), game, outcome, comment) {
                Ok(body) => {
                    println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
                    if std::env::var_os("BLIZZNUX_SEND").is_some() {
                        match tauri::async_runtime::block_on(reports::send_report(body)) {
                            Ok(url) => Ok(format!("sent → {url}")),
                            Err(e) => { eprintln!("send error: {e}"); return 1; }
                        }
                    } else { Ok(String::from("(not sent; set BLIZZNUX_SEND=1 to send)")) }
                }
                Err(e) => { eprintln!("error: {e}"); return 1; }
            }
        }
        _ => { eprintln!("unknown command"); return 2; }
    };
    match out {
        Ok(text) => { println!("{text}"); 0 }
        Err(e) => { eprintln!("error: {e}"); 1 }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// KWin on Wayland finds a window's icon only through a desktop file named exactly like the
/// window's app id (no StartupWMClass fallback, case-sensitive). GTK derives the app id from the
/// program name, i.e. the binary "blizznux", which matches nothing: install.sh ships
/// com.blizznux.launcher.desktop and the deb/rpm/AppImage bundles ship BlizzNux.desktop. So the
/// program name is set to whichever of those is installed, before GTK initialises.
fn set_app_id() {
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => dirs.push(home().join(".local/share")),
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    for id in ["com.blizznux.launcher", "BlizzNux"] {
        if dirs.iter().any(|d| d.join("applications").join(format!("{id}.desktop")).is_file()) {
            webkit2gtk::glib::set_prgname(Some(id));
            return;
        }
    }
}

pub fn run() {
    set_app_id();
    // WebKitGTK renders with hardware acceleration by default. If the forum view shows
    // glitches on your GPU, start with WEBKIT_DISABLE_DMABUF_RENDERER=1 to use the fallback path.
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // blizznux.com is shown in a frame inside our own page. WebKitGTK refuses
            // third-party cookies by default, which would log the user out of the forum on
            // every visit, so allow cookies for the embedded site.
            // Test aid: BLIZZNUX_AUTO_UPDATE=1 installs an available update without clicking.
            if std::env::var_os("BLIZZNUX_AUTO_UPDATE").is_some() {
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match updates::update_install(h).await {
                        Ok(()) => {}
                        Err(e) => eprintln!("[auto-update] {e}"),
                    }
                });
            }
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
                        // BLIZZNUX_NET_DEBUG=1: print the site's console and every request status to stderr.
                        if std::env::var_os("BLIZZNUX_NET_DEBUG").is_some() {
                            use webkit2gtk::{SettingsExt, UserContentManagerExt};
                            let wv = platform.inner();
                            if let Some(settings) = WebViewExt::settings(&wv) {
                                settings.set_enable_write_console_messages_to_stdout(true);
                            }
                            if let Some(ucm) = wv.user_content_manager() {
                                let script = webkit2gtk::UserScript::new(
                                    NET_DEBUG_JS,
                                    webkit2gtk::UserContentInjectedFrames::AllFrames,
                                    webkit2gtk::UserScriptInjectionTime::Start,
                                    &[],
                                    &[],
                                );
                                ucm.add_script(&script);
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
            readiness,
            fix_plan,
            fix_apply,
            addons::wow_installs,
            addons::list_addons,
            addons::remove_addon,
            addons::open_addons_folder,
            addons::install_addon_bytes,
            addons::install_addon_url,
            addons::wowup_status,
            addons::wowup_install,
            addons::wowup_launch,
            addons::wowup_remove,
            reports::build_report,
            reports::send_report,
            reports::account_status,
            reports::link_start,
            reports::link_poll,
            reports::link_cancel,
            reports::unlink_account,
            updates::update_check,
            updates::update_install,
            updates::battlenet_update_check,
            app_version,
            open_external,
            read_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running BlizzNux");
}

/// Request/response tracer injected into every frame when BLIZZNUX_NET_DEBUG is set.
/// Logs method, URL and status only; response bodies are logged for failures alone.
#[cfg(target_os = "linux")]
const NET_DEBUG_JS: &str = r#"(function(){
  if (!/blizznux\.com$/.test(location.hostname)) return;
  var log = function(kind, method, url, status, body){ try { console.log('NETDBG ' + kind + ' ' + method + ' ' + url + ' -> ' + status + (body ? ' :: ' + body : '')); } catch(e){} };
  var skip = function(u){ return /\.(js|css|png|svg|woff2?)(\?|$)/.test(u) || /cdn-cgi\/rum/.test(u); };
  var open = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function(m, u){ this.__m = m; this.__u = String(u); return open.apply(this, arguments); };
  var send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function(){ var x = this; x.addEventListener('loadend', function(){ if (skip(x.__u)) return; var ok = x.status >= 200 && x.status < 300; var b = ''; try { b = ok ? '' : String(x.responseText).slice(0, 300); } catch(e){} log('xhr', x.__m, x.__u, x.status, b); }); return send.apply(this, arguments); };
  var f = window.fetch;
  window.fetch = function(input, init){ var u = typeof input === 'string' ? input : (input && input.url) || ''; var m = (init && init.method) || (input && input.method) || 'GET'; return f.apply(this, arguments).then(function(r){ if (!skip(u)) { if (r.ok) log('fetch', m, u, r.status, ''); else r.clone().text().then(function(t){ log('fetch', m, u, r.status, t.slice(0,300)); }, function(){ log('fetch', m, u, r.status, ''); }); } return r; }, function(e){ log('fetch', m, u, 'ERR', String(e)); throw e; }); };
  window.addEventListener('error', function(e){ log('jserror', '', location.pathname, '', String(e.message)); });
  window.addEventListener('unhandledrejection', function(e){ log('rejection', '', location.pathname, '', String(e.reason && (e.reason.message || e.reason)).slice(0, 300)); });
})();"#;
