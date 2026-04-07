use std::{
    io::{BufRead, BufReader, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tokio::sync::mpsc;

use crate::app::BaudRate;

// ── Port discovery ────────────────────────────────────────────────────────────

/// Prefixes we consider "interesting" serial devices on Linux.
const SERIAL_PREFIXES: &[&str] = &["ttyUSB", "ttyACM", "ttyS", "ttyAMA", "rfcomm"];

/// Return a sorted list of likely serial port paths found under /dev.
pub fn discover_ports() -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };

    let mut ports: Vec<String> = read_dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let is_serial = SERIAL_PREFIXES
                .iter()
                .any(|prefix| name_str.starts_with(prefix));
            if is_serial {
                Some(format!("/dev/{name_str}"))
            } else {
                None
            }
        })
        .collect();

    ports.sort();
    ports
}

// ── Connection ────────────────────────────────────────────────────────────────

/// Settings that are fixed for GRBL connections:
/// 8 data bits, 1 stop bit, no parity, no flow control.
fn open_port(path: &str, baud: BaudRate) -> Result<Box<dyn SerialPort>> {
    serialport::new(path, baud.as_u32())
        .data_bits(DataBits::Eight)
        .stop_bits(StopBits::One)
        .parity(Parity::None)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(10))
        .open()
        .with_context(|| format!("Failed to open serial port {path}"))
}

// ── Serial actor ──────────────────────────────────────────────────────────────

/// Events the serial actor sends back to the UI task.
#[derive(Debug)]
pub enum SerialEvent {
    /// A complete line (without the trailing newline) received from the device.
    Line(String),
    /// The connection was closed (either by the user or due to an error).
    Disconnected(Option<String>),
    /// An informational / error message to surface in the console.
    Info(String),
    /// An error message to surface in the console.
    Error(String),
    /// A GCode stream is in progress: `sent` lines acknowledged so far out of `total`.
    StreamProgress { sent: usize, total: usize },
    /// The GCode stream finished successfully.
    StreamDone { total: usize },
    /// The GCode stream was aborted (by the user or due to an error).
    StreamAborted {
        sent: usize,
        total: usize,
        reason: String,
    },
}

/// Commands the UI sends to the serial actor.
#[derive(Debug)]
pub enum SerialCommand {
    /// Send a raw string to the device (a `\n` is appended automatically).
    Send(String),
    /// Send a single real-time byte to the device **without** appending `\n`.
    ///
    /// GRBL real-time commands (e.g. `0x85` = jog cancel, `~` = cycle start,
    /// `!` = feed hold, `0x18` = soft reset) must be sent as bare bytes so
    /// that GRBL's interrupt-driven receive handler picks them up immediately,
    /// regardless of the line buffer state.
    RealTimeByte(u8),
    /// Stream a list of GCode lines using GRBL's ok-gated simple-sender
    /// protocol: send one line, wait for `ok` or `error:N`, then send the
    /// next.  Progress events are emitted for every acknowledgement.
    Stream(Vec<String>),
    /// Abort an in-progress Stream (the current line finishes but no further
    /// lines are sent).
    AbortStream,
    /// Close the port and shut down the actor.
    Disconnect,
}

pub fn spawn_serial_actor(
    path: &str,
    baud: BaudRate,
) -> Result<(
    mpsc::UnboundedSender<SerialCommand>,
    mpsc::UnboundedReceiver<SerialEvent>,
)> {
    let port = open_port(path, baud)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SerialCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SerialEvent>();

    let path_owned = path.to_owned();

    // Run the blocking I/O on a dedicated OS thread so we don't block the
    // async runtime.
    std::thread::spawn(move || {
        serial_actor_loop(port, path_owned, cmd_rx, evt_tx);
    });

    Ok((cmd_tx, evt_rx))
}

// ── Internal writer message ───────────────────────────────────────────────────

/// Message type for the actor → writer thread channel.
///
/// Separates line-protocol text (needs `\n` appended) from raw real-time
/// bytes (must be forwarded verbatim, without a newline).
enum WriterMsg {
    /// A normal GCode / command line; `\n` will be appended before sending.
    Text(String),
    /// A single real-time byte; sent as-is, no trailing newline.
    RawByte(u8),
}

// ── Serial actor loop ─────────────────────────────────────────────────────────

