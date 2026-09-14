//! A tiny always-on-top, frameless, draggable widget that shows Copilot,
//! Claude and Codex usage in dollars, refreshed every few minutes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod claude_estimate;
mod claude_web;
mod codex_estimate;
mod config;
mod estimate;
mod estimate_history;
#[cfg(target_os = "macos")]
mod menubar;
mod providers;
mod renewal_lookup;
mod timeutil;

use config::Config;
use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, PointerButton, Pos2, RichText, Sense,
    Shape, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use providers::{Cycle, Meter, Provider, Unit, Usage, money, spend_meter};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use timeutil::{ago, local_when, now_unix, resets};

const WIDTH: f32 = if cfg!(windows) { 320.0 } else { 270.0 };
const MARGIN: i8 = 12;
/// Width of the meter-label column, so every reset time starts at the same x.
const LABEL_COL: f32 = 36.0;
/// Gap between usage windows shown side by side.
const CELL_GAP: f32 = 6.0;
/// Width of the percentage column on spend rows, so the amounts line up.
const PCT_COL: f32 = 46.0;

// Windows rounds the opaque window at the compositor level; macOS uses a
// rounded panel over a transparent window.
const BG: Color32 = Color32::from_rgb(24, 26, 32);
const TEXT: Color32 = Color32::from_gray(232);
/// Meter labels ("5h", "week").
const LABEL: Color32 = Color32::from_rgb(196, 199, 205);
/// Supporting details: plan, renewal, countdowns, notes.
const META: Color32 = Color32::from_rgb(150, 155, 165);
const MUTED: Color32 = Color32::from_gray(135);
const TRACK: Color32 = Color32::from_rgb(48, 50, 56);
const ERR: Color32 = Color32::from_rgb(235, 110, 110);

struct Update {
    provider: Provider,
    result: Result<Usage, String>,
    at: i64,
    started: std::time::Instant,
}

#[derive(Clone, Default)]
struct Slot {
    plan: Option<String>,
    cycle: Option<Cycle>,
    note: Option<String>,
    estimated: Vec<String>,
    meters: Option<Vec<Meter>>,
    error: Option<String>,
    updated: Option<i64>,
    loading: bool,
}

