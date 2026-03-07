use std::{borrow::Cow, fmt::Debug};

use ::g_code::{command, emit::Token};

/// Round a millimeter value to 3 decimal places (1 micron precision).
/// This keeps G-code lines short and avoids floating-point noise like
/// `X9.000000000000002` which wastes characters and can confuse some parsers.
///
/// Only applied at the final emission point for coordinate/feedrate words.
/// Arc radii and intermediate geometry values are kept at full precision so
/// the flattening / arc-splitting logic is unaffected.
#[inline]
fn mm3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
use lyon_geom::{CubicBezierSegment, Point, QuadraticBezierSegment, SvgArc};

use super::Turtle;
use crate::{
    arc::{ArcOrLineSegment, FlattenWithArcs},
    machine::Machine,
};

/// Maps path segments into g-code operations
#[derive(Debug)]
pub struct GCodeTurtle<'input> {
    pub machine: Machine<'input>,
    pub tolerance: f64,
    /// Global feedrate in mm/min (from ConversionConfig)
    pub feedrate: f64,
    /// Per-layer feedrate override in mm/min
    pub layer_feedrate: Option<f64>,
    /// Per-layer laser power override. Emitted as the S word inline on G1 commands.
    /// Range is firmware-dependent (GRBL: 0–1000, Marlin: 0–255).
    pub layer_power: Option<f64>,
    pub program: Vec<Token<'input>>,
}

impl<'input> GCodeTurtle<'input> {
    /// Returns the effective feedrate: layer override if set, otherwise the global feedrate.
    fn effective_feedrate(&self) -> f64 {
        self.layer_feedrate.unwrap_or(self.feedrate)
    }

    fn circular_interpolation(&self, svg_arc: SvgArc<f64>) -> Vec<Token<'input>> {
        debug_assert!((svg_arc.radii.x.abs() - svg_arc.radii.y.abs()).abs() < f64::EPSILON);
        // Feedrate is purely cosmetic in the output, safe to round.
        let f = mm3(self.effective_feedrate());
        // G2/G3 do not carry an S word in the g_code crate's command model.
        // Power is already established by the preceding G1 or S-word emission, so
        // no special handling is needed here for GRBL laser mode.
        //
        // NOTE: only the *output* X/Y/R values are rounded here. The radius used
        // to decide whether to split the arc (svg_arc.radii.x in the recursive
        // `true` branch) is left at full precision so the flattening geometry
        // is unaffected by rounding.
        match (svg_arc.flags.large_arc, svg_arc.flags.sweep) {
            (false, true) => command!(CounterclockwiseCircularInterpolation {
                X: mm3(svg_arc.to.x),
                Y: mm3(svg_arc.to.y),
                R: mm3(svg_arc.radii.x),
                F: f,
            })
            .into_token_vec(),
            (false, false) => command!(ClockwiseCircularInterpolation {
                X: mm3(svg_arc.to.x),
                Y: mm3(svg_arc.to.y),
                R: mm3(svg_arc.radii.x),
                F: f,
            })
            .into_token_vec(),
            (true, _) => {
                // Split at full precision — rounding radii here would change
                // which sub-arcs are produced and alter the token count.
                let (left, right) = svg_arc.to_arc().split(0.5);
                let mut token_vec = self.circular_interpolation(left.to_svg_arc());
                token_vec.append(&mut self.circular_interpolation(right.to_svg_arc()));
                token_vec
            }
        }
    }

    fn tool_on(&mut self) {
        self.program.extend(self.machine.tool_on());
        self.program.extend(self.machine.absolute());
    }

    fn tool_off(&mut self) {
        self.program.extend(self.machine.tool_off());
        self.program.extend(self.machine.absolute());
    }
}

impl<'input> Turtle for GCodeTurtle<'input> {
    fn begin(&mut self) {
        self.program
            .append(&mut command!(UnitsMillimeters {}).into_token_vec());
        self.program.extend(self.machine.absolute());
        self.program.extend(self.machine.program_begin());
        self.program.extend(self.machine.absolute());
    }

    fn end(&mut self) {
        self.program.extend(self.machine.tool_off());
        self.program.extend(self.machine.absolute());
        self.program.extend(self.machine.program_end());
    }

    fn comment(&mut self, comment: String) {
        self.program.push(Token::Comment {
            is_inline: false,
            inner: Cow::Owned(comment),
        });
    }

    fn move_to(&mut self, to: Point<f64>) {
        self.tool_off();
        // G0 automatically disables the laser in GRBL laser mode ($32=1), so no
        // explicit power-off S word is needed here.
        self.program.append(
            &mut command!(RapidPositioning {
                X: mm3(to.x),
                Y: mm3(to.y)
            })
            .into_token_vec(),
        );
    }

    fn line_to(&mut self, to: Point<f64>) {
        self.tool_on();
        let f = mm3(self.effective_feedrate());
        // Emit the S word (laser power) inline on every G1 when a layer override is
        // active. GRBL moves through consecutive S changes without stopping, so this
        // is both correct and efficient. When no override is set we omit S entirely
        // and let the firmware use whatever was last programmed (or the tool_on_sequence).
        let tokens = match self.layer_power {
            Some(s) => command!(LinearInterpolation {
                X: mm3(to.x),
                Y: mm3(to.y),
                F: f,
                S: s,
            })
            .into_token_vec(),
            None => command!(LinearInterpolation {
                X: mm3(to.x),
                Y: mm3(to.y),
                F: f,
            })
            .into_token_vec(),
        };
        self.program.extend(tokens);
    }

    fn arc(&mut self, svg_arc: SvgArc<f64>) {
        if svg_arc.is_straight_line() {
            self.line_to(svg_arc.to);
            return;
        }

        self.tool_on();

        if self
            .machine
            .supported_functionality()
            .circular_interpolation
        {
            FlattenWithArcs::flattened(&svg_arc, self.tolerance)
                .into_iter()
                .for_each(|segment| match segment {
                    ArcOrLineSegment::Arc(arc) => {
                        self.program.append(&mut self.circular_interpolation(arc))
                    }
                    ArcOrLineSegment::Line(line) => {
                        self.line_to(line.to);
                    }
                });
        } else {
            svg_arc
                .to_arc()
                .flattened(self.tolerance)
                .for_each(|point| self.line_to(point));
        };
    }

    fn cubic_bezier(&mut self, cbs: CubicBezierSegment<f64>) {
        self.tool_on();

        if self
            .machine
            .supported_functionality()
            .circular_interpolation
        {
            FlattenWithArcs::<f64>::flattened(&cbs, self.tolerance)
                .into_iter()
                .for_each(|segment| match segment {
                    ArcOrLineSegment::Arc(arc) => {
                        self.program.append(&mut self.circular_interpolation(arc))
                    }
                    ArcOrLineSegment::Line(line) => self.line_to(line.to),
                });
        } else {
            cbs.flattened(self.tolerance)
                .for_each(|point| self.line_to(point));
        };
    }

    fn quadratic_bezier(&mut self, qbs: QuadraticBezierSegment<f64>) {
        self.cubic_bezier(qbs.to_cubic());
    }

    fn set_layer_overrides(&mut self, feedrate: Option<f64>, power: Option<f64>) {
        self.layer_feedrate = feedrate;
        self.layer_power = power;
    }
}
