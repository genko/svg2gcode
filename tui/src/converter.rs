//! SVG → GCode conversion and GCode → preview-image rendering.
//!
//! Public entry points:
//!
//! - [`svg_to_gcode`]  – calls the `svg2gcode` library, validates the result against machine
//!   limits, and returns the GCode as a `String` or a detailed [`ConversionError`].
//! - [`gcode_to_image`] – parses the GCode with `gcode-nom`, traces all G0/G1/G2/G3 moves and
//!   renders them into an [`image::RgbaImage`] that ratatui-image can display.

use std::f64::consts::PI;
use std::path::Path;

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};

use gcode_nom::arc::Form as ArcForm;
use gcode_nom::command::Command;
use gcode_nom::params::head::PosVal;
use gcode_nom::{PositionMode, compute_arc};

pub use laser_bb::laser_bounding_box;

use roxmltree::ParsingOptions;
use svg2gcode::{
    ConversionConfig, ConversionOptions, Machine, SupportedFunctionality, svg2program,
};

use crate::app::MachineSettings;

// ── Validation error ──────────────────────────────────────────────────────────

/// A structured error returned when generated GCode violates machine limits.
///
/// Distinct from `anyhow::Error` so the UI can render it as a dedicated popup
/// with all the numeric detail rather than a plain error string.
#[derive(Debug, Clone)]
pub struct ConversionError {
    /// Short one-line summary shown in the popup title.
    pub title: String,
    /// Full multi-line body with actual vs. allowed values.
    pub body: String,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.title, self.body)
    }
}

impl std::error::Error for ConversionError {}

/// Validate machine settings before we even try to convert.
fn validate_settings(settings: &MachineSettings) -> Result<(), ConversionError> {
    let mut problems: Vec<String> = Vec::new();

    if settings.feedrate <= 0.0 {
        problems.push(format!(
            "• Feedrate must be > 0 mm/min (currently {:.0})",
            settings.feedrate
        ));
    }
    if settings.feedrate > settings.max_speed {
        problems.push(format!(
            "• Feedrate {:.0} mm/min exceeds max speed {:.0} mm/min",
            settings.feedrate, settings.max_speed
        ));
    }
    if settings.tolerance <= 0.0 {
        problems.push(format!(
            "• Tolerance must be > 0 mm (currently {:.4})",
            settings.tolerance
        ));
    }
    if settings.dpi <= 0.0 {
        problems.push(format!("• DPI must be > 0 (currently {:.1})", settings.dpi));
    }
    if settings.laser_power < 0.0 || settings.laser_power > settings.max_laser_power {
        problems.push(format!(
            "• Laser power {:.0} S is outside allowed range 0 – {:.0} S",
            settings.laser_power, settings.max_laser_power
        ));
    }
    if settings.max_x_mm <= 0.0 || settings.max_y_mm <= 0.0 {
        problems.push(format!(
            "• Work area must be positive (currently {:.1} × {:.1} mm)",
            settings.max_x_mm, settings.max_y_mm
        ));
    }
    if settings.origin_x < 0.0 || settings.origin_y < 0.0 {
        problems.push(format!(
            "• Origin ({:.1}, {:.1}) must be ≥ 0",
            settings.origin_x, settings.origin_y
        ));
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ConversionError {
            title: "Invalid machine settings".into(),
            body: problems.join("\n"),
        })
    }
}

/// Check that the GCode bounding box fits inside the configured work area.
///
/// `bb_min` / `bb_max` are in mm as returned by [`collect_segments`].
fn validate_extents(
    settings: &MachineSettings,
    bb_min: (f64, f64),
    bb_max: (f64, f64),
) -> Result<(), ConversionError> {
    // The job bounding box already incorporates the origin offset because the
    // converter shifts everything via ConversionConfig::origin.
    let job_x = bb_max.0 - bb_min.0;
    let job_y = bb_max.1 - bb_min.1;

    // Absolute extents (where the job actually sits in machine coordinates)
    let abs_x_min = bb_min.0;
    let abs_y_min = bb_min.1;
    let abs_x_max = bb_max.0;
    let abs_y_max = bb_max.1;

    let mut problems: Vec<String> = Vec::new();

    if abs_x_min < 0.0 {
        problems.push(format!(
            "• Job starts at X {:.3} mm (must be ≥ 0)",
            abs_x_min
        ));
    }
    if abs_y_min < 0.0 {
        problems.push(format!(
            "• Job starts at Y {:.3} mm (must be ≥ 0)",
            abs_y_min
        ));
    }
    if abs_x_max > settings.max_x_mm {
        problems.push(format!(
            "• Job reaches X {:.3} mm but machine limit is {:.1} mm (overrun by {:.3} mm)",
            abs_x_max,
            settings.max_x_mm,
            abs_x_max - settings.max_x_mm,
        ));
    }
    if abs_y_max > settings.max_y_mm {
        problems.push(format!(
            "• Job reaches Y {:.3} mm but machine limit is {:.1} mm (overrun by {:.3} mm)",
            abs_y_max,
            settings.max_y_mm,
            abs_y_max - settings.max_y_mm,
        ));
    }

    if problems.is_empty() {
        Ok(())
    } else {
        let body = format!(
            "Job size: {:.3} × {:.3} mm  (X {:.3}…{:.3}, Y {:.3}…{:.3})\n\
             Work area: {:.1} × {:.1} mm\n\
             \n\
             {}",
            job_x,
            job_y,
            abs_x_min,
            abs_x_max,
            abs_y_min,
            abs_y_max,
            settings.max_x_mm,
            settings.max_y_mm,
            problems.join("\n"),
        );
        Err(ConversionError {
            title: "Job exceeds work area".into(),
            body,
        })
    }
}