struct App {
    providers: Vec<Provider>,
    slots: HashMap<Provider, Slot>,
    rx: Receiver<Update>,
    refresh_tx: Sender<()>,
    interval: Duration,
    opacity: u32,
    /// Decimal places for percentages.
    precision: usize,
    config_error: Option<String>,
    renewal_lookup: Option<renewal_lookup::Lookup>,
    renewal_error: Option<String>,
    renewal_applied_at: Option<std::time::Instant>,
    #[cfg(target_os = "macos")]
    menubar: Option<menubar::MenuBar>,
    #[cfg(target_os = "macos")]
    shown: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, config: Config, config_error: Option<String>) -> Self {
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowExtWindows;
            // egui enables this for frameless windows; winit reserves a visible
            // 1px strip at the top to draw it, even with the DWM border disabled.
            if let Some(window) = cc.winit_window() {
                window.set_undecorated_shadow(false);
            }
            configure_windows_fonts(&cc.egui_ctx);
        }

        let providers: Vec<Provider> = Provider::ALL
            .into_iter()
            .filter(|p| config.enabled(*p))
            .collect();
        let interval = config.refresh_interval();
        let opacity = config.opacity;
        let precision = config.precision.min(3);
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        spawn_worker(
            cc.egui_ctx.clone(),
            tx,
            refresh_rx,
            providers.clone(),
            config,
        );

        let slots = providers
            .iter()
            .map(|p| {
                let slot = Slot {
                    loading: true,
                    ..Default::default()
                };
                (*p, slot)
            })
            .collect();
        #[cfg(target_os = "macos")]
        let (menubar, config_error) = match menubar::MenuBar::new(&cc.egui_ctx, &providers) {
            Ok(bar) => (Some(bar), config_error),
            Err(e) => (None, config_error.or(Some(format!("menu bar icon: {e}")))),
        };
        Self {
            providers,
            slots,
            rx,
            refresh_tx,
            interval,
            opacity,
            precision,
            config_error,
            renewal_lookup: None,
            renewal_error: None,
            renewal_applied_at: None,
            #[cfg(target_os = "macos")]
            menubar,
            #[cfg(target_os = "macos")]
            shown: true,
        }
    }

    /// An App with no window and no menu bar, plus the far ends of its channels.
    #[cfg(test)]
    fn for_test(providers: Vec<Provider>) -> (Self, Sender<Update>, Receiver<()>) {
        let (updates, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let slots = providers.iter().map(|p| (*p, Slot::default())).collect();
        let app = Self {
            providers,
            slots,
            rx,
            refresh_tx,
            interval: Duration::from_secs(300),
            opacity: 100,
            precision: 1,
            config_error: None,
            renewal_lookup: None,
            renewal_error: None,
            renewal_applied_at: None,
            #[cfg(target_os = "macos")]
            menubar: None,
            #[cfg(target_os = "macos")]
            shown: true,
        };
        (app, updates, refresh_rx)
    }

    fn refresh_now(&mut self) {
        for slot in self.slots.values_mut() {
            slot.loading = true;
        }
        let _ = self.refresh_tx.send(());
    }

    /// Menu bar clicks. Called from `logic`, which runs before every `ui` pass and
    /// also while the widget is hidden — when eframe runs no egui pass at all, so
    /// handling these in `ui` would leave a hidden widget with no way back.
    #[cfg(target_os = "macos")]
    fn handle_menu(&mut self, ctx: &egui::Context) {
        let Some(actions) = self.menubar.as_ref().map(menubar::MenuBar::take_actions) else {
            return;
        };
        for action in actions {
            self.apply_menu_action(action, ctx);
        }
    }

    #[cfg(target_os = "macos")]
    fn apply_menu_action(&mut self, action: menubar::Action, ctx: &egui::Context) {
        match action {
            menubar::Action::ToggleWidget => {
                self.shown = !self.shown;
                ctx.send_viewport_cmd(ViewportCommand::Visible(self.shown));
                if let Some(bar) = &self.menubar {
                    bar.set_widget_shown(self.shown);
                }
            }
            menubar::Action::Refresh => self.refresh_now(),
            menubar::Action::Open(p) => ctx.open_url(egui::OpenUrl::new_tab(p.url())),
            menubar::Action::EditConfig => {
                if let Err(e) = config::open(false) {
                    self.config_error = Some(e);
                }
            }
            menubar::Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn drain(&mut self) {
        while let Ok(u) = self.rx.try_recv() {
            let slot = self.slots.entry(u.provider).or_default();
            slot.loading = false;
            slot.updated = Some(u.at);
            match u.result {
                Ok(usage) => {
                    slot.plan = usage.plan;
                    let monthly_claude =
                        u.provider == Provider::Claude && spend_meter(&usage.meters).is_some();
                    if monthly_claude {
                        self.renewal_lookup = None;
                        self.renewal_error = None;
                        self.renewal_applied_at = None;
                    }
                    if u.provider != Provider::Claude
                        || self
                            .renewal_applied_at
                            .is_none_or(|applied| u.started > applied)
                    {
                        slot.cycle = usage.cycle;
                    }
                    slot.note = usage.note;
                    slot.estimated = usage.estimated;
                    slot.meters = Some(usage.meters);
                    slot.error = None;
                }
                Err(e) => slot.error = Some(e),
            }
        }
        if let Some(rx) = &self.renewal_lookup {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Renewal lookup stopped unexpectedly. Try again.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.renewal_lookup = None;
                match result {
                    Ok(cycle) => {
                        self.renewal_applied_at = Some(std::time::Instant::now());
                        if let Some(slot) = self.slots.get_mut(&Provider::Claude) {
                            slot.cycle = Some(cycle);
                        }
                        self.refresh_now();
                    }
                    Err(error) => self.renewal_error = Some(error),
                }
            }
        }
    }

    fn last_updated(&self) -> Option<i64> {
        self.slots.values().filter_map(|s| s.updated).max()
    }

    /// Sum of the dollar-denominated meters across providers. None unless there
    /// are several, since a single one would just repeat its own row.
    fn total(&self) -> Option<Meter> {
        let mut used = 0.0;
        let mut total = 0.0;
        let mut count = 0;
        for slot in self.slots.values() {
            for m in slot.meters.iter().flatten() {
                if m.unit == Unit::Dollars {
                    used += m.used;
                    total += m.total;
                    count += 1;
                }
            }
        }
        (count > 1).then(|| Meter {
            label: None,
            used,
            total,
            unit: Unit::Dollars,
            resets_at: None,
        })
    }
}

fn spawn_worker(
    ctx: egui::Context,
    tx: Sender<Update>,
    refresh_rx: Receiver<()>,
    providers: Vec<Provider>,
    config: Config,
) {
    let interval = config.refresh_interval();
    std::thread::spawn(move || {
        let mut config = config;
        loop {
            let started = std::time::Instant::now();
            // The browser helper updates this setting while the widget is open.
            let (latest, error) = config::load();
            if error.is_none() {
                config.claude.renewal_date = latest.claude.renewal_date;
            }
            std::thread::scope(|s| {
                for &p in &providers {
                    let tx = tx.clone();
                    let ctx = ctx.clone();
                    let config = &config;
                    s.spawn(move || {
                        let result = p.fetch(config);
                        let _ = tx.send(Update {
                            provider: p,
                            result,
                            at: now_unix(),
                            started,
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

/// Windows 11: round the window corners and drop the accent border, so the
/// frameless window looks like a floating card, and apply the opacity.
#[cfg(windows)]
fn apply_window_style(frame: &eframe::Frame, opacity: u32) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

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

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return;
    };
    let hwnd = win.hwnd.get();
    set_opacity(hwnd, opacity);
    unsafe {
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

#[cfg(windows)]
fn configure_windows_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontTweak, HintingTarget, SmoothHinting};

    let Some(windows) = std::env::var_os("WINDIR") else {
        return;
    };
    let path = std::path::Path::new(&windows)
        .join("Fonts")
        .join("segoeui.ttf");
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let font = egui::FontData::from_owned(data).tweak(FontTweak {
        hinting: Some(true),
        hinting_target: HintingTarget::Smooth(SmoothHinting {
            symmetric_rendering: false,
            preserve_linear_metrics: false,
            ..Default::default()
        }),
        ..Default::default()
    });
    fonts.font_data.insert("Segoe UI".into(), font.into());
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .insert(0, "Segoe UI".into());
    ctx.set_fonts(fonts);
}

fn text_size(size: f32) -> f32 {
    // Keep supporting text legible on Windows displays running at 100% scaling.
    if cfg!(windows) { size.max(11.0) } else { size }
}

/// macOS: whole-window opacity through the NSWindow's alpha, matching the layered
/// window on Windows.
#[cfg(target_os = "macos")]
fn apply_window_style(frame: &eframe::Frame, opacity: u32) {
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
    let alpha = f64::from(opacity.clamp(20, 100)) / 100.0;
    // SAFETY: `ns_view` is the live NSView eframe draws into, and `ui` runs on the
    // main thread, where AppKit calls belong.
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if !window.is_null() {
            let _: () = msg_send![window, setAlphaValue: alpha];
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn apply_window_style(_frame: &eframe::Frame, _opacity: u32) {}

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
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), TRACK);
    if fraction > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * fraction).max(4.0));
        painter.rect_filled(fill, CornerRadius::same(2), color);
    }
}

const ESTIMATE_HOVER: &str = "Estimated: Codex reports whole percents, so the part of the next one \
     comes from this computer's Codex token use. Codex cloud and other devices \
     aren't counted.";

/// The percentage, coloured by level, with "~" when it includes a local estimate.
fn percent_text(m: &Meter, size: f32, precision: usize, estimated: bool) -> RichText {
    let pct = m.percent();
    RichText::new(percent_label(pct, precision, estimated))
        .size(text_size(size))
        .strong()
        .color(level_color(pct))
}

fn percent_label(pct: f64, precision: usize, estimated: bool) -> String {
    // Estimates must retain their fractional part even when the user chooses
    // whole-percent display for authoritative readings.
    let precision = if estimated {
        precision.max(2)
    } else {
        precision
    };
    let pct = if estimated {
        pct.min(pct.floor() + 0.99)
    } else {
        pct
    };
    let mark = if estimated { "~" } else { "" };
    format!("{mark}{pct:.precision$}%")
}

/// "$523.54 / $1,000   52%" right-aligned, coloured by level.
fn amounts(ui: &mut egui::Ui, m: &Meter, size: f32, precision: usize, estimated: bool) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if m.total > 0.0 {
            let resp = ui.label(percent_text(m, size, precision, estimated));
            if estimated {
                resp.on_hover_text(ESTIMATE_HOVER);
            }
            if m.unit != Unit::Percent {
                ui.label(
                    RichText::new(m.summary())
                        .size(text_size(size - 1.0))
                        .color(TEXT),
                );
            }
        } else {
            ui.label(
                RichText::new("unlimited")
                    .size(text_size(size))
                    .color(MUTED),
            );
        }
    });
}

/// The meter of a service that only reports spend with no sub-label (Copilot, or
/// Claude and Codex on usage-based plans).
fn spend_only(slot: &Slot) -> Option<&Meter> {
    spend_meter(slot.meters.as_deref()?).filter(|m| m.total > 0.0)
}

struct RenewalButton<'a> {
    running: bool,
    error: Option<&'a str>,
    clicked: bool,
    cancelled: bool,
}

impl RenewalButton<'_> {
    fn show(&mut self, ui: &mut egui::Ui, provider: Provider, slot: &Slot) {
        if provider != Provider::Claude
            || slot.cycle.is_some()
            || slot.meters.as_deref().and_then(spend_meter).is_some()
        {
            return;
        }
        if self.running {
            let spinner = ui.add(egui::Spinner::new().size(10.0).color(MUTED));
            let response =
                ui.interact(spinner.rect, ui.id().with("cancel-renewal"), Sense::click());
            self.context_menu(&response);
            response.on_hover_text(
                "Looking up renewal — complete sign-in in Chrome. Right-click to cancel.",
            );
        } else {
            let tooltip = self.error.map_or_else(
                || "Look up Claude renewal in Chrome".to_string(),
                |error| format!("Renewal lookup failed: {error}\nClick to retry"),
            );
            self.clicked |= refresh_button(ui, &tooltip, self.error.is_some());
        }
    }

    fn context_menu(&mut self, response: &egui::Response) {
        response.context_menu(|ui| {
            let label = if self.running {
                "Looking up…"
            } else {
                "Look up renewal"
            };
            if ui
                .add_enabled(!self.running, egui::Button::new(label))
                .clicked()
            {
                self.clicked = true;
                ui.close();
            }
            if self.running {
                ui.label("Complete sign-in in Chrome");
                if ui.button("Cancel lookup").clicked() {
                    self.cancelled = true;
                    ui.close();
                }
            }
            if let Some(error) = self.error {
                ui.label(RichText::new(error).color(ERR));
            }
        });
    }
}

fn provider_block(
    ui: &mut egui::Ui,
    p: Provider,
    slot: &Slot,
    precision: usize,
    renewal: &mut RenewalButton<'_>,
) {
    if let Some(m) = spend_only(slot) {
        spend_row(ui, p, slot, m, precision);
    } else {
        header(ui, p, slot, renewal);
        ui.add_space(6.0);
        match (&slot.meters, &slot.error) {
            (Some(meters), _) => meters_block(ui, meters, slot, precision),
            (None, Some(err)) => {
                ui.label(RichText::new(err).size(text_size(10.5)).color(ERR));
            }
            (None, None) => {
                ui.label(RichText::new("loading…").size(text_size(10.5)).color(MUTED));
            }
        }
    }
    if slot.meters.is_some() {
        if let Some(note) = &slot.note {
            ui.add_space(5.0);
            ui.label(RichText::new(note).size(text_size(10.0)).color(META));
        }
        if let Some(err) = &slot.error {
            ui.add_space(5.0);
            ui.label(
                RichText::new(format!("stale: {err}"))
                    .size(text_size(10.0))
                    .color(ERR),
            );
        }
    }
}

/// Name, amount and percentage on one line over one bar, as in the original
/// widget. The plan and billing cycle move to the name's hover text.
fn spend_row(ui: &mut egui::Ui, p: Provider, slot: &Slot, m: &Meter, precision: usize) {
    let row = Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
        let mut hover: Vec<String> = slot.plan.iter().cloned().collect();
        if let Some(c) = &slot.cycle {
            hover.push(format!("{} {}", c.verb, c.when()));
        }
        hover.push(p.url().into());
        // Usage-based plans reset monthly and need no subscription renewal lookup.
        let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
        ui.hyperlink_to(name, p.url())
            .on_hover_text(hover.join("\n"));
        if slot.loading {
            ui.add(egui::Spinner::new().size(10.0).color(MUTED));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let col = Vec2::new(PCT_COL, row.y);
            ui.allocate_ui_with_layout(col, Layout::right_to_left(Align::Center), |ui| {
                ui.label(percent_text(m, 11.0, precision, false));
            });
            ui.label(RichText::new(m.summary()).size(11.0).color(TEXT));
        });
    });
    ui.add_space(3.0);
    bar(ui, m.fraction(), level_color(m.percent()));
}

