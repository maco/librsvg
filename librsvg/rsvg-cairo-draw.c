/* -*- Mode: C; indent-tabs-mode: nil; c-basic-offset: 4 -*- */
/* vim: set sw=4 sts=4 expandtab: */
/*
   rsvg-shapes.c: Draw shapes with cairo

   Copyright (C) 2005 Dom Lachowicz <cinamod@hotmail.com>
   Copyright (C) 2005 Caleb Moore <c.moore@student.unsw.edu.au>
   Copyright (C) 2005 Red Hat, Inc.

   This program is free software; you can redistribute it and/or
   modify it under the terms of the GNU Library General Public License as
   published by the Free Software Foundation; either version 2 of the
   License, or (at your option) any later version.

   This program is distributed in the hope that it will be useful,
   but WITHOUT ANY WARRANTY; without even the implied warranty of
   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
   Library General Public License for more details.

   You should have received a copy of the GNU Library General Public
   License along with this program; if not, write to the
   Free Software Foundation, Inc., 59 Temple Place - Suite 330,
   Boston, MA 02111-1307, USA.

   Authors: Dom Lachowicz <cinamod@hotmail.com>,
            Caleb Moore <c.moore@student.unsw.edu.au>
            Carl Worth <cworth@cworth.org>
*/

#include "config.h"

#include "rsvg-cairo-draw.h"
#include "rsvg-styles.h"
#include "rsvg-filter.h"
#include "rsvg-mask.h"
#include "rsvg-structure.h"

#include <math.h>
#include <string.h>

#include <pango/pangocairo.h>
#ifdef HAVE_PANGO_FT2
#include <pango/pangofc-fontmap.h>
#endif

/* Implemented in rsvg_internals/src/draw.rs */
G_GNUC_INTERNAL
void rsvg_cairo_add_clipping_rect (RsvgDrawingCtx *ctx,
                                   cairo_matrix_t *affine,
                                   double x,
                                   double y,
                                   double w,
                                   double h);

#ifdef HAVE_PANGOFT2
static cairo_font_options_t *
get_font_options_for_testing (void)
{
    cairo_font_options_t *options;

    options = cairo_font_options_create ();
    cairo_font_options_set_antialias (options, CAIRO_ANTIALIAS_GRAY);
    cairo_font_options_set_hint_style (options, CAIRO_HINT_STYLE_FULL);
    cairo_font_options_set_hint_metrics (options, CAIRO_HINT_METRICS_ON);

    return options;
}

static void
set_font_options_for_testing (PangoContext *context)
{
    cairo_font_options_t *font_options;

    font_options = get_font_options_for_testing ();
    pango_cairo_context_set_font_options (context, font_options);
    cairo_font_options_destroy (font_options);
}

static void
create_font_config_for_testing (RsvgDrawingCtx *ctx)
{
    const char *font_paths[] = {
        SRCDIR "/tests/resources/Roboto-Regular.ttf",
        SRCDIR "/tests/resources/Roboto-Italic.ttf",
        SRCDIR "/tests/resources/Roboto-Bold.ttf",
        SRCDIR "/tests/resources/Roboto-BoldItalic.ttf",
    };

    int i;

    if (ctx->font_config_for_testing != NULL)
        return;

    ctx->font_config_for_testing = FcConfigCreate ();

    for (i = 0; i < G_N_ELEMENTS(font_paths); i++) {
        if (!FcConfigAppFontAddFile (ctx->font_config_for_testing, (const FcChar8 *) font_paths[i])) {
            g_error ("Could not load font file \"%s\" for tests; aborting", font_paths[i]);
        }
    }
}

static PangoFontMap *
get_font_map_for_testing (RsvgDrawingCtx *ctx)
{
    create_font_config_for_testing (ctx);

    if (ctx->font_map_for_testing == NULL) {
        ctx->font_map_for_testing = pango_cairo_font_map_new_for_font_type (CAIRO_FONT_TYPE_FT);
        pango_fc_font_map_set_config (PANGO_FC_FONT_MAP (ctx->font_map_for_testing),
                                      ctx->font_config_for_testing);
    }

    return ctx->font_map_for_testing;
}
#endif

PangoContext *
rsvg_cairo_get_pango_context (RsvgDrawingCtx * ctx)
{
    PangoFontMap *fontmap;
    PangoContext *context;
    double dpi_y;

#ifdef HAVE_PANGOFT2
    if (ctx->is_testing) {
        fontmap = get_font_map_for_testing (ctx);
    } else {
#endif
        fontmap = pango_cairo_font_map_get_default ();
#ifdef HAVE_PANGOFT2
    }
#endif

    context = pango_font_map_create_context (fontmap);
    pango_cairo_update_context (ctx->cr, context);

    rsvg_drawing_ctx_get_dpi (ctx, NULL, &dpi_y);
    pango_cairo_context_set_resolution (context, dpi_y);

#ifdef HAVE_PANGOFT2
    if (ctx->is_testing) {
        set_font_options_for_testing (context);
    }
#endif

    return context;
}

cairo_t *
rsvg_cairo_get_cairo_context (RsvgDrawingCtx *ctx)
{
    return ctx->cr;
}

