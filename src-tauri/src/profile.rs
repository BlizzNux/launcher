//! Keeps the user's blizznux.com profile fields in step with this machine.
//!
//! The site's profile fields (FoF Masquerade) are matched by name: Distro, Kernel, GPU,
//! GPU driver, Proton / Wine. The launcher writes them through the site's own endpoint
//! (`POST /api/masquerade-answers/configure/<user id>`) as the user who is logged in inside the
//! embedded view, so the site's permissions decide what is allowed. Nothing is sent while the
//! values are unchanged since the last successful write.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::reports::{http, site_host, system_profile, Gpu, SystemProfile};
use crate::{load_config, save_config};

const SYNC_KEY: &str = "PROFILE_SYNC";
const SYNCED_KEY: &str = "PROFILE_SYNCED";

/// Whether profile updates are on. Absent switch = follows the sharing answer.
pub(crate) fn enabled(cfg: &BTreeMap<String, String>) -> bool {
    match cfg.get(SYNC_KEY).map(String::as_str) {
        Some(v) => v == "1",
        None => cfg.get("REPORTS_SHARE").map_or(false, |v| v == "1"),
    }
}

#[derive(serde::Serialize)]
pub struct ProfileSync {
    /// "not_linked", "disabled", "unchanged" or "updated".
    status: String,
    /// Site field names that were written (status "updated").
    fields: Vec<String>,
}

fn done(status: &str, fields: Vec<String>) -> ProfileSync {
    ProfileSync { status: status.into(), fields }
}

/// Canonical key for a site field name, so renames like "GPU Driver" or "Proton/Wine" still match.
fn field_key(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let compact: String = lower.split_whitespace().collect::<Vec<_>>().join(" ").replace(" / ", "/").replace(" /", "/").replace("/ ", "/");
    match compact.as_str() {
        "distro" | "distribution" | "linux distro" | "linux distribution" | "os" => "distro",
        "kernel" | "linux kernel" | "kernel version" => "kernel",
        "gpu" | "graphics card" | "graphics" | "video card" => "gpu",
        "gpu driver" | "graphics driver" | "video driver" | "driver" => "gpu driver",
        "proton/wine" | "proton" | "wine" | "wine/proton" | "proton version" | "compatibility layer" => "proton / wine",
        other => other,
    }
    .to_string()
}

/// "NVIDIA Corporation AD107M [GeForce RTX 4060 Max-Q / Mobile] [10de:28a0] (rev a1)"
/// becomes "NVIDIA GeForce RTX 4060 Max-Q / Mobile"; "Advanced Micro Devices, Inc. [AMD/ATI]
/// Phoenix1 [1002:15bf]" becomes "AMD Phoenix1".
fn pretty_gpu(g: &Gpu) -> String {
    let label = match g.vendor.as_str() { "nvidia" => "NVIDIA", "amd" => "AMD", "intel" => "Intel", _ => "" };
    let mut s = g.name.clone();
    // Drop the PCI id and revision, and the "[AMD/ATI]" marker.
    s = strip_brackets(&s, |inner| inner.len() == 9 && inner.as_bytes()[4] == b':' || inner.eq_ignore_ascii_case("AMD/ATI"));
    if let Some(open) = s.rfind('(') {
        if s[open..].starts_with("(rev") {
            s.truncate(open);
        }
    }
    // A remaining [...] is the marketing name; otherwise take what follows the company name.
    let model = match (s.rfind('['), s.rfind(']')) {
        (Some(a), Some(b)) if b > a => s[a + 1..b].trim().to_string(),
        _ => {
            let mut rest = s.trim().to_string();
            for company in ["NVIDIA Corporation", "Advanced Micro Devices, Inc.", "Intel Corporation", "NVIDIA", "AMD", "Intel"] {
                if let Some(r) = rest.strip_prefix(company) {
                    rest = r.trim().to_string();
                    break;
                }
            }
            rest
        }
    };
    let model = model.split_whitespace().collect::<Vec<_>>().join(" ");
    if model.is_empty() {
        return if label.is_empty() { g.name.clone() } else { label.into() };
    }
    if !label.is_empty() && model.eq_ignore_ascii_case(label) {
        return label.into();
    }
    if label.is_empty() || model.to_lowercase().starts_with(&label.to_lowercase()) {
        model
    } else {
        format!("{label} {model}")
    }
}

