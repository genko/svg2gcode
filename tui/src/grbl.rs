//! GRBL real-time status response parser.
//!
//! GRBL sends status reports in response to the `?` character (no newline needed).
//! The format is:
//!
//!   <State|MPos:x,y,z|WPos:x,y,z|FS:feed,speed|Pn:XYZ|Ov:feed,rapid,spindle|Bf:blocks,bytes|Ln:N|WCO:x,y,z>
//!
//! Not all fields are present in every report – the set depends on `$10` (status report mask).
//! This module parses whatever subset is present and stores it into [`GrblStatus`].
//!
//! It also recognises the common GRBL non-status lines:
//!   - `ok`
//!   - `error:N`
//!   - `ALARM:N`
//!   - `[MSG:…]`
//!   - `[GC:…]`  (G-code state)
//!   - `[HLP:…]`
//!   - `Grbl N.N build date`  (welcome banner)
//!   - `$N=…`                 (settings echo)

// ── Machine state ─────────────────────────────────────────────────────────────

/// GRBL machine states as reported in the `<State|…>` status string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineState {
    Idle,
    Run,
    Hold(u8), // Hold:0 = hold complete, Hold:1 = initiating hold
    Jog,
    Alarm,
    Door(u8), // Door:0-3
    Check,
    Home,
    Sleep,
    /// Any state string not recognised above
    Unknown(String),
}

impl MachineState {
    fn parse(s: &str) -> Self {
        // States can have a sub-code separated by ':'
        let (base, sub) = s.split_once(':').unwrap_or((s, "0"));
        let sub_u8: u8 = sub.parse().unwrap_or(0);
        match base {
            "Idle" => MachineState::Idle,
            "Run" => MachineState::Run,
            "Hold" => MachineState::Hold(sub_u8),
            "Jog" => MachineState::Jog,
            "Alarm" => MachineState::Alarm,
            "Door" => MachineState::Door(sub_u8),
            "Check" => MachineState::Check,
            "Home" => MachineState::Home,
            "Sleep" => MachineState::Sleep,
            other => MachineState::Unknown(other.to_owned()),
        }
    }

    /// Short human-readable label for display.
    pub fn label(&self) -> String {
        match self {
            MachineState::Idle => "Idle".into(),
            MachineState::Run => "Run".into(),
            MachineState::Hold(n) => format!("Hold:{n}"),
            MachineState::Jog => "Jog".into(),
            MachineState::Alarm => "ALARM".into(),
            MachineState::Door(n) => format!("Door:{n}"),
            MachineState::Check => "Check".into(),
            MachineState::Home => "Home".into(),
            MachineState::Sleep => "Sleep".into(),
            MachineState::Unknown(s) => s.clone(),
        }
    }

    /// True when it is safe to send motion commands.
    pub fn is_ready(&self) -> bool {
        matches!(
            self,
            MachineState::Idle | MachineState::Run | MachineState::Jog
        )
    }

    /// True when the machine is in ALARM state.
    pub fn is_alarm(&self) -> bool {
        matches!(self, MachineState::Alarm)
    }
}

// ── 3-axis position ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Pos3 {
    pub const ZERO: Self = Pos3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(3, ',').collect();
        if parts.len() < 2 {
            return None;
        }
        let x = parts[0].trim().parse().ok()?;
        let y = parts[1].trim().parse().ok()?;
        let z = parts
            .get(2)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0.0);
        Some(Pos3 { x, y, z })
    }
}

impl std::fmt::Display for Pos3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "X{:>8.3}  Y{:>8.3}  Z{:>8.3}", self.x, self.y, self.z)
    }
}

// ── Active input pins ─────────────────────────────────────────────────────────

/// Bit-flags for active input pins (from `Pn:` field).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivePins {
    pub x_limit: bool,
    pub y_limit: bool,
    pub z_limit: bool,
    pub probe: bool,
    pub door: bool,
    pub hard_reset: bool,
    pub feed_hold: bool,
    pub cycle_start: bool,
}

impl ActivePins {
    fn parse(s: &str) -> Self {
        ActivePins {
            x_limit: s.contains('X'),
            y_limit: s.contains('Y'),
            z_limit: s.contains('Z'),
            probe: s.contains('P'),
            door: s.contains('D'),
            hard_reset: s.contains('R'),
            feed_hold: s.contains('H'),
            cycle_start: s.contains('S'),
        }
    }
}

// ── Override percentages ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overrides {
    pub feed: u8,    // 10 – 200 %
    pub rapid: u8,   // 25, 50, 100 %
    pub spindle: u8, // 10 – 200 %
}

