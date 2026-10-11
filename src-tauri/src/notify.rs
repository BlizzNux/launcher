//! A desktop notification when a game session ends. The "how did it run?" question opens inside
//! the launcher, which usually sits behind other windows by then; without a nudge it is easy to
//! miss. Clicking the notification brings the launcher forward. Everything here runs on the
//! window's thread: the D-Bus replies and signals arrive through GTK's main loop.
use std::cell::RefCell;

use glib::{ToVariant, Variant, VariantDict, VariantTy};
use gtk::prelude::GtkWindowExt;
use tauri::{AppHandle, Manager};

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_ID: &str = "com.blizznux.launcher";

#[derive(Default)]
struct State {
    /// The notification on screen, as the notification server numbered it.
    shown: Option<u32>,
    /// What lets a Wayland window bring itself forward; the server sends it just before a click.
    token: Option<String>,
    listening: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Ask the user, from outside the launcher, to answer the question that just opened in it.
/// Nothing is shown when the launcher is the window in front: the question is already there.
pub fn game_ended(app: &AppHandle, game: &str) {
    let (h, game) = (app.clone(), game.to_string());
    let _ = app.run_on_main_thread(move || {
        let Some(win) = h.get_webview_window("main") else { return };
        if win.is_focused().unwrap_or(false) {
            return;
        }
        let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
        listen(&bus, &h);
        let hints = VariantDict::new(None);
        hints.insert("desktop-entry", APP_ID);
        hints.insert("urgency", 1u8);
        let replaces = STATE.with(|s| s.borrow().shown).unwrap_or(0);
        let args = Variant::tuple_from_iter([
            "BlizzNux".to_variant(),
            replaces.to_variant(),
            APP_ID.to_variant(),
            format!("How did {game} run?").to_variant(),
            "Rate it in BlizzNux so other players know what works.".to_variant(),
            vec!["default", "Rate now"].to_variant(),
            hints.end(),
            (-1i32).to_variant(),
        ]);
        bus.call(Some(BUS), PATH, BUS, "Notify", Some(&args), VariantTy::new("(u)").ok(), gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, |reply| {
            if let Some((id,)) = reply.ok().and_then(|v| v.get::<(u32,)>()) {
                STATE.with(|s| s.borrow_mut().shown = Some(id));
            }
        });
    });
}

/// Take the notification away: the user came to the launcher by themselves. Call on the
/// window's thread.
pub fn withdraw() {
    let Some(id) = STATE.with(|s| s.borrow_mut().shown.take()) else { return };
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    bus.call(Some(BUS), PATH, BUS, "CloseNotification", Some(&(id,).to_variant()), None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, |_| {});
}

fn listen(bus: &gio::DBusConnection, app: &AppHandle) {
    if STATE.with(|s| std::mem::replace(&mut s.borrow_mut().listening, true)) {
        return;
    }
    let app = app.clone();
    bus.signal_subscribe(Some(BUS), Some(BUS), None, Some(PATH), None, gio::DBusSignalFlags::NONE, move |_, _, _, _, signal, params| {
        let ours = |id: u32| STATE.with(|s| s.borrow().shown == Some(id));
        match signal {
            "ActivationToken" => {
                if let Some((_, token)) = params.get::<(u32, String)>().filter(|(id, _)| ours(*id)) {
                    STATE.with(|s| s.borrow_mut().token = Some(token));
                }
            }
            "ActionInvoked" => {
                if params.get::<(u32, String)>().map_or(false, |(id, _)| ours(id)) {
                    STATE.with(|s| s.borrow_mut().shown = None);
                    bring_forward(&app);
                }
            }
            "NotificationClosed" => {
                if params.get::<(u32, u32)>().map_or(false, |(id, _)| ours(id)) {
                    STATE.with(|s| *s.borrow_mut() = State { listening: true, ..State::default() });
                }
            }
            _ => {}
        }
    });
}

fn bring_forward(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else { return };
    let _ = win.unminimize();
    let _ = win.show();
    if let Ok(gtk_win) = win.gtk_window() {
        if let Some(token) = STATE.with(|s| s.borrow_mut().token.take()) {
            gtk_win.set_startup_id(&token);
        }
        gtk_win.present();
    }
    let _ = win.set_focus();
}
