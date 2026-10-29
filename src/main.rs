//! A tiny always-on-top, frameless, draggable widget that shows Copilot,
//! Claude and Codex usage (dollars on business plans, 5-hour and weekly windows
//! on personal ones), refreshed every few minutes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod limits;
#[cfg(target_os = "macos")]
mod menubar;
mod providers;
mod timeutil;
mod update;

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, PointerButton, Pos2, RichText, Sense,
    Shape, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use providers::{FetchError, Meter, Problem, Provider, Unit, Usage, money};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use timeutil::{ago, now_unix, until, until_next_minute};

const WIDTH: f32 = 250.0;
const MARGIN: i8 = 12;
const DEFAULT_REFRESH_MINS: u64 = 5;
const DEFAULT_OPACITY_PERCENT: u32 = 85;
const MINI_MARGIN_X: i8 = 10;
const MINI_MARGIN_Y: i8 = 6;
const MINIMIZED_KEY: &str = "minimized";
const RATIOS_KEY: &str = "ratios";
const HIDDEN_KEY: &str = "hidden";
/// Seconds the bar and window labels take to move over when the binding window changes.
const SWITCH_SECS: f32 = 0.35;
/// How far the window that is not the binding one fades towards the background.
const FADE: f32 = 0.65;
/// Size presets in the right-click menu, applied as egui's zoom factor on top of
/// the monitor's display scaling. Ctrl +/- also works; either way it persists.
const SIZES: [f32; 8] = [0.25, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0];

// The window is opaque and painted entirely in BG; Windows rounds the corners
// at the compositor level (see `apply_windows_chrome`), so nothing else shows.
const BG: Color32 = Color32::from_rgb(24, 26, 32);
const TEXT: Color32 = Color32::from_gray(232);
const MUTED: Color32 = Color32::from_gray(135);
/// Window labels ("5h", "wk") next to a personal plan's percentages.
const LIGHT: Color32 = Color32::from_gray(175);
const TRACK: Color32 = Color32::from_gray(58);
const ERR: Color32 = Color32::from_rgb(235, 110, 110);

struct Update {
    provider: Provider,
    result: Result<Usage, FetchError>,
    at: i64,
}

#[derive(Clone, Default)]
struct Slot {
    usage: Option<Usage>,
    error: Option<FetchError>,
    updated: Option<i64>,
    loading: bool,
}

impl Slot {
    /// A service with no paid plan on this machine is left out. Others are
    /// hidden only until their first fetch answers, so a service the user does
    /// not have never flashes up.
    fn visible(&self) -> bool {
        let absent = self
            .error
            .as_ref()
            .is_some_and(|e| e.problem == Problem::NotAvailable);
        !absent && (self.usage.is_some() || self.error.is_some())
    }
}

struct App {
    slots: HashMap<Provider, Slot>,
    rx: Receiver<Update>,
    refresh_tx: Sender<()>,
    interval: Duration,
    /// False until the window has been placed on screen and made topmost (see `settle_window`).
    settled: bool,
    /// Compact single-line view. Persisted, so the widget reopens the way it was left.
    minimized: bool,
    logos: HashMap<Provider, egui::TextureHandle>,
    /// Learnt weekly cost of the 5-hour window per service name, for personal
    /// plans. Persisted, since it takes a while to learn.
    ratios: HashMap<String, limits::Ratio>,
    /// Names of services the user has unticked in the menu. They are still
    /// fetched, so ticking one again shows it straight away. Persisted.
    hidden: HashSet<String>,
    /// Set once an update has been installed; the UI thread then restarts.
    restart: Arc<AtomicBool>,
    /// Services whose token is being refreshed, so a second click does not
    /// start another run.
    refreshing: Arc<Mutex<HashSet<Provider>>>,
    #[cfg(target_os = "macos")]
    menubar: Option<menubar::MenuBar>,
    /// Whether the window is on screen; the menu bar icon can hide it.
    #[cfg(target_os = "macos")]
    shown: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let interval = Duration::from_secs(60 * refresh_minutes());
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        spawn_worker(cc.egui_ctx.clone(), tx, refresh_rx, interval);
        let restart = Arc::new(AtomicBool::new(false));
        if update::auto_enabled() {
            spawn_auto_update(cc.egui_ctx.clone(), restart.clone());
        }
        // Selectable labels grab clicks and drags, so right-click and drag-to-move
        // would not work over text.
        cc.egui_ctx
            .all_styles_mut(|style| style.interaction.selectable_labels = false);
        let minimized = cc
            .storage
            .and_then(|s| eframe::get_value(s, MINIMIZED_KEY))
            .unwrap_or(false);
        let ratios = cc
            .storage
            .and_then(|s| eframe::get_value(s, RATIOS_KEY))
            .unwrap_or_default();
        let hidden = cc
            .storage
            .and_then(|s| eframe::get_value(s, HIDDEN_KEY))
            .unwrap_or_default();

