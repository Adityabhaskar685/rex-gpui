//! Tell the desktop the app has started, so GNOME stops showing a busy cursor.
//!
//! A launcher on Wayland passes the new process an xdg-activation token in
//! `XDG_ACTIVATION_TOKEN`, and the compositor shows the app as starting until a window
//! presents that token, or about 15 seconds pass. GPUI never presents it, so this does,
//! on GPUI's own Wayland connection and window surface.

use anyhow::{Context as _, Result, bail};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

/// Present the launch's activation token for `window`, if there is one.
pub fn finish_startup(window: &gpui::Window) {
    let Ok(token) = std::env::var("XDG_ACTIVATION_TOKEN") else {
        return;
    };
    // Programs started from this one shouldn't present the same token.
    // SAFETY: called on the main thread while it is the only one reading the environment.
    unsafe { std::env::remove_var("XDG_ACTIVATION_TOKEN") };
    match activate(window, &token) {
        Ok(()) => log::info!("presented the launch's activation token"),
        Err(err) => log::warn!("presenting the launch's activation token: {err:#}"),
    }
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore XdgActivationV1);

fn activate(window: &gpui::Window, token: &str) -> Result<()> {
    let display = window
        .display_handle()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // GPUI's own `window_handle` method shadows the trait's.
    let handle = HasWindowHandle::window_handle(window).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
        (display.as_raw(), handle.as_raw())
    else {
        bail!("not a Wayland window");
    };
    // SAFETY: both pointers come from GPUI's live connection and window, which outlive this
    // call, and the backend doesn't take ownership of a foreign display.
    let connection = unsafe {
        Connection::from_backend(Backend::from_foreign_display(
            display.display.as_ptr() as *mut _
        ))
    };
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).context("reading Wayland globals")?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .context("the compositor has no xdg-activation")?;
    let id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr() as *mut _) }
            .context("wrapping the window surface")?;
    let surface = WlSurface::from_id(&connection, id).context("wrapping the window surface")?;
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    queue
        .roundtrip(&mut State)
        .context("sending the activation")?;
    Ok(())
}
