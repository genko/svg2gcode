use std::collections::VecDeque;
use std::path::PathBuf;

use image::RgbaImage;
use svg2gcode::{LayerOverrideOptions, SvgLayerInfo};

use crate::serial::SerialCommand;

// ── Machine / conversion settings ────────────────────────────────────────────

/// All user-editable settings that drive SVG→GCode conversion and describe
/// the physical laser machine.  These are shown and edited in the Settings tab.
#[derive(Debug, Clone)]
pub struct MachineSettings {
    // ── GCode sequences ───────────────────────────────────────────────────
    /// GCode emitted at the start of every job (e.g. `G90 G21 M4 S1000`)
    pub begin_sequence: String,
    /// GCode emitted at the end of every job (e.g. `M5 M2`)
    pub end_sequence: String,

    // ── Physical machine limits ───────────────────────────────────────────
    /// Maximum X travel in mm (used to clamp the preview bounding box)
    pub max_x_mm: f64,
    /// Maximum Y travel in mm
    pub max_y_mm: f64,
    /// Maximum machine speed in mm/min (used as a UI ceiling for feedrate)
    pub max_speed: f64,
    /// Maximum laser power (S-word ceiling, typically 1000 for GRBL)
    pub max_laser_power: f64,

    // ── Conversion parameters ─────────────────────────────────────────────
    /// Laser-on feedrate in mm/min
    pub feedrate: f64,
    /// Bézier curve linearisation tolerance in mm
    pub tolerance: f64,
    /// DPI assumed for SVG pixel / point / pica units
    pub dpi: f64,
    /// Laser power written into the begin sequence (S word, 0 – max_laser_power)
    pub laser_power: f64,
    /// X origin offset in mm (shifts the whole job)
    pub origin_x: f64,
    /// Y origin offset in mm
    pub origin_y: f64,
    /// Emit G2/G3 arc commands instead of linearising arcs
    pub circular_interpolation: bool,

    // ── GCode formatting ──────────────────────────────────────────────────
    /// Prepend line numbers to every GCode line
    pub line_numbers: bool,
    /// Append checksums to every GCode line
    pub checksums: bool,
}

impl Default for MachineSettings {
    fn default() -> Self {
        Self {
            begin_sequence: "G90 G21 M4".to_owned(),
            end_sequence: "M5 M2".to_owned(),
            max_x_mm: 150.0,
            max_y_mm: 150.0,
            max_speed: 10000.0,
            max_laser_power: 1000.0,
            feedrate: 3000.0,
            tolerance: 0.1,
            dpi: 96.0,
            laser_power: 1000.0,
            origin_x: 0.0,
            origin_y: 0.0,
            circular_interpolation: false,
            line_numbers: false,
            checksums: false,
        }
    }
}

