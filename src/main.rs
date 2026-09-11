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
use timeutil::{ago, now_unix, until};

const WIDTH: f32 = 250.0;
const MARGIN: i8 = 12;
const DEFAULT_REFRESH_MINS: u64 = 5;

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
    chrome_applied: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let interval = Duration::from_secs(60 * refresh_minutes());
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        spawn_worker(cc.egui_ctx.clone(), tx, refresh_rx, interval);

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
            chrome_applied: false,
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

/// Windows 11: round the window corners and drop the 1px accent border, so the
/// frameless window looks like a floating card without needing transparency.
#[cfg(windows)]
fn apply_windows_chrome(frame: &eframe::Frame) {
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

#[cfg(not(windows))]
fn apply_windows_chrome(_frame: &eframe::Frame) {}

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

fn resets_line(ui: &mut egui::Ui, m: &Meter) {
    if let Some(ts) = m.resets_at {
        ui.label(
            RichText::new(format!("resets {}", until(ts)))
                .size(10.0)
                .color(MUTED),
        );
    }
}

fn provider_block(ui: &mut egui::Ui, p: Provider, slot: &Slot) {
    let single = slot
        .meters
        .as_ref()
        .filter(|m| m.len() == 1 && m[0].label.is_none())
        .map(|m| &m[0]);

    ui.horizontal(|ui| {
        let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
        ui.hyperlink_to(name, p.url()).on_hover_text(p.url());
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
            resets_line(ui, m);
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
                resets_line(ui, m);
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
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        if !self.chrome_applied {
            apply_windows_chrome(frame);
            self.chrome_applied = true;
        }
        self.drain();
        let ctx = root.ctx().clone();
        ctx.request_repaint_after(Duration::from_secs(30));

        let panel = egui::Frame::NONE
            .fill(BG)
            .inner_margin(Margin::same(MARGIN));

        let mut refresh = false;
        let mut quit = false;

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
                for p in Provider::ALL {
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
                    .size(10.0)
                    .color(MUTED),
                );
                if ui.button("Quit").clicked() {
                    quit = true;
                    ui.close();
                }
            });

            ui.spacing_mut().item_spacing.y = 3.0;

            // Wrap the content so its real height can be measured: the panel's own
            // min_rect is always expanded to fill the window.
            let content = ui.vertical(|ui| {
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
            });

            // Grow or shrink the window to fit the content.
            let wanted = content.response.rect.height() + 2.0 * MARGIN as f32;
            let current = ctx.viewport_rect().height();
            if (wanted - current).abs() > 1.5 {
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(WIDTH, wanted)));
            }
        });

        if refresh {
            self.refresh_now();
        }
        if quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        persist_window: true,
        viewport: ViewportBuilder::default()
            .with_app_id("usage-widget")
            .with_title("Usage")
            .with_inner_size([WIDTH, 180.0])
            .with_min_inner_size([WIDTH, 60.0])
            .with_decorations(false)
            .with_always_on_top()
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "usage-widget",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
