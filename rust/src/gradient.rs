extern crate libc;
extern crate cairo;
extern crate cairo_sys;
extern crate glib_sys;
extern crate glib;

use self::glib::translate::*;

use bbox::*;
use drawing_ctx;
use drawing_ctx::RsvgDrawingCtx;
use length::*;
use node::*;
use paint_server::*;
use stop::*;
use util::*;

use self::cairo::MatrixTrait;

#[derive(Copy, Clone)]
pub struct ColorStop {
    pub offset: f64,
    pub rgba:   u32
}

/* Any of the attributes in gradient elements may be omitted.  In turn, the missing
 * ones can be inherited from the gradient referenced by its "fallback" IRI.  We
 * represent these possibly-missing attributes as Option<foo>.
 */
pub struct GradientCommon {
    pub units:    Option<PaintServerUnits>,
    pub affine:   Option<cairo::Matrix>,
    pub spread:   Option<cairo::enums::Extend>,
    pub fallback: Option<String>,
    pub stops:    Option<Vec<ColorStop>>
}

#[derive(Copy, Clone)]
pub enum GradientVariant {
    Linear {
        x1: Option<RsvgLength>,
        y1: Option<RsvgLength>,
        x2: Option<RsvgLength>,
        y2: Option<RsvgLength>
    },

    Radial {
        cx: Option<RsvgLength>,
        cy: Option<RsvgLength>,
        r:  Option<RsvgLength>,
        fx: Option<RsvgLength>,
        fy: Option<RsvgLength>,
    }
}

pub struct Gradient {
    pub common: GradientCommon,
    pub variant: GradientVariant
}

// All of the Gradient's fields are Option<foo> values, because
// those fields can be omitted in the SVG file.  We need to resolve
// them to default values, or to fallback values that come from
// another Gradient.
//
// For the fallback case, this would need something like
//
//    if self.foo.is_none () { self.foo = fallback.foo; }
//
// And for the default case, it would be like
//    if self.foo.is_none () { self.foo = Some (default_value); }
//
// Both can be replaced by
//
//    self.foo = self.foo.take ().or (bar);
//
// So we define a macro for that.
macro_rules! fallback_to (
    ($dest:expr, $default:expr) => (
        $dest = $dest.take ().or ($default)
    );
);

impl GradientCommon {
    fn new (units:    Option<PaintServerUnits>,
            affine:   Option<cairo::Matrix>,
            spread:   Option<cairo::enums::Extend>,
            fallback: Option<String>,
            stops:    Option<Vec<ColorStop>>) -> GradientCommon {
        GradientCommon {
            units:    units,
            affine:   affine,
            spread:   spread,
            fallback: fallback,
            stops:    stops
        }
    }

    fn clone_stops (&self) -> Option<Vec<ColorStop>> {
        if let Some (ref stops) = self.stops {
            Some (stops.clone ())
        } else {
            None
        }
    }

    fn is_resolved (&self) -> bool {
        self.units.is_some() &&
            self.affine.is_some () &&
            self.spread.is_some () &&
            self.stops.is_some ()
    }

    fn resolve_from_defaults (&mut self) {
        /* These are per the spec */

        fallback_to! (self.units,  Some (PaintServerUnits::default ()));
        fallback_to! (self.affine, Some (cairo::Matrix::identity ()));
        fallback_to! (self.spread, Some (cairo::enums::Extend::Pad));
        fallback_to! (self.stops,  Some (Vec::<ColorStop>::new ())); // empty array of color stops

        self.fallback = None;
    }

    fn resolve_from_fallback (&mut self, fallback: &GradientCommon) {
        fallback_to! (self.units,  fallback.units);
        fallback_to! (self.affine, fallback.affine);
        fallback_to! (self.spread, fallback.spread);
        fallback_to! (self.stops,  fallback.clone_stops ());

        self.fallback = clone_fallback_name (&fallback.fallback);
    }

    fn add_color_stop (&mut self, mut offset: f64, rgba: u32) {
        if self.stops.is_none () {
            self.stops = Some (Vec::<ColorStop>::new ());
        }

        if let Some (ref mut stops) = self.stops {
            let mut last_offset: f64 = 0.0;

            if stops.len () > 0 {
                last_offset = stops[stops.len () - 1].offset;
            }

            if last_offset > offset {
                offset = last_offset;
            }

            stops.push (ColorStop { offset: offset,
                                    rgba:   rgba });
        } else {
            unreachable! ();
        }
    }
}

