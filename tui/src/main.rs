//! Larris – GRBL laser terminal – main entry point.
//!
//! Architecture:
//!  - Single-threaded async event loop (tokio).
//!  - Serial I/O runs on a dedicated OS thread (see `serial.rs`).
//!  - SVG→GCode conversion runs synchronously (blocking) on the main thread
//!    when the user presses 'c'.  It's fast enough (<1 s for typical SVGs) that
//!    no background thread is needed; we just redraw before and after.
//!  - GCode→image rasterisation also runs on the main thread when the user
//!    presses 'p'.
//!  - `ratatui-image` StatefulProtocol is (re-)built whenever `app.preview_image`
//!    changes; the picker is created once at startup.

mod app;
mod converter;
mod events;
mod grbl;
mod serial;
mod ui;

use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    app::App,
    events::{drain_serial_events, handle_key, poll_key, tick_status_poll},
    serial::{SerialEvent, discover_ports},
    ui::{render, render_help_overlay},
};

// ── Tick rate ─────────────────────────────────────────────────────────────────

/// How often we redraw even if no key was pressed (e.g. for arriving serial
/// data).  50 ms gives ~20 fps which is smooth without burning the CPU.
const TICK_RATE: Duration = Duration::from_millis(50);

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let mut app = App::new();

    // Discover serial ports on startup.
    app.port_list = discover_ports();
    if app.port_list.is_empty() {
        app.push_info("No serial ports found. Press 'r' to refresh.");
    } else {
        app.push_info(format!(
            "Found {} serial port(s). Use ↑/↓ + Enter to connect.",
            app.port_list.len()
        ));
        app.port_list_state.select(Some(0));
    }
    app.push_info("Welcome to Larris – GRBL Laser Terminal. Press '?' for help.");

    // ── Terminal setup ────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // ── Build the ratatui-image Picker ────────────────────────────────────
    // The Picker detects the best graphics protocol available in the terminal
    // (Kitty, Sixel, iTerm2, or Unicode half-blocks as a fallback).
    // `from_query_stdio` must be called *after* the terminal is in raw mode
    // because it sends escape sequences and reads back the response.
    let picker = match Picker::from_query_stdio() {
        Ok(p) => {
            app.push_info(format!("Terminal image protocol: {:?}", p.protocol_type()));
            p
        }
        Err(e) => {
            app.push_info(format!(
                "Could not detect image protocol ({e}), falling back to half-blocks."
            ));
            Picker::halfblocks()
        }
    };

    // ── Run the event loop ────────────────────────────────────────────────
    let result = run_event_loop(&mut terminal, &mut app, picker).await;

    // ── Restore terminal ──────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    terminal.show_cursor()?;

    if let Err(ref e) = result {
        eprintln!("Fatal error: {e:#}");
    }

    result
}

// ── Main event loop ───────────────────────────────────────────────────────────

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    picker: Picker,
) -> Result<()> {
    // Serial event receiver; replaced on each connect/disconnect.
    let mut serial_rx: Option<UnboundedReceiver<SerialEvent>> = None;

    // Help overlay visibility.
    let mut show_help = false;

    loop {
        // ── Tick status-message TTL ───────────────────────────────────────
        app.tick_status();

        // ── Auto status polling (sends '?' to GRBL periodically) ─────────
        tick_status_poll(app);

        // ── Drain serial events ───────────────────────────────────────────
        drain_serial_events(app, &mut serial_rx);

        // ── Encode preview image into StatefulProtocol if dirty ───────────
        //
        // `app.preview_image` is set by `do_render_preview()` in events.rs.
        // We consume it here (take) so we only encode once per render request.
        if let Some(img) = app.preview_image.take() {
            // `new_resize_protocol` creates a StatefulProtocol that will be
            // resized at render-time to fit the available widget area.
            let proto = picker.new_resize_protocol(image::DynamicImage::ImageRgba8(img));
            app.preview_protocol = Some(proto);
            app.preview_dirty = false;
        }

        // ── Force full repaint after native dialogs ───────────────────────
        // Native file dialogs leave the alternate screen and restore it, which
        // corrupts ratatui's internal diff buffer.  Calling clear() discards
        // the buffer so the next draw repaints every cell from scratch.
        if app.needs_clear {
            app.needs_clear = false;
            let _ = terminal.clear();
        }

        // ── Draw ──────────────────────────────────────────────────────────
        terminal.draw(|frame| {
            render(app, frame);
            if show_help {
                render_help_overlay(frame);
            }
        })?;

        // ── Check quit flag set by drain_serial_events ────────────────────
        if app.should_quit {
            break;
        }

        // ── Poll keyboard input ───────────────────────────────────────────
        if let Some(key) = poll_key(TICK_RATE)? {
            handle_key(app, key, &mut serial_rx, &mut show_help);
        }

        // ── Check quit flag set by key handler ────────────────────────────
        if app.should_quit {
            break;
        }
    }

    Ok(())
}

// ── Logging ───────────────────────────────────────────────────────────────────

/// Redirect all log output to `tui.log` so it doesn't corrupt the TUI.
/// Set `RUST_LOG=debug` (or any level) to get verbose output.
fn init_logging() {
    use std::fs::OpenOptions;

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("tui.log")
        .ok();

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"));

    if let Some(file) = log_file {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    builder.init();
}
