use std::fmt::Debug;

use g_code::emit::Token;
use lyon_geom::euclid::default::Transform2D;
use roxmltree::{Document, Node};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use svgtypes::Length;
use uom::si::{
    f64::Length as UomLength,
    length::{inch, millimeter},
};

use self::units::CSS_DEFAULT_DPI;
use crate::{Machine, turtle::*};

#[cfg(feature = "serde")]
mod length_serde;
mod path;
mod transform;
mod units;
mod visit;

/// High-level output configuration
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ConversionConfig {
    /// Curve interpolation tolerance in millimeters
    pub tolerance: f64,
    /// Feedrate in millimeters / minute
    pub feedrate: f64,
    /// Dots per inch for pixels, picas, points, etc.
    pub dpi: f64,
    /// Set the origin point in millimeters for this conversion
    #[cfg_attr(feature = "serde", serde(default = "zero_origin"))]
    pub origin: [Option<f64>; 2],
    /// Set extra attribute to add when printing node name
    pub extra_attribute_name: Option<String>,
}

const fn zero_origin() -> [Option<f64>; 2] {
    [Some(0.); 2]
}

impl Default for ConversionConfig {
    fn default() -> Self {
        Self {
            tolerance: 0.002,
            feedrate: 300.0,
            dpi: 96.0,
            origin: zero_origin(),
            extra_attribute_name: None,
        }
    }
}

/// Options are specific to this conversion.
///
/// This is separate from [ConversionConfig] to support bulk processing in the web interface.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ConversionOptions {
    /// Width and height override
    ///
    /// Useful when an SVG does not have a set width and height or you want to override it.
    #[cfg_attr(feature = "serde", serde(with = "length_serde"))]
    pub dimensions: [Option<Length>; 2],
}

/// Maps SVG [`Node`]s and their attributes into operations on a [`Terrarium`]
#[derive(Debug)]
struct ConversionVisitor<'a, T: Turtle> {
    terrarium: Terrarium<T>,
    name_stack: Vec<String>,
    /// Used to convert percentage values
    viewport_dim_stack: Vec<[f64; 2]>,
    /// Stack of per-layer overrides pushed for every `<g>` element encountered.
    /// Each entry records the values that *this specific group* declared (all `None` when
    /// the group carried no relevant attributes). `visit_exit` uses this to know whether
    /// it needs to restore an earlier override when leaving the group.
    layer_override_stack: Vec<LayerOverride>,
    _config: &'a ConversionConfig,
    options: ConversionOptions,
}

/// Per-group layer settings read from `data-*` attributes.
#[derive(Debug, Default, Clone, Copy)]
struct LayerOverride {
    /// `data-feedrate` — overrides the global feedrate (mm/min) for this layer.
    feedrate: Option<f64>,
    /// `data-power` — overrides the laser power (S word) for this layer.
    power: Option<f64>,
    /// `data-passes` — how many times every path in this layer should be repeated.
    passes: Option<u32>,
}

impl LayerOverride {
    fn has_any(&self) -> bool {
        self.feedrate.is_some() || self.power.is_some() || self.passes.is_some()
    }
}

impl<'a, T: Turtle> ConversionVisitor<'a, T> {
    /// Returns the number of passes that should be applied to shapes in the current scope.
    /// Walks the stack from innermost to outermost and returns the first explicit value found,
    /// falling back to 1 if no enclosing group specified `data-passes`.
    fn current_passes(&self) -> u32 {
        self.layer_override_stack
            .iter()
            .rev()
            .find_map(|o| o.passes)
            .unwrap_or(1)
    }

    fn comment(&mut self, node: &Node) {
        let mut comment = String::new();
        self.name_stack.iter().for_each(|name| {
            comment += name;
            comment += " > ";
        });
        comment += &node_name(node, &self._config.extra_attribute_name);

        self.terrarium.turtle.comment(comment);
    }

    fn begin(&mut self) {
        // Part 1 of converting from SVG to GCode coordinates
        self.terrarium.push_transform(Transform2D::scale(1., -1.));
        self.terrarium.turtle.begin();
    }

    fn end(&mut self) {
        self.terrarium.turtle.end();
        self.terrarium.pop_transform();
    }
}