/// Several usage windows sit side by side, each with its reset on its label line.
/// A lone window, spend and "unlimited" get a full-width row.
fn meters_block(ui: &mut egui::Ui, meters: &[Meter], slot: &Slot, precision: usize) {
    let estimated = |m: &Meter| m.label.as_ref().is_some_and(|l| slot.estimated.contains(l));
    let is_window = |m: &Meter| m.unit == Unit::Percent && m.total > 0.0;
    let windows: Vec<&Meter> = meters.iter().filter(|m| is_window(m)).collect();
    let side_by_side = windows.len() > 1;
    // Two or four windows split into pairs; otherwise rows of three.
    let cols = if matches!(windows.len(), 2 | 4) { 2 } else { 3 };

    let mut blocks = 0;
    if side_by_side {
        for chunk in windows.chunks(cols) {
            if blocks > 0 {
                ui.add_space(6.0);
            }
            blocks += 1;
            window_cells(ui, chunk, cols, precision, &estimated);
        }
    }
    for m in meters.iter().filter(|m| !(side_by_side && is_window(m))) {
        if blocks > 0 {
            ui.add_space(6.0);
        }
        blocks += 1;
        meter_row(ui, m, precision, estimated(m));
    }
}

fn window_cells(
    ui: &mut egui::Ui,
    windows: &[&Meter],
    cols: usize,
    precision: usize,
    estimated: &dyn Fn(&Meter) -> bool,
) {
    let width = (ui.available_width() - CELL_GAP * (cols - 1) as f32) / cols as f32;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = CELL_GAP;
        for m in windows {
            ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(width);
                window_cell(ui, m, precision, estimated(m));
            });
        }
    });
}

