use crate::document::AcquiredNodes;
use crate::drawing_ctx::DrawingCtx;
use crate::element::{ElementResult, SetAttributes};
use crate::node::{CascadedValues, Node};
use crate::xml::Attributes;

use super::context::{FilterContext, FilterOutput, FilterResult};
use super::{FilterEffect, FilterError, FilterRender, Primitive};

/// The `feFlood` filter primitive.
pub struct FeFlood {
    base: Primitive,
}

impl Default for FeFlood {
    /// Constructs a new `Flood` with empty properties.
    #[inline]
    fn default() -> FeFlood {
        FeFlood {
            base: Primitive::new(),
        }
    }
}

impl SetAttributes for FeFlood {
    fn set_attributes(&mut self, attrs: &Attributes) -> ElementResult {
        self.base.set_attributes(attrs)
    }
}

impl FilterRender for FeFlood {
    fn render(
        &self,
        node: &Node,
        ctx: &FilterContext,
        _acquired_nodes: &mut AcquiredNodes<'_>,
        draw_ctx: &mut DrawingCtx,
    ) -> Result<FilterResult, FilterError> {
        let bounds = self.base.get_bounds(ctx)?.into_irect(ctx, draw_ctx);

        let cascaded = CascadedValues::new_from_node(node);
        let values = cascaded.get();

        let color = match values.flood_color().0 {
            cssparser::Color::CurrentColor => values.color().0,
            cssparser::Color::RGBA(rgba) => rgba,
        };
        let opacity = values.flood_opacity().0;

        let surface = ctx.source_graphic().flood(bounds, color, opacity)?;

        Ok(FilterResult {
            name: self.base.result.clone(),
            output: FilterOutput { surface, bounds },
        })
    }
}

impl FilterEffect for FeFlood {
    #[inline]
    fn is_affected_by_color_interpolation_filters(&self) -> bool {
        false
    }
}
