//! Keyboard and serial event handling for Larris.
//!
//! Key map summary:
//!
//!  GLOBAL
//!   F1          → Connect tab
//!   F2          → GCode tab   (when disconnected: also used for disconnect)
//!   F3          → Preview tab / Home command (when connected on Connect tab)
//!   Tab/BackTab → Cycle pane focus (Connect tab)
//!   ?           → Help overlay
//!   q / Ctrl-C  → Quit
//!
//!  CONNECT TAB – port list focused
//!   ↑ / ↓ / j / k → navigate port list
//!   Enter          → connect
//!   r              → refresh ports
//!   b              → baud-rate dropdown
//!   F2             → disconnect (when connected)
//!   F3             → send homing command ($H)
//!
//!  CONNECT TAB – console focused
//!   ↑ / ↓ / PgUp / PgDn / Home / End → scroll
//!
//!  CONNECT TAB – command input focused
//!   printable chars → edit buffer
//!   Enter           → send to GRBL
//!   ↑ / ↓          → command history
//!   ← / → / Home / End / Ctrl-← / Ctrl-→ → cursor movement
//!   Backspace / Del → delete
//!   Ctrl-U          → clear line
//!   Ctrl-K          → kill to end of line
//!   Esc             → clear buffer
//!
//!  GCODE TAB
//!   o              → open SVG (blocking rfd dialog)
//!   c              → convert loaded SVG → GCode
//!   s              → save GCode to file
//!   ↑ / ↓ / PgUp / PgDn / Home / End → scroll GCode text
//!
//!  PREVIEW TAB
//!   p              → render preview image

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    app::{ActiveTab, App, AppMode, ControlFocus, ConversionStatus, FocusedPane, MachineSettings},
    converter::{gcode_to_image, svg_to_gcode},
    grbl::{GrblLine, JogDir},
    serial::{SerialCommand, SerialEvent, discover_ports, spawn_serial_actor, validate_port_path},
};

// ── Poll ──────────────────────────────────────────────────────────────────────

/// Poll crossterm for a key event with `timeout`. Returns `None` on timeout.
pub fn poll_key(timeout: Duration) -> Result<Option<KeyEvent>> {
    if event::poll(timeout)? {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Release {
                return Ok(Some(key));
            }
        }
    }
    Ok(None)
}

// ── Serial event drain ────────────────────────────────────────────────────────

