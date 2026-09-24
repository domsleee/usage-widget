//! A tiny always-on-top, frameless, draggable widget that shows Copilot,
//! Claude and Codex usage in dollars, refreshed every few minutes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod providers;
mod timeutil;

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, PointerButton, Pos2, RichText, Sense,
    Shape, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use providers::{Meter, Provider, Unit, money};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use timeutil::{ago, now_unix};

const WIDTH: f32 = 250.0;
const MARGIN: i8 = 12;
const DEFAULT_REFRESH_MINS: u64 = 5;
const DEFAULT_OPACITY_PERCENT: u32 = 85;
const MINI_MARGIN_X: i8 = 10;
const MINI_MARGIN_Y: i8 = 6;
const MINIMIZED_KEY: &str = "minimized";
/// Size presets in the right-click menu, applied as egui's zoom factor on top of
/// the monitor's display scaling. Ctrl +/- also works; either way it persists.
const SIZES: [f32; 8] = [0.25, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0];

// The window is opaque and painted entirely in BG; Windows rounds the corners
// at the compositor level (see `apply_windows_chrome`), so nothing else shows.
const BG: Color32 = Color32::from_rgb(24, 26, 32);
const TEXT: Color32 = Color32::from_gray(232);
const MUTED: Color32 = Color32::from_gray(135);
const TRACK: Color32 = Color32::from_gray(58);
const ERR: Color32 = Color32::from_rgb(235, 110, 110);

struct Update {
    provider: Provider,
    result: Result<Vec<Meter>, String>,
    at: i64,
}