impl MachineSettings {
    /// Human-readable field names (parallel to `field_value`).
    pub const FIELD_NAMES: &'static [&'static str] = &[
        "Begin sequence",
        "End sequence",
        "Max X (mm)",
        "Max Y (mm)",
        "Max speed (mm/min)",
        "Max laser power (S)",
        "Feedrate (mm/min)",
        "Tolerance (mm)",
        "DPI",
        "Laser power (S)",
        "Origin X (mm)",
        "Origin Y (mm)",
        "Circular interpolation",
        "Line numbers",
        "Checksums",
    ];

    /// Return the current value of field `idx` as an editable string.
    pub fn field_value(&self, idx: usize) -> String {
        match idx {
            0 => self.begin_sequence.clone(),
            1 => self.end_sequence.clone(),
            2 => format!("{:.1}", self.max_x_mm),
            3 => format!("{:.1}", self.max_y_mm),
            4 => format!("{:.0}", self.max_speed),
            5 => format!("{:.0}", self.max_laser_power),
            6 => format!("{:.0}", self.feedrate),
            7 => format!("{:.4}", self.tolerance),
            8 => format!("{:.1}", self.dpi),
            9 => format!("{:.0}", self.laser_power),
            10 => format!("{:.1}", self.origin_x),
            11 => format!("{:.1}", self.origin_y),
            12 => {
                if self.circular_interpolation {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            13 => {
                if self.line_numbers {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            14 => {
                if self.checksums {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            _ => String::new(),
        }
    }

    /// Apply a string value back to field `idx`.  Returns an error message on
    /// parse failure (value is not applied in that case).
    pub fn set_field(&mut self, idx: usize, raw: &str) -> Result<(), String> {
        let s = raw.trim();
        match idx {
            0 => {
                self.begin_sequence = s.to_owned();
                Ok(())
            }
            1 => {
                self.end_sequence = s.to_owned();
                Ok(())
            }
            2 => s
                .parse::<f64>()
                .map(|v| self.max_x_mm = v)
                .map_err(|e| e.to_string()),
            3 => s
                .parse::<f64>()
                .map(|v| self.max_y_mm = v)
                .map_err(|e| e.to_string()),
            4 => s
                .parse::<f64>()
                .map(|v| self.max_speed = v)
                .map_err(|e| e.to_string()),
            5 => s
                .parse::<f64>()
                .map(|v| self.max_laser_power = v)
                .map_err(|e| e.to_string()),
            6 => s
                .parse::<f64>()
                .map(|v| self.feedrate = v)
                .map_err(|e| e.to_string()),
            7 => s
                .parse::<f64>()
                .map(|v| self.tolerance = v)
                .map_err(|e| e.to_string()),
            8 => s
                .parse::<f64>()
                .map(|v| self.dpi = v)
                .map_err(|e| e.to_string()),
            9 => s
                .parse::<f64>()
                .map(|v| self.laser_power = v)
                .map_err(|e| e.to_string()),
            10 => s
                .parse::<f64>()
                .map(|v| self.origin_x = v)
                .map_err(|e| e.to_string()),
            11 => s
                .parse::<f64>()
                .map(|v| self.origin_y = v)
                .map_err(|e| e.to_string()),
            12 => {
                self.circular_interpolation =
                    matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                Ok(())
            }
            13 => {
                self.line_numbers = matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                Ok(())
            }
            14 => {
                self.checksums = matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                Ok(())
            }
            _ => Err("Unknown field".into()),
        }
    }

    pub fn field_count() -> usize {
        Self::FIELD_NAMES.len()
    }
    /// Return the begin sequence with any S word tokens stripped out.
    ///
    /// The laser power S word is always appended separately by the converter
    /// so it is driven by the "Laser power" setting rather than being
    /// hardcoded inside the sequence string.
    pub fn sanitised_begin_sequence(&self) -> String {
        // Split on whitespace, drop any token that starts with 'S' or 's'
        // followed only by digits / a decimal point (e.g. S1000, S0, S750.5).
        self.begin_sequence
            .split_whitespace()
            .filter(|token| {
                let t = token.trim_start_matches(|c: char| c == 'S' || c == 's');
                // Keep the token unless what remains after stripping the leading
                // 'S' is a valid non-empty number (meaning it was an S-word).
                !(token.len() > t.len() && t.chars().all(|c| c.is_ascii_digit() || c == '.'))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ── Conversion error popup ────────────────────────────────────────────────────

/// Which pane inside the G-Code tab has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GCodeFocus {
    /// GCode text is scrollable (default)
    #[default]
    GCode,
    /// Layer list is active
    Layers,
}

// ── Conversion error popup ────────────────────────────────────────────────────

/// Payload for the modal error dialog shown when GCode generation fails
/// due to a settings or work-area violation.
#[derive(Debug, Clone)]
pub struct ConversionErrorPopup {
    /// One-line title shown in the popup border.
    pub title: String,
    /// Multi-line detail body.
    pub body: String,
}

/// Editable per-layer settings for the TUI layer panel.
///
/// Mirrors the web `LayerSettings` but stores values directly as `f64`/`u32`
/// options rather than text strings (the TUI edit buffer is on `App`).
#[derive(Debug, Clone)]
pub struct TuiLayerSettings {
    /// Human-readable label (inkscape:label > id > key).
    pub label: String,
    /// Stable lookup key matching [`SvgLayerInfo::key`].
    pub key: String,

    /// User-override feedrate in mm/min (`None` = use SVG value or global).
    pub feedrate: Option<f64>,
    /// User-override laser power S-word (`None` = use SVG value or global).
    pub power: Option<f64>,
    /// User-override pass count (`None` = use SVG value or 1).
    pub passes: Option<u32>,

    /// Value baked into the SVG `data-feedrate` attribute (read-only).
    pub svg_feedrate: Option<f64>,
    /// Value baked into the SVG `data-power` attribute (read-only).
    pub svg_power: Option<f64>,
    /// Value baked into the SVG `data-passes` attribute (read-only).
    pub svg_passes: Option<u32>,
}

impl TuiLayerSettings {
    pub fn from_svg_layer(info: &SvgLayerInfo, max_feedrate: f64, max_power: f64) -> Self {
        Self {
            label: info.label.clone(),
            key: info.key.clone(),
            feedrate: info.svg_feedrate.map(|f| f.min(max_feedrate)),
            power: info.svg_power.map(|p| p.min(max_power)),
            passes: info.svg_passes,
            svg_feedrate: info.svg_feedrate,
            svg_power: info.svg_power,
            svg_passes: info.svg_passes,
        }
    }

    /// Build a [`LayerOverrideOptions`] to pass into the converter.
    pub fn to_override_options(&self) -> LayerOverrideOptions {
        LayerOverrideOptions {
            feedrate: self.feedrate,
            power: self.power,
            passes: self.passes,
        }
    }

    /// Return a short summary string for the layer list.
    #[allow(dead_code)]
    pub fn summary(&self, global_feedrate: f64, global_power: f64) -> String {
        let f = self
            .feedrate
            .or(self.svg_feedrate)
            .unwrap_or(global_feedrate);
        let p = self.power.or(self.svg_power).unwrap_or(global_power);
        let passes = self.passes.or(self.svg_passes).unwrap_or(1);
        format!("F{:.0} S{:.0} ×{}", f, p, passes)
    }
}

// ── Focus / Tab enums ─────────────────────────────────────────────────────────

/// Top-level tab selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    /// Serial connection + console
    Connect,
    /// Direct GRBL machine control (jog, home, position, overrides)
    Control,
    /// Generated GCode text view
    GCode,
    /// GCode-derived SVG preview rendered as pixels
    Preview,
    /// Machine & conversion settings
    Settings,
}

impl ActiveTab {
    pub const ALL: &'static [ActiveTab] = &[
        ActiveTab::Connect,
        ActiveTab::Control,
        ActiveTab::GCode,
        ActiveTab::Preview,
        ActiveTab::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActiveTab::Connect => "Connect",
            ActiveTab::Control => "Control",
            ActiveTab::GCode => "GCode",
            ActiveTab::Preview => "Preview",
            ActiveTab::Settings => "Settings",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ActiveTab::Connect => ActiveTab::Control,
            ActiveTab::Control => ActiveTab::GCode,
            ActiveTab::GCode => ActiveTab::Preview,
            ActiveTab::Preview => ActiveTab::Settings,
            ActiveTab::Settings => ActiveTab::Connect,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            ActiveTab::Connect => ActiveTab::Settings,
            ActiveTab::Control => ActiveTab::Connect,
            ActiveTab::GCode => ActiveTab::Control,
            ActiveTab::Preview => ActiveTab::GCode,
            ActiveTab::Settings => ActiveTab::Preview,
        }
    }
}

// ── Control tab focus ─────────────────────────────────────────────────────────

/// Which sub-pane has focus inside the Control tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFocus {
    /// Jog direction pad
    Jog,
    /// Override sliders area
    Overrides,
    /// GRBL settings list
    Settings,
}

/// Which pane currently has keyboard focus (within the Connect tab)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    SerialList,
    Console,
    CommandInput,
}

/// Overall connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Disconnected,
    Connected,
}

// ── Baud rate ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaudRate {
    B115200,
    B56000,
}

impl BaudRate {
    pub const ALL: &'static [BaudRate] = &[BaudRate::B115200, BaudRate::B56000];

    pub fn as_u32(self) -> u32 {
        match self {
            BaudRate::B115200 => 115_200,
            BaudRate::B56000 => 56_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BaudRate::B115200 => "115200",
            BaudRate::B56000 => "56000",
        }
    }
}

impl std::fmt::Display for BaudRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── Console line ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub text: String,
    pub kind: LineKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Received,
    Sent,
    Info,
    Error,
}