/* FIXME: Usage of this function is more less a hack.  Some code does this:
 *
 *   save_cr = rsvg_cairo_get_cairo_context (ctx);
 *
 *   some_surface = create_surface ();
 *
 *   cr = cairo_create (some_surface);
 *
 *   rsvg_cairo_set_cairo_context (ctx, cr);
 *
 *   ... draw with ctx but to that temporary surface
 *
 *   rsvg_cairo_set_cairo_context (ctx, save_cr);
 *
 * It would be better to have an explicit push/pop for the cairo_t, or
 * pushing a temporary surface, or something that does not involve
 * monkeypatching the cr directly.
 */
void
rsvg_cairo_set_cairo_context (RsvgDrawingCtx *ctx, cairo_t *cr)
{
    ctx->cr = cr;
}

gboolean
rsvg_cairo_is_cairo_context_nested (RsvgDrawingCtx *ctx, cairo_t *cr)
{
    return cr != ctx->initial_cr;
}

static void
rsvg_cairo_generate_mask (cairo_t * cr, RsvgNode *mask, RsvgDrawingCtx *ctx)
{
    cairo_surface_t *surface;
    cairo_t *mask_cr, *save_cr;
    RsvgState *state;
    guint8 opacity;
    guint8 *pixels;
    guint32 width = ctx->width;
    guint32 height = ctx->height;
    guint32 rowstride, row, i;
    cairo_matrix_t affinesave;
    RsvgLength mask_x, mask_y, mask_w, mask_h;
    double sx, sy, sw, sh;
    RsvgCoordUnits mask_units;
    RsvgCoordUnits content_units;
    cairo_matrix_t affine;
    double offset_x = 0, offset_y = 0;

    g_assert (rsvg_node_get_type (mask) == RSVG_NODE_TYPE_MASK);

    surface = cairo_image_surface_create (CAIRO_FORMAT_ARGB32, width, height);
    if (cairo_surface_status (surface) != CAIRO_STATUS_SUCCESS) {
        cairo_surface_destroy (surface);
        return;
    }

    pixels = cairo_image_surface_get_data (surface);
    rowstride = cairo_image_surface_get_stride (surface);

    mask_units    = rsvg_node_mask_get_units (mask);
    content_units = rsvg_node_mask_get_content_units (mask);

    if (mask_units == objectBoundingBox)
        rsvg_drawing_ctx_push_view_box (ctx, 1, 1);

    mask_x = rsvg_node_mask_get_x (mask);
    mask_y = rsvg_node_mask_get_y (mask);
    mask_w = rsvg_node_mask_get_width (mask);
    mask_h = rsvg_node_mask_get_height (mask);

    sx = rsvg_length_normalize (&mask_x, ctx);
    sy = rsvg_length_normalize (&mask_y, ctx);
    sw = rsvg_length_normalize (&mask_w, ctx);
    sh = rsvg_length_normalize (&mask_h, ctx);

    if (mask_units == objectBoundingBox)
        rsvg_drawing_ctx_pop_view_box (ctx);

    mask_cr = cairo_create (surface);
    save_cr = ctx->cr;
    ctx->cr = mask_cr;

    state = rsvg_drawing_ctx_get_current_state (ctx);
    affine = rsvg_state_get_affine (state);

    if (mask_units == objectBoundingBox)
        rsvg_cairo_add_clipping_rect (ctx,
                                      &affine,
                                      sx * ctx->bbox.rect.width + ctx->bbox.rect.x,
                                      sy * ctx->bbox.rect.height + ctx->bbox.rect.y,
                                      sw * ctx->bbox.rect.width,
                                      sh * ctx->bbox.rect.height);
    else
        rsvg_cairo_add_clipping_rect (ctx, &affine, sx, sy, sw, sh);

    /* Horribly dirty hack to have the bbox premultiplied to everything */
    if (content_units == objectBoundingBox) {
        cairo_matrix_t bbtransform;
        RsvgState *mask_state;

        cairo_matrix_init (&bbtransform,
                           ctx->bbox.rect.width,
                           0,
                           0,
                           ctx->bbox.rect.height,
                           ctx->bbox.rect.x,
                           ctx->bbox.rect.y);

        mask_state = rsvg_node_get_state (mask);

        affinesave = rsvg_state_get_affine (mask_state);
        cairo_matrix_multiply (&bbtransform, &bbtransform, &affinesave);
        rsvg_state_set_affine (mask_state, bbtransform);
        rsvg_drawing_ctx_push_view_box (ctx, 1, 1);
    }

    rsvg_drawing_ctx_state_push (ctx);
    rsvg_node_draw_children (mask, ctx, 0, FALSE);
    rsvg_drawing_ctx_state_pop (ctx);

    if (content_units == objectBoundingBox) {
        RsvgState *mask_state;

        rsvg_drawing_ctx_pop_view_box (ctx);

        mask_state = rsvg_node_get_state (mask);
        rsvg_state_set_affine (mask_state, affinesave);
    }

    ctx->cr = save_cr;

    opacity = rsvg_state_get_opacity (state);

    for (row = 0; row < height; row++) {
        guint8 *row_data = (pixels + (row * rowstride));
        for (i = 0; i < width; i++) {
            guint32 *pixel = (guint32 *) row_data + i;
            /*
             *  Assuming, the pixel is linear RGB (not sRGB)
             *  y = luminance
             *  Y = 0.2126 R + 0.7152 G + 0.0722 B
             *  1.0 opacity = 255
             *
             *  When Y = 1.0, pixel for mask should be 0xFFFFFFFF
             *  	(you get 1.0 luminance from 255 from R, G and B)
             *
             *	r_mult = 0xFFFFFFFF / (255.0 * 255.0) * .2126 = 14042.45  ~= 14042
             *	g_mult = 0xFFFFFFFF / (255.0 * 255.0) * .7152 = 47239.69  ~= 47240
             *	b_mult = 0xFFFFFFFF / (255.0 * 255.0) * .0722 =  4768.88  ~= 4769
             *
             * 	This allows for the following expected behaviour:
             *  (we only care about the most sig byte)
             *	if pixel = 0x00FFFFFF, pixel' = 0xFF......
             *	if pixel = 0x00020202, pixel' = 0x02......
             *	if pixel = 0x00000000, pixel' = 0x00......
             */
            *pixel = ((((*pixel & 0x00ff0000) >> 16) * 14042 +
                       ((*pixel & 0x0000ff00) >>  8) * 47240 +
                       ((*pixel & 0x000000ff)      ) * 4769    ) * opacity);
        }
    }

    cairo_destroy (mask_cr);

    if (cr == ctx->initial_cr) {
        rsvg_drawing_ctx_get_offset (ctx, &offset_x, &offset_y);
    }

    cairo_identity_matrix (cr);
    cairo_mask_surface (cr, surface, offset_x, offset_y);
    cairo_surface_destroy (surface);
}