impl Default for Overrides {
    fn default() -> Self {
        Overrides {
            feed: 100,
            rapid: 100,
            spindle: 100,
        }
    }
}

impl Overrides {
    fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(3, ',').collect();
        if parts.len() < 3 {
            return None;
        }
        Some(Overrides {
            feed: parts[0].trim().parse().ok()?,
            rapid: parts[1].trim().parse().ok()?,
            spindle: parts[2].trim().parse().ok()?,
        })
    }
}

// ── Buffer state ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferState {
    pub available_blocks: u16,
    pub available_bytes: u16,
}

impl BufferState {
    fn parse(s: &str) -> Option<Self> {
        let (a, b) = s.split_once(',')?;
        Some(BufferState {
            available_blocks: a.trim().parse().ok()?,
            available_bytes: b.trim().parse().ok()?,
        })
    }
}

// ── Full status report ────────────────────────────────────────────────────────

/// Parsed GRBL `<…>` status report.
#[derive(Debug, Clone, PartialEq)]
pub struct GrblStatus {
    /// Machine state (Idle, Run, Alarm, …)
    pub state: MachineState,
    /// Machine position (absolute, in machine coordinates)
    pub mpos: Option<Pos3>,
    /// Work position (mpos – wco)
    pub wpos: Option<Pos3>,
    /// Work coordinate offset
    pub wco: Option<Pos3>,
    /// Current feed rate (mm/min)
    pub feed: Option<f64>,
    /// Current laser power (S value)
    pub speed: Option<f64>,
    /// Active input pins
    pub pins: Option<ActivePins>,
    /// Override percentages
    pub overrides: Option<Overrides>,
    /// Planner / serial buffer availability
    pub buffer: Option<BufferState>,
    /// Last executed line number (`Ln:` field)
    pub line_number: Option<u32>,
}

impl GrblStatus {
    /// Derive WPos from MPos + WCO when WPos is not reported directly.
    pub fn work_pos(&self) -> Option<Pos3> {
        if let Some(wp) = self.wpos {
            return Some(wp);
        }
        match (self.mpos, self.wco) {
            (Some(m), Some(wco)) => Some(Pos3 {
                x: m.x - wco.x,
                y: m.y - wco.y,
                z: m.z - wco.z,
            }),
            (Some(m), None) => Some(m), // no offset known – show MPos as fallback
            _ => None,
        }
    }
}

// ── GRBL line classification ──────────────────────────────────────────────────

/// Every distinct kind of line GRBL can send.
#[derive(Debug, Clone, PartialEq)]
pub enum GrblLine {
    /// `<State|…>` real-time status report
    Status(GrblStatus),
    /// `ok`
    Ok,
    /// `error:N` – error code
    Error(u16),
    /// `ALARM:N` – alarm code
    Alarm(u16),
    /// `[MSG:…]` – informational message
    Msg(String),
    /// `[GC:…]` – G-code parser state
    GcState(String),
    /// `[HLP:…]` – help text
    Help(String),
    /// `[PRB:x,y,z:result]` – probing result
    Probe { pos: Pos3, success: bool },
    /// `Grbl M.m` or similar welcome / version banner
    Banner(String),
    /// `$N=value` – startup-block or setting echo
    Setting { key: String, value: String },
    /// Anything else (echoed GCode, debug lines, …)
    Other(String),
}

impl GrblLine {
    /// Classify and parse a raw line received from GRBL.
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim();

        // ── Status report ─────────────────────────────────────────────────
        if s.starts_with('<') && s.ends_with('>') {
            let inner = &s[1..s.len() - 1];
            return GrblLine::Status(parse_status(inner));
        }

        // ── ok ────────────────────────────────────────────────────────────
        if s.eq_ignore_ascii_case("ok") {
            return GrblLine::Ok;
        }

        // ── error:N ───────────────────────────────────────────────────────
        if let Some(rest) = s.strip_prefix("error:") {
            let code = rest.trim().parse().unwrap_or(0);
            return GrblLine::Error(code);
        }

        // ── ALARM:N ───────────────────────────────────────────────────────
        if let Some(rest) = s.strip_prefix("ALARM:") {
            let code = rest.trim().parse().unwrap_or(0);
            return GrblLine::Alarm(code);
        }