        let mut slots = HashMap::new();
        for p in Provider::ALL {
            slots.insert(
                p,
                Slot {
                    loading: true,
                    ..Default::default()
                },
            );
        }
        #[cfg(target_os = "macos")]
        let menubar = match menubar::MenuBar::new(&cc.egui_ctx, &Provider::ALL) {
            Ok(bar) => Some(bar),
            Err(e) => {
                eprintln!("menu bar icon: {e}");
                None
            }
        };
        Self {
            slots,
            rx,
            refresh_tx,
            interval,
            settled: false,
            minimized,
            logos: load_logos(&cc.egui_ctx),
            ratios,
            hidden,
            restart,
            refreshing: Arc::default(),
            #[cfg(target_os = "macos")]
            menubar,
            #[cfg(target_os = "macos")]
            shown: true,
        }
    }

    /// Menu bar clicks. Called from `logic`, which runs before every `ui` pass
    /// and also while the widget is hidden, when eframe runs no egui pass at
    /// all, so handling these in `ui` would leave a hidden widget with no way
    /// back.
    #[cfg(target_os = "macos")]
    fn handle_menu(&mut self, ctx: &egui::Context) {
        let Some(actions) = self.menubar.as_ref().map(menubar::MenuBar::take_actions) else {
            return;
        };
        for action in actions {
            match action {
                menubar::Action::ToggleWidget => {
                    self.shown = !self.shown;
                    ctx.send_viewport_cmd(ViewportCommand::Visible(self.shown));
                    if self.shown {
                        self.settled = false;
                    }
                    if let Some(bar) = &self.menubar {
                        bar.set_widget_shown(self.shown);
                    }
                }
                menubar::Action::Refresh => self.refresh_now(),
                menubar::Action::Open(p) => open_url(p.url()),
                menubar::Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
                menubar::Action::Widget(action) => self.apply_action(action, ctx),
            }
        }
    }

    /// A choice from the right-click menu.
    fn apply_action(&mut self, action: MenuAction, ctx: &egui::Context) {
        match action {
            MenuAction::Refresh => self.refresh_now(),
            MenuAction::RefreshToken(p) => self.refresh_token(p),
            MenuAction::ToggleMinimized => self.minimized = !self.minimized,
            MenuAction::Open(p) => open_url(p.url()),
            MenuAction::Toggle(p) => self.toggle(p),
            MenuAction::Size(z) => ctx.set_zoom_factor(z),
            MenuAction::CheckForUpdates => self.check_for_updates(ctx),
            MenuAction::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    /// Runs a check from the menu in the background and reports the result.
    fn check_for_updates(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let restart = self.restart.clone();
        std::thread::spawn(move || match update::check_and_install() {
            Ok(update::Outcome::UpToDate) => message_box(
                &format!("You have the latest version ({}).", update::CURRENT),
                false,
            ),
            Ok(update::Outcome::NotReady(v)) => message_box(
                &format!(
                    "Version {v} is out, but its download is not ready yet. Try again in a few minutes."
                ),
                false,
            ),
            Ok(update::Outcome::Installed) => {
                restart.store(true, Ordering::SeqCst);
                ctx.request_repaint();
            }
            Err(e) => message_box(&format!("Could not check for updates: {e}"), true),
        });
    }

    fn refresh_now(&mut self) {
        for slot in self.slots.values_mut() {
            slot.loading = true;
        }
        let _ = self.refresh_tx.send(());
    }

    /// Has the service's CLI refresh its token in the background, then refreshes
    /// every service, reporting a failed refresh.
    fn refresh_token(&mut self, p: Provider) {
        let refreshing = self.refreshing.clone();
        if !refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(p)
        {
            return;
        }
        for slot in self.slots.values_mut() {
            slot.loading = true;
        }
        let refresh_tx = self.refresh_tx.clone();
        std::thread::spawn(move || {
            let result = p.refresh_token();
            refreshing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&p);
            let _ = refresh_tx.send(());
            if let Err(e) = result {
                message_box(
                    &format!("Could not refresh the {} token: {e}", p.name()),
                    true,
                );
            }
        });
    }

    fn drain(&mut self) {
        while let Ok(u) = self.rx.try_recv() {
            let slot = self.slots.entry(u.provider).or_default();
            slot.loading = false;
            slot.updated = Some(u.at);
            match u.result {
                Ok(usage) => {
                    // Personal plans learn how their two windows relate.
                    if let Usage {
                        meters,
                        personal: true,
                    } = &usage
                        && let [five_hour, week] = meters.as_slice()
                    {
                        self.ratios
                            .entry(u.provider.name().to_string())
                            .or_default()
                            .observe(five_hour, week);
                    }
                    slot.usage = Some(usage);
                    slot.error = None;
                }
                Err(e) => {
                    slot.usage = None;
                    slot.error = Some(e);
                }
            }
        }
    }

    /// For a personal plan with 5-hour and weekly windows, the one that runs out
    /// first this session (0 or 1).
    fn binding(&self, p: Provider) -> Option<usize> {
        let usage = self.slots.get(&p)?.usage.as_ref().filter(|u| u.personal)?;
        let [five_hour, week] = usage.meters.as_slice() else {
            return None;
        };
        let k = self
            .ratios
            .get(p.name())
            .and_then(limits::Ratio::weekly_per_five_hour);
        Some(limits::binding(five_hour, week, k))
    }

    /// The services the account has, in display order. These are listed in the
    /// menu, ticked or not.
    fn found(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|p| self.slots.get(p).is_some_and(Slot::visible))
            .collect()
    }

    /// The found services the user has not unticked.
    fn shown(&self) -> Vec<Provider> {
        let mut found = self.found();
        found.retain(|p| !self.hidden.contains(p.name()));
        found
    }

    /// What the widget says when it has no service to show, in the full and the
    /// minimized view.
    fn empty_text(&self, found: &[Provider]) -> (&'static str, &'static str) {
        if !found.is_empty() {
            (
                "All services are hidden. Right-click to show them.",
                "All hidden",
            )
        } else if self.all_fetched() {
            (
                "No paid Copilot, Claude or Codex plan found. Sign in with gh, claude or codex and it shows up at the next refresh.",
                "No plans found",
            )
        } else {
            ("…", "…")
        }
    }

    fn toggle(&mut self, p: Provider) {
        if !self.hidden.remove(p.name()) {
            self.hidden.insert(p.name().to_string());
        }
    }

    /// True once every service has answered at least once.
    fn all_fetched(&self) -> bool {
        self.slots.values().all(|s| s.updated.is_some())
    }

    fn last_updated(&self) -> Option<i64> {
        self.slots.values().filter_map(|s| s.updated).max()
    }

    /// Sum of every dollar-denominated meter across the shown providers. Only when
    /// at least two have one, since otherwise it repeats that provider's line.
    fn total(&self, shown: &[Provider]) -> Option<Meter> {
        let mut used = 0.0;
        let mut total = 0.0;
        let mut providers = 0;
        for slot in shown.iter().filter_map(|p| self.slots.get(p)) {
            let mut any = false;
            for m in slot.usage.iter().flat_map(|u| &u.meters) {
                if m.unit == Unit::Dollars {
                    used += m.used;
                    total += m.total;
                    any = true;
                }
            }
            providers += usize::from(any);
        }
        (providers >= 2).then(|| Meter {
            label: None,
            used,
            total,
            unit: Unit::Dollars,
            resets_at: None,
        })
    }
}

fn refresh_minutes() -> u64 {
    std::env::var("USAGE_WIDGET_REFRESH_MINS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|m| *m >= 1)
        .unwrap_or(DEFAULT_REFRESH_MINS)
}

fn spawn_worker(
    ctx: egui::Context,
    tx: Sender<Update>,
    refresh_rx: Receiver<()>,
    interval: Duration,
) {
    std::thread::spawn(move || {
        loop {
            std::thread::scope(|s| {
                for p in Provider::ALL {
                    let tx = tx.clone();
                    let ctx = ctx.clone();
                    s.spawn(move || {
                        let result = p.fetch();
                        let _ = tx.send(Update {
                            provider: p,
                            result,
                            at: now_unix(),
                        });
                        ctx.request_repaint();
                    });
                }
            });
            match refresh_rx.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while refresh_rx.try_recv().is_ok() {}
        }
    });
}

/// Checks for updates shortly after start and then every few hours, installing
/// any newer release. Failures are ignored and retried at the next check.
fn spawn_auto_update(ctx: egui::Context, restart: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        std::thread::sleep(update::FIRST_CHECK_AFTER);
        loop {
            if let Ok(update::Outcome::Installed) = update::check_and_install() {
                restart.store(true, Ordering::SeqCst);
                ctx.request_repaint();
                return;
            }
            std::thread::sleep(update::CHECK_EVERY);
        }
    });
}