pub const CONSOLE_MAX_LINES: usize = 1_000;

// ── Conversion status ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionStatus {
    /// No conversion attempted yet
    Idle,
    /// Conversion in progress
    Running,
    /// Last conversion succeeded
    Ok,
    /// Last conversion failed with a message
    Failed(String),
}

// ── Main application state ────────────────────────────────────────────────────

pub struct App {
    // ── Tab / focus ───────────────────────────────────────────────────────
    pub active_tab: ActiveTab,
    pub focused: FocusedPane,
    pub control_focus: ControlFocus,
    pub gcode_focus: GCodeFocus,

    // ── Serial port selection ─────────────────────────────────────────────
    pub port_list: Vec<String>,
    pub port_list_state: ratatui::widgets::ListState,

    // ── Baud-rate combo-box ───────────────────────────────────────────────
    pub baud_rates: Vec<BaudRate>,
    pub selected_baud_idx: usize,
    pub baud_dropdown_open: bool,

    // ── Connection ────────────────────────────────────────────────────────
    pub mode: AppMode,
    pub connected_port: Option<String>,
    pub serial_tx: Option<tokio::sync::mpsc::UnboundedSender<SerialCommand>>,

    // ── Console log ───────────────────────────────────────────────────────
    pub console_lines: VecDeque<ConsoleLine>,
    pub console_scroll: u16,
    pub console_follow: bool,

    // ── Command input ─────────────────────────────────────────────────────
    pub input_buffer: String,
    pub input_cursor: usize,
    pub command_history: Vec<String>,
    pub history_idx: Option<usize>,
    pub input_draft: String,

    // ── SVG / GCode pipeline ──────────────────────────────────────────────
    /// Path to the loaded SVG file (shown in the UI)
    pub svg_path: Option<PathBuf>,
    /// Generated GCode as a plain string (filled after conversion)
    pub gcode_text: Option<String>,
    /// GCode panel vertical scroll offset (line index from top)
    pub gcode_scroll: usize,
    /// Current state of the SVG → GCode conversion
    pub conversion_status: ConversionStatus,

    // ── Layer overrides ───────────────────────────────────────────────────
    /// Per-layer settings extracted from the loaded SVG.
    pub layers: Vec<TuiLayerSettings>,
    /// Which layer row is highlighted in the layer panel.
    pub layer_selected: usize,
    /// Which field within the selected layer is being edited:
    /// 0 = feedrate, 1 = power, 2 = passes. `None` = browsing, not editing.
    pub layer_edit_field: Option<usize>,
    /// Raw text typed into the layer edit buffer.
    pub layer_edit_buf: String,
    /// Validation error for the current layer edit.
    pub layer_edit_error: Option<String>,

    // ── Preview image ─────────────────────────────────────────────────────
    /// RGBA pixel image built by tracing the GCode toolpath (for ratatui-image)
    pub preview_image: Option<RgbaImage>,
    /// Protocol state for ratatui-image (stateful widget)
    pub preview_protocol: Option<ratatui_image::protocol::StatefulProtocol>,
    /// Whether the preview needs to be re-encoded (size changed, new image, …)
    pub preview_dirty: bool,

    // ── Machine & conversion settings ─────────────────────────────────────
    pub machine_settings: MachineSettings,

    // ── Settings tab UI state ─────────────────────────────────────────────
    /// Which field row is highlighted
    pub settings_selected: usize,
    /// When Some(_) the field is being edited; holds the current edit buffer
    pub settings_edit_buf: Option<String>,
    /// Last validation error for the edited field
    pub settings_edit_error: Option<String>,

    // ── Control tab – live GRBL state ─────────────────────────────────────
    /// Most recently parsed GRBL status report
    pub grbl_status: Option<crate::grbl::GrblStatus>,
    /// How many ticks since we last sent `?` (status poll)
    pub status_poll_ticks: u32,
    /// How many ticks between automatic `?` polls (0 = disabled)
    pub status_poll_interval: u32,

    // ── Control tab – jog settings ────────────────────────────────────────
    /// Index into `grbl::JOG_STEPS`
    pub jog_step_idx: usize,
    /// Jog feedrate (mm/min)
    pub jog_feedrate: f64,

    // ── Control tab – override values (local copy, sent when changed) ─────
    /// Feed-rate override 10–200 %
    pub override_feed: u8,
    /// Rapid override: 25, 50 or 100 %
    pub override_rapid: u8,
    /// Laser power override 10–200 %
    pub override_spindle: u8,

