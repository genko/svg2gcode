//! SVG → GCode conversion and GCode → preview-image rendering.
//!
//! Two public entry points:
//!
//! - [`svg_to_gcode`]  – calls the `svg2gcode` library and returns the GCode as a `String`.
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

use roxmltree::ParsingOptions;
use svg2gcode::{
    ConversionConfig, ConversionOptions, Machine, SupportedFunctionality, svg2program,
};

use crate::app::MachineSettings;

// ── SVG → GCode ───────────────────────────────────────────────────────────────

/// Convert an SVG file to a GCode string using the full [`MachineSettings`].
///
/// All conversion parameters (feedrate, tolerance, dpi, origin, begin/end
/// sequences, circular interpolation, line numbers, checksums) are taken
/// directly from `settings`.
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

    let begin_snippet = snippet_parser(&settings.begin_sequence)
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

    Ok(gcode_string)
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