#[cfg(windows)]
fn hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win) => Some(win.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32)
    -> i32;
}

#[cfg(windows)]
const HWND_TOPMOST: isize = -1;
#[cfg(windows)]
const SWP_NOSIZE: u32 = 0x1;
#[cfg(windows)]
const SWP_NOMOVE: u32 = 0x2;
#[cfg(windows)]
const SWP_NOACTIVATE: u32 = 0x10;

/// Windows 11: round the window corners and drop the 1px accent border, so the
/// frameless window looks like a floating card without needing transparency.
/// Also puts the window back on top if something has dropped its topmost style.
#[cfg(windows)]
fn apply_windows_chrome(frame: &eframe::Frame) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWCP_ROUND: u32 = 2;
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOPMOST: isize = 0x8;

    let Some(hwnd) = hwnd(frame) else {
        return;
    };
    set_opacity(hwnd);
    unsafe {
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST == 0 {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&DWMWCP_ROUND as *const u32).cast(),
            4,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&DWMWA_COLOR_NONE as *const u32).cast(),
            4,
        );
        // winit keeps a 1px strip of caption at the top of undecorated windows (for
        // the drop shadow); paint it the background colour so it does not show as a
        // light line. COLORREF is 0x00BBGGRR.
        let caption = u32::from_le_bytes([BG.r(), BG.g(), BG.b(), 0]);
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            (&caption as *const u32).cast(),
            4,
        );
    }
}

/// macOS: the same whole-window opacity as on Windows, through the NSWindow's
/// alpha.
#[cfg(target_os = "macos")]
fn apply_windows_chrome(frame: &eframe::Frame) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let view = appkit.ns_view.as_ptr().cast::<AnyObject>();
    let alpha = f64::from(opacity_percent()) / 100.0;
    // SAFETY: `ns_view` is the live NSView eframe draws into, and `ui` runs on
    // the main thread.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if !window.is_null() {
            let _: () = msg_send![window, setAlphaValue: alpha];
        }
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn apply_windows_chrome(_frame: &eframe::Frame) {}

/// Runs when the window is first visible and after each self-resize. Pulls it
/// fully onto the nearest monitor's work area, since the saved position can point
/// at a monitor that is no longer there (e.g. after hotdesking), and forces it to
/// the top of the z-order: winit creates it hidden and the topmost level otherwise
/// does not take effect until the window is first activated. Returns false until
/// the window is visible.
#[cfg(windows)]
fn settle_window(frame: &eframe::Frame) -> bool {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn MonitorFromRect(rect: *const Rect, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
    }
    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    let Some(hwnd) = hwnd(frame) else {
        return true;
    };
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return false;
        }
        watch_taskbar(hwnd);
        let mut r = Rect::default();
        let mut info = MonitorInfo {
            size: size_of::<MonitorInfo>() as u32,
            ..Default::default()
        };
        let mut flags = SWP_NOSIZE | SWP_NOACTIVATE;
        let (mut x, mut y) = (r.left, r.top);
        if GetWindowRect(hwnd, &mut r) != 0
            && GetMonitorInfoW(MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST), &mut info) != 0
        {
            let work = info.work;
            x = r.left.min(work.right - (r.right - r.left)).max(work.left);
            y = r.top.min(work.bottom - (r.bottom - r.top)).max(work.top);
        } else {
            flags |= SWP_NOMOVE;
        }
        if (x, y) == (r.left, r.top) {
            flags |= SWP_NOMOVE;
        }
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, flags);
    }
    true
}

/// The taskbar is topmost too, and whenever it is activated Windows raises it
/// above every other topmost window, hiding the widget if it sits on the taskbar.
/// The taskbar also raises itself in other ways (the Start menu opening or
/// closing, for one), so rather than chasing each cause this checks every
/// `CHECK_MS` whether any taskbar is above the widget and, if so, raises the
/// widget again. A foreground hook does the same immediately for taskbar clicks.
/// The same timer restores the widget if Windows minimizes it (Win+M, for one):
/// with no taskbar button it would otherwise stay hidden until relaunched.
/// Only the first call installs the timer and hook.
#[cfg(windows)]
fn watch_taskbar(hwnd: isize) {
    use std::sync::atomic::{AtomicIsize, Ordering};

    type WinEventProc = unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32);
    type TimerProc = unsafe extern "system" fn(isize, u32, usize, u32);
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWinEventHook(
            min: u32,
            max: u32,
            module: isize,
            proc: WinEventProc,
            pid: u32,
            tid: u32,
            flags: u32,
        ) -> isize;
        fn GetClassNameW(hwnd: isize, name: *mut u16, len: i32) -> i32;
        fn SetTimer(hwnd: isize, id: usize, ms: u32, proc: TimerProc) -> usize;
        fn GetWindow(hwnd: isize, cmd: u32) -> isize;
        fn IsIconic(hwnd: isize) -> i32;
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    }
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x3;
    const WINEVENT_OUTOFCONTEXT: u32 = 0x0;
    const GW_HWNDPREV: u32 = 3;
    const SW_SHOWNOACTIVATE: i32 = 4;
    /// How often to check that the widget is not minimized or below a taskbar.
    const CHECK_MS: u32 = 500;

    static WIDGET: AtomicIsize = AtomicIsize::new(0);

    fn is_taskbar(hwnd: isize) -> bool {
        let mut buf = [0u16; 32];
        let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        // The primary monitor's taskbar, and the ones on other monitors.
        class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd"
    }

    /// True if a taskbar is above the widget in the z-order. Only the few
    /// windows above it are walked, since it sits near the top.
    fn taskbar_above() -> bool {
        let mut h = WIDGET.load(Ordering::Relaxed);
        loop {
            h = unsafe { GetWindow(h, GW_HWNDPREV) };
            if h == 0 {
                return false;
            }
            if is_taskbar(h) {
                return true;
            }
        }
    }

    fn raise() {
        unsafe {
            SetWindowPos(
                WIDGET.load(Ordering::Relaxed),
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    unsafe extern "system" fn on_timer(_hwnd: isize, _msg: u32, _id: usize, _time: u32) {
        let widget = WIDGET.load(Ordering::Relaxed);
        if unsafe { IsIconic(widget) } != 0 {
            unsafe { ShowWindow(widget, SW_SHOWNOACTIVATE) };
            raise();
        } else if taskbar_above() {
            raise();
        }
    }

    unsafe extern "system" fn on_foreground(
        _hook: isize,
        _event: u32,
        foreground: isize,
        _object: i32,
        _child: i32,
        _thread: u32,
        _time: u32,
    ) {
        if is_taskbar(foreground) {
            raise();
        }
    }

    if WIDGET.swap(hwnd, Ordering::Relaxed) != 0 {
        return;
    }
    // Out-of-context hooks and thread timers are delivered through this (the UI)
    // thread's message loop. The hook reacts to taskbar clicks straight away; the
    // timer catches the taskbar raising itself without becoming the foreground
    // window, as it does when the Start menu opens or closes.
    unsafe {
        SetTimer(0, 0, CHECK_MS, on_timer);
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            0,
            on_foreground,
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
    }
}

#[cfg(not(windows))]
fn settle_window(_frame: &eframe::Frame) -> bool {
    true
}

fn level_color(percent: f64) -> Color32 {
    if percent < 60.0 {
        Color32::from_rgb(82, 190, 128)
    } else if percent < 85.0 {
        Color32::from_rgb(232, 175, 64)
    } else {
        Color32::from_rgb(230, 88, 88)
    }
}

/// `a` moved towards `b` by `t` (0 to 1).
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// `color` at `weight` (0 to 1) of full strength: a window that is not the
/// binding one fades towards the background, as if disabled.
fn weighted(color: Color32, weight: f32) -> Color32 {
    mix(mix(color, BG, FADE), color, weight)
}

fn bar(ui: &mut egui::Ui, fraction: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), TRACK);
    if fraction > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * fraction).max(6.0));
        painter.rect_filled(fill, CornerRadius::same(3), color);
    }
}

