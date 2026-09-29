//! Tray presence for background mode (decision 0052).
//!
//! The tray's menu events arrive on a thread the toolkit owns. They are forwarded over
//! a channel and the egui context is woken with `request_repaint`, which makes eframe
//! call `App::logic` even while the window is hidden. Polling in `ui` alone would miss
//! them, because eframe runs no UI pass for a hidden window.

use std::sync::mpsc::{Receiver, Sender, channel};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::error::ShellError;
use crate::icon::IconImage;

/// What the tray asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    Quit,
}

/// The tray is a second place to look the
/// moment it lists individual schedules, since the graph and `kgraf schedule
/// list` already do. It shows process presence plus exactly one aggregate line
/// (`kgraf_shell::scheduler_bg::ScheduleSummary::line`, e.g. "3 schedules, 1
/// failing; see graph") - a count, pointing elsewhere for detail, never a schedule's
/// own name or status.
fn build_menu(show: &MenuItem, quit: &MenuItem, summary: &str) -> Result<Menu, ShellError> {
    let menu = Menu::new();
    let err = |e: tray_icon::menu::Error| ShellError::Tray(e.to_string());
    menu.append(show).map_err(err)?;
    menu.append(quit).map_err(err)?;
    if !summary.is_empty() {
        menu.append(&PredefinedMenuItem::separator()).map_err(err)?;
        // Disabled (unclickable): a label, not an action - any change to a schedule
        // is a CLI or graph-view operation, never a tray click.
        menu.append(&MenuItem::new(summary, false, None))
            .map_err(err)?;
    }
    Ok(menu)
}

/// Owns the tray icon; dropping it removes the icon.
pub struct Tray {
    icon: TrayIcon,
    commands: Receiver<TrayCommand>,
    show: MenuItem,
    quit: MenuItem,
    /// The last summary line the menu was built with (LOW finding: rebuild the menu
    /// only on an actual change, not every call).
    last_summary: String,
}

impl Tray {
    /// Creates the tray icon. Must run on the main thread after the event loop starts
    /// (macOS requirement), which eframe's app-creator callback satisfies.
    pub fn new(image: &IconImage, wake: egui::Context) -> Result<Self, ShellError> {
        let icon = Icon::from_rgba(image.rgba.clone(), image.size, image.size)
            .map_err(|e| ShellError::Icon(e.to_string()))?;

        let show = MenuItem::new("Show kGraf", true, None);
        let quit = MenuItem::new("Quit kGraf", true, None);
        let menu = build_menu(&show, &quit, "")?;

        let (tx, rx) = channel();
        install_handlers(tx, wake, show.id().clone(), quit.id().clone());

        let icon = TrayIconBuilder::new()
            .with_tooltip("kGraf is running")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .map_err(|e| ShellError::Tray(e.to_string()))?;

        Ok(Self {
            icon,
            commands: rx,
            show,
            quit,
            last_summary: String::new(),
        })
    }

    /// Drains pending commands without blocking.
    pub fn poll(&self) -> Vec<TrayCommand> {
        self.commands.try_iter().collect()
    }

    /// Rebuilds the menu with the current schedule summary line below
    /// Show/Quit, but only when `summary` actually differs from what is already
    /// shown - a native menu rebuild is an OS call on every platform this crate
    /// supports, not worth paying every poll for an unchanged string.
    pub fn update_schedule_summary(&mut self, summary: &str) -> Result<(), ShellError> {
        if summary == self.last_summary {
            return Ok(());
        }
        let menu = build_menu(&self.show, &self.quit, summary)?;
        self.icon.set_menu(Some(Box::new(menu)));
        self.last_summary = summary.to_owned();
        Ok(())
    }
}

fn install_handlers(tx: Sender<TrayCommand>, wake: egui::Context, show: MenuId, quit: MenuId) {
    let menu_tx = tx.clone();
    let menu_wake = wake.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let cmd = if event.id == show {
            Some(TrayCommand::Show)
        } else if event.id == quit {
            Some(TrayCommand::Quit)
        } else {
            None
        };
        if let Some(cmd) = cmd {
            // A closed receiver means the app is shutting down; nothing to wake.
            if menu_tx.send(cmd).is_ok() {
                menu_wake.request_repaint();
            }
        }
    }));
    // A double click on the icon reopens the window. Other icon events are ignored.
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(event, TrayIconEvent::DoubleClick { .. }) && tx.send(TrayCommand::Show).is_ok()
        {
            wake.request_repaint();
        }
    }));
}
