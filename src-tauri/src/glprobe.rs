// Remove this module once GTK can use OpenGL on Wayland with Mesa 26.3 again, i.e. when
// https://gitlab.gnome.org/GNOME/gtk/-/issues/8566 and
// https://gitlab.freedesktop.org/mesa/mesa/-/issues/16934 are fixed and shipped.
//
// Mesa 26.3.1 built without GLES1 (Arch, CachyOS) lists EGL configs as GL|GLES2|GLES3 only.
// GTK3 asks eglChooseConfig without EGL_RENDERABLE_TYPE, so nothing matches on native Wayland,
// GTK has no OpenGL and WebKitGTK renders in software. Under XWayland it works, so the launcher
// probes once at start and restarts itself with GDK_BACKEND=x11 when the probe fails.
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;

const MARKER: &str = "BLIZZNUX_GL_FALLBACK";
const OPT_OUT: &str = "BLIZZNUX_NO_GL_FALLBACK";
/// The GDK_SCALE the user had before the restart set it to 1.
const PREV_SCALE: &str = "BLIZZNUX_GL_PREV_GDK_SCALE";

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// A zoom factor from text: a finite number from 0.5 to 4.0, else None.
fn parse_zoom(text: &str) -> Option<f64> {
    let z: f64 = text.trim().parse().ok()?;
    (z.is_finite() && (0.5..=4.0).contains(&z)).then_some(z)
}

fn env_zoom(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| parse_zoom(&v))
}

/// Hyprland's `getoption -j xwayland:force_zero_scaling`: is X11 apps' scaling left to them?
/// Older builds answer in "int", the Lua build in "bool"; "custom" and "str" are text.
fn force_zero_scaling(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    v.get("int").and_then(|i| i.as_i64()) == Some(1)
        || v.get("bool").and_then(|b| b.as_bool()) == Some(true)
        || ["custom", "str"]
            .iter()
            .any(|k| v.get(*k).and_then(|t| t.as_str()).is_some_and(|t| matches!(t.trim(), "true" | "1")))
}

/// Hyprland's `monitors -j`: the scale of the focused monitor, else of the first, clamped.
fn monitor_scale(json: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let list = v.as_array()?;
    let pick = list
        .iter()
        .find(|m| m.get("focused").and_then(|f| f.as_bool()) == Some(true))
        .or_else(|| list.first())?;
    let s = pick.get("scale")?.as_f64()?;
    s.is_finite().then(|| s.clamp(0.5, 4.0))
}

fn hyprctl(args: &[&str]) -> Option<String> {
    let out = Command::new("hyprctl").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_cosmic_desktop(xdg_current_desktop: &str) -> bool {
    xdg_current_desktop.split(':').any(|part| part.trim().eq_ignore_ascii_case("COSMIC"))
}

/// COSMIC's `descale_xwayland` config: bool (`true`/`false`) or RON enum (`Enabled`/`Fractional`/`Disabled`).
fn cosmic_descale_xwayland_value(text: &str) -> bool {
    let t = text.trim();
    t == "true" || t == "Enabled" || t == "Fractional" || t.starts_with("Enabled(")
}

fn cosmic_descale_xwayland() -> bool {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| crate::home().join(".config"));
    let path = base.join("cosmic/com.system76.CosmicComp/v1/descale_xwayland");
    std::fs::read_to_string(path).is_ok_and(|t| cosmic_descale_xwayland_value(&t))
}