/// "$523.54 / $1,000   52%" right-aligned, coloured by level.
fn amounts(ui: &mut egui::Ui, m: &Meter, size: f32) {
    let pct = m.percent();
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if m.total > 0.0 {
            ui.label(
                RichText::new(format!("{pct:.0}%"))
                    .size(size)
                    .strong()
                    .color(level_color(pct)),
            );
            if m.unit != Unit::Percent {
                ui.label(RichText::new(m.summary()).size(size).color(TEXT));
            }
        } else {
            ui.label(RichText::new("unlimited").size(size).color(MUTED));
        }
    });
}

/// "42% 5h | 17% wk" right-aligned: each window's percentage coloured by level,
/// followed by its label, each faded by its weight (see `weighted`).
fn windows(ui: &mut egui::Ui, meters: &[Meter], weights: &[f32], size: f32) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        // Laid out right to left, so the last window goes first.
        for (i, (m, &w)) in meters.iter().zip(weights).rev().enumerate() {
            if i > 0 {
                ui.label(RichText::new("|").size(size).color(MUTED));
            }
            // One group per window, so its tooltip covers the label as well.
            let window = ui.scope(|ui| {
                if let Some(label) = &m.label {
                    ui.label(RichText::new(label).size(size).color(weighted(LIGHT, w)));
                }
                let pct = m.percent();
                ui.label(
                    RichText::new(format!("{pct:.0}%"))
                        .size(size)
                        .strong()
                        .color(weighted(level_color(pct), w)),
                );
            });
            if let Some(text) = reset_tooltip(m) {
                window.response.on_hover_text(text);
            }
        }
    });
}

fn reset_tooltip(m: &Meter) -> Option<String> {
    let at = m.resets_at?;
    if m.label.as_deref() == Some("5h") {
        return Some(format!("Resets in {}", until(at)));
    }
    Some(format!("Resets on {}", timeutil::local_datetime(at)?))
}

/// Eases from the values last shown to `target` over `SWITCH_SECS` whenever the
/// target changes, picking up from mid-way if it changes again. The first
/// target seen for an id is shown as-is.
fn ease<const N: usize>(ui: &egui::Ui, id: egui::Id, target: [f32; N]) -> [f32; N] {
    #[derive(Clone, Copy)]
    struct Tween<const N: usize> {
        from: [f32; N],
        to: [f32; N],
        start: f64,
    }
    let at = |tw: &Tween<N>, now: f64| {
        let t = ((now - tw.start) as f32 / SWITCH_SECS).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        std::array::from_fn(|i| tw.from[i] + (tw.to[i] - tw.from[i]) * t)
    };

    let ctx = ui.ctx();
    let now = ui.input(|i| i.time);
    let mut tw = ctx.data(|d| d.get_temp::<Tween<N>>(id)).unwrap_or(Tween {
        from: target,
        to: target,
        start: now,
    });
    if tw.to != target {
        tw = Tween {
            from: at(&tw, now),
            to: target,
            start: now,
        };
    }
    ctx.data_mut(|d| d.insert_temp(id, tw));
    if now - tw.start < f64::from(SWITCH_SECS) {
        ctx.request_repaint();
    }
    at(&tw, now)
}

fn provider_block(
    ui: &mut egui::Ui,
    p: Provider,
    slot: &Slot,
    logos: &HashMap<Provider, egui::TextureHandle>,
    binding: Option<usize>,
) {
    let usage = slot.usage.as_ref();
    // A personal plan's windows all go on the header line.
    let personal = usage.filter(|u| u.personal).map(|u| u.meters.as_slice());
    let single = usage
        .filter(|u| !u.personal && u.meters.len() == 1 && u.meters[0].label.is_none())
        .map(|u| &u.meters[0]);
    // Where the bar and the bright label sit between the 5-hour and weekly window.
    let position = binding.map(|b| ease(ui, egui::Id::new(("binding", p.name())), [b as f32])[0]);
    let weights: Vec<f32> = match position {
        Some(t) => vec![1.0 - t, t],
        None => vec![1.0; personal.map_or(0, <[Meter]>::len)],
    };
    // The binding window, whose reset is counted down next to the name.
    let selected = personal.and_then(|m| m.get(binding.unwrap_or(0)));

    ui.horizontal(|ui| {
        provider_logo(ui, p, logos);
        let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
        ui.label(name);
        if let Some(m) = selected
            && let Some(at) = m.resets_at
        {
            let text = RichText::new(format!("({})", until(at))).size(11.0);
            let countdown = ui.label(text.color(LIGHT));
            if let Some(text) = reset_tooltip(m) {
                countdown.on_hover_text(text);
            }
        }
        if slot.loading {
            ui.add(egui::Spinner::new().size(10.0).color(MUTED));
        }
        if let Some(meters) = personal {
            windows(ui, meters, &weights, 12.0);
        } else if let Some(m) = single {
            amounts(ui, m, 12.0);
        }
        if let Some(e) = &slot.error {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(e.problem.label()).size(12.0).color(ERR))
                    .on_hover_text(&e.detail);
            });
        }
    });

    if slot.error.is_some() {
        bar(ui, 0.0, MUTED);
        return;
    }

    // A personal plan's bar follows its binding window, sliding from what it
    // showed before to the new window's level when that changes.
    if let Some(m) = selected.filter(|_| binding.is_some()) {
        let c = level_color(m.percent());
        let target = [m.fraction(), c.r().into(), c.g().into(), c.b().into()];
        let [fraction, r, g, b] = ease(ui, egui::Id::new(("bar", p.name())), target);
        bar(ui, fraction, Color32::from_rgb(r as u8, g as u8, b as u8));
        return;
    }
    let header = personal.and_then(<[Meter]>::first).or(single);
    match (header, usage) {
        (Some(m), _) => {
            if m.total > 0.0 {
                bar(ui, m.fraction(), level_color(m.percent()));
            }
        }
        (None, Some(usage)) => {
            for m in &usage.meters {
                ui.horizontal(|ui| {
                    if let Some(label) = &m.label {
                        ui.label(RichText::new(label).size(11.0).color(MUTED));
                    }
                    amounts(ui, m, 12.0);
                });
                if m.total > 0.0 {
                    bar(ui, m.fraction(), level_color(m.percent()));
                }
            }
        }
        (None, None) => {
            ui.label(RichText::new("loading…").size(10.5).color(MUTED));
        }
    }
}

