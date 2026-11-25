//! macOS menu bar icon. The app runs as an accessory, with no Dock icon and no
//! Cmd+Tab entry, so the menu bar holds its menu and a way to hide the widget.

use crate::providers::Provider;
use crate::{MenuAction, SIZES};
use eframe::egui;
use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use tray_icon::menu::{
    CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Clone, Copy)]
pub enum Action {
    ToggleWidget,
    Refresh,
    Open(&'static str),
    EditConfig,
    Quit,
    /// A choice from the widget's right-click popup.
    Widget(MenuAction),
}

pub struct MenuBar {
    _tray: TrayIcon,
    toggle: MenuItem,
    actions: Vec<(MenuId, Action)>,
    refresh_menu: Submenu,
    refresh_all: MenuItem,
    refresh_items: Vec<(Provider, MenuItem)>,
    show_menu: Submenu,
    services: Vec<(Provider, CheckMenuItem)>,
    usage_menu: Submenu,
    usage_items: Vec<(Provider, MenuItem)>,
    last_services: RefCell<Option<(Vec<Provider>, Vec<Provider>)>>,
    rx: Receiver<MenuId>,
    /// The items of the last right-click popup. AppKit delivers a choice after
    /// the popup has closed, so it arrives with the menu bar's clicks.
    popup_actions: RefCell<Vec<(MenuId, MenuAction)>>,
}

impl MenuBar {
    pub fn new(ctx: &egui::Context, providers: &[Provider]) -> Result<Self, String> {
        let toggle = MenuItem::new("Hide widget", true, None);
        let refresh_all = MenuItem::new("All", true, None);
        let refresh_items: Vec<_> = providers
            .iter()
            .map(|&p| (p, MenuItem::new(p.name(), true, None)))
            .collect();
        let refresh_menu = Submenu::new("Refresh", true);
        refresh_menu
            .append(&refresh_all)
            .map_err(|e| e.to_string())?;

        let services: Vec<_> = providers
            .iter()
            .map(|&p| (p, CheckMenuItem::new(p.name(), true, false, None)))
            .collect();
        let show_menu = Submenu::new("Show", false);

        let usage_items: Vec<_> = providers
            .iter()
            .filter_map(|&p| {
                p.usage_url()?;
                Some((p, MenuItem::new(p.name(), true, None)))
            })
            .collect();
        let usage_menu = Submenu::new("View usage", false);

        let updates = MenuItem::new("Check for updates", true, None);
        let edit = MenuItem::new("Edit config", true, None);
        let quit = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        let append =
            |item: &dyn tray_icon::menu::IsMenuItem| menu.append(item).map_err(|e| e.to_string());
        append(&toggle)?;
        append(&refresh_menu)?;
        append(&PredefinedMenuItem::separator())?;
        append(&show_menu)?;
        append(&usage_menu)?;
        append(&PredefinedMenuItem::separator())?;
        append(&updates)?;
        append(&edit)?;
        append(&quit)?;

        let mut actions = vec![
            (toggle.id().clone(), Action::ToggleWidget),
            (refresh_all.id().clone(), Action::Refresh),
            (
                updates.id().clone(),
                Action::Widget(MenuAction::CheckForUpdates),
            ),
            (edit.id().clone(), Action::EditConfig),
            (quit.id().clone(), Action::Quit),
        ];
        actions.extend(refresh_items.iter().map(|(p, item)| {
            (
                item.id().clone(),
                Action::Widget(MenuAction::RefreshProvider(*p)),
            )
        }));
        actions.extend(
            services
                .iter()
                .map(|(p, item)| (item.id().clone(), Action::Widget(MenuAction::Toggle(*p)))),
        );
        actions.extend(usage_items.iter().filter_map(|(p, item)| {
            let url = p.usage_url()?;
            Some((item.id().clone(), Action::Open(url)))
        }));

        // Clicks arrive on the main thread; wake the UI so it acts on them straight
        // away, even while the widget is hidden.
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = tx.send(e.id);
            ctx.request_repaint();
        }));

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon()?)
            .with_icon_as_template(true)
            .with_tooltip("usage-widget")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            _tray: tray,
            toggle,
            actions,
            refresh_menu,
            refresh_all,
            refresh_items,
            show_menu,
            services,
            usage_menu,
            usage_items,
            last_services: RefCell::default(),
            rx,
            popup_actions: RefCell::default(),
        })
    }

    /// Menu actions chosen since the last call.
    pub fn take_actions(&self) -> Vec<Action> {
        let popup = self.popup_actions.borrow();
        self.rx
            .try_iter()
            .filter_map(|id| {
                let bar = self.actions.iter().find(|(i, _)| *i == id).map(|(_, a)| *a);
                bar.or_else(|| {
                    let (_, a) = popup.iter().find(|(i, _)| *i == id)?;
                    Some(Action::Widget(*a))
                })
            })
            .collect()
    }

    /// Shows the widget's right-click menu as a native popup at the cursor and
    /// blocks until it is dismissed. Unlike an egui menu it is not clipped to the
    /// window, which matters most for the one-line minimized widget. The choice
    /// comes back from `take_actions` as `Action::Widget`.
    ///
    /// # Safety
    ///
    /// `ns_view` must point to the widget's live `NSView`.
    pub unsafe fn popup(
        &self,
        ns_view: *const std::ffi::c_void,
        available: &[Provider],
        shown: &[Provider],
        zoom: f32,
        minimized: bool,
    ) {
        let mut actions = Vec::new();
        let add_item = |actions: &mut Vec<(MenuId, MenuAction)>, text: &str, action: MenuAction| {
            let item = MenuItem::new(text, true, None);
            actions.push((item.id().clone(), action));
            item
        };
        let refresh = Submenu::new("Refresh", true);
        let refresh_all = add_item(&mut actions, "All", MenuAction::Refresh);
        let _ = refresh.append(&refresh_all);
        if !shown.is_empty() {
            let _ = refresh.append(&PredefinedMenuItem::separator());
            for &p in shown {
                let entry = add_item(&mut actions, p.name(), MenuAction::RefreshProvider(p));
                let _ = refresh.append(&entry);
            }
        }
        let minimize = add_item(
            &mut actions,
            crate::minimize_label(minimized),
            MenuAction::ToggleMinimized,
        );
        let show = Submenu::new("Show", true);
        for &p in available {
            let check = CheckMenuItem::new(p.name(), true, shown.contains(&p), None);
            actions.push((check.id().clone(), MenuAction::Toggle(p)));
            let _ = show.append(&check);
        }
        let usage = Submenu::new("View usage", true);
        let mut has_usage = false;
        for (p, url) in shown.iter().filter_map(|&p| Some((p, p.usage_url()?))) {
            let open = add_item(&mut actions, p.name(), MenuAction::Open(url));
            let _ = usage.append(&open);
            has_usage = true;
        }
        let sizes = Submenu::new("Size", true);
        for z in SIZES {
            let size = CheckMenuItem::new(
                format!("{:.0}%", z * 100.0),
                true,
                (zoom - z).abs() < 0.01,
                None,
            );
            actions.push((size.id().clone(), MenuAction::Size(z)));
            let _ = sizes.append(&size);
        }
        let updates = add_item(
            &mut actions,
            "Check for updates",
            MenuAction::CheckForUpdates,
        );
        let edit = add_item(&mut actions, "Edit config", MenuAction::EditConfig);
        let quit = add_item(&mut actions, "Quit", MenuAction::Quit);

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator;
        let _ = menu.append_items(&[&refresh, &minimize, &separator()]);
        if !available.is_empty() {
            let _ = menu.append(&show);
        }
        if has_usage {
            let _ = menu.append(&usage);
        }
        if !available.is_empty() || has_usage {
            let _ = menu.append(&separator());
        }
        let _ = menu.append_items(&[&sizes, &updates, &separator(), &edit, &quit]);

        *self.popup_actions.borrow_mut() = actions;
        // SAFETY: upheld by the caller.
        unsafe { menu.show_context_menu_for_nsview(ns_view, None) };
    }

    pub fn set_widget_shown(&self, shown: bool) {
        self.toggle
            .set_text(if shown { "Hide widget" } else { "Show widget" });
    }

    pub fn set_services(&self, available: &[Provider], shown: &[Provider]) {
        let mut last = self.last_services.borrow_mut();
        if last
            .as_ref()
            .is_some_and(|(a, s)| a.as_slice() == available && s.as_slice() == shown)
        {
            return;
        }
        *last = Some((available.to_vec(), shown.to_vec()));

        while self.refresh_menu.remove_at(0).is_some() {}
        let _ = self.refresh_menu.append(&self.refresh_all);
        let mut has_refresh = false;
        for (p, item) in &self.refresh_items {
            if shown.contains(p) {
                if !has_refresh {
                    let _ = self.refresh_menu.append(&PredefinedMenuItem::separator());
                    has_refresh = true;
                }
                let _ = self.refresh_menu.append(item);
            }
        }

        while self.show_menu.remove_at(0).is_some() {}
        for (p, item) in &self.services {
            item.set_checked(shown.contains(p));
            if available.contains(p) {
                let _ = self.show_menu.append(item);
            }
        }
        self.show_menu.set_enabled(!available.is_empty());

        while self.usage_menu.remove_at(0).is_some() {}
        let mut has_usage = false;
        for (p, item) in &self.usage_items {
            if shown.contains(p) {
                let _ = self.usage_menu.append(item);
                has_usage = true;
            }
        }
        self.usage_menu.set_enabled(has_usage);
    }
}

/// Three usage bars over faint tracks, echoing the widget. A template image, so
/// macOS tints it to suit a light or dark menu bar; drawn at 2x its 18pt height.
fn icon() -> Result<Icon, String> {
    const SIZE: usize = 36;
    const BAR: f32 = 5.0;
    let (left, right) = (4.0, 32.0);
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for (top, fraction) in [(7.0, 0.8), (15.5, 0.55), (24.0, 0.3)] {
        let fill_end = left + (right - left) * fraction;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let coverage = capsule(px, py, left, right, top, BAR);
                let shade = if px < fill_end { 1.0 } else { 0.35 };
                let alpha = (coverage * shade * 255.0).round() as u8;
                let a = &mut rgba[(y * SIZE + x) * 4 + 3];
                *a = (*a).max(alpha);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).map_err(|e| e.to_string())
}

/// Anti-aliased coverage of a pixel centre by a horizontal bar with round ends.
fn capsule(px: f32, py: f32, left: f32, right: f32, top: f32, height: f32) -> f32 {
    let r = height / 2.0;
    let (cx, cy) = (px.clamp(left + r, right - r), top + r);
    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
    (r + 0.5 - d).clamp(0.0, 1.0)
}