/// Drain all pending events from the serial actor and apply them to app state.
pub fn drain_serial_events(
    app: &mut App,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
) {
    let Some(rx) = serial_rx else { return };

    loop {
        match rx.try_recv() {
            Ok(SerialEvent::Line(line)) => {
                // Parse the line through the GRBL classifier and update app state.
                let parsed = GrblLine::parse(&line);
                match &parsed {
                    GrblLine::Status(st) => {
                        // Merge override values from the status report back into
                        // the app's UI sliders (so they reflect the real machine state).
                        if let Some(ov) = &st.overrides {
                            app.override_feed = ov.feed;
                            app.override_rapid = ov.rapid;
                            app.override_spindle = ov.spindle;
                        }
                        app.grbl_status = Some(st.clone());
                        // Status lines are frequent — only show in console when
                        // the user has explicitly polled (not on auto-poll) to
                        // avoid flooding the log.  We still push a minimal line.
                    }
                    GrblLine::GcState(s) => {
                        app.gc_state = Some(s.clone());
                    }
                    GrblLine::Setting { key, value } => {
                        app.upsert_setting(key.clone(), value.clone());
                    }
                    GrblLine::Probe { pos, success } => {
                        app.last_probe = Some((pos.clone(), *success));
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
                // Push all lines to the console (use parsed summary for
                // status/setting lines to keep them concise).
                match &parsed {
                    GrblLine::Status(_) => {
                        // Don't spam console with raw status lines.
                    }
                    GrblLine::Error(_) | GrblLine::Alarm(_) => {
                        // Already pushed as push_error above; skip push_received.
                    }
                    GrblLine::Setting { .. } => {
                        // Settings are displayed in the settings panel; skip console spam.
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
                app.mode = AppMode::Disconnected;
                app.connected_port = None;
                app.serial_tx = None;
                *serial_rx = None;
                if let Some(r) = reason {
                    app.push_error(format!("Disconnected: {r}"));
                } else {
                    app.push_info("Disconnected from serial port.");
                }
                app.focused = FocusedPane::SerialList;
                return;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                app.mode = AppMode::Disconnected;
                app.connected_port = None;
                app.serial_tx = None;
                *serial_rx = None;
                app.push_error("Serial actor disconnected unexpectedly.");
                app.focused = FocusedPane::SerialList;
                return;
            }
        }
    }
}

// ── Status polling tick ───────────────────────────────────────────────────────

/// Called once per main-loop tick (50 ms).  When connected and the poll
/// interval is non-zero, automatically sends a `?` status request to GRBL.
pub fn tick_status_poll(app: &mut App) {
    if app.mode != AppMode::Connected || app.status_poll_interval == 0 {
        app.status_poll_ticks = 0;
        return;
    }
    app.status_poll_ticks += 1;
    if app.status_poll_ticks >= app.status_poll_interval {
        app.status_poll_ticks = 0;
        // Send the real-time status character.  This uses a direct send
        // without logging to the console to avoid flooding it.
        if let Some(tx) = &app.serial_tx {
            let _ = tx.send(SerialCommand::Send("?".to_owned()));
        }
    }
}

// ── Main key dispatcher ───────────────────────────────────────────────────────

/// Handle one key event; returns `true` if a redraw is needed.
pub fn handle_key(
    app: &mut App,
    key: KeyEvent,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
    show_help: &mut bool,
) -> bool {
    // ── Dismiss help overlay ──────────────────────────────────────────────
    if *show_help {
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => {
                *show_help = false;
                return true;
            }
            _ => return false,
        }
    }

    // ── Global shortcuts ──────────────────────────────────────────────────
    match key.code {
        // Quit
        KeyCode::Char('q') | KeyCode::Char('Q')
            if key.modifiers == KeyModifiers::NONE && app.focused != FocusedPane::CommandInput =>
        {
            do_disconnect(app, serial_rx);
            app.should_quit = true;
            return true;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            do_disconnect(app, serial_rx);
            app.should_quit = true;
            return true;
        }

        // Help
        KeyCode::Char('?') => {
            *show_help = true;
            return true;
        }

        // Tab switching via F-keys — always pure tab switches, no side-effects
        KeyCode::F(1) => {
            app.active_tab = ActiveTab::Connect;
            return true;
        }
        // F2 → Control tab
        KeyCode::F(2) => {
            app.active_tab = ActiveTab::Control;
            return true;
        }
        // F3 → GCode tab
        KeyCode::F(3) => {
            app.active_tab = ActiveTab::GCode;
            return true;
        }
        // F4 → Preview tab
        KeyCode::F(4) => {
            app.active_tab = ActiveTab::Preview;
            return true;
        }
        // F5 → Settings tab
        KeyCode::F(5) => {
            app.active_tab = ActiveTab::Settings;
            return true;
        }

        // Pane focus cycling (Connect tab only) / Control focus cycling
        KeyCode::Tab if app.active_tab == ActiveTab::Connect => {
            cycle_focus(app);
            return true;
        }
        KeyCode::BackTab if app.active_tab == ActiveTab::Connect => {
            cycle_focus_rev(app);
            return true;
        }
        KeyCode::Tab if app.active_tab == ActiveTab::Control => {
            cycle_control_focus(app);
            return true;
        }
        KeyCode::BackTab if app.active_tab == ActiveTab::Control => {
            cycle_control_focus_rev(app);
            return true;
        }

        _ => {}
    }

    // ── Baud dropdown intercept ───────────────────────────────────────────
    if app.baud_dropdown_open {
        return handle_baud_dropdown(app, key);
    }

    // ── Tab-specific dispatch ─────────────────────────────────────────────
    match app.active_tab {
        ActiveTab::Connect => handle_connect_tab(app, key, serial_rx),
        ActiveTab::Control => handle_control_tab(app, key),
        ActiveTab::GCode => handle_gcode_tab(app, key),
        ActiveTab::Preview => handle_preview_tab(app, key),
        ActiveTab::Settings => handle_settings_tab(app, key),
    }
}

// ── Baud dropdown ─────────────────────────────────────────────────────────────

fn handle_baud_dropdown(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.baud_prev();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.baud_next();
            true
        }
        KeyCode::Enter | KeyCode::Esc => {
            app.baud_dropdown_open = false;
            true
        }
        _ => false,
    }
}

// ── Connect tab ───────────────────────────────────────────────────────────────

fn handle_connect_tab(
    app: &mut App,
    key: KeyEvent,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
) -> bool {
    match app.focused {
        FocusedPane::SerialList => handle_port_list(app, key, serial_rx),
        FocusedPane::Console => handle_console(app, key),
        FocusedPane::CommandInput => {
            if app.mode == AppMode::Connected {
                handle_command_input(app, key)
            } else {
                false
            }
        }
    }
}

fn handle_port_list(
    app: &mut App,
    key: KeyEvent,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.port_list_prev();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.port_list_next();
            true
        }
        KeyCode::Home => {
            if !app.port_list.is_empty() {
                app.port_list_state.select(Some(0));
            }
            true
        }
        KeyCode::End => {
            if !app.port_list.is_empty() {
                app.port_list_state.select(Some(app.port_list.len() - 1));
            }
            true
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            do_refresh_ports(app);
            true
        }
        KeyCode::Char('b') | KeyCode::Char('B') => {
            app.baud_dropdown_open = true;
            true
        }
        // d → disconnect when connected
        KeyCode::Char('d') | KeyCode::Char('D') => {
            if app.mode == AppMode::Connected {
                do_disconnect(app, serial_rx);
                true
            } else {
                false
            }
        }
        KeyCode::Enter => {
            if app.mode == AppMode::Connected {
                // Already connected: ignore
                false
            } else {
                do_connect(app, serial_rx);
                true
            }
        }
        _ => false,
    }
}