#[derive(Clone, Default)]
struct Slot {
    meters: Option<Vec<Meter>>,
    error: Option<String>,
    updated: Option<i64>,
    loading: bool,
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let interval = Duration::from_secs(60 * refresh_minutes());
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        spawn_worker(cc.egui_ctx.clone(), tx, refresh_rx, interval);
        // Selectable labels grab clicks and drags, so right-click and drag-to-move
        // would not work over text.
        cc.egui_ctx
            .all_styles_mut(|style| style.interaction.selectable_labels = false);
        let minimized = cc
            .storage
            .and_then(|s| eframe::get_value(s, MINIMIZED_KEY))
            .unwrap_or(false);

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
        Self {
            slots,
            rx,
            refresh_tx,
            interval,
            settled: false,
            minimized,
            logos: load_logos(&cc.egui_ctx),
        }
    }

    fn refresh_now(&mut self) {
        for slot in self.slots.values_mut() {
            slot.loading = true;
        }
        let _ = self.refresh_tx.send(());
    }

    fn drain(&mut self) {
        while let Ok(u) = self.rx.try_recv() {
            let slot = self.slots.entry(u.provider).or_default();
            slot.loading = false;
            slot.updated = Some(u.at);
            match u.result {
                Ok(m) => {
                    slot.meters = Some(m);
                    slot.error = None;
                }
                Err(e) => slot.error = Some(e),
            }
        }
    }

    fn last_updated(&self) -> Option<i64> {
        self.slots.values().filter_map(|s| s.updated).max()
    }

    /// Sum of every dollar-denominated meter across providers.
    fn total(&self) -> Option<Meter> {
        let mut used = 0.0;
        let mut total = 0.0;
        let mut any = false;
        for slot in self.slots.values() {
            for m in slot.meters.iter().flatten() {
                if m.unit == Unit::Dollars {
                    used += m.used;
                    total += m.total;
                    any = true;
                }
            }
        }
        any.then(|| Meter {
            label: None,
            used,
            total,
            unit: Unit::Dollars,
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
    }
}

#[cfg(not(windows))]
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
/// Listens for foreground changes and puts the widget back on top whenever the
/// taskbar comes forward. Only the first call installs the hook.
#[cfg(windows)]
fn watch_taskbar(hwnd: isize) {
    use std::sync::atomic::{AtomicIsize, Ordering};

    type WinEventProc = unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32);
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
    }
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x3;
    const WINEVENT_OUTOFCONTEXT: u32 = 0x0;

    static WIDGET: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn on_foreground(
        _hook: isize,
        _event: u32,
        foreground: isize,
        _object: i32,
        _child: i32,
        _thread: u32,
        _time: u32,
    ) {
        let mut buf = [0u16; 32];
        let len = unsafe { GetClassNameW(foreground, buf.as_mut_ptr(), buf.len() as i32) };
        let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
        // The primary monitor's taskbar, and the ones on other monitors.
        if class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd" {
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
    }

    if WIDGET.swap(hwnd, Ordering::Relaxed) != 0 {
        return;
    }
    // Out-of-context hooks are delivered through this (the UI) thread's message loop.
    unsafe {
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

fn provider_block(ui: &mut egui::Ui, p: Provider, slot: &Slot) {
    let single = slot
        .meters
        .as_ref()
        .filter(|m| m.len() == 1 && m[0].label.is_none())
        .map(|m| &m[0]);

    ui.horizontal(|ui| {
        let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
        ui.label(name);
        if slot.loading {
            ui.add(egui::Spinner::new().size(10.0).color(MUTED));
        }
        if let Some(m) = single {
            amounts(ui, m, 12.0);
        }
    });

    match (single, &slot.meters, &slot.error) {
        (Some(m), _, _) => {
            if m.total > 0.0 {
                bar(ui, m.fraction(), level_color(m.percent()));
            }
        }
        (None, Some(meters), _) => {
            for m in meters {
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
        (None, None, Some(err)) => {
            ui.label(RichText::new(err).size(10.5).color(ERR));
        }
        (None, None, None) => {
            ui.label(RichText::new("loading…").size(10.5).color(MUTED));
        }
    }
    if slot.meters.is_some() {
        if let Some(err) = &slot.error {
            ui.label(RichText::new(format!("stale: {err}")).size(10.0).color(ERR));
        }
    }
}

#[derive(Clone, Copy)]
enum MenuAction {
    Refresh,
    ToggleMinimized,
    Open(Provider),
    Size(f32),
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
    const ID_OPEN: usize = 10;
    const ID_SIZE: usize = 100;

    let hwnd = hwnd(frame)?;
    unsafe {
        let add = |menu: isize, flags: u32, id: usize, text: &str| {
            AppendMenuW(menu, flags, id, wide(text).as_ptr());
        };
        let menu = CreatePopupMenu();
        add(menu, MF_STRING, ID_REFRESH, "Refresh now");
        add(menu, MF_STRING, ID_MINIMIZE, minimize_label(minimized));
        add(menu, MF_SEPARATOR, 0, "");
        for (i, p) in Provider::ALL.iter().enumerate() {
            add(
                menu,
                MF_STRING,
                ID_OPEN + i,
                &format!("Open {} usage", p.name()),
            );
        }
        add(menu, MF_SEPARATOR, 0, "");
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
        add(menu, MF_SEPARATOR, 0, "");
        let note = format!("Refreshes every {refresh_mins} min");
        add(menu, MF_STRING | MF_GRAYED, 0, &note);
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
            ID_QUIT => Some(MenuAction::Quit),
            _ if (ID_OPEN..ID_OPEN + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::Open(Provider::ALL[id - ID_OPEN]))
            }
            _ if (ID_SIZE..ID_SIZE + SIZES.len()).contains(&id) => {
                Some(MenuAction::Size(SIZES[id - ID_SIZE]))
            }
            _ => None,
        }
    }
}

#[cfg(not(windows))]
fn egui_menu(
    ui: &mut egui::Ui,
    zoom: f32,
    refresh_mins: u64,
    minimized: bool,
) -> Option<MenuAction> {
    let mut action = None;
    if ui.button("Refresh now").clicked() {
        action = Some(MenuAction::Refresh);
    }
    if ui.button(minimize_label(minimized)).clicked() {
        action = Some(MenuAction::ToggleMinimized);
    }
    ui.separator();
    for p in Provider::ALL {
        if ui.button(format!("Open {} usage", p.name())).clicked() {
            action = Some(MenuAction::Open(p));
        }
    }
    ui.separator();
    ui.menu_button("Size", |ui| {
        for z in SIZES {
            let label = format!("{:.0}%", z * 100.0);
            if ui.radio((zoom - z).abs() < 0.01, label).clicked() {
                action = Some(MenuAction::Size(z));
            }
        }
    });
    ui.separator();
    ui.label(
        RichText::new(format!("refreshes every {refresh_mins} min"))
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

/// The minimized view: "<logo> COP 42%  <logo> CLD 17%  <logo> CDX 99%" on one line.
fn compact_row(
    ui: &mut egui::Ui,
    slots: &HashMap<Provider, Slot>,
    logos: &HashMap<Provider, egui::TextureHandle>,
) {
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
    for (i, p) in Provider::ALL.iter().enumerate() {
        if i > 0 {
            ui.add_space(8.0);
        }
        let slot = slots.get(p).cloned().unwrap_or_default();
        if let Some(logo) = logos.get(p) {
            ui.add(egui::Image::new((logo.id(), Vec2::splat(13.0))).tint(logo_color(*p)));
        }
        ui.label(RichText::new(p.short_name()).size(11.5).color(MUTED));
        // The most-used meter is the one that matters when space is this tight.
        let pct = slot
            .meters
            .iter()
            .flatten()
            .filter(|m| m.total > 0.0)
            .map(Meter::percent)
            .reduce(f64::max);
        let text = match (pct, &slot.error) {
            (Some(pct), _) => RichText::new(format!("{pct:.0}%")).color(level_color(pct)),
            (None, Some(_)) => RichText::new("!").color(ERR),
            (None, None) => RichText::new("…").color(MUTED),
        };
        let resp = ui.label(text.size(11.5).strong());
        if let Some(err) = &slot.error {
            resp.on_hover_text(format!("{}: {err}", p.name()));
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
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, MINIMIZED_KEY, &self.minimized);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Cheap and idempotent; re-applied every frame because winit rewrites the
        // window styles whenever its own flags change (focus, level, visibility).
        apply_windows_chrome(frame);
        self.drain();
        let ctx = root.ctx().clone();
        ctx.request_repaint_after(Duration::from_secs(30));
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
        let panel = egui::Frame::NONE.fill(BG).inner_margin(margin);

        let mut refresh = false;
        let mut action = None;
        let zoom = ctx.zoom_factor();
        let refresh_mins = self.interval.as_secs() / 60;
        let minimized = self.minimized;

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
                action = native_menu(frame, zoom, refresh_mins, minimized);
            }
            #[cfg(not(windows))]
            bg.context_menu(|ui| {
                action = egui_menu(ui, zoom, refresh_mins, minimized);
                if action.is_some() {
                    ui.close();
                }
            });

            ui.spacing_mut().item_spacing.y = 3.0;

            // Wrap the content so its real size can be measured: the panel's own
            // min_rect is always expanded to fill the window.
            let content = if minimized {
                ui.horizontal(|ui| compact_row(ui, &self.slots, &self.logos))
            } else {
                ui.vertical(|ui| {
                    for (i, p) in Provider::ALL.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(5.0);
                        }
                        let slot = self.slots.get(p).cloned().unwrap_or_default();
                        provider_block(ui, *p, &slot);
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

                        if let Some(t) = self.total() {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let text = format!("${} / ${}", money(t.used), money(t.total));
                                ui.label(RichText::new(text).size(10.5).color(TEXT))
                                    .on_hover_text("Total across all three");
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

        match action {
            Some(MenuAction::Refresh) => refresh = true,
            Some(MenuAction::ToggleMinimized) => self.minimized = !self.minimized,
            Some(MenuAction::Open(p)) => open_url(p.url()),
            Some(MenuAction::Size(z)) => ctx.set_zoom_factor(z),
            Some(MenuAction::Quit) => ctx.send_viewport_cmd(ViewportCommand::Close),
            None => {}
        }
        if refresh {
            self.refresh_now();
        }
    }
}

fn main() -> eframe::Result {
    match std::env::args().nth(1).as_deref() {
        Some("--startup") => return finish(set_run_at_login(true)),
        Some("--no-startup") => return finish(set_run_at_login(false)),
        Some(flag) => {
            return finish(Err(format!(
                "unknown flag {flag}

usage: usage-widget [--startup | --no-startup]"
            )));
        }
        None => {}
    }

    if !claim_single_instance() {
        return Ok(());
    }

    let options = eframe::NativeOptions {
        persist_window: true,
        viewport: ViewportBuilder::default()
            .with_app_id("usage-widget")
            .with_title("Usage")
            .with_inner_size([WIDTH, 180.0])
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "usage-widget",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
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

    let percent = std::env::var("USAGE_WIDGET_OPACITY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_OPACITY_PERCENT)
        .clamp(20, 100);
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

/// Holds a named mutex for the life of the process; returns false if another
/// instance already holds it (e.g. launched again, or at login while running).
#[cfg(windows)]
fn claim_single_instance() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attrs: *const core::ffi::c_void, owner: i32, name: *const u16) -> isize;
        fn GetLastError() -> u32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;

    let name = wide(r"Local\usage-widget-single-instance");
    // The handle is deliberately never closed; Windows releases it on exit.
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        handle == 0 || GetLastError() != ERROR_ALREADY_EXISTS
    }
}

#[cfg(not(windows))]
fn claim_single_instance() -> bool {
    true
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
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, flags: u32) -> i32;
        }
        let text_w = wide(&text);
        let caption_w = wide("usage-widget");
        let icon = if is_err { 0x10 } else { 0x40 }; // MB_ICONERROR / MB_ICONINFORMATION
        unsafe {
            MessageBoxW(0, text_w.as_ptr(), caption_w.as_ptr(), icon);
        }
    }
    if is_err {
        eprintln!("{text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}