static void
rsvg_cairo_clip (RsvgDrawingCtx *ctx, RsvgNode *node_clip_path, RsvgBbox *bbox)
{
    cairo_matrix_t affinesave;
    RsvgState *clip_path_state;
    RsvgCoordUnits clip_units;
    GList *orig_cr_stack;
    GList *orig_surfaces_stack;
    RsvgBbox orig_bbox;
    RsvgBbox orig_ink_bbox;

    g_assert (rsvg_node_get_type (node_clip_path) == RSVG_NODE_TYPE_CLIP_PATH);
    clip_units = rsvg_node_clip_path_get_units (node_clip_path);

    clip_path_state = rsvg_node_get_state (node_clip_path);

    /* Horribly dirty hack to have the bbox premultiplied to everything */
    if (clip_units == objectBoundingBox) {
        cairo_matrix_t bbtransform;
        cairo_matrix_init (&bbtransform,
                           bbox->rect.width,
                           0,
                           0,
                           bbox->rect.height,
                           bbox->rect.x,
                           bbox->rect.y);
        affinesave = rsvg_state_get_affine (clip_path_state);
        cairo_matrix_multiply (&bbtransform, &bbtransform, &affinesave);
        rsvg_state_set_affine (clip_path_state, bbtransform);
    }

    orig_cr_stack = ctx->cr_stack;
    orig_surfaces_stack = ctx->surfaces_stack;

    orig_bbox = ctx->bbox;
    orig_ink_bbox = ctx->ink_bbox;

    rsvg_drawing_ctx_state_push (ctx);
    rsvg_node_draw_children (node_clip_path, ctx, 0, TRUE);
    rsvg_drawing_ctx_state_pop (ctx);

    if (clip_units == objectBoundingBox) {
        rsvg_state_set_affine (clip_path_state, affinesave);
    }

    g_assert (ctx->cr_stack == orig_cr_stack);
    g_assert (ctx->surfaces_stack == orig_surfaces_stack);

    /* FIXME: this is an EPIC HACK to keep the clipping context from
     * accumulating bounding boxes.  We'll remove this later, when we
     * are able to extract bounding boxes from outside the
     * general drawing loop.
     */
    ctx->bbox = orig_bbox;
    ctx->ink_bbox = orig_ink_bbox;

    cairo_clip (ctx->cr);
}

static void
push_bounding_box (RsvgDrawingCtx *ctx)
{
    RsvgState *state;
    cairo_matrix_t affine;
    RsvgBbox *bbox, *ink_bbox;

    state = rsvg_drawing_ctx_get_current_state (ctx);

    bbox = g_new0 (RsvgBbox, 1);
    *bbox = ctx->bbox;
    ctx->bb_stack = g_list_prepend (ctx->bb_stack, bbox);

    ink_bbox = g_new0 (RsvgBbox, 1);
    *ink_bbox = ctx->ink_bbox;
    ctx->ink_bb_stack = g_list_prepend (ctx->ink_bb_stack, ink_bbox);

    affine = rsvg_state_get_affine (state);
    rsvg_bbox_init (&ctx->bbox, &affine);
    rsvg_bbox_init (&ctx->ink_bbox, &affine);
}