#[derive(Clone, Copy)]
enum MenuAction {
    Refresh,
    RefreshToken(Provider),
    ToggleMinimized,
    Open(Provider),
    Toggle(Provider),
    Size(f32),
    CheckForUpdates,
    Quit,
}

fn minimize_label(minimized: bool) -> &'static str {
    if minimized { "Maximize" } else { "Minimize" }
}

/// Shows the right-click menu as a native popup at the cursor and blocks until
/// it is dismissed. Unlike an egui menu it is not clipped to the window.
#[cfg(windows)]
fn native_menu(
    frame: &eframe::Frame,
    found: &[Provider],
    shown: &[Provider],
    zoom: f32,
    refresh_mins: u64,
    minimized: bool,
) -> Option<MenuAction> {
    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreatePopupMenu() -> isize;
        fn AppendMenuW(menu: isize, flags: u32, id: usize, text: *const u16) -> i32;
        fn TrackPopupMenu(
            menu: isize,
            flags: u32,
            x: i32,
            y: i32,
            reserved: i32,
            hwnd: isize,
            rect: *const core::ffi::c_void,
        ) -> i32;
        fn DestroyMenu(menu: isize) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    }
    const MF_STRING: u32 = 0x0;
    const MF_GRAYED: u32 = 0x1;
    const MF_CHECKED: u32 = 0x8;
    const MF_POPUP: u32 = 0x10;
    const MF_SEPARATOR: u32 = 0x800;
    const TPM_RIGHTBUTTON: u32 = 0x2;
    const TPM_RETURNCMD: u32 = 0x100;
    const WM_NULL: u32 = 0;
    const ID_REFRESH: usize = 1;
    const ID_QUIT: usize = 2;
    const ID_MINIMIZE: usize = 3;
    const ID_UPDATE: usize = 4;
    const ID_OPEN: usize = 10;
    const ID_TOGGLE: usize = 20;
    const ID_TOKEN: usize = 30;
    const ID_SIZE: usize = 100;

    let hwnd = hwnd(frame)?;
    unsafe {
        let add = |menu: isize, flags: u32, id: usize, text: &str| {
            AppendMenuW(menu, flags, id, wide(text).as_ptr());
        };
        let menu = CreatePopupMenu();
        add(menu, MF_STRING, ID_REFRESH, "Refresh now");
        if found.iter().any(|p| p.refreshes_token()) {
            // Owned by `menu` once appended, so destroyed along with it.
            let tokens = CreatePopupMenu();
            for (i, p) in Provider::ALL.iter().enumerate() {
                if found.contains(p) && p.refreshes_token() {
                    add(tokens, MF_STRING, ID_TOKEN + i, p.name());
                }
            }
            add(menu, MF_POPUP, tokens as usize, "Refresh token");
        }
        add(menu, MF_STRING, ID_MINIMIZE, minimize_label(minimized));
        add(menu, MF_SEPARATOR, 0, "");
        if !found.is_empty() {
            add(menu, MF_STRING | MF_GRAYED, 0, "Show");
            for (i, p) in Provider::ALL.iter().enumerate() {
                if found.contains(p) {
                    let checked = if shown.contains(p) { MF_CHECKED } else { 0 };
                    add(menu, MF_STRING | checked, ID_TOGGLE + i, p.name());
                }
            }
            add(menu, MF_SEPARATOR, 0, "");
        }
        for (i, p) in Provider::ALL.iter().enumerate() {
            if found.contains(p) {
                add(
                    menu,
                    MF_STRING,
                    ID_OPEN + i,
                    &format!("Open {} usage", p.name()),
                );
            }
        }
        if !found.is_empty() {
            add(menu, MF_SEPARATOR, 0, "");
        }
        // Owned by `menu` once appended, so destroyed along with it.
        let sizes = CreatePopupMenu();
        for (i, z) in SIZES.iter().enumerate() {
            let checked = if (zoom - z).abs() < 0.01 {
                MF_CHECKED
            } else {
                0
            };
            add(
                sizes,
                MF_STRING | checked,
                ID_SIZE + i,
                &format!("{:.0}%", z * 100.0),
            );
        }
        add(menu, MF_POPUP, sizes as usize, "Size");
        add(menu, MF_STRING, ID_UPDATE, "Check for updates");
        add(menu, MF_SEPARATOR, 0, "");
        let note = format!("Refreshes every {refresh_mins} min");
        add(menu, MF_STRING | MF_GRAYED, 0, &note);
        let version = format!("Version {}", update::CURRENT);
        add(menu, MF_STRING | MF_GRAYED, 0, &version);
        add(menu, MF_STRING, ID_QUIT, "Quit");

        let mut pt = Point::default();
        GetCursorPos(&mut pt);
        // Without these two calls the menu does not close when clicking elsewhere
        // (see the TrackPopupMenu remarks).
        SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);

        match id {
            ID_REFRESH => Some(MenuAction::Refresh),
            ID_MINIMIZE => Some(MenuAction::ToggleMinimized),
            ID_UPDATE => Some(MenuAction::CheckForUpdates),
            ID_QUIT => Some(MenuAction::Quit),
            _ if (ID_OPEN..ID_OPEN + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::Open(Provider::ALL[id - ID_OPEN]))
            }
            _ if (ID_TOGGLE..ID_TOGGLE + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::Toggle(Provider::ALL[id - ID_TOGGLE]))
            }
            _ if (ID_TOKEN..ID_TOKEN + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::RefreshToken(Provider::ALL[id - ID_TOKEN]))
            }
            _ if (ID_SIZE..ID_SIZE + SIZES.len()).contains(&id) => {
                Some(MenuAction::Size(SIZES[id - ID_SIZE]))
            }
            _ => None,
        }
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn egui_menu(
    ui: &mut egui::Ui,
    found: &[Provider],
    shown: &[Provider],
    zoom: f32,
    refresh_mins: u64,
    minimized: bool,
) -> Option<MenuAction> {
    let mut action = None;
    if ui.button("Refresh now").clicked() {
        action = Some(MenuAction::Refresh);
    }
    if found.iter().any(|p| p.refreshes_token()) {
        ui.menu_button("Refresh token", |ui| {
            for &p in found.iter().filter(|p| p.refreshes_token()) {
                if ui.button(p.name()).clicked() {
                    action = Some(MenuAction::RefreshToken(p));
                }
            }
        });
    }
    if ui.button(minimize_label(minimized)).clicked() {
        action = Some(MenuAction::ToggleMinimized);
    }
    ui.separator();
    if !found.is_empty() {
        ui.label(RichText::new("show").size(10.0).color(MUTED));
        for &p in found {
            let mut checked = shown.contains(&p);
            if ui.checkbox(&mut checked, p.name()).clicked() {
                action = Some(MenuAction::Toggle(p));
            }
        }
        ui.separator();
    }
    for &p in found {
        if ui.button(format!("Open {} usage", p.name())).clicked() {
            action = Some(MenuAction::Open(p));
        }
    }
    if !found.is_empty() {
        ui.separator();
    }
    ui.menu_button("Size", |ui| {
        for z in SIZES {
            let label = format!("{:.0}%", z * 100.0);
            if ui.radio((zoom - z).abs() < 0.01, label).clicked() {
                action = Some(MenuAction::Size(z));
            }
        }
    });
    if ui.button("Check for updates").clicked() {
        action = Some(MenuAction::CheckForUpdates);
    }
    ui.separator();
    ui.label(
        RichText::new(format!("refreshes every {refresh_mins} min"))
            .size(10.0)
            .color(MUTED),
    );
    ui.label(
        RichText::new(format!("version {}", update::CURRENT))
            .size(10.0)
            .color(MUTED),
    );
    if ui.button("Quit").clicked() {
        action = Some(MenuAction::Quit);
    }
    action
}

