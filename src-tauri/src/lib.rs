// BlizzNux launcher — desktop app. GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tauri::{AppHandle, Manager};

mod addons;
#[cfg(target_os = "linux")]
mod glprobe;
#[cfg(target_os = "linux")]
mod notify;
mod reports;
mod updates;

const FORUM_HOST: &str = "blizznux.com";

pub(crate) fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// A command for a program of the host system. Inside an AppImage the start script points
/// the library, Python and data paths into the image; host programs break on those (umu-run
/// is Python and cannot start with the image's PYTHONHOME), so they get the environment
/// without them and do not run from inside the mount.
pub(crate) fn host_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    if let Some(appdir) = std::env::var("APPDIR").ok().filter(|d| !d.is_empty()) {
        cmd.env_clear().envs(updates::env_outside_appimage(std::env::vars_os(), &appdir)).current_dir(home());
    }
    #[cfg(target_os = "linux")]
    glprobe::scrub_child_env(&mut cmd);
    cmd
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("blizznux").join("config")
}

/// umu-run: the launcher's own copy, which the wrapper downloads into the data folder on first
/// use; the Flatpak carries the same release inside. A umu-launcher of the system is not used,
/// unless the packager points BLIZZNUX_UMU at it (not in the app; it wins).
pub(crate) fn umu_run() -> Option<PathBuf> {
    if let Some(p) = packaged_umu() {
        return Some(p).filter(|f| executable_file(f));
    }
    let bundled = PathBuf::from("/app/bin/umu-run");
    if std::env::var_os("FLATPAK_ID").is_some() && bundled.is_file() {
        return Some(bundled);
    }
    Some(own_umu()).filter(|f| f.is_file())
}

fn packaged_umu() -> Option<PathBuf> {
    std::env::var("BLIZZNUX_UMU").ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from)
}

/// The wrapper refuses a BLIZZNUX_UMU it cannot run (bash `-f` and `-x`), so the readiness
/// check must too: the same access test, for this user.
fn executable_file(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    p.is_file() && unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) } == 0
}

fn own_umu() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"));
    base.join("blizznux").join("umu").join("umu-run")
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
    if reports::battlenet_running() {
        return Err("Battle.net is already running.".into());
    }
    // Held until watch_launch finishes; dropped on any early return below.
    let guard = reports::begin_launch().ok_or("Battle.net is still starting. Give it a moment.")?;
    let script = script_path(&app).ok_or("blizznux-run not found (run install.sh)")?;
    let mut cmd = host_command(&script);
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
    reports::watch_launch(app.clone(), guard);
    Ok(script.display().to_string())
}

