// World of Warcraft addon helpers: find the game's AddOns folders inside the Battle.net
// prefix, list what is installed, and install addon archives from a file or a link.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::current_prefix;

/// Executables Battle.net ships per client. Any `_name_` folder holding one is a WoW install.
const WOW_EXES: [&str; 6] = ["Wow.exe", "WowT.exe", "WowB.exe", "WowClassic.exe", "WowClassicT.exe", "WowClassicB.exe"];

fn flavor_label(folder: &str) -> String {
    match folder {
        "_retail_" => "Retail".into(),
        "_ptr_" => "Retail PTR".into(),
        "_xptr_" => "Retail PTR (X)".into(),
        "_beta_" => "Retail Beta".into(),
        "_classic_" => "Classic".into(),
        "_classic_ptr_" => "Classic PTR".into(),
        "_classic_beta_" => "Classic Beta".into(),
        "_classic_era_" => "Classic Era".into(),
        "_classic_era_ptr_" => "Classic Era PTR".into(),
        "_anniversary_" => "Anniversary".into(),
        other => {
            // Unknown future client: make a readable label from the folder name.
            let name = other.trim_matches('_').replace('_', " ");
            let mut c = name.chars();
            match c.next() { Some(f) => f.to_uppercase().collect::<String>() + c.as_str(), None => other.to_string() }
        }
    }
}

/// Which family of addon builds an install wants: "retail", "classic" (progression) or "era".
fn flavor_family(folder: &str, exe: &str) -> &'static str {
    if !exe.starts_with("WowClassic") {
        return "retail";
    }
    match folder {
        "_classic_" | "_classic_ptr_" | "_classic_beta_" => "classic",
        _ => "era",
    }
}

#[derive(serde::Serialize)]
pub struct WowInstall {
    flavor: String,
    label: String,
    path: String,
    addons_dir: String,
    exe: String,
    family: String,
}

#[derive(serde::Serialize)]
pub struct Addon {
    folder: String,
    title: String,
    version: String,
}

/// Candidate "World of Warcraft" roots inside the prefix: Battle.net's default install
/// path (from any user's Battle.net.config) plus the usual locations.
fn wow_roots(prefix: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let users = prefix.join("drive_c/users");
    if let Ok(rd) = fs::read_dir(&users) {
        for u in rd.flatten() {
            let cfg = u.path().join("AppData/Roaming/Battle.net/Battle.net.config");
            if let Ok(text) = fs::read_to_string(&cfg) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(p) = json["Client"]["Install"]["DefaultInstallPath"].as_str() {
                        if let Some(rest) = p.strip_prefix("C:/").or_else(|| p.strip_prefix("C:\\")) {
                            roots.push(prefix.join("drive_c").join(rest.replace('\\', "/")).join("World of Warcraft"));
                        }
                    }
                }
            }
        }
    }
    roots.push(prefix.join("drive_c/Program Files (x86)/World of Warcraft"));
    roots.push(prefix.join("drive_c/Program Files/World of Warcraft"));
    roots.push(prefix.join("drive_c/Games/World of Warcraft"));
    roots.dedup();
    roots
}

#[tauri::command]
pub fn wow_installs() -> Vec<WowInstall> {
    let prefix = current_prefix();
    let mut out = Vec::new();
    for root in wow_roots(&prefix) {
        let Ok(rd) = fs::read_dir(&root) else { continue };
        let mut dirs: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with('_') && n.ends_with('_') && n.len() > 2)
            .collect();
        dirs.sort();
        for dir in dirs {
            let p = root.join(&dir);
            let Some(exe) = WOW_EXES.iter().find(|e| p.join(e).is_file()) else { continue };
            if out.iter().any(|w: &WowInstall| w.flavor == dir) {
                continue;
            }
            let addons = p.join("Interface").join("AddOns");
            let _ = fs::create_dir_all(&addons);
            out.push(WowInstall {
                label: flavor_label(&dir),
                family: flavor_family(&dir, exe).into(),
                exe: (*exe).into(),
                flavor: dir,
                path: p.display().to_string(),
                addons_dir: addons.display().to_string(),
            });
        }
    }
    // Retail first, then the rest alphabetically.
    out.sort_by_key(|w| (w.flavor != "_retail_", w.flavor.clone()));
    out
}

