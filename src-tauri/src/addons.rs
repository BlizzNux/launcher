// World of Warcraft addon helpers: find the game's AddOns folders inside the Battle.net
// prefix, list what is installed, and install addon archives from a file or a link.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::current_prefix;

const FLAVORS: [(&str, &str); 5] = [
    ("_retail_", "Retail"),
    ("_classic_", "Classic"),
    ("_classic_era_", "Classic Era"),
    ("_ptr_", "PTR"),
    ("_xptr_", "PTR (Classic)"),
];

#[derive(serde::Serialize)]
pub struct WowInstall {
    flavor: String,
    label: String,
    path: String,
    addons_dir: String,
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
        for (dir, label) in FLAVORS {
            let p = root.join(dir);
            if p.join("Wow.exe").is_file() || p.join("WowClassic.exe").is_file() || p.join("WowT.exe").is_file() {
                if out.iter().any(|w: &WowInstall| w.flavor == dir) {
                    continue;
                }
                let addons = p.join("Interface").join("AddOns");
                let _ = fs::create_dir_all(&addons);
                out.push(WowInstall {
                    flavor: dir.into(),
                    label: label.into(),
                    path: p.display().to_string(),
                    addons_dir: addons.display().to_string(),
                });
            }
        }
    }
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
    let zips: Vec<&str> = assets
        .iter()
        .filter_map(|a| a["browser_download_url"].as_str())
        .filter(|u| u.ends_with(".zip"))
        .collect();
    let classic_tags = ["-classic", "-vanilla", "-bcc", "-tbc", "-wrath", "-wotlk", "-cata", "-mists", "-mop"];
    let is_classic = |u: &&str| classic_tags.iter().any(|t| u.to_lowercase().contains(t));
    match flavor {
        "_classic_era_" => zips.iter().find(|u| { let l = u.to_lowercase(); l.contains("-classic") || l.contains("-vanilla") }).copied(),
        "_classic_" => zips.iter().find(|u| { let l = u.to_lowercase(); l.contains("-mists") || l.contains("-mop") || l.contains("-cata") || l.contains("-wrath") || l.contains("-wotlk") }).copied(),
        _ => zips.iter().find(|u| !is_classic(u)).copied(),
    }
    .or_else(|| zips.first().copied())
}

/// Install from a link: a direct .zip URL, or a GitHub repository / release page.
#[tauri::command]
pub async fn install_addon_url(addons_dir: String, flavor: String, url: String) -> Result<Vec<String>, String> {
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