static void
rsvg_cairo_push_render_stack (RsvgDrawingCtx * ctx)
{
    RsvgState *state;
    char *clip_path;
    char *filter;
    char *mask;
    guint8 opacity;
    cairo_operator_t comp_op;
    RsvgEnableBackgroundType enable_background;
    cairo_surface_t *surface;
    cairo_t *child_cr;
    gboolean lateclip = FALSE;

    state = rsvg_drawing_ctx_get_current_state (ctx);
    clip_path = rsvg_state_get_clip_path (state);
    filter = rsvg_state_get_filter (state);
    mask = rsvg_state_get_mask (state);
    opacity = rsvg_state_get_opacity (state);
    comp_op = rsvg_state_get_comp_op (state);
    enable_background = rsvg_state_get_enable_background (state);

    if (clip_path) {
        RsvgNode *node;
        node = rsvg_drawing_ctx_acquire_node_of_type (ctx, clip_path, RSVG_NODE_TYPE_CLIP_PATH);
        if (node) {
            switch (rsvg_node_clip_path_get_units (node)) {
            case userSpaceOnUse:
                rsvg_cairo_clip (ctx, node, NULL);
                break;
            case objectBoundingBox:
                lateclip = TRUE;
                break;

            default:
                g_assert_not_reached ();
                break;
            }

            rsvg_drawing_ctx_release_node (ctx, node);
        }

        g_free (clip_path);
    }

    if (opacity == 0xFF
        && !filter && !mask && !lateclip && (comp_op == CAIRO_OPERATOR_OVER)
        && (enable_background == RSVG_ENABLE_BACKGROUND_ACCUMULATE))
        return;

    g_free (mask);

    if (!filter) {
        surface = cairo_surface_create_similar (cairo_get_target (ctx->cr),
                                                CAIRO_CONTENT_COLOR_ALPHA,
                                                ctx->width, ctx->height);
    } else {
        surface = cairo_image_surface_create (CAIRO_FORMAT_ARGB32,
                                              ctx->width, ctx->height);

        /* The surface reference is owned by the child_cr created below and put on the cr_stack! */
        ctx->surfaces_stack = g_list_prepend (ctx->surfaces_stack, surface);

        g_free (filter);
    }

#if 0
    if (cairo_surface_status (surface) != CAIRO_STATUS_SUCCESS) {
        cairo_surface_destroy (surface);
        return;
    }
#endif

    child_cr = cairo_create (surface);
    cairo_surface_destroy (surface);

    ctx->cr_stack = g_list_prepend (ctx->cr_stack, ctx->cr);
    ctx->cr = child_cr;

    push_bounding_box (ctx);
}

void
rsvg_cairo_push_discrete_layer (RsvgDrawingCtx * ctx, gboolean clipping)
{
    if (!clipping) {
        cairo_save (ctx->cr);
        rsvg_cairo_push_render_stack (ctx);
    }
}

static void
pop_bounding_box (RsvgDrawingCtx *ctx)
{
    rsvg_bbox_insert ((RsvgBbox *) ctx->bb_stack->data, &ctx->bbox);
    rsvg_bbox_insert ((RsvgBbox *) ctx->ink_bb_stack->data, &ctx->ink_bbox);

    ctx->bbox = *((RsvgBbox *) ctx->bb_stack->data);
    ctx->ink_bbox = *((RsvgBbox *) ctx->ink_bb_stack->data);

    g_free (ctx->bb_stack->data);
    g_free (ctx->ink_bb_stack->data);

    ctx->bb_stack = g_list_delete_link (ctx->bb_stack, ctx->bb_stack);
    ctx->ink_bb_stack = g_list_delete_link (ctx->ink_bb_stack, ctx->ink_bb_stack);
}

static void
rsvg_cairo_pop_render_stack (RsvgDrawingCtx * ctx)
{
    RsvgState *state;
    char *clip_path;
    char *filter;
    char *mask;
    guint8 opacity;
    cairo_operator_t comp_op;
    RsvgEnableBackgroundType enable_background;
    cairo_t *child_cr = ctx->cr;
    RsvgNode *lateclip = NULL;
    cairo_surface_t *surface = NULL;
    gboolean needs_destroy = FALSE;
    double offset_x = 0, offset_y = 0;

    state = rsvg_drawing_ctx_get_current_state (ctx);
    clip_path = rsvg_state_get_clip_path (state);
    filter = rsvg_state_get_filter (state);
    mask = rsvg_state_get_mask (state);
    opacity = rsvg_state_get_opacity (state);
    comp_op = rsvg_state_get_comp_op (state);
    enable_background = rsvg_state_get_enable_background (state);

    if (clip_path) {
        RsvgNode *node;

        node = rsvg_drawing_ctx_acquire_node_of_type (ctx, clip_path, RSVG_NODE_TYPE_CLIP_PATH);
        if (node) {
            if (rsvg_node_clip_path_get_units (node) == objectBoundingBox) {
                lateclip = node;
            } else {
                rsvg_drawing_ctx_release_node (ctx, node);
            }
        }

        g_free (clip_path);
    }

    if (opacity == 0xFF
        && !filter && !mask && !lateclip && (comp_op == CAIRO_OPERATOR_OVER)
        && (enable_background == RSVG_ENABLE_BACKGROUND_ACCUMULATE))
        return;

    surface = cairo_get_target (child_cr);

    if (filter) {
        RsvgNode *node;
        cairo_surface_t *output;

        output = ctx->surfaces_stack->data;
        ctx->surfaces_stack = g_list_delete_link (ctx->surfaces_stack, ctx->surfaces_stack);

        node = rsvg_drawing_ctx_acquire_node_of_type (ctx, filter, RSVG_NODE_TYPE_FILTER);
        if (node) {
            needs_destroy = TRUE;
            surface = rsvg_filter_render (node, output, ctx, "2103");
            rsvg_drawing_ctx_release_node (ctx, node);

            /* Don't destroy the output surface, it's owned by child_cr */
        }

        g_free (filter);
    }

    ctx->cr = (cairo_t *) ctx->cr_stack->data;
    ctx->cr_stack = g_list_delete_link (ctx->cr_stack, ctx->cr_stack);

    if (ctx->cr == ctx->initial_cr) {
        rsvg_drawing_ctx_get_offset (ctx, &offset_x, &offset_y);
    }

    cairo_identity_matrix (ctx->cr);
    cairo_set_source_surface (ctx->cr, surface, offset_x, offset_y);

    if (lateclip) {
        rsvg_cairo_clip (ctx, lateclip, &ctx->bbox);
        rsvg_drawing_ctx_release_node (ctx, lateclip);
    }

    cairo_set_operator (ctx->cr, comp_op);

    if (mask) {
        RsvgNode *node;

        node = rsvg_drawing_ctx_acquire_node_of_type (ctx, mask, RSVG_NODE_TYPE_MASK);
        if (node) {
            rsvg_cairo_generate_mask (ctx->cr, node, ctx);
            rsvg_drawing_ctx_release_node (ctx, node);
        }

        g_free (mask);
    } else if (opacity != 0xFF)
        cairo_paint_with_alpha (ctx->cr, (double) opacity / 255.0);
    else
        cairo_paint (ctx->cr);

    cairo_destroy (child_cr);

    pop_bounding_box (ctx);

    if (needs_destroy) {
        cairo_surface_destroy (surface);
    }
}