/// Blocking loop that reads from the port and dispatches commands.
///
/// This runs entirely on a dedicated OS thread.
fn serial_actor_loop(
    port: Box<dyn SerialPort>,
    path: String,
    mut cmd_rx: mpsc::UnboundedReceiver<SerialCommand>,
    evt_tx: mpsc::UnboundedSender<SerialEvent>,
) {
    // We need two handles to the port: one for reading, one for writing.
    let write_port = match port.try_clone() {
        Ok(p) => p,
        Err(e) => {
            let _ = evt_tx.send(SerialEvent::Error(format!(
                "Failed to clone port for writing: {e}"
            )));
            let _ = evt_tx.send(SerialEvent::Disconnected(Some(
                "Could not clone serial port".into(),
            )));
            return;
        }
    };

    let _ = evt_tx.send(SerialEvent::Info(format!("Connected to {path}")));

    // Spawn a secondary thread for writes so reads are never blocked by I/O.
    let (write_tx, write_rx) = std::sync::mpsc::channel::<Option<WriterMsg>>();
    let evt_tx_writer = evt_tx.clone();
    std::thread::spawn(move || {
        writer_loop(write_port, write_rx, evt_tx_writer);
    });

    // Read loop – runs on this thread.
    let mut reader = BufReader::new(port);
    let mut line_buf = String::new();

    // When Some(_), we are in streaming mode.
    let mut stream: Option<StreamState> = None;

    loop {
        // ── Check for pending commands (non-blocking) ─────────────────────
        loop {
            match cmd_rx.try_recv() {
                Ok(SerialCommand::Send(text)) => {
                    if stream.is_some() {
                        // Ignore raw sends during streaming to avoid
                        // confusing GRBL's ok sequencing.
                        let _ = evt_tx.send(SerialEvent::Info(
                            "Raw send ignored while streaming – abort stream first.".into(),
                        ));
                    } else if write_tx.send(Some(WriterMsg::Text(text))).is_err() {
                        break;
                    }
                }
                Ok(SerialCommand::RealTimeByte(byte)) => {
                    // Real-time bytes bypass streaming mode – they must reach
                    // GRBL immediately (e.g. jog cancel 0x85, feed hold, reset).
                    if write_tx.send(Some(WriterMsg::RawByte(byte))).is_err() {
                        break;
                    }
                }
                Ok(SerialCommand::Stream(lines)) => {
                    if stream.is_some() {
                        let _ = evt_tx.send(SerialEvent::Info(
                            "Already streaming – ignoring duplicate Stream command.".into(),
                        ));
                    } else if lines.is_empty() {
                        let _ = evt_tx.send(SerialEvent::StreamDone { total: 0 });
                    } else {
                        let total = lines.len();
                        let _ = evt_tx.send(SerialEvent::Info(format!(
                            "Starting GCode stream: {total} lines"
                        )));
                        // Send the very first line immediately.
                        let first = lines[0].clone();
                        if write_tx.send(Some(WriterMsg::Text(first))).is_err() {
                            break;
                        }
                        stream = Some(StreamState {
                            lines,
                            next_idx: 1, // next line to send after ack
                            sent: 0,     // lines acknowledged so far
                            abort: false,
                        });
                    }
                }
                Ok(SerialCommand::AbortStream) => {
                    if let Some(ref st) = stream {
                        let _ = evt_tx.send(SerialEvent::StreamAborted {
                            sent: st.sent,
                            total: st.lines.len(),
                            reason: "Aborted by user".into(),
                        });
                        stream = None;
                    }
                }
                Ok(SerialCommand::Disconnect) => {
                    let _ = write_tx.send(None); // None = shutdown signal
                    let _ = evt_tx.send(SerialEvent::Disconnected(None));
                    return;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    let _ = write_tx.send(None); // None = shutdown signal
                    let _ = evt_tx.send(SerialEvent::Disconnected(None));
                    return;
                }
            }
        }

        // ── Try to read a line ────────────────────────────────────────────
        line_buf.clear();
        match reader.read_line(&mut line_buf) {
            Ok(0) => {
                // EOF – port closed by device
                let _ = write_tx.send(None);
                let _ = evt_tx.send(SerialEvent::Error("Serial port closed by device".into()));
                let _ = evt_tx.send(SerialEvent::Disconnected(Some(
                    "Port closed by device".into(),
                )));
                return;
            }
            Ok(_) => {
                let text = line_buf.trim_end_matches(['\r', '\n']).to_string();
                if text.is_empty() {
                    continue;
                }

                // Always forward the raw line to the UI.
                let _ = evt_tx.send(SerialEvent::Line(text.clone()));

                // ── ok-gated streaming ────────────────────────────────────
                if let Some(ref mut st) = stream {
                    match streaming_step(st, &text) {
                        StepOutcome::NotAnAck => {
                            // Non-ok/error lines (status reports, messages) are
                            // already forwarded above; streaming is not affected.
                        }
                        StepOutcome::ErrorAbort {
                            sent,
                            total,
                            reason,
                        } => {
                            let _ = evt_tx.send(SerialEvent::StreamAborted {
                                sent,
                                total,
                                reason,
                            });
                            stream = None;
                        }
                        StepOutcome::Done { total } => {
                            let _ = evt_tx.send(SerialEvent::StreamProgress { sent: total, total });
                            let _ = evt_tx.send(SerialEvent::StreamDone { total });
                            stream = None;
                        }
                        StepOutcome::SendNext(next) => {
                            let total = st.lines.len();
                            let _ = evt_tx.send(SerialEvent::StreamProgress {
                                sent: st.sent,
                                total,
                            });
                            if write_tx.send(Some(WriterMsg::Text(next))).is_err() {
                                let _ = evt_tx.send(SerialEvent::StreamAborted {
                                    sent: st.sent,
                                    total,
                                    reason: "Writer thread died".into(),
                                });
                                stream = None;
                            }
                        }
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // No data available yet – yield briefly and loop
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                let _ = write_tx.send(None);
                let _ = evt_tx.send(SerialEvent::Error(format!("Read error: {e}")));
                let _ = evt_tx.send(SerialEvent::Disconnected(Some(format!("Read error: {e}"))));
                return;
            }
        }
    }
}

// ── Writer loop ───────────────────────────────────────────────────────────────

/// Blocking loop that writes messages to the serial port.
///
/// Receives `Option<WriterMsg>` from the actor loop via a std channel.
/// `None` is the shutdown signal.  Text messages have `\n` appended;
/// raw bytes are forwarded verbatim so that GRBL real-time commands
/// (e.g. jog cancel `0x85`) arrive as single bytes without a trailing
/// newline.
fn writer_loop(
    mut port: Box<dyn SerialPort>,
    rx: std::sync::mpsc::Receiver<Option<WriterMsg>>,
    evt_tx: mpsc::UnboundedSender<SerialEvent>,
) {
    for msg in &rx {
        match msg {
            None => return, // Shutdown signal
            Some(WriterMsg::Text(text)) => {
                let to_send = format!("{text}\n");
                if let Err(e) = port.write_all(to_send.as_bytes()) {
                    let _ = evt_tx.send(SerialEvent::Error(format!("Write error: {e}")));
                    return;
                }
                if let Err(e) = port.flush() {
                    let _ = evt_tx.send(SerialEvent::Error(format!("Flush error: {e}")));
                    return;
                }
            }
            Some(WriterMsg::RawByte(byte)) => {
                // Real-time byte: send verbatim, no newline appended.
                if let Err(e) = port.write_all(&[byte]) {
                    let _ = evt_tx.send(SerialEvent::Error(format!(
                        "Write error (real-time byte 0x{byte:02X}): {e}"
                    )));
                    return;
                }
                if let Err(e) = port.flush() {
                    let _ = evt_tx.send(SerialEvent::Error(format!("Flush error: {e}")));
                    return;
                }
            }
        }
    }
}

// ── Validate helpers (unit-testable) ─────────────────────────────────────────

/// Sanity-check a path before trying to open it.
pub fn validate_port_path(path: &str) -> Result<()> {
    if !path.starts_with("/dev/") {
        bail!("Port path must start with /dev/");
    }
    if !std::path::Path::new(path).exists() {
        bail!("Device {path} does not exist");
    }
    Ok(())
}

// ── ok-gated streaming ────────────────────────────────────────────────────────

/// State kept while streaming GCode line by line.
pub(crate) struct StreamState {
    /// All lines to be sent.
    lines: Vec<String>,
    /// Index of the next line to send (after the current ack).
    next_idx: usize,
    /// Number of lines acknowledged so far.
    sent: usize,
    /// When true, stop after the current in-flight line is acked.
    abort: bool,
}

/// The outcome of processing one inbound GRBL acknowledgement line while
/// streaming.  Returned by [`streaming_step`] so the logic can be tested
/// without a real serial port.
#[derive(Debug, PartialEq)]
pub(crate) enum StepOutcome {
    /// Send the next GCode line (payload) and continue streaming.
    SendNext(String),
    /// All lines have been acknowledged — streaming is complete.
    Done { total: usize },
    /// An `error:N` response was received — streaming must be aborted.
    ErrorAbort {
        sent: usize,
        total: usize,
        reason: String,
    },
    /// The inbound line was not an ok/error acknowledgement; streaming
    /// continues unaffected (e.g. a status report `<Idle|…>`).
    NotAnAck,
}

/// Pure streaming step: given the current [`StreamState`] and an inbound line
/// from GRBL, decide what to do next.
///
/// The caller is responsible for actually sending the next line and for
/// emitting the appropriate [`SerialEvent`]s.  This separation makes the
/// ok-gated protocol logic unit-testable without a real serial port.
pub(crate) fn streaming_step(st: &mut StreamState, line: &str) -> StepOutcome {
    let is_ok = line.eq_ignore_ascii_case("ok");
    let is_error = line.to_ascii_lowercase().starts_with("error:");

    if !is_ok && !is_error {
        return StepOutcome::NotAnAck;
    }

    st.sent += 1;
    let total = st.lines.len();

    if is_error {
        return StepOutcome::ErrorAbort {
            sent: st.sent,
            total,
            reason: format!("GRBL reported '{}' on line {} of {}", line, st.sent, total),
        };
    }

    // It was an "ok".
    if st.abort || st.next_idx >= total {
        StepOutcome::Done { total: st.sent }
    } else {
        let next = st.lines[st.next_idx].clone();
        st.next_idx += 1;
        StepOutcome::SendNext(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_returns_sorted_list() {
        // Just ensure it doesn't panic and returns a Vec (may be empty in CI)
        let ports = discover_ports();
        let mut sorted = ports.clone();
        sorted.sort();
        assert_eq!(ports, sorted);
    }

    #[test]
    fn validate_rejects_non_dev_paths() {
        assert!(validate_port_path("/tmp/fake").is_err());
        assert!(validate_port_path("ttyUSB0").is_err());
    }

    #[test]
    fn validate_rejects_missing_device() {
        assert!(validate_port_path("/dev/ttyUSB_does_not_exist_xyz").is_err());
    }

    // ── streaming_step ────────────────────────────────────────────────────────

    fn make_stream(lines: &[&str]) -> StreamState {
        StreamState {
            lines: lines.iter().map(|s| s.to_string()).collect(),
            next_idx: 1, // first line already in-flight
            sent: 0,
            abort: false,
        }
    }

    #[test]
    fn step_non_ack_line_is_ignored() {
        let mut st = make_stream(&["G0 X0", "G1 X10 S500"]);
        let outcome = streaming_step(&mut st, "<Idle|MPos:0,0,0|FS:0,0>");
        assert_eq!(outcome, StepOutcome::NotAnAck);
        // State must be unchanged
        assert_eq!(st.sent, 0);
        assert_eq!(st.next_idx, 1);
    }

    #[test]
    fn step_ok_on_single_line_stream_completes() {
        // Only one line — first ok means Done
        let mut st = make_stream(&["G0 X0"]);
        // next_idx starts at 1 which equals lines.len(), so Done immediately
        let outcome = streaming_step(&mut st, "ok");
        assert_eq!(outcome, StepOutcome::Done { total: 1 });
        assert_eq!(st.sent, 1);
    }

    #[test]
    fn step_ok_advances_to_next_line() {
        let mut st = make_stream(&["G0 X0", "G1 X10 S500", "G0 X0"]);
        // First ok → send line index 1
        let outcome = streaming_step(&mut st, "ok");
        assert_eq!(outcome, StepOutcome::SendNext("G1 X10 S500".to_string()));
        assert_eq!(st.sent, 1);
        assert_eq!(st.next_idx, 2);
    }

    #[test]
    fn step_ok_case_insensitive() {
        let mut st = make_stream(&["G0 X0"]);
        // GRBL sometimes sends "OK" in uppercase
        let outcome = streaming_step(&mut st, "OK");
        assert_eq!(outcome, StepOutcome::Done { total: 1 });
    }

    #[test]
    fn step_full_sequence_all_oks() {
        // Simulate a 3-line stream with all oks
        let lines = &["G0 X0", "G1 X10 S500", "G0 X0"];
        let mut st = make_stream(lines);

        // ok 1 → SendNext line[1]
        match streaming_step(&mut st, "ok") {
            StepOutcome::SendNext(l) => assert_eq!(l, "G1 X10 S500"),
            other => panic!("expected SendNext, got {other:?}"),
        }

        // ok 2 → SendNext line[2]
        match streaming_step(&mut st, "ok") {
            StepOutcome::SendNext(l) => assert_eq!(l, "G0 X0"),
            other => panic!("expected SendNext, got {other:?}"),
        }

        // ok 3 → Done
        match streaming_step(&mut st, "ok") {
            StepOutcome::Done { total } => assert_eq!(total, 3),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn step_error_response_aborts_immediately() {
        let mut st = make_stream(&["G0 X0", "G1 X10 S500", "G0 X0"]);
        let outcome = streaming_step(&mut st, "error:22");
        match outcome {
            StepOutcome::ErrorAbort {
                sent,
                total,
                reason,
            } => {
                assert_eq!(sent, 1);
                assert_eq!(total, 3);
                assert!(reason.contains("error:22"), "reason: {reason}");
            }
            other => panic!("expected ErrorAbort, got {other:?}"),
        }
    }

    #[test]
    fn step_error_is_case_insensitive() {
        let mut st = make_stream(&["G0 X0"]);
        let outcome = streaming_step(&mut st, "ERROR:5");
        assert!(
            matches!(outcome, StepOutcome::ErrorAbort { .. }),
            "expected ErrorAbort for uppercase ERROR:5, got {outcome:?}"
        );
    }

    #[test]
    fn step_error_on_last_line_still_aborts() {
        // Even if it's the last line, an error response must abort, not Done.
        let mut st = make_stream(&["G0 X0"]);
        // Manually advance so this is the last ack
        let outcome = streaming_step(&mut st, "error:1");
        assert!(
            matches!(outcome, StepOutcome::ErrorAbort { .. }),
            "expected ErrorAbort on last line error, got {outcome:?}"
        );
    }

    #[test]
    fn step_abort_flag_causes_done_on_next_ok() {
        let mut st = make_stream(&["G0 X0", "G1 X10 S500", "G0 X0"]);
        st.abort = true;
        // Even though there are more lines, abort=true means Done after next ok
        let outcome = streaming_step(&mut st, "ok");
        assert!(
            matches!(outcome, StepOutcome::Done { .. }),
            "expected Done when abort=true, got {outcome:?}"
        );
    }

    #[test]
    fn step_sent_counter_increments_on_each_ack() {
        let lines = &["A", "B", "C", "D"];
        let mut st = make_stream(lines);
        streaming_step(&mut st, "ok"); // sent=1
        streaming_step(&mut st, "ok"); // sent=2
        assert_eq!(st.sent, 2);
    }

    #[test]
    fn step_next_idx_increments_correctly() {
        let lines = &["A", "B", "C", "D"];
        let mut st = make_stream(lines);
        assert_eq!(st.next_idx, 1);
        streaming_step(&mut st, "ok"); // sends B, next_idx→2
        assert_eq!(st.next_idx, 2);
        streaming_step(&mut st, "ok"); // sends C, next_idx→3
        assert_eq!(st.next_idx, 3);
    }

    #[test]
    fn step_empty_line_is_not_an_ack() {
        let mut st = make_stream(&["G0 X0"]);
        let outcome = streaming_step(&mut st, "");
        assert_eq!(outcome, StepOutcome::NotAnAck);
    }

    #[test]
    fn step_status_line_between_oks_does_not_affect_state() {
        // GRBL may interleave status reports between ok responses
        let lines = &["G0 X0", "G1 X10 S500"];
        let mut st = make_stream(lines);

        // Status report — not an ack
        assert_eq!(
            streaming_step(&mut st, "<Idle|MPos:0,0,0|FS:0,0>"),
            StepOutcome::NotAnAck
        );
        assert_eq!(st.sent, 0);

        // Now the real ok — should advance
        match streaming_step(&mut st, "ok") {
            StepOutcome::SendNext(l) => assert_eq!(l, "G1 X10 S500"),
            other => panic!("expected SendNext, got {other:?}"),
        }
    }
}
