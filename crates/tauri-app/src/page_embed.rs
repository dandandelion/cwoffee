//! The GTK half of the embedded browser: the widget the page lives in, and the only thread that
//! is allowed to touch it.
//!
//! Tauri's own multi-webview path (`Window::add_child`) cannot do this on Linux. It packs the child
//! into wry's default `GtkBox`, where a position and a size are not a thing a box child has — the
//! page comes up covering the whole window — and once a window holds two webviews,
//! `get_webview_window("main")` stops resolving, which silently switches off the window controls,
//! the self-test and every command that reaches for `"main"`.
//!
//! wry says what it does want, in `WebViewBuilderExtUnix::build_gtk`:
//!
//! > If the container is [`gtk::Fixed`], its size request will be set using the (width, height)
//! > bounds passed in and will be added to the container using `Fixed::put` using the (x, y) bounds.
//!
//! So this module builds the container Tauri never builds: the window's existing box (which holds
//! the app's own page) is moved inside a `GtkOverlay`, and a `gtk::Fixed` is added as the overlay
//! child. The box is the overlay's main child, so it still fills the window; the fixed floats over
//! it, and that is where the browser page goes. All of it is inside one window — no second window,
//! no window manager involvement — so moving the page means moving a widget.
//!
//! Two rules this file exists to enforce:
//!
//! * **GTK calls happen on the GTK thread, and only that thread holds the page widget.** Every
//!   operation here is a message to that thread. Nothing else in the app is allowed to own a
//!   `wry::WebView`, and that is not ceremony: a widget destroyed on the wrong thread is a crash
//!   with a stack full of C, and an eval whose callback is *awaited* on the GTK thread deadlocks
//!   the window, because the callback is delivered by that thread's own main loop.
//! * **The app's own page must be unharmed.** The box is moved, never rebuilt, so the app's webview
//!   keeps its widget, its focus and its allocation.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gtk::prelude::*;
use tauri::Manager;

/// Where the page sits in the window, in the same pixels the panel reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn usable(&self) -> bool {
        self.w > 0 && self.h > 0
    }
}

/// The overlay and the fixed over the app's window, plus the page that lives in the fixed.
/// This whole struct is GTK-thread-only; it never leaves this module.
struct Host {
    overlay: gtk::Overlay,
    fixed: gtk::Fixed,
    page: Option<wry::WebView>,
    rect: Rect,
}

/// The one host, reachable from any thread. The `unsafe` below is the whole thread story of this
/// module: a `Host` holds GTK widgets, which are `!Send`, and it goes in a static anyway — because
/// every read and write of it happens inside `run_on_main_thread`, which runs its closure on the
/// GTK main thread. That is also what makes touching those widgets sound at all; the lock is there
/// so two queued closures cannot interleave inside one host.
#[derive(Clone)]
struct HostSlot(Arc<Mutex<Option<Host>>>);
unsafe impl Send for HostSlot {}
unsafe impl Sync for HostSlot {}

impl HostSlot {
    /// The host, if it has been built. An inherent method on purpose: it is the only way in, and it
    /// hands back the poisoned-lock message this module's errors use.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Host>>, String> {
        self.0
            .lock()
            .map_err(|_| "the page host lock is poisoned".to_string())
    }
}

static HOST: OnceLock<HostSlot> = OnceLock::new();

fn host_slot() -> HostSlot {
    HOST.get_or_init(|| HostSlot(Arc::new(Mutex::new(None)))).clone()
}

/// The child box wry created for the window, or an error saying what was there instead.
fn existing_box(window: &gtk::ApplicationWindow) -> Result<gtk::Box, String> {
    let child = window
        .child()
        .ok_or("the app window has no child widget to put a browser over")?;
    child.dynamic_cast::<gtk::Box>().map_err(|_| {
        "the app window's child is not the box cwoffee expects, so its layout was not touched"
            .to_string()
    })
}