// ── SVG → GCode ───────────────────────────────────────────────────────────────

/// Convert an SVG file to a GCode string using the full [`MachineSettings`].
///
/// All conversion parameters (feedrate, tolerance, dpi, origin, begin/end
/// sequences, circular interpolation, line numbers, checksums) are taken
/// directly from `settings`.
///
/// Returns a [`ConversionError`] (wrapped in `anyhow`) when:
/// - Settings are invalid (bad feedrate, power out of range, …)
/// - The generated toolpath exceeds the configured work area
pub fn svg_to_gcode(svg_path: &Path, settings: &MachineSettings) -> Result<String> {
    let svg_text = std::fs::read_to_string(svg_path)
        .with_context(|| format!("Cannot read SVG file: {}", svg_path.display()))?;

    let document = roxmltree::Document::parse_with_options(
        &svg_text,
        ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .context("Failed to parse SVG document")?;

    use g_code::emit::FormatOptions;
    use g_code::emit::format_gcode_fmt;
    use g_code::parse::snippet_parser;

    // Strip any S word from the user-supplied begin sequence and append the
    // laser power from the dedicated setting as a separate S command.
    // This ensures the S value is always driven by "Laser power (S)" and
    // never silently overridden by a hardcoded value in the sequence string.
    let sanitised_begin = format!(
        "{} S{:.0}",
        settings.sanitised_begin_sequence(),
        settings.laser_power,
    );
    let begin_snippet = snippet_parser(&sanitised_begin)
        .map_err(|e| anyhow::anyhow!("Bad begin sequence: {:?}", e))?;
    let end_snippet = snippet_parser(&settings.end_sequence)
        .map_err(|e| anyhow::anyhow!("Bad end sequence: {:?}", e))?;

    let machine = Machine::new(
        SupportedFunctionality {
            circular_interpolation: settings.circular_interpolation,
        },
        None, // tool_on  – GRBL laser mode handles this via G1 motion
        None, // tool_off
        Some(begin_snippet),
        Some(end_snippet),
    );

    let config = ConversionConfig {
        feedrate: settings.feedrate,
        tolerance: settings.tolerance,
        dpi: settings.dpi,
        origin: [Some(settings.origin_x), Some(settings.origin_y)],
        extra_attribute_name: None,
    };

    let options = ConversionOptions {
        dimensions: [None, None],
    };

    // Validate settings before doing any heavy work.
    validate_settings(settings).map_err(|e| anyhow::anyhow!(e))?;

    let program = svg2program(&document, &config, options, machine);

    let mut gcode_string = String::new();
    format_gcode_fmt(
        &program,
        FormatOptions {
            line_numbers: settings.line_numbers,
            checksums: settings.checksums,
            newline_before_comment: false,
            ..Default::default()
        },
        &mut gcode_string,
    )
    .context("Failed to format GCode")?;

    // Parse the generated GCode to extract the actual toolpath bounding box
    // and verify it fits inside the configured work area.
    let (_segments, bb_min, bb_max) = collect_segments(&gcode_string);
    validate_extents(settings, bb_min, bb_max).map_err(|e| anyhow::anyhow!(e))?;

    Ok(gcode_string)
}

// ── Laser bounding box ────────────────────────────────────────────────────────

mod laser_bb {
    use super::*;

    /// Walk the GCode and return the axis-aligned bounding box of all moves
    /// where the laser is **on** (S word > 0).
    ///
    /// Returns `None` when no laser-on move is found (e.g. GCode is empty or
    /// consists only of rapid/travel moves).
    ///
    /// Rules applied:
    /// - G0 moves are always rapid (laser off) and never contribute.
    /// - G1/G2/G3 moves contribute only when the *effective* S value is > 0.
    /// - The S word is sticky: once set it remains in effect for subsequent
    ///   moves until a new S word appears.  This matches GRBL behaviour.
    /// - S0 explicitly turns the laser off; those moves are excluded.
    /// - The begin/end sequences commonly set S via `M4 S<n>` — we parse
    ///   plain `S<n>` words that appear on any line regardless of command.
    pub fn laser_bounding_box(gcode: &str) -> Option<((f64, f64), (f64, f64))> {
        let mut pos = (0.0_f64, 0.0_f64);
        let mut mode = PositionMode::Absolute;
        let mut laser_s: f64 = 0.0; // current S value; 0 = laser off

        let mut min = (f64::MAX, f64::MAX);
        let mut max = (f64::MIN, f64::MIN);
        let mut found = false;

        let mut update = |pt: (f64, f64)| {
            if pt.0 < min.0 {
                min.0 = pt.0;
            }
            if pt.1 < min.1 {
                min.1 = pt.1;
            }
            if pt.0 > max.0 {
                max.0 = pt.0;
            }
            if pt.1 > max.1 {
                max.1 = pt.1;
            }
            found = true;
        };

        for line in gcode.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Ok((_, cmd)) = Command::parse_line(line) else {
                // Still try to pick up a bare S word on lines we can't fully parse
                // (e.g. `M4 S1000` which gcode-nom may not know).
                laser_s = extract_s_word(line).unwrap_or(laser_s);
                continue;
            };

            match cmd {
                // ── Rapid: laser always off ───────────────────────────────
                Command::G0(ref params) => {
                    // Update S if present (uncommon on G0 but valid)
                    if let Some(s) = extract_s_from_params(params) {
                        laser_s = s;
                    }
                    let (nx, ny) = resolve_xy_bb(params, pos, &mode);
                    pos = (nx, ny);
                    // G0 never contributes to the laser bounding box.
                }

                // ── Linear cut ────────────────────────────────────────────
                Command::G1(ref params) => {
                    if let Some(s) = extract_s_from_params(params) {
                        laser_s = s;
                    }
                    let (nx, ny) = resolve_xy_bb(params, pos, &mode);
                    if laser_s > 0.0 {
                        update(pos);
                        update((nx, ny));
                    }
                    pos = (nx, ny);
                }

                // ── Clockwise arc ─────────────────────────────────────────
                Command::G2(ref form) => {
                    if let Some(seg) = make_arc_segment(pos, form, true) {
                        let end = arc_endpoint(&seg);
                        let mid = arc_midpoint(&seg);
                        if laser_s > 0.0 {
                            update(pos);
                            update(end);
                            update(mid);
                        }
                        pos = end;
                    }
                }

                // ── Counter-clockwise arc ─────────────────────────────────
                Command::G3(ref form) => {
                    if let Some(seg) = make_arc_segment(pos, form, false) {
                        let end = arc_endpoint(&seg);
                        let mid = arc_midpoint(&seg);
                        if laser_s > 0.0 {
                            update(pos);
                            update(end);
                            update(mid);
                        }
                        pos = end;
                    }
                }

                Command::G90 => {
                    mode = PositionMode::Absolute;
                }
                Command::G91 => {
                    mode = PositionMode::Relative;
                }

                _ => {
                    // Pick up S words carried on non-motion commands (M3/M4/M5 etc.)
                    // gcode-nom parses them as their own command type; we fall through
                    // and try the raw-line fallback below.
                }
            }

            // Fallback: scan raw line for S word so we catch M4/M3/M5 etc.
            if let Some(s) = extract_s_word(line) {
                laser_s = s;
            }
        }

        if found { Some((min, max)) } else { None }
    }

    /// Extract an S word value from a raw GCode line string.
    /// Matches `S` followed by an optional sign and digits (with optional decimal).
    fn extract_s_word(line: &str) -> Option<f64> {
        let upper = line.to_ascii_uppercase();
        let idx = upper.find('S')?;
        let rest = &upper[idx + 1..];
        // Read digits (and optional leading sign / decimal point)
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
            .unwrap_or(rest.len());
        rest[..end].parse::<f64>().ok()
    }

    /// Extract an S word from a parsed `HashSet<PosVal>`.
    fn extract_s_from_params(params: &std::collections::HashSet<PosVal>) -> Option<f64> {
        for p in params {
            if let PosVal::S(s) = p {
                return Some(*s);
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // ── laser_bounding_box ─────────────────────────────────────────────

        #[test]
        fn bb_empty_gcode_returns_none() {
            assert!(laser_bounding_box("").is_none());
        }

        #[test]
        fn bb_only_rapid_moves_returns_none() {
            // G0 moves with no laser-on S word should never contribute
            let gcode = "G90\nG0 X10 Y10\nG0 X20 Y20\n";
            assert!(laser_bounding_box(gcode).is_none());
        }

        #[test]
        fn bb_simple_laser_on_g1() {
            // A single G1 move with S > 0 should produce a bounding box
            let gcode = "G90\nG1 X10 Y5 S500\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some(), "expected Some bounding box");
            let ((x0, y0), (x1, y1)) = result.unwrap();
            assert!((x0 - 0.0).abs() < 1e-9, "min x should be 0 (origin)");
            assert!((y0 - 0.0).abs() < 1e-9, "min y should be 0 (origin)");
            assert!((x1 - 10.0).abs() < 1e-9, "max x should be 10");
            assert!((y1 - 5.0).abs() < 1e-9, "max y should be 5");
        }

        #[test]
        fn bb_s_word_is_sticky() {
            // S set on one line stays in effect for subsequent G1 moves
            let gcode = "G90\nG1 X10 Y0 S800\nG1 X10 Y20\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((_, y0), (_, y1)) = result.unwrap();
            assert!((y0 - 0.0).abs() < 1e-9);
            assert!(
                (y1 - 20.0).abs() < 1e-9,
                "sticky S should make second G1 contribute"
            );
        }

        #[test]
        fn bb_s0_turns_laser_off() {
            // After S0 the move should not contribute to the bounding box
            let gcode = "G90\nG1 X10 Y0 S800\nG1 X10 Y30 S0\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((_, _), (_, y1)) = result.unwrap();
            // The S0 move ends at Y30 but must not expand the box beyond Y0
            assert!(
                y1 < 1.0,
                "S0 move to Y30 must not expand the bounding box, got y1={y1}"
            );
        }

        #[test]
        fn bb_g0_never_contributes() {
            // G0 moves are always rapid (laser off) regardless of prior S value
            let gcode = "G90\nG1 X5 Y5 S500\nG0 X100 Y100\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((_, _), (x1, y1)) = result.unwrap();
            assert!(
                x1 <= 5.0 + 1e-9,
                "G0 to X100 must not expand bounding box, got x1={x1}"
            );
            assert!(
                y1 <= 5.0 + 1e-9,
                "G0 to Y100 must not expand bounding box, got y1={y1}"
            );
        }

        #[test]
        fn bb_relative_mode_accumulates_correctly() {
            // G91 relative positioning: moves are relative to current position
            let gcode = "G91\nG1 X10 Y0 S600\nG1 X0 Y10\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((x0, y0), (x1, y1)) = result.unwrap();
            assert!((x0 - 0.0).abs() < 1e-9, "origin x=0");
            assert!((y0 - 0.0).abs() < 1e-9, "origin y=0");
            assert!((x1 - 10.0).abs() < 1e-9, "relative X+10 from 0 = 10");
            assert!((y1 - 10.0).abs() < 1e-9, "relative Y+10 from 0 = 10");
        }

        #[test]
        fn bb_s_word_on_non_motion_line_is_picked_up() {
            // S word on an M4 line (before any G1) should set the sticky S value
            let gcode = "G90\nM4 S1000\nG1 X20 Y20\n";
            let result = laser_bounding_box(gcode);
            assert!(
                result.is_some(),
                "S word on M4 line should set sticky S so subsequent G1 contributes"
            );
        }

        #[test]
        fn bb_multiline_expands_correctly() {
            // Several laser-on segments; bounding box must cover all of them
            let gcode = "G90\nG1 X10 Y0 S500\nG1 X10 Y15\nG1 X-5 Y15\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((x0, _), (x1, y1)) = result.unwrap();
            assert!((x0 - -5.0).abs() < 1e-9, "min x should be -5");
            assert!((x1 - 10.0).abs() < 1e-9, "max x should be 10");
            assert!((y1 - 15.0).abs() < 1e-9, "max y should be 15");
        }

        #[test]
        fn bb_g90_restores_absolute_mode() {
            // Switch to relative then back to absolute; positions must be resolved correctly
            let gcode = "G90\nG1 X5 Y5 S500\nG91\nG1 X5 Y5\nG90\nG1 X0 Y0\n";
            let result = laser_bounding_box(gcode);
            assert!(result.is_some());
            let ((x0, y0), (x1, y1)) = result.unwrap();
            // After G91 X5 Y5 from (5,5) we are at (10,10). G90 X0 Y0 → (0,0)
            assert!(
                (x0 - 0.0).abs() < 1e-9,
                "x0 should be 0 after absolute G1 X0"
            );
            assert!(
                (y0 - 0.0).abs() < 1e-9,
                "y0 should be 0 after absolute G1 Y0"
            );
            assert!((x1 - 10.0).abs() < 1e-9, "x1 should be 10");
            assert!((y1 - 10.0).abs() < 1e-9, "y1 should be 10");
        }
    }

    /// Resolve next X/Y position from params (mirrors the private `resolve_xy`).
    fn resolve_xy_bb(
        params: &std::collections::HashSet<PosVal>,
        current: (f64, f64),
        mode: &PositionMode,
    ) -> (f64, f64) {
        let mut x = current.0;
        let mut y = current.1;
        for p in params {
            match p {
                PosVal::X(v) => {
                    x = if *mode == PositionMode::Absolute {
                        *v
                    } else {
                        current.0 + v
                    };
                }
                PosVal::Y(v) => {
                    y = if *mode == PositionMode::Absolute {
                        *v
                    } else {
                        current.1 + v
                    };
                }
                _ => {}
            }
        }
        (x, y)
    }
}

// ── GCode → preview image ─────────────────────────────────────────────────────

/// Colour scheme for the toolpath preview.
struct PreviewPalette {
    background: Rgba<u8>,
    rapid: Rgba<u8>, // G0 moves (laser off)
    cut: Rgba<u8>,   // G1 moves (laser on)
    arc: Rgba<u8>,   // G2/G3 arc moves
}

impl Default for PreviewPalette {
    fn default() -> Self {
        Self {
            background: Rgba([18, 18, 18, 255]),
            rapid: Rgba([60, 60, 80, 255]),
            cut: Rgba([0, 220, 80, 255]),
            arc: Rgba([0, 180, 220, 255]),
        }
    }
}

/// A 2-D move segment produced by walking the GCode.
#[derive(Debug, Clone, Copy)]
enum Segment {
    /// Rapid (non-cutting) linear move.
    Rapid { from: (f64, f64), to: (f64, f64) },
    /// Cutting linear move (G1).
    Cut { from: (f64, f64), to: (f64, f64) },
    /// Arc move (G2 clockwise / G3 counter-clockwise).
    Arc {
        from: (f64, f64),
        center: (f64, f64),
        radius: f64,
        theta_start: f64,
        theta_end: f64,
        clockwise: bool,
    },
}

/// Walk GCode text line-by-line using `gcode-nom` and collect move segments.
fn collect_segments(gcode: &str) -> (Vec<Segment>, (f64, f64), (f64, f64)) {
    let mut segments: Vec<Segment> = Vec::new();

    let mut pos = (0.0_f64, 0.0_f64);
    let mut mode = PositionMode::Absolute;

    // Bounding box
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);

    let update_bb = |pt: (f64, f64), min: &mut (f64, f64), max: &mut (f64, f64)| {
        if pt.0 < min.0 {
            min.0 = pt.0;
        }
        if pt.1 < min.1 {
            min.1 = pt.1;
        }
        if pt.0 > max.0 {
            max.0 = pt.0;
        }
        if pt.1 > max.1 {
            max.1 = pt.1;
        }
    };

    update_bb(pos, &mut min, &mut max);

    for line in gcode.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok((_, cmd)) = Command::parse_line(line) else {
            continue;
        };

        match cmd {
            // ── Rapid move ────────────────────────────────────────────────
            Command::G0(params) => {
                let (nx, ny) = resolve_xy(&params, pos, &mode);
                update_bb((nx, ny), &mut min, &mut max);
                segments.push(Segment::Rapid {
                    from: pos,
                    to: (nx, ny),
                });
                pos = (nx, ny);
            }

            // ── Linear cut ────────────────────────────────────────────────
            Command::G1(params) => {
                let (nx, ny) = resolve_xy(&params, pos, &mode);
                update_bb((nx, ny), &mut min, &mut max);
                segments.push(Segment::Cut {
                    from: pos,
                    to: (nx, ny),
                });
                pos = (nx, ny);
            }

            // ── Clockwise arc (G2) ────────────────────────────────────────
            Command::G2(form) => {
                if let Some(seg) = make_arc_segment(pos, &form, true) {
                    let end = arc_endpoint(&seg);
                    update_bb(end, &mut min, &mut max);
                    update_bb(arc_midpoint(&seg), &mut min, &mut max);
                    pos = end;
                    segments.push(seg);
                }
            }

            // ── Counter-clockwise arc (G3) ────────────────────────────────
            Command::G3(form) => {
                if let Some(seg) = make_arc_segment(pos, &form, false) {
                    let end = arc_endpoint(&seg);
                    update_bb(end, &mut min, &mut max);
                    update_bb(arc_midpoint(&seg), &mut min, &mut max);
                    pos = end;
                    segments.push(seg);
                }
            }

            // ── Unit / position mode ──────────────────────────────────────
            Command::G90 => {
                mode = PositionMode::Absolute;
            }
            Command::G91 => {
                mode = PositionMode::Relative;
            }

            _ => {}
        }
    }

    // Guard against empty / single-point gcode
    if min.0 == f64::MAX {
        min = (0.0, 0.0);
        max = (1.0, 1.0);
    }
    if (max.0 - min.0).abs() < 1e-9 {
        max.0 = min.0 + 1.0;
    }
    if (max.1 - min.1).abs() < 1e-9 {
        max.1 = min.1 + 1.0;
    }

    (segments, min, max)
}