void
rsvg_cairo_pop_discrete_layer (RsvgDrawingCtx * ctx, gboolean clipping)
{
    if (!clipping) {
        rsvg_cairo_pop_render_stack (ctx);
        cairo_restore (ctx->cr);
    }
}

cairo_surface_t *
rsvg_cairo_get_surface_of_node (RsvgDrawingCtx *ctx,
                                RsvgNode *drawable,
                                double width,
                                double height)
{
    cairo_surface_t *surface;
    cairo_t *cr;
    cairo_t *save_cr = ctx->cr;
    cairo_t *save_initial_cr = ctx->initial_cr;
    double save_x = ctx->offset_x;
    double save_y = ctx->offset_y;
    double save_w = ctx->width;
    double save_h = ctx->height;

    surface = cairo_image_surface_create (CAIRO_FORMAT_ARGB32, width, height);
    if (cairo_surface_status (surface) != CAIRO_STATUS_SUCCESS) {
        cairo_surface_destroy (surface);
        return NULL;
    }

    ctx->cr = cairo_create (surface);
    ctx->initial_cr = ctx->cr;
    ctx->offset_x = 0;
    ctx->offset_y = 0;
    ctx->width = width;
    ctx->height = height;

    rsvg_drawing_ctx_draw_node_from_stack (ctx, drawable, 0, FALSE);

    cairo_destroy (ctx->cr);
    ctx->cr = save_cr;
    ctx->initial_cr = save_initial_cr;
    ctx->offset_x = save_x;
    ctx->offset_y = save_y;
    ctx->width = save_w;
    ctx->height = save_h;

    return surface;
}

cairo_surface_t *
rsvg_cairo_surface_from_pixbuf (const GdkPixbuf *pixbuf)
{
    gint width, height, gdk_rowstride, n_channels, cairo_rowstride;
    guchar *gdk_pixels, *cairo_pixels;
    cairo_format_t format;
    cairo_surface_t *surface;
    int j;

    if (pixbuf == NULL)
        return NULL;

    width = gdk_pixbuf_get_width (pixbuf);
    height = gdk_pixbuf_get_height (pixbuf);
    gdk_pixels = gdk_pixbuf_get_pixels (pixbuf);
    gdk_rowstride = gdk_pixbuf_get_rowstride (pixbuf);
    n_channels = gdk_pixbuf_get_n_channels (pixbuf);

    if (n_channels == 3)
        format = CAIRO_FORMAT_RGB24;
    else
        format = CAIRO_FORMAT_ARGB32;

    surface = cairo_image_surface_create (format, width, height);
    if (cairo_surface_status (surface) != CAIRO_STATUS_SUCCESS) {
        cairo_surface_destroy (surface);
        return NULL;
    }

    cairo_pixels = cairo_image_surface_get_data (surface);
    cairo_rowstride = cairo_image_surface_get_stride (surface);

    if (n_channels == 3) {
        for (j = height; j; j--) {
            guchar *p = gdk_pixels;
            guchar *q = cairo_pixels;
            guchar *end = p + 3 * width;

            while (p < end) {
#if G_BYTE_ORDER == G_LITTLE_ENDIAN
                q[0] = p[2];
                q[1] = p[1];
                q[2] = p[0];
#else
                q[1] = p[0];
                q[2] = p[1];
                q[3] = p[2];
#endif
                p += 3;
                q += 4;
            }

            gdk_pixels += gdk_rowstride;
            cairo_pixels += cairo_rowstride;
        }
    } else {
        for (j = height; j; j--) {
            guchar *p = gdk_pixels;
            guchar *q = cairo_pixels;
            guchar *end = p + 4 * width;
            guint t1, t2, t3;

#define MULT(d,c,a,t) G_STMT_START { t = c * a + 0x7f; d = ((t >> 8) + t) >> 8; } G_STMT_END

            while (p < end) {
#if G_BYTE_ORDER == G_LITTLE_ENDIAN
                MULT (q[0], p[2], p[3], t1);
                MULT (q[1], p[1], p[3], t2);
                MULT (q[2], p[0], p[3], t3);
                q[3] = p[3];
#else
                q[0] = p[3];
                MULT (q[1], p[0], p[3], t1);
                MULT (q[2], p[1], p[3], t2);
                MULT (q[3], p[2], p[3], t3);
#endif

                p += 4;
                q += 4;
            }

#undef MULT
            gdk_pixels += gdk_rowstride;
            cairo_pixels += cairo_rowstride;
        }
    }

    cairo_surface_mark_dirty (surface);
    return surface;
}