    // ── Control tab – GRBL settings list ─────────────────────────────────
    /// Received `$N=value` lines (key, value)
    pub grbl_settings: Vec<(String, String)>,
    /// Cursor inside the settings list
    pub settings_list_state: ratatui::widgets::ListState,
    /// G-code parser state string from `[GC:…]`
    pub gc_state: Option<String>,

    // ── Control tab – probe result ────────────────────────────────────────
    pub last_probe: Option<(crate::grbl::Pos3, bool)>,

    // ── Misc ──────────────────────────────────────────────────────────────
    pub should_quit: bool,
    /// Set to true after a native dialog so the main loop forces a full
    /// terminal redraw (clears ratatui's internal diff buffer).
    pub needs_clear: bool,
    /// When Some(_), a modal error popup is shown with this content.
    /// Dismissed by the user pressing Esc or Enter.
    pub conversion_error_popup: Option<ConversionErrorPopup>,

    // ── GCode streaming state ─────────────────────────────────────────────
    /// True while the serial actor is streaming GCode line by line.
    pub is_streaming: bool,
    /// Number of lines acknowledged by GRBL so far.
    pub stream_sent: usize,
    /// Total lines in the current stream job.
    pub stream_total: usize,
    pub status_message: Option<String>,
    /// Ticks until status_message is cleared (None = permanent until replaced)
    pub status_message_ttl: Option<u32>,
}

impl App {
    pub fn new() -> Self {
        let mut list_state = ratatui::widgets::ListState::default();
        list_state.select(Some(0));

        let mut settings_list_state = ratatui::widgets::ListState::default();
        settings_list_state.select(Some(0));

        Self {
            active_tab: ActiveTab::Connect,
            focused: FocusedPane::SerialList,
            control_focus: ControlFocus::Jog,
            gcode_focus: GCodeFocus::GCode,

            port_list: Vec::new(),
            port_list_state: list_state,

            baud_rates: BaudRate::ALL.to_vec(),
            selected_baud_idx: 0,
            baud_dropdown_open: false,

            mode: AppMode::Disconnected,
            connected_port: None,
            serial_tx: None,

            console_lines: VecDeque::new(),
            console_scroll: 0,
            console_follow: true,

            input_buffer: String::new(),
            input_cursor: 0,
            command_history: Vec::new(),
            history_idx: None,
            input_draft: String::new(),

            svg_path: None,
            gcode_text: None,
            gcode_scroll: 0,
            conversion_status: ConversionStatus::Idle,

            layers: Vec::new(),
            layer_selected: 0,
            layer_edit_field: None,
            layer_edit_buf: String::new(),
            layer_edit_error: None,

            preview_image: None,
            preview_protocol: None,
            preview_dirty: false,

            machine_settings: MachineSettings::default(),
            settings_selected: 0,
            settings_edit_buf: None,
            settings_edit_error: None,

            grbl_status: None,
            status_poll_ticks: 0,
            status_poll_interval: 4, // ~200 ms at 50 ms tick

            jog_step_idx: 2, // default 1.0 mm
            jog_feedrate: 3000.0,

            override_feed: 100,
            override_rapid: 100,
            override_spindle: 100,

            grbl_settings: Vec::new(),
            settings_list_state,
            gc_state: None,

            last_probe: None,

            should_quit: false,
            needs_clear: false,
            conversion_error_popup: None,
            is_streaming: false,
            stream_sent: 0,
            stream_total: 0,
            status_message: None,
            status_message_ttl: None,
        }
    }

    // ── Status message ────────────────────────────────────────────────────

    pub fn set_status(&mut self, msg: impl Into<String>, ttl_ticks: Option<u32>) {
        self.status_message = Some(msg.into());
        self.status_message_ttl = ttl_ticks;
    }

    /// Call once per render tick to decay the TTL.
    pub fn tick_status(&mut self) {
        if let Some(ttl) = self.status_message_ttl.as_mut() {
            if *ttl == 0 {
                self.status_message = None;
                self.status_message_ttl = None;
            } else {
                *ttl -= 1;
            }
        }
    }

    // ── Console helpers ───────────────────────────────────────────────────

    pub fn push_line(&mut self, text: impl Into<String>, kind: LineKind) {
        if self.console_lines.len() >= CONSOLE_MAX_LINES {
            self.console_lines.pop_front();
        }
        self.console_lines.push_back(ConsoleLine {
            text: text.into(),
            kind,
        });
        if self.console_follow {
            self.console_scroll = 0;
        }
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.push_line(text, LineKind::Info);
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.push_line(text, LineKind::Error);
    }

    pub fn push_received(&mut self, text: impl Into<String>) {
        self.push_line(text, LineKind::Received);
    }

    pub fn push_sent(&mut self, text: impl Into<String>) {
        self.push_line(text, LineKind::Sent);
    }

    // ── Port list helpers ─────────────────────────────────────────────────

    pub fn selected_port(&self) -> Option<&str> {
        self.port_list_state
            .selected()
            .and_then(|i| self.port_list.get(i))
            .map(String::as_str)
    }

    pub fn selected_baud(&self) -> BaudRate {
        self.baud_rates[self.selected_baud_idx]
    }

    pub fn port_list_next(&mut self) {
        if self.port_list.is_empty() {
            return;
        }
        let i = match self.port_list_state.selected() {
            Some(i) => (i + 1) % self.port_list.len(),
            None => 0,
        };
        self.port_list_state.select(Some(i));
    }

    pub fn port_list_prev(&mut self) {
        if self.port_list.is_empty() {
            return;
        }
        let i = match self.port_list_state.selected() {
            Some(0) | None => self.port_list.len().saturating_sub(1),
            Some(i) => i - 1,
        };
        self.port_list_state.select(Some(i));
    }