/// Label and reset time on one line, then the bar and percentage.
fn window_cell(ui: &mut egui::Ui, m: &Meter, precision: usize, estimated: bool) {
    let line = Vec2::new(ui.available_width(), 12.0);
    ui.allocate_ui_with_layout(line, Layout::left_to_right(Align::Max), |ui| {
        if let Some(label) = &m.label {
            ui.label(RichText::new(label).size(text_size(9.5)).color(LABEL));
        }
        if let Some(ts) = m.resets_at {
            ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                ui.label(RichText::new(resets(ts)).size(text_size(9.0)).color(META))
                    .on_hover_text(local_when(ts));
            });
        }
    });
    ui.add_space(3.0);
    bar(ui, m.fraction(), level_color(m.percent()));
    ui.add_space(3.0);
    let resp = ui.label(percent_text(m, 12.0, precision, estimated));
    if estimated {
        resp.on_hover_text(ESTIMATE_HOVER);
    }
}

/// Service name, then plan and billing cycle in small text.
fn header(ui: &mut egui::Ui, p: Provider, slot: &Slot, renewal: &mut RenewalButton<'_>) {
    // A fixed-height, bottom-aligned row (`with_layout` would take all the height left).
    let row = Vec2::new(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Max), |ui| {
        let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
        ui.hyperlink_to(name, p.url()).on_hover_text(p.url());
        renewal.show(ui, p, slot);
        let mut info: Vec<String> = slot.plan.iter().cloned().collect();
        let mut hover = None;
        if let Some(c) = &slot.cycle {
            // A countdown, recomputed every frame so it stays right as time passes.
            info.push(format!(
                "{} {}",
                c.verb,
                c.countdown(chrono::Local::now().date_naive())
            ));
            hover = Some(format!("{} {}", c.verb, c.when()));
        }
        if p == Provider::Claude && slot.cycle.is_some() {
            if renewal.running {
                info.push("looking up…".into());
            } else if renewal.error.is_some() {
                info.push("lookup failed".into());
            }
        }
        if !info.is_empty() {
            let font = egui::FontId::proportional(text_size(10.0));
            let prefix_width = slot.plan.as_ref().map_or(0.0, |plan| {
                ui.painter()
                    .layout_no_wrap(format!("{plan} · "), font.clone(), META)
                    .size()
                    .x
            });
            let galley = ui.painter().layout_no_wrap(info.join(" · "), font, META);
            let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::hover());
            // egui aligns text boxes, not baselines: bottom-aligned, the smaller text
            // sits 1pt below the name's baseline (measured), so lift it by that.
            ui.painter()
                .galley(rect.min - Vec2::new(0.0, 1.0), galley, META);
            let resp = if p == Provider::Claude && slot.cycle.is_some() {
                let renewal_rect =
                    egui::Rect::from_min_max(rect.min + Vec2::new(prefix_width, 0.0), rect.max);
                let response = ui.interact(
                    renewal_rect,
                    ui.id().with("claude-renewal"),
                    Sense::click_and_drag(),
                );
                if response.drag_started_by(egui::PointerButton::Primary) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                renewal.context_menu(&response);
                response.on_hover_text(
                    renewal
                        .error
                        .unwrap_or("Right-click for renewal lookup actions"),
                )
            } else {
                resp
            };
            if let Some(hover) = hover {
                resp.on_hover_text(hover);
            }
        }
        if slot.loading {
            ui.add(egui::Spinner::new().size(10.0).color(MUTED));
        }
    });
}