fn run_script(app: &AppHandle, args: &[&str]) -> Result<String, String> {
    let script = script_path(app).ok_or("blizznux-run not found (run install.sh)")?;
    let out = host_command(&script)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_string();
    // The commands that run Proton write to the log instead of answering here.
    match (out.status.success(), text.is_empty()) {
        (true, false) => Ok(text),
        (true, true) => Ok("Done. Details are in the log.".into()),
        (false, false) => Err(text),
        (false, true) => Err("That did not work. Show the log for details.".into()),
    }
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
                if matches!(k, "PREFIX" | "OFFLOAD" | "INSTALL_ID" | "REPORTS_AUTO" | "USER_TOKEN" | "USERNAME" | "LAST_USERNAME" | "REPORTS_SHARE" | "BUG_AUTO" | "LAST_LAUNCH_OK" | "LAST_RUN_OK" | "LAB_WOW64" | "LAB_PROTON_DIR" | "LAB_DIRECT_PROTON" | "LAB_BNET_FPS" | "ZOOM") {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

#[tauri::command]
fn read_config() -> BTreeMap<String, String> {
    let mut c = load_config();
    // Not a saved setting: tells the window that the packager's BLIZZNUX_PROTON is in charge.
    if std::env::var("BLIZZNUX_PROTON").is_ok_and(|v| !v.trim().is_empty()) {
        c.insert("BLIZZNUX_PROTON_SET".into(), "1".into());
    }
    c
}

pub(crate) fn save_config(current: &BTreeMap<String, String>) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = format!(
        "PREFIX={}\nOFFLOAD={}\n",
        current.get("PREFIX").cloned().unwrap_or_default(),
        current.get("OFFLOAD").cloned().unwrap_or_else(|| "auto".into()),
    );
    for k in ["INSTALL_ID", "REPORTS_AUTO", "USER_TOKEN", "USERNAME", "LAST_USERNAME", "REPORTS_SHARE", "BUG_AUTO", "LAST_LAUNCH_OK", "LAST_RUN_OK", "LAB_WOW64", "LAB_PROTON_DIR", "LAB_DIRECT_PROTON", "LAB_BNET_FPS", "ZOOM"] {
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
        if k == "LAB_PROTON_DIR" {
            let dir = v.trim().trim_end_matches('/').to_string();
            if !dir.is_empty() {
                check_proton_dir(&dir)?;
            }
            current.insert(k, dir);
        } else if matches!(k.as_str(), "OFFLOAD" | "REPORTS_AUTO" | "REPORTS_SHARE" | "BUG_AUTO" | "LAB_WOW64" | "LAB_DIRECT_PROTON" | "LAB_BNET_FPS") {
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
    let umu = umu_run().is_some();
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
    let mut child = host_command(&script)
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

    // The wrapper downloads the launcher's own umu-launcher on first use; running it needs python3.
    let umu = umu_run();
    let python = on_path("python3");
    let direct = load_config().get("LAB_DIRECT_PROTON").is_some_and(|v| v == "1");
    let packaged = packaged_umu().is_some();
    out.push(Check {
        name: "umu-launcher".into(),
        ok: direct || umu.is_some() || (python && !packaged),
        detail: match &umu {
            _ if direct => "not needed while Labs: running Proton directly is on".into(),
            Some(_) if packaged => "found (set by BLIZZNUX_UMU)".into(),
            Some(_) => "found (the launcher's own copy)".into(),
            None if packaged => "BLIZZNUX_UMU is set, but it is not an executable file".into(),
            None if python => "not downloaded yet; the launcher fetches its own copy on first use".into(),
            None => "needs python3, which was not found".into(),
        },
        hint: match family {
            "arch" => "sudo pacman -S python".into(),
            "fedora" => "sudo dnf install python3".into(),
            _ => "sudo apt install python3".into(),
        },
        blocking: !direct,
    });

    // The wrapper downloads the launcher's own Proton on first use; unpacking it needs xz. With a
    // Proton chosen in Labs (or BLIZZNUX_PROTON) nothing is downloaded: that folder must be usable.
    let xz = on_path("xz");
    let (ok, detail) = match (reports::custom_proton(), reports::own_proton()) {
        (Some(dir), _) => match check_proton_dir(&dir.display().to_string()) {
            Ok(()) => (true, "found (the Proton you chose in Labs)".to_string()),
            Err(e) => (false, e),
        },
        (None, Some(_)) => (true, "found (the launcher's own build)".to_string()),
        (None, None) if xz => (true, "not downloaded yet; the launcher fetches its own build on first use".to_string()),
        (None, None) => (false, "needs xz to unpack it, which was not found".to_string()),
    };
    out.push(Check {
        name: "Proton".into(),
        ok,
        detail,
        hint: match family {
            "arch" => "sudo pacman -S xz".into(),
            "fedora" => "sudo dnf install xz".into(),
            _ => "sudo apt install xz-utils".into(),
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

    // 64-bit Vulkan: any ICD manifest at all. NixOS keeps them under /run/opengl-driver{,-32}.
    let icd_dirs = [
        "/usr/share/vulkan/icd.d",
        "/etc/vulkan/icd.d",
        "/usr/local/share/vulkan/icd.d",
        "/run/opengl-driver/share/vulkan/icd.d",
        "/run/opengl-driver-32/share/vulkan/icd.d",
    ];
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
        "/run/opengl-driver-32/lib/libGLX_nvidia.so.0",
    ]
    .iter()
    .any(|p| is_elf32(std::path::Path::new(p)));
    let needs_nvidia32 = vendors.contains(&"nvidia");
    let needs_mesa32 = vendors.iter().any(|v| *v == "amd" || *v == "intel");
    let vk32 = if in_flatpak { ext32 } else { (!needs_nvidia32 || nvidia32) && (!needs_mesa32 || mesa32) && (nvidia32 || mesa32) };
    let wow64 = load_config().get("LAB_WOW64").is_some_and(|v| v == "1");
    let hint = vendors
        .iter()
        .filter(|v| match **v { "nvidia" => !nvidia32, _ => !mesa32 })
        .map(|v| pkg_hint(v, family))
        .collect::<Vec<_>>()
        .join(" and ");
    out.push(Check {
        name: "Vulkan driver (32-bit)".into(),
        ok: vk32 || wow64,
        detail: if direct && !vk32 && !wow64 {
            format!("{}; running Proton directly has no container to supply 32-bit libraries, so they must come from your system (or turn on Wine WoW64 mode)", vk32_detail(vk32, wow64, &vendors))
        } else {
            vk32_detail(vk32, wow64, &vendors)
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

fn multilib_enabled() -> bool {
    fs::read_to_string("/etc/pacman.conf")
        .map(|t| t.lines().any(|l| l.trim() == "[multilib]"))
        .unwrap_or(false)
}

/// The fix steps that would repair a failed readiness check, as the ids bin/blizznux-run knows, or
/// None if we don't know how. The ids are all the app ever hands over: the wrapper maps each one to
/// its own fixed command, so no word from here can become part of a command.
fn fix_steps(check: &str) -> Option<Vec<String>> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return None; // no pkexec inside the sandbox; the hint text explains what to install
    }
    plan_steps(check, distro_family(), &gpu_vendors(), multilib_enabled())
}

/// The plan itself, with the machine facts passed in so that a test can pin a distro and a GPU.
/// Same fixes, same order and same conditions as before; only the names are ids now.
fn plan_steps(check: &str, family: &str, vendors: &[&str], multilib: bool) -> Option<Vec<String>> {
    let mut steps: Vec<String> = Vec::new();
    match (check, family) {
        // umu-launcher itself is downloaded by the wrapper; only python3 can be missing for it.
        ("umu-launcher", "arch") => steps.push("pacman-python".into()),
        ("umu-launcher", "fedora") => steps.push("dnf-python3".into()),
        ("umu-launcher", "debian") => steps.extend(["apt-update", "apt-python3"].map(String::from)),
        // The launcher's own Proton is downloaded by the wrapper; only xz can be missing for it.
        ("Proton", "arch") => steps.push("pacman-xz".into()),
        ("Proton", "fedora") => steps.push("dnf-xz".into()),
        ("Proton", "debian") => steps.extend(["apt-update", "apt-xz-utils"].map(String::from)),
        ("curl", "arch") => steps.push("pacman-curl".into()),
        ("curl", "fedora") => steps.push("dnf-curl".into()),
        ("curl", "debian") => steps.extend(["apt-update", "apt-curl"].map(String::from)),
        ("Vulkan driver (32-bit)", fam) => {
            let mut pkgs: Vec<&str> = Vec::new();
            for v in vendors {
                let pkg = match (*v, fam) {
                    ("nvidia", "arch") => "pacman-lib32-nvidia-utils",
                    ("amd", "arch") => "pacman-lib32-vulkan-radeon",
                    ("intel", "arch") => "pacman-lib32-vulkan-intel",
                    ("nvidia", "fedora") => "dnf-nvidia-driver-libs-i686",
                    (_, "fedora") => "dnf-mesa-vulkan-drivers-i686",
                    ("nvidia", "debian") => "apt-vulkan-nvidia",
                    (_, "debian") => "apt-vulkan-mesa",
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
                    if !multilib {
                        steps.extend(["sed-multilib", "pacman-sy"].map(String::from));
                    }
                    steps.extend(pkgs.iter().map(|p| p.to_string()));
                }
                "fedora" => steps.extend(pkgs.iter().map(|p| p.to_string())),
                "debian" => {
                    steps.extend(["dpkg-i386", "apt-update"].map(String::from));
                    steps.extend(pkgs.iter().map(|p| p.to_string()));
                }
                _ => return None,
            }
        }
        _ => return None,
    }
    Some(steps)
}

/// True when pkexec cannot run the wrapper where it is. An AppImage keeps it on the image's FUSE
/// mount, which only the user who mounted the image can read: root cannot start it from there.
fn needs_staged_helper(path: &Path, appimage: Option<&std::ffi::OsStr>, appdir: Option<&std::ffi::OsStr>) -> bool {
    let set = |v: Option<&std::ffi::OsStr>| v.map_or(false, |v| !v.is_empty());
    // The mount point is a name of its own, /tmp/.mount_<something>, so this is no path prefix test.
    set(appimage) || set(appdir) || path.to_string_lossy().starts_with("/tmp/.mount_")
}

/// Where a copy of the wrapper is put for pkexec: the user's runtime directory, a tmpfs only this
/// user can read, or the temporary directory when the session has none.
fn staging_base() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(d) if d.is_dir() => d,
        _ => std::env::temp_dir(),
    }
}

/// A copy of the wrapper for pkexec, in its own directory, taken away again when this is dropped.
struct StagedHelper {
    dir: PathBuf,
    path: PathBuf,
}

impl Drop for StagedHelper {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Copies the wrapper into a fresh private directory under `base`, for root to run: a 0700 folder
/// holding a 0500 copy. The name carries the pid and the time, so two calls never share one, and a
/// name already taken is simply tried again.
fn stage_helper(base: &Path, script: &Path) -> Result<StagedHelper, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..8 {
        let dir = base.join(format!("blizznux-fix-{}-{stamp}-{attempt}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
        let path = dir.join("blizznux-run");
        let copied = fs::copy(script, &path)
            .and_then(|_| fs::set_permissions(&path, fs::Permissions::from_mode(0o500)));
        if let Err(e) = copied {
            let _ = fs::remove_dir_all(&dir);
            return Err(e.to_string());
        }
        return Ok(StagedHelper { dir, path });
    }
    Err("could not find a free name for the copy of blizznux-run".into())
}

/// The commands a plan would run, as bin/blizznux-run prints them: the one place that turns an id
/// into a command, so what the user reads is exactly what fix_apply would run.
fn plan_lines(app: &AppHandle, check: &str) -> Option<Vec<String>> {
    let ids = fix_steps(check)?;
    let script = script_path(app)?; // without the wrapper nothing can run: no automatic fix
    let out = host_command(&script)
        .arg("fix")
        .arg("--print")
        .args(&ids)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

#[tauri::command]
async fn fix_plan(app: AppHandle, check: String) -> Option<Vec<String>> {
    tauri::async_runtime::spawn_blocking(move || plan_lines(&app, &check))
        .await
        .ok()
        .flatten()
}

/// Runs the fix plan as root through the system's polkit prompt (pkexec). Returns the output.
///
/// The whole plan is one pkexec call of the launcher's own blizznux-run with the step ids, so
/// whatever the plan's length the user answers one password prompt. There is no shell and the ids
/// are the only words that side accepts: the wrapper's own table decides what runs.
#[tauri::command]
async fn fix_apply(app: AppHandle, check: String) -> Result<String, String> {
    let ids = fix_steps(&check).ok_or("no automatic fix is known for this system")?;
    if !on_path("pkexec") {
        return Err("pkexec (polkit) is not available; run the commands shown in a terminal with sudo".into());
    }
    let script = script_path(&app).ok_or("blizznux-run not found (run install.sh)")?;
    let appimage = std::env::var_os("APPIMAGE");
    let appdir = std::env::var_os("APPDIR");
    tauri::async_runtime::spawn_blocking(move || {
        // Inside an AppImage root cannot read the mount the wrapper is on: it gets a private copy
        // instead, which is gone again when this call ends, however it ends.
        let staged = if needs_staged_helper(&script, appimage.as_deref(), appdir.as_deref()) {
            Some(stage_helper(&staging_base(), &script)?)
        } else {
            None
        };
        let run = staged.as_ref().map_or(script.as_path(), |s| s.path.as_path());
        let out = host_command("pkexec")
            .arg(run)
            .arg("fix")
            .args(&ids)
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
    host_command("xdg-open")
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
    let lines = reports::condense_log(&text);
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
            if std::env::var_os("BLIZZNUX_SEND").is_some() {
                match tauri::async_runtime::block_on(reports::submit_report(kind.into(), game, outcome, comment)) {
                    Ok(r) if r.sent => Ok(format!("sent → {}", r.url)),
                    Ok(r) => Ok(format!("saved for later: {}", r.reason)),
                    Err(e) => { eprintln!("send error: {e}"); return 1; }
                }
            } else {
                match reports::report_body(kind.into(), game, outcome, comment) {
                    Ok(body) => Ok(format!("{}\n(not sent; set BLIZZNUX_SEND=1 to send)", serde_json::to_string_pretty(&body).unwrap_or_default())),
                    Err(e) => { eprintln!("error: {e}"); return 1; }
                }
            }
        }
        "agent-state" => serde_json::to_string_pretty(&reports::agent_state()),
        "flush-reports" => Ok(format!("{} sent", tauri::async_runtime::block_on(reports::flush_reports()))),
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
    #[cfg(target_os = "linux")]
    glprobe::maybe_reexec();
    // WebKitGTK renders with hardware acceleration by default. If the forum view shows
    // glitches on your GPU, start with WEBKIT_DISABLE_DMABUF_RENDERER=1 to use the fallback path.
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .on_window_event(|_, event| {
            #[cfg(target_os = "linux")]
            if let tauri::WindowEvent::Focused(true) = event {
                notify::withdraw();
            }
        })
        .setup(|app| {
            reports::watch_session(app.handle().clone());
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
                // Saved zoom (Ctrl +/-/0) or the one found for XWayland (see glprobe).
                #[cfg(target_os = "linux")]
                {
                    let z = glprobe::start_zoom(load_config().get("ZOOM").map(|s| s.as_str()));
                    if z != 1.0 {
                        let _ = main.set_zoom(z);
                    }
                    // Under XWayland the window is scaled by the detected zoom, not the saved one.
                    if let Some(w) = glprobe::fallback_zoom() {
                        if let Ok(size) = main.inner_size() {
                            let _ = main.set_size(tauri::PhysicalSize::new(
                                (size.width as f64 * w) as u32,
                                (size.height as f64 * w) as u32,
                            ));
                        }
                    }
                }
                let _ = main.with_webview(|platform| {
                    #[cfg(target_os = "linux")]
                    {
                        use webkit2gtk::{CookieManagerExt, WebContextExt, WebViewExt};
                        // Ctrl +/-/0 zoom, caught here because the forum frame takes the keys otherwise.
                        {
                            use gtk::glib::Propagation;
                            use gtk::gdk::ModifierType;
                            use gtk::prelude::WidgetExt;
                            platform.inner().connect_key_press_event(|wv, event| {
                                let state = event.state();
                                if !state.contains(ModifierType::CONTROL_MASK) || state.contains(ModifierType::MOD1_MASK) {
                                    return Propagation::Proceed;
                                }
                                let Some(action) = glprobe::zoom_key(&event.keyval()) else {
                                    return Propagation::Proceed;
                                };
                                let z = glprobe::step_zoom(WebViewExt::zoom_level(wv), action);
                                WebViewExt::set_zoom_level(wv, z);
                                let mut cfg = load_config();
                                cfg.insert("ZOOM".into(), z.to_string());
                                let _ = save_config(&cfg);
                                Propagation::Stop
                            });
                        }
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
            reports::submit_report,
            reports::flush_reports,
            reports::waiting_reports,
            reports::account_status,
            reports::session_state,
            reports::close_battlenet,
            reports::reset_prefix_processes,
            reports::link_start,
            reports::link_poll,
            reports::link_cancel,
            reports::link_check,
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

/// A folder that can serve as the Proton to use: it exists and holds an executable file named
/// `proton`. The error is what the user is shown.
pub(crate) fn check_proton_dir(dir: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let d = Path::new(dir);
    if !d.is_dir() {
        return Err(format!("{dir} is not a folder"));
    }
    match fs::metadata(d.join("proton")) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => Ok(()),
        Ok(m) if m.is_file() => Err(format!("{dir}/proton is not executable")),
        _ => Err(format!("{dir} has no file named proton: choose the Proton folder itself")),
    }
}

/// What the 32-bit Vulkan check says. Labs: Wine WoW64 mode runs 32-bit code on 64-bit
/// drivers, so a missing 32-bit driver is not a problem while it is on.
fn vk32_detail(found: bool, wow64: bool, vendors: &[&str]) -> String {
    if found {
        "found".into()
    } else if wow64 {
        "not needed while Labs: Wine WoW64 mode is on".into()
    } else {
        format!("missing for {} — Battle.net and older games need it", if vendors.is_empty() { "your GPU".into() } else { vendors.join(" + ") })
    }
}

#[cfg(test)]
mod lab_tests {
    use super::*;

    #[test]
    fn a_proton_folder_needs_an_executable_proton_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("blizznux-protondir-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = dir.display().to_string();
        assert!(check_proton_dir(&format!("{d}/nope")).unwrap_err().contains("not a folder"));
        fs::create_dir_all(&dir).unwrap();
        assert!(check_proton_dir(&d).unwrap_err().contains("no file named proton"));
        fs::write(dir.join("proton"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("proton"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_proton_dir(&d).unwrap_err().contains("not executable"));
        fs::set_permissions(dir.join("proton"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(check_proton_dir(&d), Ok(()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_packaged_umu_run_must_be_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("blizznux-umu-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let umu = dir.join("umu-run");
        assert!(!executable_file(&umu));
        fs::write(&umu, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&umu, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!executable_file(&umu));
        // Executable for others only: not for its owner, the user running this.
        fs::set_permissions(&umu, fs::Permissions::from_mode(0o641)).unwrap();
        assert!(!executable_file(&umu));
        fs::set_permissions(&umu, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(executable_file(&umu));
        assert!(!executable_file(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_32_bit_driver_is_not_needed_in_wow64_mode() {
        assert_eq!(vk32_detail(false, true, &["amd"]), "not needed while Labs: Wine WoW64 mode is on");
        assert_eq!(vk32_detail(true, true, &["amd"]), "found");
        assert!(vk32_detail(false, false, &["amd"]).starts_with("missing for amd"));
    }

    // Every plan, for every system the checks know: the step ids bin/blizznux-run maps to commands.
    const CHECKS: [&str; 4] = ["umu-launcher", "Proton", "curl", "Vulkan driver (32-bit)"];
    const FAMILIES: [&str; 3] = ["arch", "fedora", "debian"];
    // One GPU of each vendor, then the pairs a laptop can have.
    const VENDOR_SETS: [&[&str]; 6] = [
        &["nvidia"],
        &["amd"],
        &["intel"],
        &["nvidia", "amd"],
        &["nvidia", "intel"],
        &["amd", "intel"],
    ];

    // Every id the app can name, from every plan for every system it knows, sorted.
    fn all_plan_ids() -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for check in CHECKS {
            for fam in FAMILIES {
                for multilib in [false, true] {
                    for vendors in VENDOR_SETS {
                        let Some(plan) = plan_steps(check, fam, vendors, multilib) else { continue };
                        for id in plan {
                            if !ids.contains(&id) {
                                ids.push(id);
                            }
                        }
                    }
                }
            }
        }
        ids.sort();
        ids
    }

    #[test]
    fn a_fix_plan_names_the_same_steps_in_the_same_order() {
        let plan = |check: &str, fam: &str, vendors: &[&str], multilib: bool| -> Vec<String> {
            plan_steps(check, fam, vendors, multilib).unwrap()
        };
        assert_eq!(plan("umu-launcher", "arch", &["amd"], true), ["pacman-python"]);
        assert_eq!(plan("umu-launcher", "fedora", &["amd"], true), ["dnf-python3"]);
        assert_eq!(plan("umu-launcher", "debian", &["amd"], true), ["apt-update", "apt-python3"]);
        assert_eq!(plan("Proton", "arch", &["amd"], true), ["pacman-xz"]);
        assert_eq!(plan("Proton", "fedora", &["amd"], true), ["dnf-xz"]);
        assert_eq!(plan("Proton", "debian", &["amd"], true), ["apt-update", "apt-xz-utils"]);
        assert_eq!(plan("curl", "arch", &["amd"], true), ["pacman-curl"]);
        assert_eq!(plan("curl", "fedora", &["amd"], true), ["dnf-curl"]);
        assert_eq!(plan("curl", "debian", &["amd"], true), ["apt-update", "apt-curl"]);
        // Arch Vulkan without multilib: the sed, then the sync, then the drivers.
        assert_eq!(
            plan("Vulkan driver (32-bit)", "arch", &["amd"], false),
            ["sed-multilib", "pacman-sy", "pacman-lib32-vulkan-radeon"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "arch", &["amd"], true),
            ["pacman-lib32-vulkan-radeon"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "arch", &["nvidia"], true),
            ["pacman-lib32-nvidia-utils"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "fedora", &["nvidia"], true),
            ["dnf-nvidia-driver-libs-i686"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "debian", &["nvidia"], false),
            ["dpkg-i386", "apt-update", "apt-vulkan-nvidia"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "debian", &["amd"], true),
            ["dpkg-i386", "apt-update", "apt-vulkan-mesa"]
        );
        // Two GPUs of one package manager's kind: one step for each driver, none twice.
        assert_eq!(
            plan("Vulkan driver (32-bit)", "arch", &["amd", "intel"], true),
            ["pacman-lib32-vulkan-radeon", "pacman-lib32-vulkan-intel"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "fedora", &["amd", "intel"], true),
            ["dnf-mesa-vulkan-drivers-i686"]
        );
        assert_eq!(
            plan("Vulkan driver (32-bit)", "debian", &["amd", "nvidia"], true),
            ["dpkg-i386", "apt-update", "apt-vulkan-mesa", "apt-vulkan-nvidia"]
        );
        assert!(plan_steps("something else", "arch", &["amd"], true).is_none());
        assert!(plan_steps("curl", "gentoo", &["amd"], true).is_none());
        assert!(plan_steps("Vulkan driver (32-bit)", "gentoo", &["amd"], true).is_none());
    }

    // The ids and the wrapper's table are one list in two files: this is what keeps them from
    // drifting apart. Every id the app can name is a case in bin/blizznux-run's fix_argv, and that
    // table holds nothing else.
    #[test]
    fn every_fix_id_the_app_can_name_is_a_step_the_wrapper_knows() {
        let src = include_str!("../../bin/blizznux-run");
        let body = src.split_once("fix_argv() {").expect("bin/blizznux-run has no fix_argv").1;
        let mut table: Vec<String> = Vec::new();
        for line in body.lines().take_while(|l| l.trim() != "}") {
            let Some((head, _)) = line.trim().split_once(')') else { continue };
            if head.contains('-') && head.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                table.push(head.to_string());
            }
        }
        table.sort();
        assert!(!table.is_empty(), "the wrapper's fix table is empty");
        assert_eq!(table.len(), 20, "the wrapper's fix table: {table:?}");
        assert_eq!(table, all_plan_ids());
    }

    // pkexec runs the wrapper where it is, except where root cannot read it: an AppImage's FUSE
    // mount, which is private to the user who mounted the image.
    #[test]
    fn a_wrapper_root_can_read_is_run_where_it_is() {
        use std::ffi::OsStr;
        let installed = Path::new("/usr/lib/BlizzNux/_up_/bin/blizznux-run");
        assert!(!needs_staged_helper(installed, None, None));
        assert!(!needs_staged_helper(&home().join("bin").join("blizznux-run"), None, None));
        // What the app sees inside an AppImage, whether or not the path says so.
        assert!(needs_staged_helper(
            Path::new("/tmp/.mount_BlizzNuxAbC123/up/bin/blizznux-run"),
            None,
            None
        ));
        assert!(needs_staged_helper(installed, Some(OsStr::new("/tmp/.mount_BlizzNuxAbC123/BlizzNux.AppImage")), None));
        assert!(needs_staged_helper(installed, None, Some(OsStr::new("/tmp/.mount_BlizzNuxAbC123/usr"))));
        // An empty value says nothing.
        assert!(!needs_staged_helper(installed, Some(OsStr::new("")), Some(OsStr::new(""))));
    }

    #[test]
    fn a_staged_wrapper_is_private_and_goes_away_again() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("blizznux-stage-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let script = base.join("blizznux-run");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        let (dir, other) = {
            let staged = stage_helper(&base, &script).unwrap();
            let dir = staged.dir.clone();
            assert!(dir.starts_with(&base), "{} is not under {}", dir.display(), base.display());
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(staged.path.file_name().unwrap(), "blizznux-run");
            assert_eq!(fs::metadata(&staged.path).unwrap().permissions().mode() & 0o777, 0o500);
            assert_eq!(fs::read_to_string(&staged.path).unwrap(), "#!/bin/sh\n");
            // A second copy gets a directory of its own.
            let other = stage_helper(&base, &script).unwrap();
            assert_ne!(other.dir, dir);
            (dir, other.dir.clone())
        };
        assert!(!dir.exists(), "the staged folder is still there: {}", dir.display());
        assert!(!other.exists(), "the staged folder is still there: {}", other.display());
        let _ = fs::remove_dir_all(&base);
    }
}