impl Clone for GradientCommon {
    fn clone (&self) -> Self {
        GradientCommon {
            units:    self.units,
            affine:   self.affine,
            spread:   self.spread,
            fallback: clone_fallback_name (&self.fallback),
            stops:    self.clone_stops ()
        }
    }
}

impl GradientVariant {
    fn is_resolved (&self) -> bool {
        match *self {
            GradientVariant::Linear { x1, y1, x2, y2 } => {
                x1.is_some () &&
                    y1.is_some () &&
                    x2.is_some () &&
                    y2.is_some ()
            },

            GradientVariant::Radial { cx, cy, r, fx, fy } => {
                cx.is_some () &&
                    cy.is_some () &&
                    r.is_some () &&
                    fx.is_some () &&
                    fy.is_some ()
            }
        }
    }

    fn resolve_from_defaults (&mut self) {
        /* These are per the spec */

        match *self {
            GradientVariant::Linear { ref mut x1, ref mut y1, ref mut x2, ref mut y2 } => {
                fallback_to! (*x1, Some (RsvgLength::parse ("0%", LengthDir::Horizontal).unwrap ()));
                fallback_to! (*y1, Some (RsvgLength::parse ("0%", LengthDir::Vertical).unwrap ()));
                fallback_to! (*x2, Some (RsvgLength::parse ("100%", LengthDir::Horizontal).unwrap ()));
                fallback_to! (*y2, Some (RsvgLength::parse ("0%", LengthDir::Vertical).unwrap ()));
            },

            GradientVariant::Radial { ref mut cx, ref mut cy, ref mut r, ref mut fx, ref mut fy } => {
                fallback_to! (*cx, Some (RsvgLength::parse ("50%", LengthDir::Horizontal).unwrap ()));
                fallback_to! (*cy, Some (RsvgLength::parse ("50%", LengthDir::Vertical).unwrap ()));
                fallback_to! (*r,  Some (RsvgLength::parse ("50%", LengthDir::Both).unwrap ()));

                /* fx and fy fall back to the presentational value of cx and cy */
                fallback_to! (*fx, *cx);
                fallback_to! (*fy, *cy);
            }
        }
    }

    fn resolve_from_fallback (&mut self, fallback: &GradientVariant) {
        match *self {
            GradientVariant::Linear { ref mut x1, ref mut y1, ref mut x2, ref mut y2 } => {
                if let &GradientVariant::Linear { x1: x1f, y1: y1f, x2: x2f, y2: y2f } = fallback {
                    fallback_to! (*x1, x1f);
                    fallback_to! (*y1, y1f);
                    fallback_to! (*x2, x2f);
                    fallback_to! (*y2, y2f);
                }
            },

            GradientVariant::Radial { ref mut cx, ref mut cy, ref mut r, ref mut fx, ref mut fy } => {
                if let &GradientVariant::Radial { cx: cxf, cy: cyf, r: rf, fx: fxf, fy: fyf } = fallback {
                    fallback_to! (*cx, cxf);
                    fallback_to! (*cy, cyf);
                    fallback_to! (*r,  rf);
                    fallback_to! (*fx, fxf);
                    fallback_to! (*fy, fyf);
                }
            }
        }
    }
}

impl Gradient {
    fn new (common: GradientCommon, variant: GradientVariant) -> Gradient {
        Gradient {
            common: common,
            variant: variant
        }
    }

    fn is_resolved (&self) -> bool {
        self.common.is_resolved () && self.variant.is_resolved ()
    }

    fn resolve_from_defaults (&mut self) {
        self.common.resolve_from_defaults ();
        self.variant.resolve_from_defaults ();
    }

    fn resolve_from_fallback (&mut self, fallback: &Gradient) {
        self.common.resolve_from_fallback (&fallback.common);
        self.variant.resolve_from_fallback (&fallback.variant);
    }

    fn add_color_stop (&mut self, offset: f64, rgba: u32) {
        self.common.add_color_stop (offset, rgba);
    }

    fn add_color_stops_to_pattern (&self,
                                   pattern:  &mut cairo::Gradient,
                                   opacity:  u8) {
        let stops = self.common.stops.as_ref ().unwrap ();

        for stop in stops {
            let rgba = stop.rgba;
            pattern.add_color_stop_rgba (stop.offset,
                                         ((rgba >> 24) & 0xff) as f64 / 255.0,
                                         ((rgba >> 16) & 0xff) as f64 / 255.0,
                                         ((rgba >> 8) & 0xff) as f64 / 255.0,
                                         (((rgba >> 0) & 0xff) * opacity as u32) as f64 / 255.0 / 255.0);
        }
    }
}