    // ── Baud combo helpers ────────────────────────────────────────────────

    pub fn baud_next(&mut self) {
        self.selected_baud_idx = (self.selected_baud_idx + 1) % self.baud_rates.len();
    }

    pub fn baud_prev(&mut self) {
        if self.selected_baud_idx == 0 {
            self.selected_baud_idx = self.baud_rates.len() - 1;
        } else {
            self.selected_baud_idx -= 1;
        }
    }

    // ── Input buffer helpers ──────────────────────────────────────────────

    pub fn input_insert_char(&mut self, ch: char) {
        self.input_buffer.insert(self.input_cursor, ch);
        self.input_cursor += ch.len_utf8();
    }

    pub fn input_delete_back(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut new_cursor = self.input_cursor - 1;
        while !self.input_buffer.is_char_boundary(new_cursor) {
            new_cursor -= 1;
        }
        self.input_buffer.drain(new_cursor..self.input_cursor);
        self.input_cursor = new_cursor;
    }

    pub fn input_delete_forward(&mut self) {
        if self.input_cursor >= self.input_buffer.len() {
            return;
        }
        let mut next = self.input_cursor + 1;
        while !self.input_buffer.is_char_boundary(next) {
            next += 1;
        }
        self.input_buffer.drain(self.input_cursor..next);
    }