/// Label, reset time and amount on one line, over a thin bar.
fn meter_row(ui: &mut egui::Ui, m: &Meter, precision: usize, estimated: bool) {
    ui.horizontal(|ui| {
        let size = Vec2::new(LABEL_COL, ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(LABEL_COL);
            if let Some(label) = &m.label {
                ui.label(RichText::new(label).size(11.0).color(LABEL));
            }
        });
        if let Some(ts) = m.resets_at {
            ui.label(RichText::new(resets(ts)).size(text_size(9.5)).color(META))
                .on_hover_text(local_when(ts));
        }
        amounts(ui, m, 12.0, precision, estimated);
    });
    if m.total > 0.0 {
        ui.add_space(3.0);
        bar(ui, m.fraction(), level_color(m.percent()));
    }
}

/// A small circular-arrow refresh button drawn with the painter (no icon font needed).
fn refresh_button(ui: &mut egui::Ui, tooltip: &str, error: bool) -> bool {
    let size = 11.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size + 2.0), Sense::click());
    let color = if error {
        ERR
    } else if resp.hovered() {
        TEXT
    } else {
        MUTED
    };
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

    resp.on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

impl eframe::App for App {
    /// Runs before every `ui` pass, and is all that runs while the widget is
    /// hidden: eframe leaves the egui state untouched then, so this is the only
    /// chance to act on "Show widget".
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        #[cfg(target_os = "macos")]
        self.handle_menu(_ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if cfg!(target_os = "macos") {
            Color32::TRANSPARENT.to_normalized_gamma_f32()
        } else {
            BG.to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Cheap and idempotent; re-applied every frame because winit rewrites the
        // window styles whenever its own flags change (focus, level, visibility).
        apply_window_style(frame, self.opacity);
        let ctx = root.ctx().clone();
        ctx.request_repaint_after(Duration::from_secs(30));

        let panel = egui::Frame::NONE
            .fill(BG)
            .corner_radius(if cfg!(target_os = "macos") { 8 } else { 0 })
            .inner_margin(Margin::same(MARGIN));

        let mut refresh = false;
        let mut edit_config = false;
        let mut quit = false;
        let mut renewal = RenewalButton {
            running: self.renewal_lookup.is_some(),
            error: self.renewal_error.as_deref(),
            clicked: false,
            cancelled: false,
        };

        egui::CentralPanel::default().frame(panel).show(root, |ui| {
            // Whole background is a drag handle and a right-click menu target.
            let bg = ui.interact(ui.max_rect(), ui.id().with("bg"), Sense::click_and_drag());
            if bg.drag_started_by(PointerButton::Primary) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            bg.context_menu(|ui| {
                if ui.button("Refresh now").clicked() {
                    refresh = true;
                    ui.close();
                }
                ui.separator();
                for p in &self.providers {
                    if ui.button(format!("Open {} usage", p.name())).clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(p.url()));
                        ui.close();
                    }
                }
                ui.separator();
                ui.label(
                    RichText::new(format!(
                        "refreshes every {} min",
                        self.interval.as_secs() / 60
                    ))
                    .size(text_size(10.0))
                    .color(MUTED),
                );
                if ui.button("Edit config").clicked() {
                    edit_config = true;
                    ui.close();
                }
                if ui.button("Quit").clicked() {
                    quit = true;
                    ui.close();
                }
            });

            // Vertical gaps are set explicitly so they can differ by role: tight within
            // a meter, looser between meters, widest between services.
            ui.spacing_mut().item_spacing.y = 0.0;

            // Wrap the content so its real height can be measured: the panel's own
            // min_rect is always expanded to fill the window.
            let content = ui.vertical(|ui| {
                for (i, p) in self.providers.iter().enumerate() {
                    let slot = self.slots.get(p).cloned().unwrap_or_default();
                    if i > 0 {
                        // One-line spend rows stack a little tighter, as in the original.
                        ui.add_space(if spend_only(&slot).is_some() {
                            10.0
                        } else {
                            12.0
                        });
                    }
                    provider_block(ui, *p, &slot, self.precision, &mut renewal);
                }
                if self.providers.is_empty() {
                    ui.label(
                        RichText::new("Every service is disabled in the config.")
                            .size(text_size(10.5))
                            .color(MUTED),
                    );
                }
                if let Some(err) = &self.config_error {
                    ui.label(RichText::new(err).size(text_size(10.0)).color(ERR));
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if refresh_button(ui, "Refresh now", false) {
                        refresh = true;
                    }
                    let updated = self
                        .last_updated()
                        .map(|t| format!("updated {}", ago(t)))
                        .unwrap_or_else(|| "fetching…".into());
                    ui.label(RichText::new(updated).size(text_size(9.5)).color(MUTED));

                    if let Some(t) = self.total() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let text = format!("${} / ${}", money(t.used), money(t.total));
                            ui.label(RichText::new(text).size(text_size(10.0)).color(MUTED))
                                .on_hover_text("Total across services");
                        });
                    }
                });
            });

            // Grow or shrink the window to fit the content. Capped so a layout that
            // feeds back on the window size can't exceed the GPU's surface limit.
            let height = content.response.rect.height() + 2.0 * MARGIN as f32;
            let wanted = Vec2::new(WIDTH, height.min(1200.0));
            if (wanted - ctx.viewport_rect().size()).length() > 1.5 {
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
            }
        });

        let lookup_renewal = renewal.clicked;
        if renewal.cancelled {
            self.renewal_lookup = None;
        }
        if refresh {
            self.refresh_now();
        }
        if lookup_renewal {
            self.renewal_error = None;
            self.renewal_lookup = Some(renewal_lookup::start(ctx.clone()));
        }
        if edit_config && let Err(e) = config::open(false) {
            self.config_error = Some(e);
        }
        if quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    #[test]
    fn monthly_spend_cancels_pending_renewal() {
        for total in [0.0, 100.0] {
            let (mut app, updates, _) = App::for_test(vec![Provider::Claude]);
            let (_lookup_tx, lookup_rx) = mpsc::channel();
            app.renewal_lookup = Some(renewal_lookup::Lookup::from_receiver(lookup_rx));
            app.renewal_error = Some("previous failure".into());
            app.renewal_applied_at = Some(std::time::Instant::now());
            app.slots.get_mut(&Provider::Claude).unwrap().cycle = Some(Cycle {
                verb: "renews".into(),
                at: 1,
                date_only: true,
            });
            updates
                .send(Update {
                    provider: Provider::Claude,
                    result: Ok(Usage {
                        plan: None,
                        cycle: None,
                        note: None,
                        estimated: Vec::new(),
                        meters: vec![Meter {
                            label: None,
                            used: 10.0,
                            total,
                            unit: Unit::Dollars,
                            resets_at: None,
                        }],
                    }),
                    at: now_unix(),
                    started: std::time::Instant::now(),
                })
                .unwrap();
            app.drain();
            assert!(app.renewal_lookup.is_none());
            assert!(app.renewal_error.is_none());
            assert!(app.renewal_applied_at.is_none());
            assert!(app.slots[&Provider::Claude].cycle.is_none());
        }
    }

    #[test]
    fn completion_applies_date_and_refreshes_without_restart() {
        let stale_started = std::time::Instant::now();
        let (lookup_tx, lookup_rx) = mpsc::channel();
        let (mut app, updates, refresh_rx) = App::for_test(vec![Provider::Claude]);
        app.renewal_lookup = Some(renewal_lookup::Lookup::from_receiver(lookup_rx));
        lookup_tx
            .send(Ok(Cycle {
                date_only: false,
                verb: "renews".into(),
                at: 1_790_000_000,
            }))
            .unwrap();
        app.drain();
        assert!(app.renewal_lookup.is_none());
        assert_eq!(
            app.slots[&Provider::Claude].cycle.as_ref().unwrap().at,
            1_790_000_000
        );
        assert!(refresh_rx.try_recv().is_ok());

        updates
            .send(Update {
                provider: Provider::Claude,
                result: Ok(Usage {
                    plan: None,
                    cycle: None,
                    note: None,
                    estimated: vec![],
                    meters: vec![],
                }),
                at: now_unix(),
                started: stale_started,
            })
            .unwrap();
        app.drain();
        assert_eq!(
            app.slots[&Provider::Claude].cycle.as_ref().unwrap().at,
            1_790_000_000
        );

        let (lookup_tx, lookup_rx) = mpsc::channel();
        app.renewal_lookup = Some(renewal_lookup::Lookup::from_receiver(lookup_rx));
        lookup_tx.send(Err("Sign-in cancelled".into())).unwrap();
        app.drain();
        assert!(app.renewal_lookup.is_none());
        assert_eq!(app.renewal_error.as_deref(), Some("Sign-in cancelled"));
        assert!(refresh_rx.try_recv().is_err());
        assert_eq!(
            app.slots[&Provider::Claude].cycle.as_ref().unwrap().at,
            1_790_000_000
        );
    }
}

