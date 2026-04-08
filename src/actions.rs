//! Business-logic action functions for Larris.
//!
//! This module contains the pure action functions that were previously part of
//! `events.rs`.  All keyboard / crossterm / rfd dialog code has been removed;
//! the GTK4 / Relm4 front-end calls these functions directly.
//!
//! ## Channel ownership
//!
//! Both `serial_tx` and `serial_rx` now live inside `App` as
//! `Option<std::sync::mpsc::Sender<SerialCommand>>` and
//! `Option<std::sync::mpsc::Receiver<SerialEvent>>` respectively.
//! `drain_serial_events` uses a take-then-put-back pattern to avoid
//! simultaneous mutable and immutable borrows of `app` inside the loop.

use crate::app::{App, AppMode, ConversionStatus, FocusedPane, GCodeFocus};
use crate::converter::{
    ConversionError, gcode_to_image, laser_bounding_box, png_to_gcode, png_to_preview_image,
    svg_to_gcode,
};
use crate::grbl::{GrblLine, JogDir, MachineState};
use crate::serial::{
    SerialCommand, SerialEvent, discover_ports, spawn_serial_actor, validate_port_path,
};

// ── Streaming helpers ─────────────────────────────────────────────────────────

fn apply_stream_progress(app: &mut App, sent: usize, total: usize) {
    app.stream_sent = sent;
    app.stream_total = total;
    app.set_status(format!("Streaming GCode: {sent}/{total} lines"), Some(10));
}

// ── Port management ───────────────────────────────────────────────────────────

/// Refresh the list of available serial ports and clamp the selection to the
/// new list length.
pub fn do_refresh_ports(app: &mut App) {
    let ports = discover_ports();
    let count = ports.len();
    app.port_list = ports;

    // Keep selection in-bounds.
    let sel = app.port_list_selected.unwrap_or(0);
    if app.port_list.is_empty() {
        app.port_list_selected = None;
    } else {
        app.port_list_selected = Some(sel.min(app.port_list.len() - 1));
    }

    app.push_info(format!("Port list refreshed – {count} port(s) found."));
}

/// Open a connection to the currently selected serial port.
///
/// On success the serial actor is spawned, `app.serial_rx` / `app.serial_tx`
/// are populated and an initial `$H` homing command is sent.
pub fn do_connect(app: &mut App) {
    let Some(port_path) = app.selected_port().map(str::to_owned) else {
        app.push_error("No port selected.");
        return;
    };

    if let Err(e) = validate_port_path(&port_path) {
        app.push_error(format!("Invalid port: {e}"));
        return;
    }

    let baud = app.selected_baud();
    app.push_info(format!(
        "Connecting to {port_path} at {} baud…",
        baud.label()
    ));

    match spawn_serial_actor(&port_path, baud) {
        Ok((cmd_tx, evt_rx)) => {
            app.mode = AppMode::Connected;
            app.connected_port = Some(port_path.clone());
            app.serial_tx = Some(cmd_tx);
            app.serial_rx = Some(evt_rx);
            app.focused = FocusedPane::CommandInput;

            // Send homing command immediately on connect.
            app.push_info("Sending homing command ($H)…");
            send_serial_raw(app, "$H");
        }
        Err(e) => {
            app.push_error(format!("Failed to connect: {e}"));
        }
    }
}

/// Close the active serial connection gracefully.
pub fn do_disconnect(app: &mut App) {
    if app.mode == AppMode::Disconnected {
        return;
    }
    if let Some(tx) = app.serial_tx.take() {
        let _ = tx.send(SerialCommand::Disconnect);
    }
    app.serial_rx = None;
    app.mode = AppMode::Disconnected;
    let port = app.connected_port.take().unwrap_or_default();
    app.push_info(format!("Disconnected from {port}."));
    app.focused = FocusedPane::SerialList;
}

// ── GRBL commands ─────────────────────────────────────────────────────────────

/// Send the GRBL homing cycle command (`$H`).
pub fn do_home(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot home.");
        return;
    }
    app.push_info("Sending homing command ($H)…");
    send_serial_raw(app, "$H");
}