/// Build (once) the overlay + fixed over the window. GTK thread only.
fn build_host(window: &gtk::ApplicationWindow) -> Result<(), String> {
    let slot = host_slot();
    if slot.lock().map_err(|_| "the page host lock is poisoned".to_string())?.is_some() {
        return Ok(());
    }
    let vbox = existing_box(window)?;
    // Move the box, never rebuild it: the app's own webview keeps its widget and its allocation.
    window.remove(&vbox);
    let overlay = gtk::Overlay::new();
    overlay.add(&vbox);
    let fixed = gtk::Fixed::new();
    // Fill: the overlay allocates its whole area to the fixed, and the page inside it is placed by
    // coordinates — the entire reason for going through `gtk::Fixed`.
    fixed.set_halign(gtk::Align::Fill);
    fixed.set_valign(gtk::Align::Fill);
    overlay.add_overlay(&fixed);
    // Input belongs to the app unless the page is under the pointer. An overlay child is *above* the
    // main child in GTK's hit-testing, and this one covers the whole window — so every click in the
    // window was being handed to the `Fixed`, which has no window and no handler and simply dropped
    // it. The keyboard kept working, which is what made it look like a stuck app rather than a stolen
    // click: Tab goes to a widget, a pointer goes to whichever allocation is on top.
    //
    // Passing through is safe for the page itself: a WebKit surface has its own window, so clicks
    // inside the page's rectangle still arrive at the page. What stops working is the area *around*
    // it — the transcript, the composer, the sidebar — which is the app's, and always was.
    overlay.set_overlay_pass_through(&fixed, true);
    fixed.set_can_focus(false);
    window.add(&overlay);
    overlay.show_all();
    fixed.show();
    let mut guard = slot.lock().map_err(|_| "the page host lock is poisoned".to_string())?;
    *guard = Some(Host {
        overlay,
        fixed,
        page: None,
        rect: Rect::default(),
    });
    Ok(())
}

/// Run `f` with the host on the GTK thread. The closure must not capture a GTK object from
/// elsewhere — it gets `&mut Host` and that is the only handle there is.
fn on_gtk<R: tauri::Runtime, F>(app: &tauri::AppHandle<R>, f: F) -> Result<(), String>
where
    F: FnOnce(&mut Host) + Send + 'static,
{
    let Some(window) = app.get_webview_window("main") else {
        return Err("there is no app window to put a page in".to_string());
    };
    let on_gtk_thread = window.clone();
    window
        .run_on_main_thread(move || {
            let gtk_window = match on_gtk_thread.gtk_window() {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("[cwoffee/browser] no GTK window to embed a page in: {e}");
                    return;
                }
            };
            if let Err(e) = build_host(&gtk_window) {
                eprintln!("[cwoffee/browser] the page host could not be built: {e}");
                return;
            }
            let slot = host_slot();
            let Ok(mut guard) = slot.lock() else {
                eprintln!("[cwoffee/browser] the page host lock was poisoned");
                return;
            };
            if let Some(host) = guard.as_mut() {
                f(host);
            }
        })
        .map_err(|e| format!("the GTK thread never took the page work: {e}"))
}

/// Send `f` to the GTK thread and wait for what it answers.
///
/// The wait is only for the *work* to be done — never for a page to answer. A caller that blocks
/// the GTK thread waiting for a JavaScript result deadlocks the whole window.
fn ask<R: tauri::Runtime, F, T>(app: &tauri::AppHandle<R>, f: F, budget: Duration) -> Result<T, String>
where
    F: FnOnce(&mut Host) -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    on_gtk(app, move |host| {
        let _ = tx.send(f(host));
    })?;
    rx.recv_timeout(budget)
        .map_err(|_| "the window never answered about its page".to_string())
}

/// The page's view is on the GTK thread; this hands back a receiver for the answer so the caller
/// can wait for it *off* that thread.
pub fn eval<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    script: &str,
) -> Result<Receiver<String>, String> {
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let js = script.to_string();
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    ask_async(app, move |host| {
        let Some(view) = host.page.as_ref() else {
            let _ = tx.send(Err("there is no page open".to_string()));
            return;
        };
        match view.evaluate_script_with_callback(&js, move |value| {
            let _ = out_tx.send(value);
        }) {
            Ok(()) => {
                let _ = tx.send(Ok(()));
            }
            Err(e) => {
                let _ = tx.send(Err(format!("the page would not take the step: {e}")));
            }
        }
    })?;
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(|_| "the window never took the step".to_string())??;
    Ok(out_rx)
}

/// Same as [`ask`] but without waiting for the answer — for operations whose result the caller
/// does not need in order to answer somebody.
fn ask_async<R: tauri::Runtime, F>(app: &tauri::AppHandle<R>, f: F) -> Result<(), String>
where
    F: FnOnce(&mut Host) + Send + 'static,
{
    on_gtk(app, f)
}