/// COSMIC's `cosmic-randr list`: the scale of the enabled XWayland-primary output, else the first enabled output.
fn cosmic_monitor_scale(text: &str) -> Option<f64> {
    let mut enabled = false;
    let mut scale: Option<f64> = None;
    let mut primary = false;
    let mut first_scale: Option<f64> = None;

    let flush = |enabled: bool, scale: Option<f64>, primary: bool, first: &mut Option<f64>| -> Option<f64> {
        if !enabled {
            return None;
        }
        let s = scale?;
        if primary {
            return Some(s);
        }
        if first.is_none() {
            *first = Some(s);
        }
        None
    };

    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') && !line.trim().is_empty() {
            if let Some(found) = flush(enabled, scale, primary, &mut first_scale) {
                return Some(found);
            }
            enabled = line.contains("(enabled)");
            scale = None;
            primary = false;
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Scale:") {
            let num = rest.trim().trim_end_matches('%').trim();
            if let Ok(pct) = num.parse::<f64>() {
                let s = pct / 100.0;
                if s.is_finite() && s > 0.0 {
                    scale = Some(s.clamp(0.5, 4.0));
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("Xwayland primary:") {
            primary = rest.trim() == "true";
        }
    }
    flush(enabled, scale, primary, &mut first_scale).or(first_scale)
}

fn cosmic_randr_list() -> Option<String> {
    let out = Command::new("cosmic-randr").arg("list").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Display scale when the compositor leaves XWayland windows unscaled (Hyprland
/// `force_zero_scaling` or COSMIC `descale_xwayland`).
fn xwayland_unscaled_display_scale() -> Option<f64> {
    if env_set("HYPRLAND_INSTANCE_SIGNATURE") {
        let zero = hyprctl(&["-j", "getoption", "xwayland:force_zero_scaling"]);
        if zero.is_some_and(|j| force_zero_scaling(&j)) {
            if let Some(z) = hyprctl(&["-j", "monitors"]).and_then(|j| monitor_scale(&j)) {
                return Some(z);
            }
        }
    }
    if std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| is_cosmic_desktop(&d))
        && cosmic_descale_xwayland()
    {
        if let Some(z) = cosmic_randr_list().and_then(|t| cosmic_monitor_scale(&t)) {
            return Some(z);
        }
    }
    None
}

/// The zoom an X11 window needs to look as large as it would on Wayland. Hyprland with
/// force_zero_scaling and COSMIC with descale_xwayland leave X11 apps unscaled; GNOME and KDE
/// scale them themselves.
fn wanted_zoom() -> f64 {
    if let Some(z) = env_zoom("BLIZZNUX_ZOOM") {
        return z;
    }
    xwayland_unscaled_display_scale().unwrap_or(1.0)
}

/// Suggested Windows DPI for Battle.net inside Wine: BLIZZNUX_DPI, or the compositor's scale
/// when XWayland windows are left unscaled (skipped when PROTON_ENABLE_WAYLAND=1).
pub(crate) fn detected_dpi() -> Option<u32> {
    if let Some(dpi) = std::env::var("BLIZZNUX_DPI")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|d| (72..=384).contains(d))
    {
        return Some(dpi);
    }
    if std::env::var("PROTON_ENABLE_WAYLAND").is_ok_and(|v| v.trim() == "1") {
        return None;
    }
    let scale = xwayland_unscaled_display_scale()?;
    let dpi = (96.0 * scale).round().clamp(72.0, 384.0) as u32;
    (dpi > 96).then_some(dpi)
}

/// What a key press does to the zoom.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ZoomKey {
    In,
    Out,
    Reset,
}

/// Ctrl+plus, Ctrl+minus and Ctrl+0 (also on the number pad); None for any other key.
pub fn zoom_key(key: &gtk::gdk::keys::Key) -> Option<ZoomKey> {
    use gtk::gdk::keys::constants as k;
    if *key == k::plus || *key == k::equal || *key == k::KP_Add {
        Some(ZoomKey::In)
    } else if *key == k::minus || *key == k::KP_Subtract {
        Some(ZoomKey::Out)
    } else if *key == k::_0 || *key == k::KP_0 {
        Some(ZoomKey::Reset)
    } else {
        None
    }
}

/// The zoom after a key: steps of 0.1, kept within 0.5 to 3.0 and rounded to two decimals.
pub fn step_zoom(current: f64, key: ZoomKey) -> f64 {
    let z = match key {
        ZoomKey::In => current + 0.1,
        ZoomKey::Out => current - 0.1,
        ZoomKey::Reset => 1.0,
    };
    ((z.clamp(0.5, 3.0)) * 100.0).round() / 100.0
}

/// The zoom at start: BLIZZNUX_ZOOM, then the saved one, then (restarted under XWayland) the
/// detected one, then 1.
pub fn start_zoom(saved: Option<&str>) -> f64 {
    env_zoom("BLIZZNUX_ZOOM")
        .or_else(|| saved.and_then(parse_zoom))
        .or_else(|| if env_set(MARKER) { env_zoom("BLIZZNUX_GL_ZOOM") } else { None })
        .unwrap_or(1.0)
}