/// Send a jog command in the given direction using the current step size and
/// feedrate.
pub fn do_jog(app: &mut App, dir: JogDir) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot jog.");
        return;
    }
    let step = app.jog_step_mm();
    let feed = app.jog_feedrate;
    let cmd = dir.to_command(step, feed);
    send_serial_raw(app, &cmd);
}

/// Send a single real-time byte to GRBL (feed-hold, cycle-start, soft-reset,
/// jog-cancel, override commands, …).
///
/// Real-time bytes must be sent without a trailing newline and without going
/// through the normal line-protocol queue; `SerialCommand::RealTimeByte`
/// handles that path.
pub fn send_realtime(app: &mut App, byte: u8) {
    if app.mode != AppMode::Connected {
        return;
    }
    if let Some(tx) = &app.serial_tx {
        let _ = tx.send(SerialCommand::RealTimeByte(byte));
    }
}

/// Manually send a `?` status poll to GRBL once.
pub fn do_poll_status(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected.");
        return;
    }
    if let Some(tx) = &app.serial_tx {
        let _ = tx.send(SerialCommand::RealTimeByte(b'?'));
    }
}

/// Abort a running ok-gated GCode stream.
pub fn do_abort_stream(app: &mut App) {
    if !app.is_streaming {
        app.set_status("No active stream to abort.", Some(60));
        return;
    }
    if let Some(tx) = &app.serial_tx {
        let _ = tx.send(SerialCommand::AbortStream);
    }
    app.push_info("Aborting GCode stream…");
}

/// Trace the laser-on bounding box of the current GCode with the laser off
/// (`S0`) so the user can verify the job position before burning.
///
/// Only G1/G2/G3 moves that carry a non-zero S word (laser power) are
/// considered when computing the bounding box – rapid travel moves are
/// excluded.
///
/// The sequence sent to the machine is:
/// ```text
/// G90 G21
/// G0 S0 F<feed> X<x0> Y<y0>   → front-left corner
/// G0 X<x1> Y<y0>              → front-right
/// G0 X<x1> Y<y1>              → back-right
/// G0 X<x0> Y<y1>              → back-left
/// G0 X<x0> Y<y0>              → close rectangle
/// ```
pub fn do_frame_job(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot frame job.");
        app.set_status("Not connected.", Some(80));
        return;
    }

    // ── Machine-state guard ───────────────────────────────────────────────
    //
    // Framing sends regular GCode commands (G90, G0 …).  GRBL rejects those
    // while in Jog or Run state, which can leave the parser in G91 (relative)
    // mode and cause subsequent rapid moves to travel relative to the
    // jogged-to position – easily driving the head into machine limits.
    //
    // • Jog state         → cancel the jog (real-time 0x85) and ask to retry.
    // • Any other non-Idle → block with a clear message.
    // • Status unknown     → allow (machine is likely idle on first connect
    //                        before any motion).
    if let Some(status) = &app.grbl_status {
        match &status.state {
            MachineState::Idle => {} // safe to proceed
            MachineState::Jog => {
                // Cancel the jog immediately via the real-time channel so GRBL
                // processes it before any queued serial data.
                if let Some(tx) = &app.serial_tx {
                    let _ = tx.send(SerialCommand::RealTimeByte(0x85));
                }
                app.push_info(
                    "Jog cancelled – wait for the machine to reach Idle, \
                     then press Frame again.",
                );
                app.set_status("Jog cancelled – retry frame when Idle.", Some(120));
                return;
            }
            state => {
                let label = state.label();
                app.push_error(format!(
                    "Cannot frame while machine is in {label} state – wait for Idle first."
                ));
                app.set_status(
                    format!("Machine not Idle ({label}) – frame blocked."),
                    Some(120),
                );
                return;
            }
        }
    }

    let gcode = match &app.gcode_text {
        Some(g) => g.clone(),
        None => {
            app.push_error("No GCode to frame. Convert a file first.");
            app.set_status("No GCode available.", Some(80));
            return;
        }
    };

    let Some((bb_min, bb_max)) = laser_bounding_box(&gcode) else {
        app.push_error("No laser-on moves found in GCode – nothing to frame.");
        app.set_status("No laser moves to frame.", Some(80));
        return;
    };

    let (x0, y0) = bb_min;
    let (x1, y1) = bb_max;
    let feed = app.machine_settings.feedrate;

    app.push_info(format!(
        "Framing job: X {:.3}…{:.3} mm, Y {:.3}…{:.3} mm",
        x0, x1, y0, y1
    ));

    // Build the five rapid moves that trace the rectangle with the laser off.
    let commands: &[(&str, f64, f64)] = &[
        ("front-left", x0, y0),
        ("front-right", x1, y0),
        ("back-right", x1, y1),
        ("back-left", x0, y1),
        ("front-left", x0, y0), // close the rectangle
    ];

    // Preamble: ensure absolute mode and metric units.
    send_serial_raw(app, "G90 G21");

    for (i, (_label, x, y)) in commands.iter().enumerate() {
        let cmd = if i == 0 {
            // First move: include S0 and feedrate explicitly.
            format!("G0 S0 F{feed:.0} X{x:.3} Y{y:.3}")
        } else {
            format!("G0 X{x:.3} Y{y:.3}")
        };
        send_serial_raw(app, &cmd);
    }

    app.set_status(
        format!(
            "Framing: {:.1}×{:.1} mm at ({:.1},{:.1})",
            x1 - x0,
            y1 - y0,
            x0,
            y0
        ),
        Some(200),
    );
}