impl Clone for Gradient {
    fn clone (&self) -> Self {
        Gradient {
            common: self.common.clone (),
            variant: self.variant
        }
    }
}

trait FallbackSource {
    fn get_fallback (&mut self, name: &str) -> Option<Box<Gradient>>;
}

fn resolve_gradient (gradient: &Gradient, fallback_source: &mut FallbackSource) -> Gradient {
    let mut result = gradient.clone ();

    while !result.is_resolved () {
        let mut opt_fallback: Option<Box<Gradient>> = None;

        if let Some (ref fallback_name) = result.common.fallback {
            opt_fallback = fallback_source.get_fallback (&**fallback_name);
        }

        if let Some (fallback_gradient) = opt_fallback {
            result.resolve_from_fallback (&*fallback_gradient);
        } else {
            result.resolve_from_defaults ();
            break;
        }
    }

    result
}

struct NodeFallbackSource {
    draw_ctx: *mut RsvgDrawingCtx,
    acquired_nodes: Vec<*mut RsvgNode>
}

impl NodeFallbackSource {
    fn new (draw_ctx: *mut RsvgDrawingCtx) -> NodeFallbackSource {
        NodeFallbackSource {
            draw_ctx: draw_ctx,
            acquired_nodes: Vec::<*mut RsvgNode>::new ()
        }
    }
}

impl Drop for NodeFallbackSource {
    fn drop (&mut self) {
        while let Some (node) = self.acquired_nodes.pop () {
            drawing_ctx::release_node (self.draw_ctx, node);
        }
    }
}

extern "C" {
    fn rsvg_gradient_node_to_rust_gradient (node: *const RsvgNode) -> *mut Gradient;
}

impl FallbackSource for NodeFallbackSource {
    fn get_fallback (&mut self, name: &str) -> Option<Box<Gradient>> {
        let fallback_node = drawing_ctx::acquire_node (self.draw_ctx, name);

        if fallback_node.is_null () {
            return None;
        }

        self.acquired_nodes.push (fallback_node);

        let raw_fallback_gradient = unsafe { rsvg_gradient_node_to_rust_gradient (fallback_node) };

        if raw_fallback_gradient.is_null () {
            return None;
        }

        let fallback_gradient = unsafe { Box::from_raw (raw_fallback_gradient) };

        return Some (fallback_gradient);
    }
}

fn set_common_on_pattern<P: cairo::Pattern + cairo::Gradient> (gradient: &Gradient,
                                                               draw_ctx: *mut RsvgDrawingCtx,
                                                               pattern:  &mut P,
                                                               bbox:     &RsvgBbox,
                                                               opacity:  u8)
{
    let cr = drawing_ctx::get_cairo_context (draw_ctx);

    let mut affine = gradient.common.affine.unwrap ();

    let units = gradient.common.units.unwrap ();

    if units == PaintServerUnits::ObjectBoundingBox {
        let bbox_matrix = cairo::Matrix::new (bbox.rect.width, 0.0,
                                              0.0, bbox.rect.height,
                                              bbox.rect.x, bbox.rect.y);
        affine = cairo::Matrix::multiply (&affine, &bbox_matrix);
    }

    affine.invert ();
    pattern.set_matrix (affine);
    pattern.set_extend (gradient.common.spread.unwrap ());

    gradient.add_color_stops_to_pattern (pattern, opacity);

    cr.set_source (pattern);
}

fn set_linear_gradient_on_pattern (gradient: &Gradient,
                                   draw_ctx: *mut RsvgDrawingCtx,
                                   bbox:     &RsvgBbox,
                                   opacity:  u8)
{
    if let GradientVariant::Linear { x1, y1, x2, y2 } = gradient.variant {
        let units = gradient.common.units.unwrap ();

        if units == PaintServerUnits::ObjectBoundingBox {
            drawing_ctx::push_view_box (draw_ctx, 1.0, 1.0);
        }

        let mut pattern = cairo::LinearGradient::new (x1.as_ref ().unwrap ().normalize (draw_ctx),
                                                      y1.as_ref ().unwrap ().normalize (draw_ctx),
                                                      x2.as_ref ().unwrap ().normalize (draw_ctx),
                                                      y2.as_ref ().unwrap ().normalize (draw_ctx));

        if units == PaintServerUnits::ObjectBoundingBox {
            drawing_ctx::pop_view_box (draw_ctx);
        }

        set_common_on_pattern (gradient, draw_ctx, &mut pattern, bbox, opacity);
    } else {
        unreachable! ();
    }
}

