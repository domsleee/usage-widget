//! A tiny always-on-top, frameless, draggable widget that shows Copilot,
//! Claude and Codex usage in dollars, refreshed every few minutes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod claude_estimate;
mod claude_web;
mod codex_estimate;
mod config;
mod estimate;
mod estimate_history;
mod limits;
#[cfg(target_os = "macos")]
mod menubar;
mod paths;
mod providers;
mod renewal_lookup;
#[cfg(target_os = "macos")]
mod renewal_webview;
mod spend;
mod timeutil;
mod update;

use config::Config;
use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, PointerButton, Pos2, RichText, Sense,
    Shape, Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use providers::{Cycle, FetchError, Meter, Problem, Provider, Unit, Usage, money, spend_meter};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use timeutil::{
    ago, local_datetime, local_when, now_unix, reset_date_time, until_next_minute, window_until,
};

const WIDTH: f32 = 250.0;
const MARGIN: i8 = 12;
const MINI_MARGIN_X: i8 = 10;
const MINI_MARGIN_Y: i8 = 6;
const MINIMIZED_KEY: &str = "minimized";
const RATIOS_KEY: &str = "ratios";
const HIDDEN_KEY: &str = "hidden";
const SHOWN_KEY: &str = "shown";
const SWITCH_SECS: f32 = 0.35;
const FADE: f32 = 0.65;
/// Size presets in the right-click menu, applied as egui's zoom factor on top of
/// the monitor's display scaling. Ctrl +/- also works; either way it persists.
const SIZES: [f32; 8] = [0.25, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0];