/// Stream the generated GCode to the connected GRBL machine using the
/// ok-gated streaming protocol.
///
/// Each non-empty, non-comment line is sent as a separate `SerialCommand::Send`
/// via the serial actor.  The actor gates each transmission on receiving `ok`
/// from GRBL so the machine's planner buffer is never overrun.
pub fn do_send_gcode(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot stream GCode.");
        return;
    }
    if app.is_streaming {
        app.push_error("Already streaming – abort the current stream first.");
        return;
    }
    let gcode = match app.gcode_text.clone() {
        Some(g) => g,
        None => {
            app.push_error("No GCode to send. Convert an SVG first.");
            return;
        }
    };

    // Filter out blank lines and comment-only lines.  Each line is also
    // stripped of inline comments so GRBL sees clean tokens.
    let lines: Vec<String> = gcode
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('('))
        .map(|l| {
            // Strip anything from ';' onwards (inline comment).
            let l = if let Some(pos) = l.find(';') {
                &l[..pos]
            } else {
                l
            };
            // Strip parenthesised comments (anything inside '(' … ')').
            let mut out = String::with_capacity(l.len());
            let mut depth = 0usize;
            for ch in l.chars() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.saturating_sub(1);
                    }
                    _ if depth == 0 => out.push(ch),
                    _ => {}
                }
            }
            out.trim().to_owned()
        })
        .filter(|l| !l.is_empty())
        .collect();

    let count = lines.len();
    if count == 0 {
        app.push_error("GCode is empty after filtering comments.");
        return;
    }

    app.push_info(format!(
        "Starting ok-gated GCode stream: {count} lines. Abort to cancel."
    ));
    app.is_streaming = true;
    app.stream_sent = 0;
    app.stream_total = count;
    app.set_status(format!("Streaming GCode: 0/{count} lines"), Some(10));

    if let Some(tx) = &app.serial_tx {
        if tx.send(SerialCommand::Stream(lines)).is_err() {
            app.push_error("Serial channel closed.");
            app.is_streaming = false;
        }
    }
}

/// Send a raw string to the serial actor and log it to the console as a sent
/// line.  Does not add to command history.
pub fn send_serial_raw(app: &mut App, cmd: &str) {
    if let Some(tx) = &app.serial_tx {
        match tx.send(SerialCommand::Send(cmd.to_owned())) {
            Ok(_) => {
                app.push_sent(cmd.to_owned());
            }
            Err(e) => {
                app.push_error(format!("Send failed: {e}"));
            }
        }
    }
}

/// Send the contents of a command-input line (from the user) and log it to
/// the console as a sent line.  Notifies the user if not connected.
pub fn send_serial(app: &mut App, line: String) {
    if let Some(tx) = &app.serial_tx {
        match tx.send(SerialCommand::Send(line.clone())) {
            Ok(_) => {
                app.push_sent(line);
            }
            Err(e) => {
                app.push_error(format!("Send failed: {e}"));
            }
        }
    } else {
        app.push_error("Not connected – command not sent.");
    }
}