/* SVG defines radial gradients as being inside a circle (cx, cy, radius).  The
 * gradient projects out from a focus point (fx, fy), which is assumed to be
 * inside the circle, to the edge of the circle.
 *
 * The description of https://www.w3.org/TR/SVG/pservers.html#RadialGradientElement
 * states:
 *
 * If the point defined by ‘fx’ and ‘fy’ lies outside the circle defined by
 * ‘cx’, ‘cy’ and ‘r’, then the user agent shall set the focal point to the
 * intersection of the line from (‘cx’, ‘cy’) to (‘fx’, ‘fy’) with the circle
 * defined by ‘cx’, ‘cy’ and ‘r’.
 *
 * So, let's do that!
 */
fn fix_focus_point (mut fx: f64,
                    mut fy: f64,
                    cx: f64,
                    cy: f64,
                    radius: f64) -> (f64, f64) {
    /* Easy case first: the focus point is inside the circle */

    if (fx - cx) * (fx - cx) + (fy - cy) * (fy - cy) <= radius * radius {
        return (fx, fy);
    }

    /* Hard case: focus point is outside the circle.
     *
     * First, translate everything to the origin.
     */

    fx -= cx;
    fy -= cy;

    /* Find the vector from the origin to (fx, fy) */

    let mut vx = fx;
    let mut vy = fy;

    /* Find the vector's magnitude */

    let mag = (vx * vx + vy * vy).sqrt ();

    /* Normalize the vector to have a magnitude equal to radius; (vx, vy) will now be on the edge of the circle */

    let scale = mag / radius;

    vx /= scale;
    vy /= scale;

    /* Translate back to (cx, cy) and we are done! */

    (vx + cx, vy + cy)
}

fn set_radial_gradient_on_pattern (gradient: &Gradient,
                                   draw_ctx: *mut RsvgDrawingCtx,
                                   bbox:     &RsvgBbox,
                                   opacity:  u8) {
    if let GradientVariant::Radial { cx, cy, r, fx, fy } = gradient.variant {
        let units = gradient.common.units.unwrap ();

        if units == PaintServerUnits::ObjectBoundingBox {
            drawing_ctx::push_view_box (draw_ctx, 1.0, 1.0);
        }

        let n_cx = cx.as_ref ().unwrap ().normalize (draw_ctx);
        let n_cy = cy.as_ref ().unwrap ().normalize (draw_ctx);
        let n_r  =  r.as_ref ().unwrap ().normalize (draw_ctx);
        let n_fx = fx.as_ref ().unwrap ().normalize (draw_ctx);
        let n_fy = fy.as_ref ().unwrap ().normalize (draw_ctx);

        let (new_fx, new_fy) = fix_focus_point (n_fx, n_fy, n_cx, n_cy, n_r);

        let mut pattern = cairo::RadialGradient::new (new_fx, new_fy, 0.0, n_cx, n_cy, n_r);

        if units == PaintServerUnits::ObjectBoundingBox {
            drawing_ctx::pop_view_box (draw_ctx);
        }

        set_common_on_pattern (gradient, draw_ctx, &mut pattern, bbox, opacity);
    } else {
        unreachable! ();
    }
}

fn set_pattern_on_draw_context (gradient: &Gradient,
                                draw_ctx: *mut RsvgDrawingCtx,
                                opacity:  u8,
                                bbox:     &RsvgBbox) {
    assert! (gradient.is_resolved ());

    match gradient.variant {
        GradientVariant::Linear { .. } => {
            set_linear_gradient_on_pattern (gradient, draw_ctx, bbox, opacity);
        }

        GradientVariant::Radial { .. } => {
            set_radial_gradient_on_pattern (gradient, draw_ctx, bbox, opacity);
        }
    }
}

fn paint_server_units_from_gboolean (v: glib_sys::gboolean) -> PaintServerUnits {
    if from_glib (v) {
        PaintServerUnits::ObjectBoundingBox
    } else {
        PaintServerUnits::UserSpaceOnUse
    }
}

/* All the arguments are pointers because they are in fact optional in
 * SVG.  We turn the arguments into Option<foo>: NULL into None, and
 * anything else into a Some().
 */