fn strip_brackets(s: &str, drop: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('[') {
        let Some(b) = rest[a..].find(']') else { break };
        let inner = &rest[a + 1..a + b];
        out.push_str(&rest[..a]);
        if !drop(inner) {
            out.push('[');
            out.push_str(inner);
            out.push(']');
        }
        rest = &rest[a + b + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pretty_driver(g: &Gpu) -> String {
    match g.vendor.as_str() {
        "nvidia" if !g.driver.is_empty() => format!("NVIDIA {}", g.driver),
        _ if g.driver.is_empty() => String::new(),
        _ => g.driver.clone(),
    }
}

/// The five values in site-field order: distro, kernel, gpu, gpu driver, proton / wine.
fn values(sp: &SystemProfile) -> Vec<(&'static str, String)> {
    let mut gpus: Vec<String> = Vec::new();
    let mut drivers: Vec<String> = Vec::new();
    // The card that runs games first: on hybrid laptops that is the NVIDIA one, not the display iGPU.
    let mut ordered: Vec<&Gpu> = sp.gpus.iter().collect();
    ordered.sort_by_key(|g| match g.vendor.as_str() { "nvidia" => 0, "amd" => 1, "intel" => 2, _ => 3 });
    for g in ordered {
        let n = pretty_gpu(g);
        if !n.is_empty() && !gpus.contains(&n) {
            gpus.push(n);
        }
        let d = pretty_driver(g);
        if !d.is_empty() && !drivers.contains(&d) {
            drivers.push(d);
        }
    }
    vec![
        ("distro", sp.distro.trim().to_string()),
        ("kernel", sp.kernel.trim().to_string()),
        ("gpu", gpus.join(" + ")),
        ("gpu driver", drivers.join(" + ")),
        ("proton / wine", pretty_proton(&sp.proton)),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .collect()
}

/// "cachyos-11.0-20260703-slr" reads as "Proton cachyos-11.0-20260703-slr"; names that already say
/// Proton or Wine (GE-Proton11-7, Wine 11) are left alone.
fn pretty_proton(v: &str) -> String {
    let v = v.trim();
    let lower = v.to_lowercase();
    if v.is_empty() || lower.contains("proton") || lower.contains("wine") {
        v.to_string()
    } else {
        format!("Proton {v}")
    }
}

fn fingerprint(vals: &[(&str, String)]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in vals {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// The embedded view's cookies for the site, as one Cookie header. Read from WebKit's own store,
/// so the request is made as whoever is logged in inside the launcher.
async fn cookie_header(app: &AppHandle, url: &str) -> Result<String, String> {
    let main = app.get_webview_window("main").ok_or("main window missing")?;
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    let url = url.to_string();
    main.with_webview(move |platform| {
        #[cfg(target_os = "linux")]
        {
            use webkit2gtk::{CookieManagerExt, WebContextExt, WebViewExt};
            let Some(ctx) = platform.inner().context() else {
                let _ = tx.send(Err("no web context".into()));
                return;
            };
            let Some(cm) = ctx.cookie_manager() else {
                let _ = tx.send(Err("no cookie manager".into()));
                return;
            };
            cm.cookies(&url, None::<&webkit2gtk::gio::Cancellable>, move |res| {
                let _ = tx.send(match res {
                    Ok(mut cookies) => Ok(cookies
                        .iter_mut()
                        .filter_map(|c| Some(format!("{}={}", c.name()?, c.value()?)))
                        .collect::<Vec<_>>()
                        .join("; ")),
                    Err(e) => Err(e.to_string()),
                });
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = tx.send(Err("profile sync needs the Linux build".into()));
        }
    })
    .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)).map_err(|_| "cookie store did not answer".to_string())?)
        .await
        .map_err(|e| e.to_string())?
}

/// Replace or add one cookie in a Cookie header string.
fn with_cookie(header: &str, name: &str, value: &str) -> String {
    let mut parts: Vec<String> = header
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.starts_with(&format!("{name}=")))
        .map(String::from)
        .collect();
    parts.push(format!("{name}={value}"));
    parts.join("; ")
}

fn session_from_set_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers().get_all(reqwest::header::SET_COOKIE).iter().find_map(|h| {
        let s = h.to_str().ok()?;
        let kv = s.split(';').next()?.trim();
        kv.strip_prefix("flarum_session=").map(String::from)
    })
}

/// Write this machine's setup into the user's profile fields on the site. `force` skips the
/// unchanged check (the "Update now" button).
#[tauri::command]
pub async fn profile_sync(app: AppHandle, force: bool) -> Result<ProfileSync, String> {
    let cfg = load_config();
    if cfg.get("USER_TOKEN").map_or(true, |t| t.is_empty()) {
        return Ok(done("not_linked", vec![]));
    }
    if !enabled(&cfg) {
        return Ok(done("disabled", vec![]));
    }
    let username = cfg.get("USERNAME").cloned().unwrap_or_default();
    if username.is_empty() {
        return Err("the account name is missing; log out of the launcher and log in again".into());
    }

    let sp = tauri::async_runtime::spawn_blocking(system_profile).await.map_err(|e| e.to_string())?;
    let vals = values(&sp);
    if vals.is_empty() {
        return Err("nothing to write: no setup values were detected".into());
    }
    let fp = fingerprint(&vals);
    if !force && cfg.get(SYNCED_KEY).map_or(false, |v| *v == fp) {
        return Ok(done("unchanged", vec![]));
    }

    let base = format!("https://{}", site_host());
    let mut cookies = cookie_header(&app, &base).await?;
    if !cookies.contains("flarum_session=") && !cookies.contains("flarum_remember=") {
        return Err("not logged in on the site inside the launcher".into());
    }
    let client = http()?;
    let accept = "application/vnd.api+json";

    // One GET gives the CSRF token for this session and the site's field definitions.
    let forum = client
        .get(format!("{base}/api"))
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::COOKIE, cookies.clone())
        .send()
        .await
        .map_err(|e| format!("site unreachable: {e}"))?;
    if let Some(s) = session_from_set_cookie(&forum) {
        cookies = with_cookie(&cookies, "flarum_session", &s);
    }
    let csrf = forum
        .headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or("the site did not hand out a CSRF token")?;
    let forum: serde_json::Value = forum.json().await.map_err(|e| e.to_string())?;
    let mut field_ids: HashMap<String, (String, String)> = HashMap::new(); // key -> (id, site name)
    for inc in forum.get("included").and_then(|v| v.as_array()).into_iter().flatten() {
        if inc.get("type").and_then(|t| t.as_str()) != Some("masquerade-fields") {
            continue;
        }
        let (Some(id), Some(name)) = (inc.get("id").and_then(|v| v.as_str()), inc.pointer("/attributes/name").and_then(|v| v.as_str())) else { continue };
        if inc.pointer("/attributes/deleted_at").map_or(false, |v| !v.is_null()) {
            continue;
        }
        if inc.pointer("/attributes/type").and_then(|v| v.as_str()).map_or(false, |t| t != "text" && t != "url") {
            continue; // only free-text fields take these values
        }
        field_ids.entry(field_key(name)).or_insert((id.to_string(), name.to_string()));
    }
    if field_ids.is_empty() {
        return Err("the site has no profile fields to fill".into());
    }

    let mut body = serde_json::Map::new();
    let mut written: Vec<String> = Vec::new();
    for (key, value) in &vals {
        if let Some((id, site_name)) = field_ids.get(*key) {
            body.insert(id.clone(), serde_json::Value::String(value.clone()));
            written.push(site_name.clone());
        }
    }
    if body.is_empty() {
        return Err("none of the site's profile fields match Distro, Kernel, GPU, GPU driver or Proton / Wine".into());
    }

    // The user's id from their name; the configure endpoint is addressed by id.
    let user: serde_json::Value = client
        .get(format!("{base}/api/users/{}?bySlug=1", urlencoding(&username)))
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::COOKIE, cookies.clone())
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let user_id = user
        .pointer("/data/id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!("the site does not know a user named {username}"))?;

    let resp = client
        .post(format!("{base}/api/masquerade-answers/configure/{user_id}"))
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, cookies)
        .header("X-CSRF-Token", csrf)
        .json(&serde_json::Value::Object(body))
        .send()
        .await
        .map_err(|e| format!("site unreachable: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            401 => "the site does not see you as logged in; open blizznux.com in the launcher and log in".to_string(),
            403 => "your account is not allowed to edit profile fields on blizznux.com".to_string(),
            419 => "the site rejected the session token; try again in a moment".to_string(),
            code => format!("profile update failed ({code}): {}", text.chars().take(200).collect::<String>()),
        });
    }

    let mut cfg = load_config();
    cfg.insert(SYNCED_KEY.into(), fp);
    save_config(&cfg)?;
    Ok(done("updated", written))
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(vendor: &str, name: &str, driver: &str) -> Gpu {
        Gpu { vendor: vendor.into(), name: name.into(), driver: driver.into() }
    }

    #[test]
    fn gpu_names_are_tidied() {
        assert_eq!(
            pretty_gpu(&gpu("nvidia", "NVIDIA Corporation AD107M [GeForce RTX 4060 Max-Q / Mobile] [10de:28a0] (rev a1)", "")),
            "NVIDIA GeForce RTX 4060 Max-Q / Mobile"
        );
        assert_eq!(pretty_gpu(&gpu("amd", "Advanced Micro Devices, Inc. [AMD/ATI] Phoenix1 [1002:15bf]", "")), "AMD Phoenix1");
        assert_eq!(pretty_gpu(&gpu("amd", "Advanced Micro Devices, Inc. [AMD/ATI] Navi 31 [Radeon RX 7900 XT/7900 XTX] [1002:744c] (rev c8)", "")), "AMD Radeon RX 7900 XT/7900 XTX");
        assert_eq!(pretty_gpu(&gpu("intel", "Intel Corporation Raptor Lake-P [Iris Xe Graphics] [8086:a7a0] (rev 04)", "")), "Intel Iris Xe Graphics");
        assert_eq!(pretty_gpu(&gpu("nvidia", "nvidia", "")), "NVIDIA");
    }

    #[test]
    fn drivers_and_fields() {
        assert_eq!(pretty_driver(&gpu("nvidia", "x", "615.71.09")), "NVIDIA 615.71.09");
        assert_eq!(pretty_driver(&gpu("amd", "x", "Mesa 26.2.4")), "Mesa 26.2.4");
        assert_eq!(field_key("GPU Driver"), "gpu driver");
        assert_eq!(field_key("Proton/Wine"), "proton / wine");
        assert_eq!(field_key("  Proton / Wine "), "proton / wine");
        assert_eq!(field_key("Distro"), "distro");
        assert_eq!(pretty_proton("cachyos-11.0-20260703-slr"), "Proton cachyos-11.0-20260703-slr");
        assert_eq!(pretty_proton("GE-Proton11-7"), "GE-Proton11-7");
        assert_eq!(pretty_proton("GE-Proton (downloaded by umu)"), "GE-Proton (downloaded by umu)");
        let sp = SystemProfile {
            distro: "CachyOS".into(), kernel: "7.2.8".into(), desktop: String::new(), session: String::new(), cpu: String::new(), ram_gb: 0,
            gpus: vec![gpu("amd", "Advanced Micro Devices, Inc. [AMD/ATI] Phoenix1 [1002:15bf]", "Mesa 26.2.3"), gpu("nvidia", "NVIDIA Corporation AD107M [GeForce RTX 4060 Max-Q / Mobile] [10de:28a0] (rev a1)", "615.71.09")],
            proton: "cachyos-11.0".into(), umu: String::new(), prefix_kind: String::new(),
        };
        let v = values(&sp);
        assert_eq!(v[2], ("gpu", "NVIDIA GeForce RTX 4060 Max-Q / Mobile + AMD Phoenix1".to_string()));
        assert_eq!(v[3], ("gpu driver", "NVIDIA 615.71.09 + Mesa 26.2.3".to_string()));
        assert_eq!(with_cookie("a=1; flarum_session=old; b=2", "flarum_session", "new"), "a=1; b=2; flarum_session=new");
    }
}