fn handle_console(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.console_scroll_up();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.console_scroll_down();
            true
        }
        KeyCode::PageUp => {
            app.console_scroll_page_up(10);
            true
        }
        KeyCode::PageDown => {
            app.console_scroll_page_down(10);
            true
        }
        KeyCode::End => {
            app.console_scroll_to_bottom();
            true
        }
        KeyCode::Home => {
            app.console_follow = false;
            app.console_scroll = app.console_lines.len() as u16;
            true
        }
        _ => false,
    }
}

fn handle_command_input(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        // Submit
        KeyCode::Enter => {
            let line = app.input_submit();
            if line.trim().is_empty() {
                return false;
            }
            send_serial(app, line);
            true
        }

        // History
        KeyCode::Up => {
            app.history_up();
            true
        }
        KeyCode::Down => {
            app.history_down();
            true
        }

        // Cursor movement
        KeyCode::Left => {
            if key.modifiers == KeyModifiers::CONTROL {
                word_left(app);
            } else {
                app.input_cursor_left();
            }
            true
        }
        KeyCode::Right => {
            if key.modifiers == KeyModifiers::CONTROL {
                word_right(app);
            } else {
                app.input_cursor_right();
            }
            true
        }
        KeyCode::Home => {
            app.input_cursor_home();
            true
        }
        KeyCode::End => {
            app.input_cursor_end();
            true
        }

        // Deletion
        KeyCode::Backspace => {
            app.input_delete_back();
            true
        }
        KeyCode::Delete => {
            app.input_delete_forward();
            true
        }

        // Ctrl-U: clear line
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            app.input_buffer.clear();
            app.input_cursor = 0;
            true
        }
        // Ctrl-K: kill to end
        KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
            app.input_buffer.truncate(app.input_cursor);
            true
        }

        // Printable characters
        KeyCode::Char(ch)
            if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.input_insert_char(ch);
            true
        }

        // Esc: clear buffer
        KeyCode::Esc => {
            app.input_buffer.clear();
            app.input_cursor = 0;
            app.history_idx = None;
            true
        }

        _ => false,
    }
}

// ── Control tab ───────────────────────────────────────────────────────────────

fn handle_control_tab(app: &mut App, key: KeyEvent) -> bool {
    match app.control_focus {
        ControlFocus::Jog => handle_control_jog(app, key),
        ControlFocus::Overrides => handle_control_overrides(app, key),
        ControlFocus::Settings => handle_control_settings(app, key),
    }
}

