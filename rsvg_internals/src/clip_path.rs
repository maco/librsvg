use std::cell::Cell;

use cairo::{self, MatrixTrait};

use crate::attributes::Attribute;
use crate::bbox::BoundingBox;
use crate::coord_units::CoordUnits;
use crate::drawing_ctx::DrawingCtx;
use crate::error::RenderingError;
use crate::node::{NodeResult, NodeTrait, RsvgNode};
use crate::parsers::ParseValue;
use crate::property_bag::PropertyBag;

coord_units!(ClipPathUnits, CoordUnits::UserSpaceOnUse);

pub struct NodeClipPath {
    units: Cell<ClipPathUnits>,
}

impl NodeClipPath {
    pub fn new() -> NodeClipPath {
        NodeClipPath {
            units: Cell::new(ClipPathUnits::default()),
        }
    }

    pub fn get_units(&self) -> ClipPathUnits {
        self.units.get()
    }

    pub fn to_cairo_context(
        &self,
        node: &RsvgNode,
        draw_ctx: &mut DrawingCtx,
        bbox: &BoundingBox,
    ) -> Result<(), RenderingError> {
        let clip_units = self.units.get();

        if clip_units == ClipPathUnits(CoordUnits::ObjectBoundingBox) && bbox.rect.is_none() {
            // The node being clipped is empty / doesn't have a
            // bounding box, so there's nothing to clip!
            return Ok(());
        }

        let cascaded = node.get_cascaded_values();

        draw_ctx.with_saved_matrix(&mut |dc| {
            let cr = dc.get_cairo_context();

            if clip_units == ClipPathUnits(CoordUnits::ObjectBoundingBox) {
                let rect = bbox.rect.as_ref().unwrap();

                cr.transform(cairo::Matrix::new(
                    rect.width,
                    0.0,
                    0.0,
                    rect.height,
                    rect.x,
                    rect.y,
                ))
            }

            // here we don't push a layer because we are clipping
            let res = node.draw_children(&cascaded, dc, true);

            cr.clip();
            res
        })
    }
}

impl NodeTrait for NodeClipPath {
    fn set_atts(&self, _: &RsvgNode, pbag: &PropertyBag<'_>) -> NodeResult {
        for (attr, value) in pbag.iter() {
            match attr {
                Attribute::ClipPathUnits => self.units.set(attr.parse(value)?),
                _ => (),
            }
        }

        Ok(())
    }
}