        // ── Bracketed messages [TAG:…] ────────────────────────────────────
        if s.starts_with('[') && s.ends_with(']') {
            let inner = &s[1..s.len() - 1];
            if let Some(rest) = inner.strip_prefix("MSG:") {
                return GrblLine::Msg(rest.to_owned());
            }
            if let Some(rest) = inner.strip_prefix("GC:") {
                return GrblLine::GcState(rest.to_owned());
            }
            if let Some(rest) = inner.strip_prefix("HLP:") {
                return GrblLine::Help(rest.to_owned());
            }
            if let Some(rest) = inner.strip_prefix("PRB:") {
                // Format: x,y,z:0|1
                if let Some((coords, result)) = rest.rsplit_once(':') {
                    if let Some(pos) = Pos3::parse(coords) {
                        let success = result.trim() == "1";
                        return GrblLine::Probe { pos, success };
                    }
                }
            }
            // Any other bracketed line
            return GrblLine::Msg(inner.to_owned());
        }

        // ── Grbl welcome banner ───────────────────────────────────────────
        if s.starts_with("Grbl") {
            return GrblLine::Banner(s.to_owned());
        }

        // ── $N=value (setting echo) ───────────────────────────────────────
        if s.starts_with('$') {
            if let Some(eq_pos) = s.find('=') {
                let key = s[..eq_pos].to_owned();
                let value = s[eq_pos + 1..].to_owned();
                return GrblLine::Setting { key, value };
            }
        }

        GrblLine::Other(s.to_owned())
    }

    /// One-line description suitable for the console.
    pub fn summary(&self) -> String {
        match self {
            GrblLine::Status(st) => format!(
                "[status] {} | WPos: {}",
                st.state.label(),
                st.work_pos()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
            GrblLine::Ok => "ok".into(),
            GrblLine::Error(n) => format!("error:{n} – {}", error_description(*n)),
            GrblLine::Alarm(n) => format!("ALARM:{n} – {}", alarm_description(*n)),
            GrblLine::Msg(m) => format!("[MSG] {m}"),
            GrblLine::GcState(s) => format!("[GC] {s}"),
            GrblLine::Help(h) => format!("[HLP] {h}"),
            GrblLine::Probe { pos, success } => {
                format!("[PRB] {} {}", pos, if *success { "OK" } else { "FAIL" })
            }
            GrblLine::Banner(b) => b.clone(),
            GrblLine::Setting { key, value } => format!("{key}={value}"),
            GrblLine::Other(o) => o.clone(),
        }
    }
}

// ── Status field parser ───────────────────────────────────────────────────────

fn parse_status(inner: &str) -> GrblStatus {
    let mut fields = inner.split('|');

    // First field is always the state
    let state_str = fields.next().unwrap_or("Unknown");
    let state = MachineState::parse(state_str);

    let mut mpos: Option<Pos3> = None;
    let mut wpos: Option<Pos3> = None;
    let mut wco: Option<Pos3> = None;
    let mut feed: Option<f64> = None;
    let mut speed: Option<f64> = None;
    let mut pins: Option<ActivePins> = None;
    let mut overrides: Option<Overrides> = None;
    let mut buffer: Option<BufferState> = None;
    let mut line_number: Option<u32> = None;

    for field in fields {
        if let Some(val) = field.strip_prefix("MPos:") {
            mpos = Pos3::parse(val);
        } else if let Some(val) = field.strip_prefix("WPos:") {
            wpos = Pos3::parse(val);
        } else if let Some(val) = field.strip_prefix("WCO:") {
            wco = Pos3::parse(val);
        } else if let Some(val) = field.strip_prefix("FS:") {
            // FS:feed,speed
            let mut parts = val.splitn(2, ',');
            feed = parts.next().and_then(|v| v.trim().parse().ok());
            speed = parts.next().and_then(|v| v.trim().parse().ok());
        } else if let Some(val) = field.strip_prefix("F:") {
            // Older firmware only sends F: without spindle speed
            feed = val.trim().parse().ok();
        } else if let Some(val) = field.strip_prefix("Pn:") {
            pins = Some(ActivePins::parse(val));
        } else if let Some(val) = field.strip_prefix("Ov:") {
            overrides = Overrides::parse(val);
        } else if let Some(val) = field.strip_prefix("Bf:") {
            buffer = BufferState::parse(val);
        } else if let Some(val) = field.strip_prefix("Ln:") {
            line_number = val.trim().parse().ok();
        }
        // Unknown fields are silently ignored for forward-compatibility
    }

    GrblStatus {
        state,
        mpos,
        wpos,
        wco,
        feed,
        speed,
        pins,
        overrides,
        buffer,
        line_number,
    }
}

// ── Error/Alarm descriptions ──────────────────────────────────────────────────