// ── File I/O ──────────────────────────────────────────────────────────────────

/// Load an SVG or raster image file into the application state.
///
/// The file type is determined from the file extension.  Raster images
/// (PNG, JPEG, BMP, GIF, TIFF, WebP) are loaded as a preview image; SVG
/// files have their layers parsed out for per-layer override editing.
///
/// This function takes the path directly — no file dialog is opened.
pub fn load_file(app: &mut App, path: &std::path::Path) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_image = matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tiff" | "tif" | "webp"
    );

    // Reset common pipeline state regardless of source type.
    app.gcode_text = None;
    app.gcode_scroll = 0;
    app.conversion_status = ConversionStatus::Idle;
    app.preview_image = None;
    app.gcode_preview_image = None;
    app.gcode_focus = GCodeFocus::GCode;
    app.cancel_offset_edit();

    if is_image {
        // ── Raster image (PNG / JPEG / …) ─────────────────────────────────
        app.is_image_source = true;
        app.layers.clear();

        // Load pixel dimensions and a scaled preview image.
        match png_to_preview_image(path, 800, 600) {
            Ok(img) => {
                app.image_dimensions = Some((img.width(), img.height()));
                app.preview_image = Some(img);
            }
            Err(e) => {
                app.image_dimensions = None;
                app.push_error(format!("Could not load image preview: {e}"));
            }
        }

        app.svg_path = Some(path.to_owned());
        app.push_info(format!("Loaded image: {}", path.display()));
        let dims = app
            .image_dimensions
            .map(|(w, h)| format!("{w}×{h}px"))
            .unwrap_or_default();
        app.set_status(
            format!("Image loaded ({dims}). Press Convert to proceed."),
            Some(160),
        );
    } else {
        // ── SVG ────────────────────────────────────────────────────────────
        app.is_image_source = false;
        app.image_dimensions = None;

        if let Ok(svg_text) = std::fs::read_to_string(path) {
            app.load_layers_from_svg(&svg_text);
            let n = app.layers.len();
            if n > 0 {
                app.set_status(format!("SVG loaded – {n} layer(s) detected."), Some(160));
            } else {
                app.set_status("SVG loaded. Click Convert to proceed.", Some(120));
            }
        } else {
            app.layers.clear();
            app.set_status("SVG loaded. Click Convert to proceed.", Some(120));
        }

        app.svg_path = Some(path.to_owned());
        app.push_info(format!("Loaded SVG: {}", path.display()));
    }
}

/// Write the current GCode buffer to `path`.
///
/// This function saves directly to the given path — no file dialog is
/// opened.  Reports success or failure to the console and status bar.
pub fn save_gcode_to(app: &mut App, path: &std::path::Path) {
    let gcode = match &app.gcode_text {
        Some(g) => g.clone(),
        None => {
            app.push_error("No GCode to save.");
            app.set_status("No GCode to save.", Some(80));
            return;
        }
    };

    match std::fs::write(path, gcode.as_bytes()) {
        Ok(_) => {
            app.push_info(format!("GCode saved to: {}", path.display()));
            app.set_status(format!("Saved: {}", path.display()), Some(120));
        }
        Err(e) => {
            app.push_error(format!("Save failed: {e}"));
            app.set_status(format!("Save failed: {e}"), Some(120));
        }
    }
}

// ── Conversion ────────────────────────────────────────────────────────────────