/* Copied from gtk+/gdk/gdkpixbuf-drawable.c, LGPL 2+.
 *
 * Copyright (C) 1999 Michael Zucchi
 *
 * Authors: Michael Zucchi <zucchi@zedzone.mmc.com.au>
 *          Cody Russell <bratsche@dfw.net>
 *          Federico Mena-Quintero <federico@gimp.org>
 */

static void
convert_alpha (guchar *dest_data,
               int     dest_stride,
               guchar *src_data,
               int     src_stride,
               int     src_x,
               int     src_y,
               int     width,
               int     height)
{
    int x, y;

    src_data += src_stride * src_y + src_x * 4;

    for (y = 0; y < height; y++) {
        guint32 *src = (guint32 *) src_data;

        for (x = 0; x < width; x++) {
          guint alpha = src[x] >> 24;

          if (alpha == 0) {
              dest_data[x * 4 + 0] = 0;
              dest_data[x * 4 + 1] = 0;
              dest_data[x * 4 + 2] = 0;
          } else {
              dest_data[x * 4 + 0] = (((src[x] & 0xff0000) >> 16) * 255 + alpha / 2) / alpha;
              dest_data[x * 4 + 1] = (((src[x] & 0x00ff00) >>  8) * 255 + alpha / 2) / alpha;
              dest_data[x * 4 + 2] = (((src[x] & 0x0000ff) >>  0) * 255 + alpha / 2) / alpha;
          }
          dest_data[x * 4 + 3] = alpha;
      }

      src_data += src_stride;
      dest_data += dest_stride;
    }
}

static void
convert_no_alpha (guchar *dest_data,
                  int     dest_stride,
                  guchar *src_data,
                  int     src_stride,
                  int     src_x,
                  int     src_y,
                  int     width,
                  int     height)
{
    int x, y;

    src_data += src_stride * src_y + src_x * 4;

    for (y = 0; y < height; y++) {
        guint32 *src = (guint32 *) src_data;

        for (x = 0; x < width; x++) {
            dest_data[x * 3 + 0] = src[x] >> 16;
            dest_data[x * 3 + 1] = src[x] >>  8;
            dest_data[x * 3 + 2] = src[x];
        }

        src_data += src_stride;
        dest_data += dest_stride;
    }
}

GdkPixbuf *
rsvg_cairo_surface_to_pixbuf (cairo_surface_t *surface)
{
    cairo_content_t content;
    GdkPixbuf *dest;
    int width, height;

    /* General sanity checks */
    g_assert (cairo_surface_get_type (surface) == CAIRO_SURFACE_TYPE_IMAGE);

    width = cairo_image_surface_get_width (surface);
    height = cairo_image_surface_get_height (surface);
    if (width == 0 || height == 0)
        return NULL;

    content = cairo_surface_get_content (surface) | CAIRO_CONTENT_COLOR;
    dest = gdk_pixbuf_new (GDK_COLORSPACE_RGB,
                          !!(content & CAIRO_CONTENT_ALPHA),
                          8,
                          width, height);

    if (gdk_pixbuf_get_has_alpha (dest))
      convert_alpha (gdk_pixbuf_get_pixels (dest),
                    gdk_pixbuf_get_rowstride (dest),
                    cairo_image_surface_get_data (surface),
                    cairo_image_surface_get_stride (surface),
                    0, 0,
                    width, height);
    else
      convert_no_alpha (gdk_pixbuf_get_pixels (dest),
                        gdk_pixbuf_get_rowstride (dest),
                        cairo_image_surface_get_data (surface),
                        cairo_image_surface_get_stride (surface),
                        0, 0,
                        width, height);

    return dest;
}

static void
rsvg_cairo_transformed_image_bounding_box (cairo_matrix_t *affine,
                                           double width, double height,
                                           double *x0, double *y0, double *x1, double *y1)
{
    double x00 = 0, x01 = 0, x10 = width, x11 = width;
    double y00 = 0, y01 = height, y10 = 0, y11 = height;
    double t;

    /* transform the four corners of the image */
    cairo_matrix_transform_point (affine, &x00, &y00);
    cairo_matrix_transform_point (affine, &x01, &y01);
    cairo_matrix_transform_point (affine, &x10, &y10);
    cairo_matrix_transform_point (affine, &x11, &y11);

    /* find minimum and maximum coordinates */
    t = x00  < x01 ? x00  : x01;
    t = t < x10 ? t : x10;
    *x0 = floor (t < x11 ? t : x11);

    t = y00  < y01 ? y00  : y01;
    t = t < y10 ? t : y10;
    *y0 = floor (t < y11 ? t : y11);

    t = x00  > x01 ? x00  : x01;
    t = t > x10 ? t : x10;
    *x1 = ceil (t > x11 ? t : x11);

    t = y00  > y01 ? y00  : y01;
    t = t > y10 ? t : y10;
    *y1 = ceil (t > y11 ? t : y11);
}

