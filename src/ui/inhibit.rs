//! Inhibits the compositor's keyboard shortcuts for the viewer window
//! (`zwp_keyboard_shortcuts_inhibit_v1`), so that Super and the shortcuts
//! with it reach the Mac. The compositor applies the inhibitor only while the
//! window has the keyboard focus, and it can keep some shortcuts for itself,
//! such as a Hyprland bind with `dont_inhibit`.
//!
//! winit does not bind the protocol, so this module talks to winit's own
//! Wayland connection through an event queue of its own.

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::window::Window;

pub struct ShortcutInhibitor {
    conn: Connection,
    queue: EventQueue<State>,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    seat: WlSeat,
    surface: WlSurface,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl ShortcutInhibitor {
    /// Inhibits the shortcuts for `window`. `None` when the window is not on
    /// Wayland, or the compositor does not offer the protocol.
    pub fn new(window: &Window) -> Option<Self> {
        let (Ok(display), Ok(surface)) = (window.display_handle(), window.window_handle()) else { return None };
        let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
            (display.as_raw(), surface.as_raw())
        else {
            return None;
        };
        // Safety: winit owns the display and the surface, and both live as
        // long as the window, which outlives this inhibitor (see `App`).
        let conn = Connection::from_backend(unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) });
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast()) };
        let surface = id.ok().and_then(|id| WlSurface::from_id(&conn, id).ok())?;
        let (globals, queue) = registry_queue_init::<State>(&conn)
            .inspect_err(|e| tracing::warn!("keyboard shortcuts: Wayland registry: {e}"))
            .ok()?;
        let qh = queue.handle();
        let (manager, seat) = match (globals.bind(&qh, 1..=1, ()), globals.bind(&qh, 1..=1, ())) {
            (Ok(m), Ok(s)) => (m, s),
            (Err(e), _) | (_, Err(e)) => {
                tracing::info!("keyboard shortcuts stay with the compositor: {e}");
                return None;
            }
        };
        let mut this = Self { conn, queue, manager, seat, surface, inhibitor: None };
        this.inhibit();
        tracing::info!("keyboard shortcuts go to the Mac; Ctrl+Alt+Shift+Z gives them back to the compositor");
        Some(this)
    }

    /// Gives the shortcuts back to the compositor, or takes them again.
    pub fn toggle(&mut self) {
        // The active and inactive events since the last toggle, for the log.
        if let Err(e) = self.queue.dispatch_pending(&mut State) {
            tracing::warn!("keyboard shortcuts: {e}");
        }
        match self.inhibitor.take() {
            Some(i) => {
                i.destroy();
                self.flush();
                tracing::info!("keyboard shortcuts go to the compositor; Ctrl+Alt+Shift+Z gives them to the Mac");
            }
            None => {
                self.inhibit();
                tracing::info!("keyboard shortcuts go to the Mac; Ctrl+Alt+Shift+Z gives them back to the compositor");
            }
        }
    }

    fn inhibit(&mut self) {
        let qh = self.queue.handle();
        self.inhibitor = Some(self.manager.inhibit_shortcuts(&self.surface, &self.seat, &qh, ()));
        self.flush();
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            tracing::warn!("keyboard shortcuts: Wayland flush: {e}");
        }
    }
}

impl Drop for ShortcutInhibitor {
    fn drop(&mut self) {
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.manager.destroy();
        self.flush();
    }
}

struct State;

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        tracing::debug!("keyboard shortcuts inhibitor: {event:?}");
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ignore WlSeat);