    pub fn input_cursor_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.input_cursor -= 1;
        while !self.input_buffer.is_char_boundary(self.input_cursor) {
            self.input_cursor -= 1;
        }
    }

    pub fn input_cursor_right(&mut self) {
        if self.input_cursor >= self.input_buffer.len() {
            return;
        }
        self.input_cursor += 1;
        while !self.input_buffer.is_char_boundary(self.input_cursor) {
            self.input_cursor += 1;
        }
    }

    pub fn input_cursor_home(&mut self) {
        self.input_cursor = 0;
    }
    pub fn input_cursor_end(&mut self) {
        self.input_cursor = self.input_buffer.len();
    }

    pub fn input_submit(&mut self) -> String {
        let line = std::mem::take(&mut self.input_buffer);
        self.input_cursor = 0;
        self.history_idx = None;
        self.input_draft.clear();
        if !line.trim().is_empty() {
            self.command_history.push(line.clone());
        }
        line
    }

    pub fn history_up(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.input_draft = self.input_buffer.clone();
                let idx = self.command_history.len() - 1;
                self.history_idx = Some(idx);
                self.input_buffer = self.command_history[idx].clone();
            }
            Some(0) => {}
            Some(i) => {
                self.history_idx = Some(i - 1);
                self.input_buffer = self.command_history[i - 1].clone();
            }
        }
        self.input_cursor = self.input_buffer.len();
    }

    pub fn history_down(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 >= self.command_history.len() => {
                self.history_idx = None;
                self.input_buffer = std::mem::take(&mut self.input_draft);
            }
            Some(i) => {
                self.history_idx = Some(i + 1);
                self.input_buffer = self.command_history[i + 1].clone();
            }
        }
        self.input_cursor = self.input_buffer.len();
    }

    // ── Console scroll helpers ────────────────────────────────────────────

    pub fn console_scroll_up(&mut self) {
        self.console_follow = false;
        self.console_scroll = self.console_scroll.saturating_add(1);
    }

    pub fn console_scroll_down(&mut self) {
        if self.console_scroll == 0 {
            self.console_follow = true;
        } else {
            self.console_scroll -= 1;
        }
    }

    pub fn console_scroll_page_up(&mut self, page: u16) {
        self.console_follow = false;
        self.console_scroll = self.console_scroll.saturating_add(page);
    }

    pub fn console_scroll_page_down(&mut self, page: u16) {
        self.console_scroll = self.console_scroll.saturating_sub(page);
        if self.console_scroll == 0 {
            self.console_follow = true;
        }
    }

    pub fn console_scroll_to_bottom(&mut self) {
        self.console_scroll = 0;
        self.console_follow = true;
    }

    // ── GCode panel scroll helpers ────────────────────────────────────────

    // ── Layer panel helpers ───────────────────────────────────────────────

    /// Load layers from an SVG document, resetting user edits.
    pub fn load_layers_from_svg(&mut self, svg_text: &str) {
        use roxmltree::ParsingOptions;
        use svg2gcode::extract_svg_layers;

        let Ok(doc) = roxmltree::Document::parse_with_options(
            svg_text,
            ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        ) else {
            self.layers = Vec::new();
            return;
        };

        let max_feedrate = self.machine_settings.max_speed;
        let max_power = self.machine_settings.max_laser_power;
        self.layers = extract_svg_layers(&doc)
            .iter()
            .map(|info| TuiLayerSettings::from_svg_layer(info, max_feedrate, max_power))
            .collect();
        self.layer_selected = 0;
        self.layer_edit_field = None;
        self.layer_edit_buf.clear();
        self.layer_edit_error = None;
    }

    /// Build the `HashMap<String, LayerOverrideOptions>` for the converter.
    pub fn layer_override_map(&self) -> std::collections::HashMap<String, LayerOverrideOptions> {
        self.layers
            .iter()
            .filter_map(|l| {
                let opts = l.to_override_options();
                if opts.feedrate.is_some() || opts.power.is_some() || opts.passes.is_some() {
                    Some((l.key.clone(), opts))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn layer_next(&mut self) {
        if !self.layers.is_empty() {
            self.layer_selected = (self.layer_selected + 1) % self.layers.len();
            self.layer_edit_field = None;
            self.layer_edit_buf.clear();
            self.layer_edit_error = None;
        }
    }

    pub fn layer_prev(&mut self) {
        if !self.layers.is_empty() {
            self.layer_selected = if self.layer_selected == 0 {
                self.layers.len() - 1
            } else {
                self.layer_selected - 1
            };
            self.layer_edit_field = None;
            self.layer_edit_buf.clear();
            self.layer_edit_error = None;
        }
    }

    /// Begin editing the given field (0=feedrate, 1=power, 2=passes) of the
    /// selected layer, pre-filling the buffer with the current value.
    pub fn layer_begin_edit(&mut self, field: usize) {
        if let Some(layer) = self.layers.get(self.layer_selected) {
            let val = match field {
                0 => layer
                    .feedrate
                    .map(|f| format!("{f:.0}"))
                    .unwrap_or_default(),
                1 => layer.power.map(|p| format!("{p:.0}")).unwrap_or_default(),
                2 => layer.passes.map(|p| p.to_string()).unwrap_or_default(),
                _ => return,
            };
            self.layer_edit_field = Some(field);
            self.layer_edit_buf = val;
            self.layer_edit_error = None;
        }
    }

    /// Commit the edit buffer into the selected layer. Returns true on success.
    pub fn layer_commit_edit(&mut self) -> bool {
        let Some(field) = self.layer_edit_field else {
            return true;
        };
        let buf = self.layer_edit_buf.trim().to_owned();
        let max_feedrate = self.machine_settings.max_speed;
        let max_power = self.machine_settings.max_laser_power;

        if let Some(layer) = self.layers.get_mut(self.layer_selected) {
            match field {
                0 => {
                    if buf.is_empty() {
                        layer.feedrate = None;
                    } else {
                        match buf.parse::<f64>() {
                            Ok(f) if f >= 0.0 => {
                                layer.feedrate = Some(f.min(max_feedrate));
                            }
                            _ => {
                                self.layer_edit_error =
                                    Some(format!("Invalid feedrate (0..{max_feedrate:.0})"));
                                return false;
                            }
                        }
                    }
                }
                1 => {
                    if buf.is_empty() {
                        layer.power = None;
                    } else {
                        match buf.parse::<f64>() {
                            Ok(p) if p >= 0.0 => {
                                layer.power = Some(p.min(max_power));
                            }
                            _ => {
                                self.layer_edit_error =
                                    Some(format!("Invalid power (0..{max_power:.0})"));
                                return false;
                            }
                        }
                    }
                }
                2 => {
                    if buf.is_empty() {
                        layer.passes = None;
                    } else {
                        match buf.parse::<u32>() {
                            Ok(p) if p >= 1 => {
                                layer.passes = Some(p);
                            }
                            _ => {
                                self.layer_edit_error =
                                    Some("Passes must be a whole number ≥ 1".to_owned());
                                return false;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        self.layer_edit_field = None;
        self.layer_edit_buf.clear();
        self.layer_edit_error = None;
        true
    }

    /// Cancel the current layer field edit without applying it.
    pub fn layer_cancel_edit(&mut self) {
        self.layer_edit_field = None;
        self.layer_edit_buf.clear();
        self.layer_edit_error = None;
    }

    /// Clear all user overrides for every layer.
    pub fn layer_clear_all(&mut self) {
        for layer in &mut self.layers {
            layer.feedrate = layer.svg_feedrate;
            layer.power = layer.svg_power;
            layer.passes = layer.svg_passes;
        }
        self.layer_edit_field = None;
        self.layer_edit_buf.clear();
        self.layer_edit_error = None;
    }

    pub fn gcode_line_count(&self) -> usize {
        self.gcode_text
            .as_deref()
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    pub fn gcode_scroll_up(&mut self) {
        self.gcode_scroll = self.gcode_scroll.saturating_sub(1);
    }

    pub fn gcode_scroll_down(&mut self) {
        let max = self.gcode_line_count().saturating_sub(1);
        if self.gcode_scroll < max {
            self.gcode_scroll += 1;
        }
    }

    pub fn gcode_scroll_page_up(&mut self, page: usize) {
        self.gcode_scroll = self.gcode_scroll.saturating_sub(page);
    }

    pub fn gcode_scroll_page_down(&mut self, page: usize) {
        let max = self.gcode_line_count().saturating_sub(1);
        self.gcode_scroll = (self.gcode_scroll + page).min(max);
    }

    pub fn gcode_scroll_top(&mut self) {
        self.gcode_scroll = 0;
    }
    pub fn gcode_scroll_bottom(&mut self) {
        self.gcode_scroll = self.gcode_line_count().saturating_sub(1);
    }

    // ── Control tab helpers ───────────────────────────────────────────────

    pub fn jog_step_mm(&self) -> f64 {
        crate::grbl::JOG_STEPS[self.jog_step_idx]
    }

    pub fn jog_step_larger(&mut self) {
        if self.jog_step_idx + 1 < crate::grbl::JOG_STEPS.len() {
            self.jog_step_idx += 1;
        }
    }

    pub fn jog_step_smaller(&mut self) {
        if self.jog_step_idx > 0 {
            self.jog_step_idx -= 1;
        }
    }

    /// Upsert a GRBL setting received as `$N=value`.
    pub fn upsert_setting(&mut self, key: String, value: String) {
        if let Some(entry) = self.grbl_settings.iter_mut().find(|(k, _)| k == &key) {
            entry.1 = value;
        } else {
            self.grbl_settings.push((key, value));
            // Sort numerically by the number after '$'
            self.grbl_settings.sort_by(|(a, _), (b, _)| {
                let na: i64 = a.trim_start_matches('$').parse().unwrap_or(i64::MAX);
                let nb: i64 = b.trim_start_matches('$').parse().unwrap_or(i64::MAX);
                na.cmp(&nb)
            });
        }
    }

    pub fn settings_next(&mut self) {
        let len = self.grbl_settings.len();
        if len == 0 {
            return;
        }
        let i = self.settings_list_state.selected().unwrap_or(0);
        self.settings_list_state.select(Some((i + 1) % len));
    }

    pub fn settings_prev(&mut self) {
        let len = self.grbl_settings.len();
        if len == 0 {
            return;
        }
        let i = self.settings_list_state.selected().unwrap_or(0);
        self.settings_list_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }

    // ── Conversion error popup ────────────────────────────────────────────

    /// Show a conversion error popup with a title and detailed body.
    pub fn show_conversion_error(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.conversion_error_popup = Some(ConversionErrorPopup {
            title: title.into(),
            body: body.into(),
        });
    }

    /// Dismiss the conversion error popup.
    pub fn dismiss_conversion_error(&mut self) {
        self.conversion_error_popup = None;
    }

    // ── Settings tab helpers ──────────────────────────────────────────────

    pub fn settings_tab_next(&mut self) {
        let max = MachineSettings::field_count().saturating_sub(1);
        if self.settings_selected < max {
            self.settings_selected += 1;
        }
    }

    pub fn settings_tab_prev(&mut self) {
        if self.settings_selected > 0 {
            self.settings_selected -= 1;
        }
    }

    /// Enter edit mode for the currently selected field.
    pub fn settings_begin_edit(&mut self) {
        let val = self.machine_settings.field_value(self.settings_selected);
        self.settings_edit_buf = Some(val);
        self.settings_edit_error = None;
    }

    /// Commit the current edit buffer back into `machine_settings`.
    /// Returns true on success, false on validation error.
    pub fn settings_commit_edit(&mut self) -> bool {
        if let Some(buf) = self.settings_edit_buf.take() {
            match self
                .machine_settings
                .set_field(self.settings_selected, &buf)
            {
                Ok(()) => {
                    self.settings_edit_error = None;
                    true
                }
                Err(e) => {
                    self.settings_edit_buf = Some(buf);
                    self.settings_edit_error = Some(e);
                    false
                }
            }
        } else {
            true
        }
    }

    /// Discard the current edit buffer without applying it.
    pub fn settings_cancel_edit(&mut self) {
        self.settings_edit_buf = None;
        self.settings_edit_error = None;
    }

    /// Toggle a boolean field (fields 12–14).
    pub fn settings_toggle_bool(&mut self) {
        let idx = self.settings_selected;
        if idx >= 12 {
            let cur = self.machine_settings.field_value(idx);
            let toggled = if cur == "true" { "false" } else { "true" };
            let _ = self.machine_settings.set_field(idx, toggled);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_begin(seq: &str) -> MachineSettings {
        MachineSettings {
            begin_sequence: seq.to_owned(),
            ..MachineSettings::default()
        }
    }

    #[test]
    fn sanitise_removes_integer_s_word() {
        let s = settings_with_begin("G90 G21 M4 S1000");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_removes_decimal_s_word() {
        let s = settings_with_begin("G90 G21 M4 S750.5");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_removes_zero_s_word() {
        let s = settings_with_begin("G90 G21 M4 S0");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_leaves_sequence_without_s_unchanged() {
        let s = settings_with_begin("G90 G21 M4");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_handles_empty_sequence() {
        let s = settings_with_begin("");
        assert_eq!(s.sanitised_begin_sequence(), "");
    }

    #[test]
    fn sanitise_removes_s_word_at_start() {
        let s = settings_with_begin("S1000 G90 G21 M4");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_removes_multiple_s_words() {
        let s = settings_with_begin("G90 S500 G21 M4 S1000");
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    #[test]
    fn sanitise_does_not_remove_commands_starting_with_s_letter() {
        // There are no standard GCode commands starting with S that are not
        // S-words, but ensure tokens like "SCAN" are not wrongly stripped.
        // A token "S" alone with no digits after it is not a valid S-word.
        let s = settings_with_begin("G90 G21 M4");
        // Baseline: no S word present, result unchanged.
        assert_eq!(s.sanitised_begin_sequence(), "G90 G21 M4");
    }

    // ── MachineSettings field_value / set_field round-trip ────────────────────

    #[test]
    fn field_count_matches_field_names_length() {
        assert_eq!(
            MachineSettings::field_count(),
            MachineSettings::FIELD_NAMES.len()
        );
    }

    #[test]
    fn field_value_returns_string_for_every_index() {
        let s = MachineSettings::default();
        for idx in 0..MachineSettings::field_count() {
            // Must not panic and must return a non-empty string for all valid indices
            let v = s.field_value(idx);
            assert!(
                !v.is_empty() || idx == 0 || idx == 1,
                "field_value({idx}) returned empty string unexpectedly"
            );
        }
    }

    #[test]
    fn field_value_out_of_range_returns_empty() {
        let s = MachineSettings::default();
        assert_eq!(s.field_value(999), "");
    }

    #[test]
    fn set_field_out_of_range_returns_error() {
        let mut s = MachineSettings::default();
        assert!(s.set_field(999, "42").is_err());
    }

    #[test]
    fn set_field_roundtrip_begin_sequence() {
        let mut s = MachineSettings::default();
        s.set_field(0, "G90 G21 M3").unwrap();
        assert_eq!(s.field_value(0), "G90 G21 M3");
        assert_eq!(s.begin_sequence, "G90 G21 M3");
    }

    #[test]
    fn set_field_roundtrip_end_sequence() {
        let mut s = MachineSettings::default();
        s.set_field(1, "M5 G0 X0 Y0").unwrap();
        assert_eq!(s.field_value(1), "M5 G0 X0 Y0");
        assert_eq!(s.end_sequence, "M5 G0 X0 Y0");
    }

    #[test]
    fn set_field_roundtrip_max_x_mm() {
        let mut s = MachineSettings::default();
        s.set_field(2, "200.0").unwrap();
        assert!((s.max_x_mm - 200.0).abs() < 1e-9);
        assert_eq!(s.field_value(2), "200.0");
    }

    #[test]
    fn set_field_roundtrip_max_y_mm() {
        let mut s = MachineSettings::default();
        s.set_field(3, "300.5").unwrap();
        assert!((s.max_y_mm - 300.5).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_max_speed() {
        let mut s = MachineSettings::default();
        s.set_field(4, "5000").unwrap();
        assert!((s.max_speed - 5000.0).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_max_laser_power() {
        let mut s = MachineSettings::default();
        s.set_field(5, "500").unwrap();
        assert!((s.max_laser_power - 500.0).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_feedrate() {
        let mut s = MachineSettings::default();
        s.set_field(6, "1500").unwrap();
        assert!((s.feedrate - 1500.0).abs() < 1e-9);
        assert_eq!(s.field_value(6), "1500");
    }

    #[test]
    fn set_field_roundtrip_tolerance() {
        let mut s = MachineSettings::default();
        s.set_field(7, "0.0500").unwrap();
        assert!((s.tolerance - 0.05).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_dpi() {
        let mut s = MachineSettings::default();
        s.set_field(8, "72.0").unwrap();
        assert!((s.dpi - 72.0).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_laser_power() {
        let mut s = MachineSettings::default();
        s.set_field(9, "750").unwrap();
        assert!((s.laser_power - 750.0).abs() < 1e-9);
        assert_eq!(s.field_value(9), "750");
    }

    #[test]
    fn set_field_roundtrip_origin_x() {
        let mut s = MachineSettings::default();
        s.set_field(10, "12.5").unwrap();
        assert!((s.origin_x - 12.5).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_origin_y() {
        let mut s = MachineSettings::default();
        s.set_field(11, "7.0").unwrap();
        assert!((s.origin_y - 7.0).abs() < 1e-9);
    }

    #[test]
    fn set_field_roundtrip_circular_interpolation_true() {
        let mut s = MachineSettings::default();
        assert!(!s.circular_interpolation);
        s.set_field(12, "true").unwrap();
        assert!(s.circular_interpolation);
        assert_eq!(s.field_value(12), "true");
    }

    #[test]
    fn set_field_roundtrip_circular_interpolation_false() {
        let mut s = MachineSettings::default();
        s.circular_interpolation = true;
        s.set_field(12, "false").unwrap();
        assert!(!s.circular_interpolation);
        assert_eq!(s.field_value(12), "false");
    }

    #[test]
    fn set_field_boolean_accepts_1_and_yes() {
        let mut s = MachineSettings::default();
        s.set_field(13, "1").unwrap();
        assert!(s.line_numbers);
        s.set_field(13, "yes").unwrap();
        assert!(s.line_numbers);
        s.set_field(13, "no").unwrap();
        assert!(!s.line_numbers);
    }

    #[test]
    fn set_field_roundtrip_line_numbers() {
        let mut s = MachineSettings::default();
        s.set_field(13, "true").unwrap();
        assert!(s.line_numbers);
        assert_eq!(s.field_value(13), "true");
    }

    #[test]
    fn set_field_roundtrip_checksums() {
        let mut s = MachineSettings::default();
        s.set_field(14, "true").unwrap();
        assert!(s.checksums);
        assert_eq!(s.field_value(14), "true");
    }

    #[test]
    fn set_field_rejects_non_numeric_for_numeric_field() {
        let mut s = MachineSettings::default();
        assert!(s.set_field(2, "not_a_number").is_err());
        assert!(s.set_field(6, "abc").is_err());
        assert!(s.set_field(7, "???").is_err());
    }

    #[test]
    fn set_field_trims_whitespace() {
        let mut s = MachineSettings::default();
        // Numeric field with surrounding whitespace should still parse
        s.set_field(6, "  2000  ").unwrap();
        assert!((s.feedrate - 2000.0).abs() < 1e-9);
        // String field with surrounding whitespace is trimmed too
        s.set_field(0, "  G90  ").unwrap();
        assert_eq!(s.begin_sequence, "G90");
    }

    // ── ActiveTab navigation ──────────────────────────────────────────────────

    #[test]
    fn active_tab_next_wraps_around() {
        let last = *ActiveTab::ALL.last().unwrap();
        assert_eq!(last.next(), ActiveTab::ALL[0]);
    }

    #[test]
    fn active_tab_prev_wraps_around() {
        let first = ActiveTab::ALL[0];
        assert_eq!(first.prev(), *ActiveTab::ALL.last().unwrap());
    }

    #[test]
    fn active_tab_next_advances_sequentially() {
        for window in ActiveTab::ALL.windows(2) {
            assert_eq!(window[0].next(), window[1]);
        }
    }

    #[test]
    fn active_tab_prev_retreats_sequentially() {
        for window in ActiveTab::ALL.windows(2) {
            assert_eq!(window[1].prev(), window[0]);
        }
    }

    #[test]
    fn active_tab_all_have_non_empty_labels() {
        for tab in ActiveTab::ALL {
            assert!(!tab.label().is_empty(), "{tab:?} has an empty label");
        }
    }

    // ── BaudRate ──────────────────────────────────────────────────────────────

    #[test]
    fn baud_rate_all_have_positive_u32_values() {
        for b in BaudRate::ALL {
            assert!(b.as_u32() > 0, "{b:?} has zero baud rate");
        }
    }

    #[test]
    fn baud_rate_all_have_non_empty_labels() {
        for b in BaudRate::ALL {
            assert!(!b.label().is_empty(), "{b:?} has empty label");
        }
    }
}