fn handle_control_jog(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        // ── XY jogging ──────────────────────────────────────────────────
        KeyCode::Left | KeyCode::Char('h') if key.modifiers == KeyModifiers::NONE => {
            do_jog(app, JogDir::XMinus);
            true
        }
        KeyCode::Right | KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
            do_jog(app, JogDir::XPlus);
            true
        }
        KeyCode::Up | KeyCode::Char('k') if key.modifiers == KeyModifiers::NONE => {
            do_jog(app, JogDir::YPlus);
            true
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers == KeyModifiers::NONE => {
            do_jog(app, JogDir::YMinus);
            true
        }

        // ── Z jogging ───────────────────────────────────────────────────
        KeyCode::PageUp => {
            do_jog(app, JogDir::ZPlus);
            true
        }
        KeyCode::PageDown => {
            do_jog(app, JogDir::ZMinus);
            true
        }

        // ── Step size ───────────────────────────────────────────────────
        KeyCode::Char('+') | KeyCode::Char('=') => {
            app.jog_step_larger();
            true
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            app.jog_step_smaller();
            true
        }

        // ── GRBL control ────────────────────────────────────────────────

        // ~ → cycle start / resume
        KeyCode::Char('~') => {
            send_serial_raw(app, "~");
            true
        }
        // ! → feed hold
        KeyCode::Char('!') => {
            send_serial_raw(app, "!");
            true
        }
        // Ctrl-X → soft reset
        KeyCode::Char('x') if key.modifiers == KeyModifiers::CONTROL => {
            send_serial_raw(app, "\x18");
            true
        }
        // $X → unlock alarm
        KeyCode::Char('X') if key.modifiers == KeyModifiers::SHIFT => {
            send_serial_raw(app, "$X");
            true
        }
        // $H → home
        KeyCode::Char('H') if key.modifiers == KeyModifiers::SHIFT => {
            do_home(app);
            true
        }
        // ? → manual status poll
        KeyCode::Char('?') => {
            do_poll_status(app);
            true
        }
        // $$ → dump settings
        KeyCode::Char('$') => {
            send_serial_raw(app, "$$");
            true
        }
        // $G → GCode parser state
        KeyCode::Char('g') | KeyCode::Char('G') => {
            send_serial_raw(app, "$G");
            true
        }
        // p → toggle auto-polling on/off
        KeyCode::Char('p') | KeyCode::Char('P') => {
            if app.status_poll_interval == 0 {
                app.status_poll_interval = 4; // ~200 ms at 50 ms tick
                app.push_info("Status auto-poll enabled (~200 ms).");
            } else {
                app.status_poll_interval = 0;
                app.push_info("Status auto-poll disabled.");
            }
            true
        }

        _ => false,
    }
}

fn handle_control_overrides(app: &mut App, key: KeyEvent) -> bool {
    // GRBL real-time override byte codes (0x90–0x9D):
    //   0x90 = feed 100%   0x91 = feed +10%   0x92 = feed -10%
    //   0x93 = feed +1%    0x94 = feed -1%
    //   0x95 = rapid 100%  0x96 = rapid 50%   0x97 = rapid 25%
    //   0x99 = laser 100%  0x9A = laser +10%  0x9B = laser -10%
    //   0x9C = laser +1%   0x9D = laser -1%
    match key.code {
        // Feed override
        KeyCode::Char('f') | KeyCode::Char('F') => {
            // Reset feed to 100%
            send_realtime(app, 0x90);
            app.override_feed = 100;
            true
        }
        KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
            send_realtime(app, 0x91); // feed +10%
            true
        }
        KeyCode::Down | KeyCode::Char('-') => {
            send_realtime(app, 0x92); // feed -10%
            true
        }
        KeyCode::Right => {
            send_realtime(app, 0x93); // feed +1%
            true
        }
        KeyCode::Left => {
            send_realtime(app, 0x94); // feed -1%
            true
        }
        // Rapid override
        KeyCode::Char('r') | KeyCode::Char('R') => {
            send_realtime(app, 0x95); // rapid 100%
            app.override_rapid = 100;
            true
        }
        KeyCode::Char('5') => {
            send_realtime(app, 0x96); // rapid 50%
            true
        }
        KeyCode::Char('2') => {
            send_realtime(app, 0x97); // rapid 25%
            true
        }
        // Laser power override
        KeyCode::Char('s') | KeyCode::Char('S') => {
            send_realtime(app, 0x99); // laser 100%
            app.override_spindle = 100;
            true
        }
        KeyCode::PageUp => {
            send_realtime(app, 0x9A); // laser +10%
            true
        }
        KeyCode::PageDown => {
            send_realtime(app, 0x9B); // laser -10%
            true
        }
        // ~ / ! / Ctrl-X as shortcuts even in override focus
        KeyCode::Char('~') => {
            send_serial_raw(app, "~");
            true
        }
        KeyCode::Char('!') => {
            send_serial_raw(app, "!");
            true
        }
        KeyCode::Char('x') if key.modifiers == KeyModifiers::CONTROL => {
            send_serial_raw(app, "\x18");
            true
        }
        _ => false,
    }
}