// Windows rounds the opaque window at the compositor level; macOS uses a
// rounded panel over a transparent window.
const BG: Color32 = Color32::from_rgb(24, 26, 32);
const TEXT: Color32 = Color32::from_gray(232);
const META: Color32 = Color32::from_rgb(150, 155, 165);
const MUTED: Color32 = Color32::from_gray(135);
const LIGHT: Color32 = Color32::from_gray(175);
const TRACK: Color32 = Color32::from_gray(58);
const ERR: Color32 = Color32::from_rgb(235, 110, 110);
/// Background of a hover card, a step lighter than the widget so it reads as raised.
const CARD: Color32 = Color32::from_rgb(38, 41, 50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshRequest {
    All,
    One(Provider),
}

struct Update {
    provider: Provider,
    result: Result<Usage, FetchError>,
    at: i64,
    started: std::time::Instant,
}

#[derive(Clone, Default)]
struct Slot {
    personal: bool,
    plan: Option<String>,
    cycle: Option<Cycle>,
    note: Option<String>,
    estimated: Vec<String>,
    api_spend: Option<f64>,
    meters: Option<Vec<Meter>>,
    error: Option<FetchError>,
    updated: Option<i64>,
    loading: bool,
}

impl Slot {
    fn visible(&self) -> bool {
        let absent = self
            .error
            .as_ref()
            .is_some_and(|e| e.problem == Problem::NotAvailable);
        !absent && (self.meters.is_some() || self.error.is_some())
    }

    /// True when Antigravity has no usable login yet (missing, expired, or
    /// rejected, with no prior quota reading).
    fn antigravity_unauthenticated(&self) -> bool {
        self.meters.is_none()
            && self.error.as_ref().is_none_or(|e| {
                matches!(
                    e.problem,
                    Problem::NotAvailable | Problem::TokenExpired | Problem::TokenRejected
                )
            })
    }

    /// Primary windows share the header; scoped caps and spend stay in the tooltip.
    fn windows(&self, provider: Provider) -> Vec<&Meter> {
        if !self.personal {
            return Vec::new();
        }
        let meters = self
            .meters
            .iter()
            .flatten()
            .filter(|m| m.unit == Unit::Percent && m.total > 0.0);
        if provider == Provider::Antigravity {
            return antigravity_windows(meters.collect());
        }
        meters
            .filter(|m| matches!(m.label.as_deref(), Some("5h" | "week" | "wk" | "mo")))
            .collect()
    }

    fn estimated(&self, m: &Meter) -> bool {
        m.label
            .as_ref()
            .is_some_and(|label| self.estimated.contains(label))
    }
}

struct App {
    providers: Vec<Provider>,
    slots: HashMap<Provider, Slot>,
    rx: Receiver<Update>,
    refresh_tx: Sender<RefreshRequest>,
    /// False until the window has been placed on screen and made topmost (see `settle_window`).
    settled: bool,
    /// Compact single-line view. Persisted, so the widget reopens the way it was left.
    minimized: bool,
    logos: HashMap<Provider, egui::TextureHandle>,
    ratios: HashMap<String, limits::Ratio>,
    hidden: HashSet<String>,
    /// Providers explicitly ticked in Show even when hidden by default (such as
    /// Antigravity with no valid login).
    force_shown: HashSet<String>,
    restart: Arc<AtomicBool>,
    /// Services whose token is being refreshed, so a second click does not
    /// start another run.
    refreshing: Arc<Mutex<HashSet<Provider>>>,
    opacity: u32,
    format: TextFormat,
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
        let providers: Vec<Provider> = Provider::ALL
            .into_iter()
            .filter(|p| config.enabled(*p))
            .collect();
        let opacity = std::env::var("USAGE_WIDGET_OPACITY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(config.opacity);
        let format = TextFormat::from(&config);
        let restart = Arc::new(AtomicBool::new(false));
        if update::auto_enabled() {
            spawn_auto_update(cc.egui_ctx.clone(), restart.clone());
        }
        let (tx, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        spawn_worker(
            cc.egui_ctx.clone(),
            tx,
            refresh_rx,
            providers.clone(),
            config,
        );
        cc.egui_ctx.all_styles_mut(widget_style);
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
        let force_shown = cc
            .storage
            .and_then(|s| eframe::get_value(s, SHOWN_KEY))
            .unwrap_or_default();

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
            settled: false,
            minimized,
            logos: load_logos(&cc.egui_ctx),
            ratios,
            hidden,
            force_shown,
            restart,
            refreshing: Arc::default(),
            opacity,
            format,
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
    fn for_test(providers: Vec<Provider>) -> (Self, Sender<Update>, Receiver<RefreshRequest>) {
        let (updates, rx) = mpsc::channel();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let slots = providers.iter().map(|p| (*p, Slot::default())).collect();
        let app = Self {
            providers,
            slots,
            rx,
            refresh_tx,
            ratios: HashMap::new(),
            hidden: HashSet::new(),
            force_shown: HashSet::new(),
            restart: Arc::new(AtomicBool::new(false)),
            refreshing: Arc::default(),
            opacity: 100,
            format: TextFormat::default(),
            config_error: None,
            renewal_lookup: None,
            renewal_error: None,
            renewal_applied_at: None,
            settled: true,
            minimized: false,
            logos: HashMap::new(),
            #[cfg(target_os = "macos")]
            menubar: None,
            #[cfg(target_os = "macos")]
            shown: true,
        };
        (app, updates, refresh_rx)
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
        let _ = self.refresh_tx.send(RefreshRequest::All);
    }

    /// Refreshes a single service; runs its CLI token refresh first when supported.
    fn refresh_provider(&mut self, p: Provider) {
        if p.refreshes_token() {
            self.refresh_token(p);
        } else {
            if let Some(slot) = self.slots.get_mut(&p) {
                slot.loading = true;
            }
            let _ = self.refresh_tx.send(RefreshRequest::One(p));
        }
    }

    /// Has the service's CLI refresh its token in the background, then refreshes
    /// that service, reporting a failed refresh.
    fn refresh_token(&mut self, p: Provider) {
        let refreshing = self.refreshing.clone();
        if !refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(p)
        {
            return;
        }
        if let Some(slot) = self.slots.get_mut(&p) {
            slot.loading = true;
        }
        let refresh_tx = self.refresh_tx.clone();
        std::thread::spawn(move || {
            let result = p.refresh_token();
            refreshing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&p);
            let _ = refresh_tx.send(RefreshRequest::One(p));
            if let Err(e) = result {
                message_box(
                    &format!("Could not refresh the {} token: {e}", p.name()),
                    true,
                );
            }
        });
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
                if self.shown {
                    self.settled = false;
                }
                if let Some(bar) = &self.menubar {
                    bar.set_widget_shown(self.shown);
                }
            }
            menubar::Action::Refresh => self.refresh_now(),
            menubar::Action::Open(url) => open_url(url),
            menubar::Action::EditConfig => {
                if let Err(e) = config::open(false) {
                    self.config_error = Some(e);
                }
            }
            menubar::Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
            menubar::Action::Widget(action) => self.apply_action(action, ctx),
        }
    }

    /// A choice from the right-click menu.
    fn apply_action(&mut self, action: MenuAction, ctx: &egui::Context) {
        match action {
            MenuAction::Refresh => self.refresh_now(),
            MenuAction::RefreshProvider(p) => self.refresh_provider(p),
            MenuAction::ToggleMinimized => self.minimized = !self.minimized,
            MenuAction::Open(url) => open_url(url),
            MenuAction::Toggle(p) => self.toggle(p),
            MenuAction::CheckForUpdates => self.check_for_updates(ctx),
            MenuAction::Size(z) => ctx.set_zoom_factor(z),
            MenuAction::EditConfig => {
                if let Err(e) = config::open(false) {
                    self.config_error = Some(e);
                }
            }
            MenuAction::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn drain(&mut self) {
        while let Ok(u) = self.rx.try_recv() {
            let slot = self.slots.entry(u.provider).or_default();
            slot.loading = false;
            match u.result {
                Ok(usage) => {
                    slot.updated = Some(u.at);
                    if slot.plan.is_some() && slot.plan != usage.plan {
                        self.ratios.remove(u.provider.name());
                    }
                    if usage.personal {
                        let five = usage
                            .meters
                            .iter()
                            .find(|m| m.label.as_deref() == Some("5h"));
                        let week = usage
                            .meters
                            .iter()
                            .find(|m| matches!(m.label.as_deref(), Some("week" | "wk")));
                        if let (Some(five), Some(week)) = (five, week) {
                            let mut five = five.clone();
                            let mut week = week.clone();
                            for m in [&mut five, &mut week] {
                                if m.label
                                    .as_ref()
                                    .is_some_and(|l| usage.estimated.contains(l))
                                {
                                    m.used = m.used.floor();
                                }
                            }
                            self.ratios
                                .entry(u.provider.name().to_string())
                                .or_default()
                                .observe(&five, &week);
                        }
                    }
                    slot.personal = usage.personal;
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
                    slot.api_spend = usage.api_spend;
                    slot.meters = Some(usage.meters);
                    slot.error = None;
                }
                Err(e) => {
                    if e.problem == Problem::NotAvailable || slot.meters.is_none() {
                        slot.meters = None;
                        slot.updated = Some(u.at);
                    }
                    slot.error = Some(e);
                }
            }
        }
        if let Some(rx) = &mut self.renewal_lookup {
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

    /// For a personal plan with 5-hour and weekly windows, the one that runs out
    /// first this session (0 or 1).
    fn binding(&self, p: Provider) -> Option<usize> {
        let slot = self.slots.get(&p)?;
        let windows = slot.windows(p);
        let [five_hour, week] = windows.as_slice() else {
            return None;
        };
        if p == Provider::Antigravity {
            return (header_period(five_hour) == Some("5h") && header_period(week) == Some("wk"))
                .then(|| limits::binding(five_hour, week, None));
        }
        if five_hour.label.as_deref() != Some("5h")
            || !matches!(week.label.as_deref(), Some("week" | "wk"))
        {
            return None;
        }
        let k = self
            .ratios
            .get(p.name())
            .and_then(limits::Ratio::weekly_per_five_hour);
        Some(limits::binding(five_hour, week, k))
    }

    /// Services with an active account or reading, in display order.
    fn found(&self) -> Vec<Provider> {
        self.providers
            .iter()
            .copied()
            .filter(|p| {
                self.slots.get(p).is_some_and(|s| {
                    s.visible() && (*p != Provider::Antigravity || !s.antigravity_unauthenticated())
                })
            })
            .collect()
    }

    /// Services listed under Show: every found service, plus Antigravity once
    /// checked so it can be toggled on even without a valid login.
    fn available(&self) -> Vec<Provider> {
        self.providers
            .iter()
            .copied()
            .filter(|p| {
                self.slots.get(p).is_some_and(|s| {
                    s.visible()
                        || (*p == Provider::Antigravity
                            && (s.updated.is_some() || s.error.is_some()))
                })
            })
            .collect()
    }

    /// Services currently visible in the widget.
    fn shown(&self) -> Vec<Provider> {
        self.available()
            .into_iter()
            .filter(|&p| self.is_shown(p))
            .collect()
    }

    fn is_shown(&self, p: Provider) -> bool {
        let Some(slot) = self.slots.get(&p) else {
            return false;
        };
        if p == Provider::Antigravity && slot.antigravity_unauthenticated() {
            (slot.updated.is_some() || slot.error.is_some()) && self.force_shown.contains(p.name())
        } else {
            slot.visible() && !self.hidden.contains(p.name())
        }
    }

    /// What the widget says when it has no service to show, in the full and the
    /// minimized view.
    fn empty_text(&self, found: &[Provider]) -> (&'static str, &'static str) {
        if self.providers.is_empty() {
            ("Every service is disabled in the config.", "All disabled")
        } else if !found.is_empty() {
            (
                "All services are hidden. Right-click to show them.",
                "All hidden",
            )
        } else if self.all_fetched() {
            (
                "No usage found. Sign in with gh, claude, codex or agy and refresh.",
                "No plans found",
            )
        } else {
            ("…", "…")
        }
    }

    fn toggle(&mut self, p: Provider) {
        let antigravity_unauth = p == Provider::Antigravity
            && self
                .slots
                .get(&p)
                .is_some_and(Slot::antigravity_unauthenticated);
        if antigravity_unauth {
            if !self.force_shown.remove(p.name()) {
                self.hidden.remove(p.name());
                self.force_shown.insert(p.name().to_string());
            }
        } else {
            self.force_shown.remove(p.name());
            if !self.hidden.remove(p.name()) {
                self.hidden.insert(p.name().to_string());
            }
        }
    }

    fn all_fetched(&self) -> bool {
        self.slots.values().all(|s| s.updated.is_some())
    }

    fn last_updated(&self) -> Option<i64> {
        self.slots.values().filter_map(|s| s.updated).max()
    }

    /// Fit the readings and reset label; status icons use the existing logo space.
    fn card_width(&self, ctx: &egui::Context, shown: &[Provider]) -> f32 {
        let gap = self.format.header.spacing_size;
        ctx.fonts_mut(|fonts| {
            let mut text_width = |text: String, size| {
                fonts
                    .layout_no_wrap(text, egui::FontId::proportional(size), TEXT)
                    .size()
                    .x
            };
            shown
                .iter()
                .filter_map(|p| {
                    let slot = self.slots.get(p)?;
                    let windows = slot.windows(*p);
                    let mut width = 13.0 + gap + text_width(p.name().into(), 13.0);
                    if let Some(reset) = windows
                        .get(self.binding(*p).unwrap_or(0))
                        .and_then(|m| self.format.reset_label(m))
                    {
                        width += gap + text_width(reset, 11.0);
                    }
                    if !windows.is_empty() {
                        width += gap;
                        for (i, m) in windows.iter().enumerate() {
                            if i > 0 {
                                width += 9.0 + text_width("|".into(), 12.0);
                            }
                            width += text_width(
                                percent_label(
                                    m.percent(),
                                    self.format.precision,
                                    slot.estimated(m),
                                ),
                                12.0,
                            );
                            if let Some(period) = header_period(m) {
                                width += 3.0 + text_width(period.into(), 12.0);
                            }
                        }
                    } else if let Some(m) = slot.meters.as_deref().and_then(spend_meter) {
                        width += gap + text_width(m.summary(), 12.0);
                        if m.total > 0.0 {
                            width += gap
                                + text_width(
                                    percent_label(m.percent(), self.format.precision, false),
                                    12.0,
                                );
                        }
                    }
                    if let Some(e) = &slot.error {
                        width += gap + text_width(e.problem.label().into(), 12.0);
                    }
                    Some(width + 2.0 * f32::from(MARGIN) + 4.0)
                })
                .fold(WIDTH, f32::max)
        })
    }

    /// Sum of every dollar-denominated meter across the shown providers. Only when
    /// at least two have one, since otherwise it repeats that provider's line.
    fn total(&self, shown: &[Provider]) -> Option<Meter> {
        let mut used = 0.0;
        let mut total = 0.0;
        let mut providers = 0;
        for slot in shown
            .iter()
            .filter_map(|p| self.slots.get(p))
            .filter(|s| !s.personal)
        {
            let mut any = false;
            for m in slot.meters.iter().flatten() {
                if m.unit == Unit::Dollars {
                    used += m.used;
                    total += m.total;
                    any = true;
                }
            }
            providers += usize::from(any);
        }
        (providers >= 2).then_some(Meter {
            label: None,
            used,
            total,
            unit: Unit::Dollars,
            resets_at: None,
        })
    }
}

/// Resolves an initial wake-up and any queued refresh requests into the
/// providers to fetch and whether the periodic refresh timer should reset.
fn collect_refresh_targets(
    first: RefreshRequest,
    queued: impl IntoIterator<Item = RefreshRequest>,
    providers: &[Provider],
) -> (Vec<Provider>, bool) {
    let mut all = false;
    let mut selected = Vec::new();
    for req in std::iter::once(first).chain(queued) {
        match req {
            RefreshRequest::All => all = true,
            RefreshRequest::One(p) if providers.contains(&p) && !selected.contains(&p) => {
                selected.push(p);
            }
            RefreshRequest::One(_) => {}
        }
    }
    if all {
        (providers.to_vec(), true)
    } else {
        (selected, false)
    }
}

fn spawn_worker(
    ctx: egui::Context,
    tx: Sender<Update>,
    refresh_rx: Receiver<RefreshRequest>,
    providers: Vec<Provider>,
    config: Config,
) {
    let interval = config.refresh_interval();
    std::thread::spawn(move || {
        let mut config = config;
        let mut targets = providers.clone();
        let mut full_refresh = true;
        let mut next_auto = std::time::Instant::now() + interval;
        loop {
            if !targets.is_empty() {
                let started = std::time::Instant::now();
                // The browser helper updates this setting while the widget is open.
                // A broken file loads as defaults, without a date; a warning such as
                // an unknown key still has the real one.
                if targets.contains(&Provider::Claude) {
                    let (latest, error) = config::load();
                    if error.is_none() || latest.claude.renewal_date.is_some() {
                        config.claude.renewal_date = latest.claude.renewal_date;
                    }
                }
                std::thread::scope(|s| {
                    for &p in &targets {
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
            }
            if full_refresh {
                next_auto = std::time::Instant::now() + interval;
            }
            let wait = next_auto.saturating_duration_since(std::time::Instant::now());
            match refresh_rx.recv_timeout(wait) {
                Ok(first) => {
                    let queued = std::iter::from_fn(|| refresh_rx.try_recv().ok());
                    (targets, full_refresh) = collect_refresh_targets(first, queued, &providers);
                }
                Err(RecvTimeoutError::Timeout) => {
                    targets = providers.clone();
                    full_refresh = true;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
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

/// Windows 11: round the window corners and drop the accent border, so the
/// frameless window looks like a floating card, and apply the opacity.
/// Also puts the window back on top if something has dropped its topmost style.
#[cfg(windows)]
fn apply_window_style(frame: &eframe::Frame, opacity: u32) {
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
    set_opacity(hwnd, opacity);
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

const ESTIMATE_HOVER: &str = "Estimated from this computer's CLI token logs between reported \
     percentage ticks. Web, cloud and other devices aren't counted.";

/// The short form for a hover card, which has no room for the full explanation.
const ESTIMATE_NOTE: &str = "~ estimated from this computer's CLI logs";

/// The percentage, coloured by level, with "~" when it includes a local estimate.
fn percent_text(m: &Meter, size: f32, precision: usize, estimated: bool) -> RichText {
    let pct = m.percent();
    RichText::new(percent_label(pct, precision, estimated))
        .size(size)
        .strong()
        .color(level_color(pct))
}

fn percent_label(pct: f64, precision: usize, estimated: bool) -> String {
    // Estimates always show at least two decimals, even when readings are shown
    // as whole percents: the decimals are the estimate.
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
                ui.label(RichText::new(m.summary()).size(size - 1.0).color(TEXT));
            }
        } else {
            ui.label(RichText::new("unlimited").size(size).color(MUTED));
        }
    });
}

/// The service name opens its usage page; plan and renewal details stay on hover.
fn name_link(ui: &mut egui::Ui, p: Provider) -> egui::Response {
    let name = RichText::new(p.name()).size(13.0).strong().color(TEXT);
    let Some(url) = p.usage_url() else {
        return ui.label(name);
    };
    let response = ui
        .add(egui::Label::new(name).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        open_url(url);
    }
    response
}

/// Whether the primary button went down on the background this pass, and not on
/// a button or link drawn over it. The background is still the drag target under
/// those, so its own response can't tell them apart.
fn pressed_on_background(ctx: &egui::Context, bg: &egui::Response) -> bool {
    if !bg.is_pointer_button_down_on() || !ctx.input(|i| i.pointer.primary_pressed()) {
        return false;
    }
    let others: Vec<egui::Id> = ctx.interaction_snapshot(|s| {
        s.hovered
            .iter()
            .copied()
            .filter(|&id| id != bg.id)
            .collect()
    });
    !others.into_iter().any(|id| {
        ctx.read_response(id)
            .is_some_and(|r| r.sense.senses_click())
    })
}

/// The widget's style on top of egui's default.
fn widget_style(style: &mut egui::Style) {
    // Selectable labels grab clicks and drags, so right-click and drag-to-move
    // would not work over text.
    style.interaction.selectable_labels = false;
    hover_style(style);
}

/// Hover cards are clipped to this small window and sit on top of the readings,
/// so they are drawn small and flat, in the widget's own colours.
fn hover_style(style: &mut egui::Style) {
    style.visuals.window_fill = CARD;
    style.visuals.window_stroke = Stroke::new(1.0, TRACK);
    style.visuals.popup_shadow = egui::Shadow::NONE;
    style.visuals.menu_corner_radius = CornerRadius::same(4);
    style.visuals.widgets.noninteractive.fg_stroke.color = LIGHT;
    style.spacing.menu_margin = Margin::symmetric(7, 5);
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::proportional(HOVER_SIZE),
    );
}

const HOVER_SIZE: f32 = 10.5;

/// "Claude Pro ($12.34 API spend this week)": the service, its plan, and what
/// this computer's CLI use in the weekly window would have cost at API prices.
fn plan_summary(p: Provider, slot: &Slot) -> String {
    let mut text = p.name().to_string();
    if let Some(plan) = &slot.plan {
        text.push(' ');
        text.push_str(plan);
    }
    if let Some(spend) = slot.api_spend {
        text.push_str(&format!(" (${spend:.2} API spend this week)"));
    }
    text
}

/// What hovering a service in the minimized row shows, where nothing else is on
/// screen: the renewal and every meter with its reset time.
#[derive(Default)]
struct HoverCard {
    cycle: Option<String>,
    rows: Vec<HoverRow>,
    notes: Vec<String>,
    error: Option<String>,
}

struct HoverRow {
    label: String,
    amount: String,
    color: Color32,
    reset: Option<String>,
}

impl HoverCard {
    fn show(&self, ui: &mut egui::Ui, heading: &str) {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 2.0);
        let text = |s: &str, color| RichText::new(s).size(HOVER_SIZE).color(color);
        ui.label(RichText::new(heading).size(11.5).strong().color(TEXT));
        if let Some(cycle) = &self.cycle {
            ui.label(text(cycle, META));
        }
        if !self.rows.is_empty() {
            egui::Grid::new("meters")
                .spacing([8.0, 2.0])
                .min_col_width(0.0)
                .min_row_height(0.0)
                .show(ui, |ui| {
                    for row in &self.rows {
                        ui.label(text(&row.label, MUTED));
                        ui.label(text(&row.amount, row.color).strong());
                        if let Some(reset) = &row.reset {
                            ui.label(text(reset, LIGHT));
                        }
                        ui.end_row();
                    }
                });
        }
        for note in &self.notes {
            ui.label(text(note, META));
        }
        if let Some(error) = &self.error {
            ui.label(text(error, ERR));
        }
    }
}

fn usage_hover(slot: &Slot, precision: usize) -> HoverCard {
    let rows = slot
        .meters
        .iter()
        .flatten()
        .map(|m| {
            let (amount, color) = if m.total <= 0.0 {
                ("unlimited".into(), MUTED)
            } else if m.unit == Unit::Dollars {
                (m.summary(), TEXT)
            } else {
                (
                    percent_label(m.percent(), precision, slot.estimated(m)),
                    level_color(m.percent()),
                )
            };
            HoverRow {
                label: m.label.clone().unwrap_or_else(|| "usage".into()),
                amount,
                color,
                reset: m.resets_at.map(|at| format!("resets {}", local_when(at))),
            }
        })
        .collect();
    let mut notes: Vec<String> = slot.note.iter().cloned().collect();
    if !slot.estimated.is_empty() {
        notes.push(ESTIMATE_NOTE.into());
    }
    HoverCard {
        cycle: slot.cycle.as_ref().map(|cycle| {
            format!(
                "{} {} ({})",
                cycle.verb,
                cycle.countdown(chrono::Local::now().date_naive()),
                cycle.when()
            )
        }),
        rows,
        notes,
        error: slot.error.as_ref().map(|e| e.detail.clone()),
    }
}

fn antigravity_windows(meters: Vec<&Meter>) -> Vec<&Meter> {
    fn group(m: &Meter) -> Option<&str> {
        m.label
            .as_deref()
            .and_then(|l| l.rsplit_once(' '))
            .map(|(g, _)| g)
    }
    fn fullest<'a>(a: &'a Meter, b: &'a Meter) -> &'a Meter {
        if b.percent() > a.percent() { b } else { a }
    }
    let paired = |m: &&Meter| matches!(header_period(m), Some("5h" | "wk"));
    let Some(busiest) = meters.iter().copied().reduce(fullest) else {
        return Vec::new();
    };
    if !paired(&busiest) {
        return vec![busiest];
    }
    let in_group = |g| {
        meters
            .iter()
            .copied()
            .filter(paired)
            .filter(move |m| group(m) == g)
    };
    let chosen = meters
        .iter()
        .copied()
        .filter(paired)
        .filter(|m| m.percent() == busiest.percent())
        .map(group)
        .find(|&g| {
            let periods: Vec<_> = in_group(g).map(header_period).collect();
            periods.contains(&Some("5h")) && periods.contains(&Some("wk"))
        })
        .unwrap_or_else(|| group(busiest));
    ["5h", "wk"]
        .into_iter()
        .filter_map(|period| {
            in_group(chosen)
                .filter(|m| header_period(m) == Some(period))
                .reduce(fullest)
        })
        .collect()
}

fn header_period(m: &Meter) -> Option<&'static str> {
    let label = m.label.as_deref()?;
    let label = label.strip_suffix(" Limit").unwrap_or(label);
    if label.ends_with("Five Hour") {
        return Some("5h");
    }
    match label.split_whitespace().next_back()? {
        "weekly" | "Weekly" | "week" | "wk" => Some("wk"),
        "5h" => Some("5h"),
        "mo" | "monthly" | "Monthly" => Some("mo"),
        _ => None,
    }
}

fn reset_tooltip(m: &Meter) -> Option<String> {
    let at = m.resets_at?;
    if header_period(m) == Some("5h") {
        return Some(format!("Resets in {}", window_until(at)));
    }
    Some(format!("Resets on {}", local_datetime(at)?))
}

/// Primary windows share one header, fading towards the window that binds first.
fn windows(ui: &mut egui::Ui, meters: &[&Meter], weights: &[f32], slot: &Slot, precision: usize) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        for (i, (m, &weight)) in meters.iter().zip(weights).rev().enumerate() {
            if i > 0 {
                ui.label(RichText::new("|").size(12.0).color(MUTED));
            }
            if let Some(label) = header_period(m) {
                let label_response = ui.label(
                    RichText::new(label)
                        .size(12.0)
                        .color(weighted(LIGHT, weight)),
                );
                if let Some(text) = reset_tooltip(m) {
                    label_response.on_hover_text(text);
                }
            }
            let response = ui.label(
                percent_text(m, 12.0, precision, slot.estimated(m))
                    .color(weighted(level_color(m.percent()), weight)),
            );
            if let Some(text) = reset_tooltip(m) {
                response.clone().on_hover_text(text);
            }
            if slot.estimated(m) {
                response.on_hover_text(ESTIMATE_HOVER);
            }
        }
    });
}

// Retain upstream's animation when the limiting window changes.
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

/// How readings are written out.
#[derive(Clone, Copy, Default)]
struct TextFormat {
    /// Decimal places for percentages.
    precision: usize,
    /// Show the weekly window's reset as a date and time, not a countdown.
    weekly_reset_date: bool,
    header: config::Header,
}

impl From<&Config> for TextFormat {
    fn from(config: &Config) -> Self {
        Self {
            precision: config.precision.min(3),
            weekly_reset_date: config.weekly_reset_date,
            header: config.header,
        }
    }
}

impl TextFormat {
    fn reset_label(self, meter: &Meter) -> Option<String> {
        let at = meter.resets_at?;
        let reset = if self.weekly_reset_date && header_period(meter) == Some("wk") {
            reset_date_time(at, self.header.clock_24h)
        } else {
            window_until(at)
        };
        Some(if self.header.reset_parentheses {
            format!("({reset})")
        } else {
            reset
        })
    }
}

fn provider_block(
    ui: &mut egui::Ui,
    p: Provider,
    slot: &Slot,
    format: TextFormat,
    renewal: &mut RenewalButton<'_>,
    logos: &HashMap<Provider, egui::TextureHandle>,
    binding: Option<usize>,
) {
    let precision = format.precision;
    let meters = slot.meters.as_deref().unwrap_or_default();
    let personal = slot.windows(p);
    let single = (!slot.personal).then(|| spend_meter(meters)).flatten();
    let selected = personal.get(binding.unwrap_or(0)).copied();
    let position = binding.map(|b| ease(ui, egui::Id::new(("binding", p.name())), [b as f32])[0]);
    let weights = position.map_or_else(|| vec![1.0; personal.len()], |t| vec![1.0 - t, t]);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = format.header.spacing_size;
        if renewal.available(p, slot) {
            renewal.show(ui, p, slot);
        } else if slot.loading {
            ui.add(egui::Spinner::new().size(13.0).color(MUTED));
        } else {
            provider_logo(ui, p, logos);
        }
        let response = name_link(ui, p);
        if p == Provider::Claude {
            renewal.context_menu(&response);
        }
        if p == Provider::Antigravity {
            response.on_hover_ui(|ui| usage_hover(slot, precision).show(ui, p.name()));
        } else {
            response.on_hover_text(plan_summary(p, slot));
        }
        if let Some(meter) = selected
            && let Some(reset) = format.reset_label(meter)
        {
            let countdown = ui.label(RichText::new(reset).size(11.0).color(LIGHT));
            if let Some(text) = reset_tooltip(meter) {
                countdown.on_hover_text(text);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(error) = &slot.error {
                let color = if error.problem == Problem::NotAvailable {
                    MUTED
                } else {
                    ERR
                };
                ui.label(RichText::new(error.problem.label()).size(12.0).color(color))
                    .on_hover_text(&error.detail);
            }
            if !personal.is_empty() {
                windows(ui, &personal, &weights, slot, precision);
            } else if let Some(m) = single {
                amounts(ui, m, 12.0, precision, false);
            }
        });
    });

    if slot.meters.is_none() {
        bar(ui, 0.0, MUTED);
        return;
    }
    if let Some(m) = selected {
        let color = level_color(m.percent());
        let target = [
            m.fraction(),
            color.r().into(),
            color.g().into(),
            color.b().into(),
        ];
        let [fraction, r, g, b] = ease(ui, egui::Id::new(("bar", p.name())), target);
        bar(ui, fraction, Color32::from_rgb(r as u8, g as u8, b as u8));
    } else if let Some(m) = single {
        if m.total > 0.0 {
            bar(ui, m.fraction(), level_color(m.percent()));
        }
    } else {
        for m in meters {
            ui.horizontal(|ui| {
                if let Some(label) = &m.label {
                    ui.label(RichText::new(label).size(11.0).color(MUTED));
                }
                amounts(ui, m, 12.0, precision, slot.estimated(m));
            });
            if m.total > 0.0 {
                bar(ui, m.fraction(), level_color(m.percent()));
            }
        }
    }
    if let Some(note) = &slot.note {
        ui.label(RichText::new(note).size(10.0).color(META));
    }
}

#[cfg(target_os = "macos")]
const RENEWAL_SIGN_IN: &str = "complete sign-in in the Claude window";
#[cfg(not(target_os = "macos"))]
const RENEWAL_SIGN_IN: &str = "complete sign-in in Chrome";
#[cfg(target_os = "macos")]
const RENEWAL_LOOKUP_LABEL: &str = "Look up Claude renewal";
#[cfg(not(target_os = "macos"))]
const RENEWAL_LOOKUP_LABEL: &str = "Look up Claude renewal in Chrome";
#[cfg(target_os = "macos")]
const RENEWAL_SIGN_IN_LABEL: &str = "Complete sign-in in the Claude window";
#[cfg(not(target_os = "macos"))]
const RENEWAL_SIGN_IN_LABEL: &str = "Complete sign-in in Chrome";

struct RenewalButton<'a> {
    running: bool,
    error: Option<&'a str>,
    clicked: bool,
    cancelled: bool,
    raise_clicked: bool,
}

impl RenewalButton<'_> {
    fn available(&self, provider: Provider, slot: &Slot) -> bool {
        provider == Provider::Claude
            && slot.cycle.is_none()
            && slot.meters.as_deref().and_then(spend_meter).is_none()
    }

    fn show(&mut self, ui: &mut egui::Ui, provider: Provider, slot: &Slot) {
        if !self.available(provider, slot) {
            return;
        }
        if self.running {
            let spinner = ui.add(egui::Spinner::new().size(13.0).color(MUTED));
            let response =
                ui.interact(spinner.rect, ui.id().with("cancel-renewal"), Sense::click());
            self.context_menu(&response);
            response.on_hover_text(format!(
                "Looking up renewal — {RENEWAL_SIGN_IN}. Right-click for options."
            ));
        } else {
            let tooltip = self.error.map_or_else(
                || RENEWAL_LOOKUP_LABEL.to_string(),
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
                ui.label(RENEWAL_SIGN_IN_LABEL);
                if ui.button("Bring sign-in window to front").clicked() {
                    self.raise_clicked = true;
                    ui.close();
                }
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuAction {
    Refresh,
    RefreshProvider(Provider),
    ToggleMinimized,
    Open(&'static str),
    Toggle(Provider),
    Size(f32),
    CheckForUpdates,
    EditConfig,
    Quit,
}

fn minimize_label(minimized: bool) -> &'static str {
    if minimized { "Maximize" } else { "Minimize" }
}

#[cfg(windows)]
mod win_menu {
    use super::{MenuAction, Provider, SIZES, minimize_label, update, wide};

    pub(super) const ID_REFRESH: usize = 1;
    pub(super) const ID_QUIT: usize = 2;
    pub(super) const ID_MINIMIZE: usize = 3;
    pub(super) const ID_UPDATE: usize = 4;
    pub(super) const ID_EDIT_CONFIG: usize = 5;
    pub(super) const ID_OPEN: usize = 10;
    pub(super) const ID_TOGGLE: usize = 20;
    pub(super) const ID_REFRESH_PROVIDER: usize = 30;
    pub(super) const ID_SIZE: usize = 100;

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
        pub(super) fn DestroyMenu(menu: isize) -> i32;
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

    pub(super) unsafe fn build(
        available: &[Provider],
        shown: &[Provider],
        zoom: f32,
        minimized: bool,
    ) -> isize {
        unsafe {
            let add = |menu: isize, flags: u32, id: usize, text: &str| {
                AppendMenuW(menu, flags, id, wide(text).as_ptr());
            };
            let menu = CreatePopupMenu();
            // Submenus are owned by `menu` once appended, so destroyed along with it.
            let refresh = CreatePopupMenu();
            add(refresh, MF_STRING, ID_REFRESH, "All");
            if !shown.is_empty() {
                add(refresh, MF_SEPARATOR, 0, "");
                for (i, p) in Provider::ALL.iter().enumerate() {
                    if shown.contains(p) {
                        add(refresh, MF_STRING, ID_REFRESH_PROVIDER + i, p.name());
                    }
                }
            }
            add(menu, MF_POPUP, refresh as usize, "Refresh");
            add(menu, MF_STRING, ID_MINIMIZE, minimize_label(minimized));
            add(menu, MF_SEPARATOR, 0, "");
            if !available.is_empty() {
                let show = CreatePopupMenu();
                for (i, p) in Provider::ALL.iter().enumerate() {
                    if available.contains(p) {
                        let checked = if shown.contains(p) { MF_CHECKED } else { 0 };
                        add(show, MF_STRING | checked, ID_TOGGLE + i, p.name());
                    }
                }
                add(menu, MF_POPUP, show as usize, "Show");
            }
            let has_usage = shown.iter().any(|p| p.usage_url().is_some());
            if has_usage {
                let usage = CreatePopupMenu();
                for (i, p) in Provider::ALL.iter().enumerate() {
                    if shown.contains(p) && p.usage_url().is_some() {
                        add(usage, MF_STRING, ID_OPEN + i, p.name());
                    }
                }
                add(menu, MF_POPUP, usage as usize, "View usage");
            }
            if !available.is_empty() || has_usage {
                add(menu, MF_SEPARATOR, 0, "");
            }
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
            let version = format!("Version {}", update::CURRENT);
            add(menu, MF_STRING | MF_GRAYED, 0, &version);
            add(menu, MF_STRING, ID_EDIT_CONFIG, "Edit config");
            add(menu, MF_STRING, ID_QUIT, "Quit");
            menu
        }
    }

    pub(super) fn action(id: usize) -> Option<MenuAction> {
        match id {
            ID_REFRESH => Some(MenuAction::Refresh),
            ID_MINIMIZE => Some(MenuAction::ToggleMinimized),
            ID_UPDATE => Some(MenuAction::CheckForUpdates),
            ID_EDIT_CONFIG => Some(MenuAction::EditConfig),
            ID_QUIT => Some(MenuAction::Quit),
            _ if (ID_OPEN..ID_OPEN + Provider::ALL.len()).contains(&id) => Provider::ALL
                [id - ID_OPEN]
                .usage_url()
                .map(MenuAction::Open),
            _ if (ID_TOGGLE..ID_TOGGLE + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::Toggle(Provider::ALL[id - ID_TOGGLE]))
            }
            _ if (ID_REFRESH_PROVIDER..ID_REFRESH_PROVIDER + Provider::ALL.len()).contains(&id) => {
                Some(MenuAction::RefreshProvider(
                    Provider::ALL[id - ID_REFRESH_PROVIDER],
                ))
            }
            _ if (ID_SIZE..ID_SIZE + SIZES.len()).contains(&id) => {
                Some(MenuAction::Size(SIZES[id - ID_SIZE]))
            }
            _ => None,
        }
    }

    pub(super) fn show(
        hwnd: isize,
        available: &[Provider],
        shown: &[Provider],
        zoom: f32,
        minimized: bool,
    ) -> Option<MenuAction> {
        unsafe {
            let menu = build(available, shown, zoom, minimized);
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
            action(id)
        }
    }
}

/// Shows the right-click menu as a native popup at the cursor and blocks until
/// it is dismissed. Unlike an egui menu it is not clipped to the window.
#[cfg(windows)]
fn native_menu(
    frame: &eframe::Frame,
    available: &[Provider],
    shown: &[Provider],
    zoom: f32,
    minimized: bool,
) -> Option<MenuAction> {
    let hwnd = hwnd(frame)?;
    win_menu::show(hwnd, available, shown, zoom, minimized)
}

#[cfg(any(not(windows), test))]
fn egui_menu(
    ui: &mut egui::Ui,
    available: &[Provider],
    shown: &[Provider],
    zoom: f32,
    minimized: bool,
) -> Option<MenuAction> {
    let mut action = None;
    ui.menu_button("Refresh", |ui| {
        if ui.button("All").clicked() {
            action = Some(MenuAction::Refresh);
        }
        if !shown.is_empty() {
            ui.separator();
            for &p in shown {
                if ui.button(p.name()).clicked() {
                    action = Some(MenuAction::RefreshProvider(p));
                }
            }
        }
    });
    if ui.button(minimize_label(minimized)).clicked() {
        action = Some(MenuAction::ToggleMinimized);
    }
    ui.separator();
    if !available.is_empty() {
        ui.menu_button("Show", |ui| {
            for &p in available {
                let mut checked = shown.contains(&p);
                if ui.checkbox(&mut checked, p.name()).clicked() {
                    action = Some(MenuAction::Toggle(p));
                }
            }
        });
    }
    let has_usage = shown.iter().any(|p| p.usage_url().is_some());
    if has_usage {
        ui.menu_button("View usage", |ui| {
            for (p, url) in shown.iter().filter_map(|&p| Some((p, p.usage_url()?))) {
                if ui.button(p.name()).clicked() {
                    action = Some(MenuAction::Open(url));
                }
            }
        });
    }
    if !available.is_empty() || has_usage {
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
        RichText::new(format!("version {}", update::CURRENT))
            .size(10.0)
            .color(MUTED),
    );
    if ui.button("Edit config").clicked() {
        action = Some(MenuAction::EditConfig);
    }
    if ui.button("Quit").clicked() {
        action = Some(MenuAction::Quit);
    }
    action
}

/// The right-click menu as a native popup from the menu bar's menu library. The
/// choice arrives later through `MenuBar::take_actions`.
#[cfg(target_os = "macos")]
fn mac_menu(frame: &eframe::Frame, app: &App, zoom: f32, minimized: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Some(bar) = &app.menubar else { return };
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: `ns_view` is the live NSView eframe draws into, and `ui` runs on the
    // main thread.
    unsafe {
        bar.popup(
            appkit.ns_view.as_ptr(),
            &app.available(),
            &app.shown(),
            zoom,
            minimized,
        )
    }
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
                Provider::Antigravity => include_bytes!("../assets/antigravity.alpha"),
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
        Provider::Copilot | Provider::Codex | Provider::Antigravity => TEXT,
    }
}

fn provider_logo(ui: &mut egui::Ui, p: Provider, logos: &HashMap<Provider, egui::TextureHandle>) {
    if let Some(logo) = logos.get(&p) {
        ui.add(egui::Image::new((logo.id(), Vec2::splat(13.0))).tint(logo_color(p)));
    } else {
        ui.allocate_exact_size(Vec2::splat(13.0), Sense::hover());
    }
}

/// The minimized view: "<logo> COP 42%  <logo> CLD 17%  <logo> CDX 99%" on one line.
fn compact_row(
    ui: &mut egui::Ui,
    providers: &[Provider],
    slots: &HashMap<Provider, Slot>,
    logos: &HashMap<Provider, egui::TextureHandle>,
    precision: usize,
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
        let slot = slots.get(p).cloned().unwrap_or_default();
        provider_logo(ui, *p, logos);
        ui.label(RichText::new(p.short_name()).size(11.5).color(MUTED));
        let top = bindings
            .get(p)
            .and_then(|&b| slot.windows(*p).get(b).copied())
            .or_else(|| {
                slot.meters
                    .iter()
                    .flatten()
                    .filter(|m| m.total > 0.0)
                    .max_by(|a, b| a.percent().total_cmp(&b.percent()))
            });
        let text = match (top, &slot.error) {
            (Some(m), _) => {
                let estimated = m.label.as_ref().is_some_and(|l| slot.estimated.contains(l));
                RichText::new(percent_label(m.percent(), precision, estimated))
                    .color(level_color(m.percent()))
            }
            (None, Some(e)) if e.problem == Problem::NotAvailable => {
                RichText::new("N/A").color(MUTED)
            }
            (None, Some(_)) => RichText::new("!").color(ERR),
            (None, None) => RichText::new("…").color(MUTED),
        };
        let resp = ui.label(text.size(11.5).strong());
        // What the full card would say under the numbers: stale data, its age, errors.
        if top.is_some() && slot.note.is_some() {
            ui.label(RichText::new("*").size(11.5).color(META));
        }
        if let (Some(_), Some(e)) = (top, &slot.error) {
            ui.label(RichText::new("!").size(11.5).strong().color(ERR))
                .on_hover_text(&e.detail);
        }
        let hover = usage_hover(&slot, precision);
        resp.on_hover_ui(|ui| hover.show(ui, &plan_summary(*p, &slot)));
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

    // Arc anticlockwise from the top round to the right, leaving a gap at the
    // top-right that the arrow head points into.
    let start = 90f32.to_radians();
    let end = 365f32.to_radians();
    let n = 20;
    let points: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = start + (end - start) * i as f32 / n as f32;
            Pos2::new(c.x + r * a.cos(), c.y - r * a.sin())
        })
        .collect();
    ui.painter().add(Shape::line(points, stroke));

    // Arrow head at the arc end, pointing along it: base across the arc, tip ahead.
    let base = Pos2::new(c.x + r * end.cos(), c.y - r * end.sin());
    let ahead = Vec2::new(-end.sin(), -end.cos());
    let out = Vec2::new(end.cos(), -end.sin());
    let head = r * 0.9;
    let tri = vec![
        base + out * head * 0.6,
        base + ahead * head,
        base - out * head * 0.6,
    ];
    ui.painter()
        .add(Shape::convex_polygon(tri, color, Stroke::NONE));

    resp.on_hover_text(tooltip)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, MINIMIZED_KEY, &self.minimized);
        eframe::set_value(storage, RATIOS_KEY, &self.ratios);
        eframe::set_value(storage, HIDDEN_KEY, &self.hidden);
        eframe::set_value(storage, SHOWN_KEY, &self.force_shown);
    }

    /// Runs before every `ui` pass, and is all that runs while the widget is
    /// hidden: eframe leaves the egui state untouched then, so this is the only
    /// chance to act on "Show widget".
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        self.restart_if_updated(ctx);
        #[cfg(target_os = "macos")]
        {
            self.handle_menu(ctx);
            if let Some(bar) = &self.menubar {
                bar.set_services(&self.available(), &self.shown());
            }
        }
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
            .corner_radius(if cfg!(target_os = "macos") { 8 } else { 0 })
            .inner_margin(margin);

        let mut refresh = false;
        let mut action = None;
        let zoom = ctx.zoom_factor();
        let minimized = self.minimized;
        let found = self.found();
        let available = self.available();
        let providers = self.shown();
        let bindings: HashMap<Provider, usize> = providers
            .iter()
            .filter_map(|&p| Some((p, self.binding(p)?)))
            .collect();
        let (empty, empty_compact) = self.empty_text(&found);
        let full_width = self.card_width(&ctx, &providers);
        let mut resized = false;
        let mut renewal = RenewalButton {
            running: self.renewal_lookup.is_some(),
            error: self.renewal_error.as_deref(),
            clicked: false,
            cancelled: false,
            raise_clicked: false,
        };

        // Checked on the raw pointer so a right-click over any widget counts.
        let right_clicked = ctx.input(|i| i.pointer.button_clicked(PointerButton::Secondary));
        // A native popup menu where there is one (Windows, and macOS with its menu
        // bar icon), since an egui menu is clipped to this small window.
        #[cfg(target_os = "macos")]
        let native_menu_available = self.menubar.is_some();
        #[cfg(not(target_os = "macos"))]
        let native_menu_available = cfg!(windows);

        egui::CentralPanel::default().frame(panel).show(root, |ui| {
            // Whole background is a drag handle and a right-click menu target.
            // `ui.max_rect()` is the content rect, inset by the panel's margin, so
            // add the margin back or the edges of the window can't be dragged.
            let bg = ui.interact(
                ui.max_rect() + panel.total_margin(),
                ui.id().with("bg"),
                Sense::click_and_drag(),
            );
            // Start on the press itself. Windows' move loop starts from wherever the
            // cursor is when it takes over and keeps that offset, so waiting for
            // egui's drag threshold left the window trailing the cursor.
            if pressed_on_background(&ctx, &bg) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            #[cfg(not(windows))]
            if !native_menu_available {
                bg.context_menu(|ui| {
                    action = egui_menu(ui, &available, &providers, zoom, minimized);
                    if action.is_some() {
                        ui.close();
                    }
                });
            }

            // Wrap the content so its real size can be measured: the panel's own
            // min_rect is always expanded to fill the window.
            let content = if minimized {
                ui.horizontal(|ui| {
                    compact_row(
                        ui,
                        &providers,
                        &self.slots,
                        &self.logos,
                        self.format.precision,
                        &bindings,
                        empty_compact,
                    )
                })
            } else {
                ui.vertical(|ui| {
                    ui.set_width(full_width - margin.sum().x);
                    if providers.is_empty() {
                        ui.label(RichText::new(empty).size(10.5).color(MUTED));
                    }
                    for (i, p) in providers.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(5.0);
                        }
                        if let Some(slot) = self.slots.get(p) {
                            provider_block(
                                ui,
                                *p,
                                slot,
                                self.format,
                                &mut renewal,
                                &self.logos,
                                bindings.get(p).copied(),
                            );
                        }
                    }
                    if let Some(err) = &self.config_error {
                        ui.label(RichText::new(err).size(10.0).color(ERR));
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if refresh_button(ui, "Refresh now", false) {
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
                                    .on_hover_text("Total across shown services");
                            });
                        }
                    });
                })
            };

            // Grow or shrink the window to fit the content. Sizes are in points, so
            // this also resizes the window when the zoom factor changes. The compact
            // row sizes the width to its content too; the full card has a fixed width.
            // Capped so a layout that feeds back on the window size can't exceed the
            // GPU's surface limit.
            let size = content.response.rect.size();
            let wanted = Vec2::new(
                if minimized {
                    size.x + margin.sum().x
                } else {
                    full_width
                },
                (size.y + margin.sum().y).min(1200.0),
            );
            let current = ctx.viewport_rect().size();
            if (wanted - current).abs().max_elem() > 1.5 {
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
                // The card grows downwards as data arrives (or on zoom), which can
                // push it past the screen edge, so re-check placement next frame.
                resized = true;
            }
        });

        // Shown after the content, so a widget with its own context menu (the
        // Claude renewal lookup) can take the click.
        if native_menu_available && right_clicked && !egui::Popup::is_any_open(&ctx) {
            #[cfg(windows)]
            {
                action = native_menu(frame, &available, &providers, zoom, minimized);
            }
            #[cfg(target_os = "macos")]
            mac_menu(frame, self, zoom, minimized);
        }

        let (lookup_renewal, cancel_renewal, raise_renewal) =
            (renewal.clicked, renewal.cancelled, renewal.raise_clicked);
        if let Some(action) = action {
            self.apply_action(action, &ctx);
        }
        if resized {
            self.settled = false;
        }
        if raise_renewal && let Some(lookup) = &self.renewal_lookup {
            lookup.bring_to_front();
        }
        if cancel_renewal {
            self.renewal_lookup = None;
        }
        if refresh {
            self.refresh_now();
        }
        if lookup_renewal {
            self.renewal_error = None;
            self.renewal_lookup = Some(renewal_lookup::start(ctx.clone()));
        }
    }
}