/// Convert the loaded SVG or raster image to GCode using the current machine
/// settings and any per-layer overrides.
///
/// Runs synchronously on the calling thread.  For typical SVGs and images
/// this completes in well under a second; no background thread is used.
pub fn do_convert(app: &mut App) {
    let Some(source_path) = app.svg_path.clone() else {
        app.push_error("No file loaded. Open an SVG or image first.");
        app.set_status("No file loaded.", Some(80));
        return;
    };

    app.conversion_status = ConversionStatus::Running;
    app.gcode_text = None;
    app.gcode_scroll = 0;
    app.gcode_preview_image = None;

    // For image sources the source preview (left panel) is static – keep it.
    // For SVG, clear it so it can be re-rendered after conversion.
    if !app.is_image_source {
        app.preview_image = None;
    }

    let settings = app.machine_settings.clone();

    let result = if app.is_image_source {
        png_to_gcode(&source_path, &settings, app.invert_image)
    } else {
        let layer_overrides = app.layer_override_map();
        svg_to_gcode(&source_path, &settings, layer_overrides)
    };

    match result {
        Ok(gcode) => {
            let line_count = gcode.lines().count();
            app.push_info(format!("Conversion OK – {line_count} lines of GCode."));
            app.gcode_text = Some(gcode);
            app.gcode_scroll = 0;
            app.conversion_status = ConversionStatus::Ok;
            app.set_status(format!("Converted: {line_count} GCode lines."), Some(120));
        }
        Err(e) => {
            if let Some(conv_err) = e.downcast_ref::<ConversionError>() {
                app.conversion_status = ConversionStatus::Failed(conv_err.title.clone());
                app.push_error(format!("Conversion failed: {}", conv_err.title));
                app.show_conversion_error(conv_err.title.clone(), conv_err.body.clone());
                app.set_status(format!("Conversion failed: {}", conv_err.title), Some(200));
            } else {
                let msg = format!("{e:#}");
                app.push_error(format!("Conversion failed: {msg}"));
                app.conversion_status = ConversionStatus::Failed(msg.clone());
                app.set_status(format!("Conversion failed: {msg}"), Some(120));
            }
        }
    }
}

// ── Preview render ────────────────────────────────────────────────────────────

/// Rasterise the GCode toolpath into an `RgbaImage` and store it in
/// `app.gcode_preview_image`.
///
/// The raw `RgbaImage` is stored here; the GTK4 front-end is responsible for
/// converting it into a `gdk4::Texture` or equivalent for display.
pub fn do_render_preview(app: &mut App) {
    // Clone early so we don't hold an immutable borrow across mutable calls.
    let gcode = match app.gcode_text.clone() {
        Some(g) => g,
        None => {
            app.push_error("No GCode to preview. Convert an SVG first.");
            app.set_status("No GCode to preview.", Some(80));
            return;
        }
    };

    let travel = app.show_travel_lines;
    app.set_status("Rendering toolpath preview…", Some(20));

    // Reasonable pixel dimensions for the preview panel.
    let max_w = 800u32;
    let max_h = 600u32;

    match gcode_to_image(&gcode, max_w, max_h, travel) {
        Ok(img) => {
            app.gcode_preview_image = Some(img);
            app.push_info("Toolpath preview rendered.");
            app.set_status("Preview ready.", Some(100));
        }
        Err(e) => {
            app.push_error(format!("Preview render failed: {e}"));
            app.set_status(format!("Preview failed: {e}"), Some(120));
        }
    }
}

// ── Serial event drain ────────────────────────────────────────────────────────