fn handle_control_settings(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.settings_prev();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.settings_next();
            true
        }
        // Refresh settings list
        KeyCode::Char('r') | KeyCode::Char('R') => {
            send_serial_raw(app, "$$");
            app.push_info("Requested GRBL settings ($$).");
            true
        }
        _ => false,
    }
}

fn cycle_control_focus(app: &mut App) {
    app.control_focus = match app.control_focus {
        ControlFocus::Jog => ControlFocus::Overrides,
        ControlFocus::Overrides => ControlFocus::Settings,
        ControlFocus::Settings => ControlFocus::Jog,
    };
}

fn cycle_control_focus_rev(app: &mut App) {
    app.control_focus = match app.control_focus {
        ControlFocus::Jog => ControlFocus::Settings,
        ControlFocus::Overrides => ControlFocus::Jog,
        ControlFocus::Settings => ControlFocus::Overrides,
    };
}

// ── GCode tab ─────────────────────────────────────────────────────────────────

fn handle_gcode_tab(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        // Open SVG
        KeyCode::Char('o') | KeyCode::Char('O') => {
            do_open_svg(app);
            true
        }

        // Convert
        KeyCode::Char('c') | KeyCode::Char('C') => {
            do_convert(app);
            true
        }

        // Save GCode
        KeyCode::Char('s') | KeyCode::Char('S') => {
            do_save_gcode(app);
            true
        }

        // Send GCode to connected GRBL machine
        KeyCode::Char('g') | KeyCode::Char('G') => {
            do_send_gcode(app);
            true
        }

        // Scroll
        KeyCode::Up | KeyCode::Char('k') => {
            app.gcode_scroll_up();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.gcode_scroll_down();
            true
        }
        KeyCode::PageUp => {
            app.gcode_scroll_page_up(20);
            true
        }
        KeyCode::PageDown => {
            app.gcode_scroll_page_down(20);
            true
        }
        KeyCode::Home => {
            app.gcode_scroll_top();
            true
        }
        KeyCode::End => {
            app.gcode_scroll_bottom();
            true
        }

        _ => false,
    }
}

// ── Preview tab ───────────────────────────────────────────────────────────────

fn handle_preview_tab(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('p') | KeyCode::Char('P') => {
            do_render_preview(app);
            true
        }
        _ => false,
    }
}

// ── Settings tab ──────────────────────────────────────────────────────────────

fn handle_settings_tab(app: &mut App, key: KeyEvent) -> bool {
    // ── Edit mode ─────────────────────────────────────────────────────────
    if app.settings_edit_buf.is_some() {
        return handle_settings_edit(app, key);
    }

    // ── Navigation mode ───────────────────────────────────────────────────
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.settings_tab_prev();
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.settings_tab_next();
            true
        }
        KeyCode::Home => {
            app.settings_selected = 0;
            true
        }
        KeyCode::End => {
            let max = crate::app::MachineSettings::field_count().saturating_sub(1);
            app.settings_selected = max;
            true
        }
        // Enter / e → start editing the selected field
        KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('E') => {
            let idx = app.settings_selected;
            // Boolean fields are toggled directly; all others open an edit buffer.
            if idx >= 12 {
                app.settings_toggle_bool();
            } else {
                app.settings_begin_edit();
            }
            true
        }
        // Space → toggle boolean fields
        KeyCode::Char(' ') => {
            app.settings_toggle_bool();
            true
        }
        // r → reset all settings to defaults
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.machine_settings = MachineSettings::default();
            app.set_status("Settings reset to defaults.", Some(80));
            true
        }
        _ => false,
    }
}