/// Ask the app which folder a local page may be opened from, **now**.
///
/// A function rather than a `PathBuf` because the answer moves: the person switches conversation, or
/// moves the conversation to another folder, while this window — and the page in it — stays open. A
/// guard holding the folder the app booted with refuses a page in the folder they are standing in,
/// and calls it an escape. One question, asked of the engine that keeps the answer.
pub type WsOf = Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// Open a page in the panel's box. `emit` is how the page's own world (a download, a refused
/// navigation) tells the browser hub about itself, from whichever thread WebKit calls us on.
pub fn open<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
    rect: Rect,
    init_script: &str,
    emit: tokio::sync::mpsc::UnboundedSender<String>,
    ws_of: WsOf,
) -> Result<(), String> {
    let url = url.to_string();
    let init = init_script.to_string();
    ask(
        app,
        move |host: &mut Host| {
            // Another address in the page that is already up. This is the branch that keeps the
            // browser honest: the session history Back walks, the scroll position, the half-filled
            // form and the cookie jar all live in that one widget, and rebuilding it would throw
            // them away while telling the person nothing happened.
            if let Some(view) = host.page.as_ref() {
                let _ = view.load_url(&url);
                host.rect = rect;
                let _ = view.set_bounds(wry::Rect {
                    position: dpi::LogicalPosition::new(rect.x as f64, rect.y as f64).into(),
                    size: dpi::LogicalSize::new(rect.w.max(1) as f64, rect.h.max(1) as f64).into(),
                });
                let _ = view.set_visible(rect.usable());
                report(&host.fixed, rect);
                return;
            }
            // One page at a time: a second page in the same fixed would be a second document the
            // person cannot see and the model cannot tell apart from the first.
            host.page = None;
            host.rect = rect;
            let (x, y, w, h) = (
                rect.x as f64,
                rect.y as f64,
                rect.w.max(1) as f64,
                rect.h.max(1) as f64,
            );
            let emit_dl = emit.clone();
            let emit_nav = emit.clone();
            let emit_win = emit.clone();
            // Each handler gets its own handle on the same live answer.
            let ws_nav = ws_of.clone();
            let ws_dl = ws_of.clone();
            let builder = wry::WebViewBuilder::new()
                .with_url(&url)
                .with_initialization_script(&init)
                .with_bounds(wry::Rect {
                    position: dpi::LogicalPosition::new(x, y).into(),
                    size: dpi::LogicalSize::new(w, h).into(),
                })
                // A page may walk itself out of the workspace with `file:`, whatever it claims.
                .with_navigation_handler(move |target: String| {
                    // Read the folder at navigation time, not at build time: see `WsOf`.
                    confined(&ws_nav, &target, &emit_nav)
                })
                // A download lands in the workspace, where the person can find it and the model
                // can read it, instead of being written to the home folder by a widget nobody asked
                // for. And it is reported, because a download the app swallowed is a page event
                // nobody can see.
                .with_download_started_handler(move |source: String, dest: &mut PathBuf| {
                    // The same folder and the same naming rule as an offer the page makes through
                    // the bridge, whichever way the file arrived. On WebKitGTK 2.40+ this handler is
                    // the quiet path - the signal behind it no longer fires for an ordinary link - so
                    // what it does has to match what the loud path does rather than drift from it.
                    let name = cwoffee_engine::browser_hub::downloads::name_for(&source);
                    let dir = cwoffee_engine::browser_hub::downloads::dir_for(&(ws_dl)());
                    let _ = std::fs::create_dir_all(&dir);
                    *dest = dir.join(&name);
                    let _ = emit_dl.send(format!(
                        "{}",
                        serde_json::json!({"ev": "diag", "diag": {
                            "t": now_ms(), "kind": "download", "level": "info",
                            "text": format!("saved to {}", dest.display()),
                            "url": source
                        }})
                    ));
                    true
                })
                // `window.open` and `target=_blank` do not get an invisible second webview. The
                // requested page becomes the page, so the person and the model keep looking at one
                // document.
                .with_new_window_req_handler(move |target: String, _features: wry::NewWindowFeatures| {
                    // The bridge rewrites `window.open` and `target=_blank` onto the same page, so
                    // this only fires for something that got past the page's own code. Either way
                    // the answer is the same: one page, and the person is told what was refused.
                    let _ = emit_win.send(format!(
                        "{}",
                        serde_json::json!({"ev": "diag", "diag": {
                            "t": now_ms(), "kind": "navigation", "level": "warning",
                            "text": "the page asked for a second window; this browser keeps one page, and the request was refused",
                            "url": target
                        }})
                    ));
                    wry::NewWindowResponse::Deny
                });
            match wry::WebViewBuilderExtUnix::build_gtk(builder, &host.fixed) {
                Ok(view) => {
                    // A widget that was never shown has no allocation, and WebKit renders a 1x1
                    // page at no allocation — which reads as "the page will not answer". A page
                    // with no box to show itself in is kept rather than displayed: the hub may have
                    // opened it while the panel was closed, and it gets placed when the panel
                    // reports a box.
                    let _ = view.set_visible(rect.usable());
                    host.page = Some(view);
                    raise_page(&host.fixed);
                }
                Err(e) => eprintln!("[cwoffee/browser] the page could not be built: {e}"),
            }
        },
        Duration::from_secs(5),
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Whether a navigation may happen. http(s) is the world; `file:` is only the workspace.
fn confined(ws_of: &WsOf, target: &str, emit: &tokio::sync::mpsc::UnboundedSender<String>) -> bool {
    // Asked now, not remembered from whenever this page was built.
    let ws = ws_of();
    let ws: &std::path::Path = &ws;
    let lower = target.to_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || target == "about:blank" {
        return true;
    }
    if lower.starts_with("file:") {
        let Some(path) = file_to_path(target) else {
            return false;
        };
        let inside = path.starts_with(ws);
        if !inside {
            let _ = emit.send(format!(
                "{}",
                serde_json::json!({"ev": "diag", "diag": {
                    "t": now_ms(), "kind": "navigation", "level": "warning",
                    "text": format!(
                        "a link out of the working folder ({}); this box opens pages inside {} only; the page stayed where it was",
                        path.display(),
                        ws.display()
                    ),
                    "url": target
                }})
            ));
        }
        inside
    } else {
        // data:, javascript:, blob: from the page itself are not pages in the sense this app means.
        false
    }
}

fn file_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let rest = rest.split('?').next().unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Move and resize the page. The panel's box is the authority on both.
pub fn place<R: tauri::Runtime>(app: &tauri::AppHandle<R>, rect: Rect) -> Result<Rect, String> {
    ask(
        app,
        move |host: &mut Host| {
            host.rect = rect;
            if let Some(view) = host.page.as_ref() {
                let _ = view.set_bounds(wry::Rect {
                    position: dpi::LogicalPosition::new(rect.x as f64, rect.y as f64).into(),
                    size: dpi::LogicalSize::new(rect.w.max(1) as f64, rect.h.max(1) as f64).into(),
                });
                let _ = view.set_visible(rect.usable());
            }
            // What the widget tree actually gave the page. A container is free to hand a child more
            // than it asked for, and WebKit renders whatever it is given — so without this line a
            // wrong rectangle is a page in the wrong place with nothing said about it.
            raise_page(&host.fixed);
            report(&host.fixed, rect);
            // The answer is the widget's own box, not the one that was asked for. A page placed at
            // a size nobody asked for is otherwise invisible from the outside: the panel would keep
            // reporting the numbers it sent in, which is how a panel can say "the page is here"
            // while the page is nowhere.
            given(&host.fixed).unwrap_or(rect)
        },
        Duration::from_secs(3),
    )
}

/// Where the page widget really is, in the window's own pixels, and whether it is shown.
///
/// This is the widget's allocation, not the rectangle somebody requested: GTK hands a child what it
/// likes, a hidden widget keeps no box, and a page that was never given an area paints nothing.
pub fn placed<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Option<(Rect, bool)>, String> {
    ask(
        app,
        |host: &mut Host| match host.page.as_ref() {
            None => None,
            Some(_) => {
                let box_ = given(&host.fixed)?;
                let shown = host
                    .fixed
                    .children()
                    .first()
                    .map(|k| k.is_visible())
                    .unwrap_or(false);
                Some((box_, shown))
            }
        },
        Duration::from_secs(3),
    )
}

/// Lift the page above the app's own page.
///
/// Both are WebKit surfaces, and a surface paints what it likes: GTK's paint order puts the fixed
/// over the app's box, but the two X windows underneath are stacked by whoever mapped last, which
/// after a stream, a resize or a re-open is not reliably the page. A page that is placed exactly
/// where it was asked for and still paints nothing is the worst kind of bug — every number is right
/// and the person sees an empty frame — so the lift is repeated on every placement.
fn raise_page(fixed: &gtk::Fixed) {
    for kid in fixed.children() {
        if let Some(win) = kid.window() {
            let _ = win.raise();
        }
    }
    let _ = fixed.window().map(|w| w.raise());
}

/// The first child of the fixed, as the widget tree sees it: its allocation, moved into the
/// window's own coordinates. `None` when there is no child at all, which is the case a panel needs
/// to hear about — a page that was opened and then never given a place in the widget tree.
fn given(fixed: &gtk::Fixed) -> Option<Rect> {
    let kid = fixed.children().first()?.allocation();
    let fx = fixed.allocation();
    Some(Rect {
        x: (fx.x() + kid.x()).max(0) as u32,
        y: (fx.y() + kid.y()).max(0) as u32,
        w: kid.width().max(0) as u32,
        h: kid.height().max(0) as u32,
    })
}

fn report(fixed: &gtk::Fixed, rect: Rect) {
    let alloc = fixed.allocation();
    let kids = fixed.children();
    eprintln!(
        "[cwoffee/browser] the panel asked for {}x{} at {},{}; the fixed is {}x{} at {},{} and holds {} child widget(s)",
        rect.w, rect.h, rect.x, rect.y, alloc.width(), alloc.height(), alloc.x(), alloc.y(), kids.len()
    );
    for kid in &kids {
        let a = kid.allocation();
        let (min, nat) = kid.preferred_size();
        eprintln!(
            "[cwoffee/browser]   {} allocated {}x{} at {},{} (minimum {}x{}, natural {}x{}, visible={})",
            kid.type_().name(),
            a.width(),
            a.height(),
            a.x(),
            a.y(),
            min.width,
            min.height,
            nat.width,
            nat.height,
            kid.is_visible()
        );
    }
    if kids.is_empty() {
        eprintln!("[cwoffee/browser] there is no page widget in the fixed at all");
    }
}

/// Show or hide the page. A page the panel is not showing is hidden rather than left floating over
/// the chat — a widget that is not visible takes no input and no paint.
pub fn visible<R: tauri::Runtime>(app: &tauri::AppHandle<R>, on: bool) -> Result<(), String> {
    ask(
        app,
        move |host: &mut Host| {
            if let Some(view) = host.page.as_ref() {
                let _ = view.set_visible(on);
            }
        },
        Duration::from_secs(3),
    )
}

/// Put the page away. Destroying a widget belongs on the thread that made it, so the view is taken
/// out of the host there and dropped there.
pub fn close<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<(), String> {
    ask(
        app,
        move |host: &mut Host| {
            host.page = None;
        },
        Duration::from_secs(3),
    )
}

/// Whether a page is open, and where.
pub fn status<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<(bool, Rect), String> {
    ask(
        app,
        |host: &mut Host| (host.page.is_some(), host.rect),
        Duration::from_secs(3),
    )
}

/// The window's own area, which is the largest box a page can be placed in.
pub fn area<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<(u32, u32), String> {
    ask(
        app,
        |host: &mut Host| {
            let alloc = host.fixed.allocation();
            (
                alloc.width().max(0) as u32,
                alloc.height().max(0) as u32,
            )
        },
        Duration::from_secs(3),
    )
}

#[cfg(test)]
mod tests {
    use super::confined;
    use super::WsOf;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// The page box's guard used to hold the folder the app **booted** with. The person switches the
    /// conversation to another folder and the window stays open, so the next local page they open —
    /// one sitting in the folder they are actually standing in — was refused as an escape. That is
    /// the bug this grades: the answer has to be asked at navigation time.
    #[test]
    fn a_local_page_is_confined_to_the_folder_the_conversation_is_in_now() {
        let folder = Arc::new(Mutex::new(PathBuf::from("/home/w/first")));
        let ws_of: WsOf = {
            let folder = Arc::clone(&folder);
            Arc::new(move || folder.lock().unwrap().clone())
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(
            confined(&ws_of, "file:///home/w/first/page.html", &tx),
            "a page in the folder the conversation is in opens"
        );
        assert!(
            !confined(&ws_of, "file:///home/w/other/page.html", &tx),
            "a page outside it does not"
        );

        // The person moves the conversation. The same page box, unbuilt and unmoved, follows.
        *folder.lock().unwrap() = PathBuf::from("/home/w/second");
        assert!(
            confined(&ws_of, "file:///home/w/second/page.html", &tx),
            "the new folder is the working folder, not the one the app started in"
        );
        assert!(
            !confined(&ws_of, "file:///home/w/first/page.html", &tx),
            "and the folder it left is now outside"
        );
    }

    /// A scheme that is not a page is refused whatever the folder, and the refusal is silent
    /// because there is nothing for the person to act on.
    #[test]
    fn a_page_cannot_walk_itself_into_a_scheme_that_is_not_a_page() {
        let ws_of: WsOf = Arc::new(|| PathBuf::from("/home/w"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for scheme in ["javascript:alert(1)", "data:text/html,hi", "blob:whatever"] {
            assert!(!confined(&ws_of, scheme, &tx), "{scheme} is not a page to open here");
        }
        assert!(
            rx.try_recv().is_err(),
            "a refused scheme says nothing about the working folder: there is no folder involved"
        );
    }
}