RsvgDrawingCtx *
rsvg_drawing_ctx_new (cairo_t *cr, RsvgHandle *handle)
{
    RsvgDimensionData data;
    RsvgDrawingCtx *draw;
    RsvgState *state;
    cairo_matrix_t affine;
    cairo_matrix_t state_affine;
    double bbx0, bby0, bbx1, bby1;

    rsvg_handle_get_dimensions (handle, &data);
    if (data.width == 0 || data.height == 0)
        return NULL;

    draw = g_new0 (RsvgDrawingCtx, 1);

    cairo_get_matrix (cr, &affine);

    /* find bounding box of image as transformed by the current cairo context
     * The size of this bounding box determines the size of the intermediate
     * surfaces allocated during drawing. */
    rsvg_cairo_transformed_image_bounding_box (&affine,
                                               data.width, data.height,
                                               &bbx0, &bby0, &bbx1, &bby1);

    draw->initial_cr = cr;
    draw->cr = cr;
    draw->cr_stack = NULL;
    draw->surfaces_stack = NULL;

    draw->offset_x = bbx0;
    draw->offset_y = bby0;
    draw->width = bbx1 - bbx0;
    draw->height = bby1 - bby0;

    draw->state = NULL;

    draw->defs = handle->priv->defs;
    draw->dpi_x = handle->priv->dpi_x;
    draw->dpi_y = handle->priv->dpi_y;
    draw->vb.rect.width = data.em;
    draw->vb.rect.height = data.ex;
    draw->vb_stack = NULL;
    draw->drawsub_stack = NULL;
    draw->acquired_nodes = NULL;
    draw->is_testing = handle->priv->is_testing;

    rsvg_drawing_ctx_state_push (draw);
    state = rsvg_drawing_ctx_get_current_state (draw);

    state_affine = rsvg_state_get_affine (state);

    /* apply cairo transformation to our affine transform */
    cairo_matrix_multiply (&state_affine, &affine, &state_affine);

    /* scale according to size set by size_func callback */
    cairo_matrix_init_scale (&affine, data.width / data.em, data.height / data.ex);
    cairo_matrix_multiply (&state_affine, &affine, &state_affine);

    /* adjust transform so that the corner of the bounding box above is
     * at (0,0) - we compensate for this in _set_rsvg_affine() in
     * rsvg-cairo-render.c and a few other places */
    state_affine.x0 -= draw->offset_x;
    state_affine.y0 -= draw->offset_y;

    rsvg_bbox_init (&draw->bbox, &state_affine);
    rsvg_bbox_init (&draw->ink_bbox, &state_affine);

    rsvg_state_set_affine (state, state_affine);

#ifdef HAVE_PANGOFT2
    draw->font_config_for_testing = NULL;
    draw->font_map_for_testing = NULL;
#endif

    return draw;
}

void
rsvg_drawing_ctx_free (RsvgDrawingCtx *ctx)
{
    g_assert (ctx->cr_stack == NULL);
    g_assert (ctx->surfaces_stack == NULL);

    g_assert(ctx->state);
    g_assert(rsvg_state_parent(ctx->state) == NULL);
    rsvg_state_free (ctx->state);

	g_slist_free_full (ctx->drawsub_stack, (GDestroyNotify) rsvg_node_unref);

    g_warn_if_fail (ctx->acquired_nodes == NULL);
    g_slist_free (ctx->acquired_nodes);

    g_assert (ctx->bb_stack == NULL);
    g_assert (ctx->ink_bb_stack == NULL);

#ifdef HAVE_PANGOFT2
    if (ctx->font_config_for_testing) {
        FcConfigDestroy (ctx->font_config_for_testing);
        ctx->font_config_for_testing = NULL;
    }

    if (ctx->font_map_for_testing) {
        g_object_unref (ctx->font_map_for_testing);
        ctx->font_map_for_testing = NULL;
    }
#endif

    g_free (ctx);
}

RsvgState *
rsvg_drawing_ctx_get_current_state (RsvgDrawingCtx *ctx)
{
    return ctx->state;
}

void
rsvg_drawing_ctx_set_current_state (RsvgDrawingCtx *ctx, RsvgState *state)
{
    ctx->state = state;
}

/*
 * rsvg_drawing_ctx_acquire_node:
 * @ctx: The drawing context in use
 * @url: The IRI to lookup, or %NULL
 *
 * Use this function when looking up urls to other nodes. This
 * function does proper recursion checking and thereby avoids
 * infinite loops.
 *
 * Nodes acquired by this function must be released using
 * rsvg_drawing_ctx_release_node() in reverse acquiring order.
 *
 * Note that if you acquire a node, you have to release it before trying to
 * acquire it again.  If you acquire a node "#foo" and don't release it before
 * trying to acquire "foo" again, you will obtain a %NULL the second time.
 *
 * Returns: The node referenced by @url; or %NULL if the @url
 *          is %NULL or it does not reference a node.
 */
RsvgNode *
rsvg_drawing_ctx_acquire_node (RsvgDrawingCtx *ctx, const char *url)
{
  RsvgNode *node;

  if (url == NULL)
      return NULL;

  node = rsvg_defs_lookup (ctx->defs, url);
  if (node == NULL)
    return NULL;

  if (g_slist_find (ctx->acquired_nodes, node))
    return NULL;

  ctx->acquired_nodes = g_slist_prepend (ctx->acquired_nodes, node);

  return node;
}

