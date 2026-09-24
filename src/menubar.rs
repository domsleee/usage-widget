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
    Open(Provider),
    EditConfig,
    Quit,
    /// A choice from the widget's right-click popup.
    Widget(MenuAction),
}

pub struct MenuBar {
    _tray: TrayIcon,
    toggle: MenuItem,
    actions: Vec<(MenuId, Action)>,
    rx: Receiver<MenuId>,
    /// The items of the last right-click popup. AppKit delivers a choice after
    /// the popup has closed, so it arrives with the menu bar's clicks.
    popup_actions: RefCell<Vec<(MenuId, MenuAction)>>,
}

impl MenuBar {
    pub fn new(ctx: &egui::Context, providers: &[Provider]) -> Result<Self, String> {
        let toggle = MenuItem::new("Hide widget", true, None);
        let refresh = MenuItem::new("Refresh now", true, None);
        let opens: Vec<(MenuItem, Provider)> = providers
            .iter()
            .map(|&p| {
                (
                    MenuItem::new(format!("Open {} usage", p.name()), true, None),
                    p,
                )
            })
            .collect();
        let edit = MenuItem::new("Edit config", true, None);
        let quit = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        let append =
            |item: &dyn tray_icon::menu::IsMenuItem| menu.append(item).map_err(|e| e.to_string());
        append(&toggle)?;
        append(&refresh)?;
        append(&PredefinedMenuItem::separator())?;
        for (item, _) in &opens {
            append(item)?;
        }
        append(&PredefinedMenuItem::separator())?;
        append(&edit)?;
        append(&quit)?;

        let mut actions = vec![
            (toggle.id().clone(), Action::ToggleWidget),
            (refresh.id().clone(), Action::Refresh),
            (edit.id().clone(), Action::EditConfig),
            (quit.id().clone(), Action::Quit),
        ];
        actions.extend(
            opens
                .iter()
                .map(|(item, p)| (item.id().clone(), Action::Open(*p))),
        );

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
        providers: &[Provider],
        zoom: f32,
        refresh_mins: u64,
        minimized: bool,
    ) {
        let mut actions = Vec::new();
        let mut item = |text: &str, action: Option<MenuAction>| {
            let item = MenuItem::new(text, action.is_some(), None);
            if let Some(action) = action {
                actions.push((item.id().clone(), action));
            }
            item
        };
        let refresh = item("Refresh now", Some(MenuAction::Refresh));
        let minimize = item(
            crate::minimize_label(minimized),
            Some(MenuAction::ToggleMinimized),
        );
        let opens: Vec<MenuItem> = providers
            .iter()
            .map(|&p| item(&format!("Open {} usage", p.name()), Some(MenuAction::Open(p))))
            .collect();
        let note = item(&format!("Refreshes every {refresh_mins} min"), None);
        let edit = item("Edit config", Some(MenuAction::EditConfig));
        let quit = item("Quit", Some(MenuAction::Quit));
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

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator;
        let _ = menu.append_items(&[&refresh, &minimize, &separator()]);
        for open in &opens {
            let _ = menu.append(open);
        }
        let _ = menu.append_items(&[&separator(), &sizes, &separator(), &note, &edit, &quit]);

        *self.popup_actions.borrow_mut() = actions;
        // SAFETY: upheld by the caller.
        unsafe { menu.show_context_menu_for_nsview(ns_view, None) };
    }

    pub fn set_widget_shown(&self, shown: bool) {
        self.toggle
            .set_text(if shown { "Hide widget" } else { "Show widget" });
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