pub fn error_description(code: u16) -> &'static str {
    match code {
        1 => "G-code letter not found",
        2 => "Bad number format",
        3 => "Invalid statement",
        4 => "Negative value",
        5 => "Homing not enabled",
        6 => "Step pulse too short",
        7 => "EEPROM read fail",
        8 => "Not idle",
        9 => "G-code lock",
        10 => "Homing not enabled",
        11 => "Line overflow",
        12 => "Step rate too high",
        13 => "Check door",
        14 => "Line length exceeded",
        15 => "Travel exceeded",
        16 => "Invalid jog command",
        17 => "Laser mode requires PWM output",
        20 => "Unsupported command",
        21 => "Modal group violation",
        22 => "Undefined feed rate",
        23 => "Invalid G-code ID:23",
        24 => "Invalid G-code ID:24",
        25 => "Invalid G-code ID:25",
        26 => "Invalid G-code ID:26",
        27 => "Invalid G-code ID:27",
        28 => "Invalid G-code ID:28",
        29 => "Invalid G-code ID:29",
        30 => "Invalid G-code ID:30",
        31 => "Invalid G-code ID:31",
        32 => "Invalid G-code ID:32",
        33 => "Invalid G-code ID:33",
        34 => "Invalid G-code ID:34",
        35 => "Invalid G-code ID:35",
        36 => "Invalid G-code ID:36",
        37 => "Invalid G-code ID:37",
        38 => "Invalid G-code ID:38",
        _ => "Unknown error",
    }
}

pub fn alarm_description(code: u16) -> &'static str {
    match code {
        1 => "Hard limit triggered",
        2 => "Soft limit alarm",
        3 => "Abort during cycle",
        4 => "Probe fail: initial state not open",
        5 => "Probe fail: contact not detected",
        6 => "Homing fail: reset during cycle",
        7 => "Homing fail: door opened during cycle",
        8 => "Homing fail: pull-off failed",
        9 => "Homing fail: target not found",
        _ => "Unknown alarm",
    }
}

// ── Jog step sizes ────────────────────────────────────────────────────────────

/// Standard jog step sizes (mm).
pub const JOG_STEPS: &[f64] = &[0.01, 0.1, 1.0, 10.0, 100.0];

/// Direction multipliers for named axis directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JogDir {
    XPlus,
    XMinus,
    YPlus,
    YMinus,
    ZPlus,
    ZMinus,
}