/// The zoom to apply in a restarted process; None unless this is one and the zoom is not 1.
pub fn fallback_zoom() -> Option<f64> {
    if !env_set(MARKER) {
        return None;
    }
    let z = env_zoom("BLIZZNUX_ZOOM").or_else(|| env_zoom("BLIZZNUX_GL_ZOOM")).unwrap_or(1.0);
    (z != 1.0).then_some(z)
}

/// Can GTK create an OpenGL context on its default (Wayland) display? Err carries why not.
/// Ok(false) means GTK itself would not start, which Tauri reports on its own.
fn probe() -> Result<bool, String> {
    if gtk::init().is_err() {
        return Ok(false);
    }
    use gtk::prelude::*;
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.realize();
    let result = match window.window() {
        None => Err("no GDK window".to_string()),
        Some(gdk_window) => gdk_window
            .create_gl_context()
            .and_then(|ctx| ctx.realize())
            .map_err(|e| e.to_string()),
    };
    #[allow(unused_unsafe)]
    unsafe {
        window.destroy();
    }
    result.map(|_| true)
}

fn note(line: &str) {
    eprintln!("{line}");
    let path = crate::log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Called first thing in run(), after set_app_id(): on Wayland, restarts under XWayland when
/// GTK cannot use OpenGL. Does nothing in every other case.
pub fn maybe_reexec() {
    if !env_set("WAYLAND_DISPLAY") || env_set("GDK_BACKEND") || env_set(MARKER) || env_set(OPT_OUT) {
        return;
    }
    let why = match probe() {
        Ok(true) | Ok(false) => return,
        Err(e) => e,
    };
    let z = wanted_zoom();
    note(&format!(
        "[gl] GTK could not use OpenGL on Wayland ({why}); restarting under XWayland (GDK_BACKEND=x11), zoom {z}. See gtk#8566 / mesa#16934."
    ));
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env("GDK_BACKEND", "x11")
        .env(MARKER, "1")
        .env("BLIZZNUX_GL_ZOOM", z.to_string());
    if z != 1.0 {
        if let Some(prev) = std::env::var_os("GDK_SCALE") {
            cmd.env(PREV_SCALE, prev);
        }
        cmd.env("GDK_SCALE", "1");
    }
    let err = cmd.exec();
    note(&format!("[gl] restart under XWayland failed ({err}); continuing on Wayland."));
}

/// What the restart under XWayland put in our environment, for the launcher's own window only.
/// Battle.net, the games and WowUp must not inherit it: their windows would land on XWayland
/// at the wrong scale. A GDK_SCALE the user had set before the restart is put back.
pub fn scrub_child_env(cmd: &mut Command) {
    if !env_set(MARKER) {
        return;
    }
    for (k, v) in child_env_changes(std::env::var_os(PREV_SCALE)) {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
}

fn child_env_changes(prev_scale: Option<std::ffi::OsString>) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    vec![
        ("GDK_BACKEND", None),
        (MARKER, None),
        ("BLIZZNUX_GL_ZOOM", None),
        (PREV_SCALE, None),
        ("GDK_SCALE", prev_scale),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_lose_the_fallback_env_and_get_their_own_scale_back() {
        let none = child_env_changes(None);
        for k in ["GDK_BACKEND", MARKER, "BLIZZNUX_GL_ZOOM", PREV_SCALE, "GDK_SCALE"] {
            assert!(none.iter().any(|(n, v)| *n == k && v.is_none()), "{k} not removed");
        }
        let back = child_env_changes(Some("2".into()));
        assert!(back.iter().any(|(n, v)| *n == "GDK_SCALE" && v.as_deref() == Some(std::ffi::OsStr::new("2"))));
    }

    #[test]
    fn zoom_text_must_be_a_sane_number() {
        assert_eq!(parse_zoom("1.5"), Some(1.5));
        assert_eq!(parse_zoom(" 2 "), Some(2.0));
        assert_eq!(parse_zoom("0.4"), None);
        assert_eq!(parse_zoom("4.1"), None);
        assert_eq!(parse_zoom("NaN"), None);
        assert_eq!(parse_zoom("inf"), None);
        assert_eq!(parse_zoom("big"), None);
        assert_eq!(parse_zoom(""), None);
    }

    #[test]
    fn force_zero_scaling_reads_int_bool_and_text() {
        assert!(force_zero_scaling(r#"{"option": "xwayland:force_zero_scaling", "bool": true, "set": true }"#));
        assert!(!force_zero_scaling(r#"{"option": "xwayland:force_zero_scaling", "bool": false, "set": true }"#));
        assert!(force_zero_scaling(r#"{"custom":"true"}"#));
        assert!(force_zero_scaling(r#"{"str":"1"}"#));
        assert!(!force_zero_scaling(r#"{"str":"0"}"#));
    }

    #[test]
    fn zoom_keys_map_to_in_out_and_reset() {
        use gtk::gdk::keys::constants as k;
        assert_eq!(zoom_key(&k::plus), Some(ZoomKey::In));
        assert_eq!(zoom_key(&k::equal), Some(ZoomKey::In));
        assert_eq!(zoom_key(&k::KP_Add), Some(ZoomKey::In));
        assert_eq!(zoom_key(&k::minus), Some(ZoomKey::Out));
        assert_eq!(zoom_key(&k::KP_Subtract), Some(ZoomKey::Out));
        assert_eq!(zoom_key(&k::_0), Some(ZoomKey::Reset));
        assert_eq!(zoom_key(&k::KP_0), Some(ZoomKey::Reset));
        assert_eq!(zoom_key(&k::a), None);
    }

    #[test]
    fn zoom_steps_are_tenths_within_bounds() {
        assert_eq!(step_zoom(1.0, ZoomKey::In), 1.1);
        assert_eq!(step_zoom(1.1, ZoomKey::In), 1.2);
        assert_eq!(step_zoom(1.0, ZoomKey::Out), 0.9);
        assert_eq!(step_zoom(0.5, ZoomKey::Out), 0.5);
        assert_eq!(step_zoom(3.0, ZoomKey::In), 3.0);
        assert_eq!(step_zoom(2.3, ZoomKey::Reset), 1.0);
    }

    #[test]
    fn force_zero_scaling_is_read_from_the_int_field() {
        assert!(force_zero_scaling(r#"{"option":"xwayland:force_zero_scaling","int":1,"set":true}"#));
        assert!(!force_zero_scaling(r#"{"option":"xwayland:force_zero_scaling","int":0,"set":true}"#));
        assert!(!force_zero_scaling("not json"));
        assert!(!force_zero_scaling("{}"));
    }

    #[test]
    fn the_focused_monitors_scale_is_used_and_clamped() {
        let two = r#"[{"name":"A","scale":1.0,"focused":false},{"name":"B","scale":1.5,"focused":true}]"#;
        assert_eq!(monitor_scale(two), Some(1.5));
        let none = r#"[{"name":"A","scale":2.0,"focused":false},{"name":"B","scale":1.0,"focused":false}]"#;
        assert_eq!(monitor_scale(none), Some(2.0));
        assert_eq!(monitor_scale(r#"[{"scale":9.0,"focused":true}]"#), Some(4.0));
        assert_eq!(monitor_scale("[]"), None);
        assert_eq!(monitor_scale("oops"), None);
    }

    #[test]
    fn cosmic_descale_xwayland_reads_bool_and_enum() {
        assert!(cosmic_descale_xwayland_value("true\n"));
        assert!(cosmic_descale_xwayland_value("Enabled"));
        assert!(cosmic_descale_xwayland_value("Fractional"));
        assert!(cosmic_descale_xwayland_value("Enabled(All)"));
        assert!(!cosmic_descale_xwayland_value("false"));
        assert!(!cosmic_descale_xwayland_value("Disabled"));
        assert!(!cosmic_descale_xwayland_value(""));
        assert!(is_cosmic_desktop("COSMIC"));
        assert!(is_cosmic_desktop("pop:COSMIC"));
        assert!(!is_cosmic_desktop("GNOME"));
    }

    #[test]
    fn cosmic_monitor_scale_prefers_xwayland_primary_enabled_output() {
        let multi = "\
HDMI-1 (disabled)
  Scale: 200%
  Xwayland primary: true
DP-2 (enabled)
  Scale: 100%
  Xwayland primary: false
DP-1 (enabled)
  Scale: 125%
  Xwayland primary: true
";
        assert_eq!(cosmic_monitor_scale(multi), Some(1.25));
        let no_primary = "\
DP-2 (enabled)
  Scale: 150%
";
        assert_eq!(cosmic_monitor_scale(no_primary), Some(1.5));
        assert_eq!(cosmic_monitor_scale("HDMI-1 (disabled)\n  Scale: 200%\n"), None);
        assert_eq!(cosmic_monitor_scale(""), None);
    }
}