fn handle_settings_edit(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        // Confirm
        KeyCode::Enter => {
            if app.settings_commit_edit() {
                app.set_status("Setting saved.", Some(60));
            }
            true
        }
        // Cancel
        KeyCode::Esc => {
            app.settings_cancel_edit();
            true
        }
        // Backspace
        KeyCode::Backspace => {
            if let Some(buf) = app.settings_edit_buf.as_mut() {
                buf.pop();
            }
            true
        }
        // Printable characters
        KeyCode::Char(ch)
            if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
        {
            if let Some(buf) = app.settings_edit_buf.as_mut() {
                buf.push(ch);
            }
            true
        }
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Actions
// ═══════════════════════════════════════════════════════════════════════════════

// ── Port management ───────────────────────────────────────────────────────────

fn do_refresh_ports(app: &mut App) {
    let ports = discover_ports();
    let count = ports.len();
    app.port_list = ports;

    // Keep selection in-bounds
    let sel = app.port_list_state.selected().unwrap_or(0);
    if app.port_list.is_empty() {
        app.port_list_state.select(None);
    } else {
        app.port_list_state
            .select(Some(sel.min(app.port_list.len() - 1)));
    }

    app.push_info(format!("Port list refreshed – {count} port(s) found."));
}

fn do_connect(
    app: &mut App,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
) {
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
            *serial_rx = Some(evt_rx);
            app.focused = FocusedPane::CommandInput;

            // Send homing command immediately on connect
            app.push_info("Sending homing command ($H)…");
            send_serial_raw(app, "$H");
        }
        Err(e) => {
            app.push_error(format!("Failed to connect: {e}"));
        }
    }
}

fn do_disconnect(
    app: &mut App,
    serial_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<SerialEvent>>,
) {
    if app.mode == AppMode::Disconnected {
        return;
    }
    if let Some(tx) = app.serial_tx.take() {
        let _ = tx.send(SerialCommand::Disconnect);
    }
    *serial_rx = None;
    app.mode = AppMode::Disconnected;
    let port = app.connected_port.take().unwrap_or_default();
    app.push_info(format!("Disconnected from {port}."));
    app.focused = FocusedPane::SerialList;
}

// ── GRBL commands ─────────────────────────────────────────────────────────────

/// Send the GRBL homing cycle command.
fn do_home(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot home.");
        return;
    }
    app.push_info("Sending homing command ($H)…");
    send_serial_raw(app, "$H");
}

/// Send a jog command in the given direction using current step/feedrate.
fn do_jog(app: &mut App, dir: JogDir) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot jog.");
        return;
    }
    let step = app.jog_step_mm();
    let feed = app.jog_feedrate;
    let cmd = dir.to_command(step, feed);
    send_serial_raw(app, &cmd);
}

/// Send a single real-time byte to GRBL (override commands, feed-hold, etc.).
fn send_realtime(app: &mut App, byte: u8) {
    if app.mode != AppMode::Connected {
        return;
    }
    let s = std::str::from_utf8(&[byte])
        .map(str::to_owned)
        .unwrap_or_else(|_| format!("\\x{byte:02X}"));
    if let Some(tx) = &app.serial_tx {
        let _ = tx.send(SerialCommand::Send(s));
    }
}

/// Manually poll GRBL status once.
fn do_poll_status(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected.");
        return;
    }
    if let Some(tx) = &app.serial_tx {
        let _ = tx.send(SerialCommand::Send("?".to_owned()));
    }
}

/// Stream the generated GCode to the connected GRBL machine line-by-line.
///
/// Each non-empty, non-comment line is sent as a separate serial command.
/// The lines are enqueued via the serial actor's unbounded channel; the actor
/// sends them in order.  A summary is printed to the console.
fn do_send_gcode(app: &mut App) {
    if app.mode != AppMode::Connected {
        app.push_error("Not connected – cannot stream GCode.");
        return;
    }
    let gcode = match app.gcode_text.clone() {
        Some(g) => g,
        None => {
            app.push_error("No GCode to send. Convert an SVG first.");
            return;
        }
    };

    let lines: Vec<&str> = gcode
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with('('))
        .collect();

    let count = lines.len();
    if count == 0 {
        app.push_error("GCode is empty after filtering comments.");
        return;
    }

    app.push_info(format!("Streaming {count} GCode lines to GRBL…"));

    if let Some(tx) = &app.serial_tx {
        for line in &lines {
            if tx.send(SerialCommand::Send(line.to_string())).is_err() {
                app.push_error("Serial channel closed during GCode stream.");
                return;
            }
        }
    }

    app.set_status(format!("Streaming {count} lines…"), Some(200));
}

