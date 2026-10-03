// Self-update of the launcher (signed, AppImage builds) and a Battle.net version check.
// GPL-3.0-or-later. Not affiliated with Blizzard Entertainment.
use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

use crate::reports::battlenet_info;

#[derive(serde::Serialize)]
pub struct UpdateInfo {
    available: bool,
    version: String,
    notes: String,
    can_install: bool,
    url: String,
}

/// Only AppImage builds can replace themselves; deb, rpm, Flatpak and plain binaries
/// update through their package manager or the release page.
fn self_updatable() -> bool {
    std::env::var_os("APPIMAGE").is_some() && std::env::var_os("FLATPAK_ID").is_none()
}

async fn checker(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut b = app.updater_builder();
    if let Ok(u) = std::env::var("BLIZZNUX_UPDATE_URL") {
        // Test override only: a local http manifest is allowed here, never in the shipped endpoint.
        let url: url::Url = u.parse().map_err(|e: url::ParseError| e.to_string())?;
        b = b.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    b.build().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateInfo, String> {
    let updater = checker(&app).await?;
    match updater.check().await {
        Ok(Some(u)) => Ok(UpdateInfo {
            available: true,
            version: u.version.clone(),
            notes: u.body.clone().unwrap_or_default(),
            can_install: self_updatable(),
            url: format!("https://github.com/BlizzNux/launcher/releases/tag/v{}", u.version),
        }),
        Ok(None) => Ok(UpdateInfo { available: false, version: String::new(), notes: String::new(), can_install: self_updatable(), url: String::new() }),
        Err(e) => Err(e.to_string()),
    }
}

/// Download, verify the signature, install, and restart. Progress goes out as events.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    if !self_updatable() {
        return Err("this install updates through its package manager; use the release page".into());
    }
    let updater = checker(&app).await?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else { return Err("no update available".into()) };
    let h = app.clone();
    let mut got: u64 = 0;
    update
        .download_and_install(
            move |chunk, total| {
                got += chunk as u64;
                let _ = h.emit("update-progress", serde_json::json!({ "downloaded": got, "total": total }));
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    let Some(image) = std::env::var_os("APPIMAGE") else { app.restart() };
    // This copy was started by the old image: its start script put the old mount into the
    // search paths, and the old mount helper waits on handles this process holds. Start the
    // new image without either, so the old one is released instead of staying mounted until quit.
    let appdir = std::env::var("APPDIR").unwrap_or_default();
    keep_handles_out_of_children();
    Command::new(image)
        .args(std::env::args_os().skip(1))
        .env_clear()
        .envs(env_outside_appimage(std::env::vars_os(), &appdir))
        .current_dir(crate::home())
        .spawn()
        .map_err(|e| e.to_string())?;
    app.exit(0);
    Ok(())
}

/// The environment without what an AppImage's start script added: entries of a path list that
/// point into the image's mount are dropped, and a variable left with nothing is removed.
fn env_outside_appimage(vars: impl Iterator<Item = (OsString, OsString)>, appdir: &str) -> Vec<(OsString, OsString)> {
    vars.filter_map(|(key, val)| {
        let Some(text) = val.to_str().filter(|t| !appdir.is_empty() && t.contains(appdir)) else {
            return Some((key, val));
        };
        let kept: Vec<&str> = text.split(':').filter(|p| !p.is_empty() && !p.starts_with(appdir)).collect();
        if kept.is_empty() { None } else { Some((key, kept.join(":").into())) }
    })
    .collect()
}

/// The AppImage runtime hands the app an open handle on its mount and the pipe its mount
/// helper waits on, and children inherit both. Mark every open handle except the standard
/// three, so that nothing of the old image is passed on to the next one.
fn keep_handles_out_of_children() {
    let Ok(fds) = std::fs::read_dir("/proc/self/fd") else { return };
    for n in fds.flatten().filter_map(|fd| fd.file_name().to_string_lossy().parse::<i32>().ok()) {
        if n > 2 {
            unsafe { libc::fcntl(n, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }
}

#[derive(serde::Serialize)]
pub struct BattlenetUpdate {
    installed: String,
    latest: String,
    available: bool,
}

/// Blizzard's version table for the Battle.net app (TACT product "bna"), region US.
#[tauri::command]
pub async fn battlenet_update_check() -> Result<BattlenetUpdate, String> {
    let installed = battlenet_info().version;
    let client = reqwest::Client::builder()
        .user_agent(format!("BlizzNux/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let text = client.get("https://us.version.battle.net/bna/versions").send().await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string())?;
    let mut lines = text.lines().filter(|l| !l.starts_with("##") && !l.trim().is_empty());
    let header: Vec<&str> = lines.next().ok_or("empty version table")?.split('|').map(|h| h.split('!').next().unwrap_or("")).collect();
    let vi = header.iter().position(|h| *h == "VersionsName").ok_or("no VersionsName column")?;
    let latest = lines
        .map(|l| l.split('|').collect::<Vec<_>>())
        .find(|f| f.first() == Some(&"us"))
        .and_then(|f| f.get(vi).map(|s| s.to_string()))
        .ok_or("no us row")?;
    let newer = |a: &str, b: &str| {
        let pa: Vec<u64> = a.split('.').filter_map(|x| x.parse().ok()).collect();
        let pb: Vec<u64> = b.split('.').filter_map(|x| x.parse().ok()).collect();
        pa > pb
    };
    let available = !installed.is_empty() && newer(&latest, &installed);
    Ok(BattlenetUpdate { installed, latest, available })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        let vars = vars.iter().map(|(k, v)| (OsString::from(k), OsString::from(v)));
        env_outside_appimage(vars, "/tmp/.mount_old")
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect()
    }

    #[test]
    fn mount_entries_leave_path_lists() {
        let out = clean(&[("PATH", "/tmp/.mount_old/usr/bin/:/tmp/.mount_old/bin/:/usr/local/bin:/usr/bin")]);
        assert_eq!(out, vec![("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string())]);
    }

    #[test]
    fn variables_left_empty_are_removed() {
        let out = clean(&[("APPDIR", "/tmp/.mount_old"), ("LD_LIBRARY_PATH", "/tmp/.mount_old/usr/lib/:/tmp/.mount_old/lib/:"), ("HOME", "/home/u")]);
        assert_eq!(out, vec![("HOME".to_string(), "/home/u".to_string())]);
    }

    #[test]
    fn nothing_changes_outside_an_appimage() {
        let vars = [(OsString::from("PATH"), OsString::from("/usr/bin::/bin"))];
        assert_eq!(env_outside_appimage(vars.clone().into_iter(), ""), vars.to_vec());
    }
}