/// Drain all pending events from the serial actor and apply them to app state.
///
/// Uses a take-then-put-back pattern for `app.serial_rx` to avoid a
/// simultaneous mutable + immutable borrow of `app` inside the loop
/// (the loop body needs `&mut app` for logging / state updates, while the
/// receiver itself is also owned by `app`).
///
/// When the connection ends (`SerialEvent::Disconnected` or channel closed)
/// the receiver is *not* put back — `app.serial_rx` stays `None`.
pub fn drain_serial_events(app: &mut App) {
    // Take the receiver out of app for the duration of the drain loop.
    let Some(rx) = app.serial_rx.take() else {
        return;
    };

    loop {
        match rx.try_recv() {
            Ok(SerialEvent::StreamProgress { sent, total }) => {
                apply_stream_progress(app, sent, total);
            }

            Ok(SerialEvent::StreamDone { total }) => {
                app.is_streaming = false;
                app.stream_sent = total;
                app.stream_total = total;
                app.push_info(format!("GCode stream complete – {total} lines sent."));
                app.set_status(format!("Stream done: {total} lines"), Some(200));
            }

            Ok(SerialEvent::StreamAborted {
                sent,
                total,
                reason,
            }) => {
                app.is_streaming = false;
                app.push_error(format!(
                    "GCode stream aborted after {sent}/{total} lines: {reason}"
                ));
                app.set_status(format!("Stream aborted at {sent}/{total}"), Some(200));
            }

            Ok(SerialEvent::Line(line)) => {
                // Parse the line through the GRBL classifier and update state.
                let parsed = GrblLine::parse(&line);

                match &parsed {
                    GrblLine::Status(st) => {
                        // Merge override values from the status report back into
                        // the app's UI sliders so they reflect the real machine
                        // state.
                        if let Some(ov) = &st.overrides {
                            app.override_feed = ov.feed;
                            app.override_rapid = ov.rapid;
                            app.override_spindle = ov.spindle;
                        }
                        app.grbl_status = Some(st.clone());
                    }
                    GrblLine::GcState(s) => {
                        app.gc_state = Some(s.clone());
                    }
                    GrblLine::Setting { key, value } => {
                        app.upsert_setting(key.clone(), value.clone());
                    }
                    GrblLine::Probe { pos, success } => {
                        app.last_probe = Some((*pos, *success));
                    }
                    GrblLine::Error(code) => {
                        let desc = crate::grbl::error_description(*code);
                        app.push_error(format!("error:{code} – {desc}"));
                    }
                    GrblLine::Alarm(code) => {
                        let desc = crate::grbl::alarm_description(*code);
                        app.push_error(format!("ALARM:{code} – {desc}"));
                    }
                    _ => {}
                }

                // Push lines to the console; use parsed summary for
                // status/setting lines to keep them concise.  Status and
                // error/alarm lines are handled specially to avoid duplication.
                match &parsed {
                    GrblLine::Status(_) => {
                        // Status lines are frequent — do not spam the console.
                    }
                    GrblLine::Error(_) | GrblLine::Alarm(_) => {
                        // Already pushed as push_error above; skip push_received.
                    }
                    GrblLine::Setting { .. } => {
                        // Settings are displayed in the settings panel; skip
                        // console spam.
                    }
                    _ => {
                        app.push_received(parsed.summary());
                    }
                }
            }

            Ok(SerialEvent::Info(msg)) => {
                app.push_info(msg);
            }

            Ok(SerialEvent::Error(msg)) => {
                app.push_error(msg);
            }

            Ok(SerialEvent::Disconnected(reason)) => {
                // Connection ended — clear serial state and do NOT put rx back.
                app.mode = AppMode::Disconnected;
                app.connected_port = None;
                app.serial_tx = None;
                // app.serial_rx stays None (already taken above).
                if let Some(r) = reason {
                    app.push_error(format!("Disconnected: {r}"));
                } else {
                    app.push_info("Disconnected from serial port.");
                }
                app.focused = FocusedPane::SerialList;
                return;
            }

            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // No more events waiting right now — put rx back and return.
                break;
            }

            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The actor thread exited unexpectedly — do NOT put rx back.
                app.mode = AppMode::Disconnected;
                app.connected_port = None;
                app.serial_tx = None;
                // app.serial_rx stays None.
                app.push_error("Serial actor disconnected unexpectedly.");
                app.focused = FocusedPane::SerialList;
                return;
            }
        }
    }

    // Still connected — put the receiver back so the next tick can drain again.
    app.serial_rx = Some(rx);
}

// ── Status polling tick ───────────────────────────────────────────────────────

/// Called once per main-loop tick (e.g. every 50 ms).  When connected and
/// `status_poll_interval` is non-zero, automatically sends a `?` status
/// request to GRBL at the configured interval without flooding the console.
pub fn tick_status_poll(app: &mut App) {
    if app.mode != AppMode::Connected || app.status_poll_interval == 0 {
        app.status_poll_ticks = 0;
        return;
    }
    app.status_poll_ticks += 1;
    if app.status_poll_ticks >= app.status_poll_interval {
        app.status_poll_ticks = 0;
        // '?' is a GRBL real-time byte: send it as a single raw byte with no
        // trailing newline.  Using SerialCommand::Send would append '\n' and
        // queue an empty command in GRBL's 128-byte RX buffer on every poll
        // cycle, eventually overflowing it and flooding the console with
        // spurious 'ok' responses while corrupting jog/stream sequencing.
        if let Some(tx) = &app.serial_tx {
            let _ = tx.send(SerialCommand::RealTimeByte(b'?'));
        }
    }
}
