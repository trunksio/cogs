//! Native window hosting the graph viz (WebKit via wry) with show/hide
//! toggle semantics. Zed extensions can't render custom panels, so this is
//! the closest thing to "the graph in Zed": a chromeless native window
//! toggled by a Zed task/keybinding. The process stays alive while hidden so
//! the WebView (camera, layout, filters) keeps its state.
//!
//! Control protocol: a Unix socket at <state>/runtime/viz.sock accepts
//! single-line verbs: "show", "toggle", "quit". The CLI connects and sends a
//! verb when an instance is already running; otherwise it becomes the
//! instance.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

use cogs_core::config::Vault;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Show,
    Toggle,
    Quit,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Show => "show",
            Verb::Toggle => "toggle",
            Verb::Quit => "quit",
        }
    }
}

/// Send a verb to a running instance. Ok(true) = delivered, Ok(false) = no
/// instance is listening.
fn send_verb(sock: &PathBuf, verb: Verb) -> Result<bool> {
    match UnixStream::connect(sock) {
        Ok(mut stream) => {
            stream.write_all(verb.as_str().as_bytes())?;
            stream.write_all(b"\n")?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

fn port_responding(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// Entry point for `cogs viz`. Must run on the main thread (macOS AppKit).
pub fn run(vault: Vault, port: u16, verb: Verb) -> Result<()> {
    std::fs::create_dir_all(vault.runtime_dir())?;
    let sock_path = vault.runtime_dir().join("viz.sock");

    // Already running? Just deliver the verb.
    if send_verb(&sock_path, verb)? {
        return Ok(());
    }
    if verb == Verb::Quit {
        return Ok(()); // nothing to quit
    }

    // We are the instance. Stale socket from a crashed run, if any.
    std::fs::remove_file(&sock_path).ok();

    // Serve the API+app in-process unless something already does (the HTTP
    // layer itself falls back to read-only when another process holds the
    // DB writer role).
    if !port_responding(port) {
        let vault = vault.clone();
        std::thread::Builder::new()
            .name("cogs-viz-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                if let Err(e) = rt.block_on(cogs_server::serve(vault, port)) {
                    tracing::error!("viz server exited: {e:#}");
                }
            })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !port_responding(port) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let url = format!("http://127.0.0.1:{port}/");

    // Window + webview.
    let event_loop = EventLoopBuilder::<Verb>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("cogs graph")
        .with_inner_size(LogicalSize::new(1240.0, 860.0))
        .build(&event_loop)
        .context("creating window")?;
    let webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)
        .context("creating webview")?;

    // Control socket listener.
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    std::thread::Builder::new()
        .name("cogs-viz-control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_ok() {
                    let verb = match line.trim() {
                        "show" => Verb::Show,
                        "quit" => Verb::Quit,
                        _ => Verb::Toggle,
                    };
                    if proxy.send_event(verb).is_err() {
                        break; // event loop gone
                    }
                }
            }
        })?;

    let sock_cleanup = sock_path.clone();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep the webview alive for the lifetime of the loop.
        let _ = &webview;
        match event {
            Event::UserEvent(verb) => match verb {
                Verb::Show => {
                    window.set_visible(true);
                    window.set_focus();
                }
                Verb::Toggle => {
                    if window.is_visible() {
                        window.set_visible(false);
                    } else {
                        window.set_visible(true);
                        window.set_focus();
                    }
                }
                Verb::Quit => {
                    std::fs::remove_file(&sock_cleanup).ok();
                    *control_flow = ControlFlow::Exit;
                }
            },
            // Closing the window hides it — the instance (and its state)
            // survives for the next toggle. `cogs viz --quit` really exits.
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                window.set_visible(false);
            }
            Event::LoopDestroyed => {
                std::fs::remove_file(&sock_cleanup).ok();
            }
            _ => {}
        }
    });
}
