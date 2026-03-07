use std::collections::VecDeque;
use std::path::PathBuf;

use image::RgbaImage;

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
            begin_sequence: "G90 G21 M4 S1000".to_owned(),
            end_sequence: "M5 M2".to_owned(),
            max_x_mm: 300.0,
            max_y_mm: 300.0,
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