/// Resolve next X/Y from a G0/G1 parameter set, handling absolute/relative.
fn resolve_xy(
    params: &std::collections::HashSet<PosVal>,
    current: (f64, f64),
    mode: &PositionMode,
) -> (f64, f64) {
    let mut x = current.0;
    let mut y = current.1;
    for p in params {
        match p {
            PosVal::X(v) => {
                x = if *mode == PositionMode::Absolute {
                    *v
                } else {
                    current.0 + v
                }
            }
            PosVal::Y(v) => {
                y = if *mode == PositionMode::Absolute {
                    *v
                } else {
                    current.1 + v
                }
            }
            _ => {}
        }
    }
    (x, y)
}

/// Build an `Arc` segment from a G2/G3 form, returning `None` if the form is
/// not the IJ variant (R-form is not yet implemented in gcode-nom).
fn make_arc_segment(pos: (f64, f64), form: &ArcForm, clockwise: bool) -> Option<Segment> {
    match form {
        ArcForm::IJ(_) => {
            let params = compute_arc(pos.0, pos.1, form);
            Some(Segment::Arc {
                from: pos,
                center: params.center,
                radius: params.radius,
                theta_start: params.theta_start,
                theta_end: params.theta_end,
                clockwise,
            })
        }
        ArcForm::R(_) => None, // R-form is not yet implemented in gcode-nom
    }
}