/// Opens a link in the default browser.
#[cfg(windows)]
fn open_url(url: &str) {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    const SW_SHOWNORMAL: i32 = 1;
    unsafe {
        ShellExecuteW(
            0,
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// NUL-terminated UTF-16 for Win32 string parameters.
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Side of the square logo masks in assets/*.alpha.
const LOGO_PX: usize = 64;

/// Uploads the provider logos as white textures so they can be tinted. The marks
/// are the Simple Icons SVGs (CC0) in assets/, rasterized once with resvg to
/// 64x64 8-bit alpha masks so no SVG renderer is needed at runtime.
fn load_logos(ctx: &egui::Context) -> HashMap<Provider, egui::TextureHandle> {
    let options = egui::TextureOptions {
        mipmap_mode: Some(egui::TextureFilter::Linear),
        ..egui::TextureOptions::LINEAR
    };
    Provider::ALL
        .into_iter()
        .map(|p| {
            let alpha: &[u8] = match p {
                Provider::Copilot => include_bytes!("../assets/copilot.alpha"),
                Provider::Claude => include_bytes!("../assets/claude.alpha"),
                Provider::Codex => include_bytes!("../assets/codex.alpha"),
            };
            let rgba: Vec<u8> = alpha.iter().flat_map(|&a| [255, 255, 255, a]).collect();
            let image = egui::ColorImage::from_rgba_unmultiplied([LOGO_PX, LOGO_PX], &rgba);
            (p, ctx.load_texture(p.name(), image, options))
        })
        .collect()
}

fn logo_color(p: Provider) -> Color32 {
    match p {
        Provider::Claude => Color32::from_rgb(217, 119, 87),
        Provider::Copilot | Provider::Codex => TEXT,
    }
}

fn provider_logo(ui: &mut egui::Ui, p: Provider, logos: &HashMap<Provider, egui::TextureHandle>) {
    if let Some(logo) = logos.get(&p) {
        ui.add(egui::Image::new((logo.id(), Vec2::splat(13.0))).tint(logo_color(p)));
    }
}

/// The minimized view: "<logo> COP 42%  <logo> CLD 17%  <logo> CDX 99%" on one line.
fn compact_row(
    ui: &mut egui::Ui,
    providers: &[Provider],
    slots: &HashMap<Provider, Slot>,
    logos: &HashMap<Provider, egui::TextureHandle>,
    bindings: &HashMap<Provider, usize>,
    empty: &str,
) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
    if providers.is_empty() {
        ui.label(RichText::new(empty).size(11.5).color(MUTED));
    }
    for (i, p) in providers.iter().enumerate() {
        if i > 0 {
            ui.add_space(8.0);
        }
        let Some(slot) = slots.get(p) else {
            continue;
        };
        provider_logo(ui, *p, logos);
        ui.label(RichText::new(p.short_name()).size(11.5).color(MUTED));
        // The meter that matters when space is this tight: the binding window on
        // a personal plan, otherwise the most-used one.
        let meters = slot.usage.iter().flat_map(|u| &u.meters);
        let pct = match bindings.get(p) {
            Some(&b) => meters.clone().nth(b).map(Meter::percent),
            None => meters
                .filter(|m| m.total > 0.0)
                .map(Meter::percent)
                .reduce(f64::max),
        };
        let text = match (pct, &slot.error) {
            (Some(pct), _) => RichText::new(format!("{pct:.0}%")).color(level_color(pct)),
            (None, Some(_)) => RichText::new("!").color(ERR),
            (None, None) => RichText::new("…").color(MUTED),
        };
        let resp = ui.label(text.size(11.5).strong());
        if let Some(e) = &slot.error {
            resp.on_hover_text(format!("{}: {}", p.name(), e.detail));
        }
    }
}

/// A small circular-arrow refresh button drawn with the painter (no icon font needed).
fn refresh_button(ui: &mut egui::Ui) -> bool {
    let size = 11.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size + 2.0), Sense::click());
    let color = if resp.hovered() { TEXT } else { MUTED };
    let c = rect.center();
    let r = size * 0.38;
    let stroke = Stroke::new(1.4, color);

    // Arc from ~50° to ~320° (leaving a gap at the top-right where the arrow head sits).
    let start = 50f32.to_radians();
    let end = 320f32.to_radians();
    let n = 20;
    let points: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = start + (end - start) * i as f32 / n as f32;
            Pos2::new(c.x + r * a.cos(), c.y - r * a.sin())
        })
        .collect();
    ui.painter().add(Shape::line(points, stroke));

    // Arrow head at the arc end.
    let tip = Pos2::new(c.x + r * end.cos(), c.y - r * end.sin());
    let head = r * 0.75;
    let tri = vec![
        tip + Vec2::new(-head * 0.55, -head * 0.55),
        tip + Vec2::new(head * 0.4, -head * 0.6),
        tip + Vec2::new(0.1 * head, head * 0.45),
    ];
    ui.painter()
        .add(Shape::convex_polygon(tri, color, Stroke::NONE));

    resp.on_hover_text("Refresh now").clicked()
}

impl eframe::App for App {
    #[cfg(target_os = "macos")]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_menu(ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, MINIMIZED_KEY, &self.minimized);
        eframe::set_value(storage, RATIOS_KEY, &self.ratios);
        eframe::set_value(storage, HIDDEN_KEY, &self.hidden);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // The macOS window is transparent so the panel's rounded corners show;
        // elsewhere the window is opaque and painted entirely in BG.
        if cfg!(target_os = "macos") {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            BG.to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Cheap and idempotent; re-applied every frame because winit rewrites the
        // window styles whenever its own flags change (focus, level, visibility).
        apply_windows_chrome(frame);
        self.drain();
        let ctx = root.ctx().clone();
        // Repaints only to keep the text current: "updated 5m ago" every 30s, and
        // on the minute so a reset countdown ticks over on time.
        ctx.request_repaint_after(until_next_minute().min(Duration::from_secs(30)));
        if !self.settled {
            self.settled = settle_window(frame);
            if !self.settled {
                ctx.request_repaint();
            }
        }

        let margin = if self.minimized {
            Margin::symmetric(MINI_MARGIN_X, MINI_MARGIN_Y)
        } else {
            Margin::same(MARGIN)
        };
        let panel = egui::Frame::NONE
            .fill(BG)
            .inner_margin(margin)
            .corner_radius(if cfg!(target_os = "macos") { 8 } else { 0 });

        let mut refresh = false;
        // On macOS the right-click choice arrives through the menu bar's
        // clicks instead, so this stays unset there.
        #[cfg_attr(target_os = "macos", allow(unused_mut))]
        let mut action = None;
        let zoom = ctx.zoom_factor();
        let refresh_mins = self.interval.as_secs() / 60;
        let minimized = self.minimized;
        let found = self.found();
        let providers = self.shown();
        let bindings: HashMap<Provider, usize> = providers
            .iter()
            .filter_map(|&p| Some((p, self.binding(p)?)))
            .collect();
        let (empty, empty_compact) = self.empty_text(&found);

        // Checked on the raw pointer so a right-click over any widget counts.
        #[cfg(target_os = "macos")]
        let right_clicked = ctx.input(|i| i.pointer.button_clicked(PointerButton::Secondary));
        #[cfg(target_os = "macos")]
        if let Some(bar) = &self.menubar {
            bar.set_services(&found, &providers);
        }

        egui::CentralPanel::default().frame(panel).show(root, |ui| {
            // Whole background is a drag handle and a right-click menu target.
            let bg = ui.interact(ui.max_rect(), ui.id().with("bg"), Sense::click_and_drag());
            if bg.drag_started_by(PointerButton::Primary) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            // An egui menu is clipped to this small window, so Windows gets a
            // native popup menu that can extend past it.
            #[cfg(windows)]
            // Checked on the raw pointer so a right-click over any widget counts.
            if ui.input(|i| i.pointer.button_clicked(PointerButton::Secondary)) {
                action = native_menu(frame, &found, &providers, zoom, refresh_mins, minimized);
            }
            #[cfg(all(not(windows), not(target_os = "macos")))]
            bg.context_menu(|ui| {
                action = egui_menu(ui, &found, &providers, zoom, refresh_mins, minimized);
                if action.is_some() {
                    ui.close();
                }
            });

            ui.spacing_mut().item_spacing.y = 3.0;

            // Wrap the content so its real size can be measured: the panel's own
            // min_rect is always expanded to fill the window.
            let content = if minimized {
                ui.horizontal(|ui| {
                    compact_row(
                        ui,
                        &providers,
                        &self.slots,
                        &self.logos,
                        &bindings,
                        empty_compact,
                    )
                })
            } else {
                ui.vertical(|ui| {
                    if providers.is_empty() {
                        ui.label(RichText::new(empty).size(10.5).color(MUTED));
                    }
                    for (i, p) in providers.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(5.0);
                        }
                        if let Some(slot) = self.slots.get(p) {
                            provider_block(ui, *p, slot, &self.logos, bindings.get(p).copied());
                        }
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if refresh_button(ui) {
                            refresh = true;
                        }
                        let updated = self
                            .last_updated()
                            .map(|t| format!("updated {}", ago(t)))
                            .unwrap_or_else(|| "fetching…".into());
                        ui.label(RichText::new(updated).size(9.5).color(MUTED));

                        if let Some(t) = self.total(&providers) {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let text = format!("${} / ${}", money(t.used), money(t.total));
                                ui.label(RichText::new(text).size(10.5).color(TEXT))
                                    .on_hover_text("Total across all services");
                            });
                        }
                    });
                })
            };

            // Grow or shrink the window to fit the content. Sizes are in points, so
            // this also resizes the window when the zoom factor changes. The compact
            // row sizes the width to its content too; the full card has a fixed width.
            let size = content.response.rect.size();
            let wanted = Vec2::new(
                if minimized {
                    size.x + margin.sum().x
                } else {
                    WIDTH
                },
                size.y + margin.sum().y,
            );
            let current = ctx.viewport_rect().size();
            if (wanted - current).abs().max_elem() > 1.5 {
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
                // The card grows downwards as data arrives (or on zoom), which can
                // push it past the screen edge, so re-check placement next frame.
                self.settled = false;
            }
        });

        // A native popup menu on macOS, since an egui menu is clipped to this
        // small window. The choice arrives later through the menu bar's clicks.
        #[cfg(target_os = "macos")]
        if right_clicked && self.menubar.is_some() && !egui::Popup::is_any_open(&ctx) {
            mac_menu(frame, self, zoom, refresh_mins, minimized);
        }

        if let Some(action) = action {
            self.apply_action(action, &ctx);
        }
        if refresh {
            self.refresh_now();
        }
        // Closing normally lets eframe save position and size before the new
        // version, which waits for this process to exit, starts up.
        if self.restart.swap(false, Ordering::SeqCst) {
            match update::restart() {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => message_box(&format!("Updated, but {e}"), true),
            }
        }
    }
}

/// Shows the right-click menu as a native popup at the cursor and blocks until
/// it is dismissed. Unlike an egui menu it is not clipped to the window. The
/// choice arrives later through `MenuBar::take_actions`.
#[cfg(target_os = "macos")]
fn mac_menu(frame: &eframe::Frame, app: &App, zoom: f32, refresh_mins: u64, minimized: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Some(bar) = &app.menubar else { return };
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: `ns_view` is the live NSView eframe draws into, and `ui` runs on
    // the main thread.
    unsafe {
        bar.popup(
            appkit.ns_view.as_ptr(),
            &app.found(),
            &app.shown(),
            zoom,
            refresh_mins,
            minimized,
        )
    }
}

fn main() -> eframe::Result {
    let mut wait_for_exit = Duration::ZERO;
    match std::env::args().nth(1).as_deref() {
        Some("--startup") => return finish(set_run_at_login(true)),
        Some("--no-startup") => return finish(set_run_at_login(false)),
        // Started by the previous version after an update; it is still closing.
        Some("--updated") => wait_for_exit = Duration::from_secs(30),
        Some(flag) => {
            return finish(Err(format!(
                "unknown flag {flag}

usage: usage-widget [--startup | --no-startup]"
            )));
        }
        None => {}
    }

    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    if detach_from_terminal() {
        return Ok(());
    }

    if !claim_single_instance(wait_for_exit) {
        return Ok(());
    }
    update::cleanup();

    let options = eframe::NativeOptions {
        persist_window: true,
        viewport: ViewportBuilder::default()
            .with_app_id("usage-widget")
            .with_title("Usage")
            .with_inner_size([WIDTH, 180.0])
            .with_decorations(false)
            .with_transparent(cfg!(target_os = "macos"))
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        // An accessory app on macOS: no Dock icon or Cmd+Tab entry; the menu
        // bar icon stands in for them.
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "usage-widget",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// Release builds on Windows have no console; this is the macOS equivalent.
/// Started from a terminal, the widget relaunches itself with no terminal and
/// returns the prompt. The relaunched copy has no terminal, so it runs the
/// widget. If the relaunch fails, the widget runs in the foreground instead.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
fn detach_from_terminal() -> bool {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    unsafe extern "C" {
        fn setsid() -> i32;
    }

    let from_terminal = std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal();
    if !from_terminal {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A new session, so Ctrl+C or closing the terminal does not reach the widget.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().is_ok()
}

/// Whole-window opacity through a layered window. Per-pixel transparency is not
/// available with the OpenGL renderer on Windows, so this dims the whole card.
/// `USAGE_WIDGET_OPACITY` (20-100) overrides the default.
#[cfg(windows)]
fn set_opacity(hwnd: isize) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn SetLayeredWindowAttributes(hwnd: isize, key: u32, alpha: u8, flags: u32) -> i32;
    }
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_LAYERED: isize = 0x0008_0000;
    const LWA_ALPHA: u32 = 0x2;

    let percent = opacity_percent();
    if percent >= 100 {
        return;
    }
    let alpha = (percent * 255 / 100) as u8;
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_LAYERED == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED);
            SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
        }
    }
}

/// `USAGE_WIDGET_OPACITY` (20-100), or the default.
#[cfg(any(windows, target_os = "macos"))]
fn opacity_percent() -> u32 {
    std::env::var("USAGE_WIDGET_OPACITY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_OPACITY_PERCENT)
        .clamp(20, 100)
}

/// Holds a named mutex for the life of the process; returns false if another
/// instance already holds it (e.g. launched again, or at login while running).
/// Keeps trying for up to `wait`, for when the other instance is about to exit.
#[cfg(windows)]
fn claim_single_instance(wait: Duration) -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attrs: *const core::ffi::c_void, owner: i32, name: *const u16) -> isize;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;

    let name = wide(r"Local\usage-widget-single-instance");
    let start = std::time::Instant::now();
    loop {
        // The handle is deliberately never closed once claimed; Windows releases it on exit.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle == 0 || GetLastError() != ERROR_ALREADY_EXISTS {
                return true;
            }
            // Our own handle would keep the mutex alive, so drop it before retrying.
            CloseHandle(handle);
        }
        if start.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Holds an exclusive lock on a file for the life of the process; returns false
/// if another instance already holds it.
#[cfg(not(windows))]
fn claim_single_instance(wait: Duration) -> bool {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    let Ok(file) = std::fs::File::create(std::env::temp_dir().join("usage-widget.lock")) else {
        return true;
    };
    let start = std::time::Instant::now();
    loop {
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            // The system releases the lock when the process exits.
            std::mem::forget(file);
            return true;
        }
        if start.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Registers (or removes) this exe in the per-user Run key so it starts at login.
#[cfg(windows)]
fn set_run_at_login(enable: bool) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("reg");
    if enable {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = exe.display().to_string();
        let value = if exe.contains(' ') {
            format!("\"{exe}\"")
        } else {
            exe
        };
        cmd.args([
            "add",
            RUN_KEY,
            "/v",
            "usage-widget",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ]);
    } else {
        cmd.args(["delete", RUN_KEY, "/v", "usage-widget", "/f"]);
    }
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd
        .output()
        .map_err(|e| format!("could not run reg.exe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !enable && err.contains("unable to find") {
            return Ok("usage-widget was not registered to run at login.".into());
        }
        return Err(format!("reg.exe failed: {err}"));
    }
    Ok(if enable {
        "usage-widget will start at login.\n\nRun `usage-widget --no-startup` to undo.".into()
    } else {
        "usage-widget will no longer start at login.".into()
    })
}

#[cfg(not(windows))]
fn set_run_at_login(_enable: bool) -> Result<String, String> {
    Err("--startup is only supported on Windows".into())
}

/// Reports a flag's outcome. Release builds have no console, so use a message box on Windows.
fn finish(result: Result<String, String>) -> eframe::Result {
    let (text, is_err) = match &result {
        Ok(msg) => (msg.clone(), false),
        Err(msg) => (msg.clone(), true),
    };
    #[cfg(windows)]
    message_box(&text, is_err);
    if is_err {
        eprintln!("{text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

/// Shows a message box on Windows; elsewhere the text goes to stderr.
fn message_box(text: &str, is_err: bool) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, flags: u32) -> i32;
        }
        const MB_ICONERROR: u32 = 0x10;
        const MB_ICONINFORMATION: u32 = 0x40;
        // Without an owner window the box could open behind the always-on-top widget.
        const MB_SETFOREGROUND: u32 = 0x1_0000;
        const MB_TOPMOST: u32 = 0x4_0000;
        let text_w = wide(text);
        let caption_w = wide("usage-widget");
        let icon = if is_err {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
        unsafe {
            MessageBoxW(
                0,
                text_w.as_ptr(),
                caption_w.as_ptr(),
                icon | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    }
    // The widget has no terminal on macOS either, so show a dialog.
    #[cfg(target_os = "macos")]
    {
        let style = if is_err { "critical" } else { "informational" };
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                &format!("display alert \"usage-widget\" message (item 1 of argv) as {style}"),
                "-e",
                "end run",
                text,
            ])
            .status();
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    eprintln!("{text}");
}