impl App {
    /// Closing normally lets eframe save position and size before the new
    /// version, which waits for this process to exit, starts up.
    fn restart_if_updated(&self, ctx: &egui::Context) {
        if self.restart.swap(false, Ordering::SeqCst) {
            match update::restart() {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => message_box(&format!("Updated, but {e}"), true),
            }
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
                        personal: false,
                        plan: None,
                        cycle: None,
                        note: None,
                        estimated: Vec::new(),
                        api_spend: None,
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
    fn antigravity_shows_both_windows_of_the_busiest_group() {
        let meter = |label: &str, used| Meter {
            label: Some(label.into()),
            used,
            total: 100.0,
            unit: Unit::Percent,
            resets_at: None,
        };
        let slot = Slot {
            personal: true,
            meters: Some(vec![
                meter("Gemini Models weekly", 4.0),
                meter("Gemini Models 5h", 13.0),
                meter("Claude and GPT models weekly", 0.0),
                meter("Claude and GPT models 5h", 0.0),
                meter("Gemini 4 Argon weekly", 0.0),
            ]),
            ..Slot::default()
        };
        let labels = |slot: &Slot| {
            slot.windows(Provider::Antigravity)
                .iter()
                .map(|m| m.label.clone().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&slot), ["Gemini Models 5h", "Gemini Models weekly"]);

        let with = |meters| Slot {
            personal: true,
            meters: Some(meters),
            ..Slot::default()
        };
        let used = |slot: &Slot| {
            slot.windows(Provider::Antigravity)
                .iter()
                .map(|m| m.used)
                .collect::<Vec<_>>()
        };
        let argon = with(vec![
            meter("Gemini Models weekly", 4.0),
            meter("Gemini Models 5h", 13.0),
            meter("Gemini 4 Argon weekly", 50.0),
        ]);
        assert_eq!(labels(&argon), ["Gemini 4 Argon weekly"]);
        let idle = with(vec![
            meter("Gemini 4 Argon weekly", 0.0),
            meter("Gemini Models 5h", 0.0),
            meter("Gemini Models weekly", 0.0),
        ]);
        assert_eq!(labels(&idle), ["Gemini Models 5h", "Gemini Models weekly"]);
        let repeated = with(vec![
            meter("Gemini Models 5h", 10.0),
            meter("Gemini Models 5h", 30.0),
            meter("Gemini Models weekly", 5.0),
        ]);
        assert_eq!(used(&repeated), [30.0, 5.0]);
        let burst = with(vec![
            meter("Gemini burst", 90.0),
            meter("Gemini weekly", 10.0),
        ]);
        assert_eq!(labels(&burst), ["Gemini burst"]);
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_reading_unless_the_service_is_gone() {
        let (mut app, updates, _) = App::for_test(vec![Provider::Antigravity]);
        let send = |at, result| {
            updates
                .send(Update {
                    provider: Provider::Antigravity,
                    result,
                    at,
                    started: std::time::Instant::now(),
                })
                .unwrap();
        };
        let reading = Usage {
            personal: true,
            plan: None,
            cycle: None,
            note: None,
            estimated: Vec::new(),
            api_spend: None,
            meters: vec![Meter {
                label: Some("Gemini 5h".into()),
                used: 25.0,
                total: 100.0,
                unit: Unit::Percent,
                resets_at: None,
            }],
        };
        send(1, Ok(reading));
        send(
            2,
            Err(FetchError {
                problem: Problem::Service,
                detail: "request timed out".into(),
            }),
        );
        app.drain();
        let slot = &app.slots[&Provider::Antigravity];
        assert_eq!(slot.meters.as_ref().map(Vec::len), Some(1));
        assert_eq!(slot.updated, Some(1));
        assert!(slot.error.is_some());

        send(
            3,
            Err(FetchError {
                problem: Problem::NotAvailable,
                detail: "Sign in".into(),
            }),
        );
        app.drain();
        let slot = &app.slots[&Provider::Antigravity];
        assert!(slot.meters.is_none());
        assert_eq!(slot.updated, Some(3));
        assert!(!slot.visible());
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
                    personal: false,
                    plan: None,
                    cycle: None,
                    note: None,
                    estimated: vec![],
                    api_spend: None,
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

/// Reads the same subscription data as the widget without opening a window.
fn print_usage() {
    let (mut config, config_error) = config::load();
    config.claude.estimate = false;
    config.codex.estimate = false;
    let mut services = serde_json::Map::new();
    for provider in Provider::ALL.into_iter().filter(|p| config.enabled(*p)) {
        let result = match provider.fetch(&config) {
            Ok(usage) => serde_json::json!({ "usage": usage }),
            Err(error) => serde_json::json!({
                "error": error.problem.label(),
                "detail": error.detail,
            }),
        };
        services.insert(provider.name().to_lowercase(), result);
    }
    println!(
        "{}",
        serde_json::json!({
            "fetched_at": now_unix(),
            "config_error": config_error,
            "services": services,
        })
    );
}

fn main() -> eframe::Result {
    let mut wait_for_exit = Duration::ZERO;
    match std::env::args().nth(1).as_deref() {
        Some("usage") => {
            print_usage();
            return Ok(());
        }
        Some("prices") => {
            println!(
                "{}",
                serde_json::json!({
                    "per_tokens": 1_000_000,
                    "claude": claude_estimate::pricing(),
                    "codex": codex_estimate::pricing(),
                })
            );
            return Ok(());
        }
        Some("config") => {
            return match config::open(true) {
                Ok(path) => {
                    println!("{}", path.display());
                    Ok(())
                }
                Err(e) => finish(Err(e)),
            };
        }
        Some("--updated") => wait_for_exit = Duration::from_secs(30),
        Some("--startup") => return finish(set_run_at_login(true)),
        Some("--no-startup") => return finish(set_run_at_login(false)),
        Some(flag) => {
            return finish(Err(format!(
                "unknown argument {flag}

usage: usage-widget [usage | prices | config | --startup | --no-startup]"
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
    let (config, config_error) = config::load();
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

/// Release builds on Windows have no console; this is the macOS equivalent. Started
/// from a terminal, the widget relaunches itself with no terminal and returns the
/// prompt. The relaunched copy has no terminal, so it runs the widget. If the
/// relaunch fails, the widget runs in the foreground instead.
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
    // Keeps `--updated`, so the copy waits for the old version to exit.
    cmd.args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
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

    // Opened without truncating, so a link planted at this path cannot be used
    // to empty another file.
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(std::env::temp_dir().join("usage-widget.lock"))
    else {
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

#[cfg(windows)]
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

/// Adds (or removes) a launch agent that starts this binary at login.
#[cfg(target_os = "macos")]
fn set_run_at_login(enable: bool) -> Result<String, String> {
    const LABEL: &str = "io.github.pepsi-enjoyer.usage-widget";
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let plist = std::path::Path::new(&home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    if !enable {
        return match std::fs::remove_file(&plist) {
            Ok(()) => Ok("usage-widget will no longer start at login.".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok("usage-widget was not registered to run at login.".into())
            }
            Err(e) => Err(format!("could not remove {}: {e}", plist.display())),
        };
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe
        .display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    );
    let write = || {
        std::fs::create_dir_all(plist.parent().unwrap())?;
        std::fs::write(&plist, xml)
    };
    write().map_err(|e| format!("could not write {}: {e}", plist.display()))?;
    Ok("usage-widget will start at login.\n\nRun `usage-widget --no-startup` to undo.".into())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn set_run_at_login(_enable: bool) -> Result<String, String> {
    Err("--startup is only supported on Windows and macOS".into())
}

/// Reports a flag's outcome, with a Windows dialog only for errors.
fn finish(result: Result<String, String>) -> eframe::Result {
    let (text, is_err) = match &result {
        Ok(msg) => (msg.clone(), false),
        Err(msg) => (msg.clone(), true),
    };
    #[cfg(windows)]
    if is_err {
        message_box(&text, true);
    }
    if is_err {
        eprintln!("{text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

/// Shows a message box on Windows or an alert on macOS; elsewhere the text goes to stderr.
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

#[cfg(test)]
mod display_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn weekly_reset_is_a_countdown_unless_the_date_is_asked_for() {
        let at = now_unix() + 3 * 86_400 + 2 * 3600;
        let meter = |label: &str| Meter {
            label: Some(label.into()),
            used: 18.0,
            total: 100.0,
            unit: Unit::Percent,
            resets_at: Some(at),
        };
        let default = TextFormat::default();
        let date = TextFormat {
            weekly_reset_date: true,
            ..default
        };
        assert_eq!(
            default.reset_label(&meter("week")),
            Some(format!("({})", window_until(at)))
        );
        assert_eq!(
            date.reset_label(&meter("week")),
            Some(format!("({})", reset_date_time(at, false)))
        );
        assert_eq!(
            date.reset_label(&meter("5h")),
            Some(format!("({})", window_until(at)))
        );
        assert!(!config::Config::default().weekly_reset_date);
    }

    #[test]
    fn compact_header_config_formats_and_narrows_a_full_row() {
        let config: Config = toml::from_str(
            "weekly_reset_date = true\nprecision = 3\n[header]\nclock_24h = true\nreset_parentheses = false\nspacing_size = 6",
        ).unwrap();
        let provider = Provider::Claude;
        let (mut app, _, _) = App::for_test(vec![provider]);
        app.format = TextFormat::from(&config);
        let at = chrono::Local
            .with_ymd_and_hms(2026, 11, 25, 22, 58, 0)
            .unwrap()
            .timestamp();
        let meter = window("week", 100.0, at);
        assert_eq!(app.format.reset_label(&meter).as_deref(), Some("Wed 22:58"));
        app.format.header.reset_parentheses = true;
        assert_eq!(
            app.format.reset_label(&meter).as_deref(),
            Some("(Wed 22:58)")
        );
        app.format.header.reset_parentheses = false;
        app.slots.insert(
            provider,
            Slot {
                personal: true,
                meters: Some(vec![window("5h", 100.0, at), meter]),
                ..Default::default()
            },
        );
        let ctx = egui::Context::default();
        ctx.run_ui(Default::default(), |_| {
            let compact = app.card_width(&ctx, &[provider]);
            app.format.header.spacing_size = 8.0;
            assert!(app.card_width(&ctx, &[provider]) > compact);
        })
        .textures_delta
        .clear();
    }

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

    fn window(label: &str, used: f64, resets_at: i64) -> Meter {
        Meter {
            label: Some(label.into()),
            used,
            total: 100.0,
            unit: Unit::Percent,
            resets_at: Some(resets_at),
        }
    }

    fn personal_slot() -> Slot {
        let now = now_unix().div_euclid(60) * 60;
        Slot {
            personal: true,
            plan: Some("Max 5x".into()),
            cycle: Some(Cycle {
                verb: "renews".into(),
                at: now + 9 * 86_400,
                date_only: true,
            }),
            meters: Some(vec![
                window("5h", 80.12, now + 3600),
                window("week", 90.0, now + 4 * 86_400),
                window("fable wk", 98.0, now + 4 * 86_400),
            ]),
            estimated: vec!["5h".into()],
            updated: Some(now),
            ..Default::default()
        }
    }

    #[test]
    fn hidden_and_unavailable_services_are_excluded_from_totals() {
        let (mut app, _, _) = App::for_test(Provider::ALL.to_vec());
        assert!(app.found().is_empty());
        for (p, used) in [
            (Provider::Copilot, 20.0),
            (Provider::Claude, 30.0),
            (Provider::Codex, 40.0),
        ] {
            app.slots.insert(
                p,
                Slot {
                    meters: Some(vec![Meter {
                        label: None,
                        used,
                        total: 100.0,
                        unit: Unit::Dollars,
                        resets_at: None,
                    }]),
                    updated: Some(now_unix()),
                    ..Default::default()
                },
            );
        }
        app.toggle(Provider::Claude);
        let shown = app.shown();
        assert_eq!(shown, [Provider::Copilot, Provider::Codex]);
        let total = app.total(&shown).unwrap();
        assert_eq!((total.used, total.total), (60.0, 200.0));
        app.slots.get_mut(&Provider::Codex).unwrap().error = Some(FetchError {
            problem: Problem::NotAvailable,
            detail: "no paid plan".into(),
        });
        assert_eq!(app.shown(), [Provider::Copilot]);
        assert!(app.total(&app.shown()).is_none());
        app.toggle(Provider::Copilot);
        assert_eq!(app.empty_text(&app.found()).1, "All hidden");
    }

    #[test]
    fn antigravity_without_valid_login_is_hidden_by_default_but_in_show() {
        for problem in [
            Problem::NotAvailable,
            Problem::TokenExpired,
            Problem::TokenRejected,
        ] {
            let (mut app, updates, _) =
                App::for_test(vec![Provider::Copilot, Provider::Antigravity]);
            app.slots.insert(
                Provider::Copilot,
                Slot {
                    meters: Some(vec![Meter {
                        label: None,
                        used: 10.0,
                        total: 100.0,
                        unit: Unit::Dollars,
                        resets_at: None,
                    }]),
                    updated: Some(now_unix()),
                    ..Default::default()
                },
            );
            updates
                .send(Update {
                    provider: Provider::Antigravity,
                    result: Err(FetchError {
                        problem,
                        detail: "no login".into(),
                    }),
                    at: now_unix(),
                    started: std::time::Instant::now(),
                })
                .unwrap();
            app.drain();

            assert_eq!(app.found(), [Provider::Copilot]);
            assert_eq!(app.available(), [Provider::Copilot, Provider::Antigravity]);
            assert_eq!(app.shown(), [Provider::Copilot]);

            // Ticking Antigravity in Show reveals it, and unticking hides it again
            // without permanently suppressing auto-show once signed in.
            app.toggle(Provider::Antigravity);
            assert_eq!(app.shown(), [Provider::Copilot, Provider::Antigravity]);
            app.toggle(Provider::Antigravity);
            assert_eq!(app.shown(), [Provider::Copilot]);

            // Once a valid Antigravity reading arrives, it appears automatically.
            updates
                .send(Update {
                    provider: Provider::Antigravity,
                    result: Ok(Usage {
                        personal: true,
                        plan: Some("Free".into()),
                        cycle: None,
                        note: None,
                        estimated: Vec::new(),
                        api_spend: None,
                        meters: vec![Meter {
                            label: Some("Gemini Models 5h".into()),
                            used: 20.0,
                            total: 100.0,
                            unit: Unit::Percent,
                            resets_at: None,
                        }],
                    }),
                    at: now_unix(),
                    started: std::time::Instant::now(),
                })
                .unwrap();
            app.drain();
            assert_eq!(app.found(), [Provider::Copilot, Provider::Antigravity]);
            assert_eq!(app.shown(), [Provider::Copilot, Provider::Antigravity]);

            app.toggle(Provider::Antigravity);
            assert_eq!(app.shown(), [Provider::Copilot]);
        }
    }

    #[test]
    fn refresh_provider_and_request_coalescing() {
        let ctx = egui::Context::default();
        let (mut app, _updates, refresh_rx) =
            App::for_test(vec![Provider::Copilot, Provider::Antigravity]);

        app.apply_action(MenuAction::RefreshProvider(Provider::Copilot), &ctx);
        assert!(app.slots[&Provider::Copilot].loading);
        assert!(!app.slots[&Provider::Antigravity].loading);
        assert_eq!(
            refresh_rx.try_recv().unwrap(),
            RefreshRequest::One(Provider::Copilot)
        );

        app.apply_action(MenuAction::Refresh, &ctx);
        assert!(app.slots[&Provider::Copilot].loading);
        assert!(app.slots[&Provider::Antigravity].loading);
        assert_eq!(refresh_rx.try_recv().unwrap(), RefreshRequest::All);

        let all = [Provider::Copilot, Provider::Claude, Provider::Antigravity];
        assert_eq!(
            collect_refresh_targets(
                RefreshRequest::One(Provider::Copilot),
                [
                    RefreshRequest::One(Provider::Copilot),
                    RefreshRequest::One(Provider::Antigravity),
                    RefreshRequest::One(Provider::Codex),
                ],
                &all,
            ),
            (vec![Provider::Copilot, Provider::Antigravity], false)
        );
        assert_eq!(
            collect_refresh_targets(
                RefreshRequest::One(Provider::Copilot),
                [RefreshRequest::All],
                &all,
            ),
            (all.to_vec(), true)
        );
    }

    #[test]
    fn egui_menu_renders_submenus() {
        let ctx = widget_ctx();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let action = egui_menu(
                    ui,
                    &Provider::ALL,
                    &[Provider::Copilot, Provider::Claude],
                    1.0,
                    false,
                );
                assert!(action.is_none());
            });
        });
        out.textures_delta.clear();
    }

    #[cfg(windows)]
    #[test]
    fn native_menu_organizes_refresh_show_view_usage_and_omits_interval() {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetMenuItemCount(menu: isize) -> i32;
            fn GetMenuStringW(
                menu: isize,
                id: u32,
                buf: *mut u16,
                max_count: i32,
                flags: u32,
            ) -> i32;
            fn GetMenuState(menu: isize, id: u32, flags: u32) -> u32;
            fn GetSubMenu(menu: isize, pos: i32) -> isize;
            fn GetMenuItemID(menu: isize, pos: i32) -> u32;
        }
        const MF_BYPOSITION: u32 = 0x400;
        const MF_CHECKED: u32 = 0x8;
        const MF_SEPARATOR: u32 = 0x800;

        #[derive(Debug, PartialEq)]
        struct Entry {
            label: String,
            checked: bool,
            separator: bool,
            action: Option<MenuAction>,
            children: Option<Vec<Entry>>,
        }

        unsafe fn dump(menu: isize) -> Vec<Entry> {
            unsafe {
                let count = GetMenuItemCount(menu);
                let mut out = Vec::new();
                for pos in 0..count {
                    let sub = GetSubMenu(menu, pos);
                    let state = GetMenuState(menu, pos as u32, MF_BYPOSITION);
                    // For a popup item, the high byte of GetMenuState holds its child count
                    // (e.g. 8 items = 0x800), so check MF_SEPARATOR only on leaf items.
                    let separator = sub == 0 && (state & MF_SEPARATOR) != 0;
                    let checked = sub == 0 && (state & MF_CHECKED) != 0;
                    let mut buf = [0u16; 128];
                    let len = GetMenuStringW(
                        menu,
                        pos as u32,
                        buf.as_mut_ptr(),
                        buf.len() as i32,
                        MF_BYPOSITION,
                    );
                    let label = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
                    let (action, children) = if sub != 0 {
                        (None, Some(dump(sub)))
                    } else {
                        let id = GetMenuItemID(menu, pos);
                        let action = (id != u32::MAX)
                            .then(|| win_menu::action(id as usize))
                            .flatten();
                        (action, None)
                    };
                    out.push(Entry {
                        label,
                        checked,
                        separator,
                        action,
                        children,
                    });
                }
                out
            }
        }

        let entries = unsafe {
            let menu = win_menu::build(
                &Provider::ALL,
                &[Provider::Copilot, Provider::Claude],
                1.0,
                false,
            );
            let entries = dump(menu);
            win_menu::DestroyMenu(menu);
            entries
        };

        let top_labels: Vec<&str> = entries
            .iter()
            .filter(|e| !e.separator)
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(top_labels[0], "Refresh");
        assert_eq!(top_labels[1], "Minimize");
        assert_eq!(top_labels[2], "Show");
        assert_eq!(top_labels[3], "View usage");
        assert_eq!(top_labels[4], "Size");
        assert_eq!(top_labels[5], "Check for updates");
        assert!(top_labels[6].starts_with("Version "));
        assert_eq!(top_labels[7], "Edit config");
        assert_eq!(top_labels[8], "Quit");
        assert_eq!(top_labels.len(), 9);
        assert!(
            top_labels
                .iter()
                .all(|l| !l.to_ascii_lowercase().contains("refreshes every"))
        );

        let submenu = |name: &str| {
            entries
                .iter()
                .find(|e| e.label == name)
                .and_then(|e| e.children.as_ref())
                .unwrap()
        };

        let refresh = submenu("Refresh");
        assert_eq!(refresh.len(), 4);
        assert_eq!(refresh[0].label, "All");
        assert_eq!(refresh[0].action, Some(MenuAction::Refresh));
        assert!(refresh[1].separator);
        assert_eq!(refresh[2].label, "Copilot");
        assert_eq!(
            refresh[2].action,
            Some(MenuAction::RefreshProvider(Provider::Copilot))
        );
        assert_eq!(refresh[3].label, "Claude");
        assert_eq!(
            refresh[3].action,
            Some(MenuAction::RefreshProvider(Provider::Claude))
        );

        let show = submenu("Show");
        let show_items: Vec<(&str, bool, Option<MenuAction>)> = show
            .iter()
            .map(|e| (e.label.as_str(), e.checked, e.action))
            .collect();
        assert_eq!(
            show_items,
            vec![
                ("Copilot", true, Some(MenuAction::Toggle(Provider::Copilot))),
                ("Claude", true, Some(MenuAction::Toggle(Provider::Claude))),
                ("Codex", false, Some(MenuAction::Toggle(Provider::Codex))),
                (
                    "Antigravity",
                    false,
                    Some(MenuAction::Toggle(Provider::Antigravity)),
                ),
            ]
        );

        let usage = submenu("View usage");
        let usage_items: Vec<(&str, Option<MenuAction>)> =
            usage.iter().map(|e| (e.label.as_str(), e.action)).collect();
        assert_eq!(
            usage_items,
            vec![
                (
                    "Copilot",
                    Provider::Copilot.usage_url().map(MenuAction::Open)
                ),
                ("Claude", Provider::Claude.usage_url().map(MenuAction::Open)),
            ]
        );

        // When only Antigravity is shown, View usage is omitted (it has no usage URL).
        let agy_only = unsafe {
            let menu = win_menu::build(
                &[Provider::Antigravity],
                &[Provider::Antigravity],
                1.0,
                true,
            );
            let entries = dump(menu);
            win_menu::DestroyMenu(menu);
            entries
        };
        assert!(agy_only.iter().all(|e| e.label != "View usage"));
        assert!(agy_only.iter().any(|e| e.label == "Maximize"));
    }

    #[test]
    fn primary_windows_keep_scoped_caps_and_renewals_in_hover_text() {
        let slot = personal_slot();
        assert_eq!(
            slot.windows(Provider::Claude)
                .iter()
                .map(|m| m.label.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["5h", "week"]
        );
        assert_eq!(plan_summary(Provider::Claude, &slot), "Claude Max 5x");
        let spent = Slot {
            api_spend: Some(1234.5),
            ..slot.clone()
        };
        assert_eq!(
            plan_summary(Provider::Claude, &spent),
            "Claude Max 5x ($1234.50 API spend this week)"
        );
        assert_eq!(plan_summary(Provider::Codex, &Slot::default()), "Codex");
        let hover = usage_hover(&slot, 0);
        assert!(hover.cycle.as_deref().unwrap().starts_with("renews 9d"));
        let rows: Vec<_> = hover
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.amount.as_str(), r.reset.is_some()))
            .collect();
        assert_eq!(
            rows,
            [
                ("5h", "~80.12%", true),
                ("week", "90%", true),
                ("fable wk", "98%", true)
            ]
        );
        assert_eq!(hover.notes, [ESTIMATE_NOTE]);
    }

    #[test]
    fn first_fetch_retains_learned_ratio_and_plan_change_resets_it() {
        let (mut app, updates, _) = App::for_test(vec![Provider::Claude]);
        let now = now_unix();
        let mut ratio = limits::Ratio::default();
        ratio.observe(
            &window("5h", 0.0, now + 3600),
            &window("week", 0.0, now + 4 * 86_400),
        );
        ratio.observe(
            &window("5h", 30.0, now + 3600),
            &window("week", 3.0, now + 4 * 86_400),
        );
        app.ratios.insert("Claude".into(), ratio);
        for (plan, expected) in [("Max 5x", Some(0.1)), ("Max 20x", None)] {
            updates
                .send(Update {
                    provider: Provider::Claude,
                    result: Ok(Usage {
                        personal: true,
                        plan: Some(plan.into()),
                        cycle: None,
                        note: None,
                        estimated: vec!["5h".into()],
                        api_spend: None,
                        meters: vec![
                            window("5h", 30.47, now + 3600),
                            window("week", 3.0, now + 4 * 86_400),
                        ],
                    }),
                    at: now,
                    started: std::time::Instant::now(),
                })
                .unwrap();
            app.drain();
            assert_eq!(app.ratios["Claude"].weekly_per_five_hour(), expected);
        }
    }

    fn rendered_text(output: &egui::FullOutput) -> Vec<String> {
        output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                Shape::Text(t) => Some(t.galley.job.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn antigravity_row_shows_both_windows_without_clipping() {
        let provider = Provider::Antigravity;
        // Half a minute past the mark, so countdowns read the same when drawn.
        let at = now_unix() + 6 * 86_400 + 30;
        let soon = now_unix() + 3600 + 30;
        let (mut app, _, _) = App::for_test(vec![provider]);
        app.format.precision = 2;
        app.slots.insert(
            provider,
            Slot {
                personal: true,
                meters: Some(vec![window("Gemini 4 Argon weekly", 50.0, at)]),
                ..Slot::default()
            },
        );
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |_| {
            assert_eq!(app.card_width(&ctx, &[provider]), WIDTH);
        });
        output.textures_delta.clear();

        for (weekly_date, session_used, session_pct, reset) in [
            (true, 0.0, "0.00%", reset_date_time(at, false)),
            (false, 0.0, "0.00%", window_until(at)),
            (true, 75.0, "75.00%", window_until(soon)),
        ] {
            let (mut app, _, _) = App::for_test(vec![provider]);
            app.format.weekly_reset_date = weekly_date;
            app.format.precision = 2;
            app.slots.insert(
                provider,
                Slot {
                    personal: true,
                    meters: Some(vec![
                        window("Gemini Models weekly", 0.44, at),
                        window("Claude and GPT models weekly", 0.0, at),
                        window("Gemini Models 5h", session_used, soon),
                    ]),
                    ..Slot::default()
                },
            );
            let ctx = egui::Context::default();
            let width = std::cell::Cell::new(0.0);
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        Vec2::new(500.0, 60.0),
                    )),
                    ..Default::default()
                },
                |root| {
                    width.set(app.card_width(&ctx, &[provider]));
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.inner_margin(MARGIN))
                        .show(root, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(width.get() - 2.0 * f32::from(MARGIN));
                                provider_block(
                                    ui,
                                    provider,
                                    &app.slots[&provider],
                                    app.format,
                                    &mut RenewalButton {
                                        running: false,
                                        error: None,
                                        clicked: false,
                                        cancelled: false,
                                        raise_clicked: false,
                                    },
                                    &HashMap::new(),
                                    app.binding(provider),
                                );
                            });
                        });
                },
            );
            assert!(width.get() >= WIDTH);
            let viewport = egui::Rect::from_min_size(Pos2::ZERO, Vec2::new(width.get(), 60.0));
            output.textures_delta.clear();
            let mut labels = Vec::new();
            let mut tracks = Vec::new();
            for shape in &output.shapes {
                match &shape.shape {
                    Shape::Text(t) => {
                        let bounds = t.galley.rect.translate(t.pos.to_vec2());
                        assert!(
                            shape.clip_rect.intersect(viewport).contains_rect(bounds),
                            "{} {bounds:?} clip {:?} view {viewport:?}",
                            t.galley.job.text,
                            shape.clip_rect
                        );
                        labels.push((t.galley.job.text.as_str(), bounds));
                    }
                    Shape::Rect(r) if r.fill == TRACK => tracks.push(r.rect),
                    _ => {}
                }
            }
            labels.sort_by(|a, b| a.1.left().total_cmp(&b.1.left()));
            assert_eq!(
                labels.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
                [
                    "Antigravity",
                    &format!("({reset})"),
                    session_pct,
                    "5h",
                    "|",
                    "0.44%",
                    "wk"
                ]
            );
            let [track] = tracks.as_slice() else {
                panic!("expected one progress bar")
            };
            for pair in labels.windows(2) {
                let (left, right) = (pair[0].1, pair[1].1);
                assert!(
                    left.right() <= right.left()
                        && left.bottom() > right.top()
                        && right.bottom() > left.top()
                );
            }
            assert!(labels.iter().all(|(_, rect)| rect.bottom() <= track.top()));
            assert!(viewport.contains_rect(*track));

            let mut compact = ctx.run_ui(Default::default(), |root| {
                egui::CentralPanel::default().show(root, |ui| {
                    ui.horizontal(|ui| {
                        compact_row(
                            ui,
                            &[provider],
                            &app.slots,
                            &HashMap::new(),
                            2,
                            &HashMap::new(),
                            "",
                        )
                    });
                });
            });
            compact.textures_delta.clear();
            let busiest = if session_used > 0.44 {
                session_pct
            } else {
                "0.44%"
            };
            assert_eq!(rendered_text(&compact), ["AGY", busiest]);
            assert_eq!(
                usage_hover(&app.slots[&provider], 2).rows[0].label,
                "Gemini Models weekly"
            );
        }
    }

    #[test]
    fn refresh_and_renewal_icons_keep_header_width_and_weekly_reset_visible() {
        let ctx = egui::Context::default();
        let (mut app, _, _) = App::for_test(vec![Provider::Claude]);
        app.slots.insert(Provider::Claude, personal_slot());
        assert_eq!(app.binding(Provider::Claude), Some(1));
        let slot = &app.slots[&Provider::Claude];
        let at = slot.windows(Provider::Claude)[1].resets_at.unwrap();
        let date = chrono::Local.timestamp_opt(at, 0).single().unwrap();
        let expected = format!("({})", date.format("%a %-I:%M%P"));
        let mut idle_width = None;
        for (loading, missing_renewal, lookup_running) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, true, true),
            (true, true, true),
        ] {
            let slot = app.slots.get_mut(&Provider::Claude).unwrap();
            slot.loading = loading;
            if missing_renewal {
                slot.cycle = None;
            }
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        Vec2::new(500.0, 300.0),
                    )),
                    ..Default::default()
                },
                |root| {
                    let width = app.card_width(&ctx, &app.shown());
                    assert_eq!(width, *idle_width.get_or_insert(width));
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.inner_margin(MARGIN))
                        .show(root, |ui| {
                            let mut renewal = RenewalButton {
                                running: lookup_running,
                                error: None,
                                clicked: false,
                                cancelled: false,
                                raise_clicked: false,
                            };
                            let response = ui.vertical(|ui| {
                                ui.set_width(width - 2.0 * f32::from(MARGIN));
                                provider_block(
                                    ui,
                                    Provider::Claude,
                                    &app.slots[&Provider::Claude],
                                    TextFormat {
                                        precision: 0,
                                        weekly_reset_date: true,
                                        ..Default::default()
                                    },
                                    &mut renewal,
                                    &HashMap::new(),
                                    Some(1),
                                );
                            });
                            assert!(
                                response.response.rect.width() + 2.0 * f32::from(MARGIN)
                                    <= width + 1.0
                            );
                            assert!(response.response.rect.height() < 45.0);
                        });
                },
            );
            assert!(rendered_text(&output).contains(&expected));
            output.textures_delta.clear();
        }
    }

    #[test]
    fn estimates_fit_one_header_and_one_bar_and_compact_uses_binding() {
        let ctx = egui::Context::default();
        let (mut app, _, _) = App::for_test(vec![Provider::Claude]);
        app.slots.insert(Provider::Claude, personal_slot());
        let resets = now_unix();
        let mut ratio = limits::Ratio::default();
        ratio.observe(
            &window("5h", 0.0, resets + 3600),
            &window("week", 0.0, resets + 4 * 86_400),
        );
        ratio.observe(
            &window("5h", 30.0, resets + 3600),
            &window("week", 3.0, resets + 4 * 86_400),
        );
        app.ratios.insert("Claude".into(), ratio);
        assert_eq!(app.binding(Provider::Claude), Some(0));
        let mut height = 0.0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    Pos2::ZERO,
                    Vec2::new(500.0, 300.0),
                )),
                ..Default::default()
            },
            |root| {
                let width = app.card_width(&ctx, &app.shown());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.inner_margin(MARGIN))
                    .show(root, |ui| {
                        let mut renewal = RenewalButton {
                            running: false,
                            error: None,
                            clicked: false,
                            cancelled: false,
                            raise_clicked: false,
                        };
                        let response = ui.vertical(|ui| {
                            ui.set_width(width - 2.0 * f32::from(MARGIN));
                            provider_block(
                                ui,
                                Provider::Claude,
                                &app.slots[&Provider::Claude],
                                TextFormat {
                                    precision: 0,
                                    weekly_reset_date: true,
                                    ..Default::default()
                                },
                                &mut renewal,
                                &HashMap::new(),
                                Some(0),
                            )
                        });
                        height = response.response.rect.height();
                        assert!(
                            response.response.rect.width() + 2.0 * f32::from(MARGIN) <= width + 1.0,
                            "rendered width {} exceeds requested {width}",
                            response.response.rect.width() + 2.0 * f32::from(MARGIN)
                        );
                    });
            },
        );
        assert!(height < 45.0, "header and bar expanded to {height}");
        let text = rendered_text(&output);
        assert!(text.iter().any(|t| t == "~80.12%"), "{text:?}");
        assert!(text.iter().any(|t| t == "90%"));
        assert!(!text.iter().any(|t| t.contains("fable")));
        assert_eq!(
            output
                .shapes
                .iter()
                .filter(|s| matches!(&s.shape, Shape::Rect(r) if r.fill == TRACK))
                .count(),
            1
        );

        output.textures_delta.clear();
        let bindings = HashMap::from([(Provider::Claude, 0)]);
        let mut output = ctx.run_ui(egui::RawInput::default(), |root| {
            egui::CentralPanel::default().show(root, |ui| {
                ui.horizontal(|ui| {
                    compact_row(
                        ui,
                        &[Provider::Claude],
                        &app.slots,
                        &HashMap::new(),
                        0,
                        &bindings,
                        "",
                    )
                });
            });
        });
        let text = rendered_text(&output);
        assert!(text.iter().any(|t| t == "~80.12%"), "{text:?}");
        assert!(!text.iter().any(|t| t == "98%"));
        output.textures_delta.clear();
    }

    /// The commands one `App::ui` pass sends, in a window of `size`.
    fn ui_commands(
        ctx: &egui::Context,
        app: &mut App,
        size: Vec2,
        events: Vec<egui::Event>,
    ) -> Vec<ViewportCommand> {
        // eframe's windowless stand-in, as egui_kittest uses for testing apps.
        let mut frame = eframe::Frame::_new_kittest();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |root| eframe::App::ui(app, root, &mut frame),
        );
        output.textures_delta.clear();
        output
            .viewport_output
            .remove(&egui::ViewportId::ROOT)
            .map(|v| v.commands)
            .unwrap_or_default()
    }

    /// A context styled as `App::new` styles it, so text doesn't take drags.
    fn widget_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(widget_style);
        ctx
    }

    fn primary(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Presses the primary button at `at` in a fresh window of `size`, then sends
    /// each of `then` in a pass of its own, and returns every command `App::ui`
    /// sent.
    fn press(
        app: &mut App,
        size: Vec2,
        at: Pos2,
        then: Vec<Vec<egui::Event>>,
    ) -> Vec<ViewportCommand> {
        let ctx = widget_ctx();
        // Hit testing uses the previous pass's widgets, so lay them out first.
        ui_commands(&ctx, app, size, vec![]);
        let down = vec![egui::Event::PointerMoved(at), primary(at, true)];
        let mut sent = ui_commands(&ctx, app, size, down);
        for events in then {
            sent.extend(ui_commands(&ctx, app, size, events));
        }
        sent
    }

    /// The whole window is a drag handle, margins and corners included (the
    /// handle used to stop at the content, so the edges didn't drag). The drag
    /// starts on the press, before the cursor moves, so the window doesn't trail
    /// it. A button on top still takes its click, and a right-click doesn't drag.
    #[test]
    fn window_drags_from_its_edges_and_buttons_still_click() {
        let (mut app, _updates, refresh_rx) = App::for_test(vec![Provider::Claude]);
        app.slots.insert(Provider::Claude, personal_slot());
        for minimized in [false, true] {
            app.minimized = minimized;
            // The size the widget asks for, so the probes sit at its real edges.
            let size = ui_commands(&widget_ctx(), &mut app, Vec2::splat(400.0), vec![])
                .into_iter()
                .find_map(|c| match c {
                    ViewportCommand::InnerSize(size) => Some(size),
                    _ => None,
                })
                .unwrap();
            let centre = (size / 2.0).to_pos2();
            // Half a point in: egui's 5pt hit radius used to cover all but the
            // outer 1pt of the 6pt minimized margin.
            let (near, far) = (0.5, size - Vec2::splat(0.5));
            let probes = [
                ("centre", centre),
                ("top", Pos2::new(centre.x, near)),
                ("bottom", Pos2::new(centre.x, far.y)),
                ("left", Pos2::new(near, centre.y)),
                ("right", Pos2::new(far.x, centre.y)),
                ("top left", Pos2::new(near, near)),
                ("top right", Pos2::new(far.x, near)),
                ("bottom left", Pos2::new(near, far.y)),
                ("bottom right", far.to_pos2()),
            ];
            let dead: Vec<_> = probes
                .into_iter()
                .filter(|&(_, at)| {
                    !press(&mut app, size, at, vec![]).contains(&ViewportCommand::StartDrag)
                })
                .map(|(name, _)| name)
                .collect();
            assert!(
                dead.is_empty(),
                "minimized {minimized}: {dead:?} don't drag on press"
            );

            let ctx = widget_ctx();
            ui_commands(&ctx, &mut app, size, vec![]);
            let right = egui::Event::PointerButton {
                pos: centre,
                button: PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            };
            let sent = ui_commands(
                &ctx,
                &mut app,
                size,
                vec![egui::Event::PointerMoved(centre), right],
            );
            assert!(
                !sent.contains(&ViewportCommand::StartDrag),
                "right-click dragged"
            );

            if !minimized {
                // The 13pt refresh button at the bottom left of the content.
                let inset = f32::from(MARGIN) + 6.5;
                let at = Pos2::new(inset, size.y - inset);
                assert!(refresh_rx.try_recv().is_err());
                let sent = press(&mut app, size, at, vec![vec![primary(at, false)]]);
                assert!(refresh_rx.try_recv().is_ok(), "refresh button didn't click");
                assert!(!sent.contains(&ViewportCommand::StartDrag));
            }
        }
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