/// Only paths that are an AddOns folder of a detected install are accepted from the UI.
fn checked_addons_dir(addons_dir: &str) -> Result<PathBuf, String> {
    let wanted = PathBuf::from(addons_dir);
    if wow_installs().iter().any(|w| PathBuf::from(&w.addons_dir) == wanted) {
        Ok(wanted)
    } else {
        Err("that folder is not a known World of Warcraft AddOns folder".into())
    }
}

fn toc_field(text: &str, key: &str) -> String {
    text.lines()
        .filter_map(|l| l.strip_prefix("## "))
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

/// Strip WoW's colour codes and texture escapes from a TOC title.
fn clean_title(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '|' {
            match chars.peek() {
                Some('c') | Some('C') => { chars.next(); for _ in 0..8 { chars.next(); } }
                Some('r') | Some('R') => { chars.next(); }
                Some('T') => { while let Some(n) = chars.next() { if n == 't' { break; } } }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn find_toc(folder: &Path) -> Option<PathBuf> {
    let name = folder.file_name()?.to_string_lossy().to_string();
    let candidates = [
        format!("{name}.toc"),
        format!("{name}_Mainline.toc"),
        format!("{name}-Mainline.toc"),
        format!("{name}_Classic.toc"),
        format!("{name}_Vanilla.toc"),
        format!("{name}_Cata.toc"),
        format!("{name}_Mists.toc"),
        format!("{name}_Wrath.toc"),
        format!("{name}_TBC.toc"),
    ];
    for c in candidates {
        let p = folder.join(&c);
        if p.is_file() {
            return Some(p);
        }
    }
    fs::read_dir(folder).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().map_or(false, |e| e == "toc"))
}

#[tauri::command]
pub fn list_addons(addons_dir: String) -> Result<Vec<Addon>, String> {
    let dir = checked_addons_dir(&addons_dir)?;
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let folder = e.file_name().to_string_lossy().to_string();
            if folder.starts_with("Blizzard_") {
                continue;
            }
            let Some(toc) = find_toc(&p) else { continue };
            let text = fs::read_to_string(&toc).unwrap_or_default();
            let title = clean_title(&toc_field(&text, "Title"));
            out.push(Addon {
                title: if title.is_empty() { folder.clone() } else { title },
                version: toc_field(&text, "Version"),
                folder,
            });
        }
    }
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    Ok(out)
}

#[tauri::command]
pub fn remove_addon(addons_dir: String, folder: String) -> Result<(), String> {
    let dir = checked_addons_dir(&addons_dir)?;
    if folder.is_empty() || folder.contains('/') || folder.contains("..") || folder.starts_with("Blizzard_") {
        return Err("invalid addon folder".into());
    }
    let target = dir.join(&folder);
    if !target.is_dir() {
        return Err("addon folder not found".into());
    }
    fs::remove_dir_all(&target).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_addons_folder(addons_dir: String) -> Result<(), String> {
    let dir = checked_addons_dir(&addons_dir)?;
    Command::new("xdg-open")
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Unpack an addon archive into AddOns. The archive must contain addon folders with a TOC
/// at their top level (the format every addon site and the BigWigs packager produce).
fn install_archive(addons_dir: &Path, bytes: &[u8]) -> Result<Vec<String>, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).map_err(|_| "that file is not a ZIP archive".to_string())?;
    // Find top-level folders and make sure at least one holds a .toc file.
    let mut tops: Vec<String> = Vec::new();
    let mut has_toc = false;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().to_string();
        let Some(top) = name.split('/').next().filter(|t| !t.is_empty()) else { continue };
        if !tops.iter().any(|t| t == top) {
            tops.push(top.to_string());
        }
        if name.matches('/').count() == 1 && name.ends_with(".toc") {
            has_toc = true;
        }
    }
    if !has_toc {
        return Err("this archive doesn't look like a WoW addon: no folder with a .toc file at the top level".into());
    }
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = f.enclosed_name().map(|p| p.to_path_buf()) else { return Err("archive contains an unsafe path".into()) };
        let dest = addons_dir.join(&rel);
        if !dest.starts_with(addons_dir) {
            return Err("archive contains an unsafe path".into());
        }
        if f.is_dir() {
            fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        fs::write(&dest, buf).map_err(|e| e.to_string())?;
    }
    Ok(tops)
}