/// Top-level function for converting an SVG [`Document`] into g-code
pub fn svg2program<'a, 'input: 'a>(
    doc: &'a Document,
    config: &ConversionConfig,
    options: ConversionOptions,
    machine: Machine<'input>,
) -> Vec<Token<'input>> {
    let bounding_box_generator = || {
        let mut visitor = ConversionVisitor {
            terrarium: Terrarium::new(DpiConvertingTurtle {
                inner: PreprocessTurtle::default(),
                dpi: config.dpi,
            }),
            _config: config,
            options: options.clone(),
            name_stack: vec![],
            viewport_dim_stack: vec![],
            layer_override_stack: vec![],
        };

        visitor.begin();
        visit::depth_first_visit(doc, &mut visitor);
        visitor.end();

        visitor.terrarium.turtle.inner.bounding_box
    };

    // Convert from millimeters to user units
    let origin = config
        .origin
        .map(|dim| dim.map(|d| UomLength::new::<millimeter>(d).get::<inch>() * CSS_DEFAULT_DPI));

    let origin_transform = match origin {
        [None, Some(origin_y)] => {
            let bb = bounding_box_generator();
            Transform2D::translation(0., origin_y - bb.min.y)
        }
        [Some(origin_x), None] => {
            let bb = bounding_box_generator();
            Transform2D::translation(origin_x - bb.min.x, 0.)
        }
        [Some(origin_x), Some(origin_y)] => {
            let bb = bounding_box_generator();
            Transform2D::translation(origin_x - bb.min.x, origin_y - bb.min.y)
        }
        [None, None] => Transform2D::identity(),
    };

    let mut conversion_visitor = ConversionVisitor {
        terrarium: Terrarium::new(DpiConvertingTurtle {
            inner: GCodeTurtle {
                machine,
                tolerance: config.tolerance,
                feedrate: config.feedrate,
                layer_feedrate: None,
                layer_power: None,
                program: vec![],
            },
            dpi: config.dpi,
        }),
        _config: config,
        options,
        name_stack: vec![],
        viewport_dim_stack: vec![],
        layer_override_stack: vec![],
    };

    conversion_visitor
        .terrarium
        .push_transform(origin_transform);
    conversion_visitor.begin();
    visit::depth_first_visit(doc, &mut conversion_visitor);
    conversion_visitor.end();
    conversion_visitor.terrarium.pop_transform();

    conversion_visitor.terrarium.turtle.inner.program
}

fn node_name(node: &Node, attr_to_print: &Option<String>) -> String {
    let mut name = node.tag_name().name().to_string();
    if let Some(id) = node.attribute("id") {
        name += "#";
        name += id;
        if let Some(extra_attr_to_print) = attr_to_print {
            for a_attr in node.attributes() {
                if a_attr.name() == extra_attr_to_print {
                    name += " ( ";
                    name += a_attr.value();
                    name += " ) ";
                }
            }
        }
    }
    name
}

#[cfg(all(test, feature = "serde"))]
mod test {
    use svgtypes::LengthUnit;

    use super::*;

    #[test]
    fn serde_conversion_options_is_correct() {
        let default_struct = ConversionOptions::default();
        let default_json = r#"{"dimensions":[null,null]}"#;

        assert_eq!(
            serde_json::to_string(&default_struct).unwrap(),
            default_json
        );
        assert_eq!(
            serde_json::from_str::<ConversionOptions>(default_json).unwrap(),
            default_struct
        );
    }

    #[test]
    fn serde_conversion_options_with_single_dimension_is_correct() {
        let mut r#struct = ConversionOptions::default();
        r#struct.dimensions[0] = Some(Length {
            number: 4.,
            unit: LengthUnit::Mm,
        });
        let json = r#"{"dimensions":[{"number":4.0,"unit":"Mm"},null]}"#;

        assert_eq!(serde_json::to_string(&r#struct).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<ConversionOptions>(json).unwrap(),
            r#struct
        );
    }

    #[test]
    fn serde_conversion_options_with_both_dimensions_is_correct() {
        let mut r#struct = ConversionOptions::default();
        r#struct.dimensions = [
            Some(Length {
                number: 4.,
                unit: LengthUnit::Mm,
            }),
            Some(Length {
                number: 10.5,
                unit: LengthUnit::In,
            }),
        ];
        let json = r#"{"dimensions":[{"number":4.0,"unit":"Mm"},{"number":10.5,"unit":"In"}]}"#;

        assert_eq!(serde_json::to_string(&r#struct).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<ConversionOptions>(json).unwrap(),
            r#struct
        );
    }
}
