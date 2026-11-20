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
fn force_zero_scaling(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("int").and_then(|i| i.as_i64()))
        == Some(1)
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

/// The zoom an X11 window needs to look as large as it would on Wayland. Hyprland with
/// force_zero_scaling leaves X11 apps unscaled; GNOME and KDE scale them themselves.
fn wanted_zoom() -> f64 {
    if let Some(z) = env_zoom("BLIZZNUX_ZOOM") {
        return z;
    }
    if env_set("HYPRLAND_INSTANCE_SIGNATURE") {
        let zero = hyprctl(&["-j", "getoption", "xwayland:force_zero_scaling"]);
        if zero.is_some_and(|j| force_zero_scaling(&j)) {
            if let Some(z) = hyprctl(&["-j", "monitors"]).and_then(|j| monitor_scale(&j)) {
                return z;
            }
        }
    }
    1.0
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
        cmd.env("GDK_SCALE", "1");
    }
    let err = cmd.exec();
    note(&format!("[gl] restart under XWayland failed ({err}); continuing on Wayland."));
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