/**
 * rsvg_drawing_ctx_acquire_node_of_type:
 * @ctx: The drawing context in use
 * @url: The IRI to lookup
 * @type: Type which the node must have
 *
 * Use this function when looking up urls to other nodes, and when you expect
 * the node to be of a particular type. This function does proper recursion
 * checking and thereby avoids infinite loops.
 *
 * Malformed SVGs, for example, may reference a marker by its IRI, but
 * the object referenced by the IRI is not a marker.
 *
 * Nodes acquired by this function must be released using
 * rsvg_drawing_ctx_release_node() in reverse acquiring order.
 *
 * Note that if you acquire a node, you have to release it before trying to
 * acquire it again.  If you acquire a node "#foo" and don't release it before
 * trying to acquire "foo" again, you will obtain a %NULL the second time.
 *
 * Returns: The node referenced by @url or %NULL if the @url
 *          does not reference a node.  Also returns %NULL if
 *          the node referenced by @url is not of the specified @type.
 */
RsvgNode *
rsvg_drawing_ctx_acquire_node_of_type (RsvgDrawingCtx *ctx, const char *url, RsvgNodeType type)
{
    RsvgNode *node;

    node = rsvg_drawing_ctx_acquire_node (ctx, url);
    if (node == NULL || rsvg_node_get_type (node) != type) {
        rsvg_drawing_ctx_release_node (ctx, node);
        return NULL;
    }

    return node;
}

/*
 * rsvg_drawing_ctx_release_node:
 * @ctx: The drawing context the node was acquired from
 * @node: Node to release
 *
 * Releases a node previously acquired via rsvg_drawing_ctx_acquire_node() or
 * rsvg_drawing_ctx_acquire_node_of_type().
 *
 * if @node is %NULL, this function does nothing.
 */
void
rsvg_drawing_ctx_release_node (RsvgDrawingCtx *ctx, RsvgNode *node)
{
  if (node == NULL)
    return;

  g_return_if_fail (ctx->acquired_nodes != NULL);
  g_return_if_fail (ctx->acquired_nodes->data == node);

  ctx->acquired_nodes = g_slist_remove (ctx->acquired_nodes, node);
}

void
rsvg_drawing_ctx_add_node_and_ancestors_to_stack (RsvgDrawingCtx *draw_ctx, RsvgNode *node)
{
    if (node) {
        node = rsvg_node_ref (node);

        while (node != NULL) {
            draw_ctx->drawsub_stack = g_slist_prepend (draw_ctx->drawsub_stack, node);
            node = rsvg_node_get_parent (node);
        }
    }
}

void
rsvg_drawing_ctx_draw_node_from_stack (RsvgDrawingCtx *ctx,
                                       RsvgNode *node,
                                       int dominate,
                                       gboolean clipping)
{
    RsvgState *state;
    GSList *stacksave;

    stacksave = ctx->drawsub_stack;
    if (stacksave) {
        RsvgNode *stack_node = stacksave->data;

        if (!rsvg_node_is_same (stack_node, node))
            return;

        ctx->drawsub_stack = stacksave->next;
    }

    state = rsvg_node_get_state (node);

    if (rsvg_state_is_visible (state)) {
        rsvg_drawing_ctx_state_push (ctx);
        rsvg_node_draw (node, ctx, dominate, clipping);
        rsvg_drawing_ctx_state_pop (ctx);
    }

    ctx->drawsub_stack = stacksave;
}

void
rsvg_drawing_ctx_get_offset (RsvgDrawingCtx *draw_ctx, double *x, double *y)
{
    if (x != NULL) {
        *x = draw_ctx->offset_x;
    }

    if (y != NULL) {
        *y = draw_ctx->offset_y;
    }
}

void
rsvg_drawing_ctx_insert_bbox (RsvgDrawingCtx *draw_ctx, RsvgBbox *bbox)
{
    rsvg_bbox_insert (&draw_ctx->bbox, bbox);
}

void
rsvg_drawing_ctx_insert_ink_bbox (RsvgDrawingCtx *draw_ctx, RsvgBbox *ink_bbox)
{
    rsvg_bbox_insert (&draw_ctx->ink_bbox, ink_bbox);
}

void
rsvg_drawing_ctx_push_view_box (RsvgDrawingCtx *ctx, double w, double h)
{
    RsvgViewBox *vb = g_new0 (RsvgViewBox, 1);
    *vb = ctx->vb;
    ctx->vb_stack = g_slist_prepend (ctx->vb_stack, vb);
    ctx->vb.rect.width = w;
    ctx->vb.rect.height = h;
}

void
rsvg_drawing_ctx_pop_view_box (RsvgDrawingCtx *ctx)
{
    ctx->vb = *((RsvgViewBox *) ctx->vb_stack->data);
    g_free (ctx->vb_stack->data);
    ctx->vb_stack = g_slist_delete_link (ctx->vb_stack, ctx->vb_stack);
}

void
rsvg_drawing_ctx_get_view_box_size (RsvgDrawingCtx *ctx, double *out_width, double *out_height)
{
    if (out_width)
        *out_width = ctx->vb.rect.width;

    if (out_height)
        *out_height = ctx->vb.rect.height;
}

void
rsvg_drawing_ctx_get_dpi (RsvgDrawingCtx *ctx, double *out_dpi_x, double *out_dpi_y)
{
    if (out_dpi_x)
        *out_dpi_x = ctx->dpi_x;

    if (out_dpi_y)
        *out_dpi_y = ctx->dpi_y;
}