fn main() -> eframe::Result {
    match std::env::args().nth(1).as_deref() {
        Some("config") => {
            return match config::open(true) {
                Ok(path) => {
                    println!("{}", path.display());
                    Ok(())
                }
                Err(e) => finish(Err(e)),
            };
        }
        Some("--startup") => return finish(set_run_at_login(true)),
        Some("--no-startup") => return finish(set_run_at_login(false)),
        Some(flag) => {
            return finish(Err(format!(
                "unknown argument {flag}

usage: usage-widget [config | --startup | --no-startup]"
            )));
        }
        None => {}
    }

    let (config, config_error) = config::load();
    let options = eframe::NativeOptions {
        persist_window: true,
        viewport: ViewportBuilder::default()
            .with_app_id("usage-widget")
            .with_title("Usage")
            .with_inner_size([WIDTH, 180.0])
            .with_min_inner_size([WIDTH, 60.0])
            .with_decorations(false)
            .with_transparent(cfg!(target_os = "macos"))
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        // An accessory app: no Dock icon or Cmd+Tab entry; the menu bar icon
        // stands in for them.
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
        Box::new(move |cc| Ok(Box::new(App::new(cc, config, config_error)))),
    )
}

/// Whole-window opacity through a layered window. Per-pixel transparency is not
/// available with the OpenGL renderer on Windows, so this dims the whole card.
#[cfg(windows)]
fn set_opacity(hwnd: isize, percent: u32) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn SetLayeredWindowAttributes(hwnd: isize, key: u32, alpha: u8, flags: u32) -> i32;
    }
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_LAYERED: isize = 0x0008_0000;
    const LWA_ALPHA: u32 = 0x2;

    let percent = percent.clamp(20, 100);
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
        let wide = |s: &str| {
            s.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
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

#[cfg(test)]
mod display_tests {
    #[test]
    fn estimates_do_not_round_into_the_next_reported_percent() {
        for precision in [0, 1, 2] {
            assert_eq!(super::percent_label(87.99, precision, true), "~87.99%");
            assert_eq!(super::percent_label(87.999, precision, true), "~87.99%");
        }
        assert_eq!(super::percent_label(87.0, 0, true), "~87.00%");
        assert_eq!(super::percent_label(87.0, 0, false), "87%");
        assert_eq!(super::percent_label(87.999, 3, true), "~87.990%");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod menu_tests {
    use super::*;

    /// The commands eframe would pick up, collected the way it collects them while
    /// the widget is hidden and no egui pass runs.
    fn commands(
        ctx: &egui::Context,
        app: &mut App,
        action: menubar::Action,
    ) -> Vec<ViewportCommand> {
        ctx.run_logic(&egui::RawInput::default(), |ctx| {
            app.apply_menu_action(action, ctx);
        })
        .viewport_commands
        .get(&egui::ViewportId::ROOT)
        .cloned()
        .unwrap_or_default()
    }

    /// A hidden widget can be brought back: nothing but this runs while it is
    /// hidden, so the command has to come from the menu action itself.
    #[test]
    fn toggling_hides_the_widget_and_shows_it_again() {
        let ctx = egui::Context::default();
        let (mut app, _updates, _refresh_rx) = App::for_test(vec![Provider::Claude]);

        let hide = commands(&ctx, &mut app, menubar::Action::ToggleWidget);
        assert!(!app.shown);
        assert_eq!(hide, vec![ViewportCommand::Visible(false)]);

        let show = commands(&ctx, &mut app, menubar::Action::ToggleWidget);
        assert!(app.shown);
        assert_eq!(show, vec![ViewportCommand::Visible(true)]);
    }

    #[test]
    fn quitting_closes_the_window_even_while_hidden() {
        let ctx = egui::Context::default();
        let (mut app, _updates, _refresh_rx) = App::for_test(vec![Provider::Claude]);
        app.shown = false;

        let quit = commands(&ctx, &mut app, menubar::Action::Quit);

        assert_eq!(quit, vec![ViewportCommand::Close]);
    }

    #[test]
    fn refreshing_from_the_menu_asks_the_worker_for_new_numbers() {
        let ctx = egui::Context::default();
        let (mut app, _updates, refresh_rx) = App::for_test(vec![Provider::Claude]);

        commands(&ctx, &mut app, menubar::Action::Refresh);

        assert!(app.slots[&Provider::Claude].loading);
        assert!(refresh_rx.try_recv().is_ok());
    }
}