/// Install from bytes sent by the UI (a ZIP the user picked). Headers carry the target folder.
#[tauri::command]
pub fn install_addon_bytes(request: tauri::ipc::Request<'_>) -> Result<Vec<String>, String> {
    let addons_dir = request
        .headers()
        .get("x-addons-dir")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing target folder")?
        .to_string();
    let dir = checked_addons_dir(&addons_dir)?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else { return Err("expected file bytes".into()) };
    install_archive(&dir, bytes)
}

fn pick_asset_for_flavor<'a>(assets: &'a [serde_json::Value], flavor: &str) -> Option<&'a str> {
    let family = match flavor {
        "_retail_" | "_ptr_" | "_xptr_" | "_beta_" => "retail",
        "_classic_" | "_classic_ptr_" | "_classic_beta_" => "classic",
        f if f.starts_with("family:") => &f[7..],
        _ => "era",
    };
    let zips: Vec<&str> = assets
        .iter()
        .filter_map(|a| a["browser_download_url"].as_str())
        .filter(|u| u.ends_with(".zip"))
        .collect();
    let classic_tags = ["-classic", "-vanilla", "-bcc", "-tbc", "-wrath", "-wotlk", "-cata", "-mists", "-mop"];
    let is_classic = |u: &&str| classic_tags.iter().any(|t| u.to_lowercase().contains(t));
    match family {
        "era" => zips.iter().find(|u| { let l = u.to_lowercase(); l.contains("-classic") || l.contains("-vanilla") }).copied(),
        "classic" => zips.iter().find(|u| { let l = u.to_lowercase(); l.contains("-mists") || l.contains("-mop") || l.contains("-cata") || l.contains("-wrath") || l.contains("-wotlk") }).copied(),
        _ => zips.iter().find(|u| !is_classic(u)).copied(),
    }
    .or_else(|| zips.first().copied())
}