fn arc_endpoint(seg: &Segment) -> (f64, f64) {
    if let Segment::Arc {
        center,
        radius,
        theta_end,
        ..
    } = seg
    {
        (
            center.0 + radius * theta_end.cos(),
            center.1 + radius * theta_end.sin(),
        )
    } else {
        (0.0, 0.0)
    }
}

fn arc_midpoint(seg: &Segment) -> (f64, f64) {
    if let Segment::Arc {
        center,
        radius,
        theta_start,
        theta_end,
        ..
    } = seg
    {
        let mid = (theta_start + theta_end) / 2.0;
        (center.0 + radius * mid.cos(), center.1 + radius * mid.sin())
    } else {
        (0.0, 0.0)
    }
}

// ── Pixel rasteriser ──────────────────────────────────────────────────────────

/// Render the parsed GCode segments into an RGBA pixel image.
///
/// The image dimensions are capped at `max_w × max_h` pixels to avoid
/// allocating enormous buffers for huge workpieces.
pub fn gcode_to_image(gcode: &str, max_w: u32, max_h: u32) -> Result<RgbaImage> {
    let palette = PreviewPalette::default();
    let (segments, bb_min, bb_max) = collect_segments(gcode);

    let w = max_w.max(64);
    let h = max_h.max(64);

    let mut img = RgbaImage::from_pixel(w, h, palette.background);

    // Margin in pixels
    let margin = 8u32;
    let draw_w = (w - 2 * margin) as f64;
    let draw_h = (h - 2 * margin) as f64;

    let scale_x = draw_w / (bb_max.0 - bb_min.0);
    let scale_y = draw_h / (bb_max.1 - bb_min.1);
    // Uniform scale so aspect ratio is preserved
    let scale = scale_x.min(scale_y);

    // Centre the drawing inside the image
    let world_w = (bb_max.0 - bb_min.0) * scale;
    let world_h = (bb_max.1 - bb_min.1) * scale;
    let offset_x = margin as f64 + (draw_w - world_w) / 2.0;
    let offset_y = margin as f64 + (draw_h - world_h) / 2.0;

    let to_pixel = |wx: f64, wy: f64| -> (i64, i64) {
        let px = ((wx - bb_min.0) * scale + offset_x).round() as i64;
        // Flip Y: GCode Y+ is "up", image Y+ is "down"
        let py = (h as f64 - ((wy - bb_min.1) * scale + offset_y)).round() as i64;
        (px, py)
    };

    for seg in &segments {
        match *seg {
            Segment::Rapid { from, to } => {
                draw_line(
                    &mut img,
                    to_pixel(from.0, from.1),
                    to_pixel(to.0, to.1),
                    palette.rapid,
                );
            }
            Segment::Cut { from, to } => {
                draw_line(
                    &mut img,
                    to_pixel(from.0, from.1),
                    to_pixel(to.0, to.1),
                    palette.cut,
                );
            }
            Segment::Arc {
                center,
                radius,
                theta_start,
                theta_end,
                clockwise,
                ..
            } => {
                draw_arc(
                    &mut img,
                    center,
                    radius,
                    theta_start,
                    theta_end,
                    clockwise,
                    &to_pixel,
                    palette.arc,
                );
            }
        }
    }

    Ok(img)
}