impl JogDir {
    /// Build the GRBL jog command string for this direction.
    ///
    /// Uses `$J=G21G91` (metric, relative) mode so the step is always relative
    /// to current position.
    pub fn to_command(self, step_mm: f64, feedrate: f64) -> String {
        let (axis, dist) = match self {
            JogDir::XPlus => ("X", step_mm),
            JogDir::XMinus => ("X", -step_mm),
            JogDir::YPlus => ("Y", step_mm),
            JogDir::YMinus => ("Y", -step_mm),
            JogDir::ZPlus => ("Z", step_mm),
            JogDir::ZMinus => ("Z", -step_mm),
        };
        format!("$J=G21G91{axis}{dist:.4}F{feedrate:.0}")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_idle_mpos() {
        let line = "<Idle|MPos:0.000,0.000,0.000|FS:0,0|WCO:0.000,0.000,0.000>";
        let parsed = GrblLine::parse(line);
        match parsed {
            GrblLine::Status(st) => {
                assert_eq!(st.state, MachineState::Idle);
                let mp = st.mpos.unwrap();
                assert!((mp.x - 0.0).abs() < 1e-9);
                assert!((mp.y - 0.0).abs() < 1e-9);
                let wp = st.work_pos().unwrap();
                assert!((wp.x - 0.0).abs() < 1e-9);
                assert_eq!(st.feed.unwrap() as u32, 0);
                assert_eq!(st.speed.unwrap() as u32, 0);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_run_with_wpos_and_feed() {
        let line = "<Run|WPos:10.250,-3.500,0.000|FS:3000,500>";
        let parsed = GrblLine::parse(line);
        match parsed {
            GrblLine::Status(st) => {
                assert_eq!(st.state, MachineState::Run);
                let wp = st.wpos.unwrap();
                assert!((wp.x - 10.25).abs() < 1e-6);
                assert!((wp.y - -3.5).abs() < 1e-6);
                assert!((st.feed.unwrap() - 3000.0).abs() < 1e-6);
                assert!((st.speed.unwrap() - 500.0).abs() < 1e-6);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_alarm_state() {
        let line = "<Alarm|MPos:0.000,0.000,0.000|FS:0,0>";
        let parsed = GrblLine::parse(line);
        match parsed {
            GrblLine::Status(st) => {
                assert!(st.state.is_alarm());
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn parse_hold_with_subcode() {
        let line = "<Hold:1|MPos:5.000,5.000,0.000|FS:100,0>";
        let parsed = GrblLine::parse(line);
        match parsed {
            GrblLine::Status(st) => {
                assert_eq!(st.state, MachineState::Hold(1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_ok() {
        assert_eq!(GrblLine::parse("ok"), GrblLine::Ok);
    }

    #[test]
    fn parse_error() {
        let parsed = GrblLine::parse("error:22");
        assert_eq!(parsed, GrblLine::Error(22));
    }

    #[test]
    fn parse_alarm_line() {
        assert_eq!(GrblLine::parse("ALARM:1"), GrblLine::Alarm(1));
    }

    #[test]
    fn parse_msg() {
        let parsed = GrblLine::parse("[MSG:Reset to continue]");
        assert_eq!(parsed, GrblLine::Msg("Reset to continue".to_owned()));
    }

    #[test]
    fn parse_gc_state() {
        let parsed = GrblLine::parse("[GC:G0 G54 G17 G21 G90 G94 M5 M9 T0 F0 S0]");
        match parsed {
            GrblLine::GcState(s) => {
                assert!(s.contains("G0"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_probe() {
        let parsed = GrblLine::parse("[PRB:0.000,0.000,-1.500:1]");
        match parsed {
            GrblLine::Probe { pos, success } => {
                assert!((pos.z - -1.5).abs() < 1e-6);
                assert!(success);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_pins() {
        let line = "<Idle|MPos:0.000,0.000,0.000|Pn:XYZ|FS:0,0>";
        match GrblLine::parse(line) {
            GrblLine::Status(st) => {
                let pins = st.pins.unwrap();
                assert!(pins.x_limit);
                assert!(pins.y_limit);
                assert!(pins.z_limit);
                assert!(!pins.probe);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_overrides() {
        let line = "<Idle|MPos:0.000,0.000,0.000|FS:0,0|Ov:110,100,90>";
        match GrblLine::parse(line) {
            GrblLine::Status(st) => {
                let ov = st.overrides.unwrap();
                assert_eq!(ov.feed, 110);
                assert_eq!(ov.rapid, 100);
                assert_eq!(ov.spindle, 90);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_buffer() {
        let line = "<Idle|MPos:0.000,0.000,0.000|FS:0,0|Bf:15,127>";
        match GrblLine::parse(line) {
            GrblLine::Status(st) => {
                let bf = st.buffer.unwrap();
                assert_eq!(bf.available_blocks, 15);
                assert_eq!(bf.available_bytes, 127);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_banner() {
        let parsed = GrblLine::parse("Grbl 1.1f ['$' for help]");
        match parsed {
            GrblLine::Banner(b) => assert!(b.contains("Grbl")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_setting_echo() {
        let parsed = GrblLine::parse("$32=1");
        match parsed {
            GrblLine::Setting { key, value } => {
                assert_eq!(key, "$32");
                assert_eq!(value, "1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn jog_command_positive_x() {
        let cmd = JogDir::XPlus.to_command(10.0, 3000.0);
        assert_eq!(cmd, "$J=G21G91X10.0000F3000");
    }

    #[test]
    fn jog_command_negative_y() {
        let cmd = JogDir::YMinus.to_command(0.1, 500.0);
        assert_eq!(cmd, "$J=G21G91Y-0.1000F500");
    }

    #[test]
    fn work_pos_derived_from_mpos_wco() {
        let st = GrblStatus {
            state: MachineState::Idle,
            mpos: Some(Pos3 {
                x: 10.0,
                y: 20.0,
                z: 0.0,
            }),
            wpos: None,
            wco: Some(Pos3 {
                x: 5.0,
                y: 10.0,
                z: 0.0,
            }),
            feed: None,
            speed: None,
            pins: None,
            overrides: None,
            buffer: None,
            line_number: None,
        };
        let wp = st.work_pos().unwrap();
        assert!((wp.x - 5.0).abs() < 1e-9);
        assert!((wp.y - 10.0).abs() < 1e-9);
    }

    #[test]
    fn error_descriptions_are_non_empty() {
        for code in [1u16, 5, 9, 16, 22] {
            let desc = error_description(code);
            assert!(!desc.is_empty(), "empty description for error {code}");
        }
    }

    #[test]
    fn alarm_descriptions_are_non_empty() {
        for code in 1u16..=9 {
            let desc = alarm_description(code);
            assert!(!desc.is_empty(), "empty description for alarm {code}");
        }
    }
}