/// Send a raw string to the serial actor (no history push).
fn send_serial_raw(app: &mut App, cmd: &str) {
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

/// Send the contents of a line (from input submit) and log it.
fn send_serial(app: &mut App, line: String) {
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

// ── SVG open ──────────────────────────────────────────────────────────────────

/// Open a native file-picker dialog (blocking on current thread) and load
/// the chosen SVG path into `app.svg_path`.
///
/// Uses a simple stdin-based fallback when `rfd` cannot open a dialog
/// (e.g., headless CI).  Since crossterm is in raw mode we temporarily
/// pause it and restore it afterwards.
fn do_open_svg(app: &mut App) {
    // Suspend raw mode so the dialog / fallback works
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);

    let chosen = native_file_open_svg();

    // Restore terminal
    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
    app.needs_clear = true;

    match chosen {
        Some(path) => {
            app.push_info(format!("Loaded SVG: {}", path.display()));
            app.svg_path = Some(path);
            // Reset any previous conversion artefacts
            app.gcode_text = None;
            app.gcode_scroll = 0;
            app.conversion_status = ConversionStatus::Idle;
            app.preview_image = None;
            app.preview_protocol = None;
            app.preview_dirty = false;
            app.set_status("SVG loaded. Press 'c' to convert.", Some(120));
        }
        None => {
            app.set_status("No file selected.", Some(60));
        }
    }
}

/// Attempt to open a native GTK/macOS/Windows file picker.
/// Falls back to a simple prompt read from stderr/stdin when unavailable.
fn native_file_open_svg() -> Option<std::path::PathBuf> {
    // rfd::FileDialog is synchronous on Linux (via GTK).
    // It blocks until the user picks or cancels.
    let result = std::panic::catch_unwind(|| {
        rfd::FileDialog::new()
            .add_filter("SVG files", &["svg", "SVG"])
            .add_filter("All files", &["*"])
            .set_title("Open SVG file")
            .pick_file()
    });

    match result {
        Ok(path) => path,
        Err(_) => {
            // rfd panicked (no display, Wayland issue, etc.): fall back to
            // reading a path from stderr prompt.
            eprintln!("\nEnter SVG file path: ");
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_ok() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    Some(std::path::PathBuf::from(trimmed))
                } else {
                    None
                }
            } else {
                None
            }
        }
    }
}

// ── Convert ───────────────────────────────────────────────────────────────────

fn do_convert(app: &mut App) {
    let Some(svg_path) = app.svg_path.clone() else {
        app.push_error("No SVG loaded. Press 'o' to open one.");
        app.set_status("No SVG loaded.", Some(80));
        return;
    };

    app.conversion_status = ConversionStatus::Running;
    app.gcode_text = None;
    app.gcode_scroll = 0;
    app.preview_image = None;
    app.preview_protocol = None;
    app.preview_dirty = false;

    let settings = app.machine_settings.clone();

    match svg_to_gcode(&svg_path, &settings) {
        Ok(gcode) => {
            let line_count = gcode.lines().count();
            app.push_info(format!("Conversion OK – {} lines of GCode.", line_count));
            app.gcode_text = Some(gcode);
            app.gcode_scroll = 0;
            app.conversion_status = ConversionStatus::Ok;
            app.preview_dirty = true;
            app.set_status(format!("Converted: {} GCode lines.", line_count), Some(120));
        }
        Err(e) => {
            let msg = format!("{e:#}");
            app.push_error(format!("Conversion failed: {msg}"));
            app.conversion_status = ConversionStatus::Failed(msg.clone());
            app.set_status(format!("Conversion failed: {msg}"), Some(120));
        }
    }
}

// ── Save GCode ────────────────────────────────────────────────────────────────