/// Install from a link: a direct .zip URL, or a GitHub repository / release page.
#[tauri::command]
pub async fn install_addon_url(addons_dir: String, flavor: String, family: Option<String>, url: String) -> Result<Vec<String>, String> {
    let flavor = match family { Some(f) if !f.is_empty() => format!("family:{f}"), _ => flavor };
    let dir = checked_addons_dir(&addons_dir)?;
    let u: url::Url = url.trim().parse().map_err(|e: url::ParseError| e.to_string())?;
    if u.scheme() != "https" {
        return Err("only https links are supported".into());
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let mut download = u.to_string();
    if u.host_str() == Some("github.com") && !download.ends_with(".zip") {
        let parts: Vec<&str> = u.path().trim_matches('/').split('/').collect();
        if parts.len() < 2 {
            return Err("GitHub link must point at a repository".into());
        }
        let api = format!("https://api.github.com/repos/{}/{}/releases/latest", parts[0], parts[1]);
        let rel: serde_json::Value = client.get(&api).send().await.map_err(|e| e.to_string())?.json().await.map_err(|_| "no release found for that repository".to_string())?;
        let assets = rel["assets"].as_array().cloned().unwrap_or_default();
        download = pick_asset_for_flavor(&assets, &flavor)
            .map(|s| s.to_string())
            .ok_or("the latest release has no .zip asset; download it manually and use Install from ZIP")?;
    }
    let resp = client.get(&download).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > 200 * 1024 * 1024 {
        return Err("archive is larger than 200 MB; refusing".into());
    }
    install_archive(&dir, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, content) in files {
                w.start_file(*name, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn installs_a_proper_addon_archive() {
        let dir = std::env::temp_dir().join(format!("blizznux-addons-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let z = zip_with(&[("MyAddon/MyAddon.toc", "## Title: My |cff00ff00Addon|r\n## Version: 1.2.3\n"), ("MyAddon/core.lua", "-- lua")]);
        let tops = install_archive(&dir, &z).unwrap();
        assert_eq!(tops, vec!["MyAddon".to_string()]);
        assert!(dir.join("MyAddon/core.lua").is_file());
        let toc = fs::read_to_string(find_toc(&dir.join("MyAddon")).unwrap()).unwrap();
        assert_eq!(clean_title(&toc_field(&toc, "Title")), "My Addon");
        assert_eq!(toc_field(&toc, "Version"), "1.2.3");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_archives_without_a_toc_or_with_unsafe_paths() {
        let dir = std::env::temp_dir().join(format!("blizznux-addons-test2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let no_toc = zip_with(&[("Something/readme.txt", "hi")]);
        assert!(install_archive(&dir, &no_toc).is_err());
        let not_zip = b"definitely not a zip".to_vec();
        assert!(install_archive(&dir, &not_zip).is_err());
        let traversal = zip_with(&[("Evil/Evil.toc", "## Title: x"), ("../escape.txt", "x")]);
        assert!(install_archive(&dir, &traversal).is_err());
        assert!(!dir.parent().unwrap().join("escape.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picks_release_assets_per_flavor() {
        let assets: Vec<serde_json::Value> = ["https://x/Addon-1.0.zip", "https://x/Addon-1.0-classic.zip", "https://x/Addon-1.0-mists.zip"]
            .iter().map(|u| serde_json::json!({ "browser_download_url": u })).collect();
        assert_eq!(pick_asset_for_flavor(&assets, "_retail_"), Some("https://x/Addon-1.0.zip"));
        assert_eq!(pick_asset_for_flavor(&assets, "_classic_era_"), Some("https://x/Addon-1.0-classic.zip"));
        assert_eq!(pick_asset_for_flavor(&assets, "_classic_"), Some("https://x/Addon-1.0-mists.zip"));
    }
}

// ---------------------------------------------------------------------------------------
// WowUp bridge: install the WowUp (CurseForge build) AppImage and open it with the WoW
// installs from our prefix already registered, so the user never hunts for Wow.exe.
// ---------------------------------------------------------------------------------------

const WOWUP_RELEASES: &str = "https://api.github.com/repos/WowUp/WowUp.CF/releases/latest";
const WOWUP_CONFIG_DIRS: [&str; 3] = ["WowUpCf", "WowUp-CF", "wowup-cf"];

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var(var).map(PathBuf::from).unwrap_or_else(|_| crate::home().join(fallback))
}

fn wowup_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("blizznux").join("wowup")
}

fn wowup_appimage() -> PathBuf {
    wowup_dir().join("WowUp-CF.AppImage")
}

#[derive(serde::Serialize)]
pub struct WowUpStatus {
    installed: bool,
    version: String,
    downloading: bool,
    downloaded: u64,
    total: u64,
}

fn progress_file() -> PathBuf {
    wowup_dir().join("download.progress")
}

#[tauri::command]
pub fn wowup_status() -> WowUpStatus {
    let app = wowup_appimage();
    let version = fs::read_to_string(wowup_dir().join("version")).unwrap_or_default().trim().to_string();
    let (downloading, downloaded, total) = fs::read_to_string(progress_file())
        .ok()
        .and_then(|t| {
            let mut it = t.split_whitespace();
            Some((true, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .unwrap_or((false, 0, 0));
    WowUpStatus { installed: app.is_file(), version, downloading, downloaded, total }
}

/// Download the latest WowUp-CF AppImage. Progress is written to a file the UI polls.
#[tauri::command]
pub async fn wowup_install() -> Result<String, String> {
    let dir = wowup_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let rel: serde_json::Value = client
        .get(WOWUP_RELEASES)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|_| "could not read the WowUp release list".to_string())?;
    let version = rel["tag_name"].as_str().unwrap_or("unknown").trim_start_matches('v').to_string();
    let asset = rel["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str().map_or(false, |n| n.ends_with(".AppImage"))))
        .ok_or("the latest WowUp release has no Linux AppImage")?;
    let url = asset["browser_download_url"].as_str().ok_or("bad release data")?.to_string();
    let total = asset["size"].as_u64().unwrap_or(0);

    let tmp = dir.join("WowUp-CF.AppImage.part");
    let mut resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut done: u64 = 0;
    let mut last_written: u64 = 0;
    use std::io::Write;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        done += chunk.len() as u64;
        if done - last_written > 1_000_000 {
            let _ = fs::write(progress_file(), format!("{done} {total}"));
            last_written = done;
        }
    }
    drop(file);
    let _ = fs::remove_file(progress_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, wowup_appimage()).map_err(|e| e.to_string())?;
    fs::write(dir.join("version"), &version).map_err(|e| e.to_string())?;
    Ok(version)
}

/// WowUp's WowClientType for an install, mirroring WowUp's own getClientType():
/// Retail 0, Classic 1, RetailPtr 2, ClassicPtr 3, Beta 4, ClassicBeta 5, ClassicEra 6,
/// ClassicEraPtr 7, RetailXPtr 8, Anniversary 9.
fn wowup_client_type(folder: &str, exe: &str) -> u32 {
    match exe {
        "Wow.exe" => 0,
        "WowClassic.exe" => match folder { "_classic_era_" => 6, "_anniversary_" => 9, _ => 1 },
        "WowT.exe" => if folder == "_xptr_" { 8 } else { 2 },
        "WowClassicT.exe" => if folder == "_classic_era_ptr_" { 7 } else { 3 },
        "WowB.exe" => 4,
        _ => 5,
    }
}

/// WowUp's Electron user-data directory: an existing one if present, else the default name.
pub(crate) fn wowup_config_dir() -> PathBuf {
    let base = xdg("XDG_CONFIG_HOME", ".config");
    for d in WOWUP_CONFIG_DIRS {
        let p = base.join(d);
        if p.is_dir() {
            return p;
        }
    }
    base.join(WOWUP_CONFIG_DIRS[0])
}

/// Register our WoW installs in WowUp's preferences (merging with whatever is there).
pub(crate) fn seed_wowup_installs() -> Result<usize, String> {
    let installs = wow_installs();
    if installs.is_empty() {
        return Ok(0);
    }
    let dir = wowup_config_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("preferences.json");
    let mut prefs: serde_json::Value = fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !prefs.is_object() {
        prefs = serde_json::json!({});
    }
    let mut list: Vec<serde_json::Value> = prefs["wow_installations"].as_array().cloned().unwrap_or_default();
    let mut added = 0;
    for w in installs {
        let client_type = wowup_client_type(&w.flavor, &w.exe);
        let location = PathBuf::from(&w.path).join(&w.exe);
        if !location.is_file() {
            continue;
        }
        let loc = location.display().to_string();
        if list.iter().any(|i| i["location"].as_str() == Some(loc.as_str())) {
            continue;
        }
        let id = format!("blizznux-{}", w.flavor.trim_matches('_'));
        list.push(serde_json::json!({
            "id": id,
            "clientType": client_type,
            "defaultAddonChannelType": 0,
            "defaultAutoUpdate": false,
            "label": "BlizzNux",
            "displayName": format!("BlizzNux {}", w.label),
            "location": loc,
            "selected": list.is_empty(),
        }));
        added += 1;
    }
    prefs["wow_installations"] = serde_json::Value::Array(list);
    fs::write(&file, serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(added)
}

#[tauri::command]
pub fn wowup_launch() -> Result<String, String> {
    let app = wowup_appimage();
    if !app.is_file() {
        return Err("WowUp is not installed yet".into());
    }
    let added = seed_wowup_installs()?;
    let mut cmd = Command::new(&app);
    // AppImages need FUSE to mount themselves; fall back to extracting when it is missing.
    if !Path::new("/usr/lib/libfuse.so.2").exists() && !Path::new("/usr/lib64/libfuse.so.2").exists()
        && !Path::new("/usr/lib/x86_64-linux-gnu/libfuse.so.2").exists()
    {
        cmd.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!("WowUp started ({added} WoW install(s) registered)"))
}

#[tauri::command]
pub fn wowup_remove() -> Result<(), String> {
    let dir = wowup_dir();
    if dir.is_dir() {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}