#[no_mangle]
pub unsafe extern fn gradient_linear_new (x1: *const RsvgLength,
                                          y1: *const RsvgLength,
                                          x2: *const RsvgLength,
                                          y2: *const RsvgLength,
                                          obj_bbox: *const glib_sys::gboolean,
                                          affine: *const cairo::Matrix,
                                          spread: *const cairo::enums::Extend,
                                          fallback_name: *const libc::c_char) -> *mut Gradient {
    let my_units         = { if obj_bbox.is_null ()      { None } else { Some (paint_server_units_from_gboolean (*obj_bbox)) } };
    let my_affine        = { if affine.is_null ()        { None } else { Some (*affine) } };
    let my_spread        = { if spread.is_null ()        { None } else { Some (*spread) } };
    let my_fallback_name = { if fallback_name.is_null () { None } else { Some (String::from_glib_none (fallback_name)) } };

    let my_x1 = { if x1.is_null () { None } else { Some (*x1) } };
    let my_y1 = { if y1.is_null () { None } else { Some (*y1) } };
    let my_x2 = { if x2.is_null () { None } else { Some (*x2) } };
    let my_y2 = { if y2.is_null () { None } else { Some (*y2) } };

    let gradient = Gradient::new (GradientCommon::new (my_units, my_affine, my_spread, my_fallback_name, None),
                                  GradientVariant::Linear { x1: my_x1,
                                                            y1: my_y1,
                                                            x2: my_x2,
                                                            y2: my_y2 });

    let boxed_gradient = Box::new (gradient);

    Box::into_raw (boxed_gradient)
}

#[no_mangle]
pub unsafe extern fn gradient_radial_new (cx: *const RsvgLength,
                                          cy: *const RsvgLength,
                                          r:  *const RsvgLength,
                                          fx: *const RsvgLength,
                                          fy: *const RsvgLength,
                                          obj_bbox: *const glib_sys::gboolean,
                                          affine: *const cairo::Matrix,
                                          spread: *const cairo::enums::Extend,
                                          fallback_name: *const libc::c_char) -> *mut Gradient {
    let my_units         = { if obj_bbox.is_null ()      { None } else { Some (paint_server_units_from_gboolean (*obj_bbox)) } };
    let my_affine        = { if affine.is_null ()        { None } else { Some (*affine) } };
    let my_spread        = { if spread.is_null ()        { None } else { Some (*spread) } };
    let my_fallback_name = { if fallback_name.is_null () { None } else { Some (String::from_glib_none (fallback_name)) } };

    let my_cx = { if cx.is_null () { None } else { Some (*cx) } };
    let my_cy = { if cy.is_null () { None } else { Some (*cy) } };
    let my_r  = { if r.is_null  () { None } else { Some (*r)  } };
    let my_fx = { if fx.is_null () { None } else { Some (*fx) } };
    let my_fy = { if fy.is_null () { None } else { Some (*fy) } };

    let gradient = Gradient::new (GradientCommon::new (my_units, my_affine, my_spread, my_fallback_name, None),
                                  GradientVariant::Radial { cx: my_cx,
                                                            cy: my_cy,
                                                            r:  my_r,
                                                            fx: my_fx,
                                                            fy: my_fy });

    let boxed_gradient = Box::new (gradient);

    Box::into_raw (boxed_gradient)
}

#[no_mangle]
pub unsafe extern fn gradient_destroy (raw_gradient: *mut Gradient) {
    assert! (!raw_gradient.is_null ());

    let _ = Box::from_raw (raw_gradient);
}

#[no_mangle]
pub extern fn gradient_add_color_stops_from_node (raw_gradient: *mut Gradient,
                                                  raw_node:     *const RsvgNode) {
    assert! (!raw_gradient.is_null ());
    assert! (!raw_node.is_null ());

    let gradient: &mut Gradient = unsafe { &mut (*raw_gradient) };
    let node: &RsvgNode = unsafe { & *raw_node };

    for child in &*node.children.borrow () {
        if child.get_type () != NodeType::Stop {
            continue; // just ignore this child; we are only interested in gradient stops
        }

        if child.get_result ().is_err () {
            break; // don't add any more stops
        }

        child.with_impl (|stop: &NodeStop| {
            gradient.add_color_stop (stop.get_offset (), stop.get_rgba ());
        });
    }
}

#[no_mangle]
pub extern fn gradient_resolve_fallbacks_and_set_pattern (raw_gradient: *mut Gradient,
                                                          draw_ctx:     *mut RsvgDrawingCtx,
                                                          opacity:      u8,
                                                          bbox:         RsvgBbox) {
    assert! (!raw_gradient.is_null ());
    let gradient: &mut Gradient = unsafe { &mut (*raw_gradient) };

    let mut fallback_source = NodeFallbackSource::new (draw_ctx);

    let resolved = resolve_gradient (gradient, &mut fallback_source);

    set_pattern_on_draw_context (&resolved,
                                 draw_ctx,
                                 opacity,
                                 &bbox);
}