fn do_save_gcode(app: &mut App) {
    // Clone early to end the immutable borrow before any mutable borrows below.
    let gcode_clone = match app.gcode_text.clone() {
        Some(g) => g,
        None => {
            app.push_error("No GCode to save. Convert an SVG first.");
            app.set_status("No GCode to save.", Some(80));
            return;
        }
    };

    // Derive a default save name from the SVG filename
    let default_name = app
        .svg_path
        .as_deref()
        .and_then(|p| p.file_stem())
        .map(|s| format!("{}.gcode", s.to_string_lossy()))
        .unwrap_or_else(|| "output.gcode".to_string());

    // Temporarily leave raw mode for the dialog
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);

    let chosen = std::panic::catch_unwind(|| {
        rfd::FileDialog::new()
            .add_filter("GCode files", &["gcode", "nc", "gc"])
            .add_filter("All files", &["*"])
            .set_file_name(&default_name)
            .set_title("Save GCode")
            .save_file()
    });

    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
    app.needs_clear = true;

    let path = match chosen {
        Ok(Some(p)) => p,
        Ok(None) => {
            app.set_status("Save cancelled.", Some(60));
            return;
        }
        Err(_) => {
            // rfd unavailable: construct a path next to the SVG
            let fallback = app
                .svg_path
                .as_deref()
                .and_then(|p| p.parent())
                .map(|dir| dir.join(&default_name))
                .unwrap_or_else(|| std::path::PathBuf::from(&default_name));
            app.push_info(format!(
                "Dialog unavailable, saving to: {}",
                fallback.display()
            ));
            fallback
        }
    };

    match std::fs::write(&path, gcode_clone.as_bytes()) {
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

// ── Preview render ────────────────────────────────────────────────────────────

/// Rasterise the GCode toolpath into an `RgbaImage` and store it in `app`.
///
/// The image picker from `ratatui-image` must be constructed *outside* the
/// render closure (in the event loop) so we only produce the raw `RgbaImage`
/// here; `main.rs` then encodes it into a `StatefulProtocol`.
fn do_render_preview(app: &mut App) {
    // Clone early so we don't hold an immutable borrow across mutable calls.
    let gcode = match app.gcode_text.clone() {
        Some(g) => g,
        None => {
            app.push_error("No GCode to preview. Convert an SVG first.");
            app.set_status("No GCode to preview.", Some(80));
            return;
        }
    };

    app.set_status("Rendering toolpath preview…", Some(20));

    // Reasonable pixel dimensions for the preview panel
    let max_w = 800u32;
    let max_h = 600u32;

    match gcode_to_image(&gcode, max_w, max_h) {
        Ok(img) => {
            app.preview_image = Some(img);
            app.preview_dirty = false;
            app.push_info("Toolpath preview rendered.");
            app.set_status("Preview ready.", Some(100));
        }
        Err(e) => {
            app.push_error(format!("Preview render failed: {e}"));
            app.set_status(format!("Preview failed: {e}"), Some(120));
        }
    }
}

// ── Focus helpers ─────────────────────────────────────────────────────────────

fn cycle_focus(app: &mut App) {
    app.focused = match app.focused {
        FocusedPane::SerialList => FocusedPane::Console,
        FocusedPane::Console => FocusedPane::CommandInput,
        FocusedPane::CommandInput => FocusedPane::SerialList,
    };
}

fn cycle_focus_rev(app: &mut App) {
    app.focused = match app.focused {
        FocusedPane::SerialList => FocusedPane::CommandInput,
        FocusedPane::Console => FocusedPane::SerialList,
        FocusedPane::CommandInput => FocusedPane::Console,
    };
}

// ── Word-jump helpers ─────────────────────────────────────────────────────────

fn word_left(app: &mut App) {
    let buf = app.input_buffer.clone();
    let mut pos = app.input_cursor;
    while pos > 0 {
        let p = prev_char_boundary(&buf, pos);
        if buf[p..pos]
            .chars()
            .next()
            .map_or(false, |c| c.is_whitespace())
        {
            pos = p;
        } else {
            break;
        }
    }
    while pos > 0 {
        let p = prev_char_boundary(&buf, pos);
        if buf[p..pos]
            .chars()
            .next()
            .map_or(false, |c| !c.is_whitespace())
        {
            pos = p;
        } else {
            break;
        }
    }
    app.input_cursor = pos;
}

fn word_right(app: &mut App) {
    let buf = app.input_buffer.clone();
    let len = buf.len();
    let mut pos = app.input_cursor;
    while pos < len {
        let n = next_char_boundary(&buf, pos);
        if buf[pos..n]
            .chars()
            .next()
            .map_or(false, |c| !c.is_whitespace())
        {
            pos = n;
        } else {
            break;
        }
    }
    while pos < len {
        let n = next_char_boundary(&buf, pos);
        if buf[pos..n]
            .chars()
            .next()
            .map_or(false, |c| c.is_whitespace())
        {
            pos = n;
        } else {
            break;
        }
    }
    app.input_cursor = pos;
}

fn prev_char_boundary(s: &str, mut pos: usize) -> usize {
    pos -= 1;
    while !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn next_char_boundary(s: &str, mut pos: usize) -> usize {
    pos += 1;
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}