// ── Bresenham line drawing ────────────────────────────────────────────────────

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn valid_settings() -> MachineSettings {
        MachineSettings::default()
    }

    // ── validate_settings ─────────────────────────────────────────────────────

    #[test]
    fn validate_settings_accepts_defaults() {
        assert!(validate_settings(&valid_settings()).is_ok());
    }

    #[test]
    fn validate_settings_rejects_zero_feedrate() {
        let mut s = valid_settings();
        s.feedrate = 0.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Feedrate"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_negative_feedrate() {
        let mut s = valid_settings();
        s.feedrate = -100.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Feedrate"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_feedrate_above_max_speed() {
        let mut s = valid_settings();
        s.max_speed = 1000.0;
        s.feedrate = 2000.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(
            err.body.contains("max speed"),
            "expected max speed message, got: {}",
            err.body
        );
    }

    #[test]
    fn validate_settings_rejects_zero_tolerance() {
        let mut s = valid_settings();
        s.tolerance = 0.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Tolerance"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_negative_tolerance() {
        let mut s = valid_settings();
        s.tolerance = -0.01;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Tolerance"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_zero_dpi() {
        let mut s = valid_settings();
        s.dpi = 0.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("DPI"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_negative_laser_power() {
        let mut s = valid_settings();
        s.laser_power = -1.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Laser power"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_laser_power_above_max() {
        let mut s = valid_settings();
        s.max_laser_power = 500.0;
        s.laser_power = 600.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Laser power"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_zero_work_area() {
        let mut s = valid_settings();
        s.max_x_mm = 0.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Work area"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_rejects_negative_origin() {
        let mut s = valid_settings();
        s.origin_x = -1.0;
        let err = validate_settings(&s).unwrap_err();
        assert!(err.body.contains("Origin"), "body: {}", err.body);
    }

    #[test]
    fn validate_settings_accumulates_multiple_errors() {
        let mut s = valid_settings();
        s.feedrate = 0.0;
        s.tolerance = 0.0;
        s.dpi = 0.0;
        let err = validate_settings(&s).unwrap_err();
        // All three problems should appear in the body
        assert!(err.body.contains("Feedrate"), "body: {}", err.body);
        assert!(err.body.contains("Tolerance"), "body: {}", err.body);
        assert!(err.body.contains("DPI"), "body: {}", err.body);
    }

    // ── validate_extents ──────────────────────────────────────────────────────

    #[test]
    fn validate_extents_accepts_job_inside_work_area() {
        let s = valid_settings(); // max 150 × 150 mm
        assert!(validate_extents(&s, (0.0, 0.0), (100.0, 100.0)).is_ok());
    }

    #[test]
    fn validate_extents_accepts_job_exactly_at_limit() {
        let s = valid_settings();
        assert!(validate_extents(&s, (0.0, 0.0), (150.0, 150.0)).is_ok());
    }

    #[test]
    fn validate_extents_rejects_x_overrun() {
        let s = valid_settings();
        let err = validate_extents(&s, (0.0, 0.0), (160.0, 100.0)).unwrap_err();
        assert_eq!(err.title, "Job exceeds work area");
        assert!(
            err.body.contains("machine limit"),
            "expected machine limit message, got: {}",
            err.body
        );
    }

    #[test]
    fn validate_extents_rejects_y_overrun() {
        let s = valid_settings();
        let err = validate_extents(&s, (0.0, 0.0), (100.0, 200.0)).unwrap_err();
        assert_eq!(err.title, "Job exceeds work area");
        assert!(err.body.contains("machine limit"), "body: {}", err.body);
    }

    #[test]
    fn validate_extents_rejects_negative_x_min() {
        let s = valid_settings();
        let err = validate_extents(&s, (-5.0, 0.0), (50.0, 50.0)).unwrap_err();
        assert!(
            err.body.contains("must be ≥ 0"),
            "expected negative-X message, got: {}",
            err.body
        );
    }

    #[test]
    fn validate_extents_rejects_negative_y_min() {
        let s = valid_settings();
        let err = validate_extents(&s, (0.0, -3.0), (50.0, 50.0)).unwrap_err();
        assert!(
            err.body.contains("must be ≥ 0"),
            "expected negative-Y message, got: {}",
            err.body
        );
    }

    #[test]
    fn validate_extents_reports_both_axis_overruns() {
        let s = valid_settings();
        let err = validate_extents(&s, (0.0, 0.0), (200.0, 200.0)).unwrap_err();
        // Body must mention both X and Y overruns
        let x_mention = err.body.contains("X 200");
        let y_mention = err.body.contains("Y 200");
        assert!(
            x_mention && y_mention,
            "expected both X and Y overrun messages, got: {}",
            err.body
        );
    }

    #[test]
    fn validate_extents_body_contains_job_size_summary() {
        let s = valid_settings();
        let err = validate_extents(&s, (0.0, 0.0), (200.0, 175.0)).unwrap_err();
        assert!(
            err.body.contains("Job size"),
            "expected job size summary, got: {}",
            err.body
        );
        assert!(
            err.body.contains("Work area"),
            "expected work area summary, got: {}",
            err.body
        );
    }
}

fn put_pixel_safe(img: &mut RgbaImage, x: i64, y: i64, colour: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        img.put_pixel(x as u32, y as u32, colour);
    }
}

