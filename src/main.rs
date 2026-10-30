//! A Linux client for Rex, the terminal multiplexer, built on GPUI.
//!
//! Usage: `rex-gpui [-S SERVER] [SESSION]`. SESSION is a session's label or ID; without one
//! it attaches to the first session, or creates one when there are none. SERVER is a host
//! label from `rex hosts` or an address such as `https://build.example.ts.net`.

mod assets;
mod check;
mod keys;
mod rex;
mod settings;
mod terminal;
mod text_field;
mod theme;
mod workspace;

use std::sync::OnceLock;

use gpui::{App, Application, Bounds, Focusable, WindowBounds, WindowOptions, prelude::*, px, size};

use crate::rex::control::Control;
use crate::rex::endpoint::{self, Endpoint};
use crate::workspace::Workspace;

/// The tokio runtime that drives the WebSocket connections. GPUI has its own executor.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("starting the tokio runtime")
    })
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("rex_gpui=info"))
        .init();
    theme::load();
    let mut args = std::env::args().skip(1).peekable();
    let mut server = std::env::var("REX_SERVER").ok().filter(|s| !s.is_empty());
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-S" | "--server" => server = args.next(),
            "--list-themes" => {
                for t in theme::all() {
                    let kind = match &t.source {
                        theme::Source::BuiltIn => "built-in".to_string(),
                        theme::Source::File(path) => path.display().to_string(),
                    };
                    println!("{}\t{kind}", t.name);
                }
                return;
            }
            "-h" | "--help" => {
                println!("usage: rex-gpui [-S SERVER] [SESSION]\n       rex-gpui [-S SERVER] --check SESSION [COMMAND]\n       rex-gpui --list-themes");
                return;
            }
            _ => positional.push(arg),
        }
    }
    let target = positional.first().cloned();
    // Without -S, go back to the saved host this window last used, as a desktop launch has
    // no way to pass one. If it can't be reached, use the local server instead.
    let remembered = match &server {
        None => settings::get("last_host").and_then(|v| v.as_str().map(str::to_string)),
        Some(_) => None,
    };
    let server = server.or(remembered.clone());

    let endpoints = match &server {
        Some(server) => match runtime().block_on(endpoint::resolve(server)) {
            Ok(endpoints) => endpoints,
            Err(err) => {
                eprintln!("rex-gpui: {server}: {err:#}");
                std::process::exit(2);
            }
        },
        None => vec![Endpoint::local()],
    };
    let autostart = endpoints.iter().all(Endpoint::is_local);
    let mut host_label = server.clone();
    let connected = match runtime().block_on(Control::connect(&endpoints, autostart)) {
        Err(err) if remembered.is_some() => {
            log::warn!("{} is unreachable, so using the local server: {err:#}", server.as_deref().unwrap_or_default());
            host_label = None;
            runtime().block_on(Control::connect(&[Endpoint::local()], true))
        }
        other => other,
    };
    let (control, events) = match connected {
        Ok(connected) => connected,
        Err(err) => {
            eprintln!("rex-gpui: can't reach the Rex server: {err:#}");
            std::process::exit(1);
        }
    };
    log::info!("connected to {} as {}", control.endpoint, control.client_id);

    if target.as_deref() == Some("--check") {
        let session = positional.get(1).cloned().expect("usage: rex-gpui --check SESSION [COMMAND]");
        let command = positional.get(2).cloned();
        let _events = events;
        if let Err(err) = runtime().block_on(check::run(control, &session, command)) {
            eprintln!("check failed: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        workspace::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    app_id: Some("rex-gpui".into()),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Rex".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    window_background: gpui::WindowBackgroundAppearance::Transparent,
                    window_decorations: Some(gpui::WindowDecorations::Client),
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(control, events, target, host_label, cx));
                    window.focus(&workspace.focus_handle(cx));
                    workspace
                },
            )
            .expect("opening the window");
        let _ = window;
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