fn draw_line(img: &mut RgbaImage, from: (i64, i64), to: (i64, i64), colour: Rgba<u8>) {
    // Integer Bresenham
    let (mut x0, mut y0) = from;
    let (x1, y1) = to;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx: i64 = if x0 < x1 { 1 } else { -1 };
    let sy: i64 = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        put_pixel_safe(img, x0, y0, colour);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn draw_arc(
    img: &mut RgbaImage,
    center: (f64, f64),
    radius: f64,
    theta_start: f64,
    theta_end: f64,
    clockwise: bool,
    to_pixel: &impl Fn(f64, f64) -> (i64, i64),
    colour: Rgba<u8>,
) {
    if radius < 1e-9 {
        return;
    }

    // Number of line segments to approximate the arc.
    // More segments for larger radii.
    let circumference = 2.0 * PI * radius;
    let steps = ((circumference * 4.0) as usize).clamp(16, 2048);

    // Angular span – handle the direction
    let span = if clockwise {
        // CW: theta decreases from start to end
        let mut s = theta_start - theta_end;
        if s <= 0.0 {
            s += 2.0 * PI;
        }
        s
    } else {
        // CCW: theta increases from start to end
        let mut s = theta_end - theta_start;
        if s <= 0.0 {
            s += 2.0 * PI;
        }
        s
    };

    let d_theta = span / steps as f64;
    let sign = if clockwise { -1.0_f64 } else { 1.0_f64 };

    let mut prev = to_pixel(
        center.0 + radius * theta_start.cos(),
        center.1 + radius * theta_start.sin(),
    );

    for i in 1..=steps {
        let theta = theta_start + sign * d_theta * i as f64;
        let wx = center.0 + radius * theta.cos();
        let wy = center.1 + radius * theta.sin();
        let cur = to_pixel(wx, wy);
        draw_line(img, prev, cur, colour);
        prev = cur;
    }
}

// ── SVG → pixel image (source SVG preview) ───────────────────────────────────

/// Render an SVG file to an RGBA pixel image using `resvg`.
///
/// This is used to show the *original SVG* before conversion, in the Preview tab.
pub fn svg_to_image(svg_path: &Path, max_w: u32, max_h: u32) -> Result<RgbaImage> {
    let svg_text = std::fs::read_to_string(svg_path)
        .with_context(|| format!("Cannot read SVG: {}", svg_path.display()))?;

    let options = usvg::Options::default();
    let tree =
        usvg::Tree::from_str(&svg_text, &options).context("Failed to parse SVG for rendering")?;

    let svg_size = tree.size();
    let svg_w = svg_size.width();
    let svg_h = svg_size.height();

    // Compute a scale that fits within max_w × max_h
    let scale = (max_w as f32 / svg_w).min(max_h as f32 / svg_h).min(1.0);
    let px_w = ((svg_w * scale) as u32).max(1);
    let px_h = ((svg_h * scale) as u32).max(1);

    let mut pixmap = tiny_skia::Pixmap::new(px_w, px_h).context("Failed to allocate pixmap")?;

    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia uses premultiplied RGBA; convert to straight RGBA for `image`
    let raw = pixmap.take();
    let mut rgba = RgbaImage::new(px_w, px_h);
    for (i, pixel) in rgba.pixels_mut().enumerate() {
        let base = i * 4;
        let a = raw[base + 3];
        if a == 0 {
            *pixel = Rgba([0, 0, 0, 0]);
        } else {
            // Un-premultiply
            let inv = 255.0 / a as f32;
            *pixel = Rgba([
                ((raw[base] as f32) * inv) as u8,
                ((raw[base + 1] as f32) * inv) as u8,
                ((raw[base + 2] as f32) * inv) as u8,
                a,
            ]);
        }
    }

    Ok(rgba)
}
