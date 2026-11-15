use crate::core::alignment;
use crate::core::text::{Alignment, Ellipsis, Shaping, Wrapping};
use crate::core::{Color, Font, Pixels, Point, Rectangle, Transformation};
use crate::graphics::text::cache::{self, Cache};
use crate::graphics::text::editor;
use crate::graphics::text::font_system;
use crate::graphics::text::{self, paragraph};

use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map;

#[derive(Debug)]
pub struct Pipeline {
    glyph_cache: GlyphCache,
    cache: RefCell<Cache>,
}

impl Pipeline {
    pub fn new() -> Self {
        Pipeline {
            glyph_cache: GlyphCache::new(),
            cache: RefCell::new(Cache::new()),
        }
    }

    // TODO: Shared engine
    #[allow(dead_code)]
    pub fn load_font(&mut self, bytes: Cow<'static, [u8]>) {
        font_system()
            .write()
            .expect("Write font system")
            .load_font(bytes);

        self.cache = RefCell::new(Cache::new());
    }

    pub fn draw_paragraph(
        &mut self,
        paragraph: &paragraph::Weak,
        position: Point,
        color: Color,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        clip_mask: Option<&tiny_skia::Mask>,
        transformation: Transformation,
    ) {
        let Some(paragraph) = paragraph.upgrade() else {
            return;
        };

        let mut font_system = font_system().write().expect("Write font system");

        draw(
            font_system.raw(),
            &mut self.glyph_cache,
            paragraph.buffer(),
            paragraph.effects(),
            position,
            color,
            pixels,
            clip_mask,
            transformation,
        );
    }

    pub fn draw_editor(
        &mut self,
        editor: &editor::Weak,
        position: Point,
        color: Color,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        clip_mask: Option<&tiny_skia::Mask>,
        transformation: Transformation,
    ) {
        let Some(editor) = editor.upgrade() else {
            return;
        };

        let mut font_system = font_system().write().expect("Write font system");

        draw(
            font_system.raw(),
            &mut self.glyph_cache,
            editor.buffer(),
            &[],
            position,
            color,
            pixels,
            clip_mask,
            transformation,
        );
    }

    pub fn draw_cached(
        &mut self,
        content: &str,
        bounds: Rectangle,
        color: Color,
        size: Pixels,
        line_height: Pixels,
        font: Font,
        align_x: Alignment,
        align_y: alignment::Vertical,
        shaping: Shaping,
        wrapping: Wrapping,
        ellipsis: Ellipsis,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        clip_mask: Option<&tiny_skia::Mask>,
        transformation: Transformation,
    ) {
        let line_height = f32::from(line_height);

        let mut font_system = font_system().write().expect("Write font system");
        let font_system = font_system.raw();

        let key = cache::Key {
            bounds: bounds.size(),
            content,
            font,
            size: size.into(),
            line_height,
            shaping,
            wrapping,
            ellipsis,
            align_x,
        };

        let (_, entry) = self.cache.get_mut().allocate(font_system, key);

        let width = entry.min_bounds.width;
        let height = entry.min_bounds.height;

        let x = match align_x {
            Alignment::Default | Alignment::Left | Alignment::Justified => bounds.x,
            Alignment::Center => bounds.x - width / 2.0,
            Alignment::Right => bounds.x - width,
        };

        let y = match align_y {
            alignment::Vertical::Top => bounds.y,
            alignment::Vertical::Center => bounds.y - height / 2.0,
            alignment::Vertical::Bottom => bounds.y - height,
        };

        draw(
            font_system,
            &mut self.glyph_cache,
            &entry.buffer,
            &[],
            Point::new(x, y),
            color,
            pixels,
            clip_mask,
            transformation,
        );
    }

    pub fn draw_raw(
        &mut self,
        buffer: &cosmic_text::Buffer,
        position: Point,
        color: Color,
        pixels: &mut tiny_skia::PixmapMut<'_>,
        clip_mask: Option<&tiny_skia::Mask>,
        transformation: Transformation,
    ) {
        let mut font_system = font_system().write().expect("Write font system");

        draw(
            font_system.raw(),
            &mut self.glyph_cache,
            buffer,
            &[],
            position,
            color,
            pixels,
            clip_mask,
            transformation,
        );
    }

    pub fn trim_cache(&mut self) {
        self.cache.get_mut().trim();
        self.glyph_cache.trim();
    }
}

fn draw(
    font_system: &mut cosmic_text::FontSystem,
    glyph_cache: &mut GlyphCache,
    buffer: &cosmic_text::Buffer,
    effects: &[text::Effects],
    position: Point,
    color: Color,
    pixels: &mut tiny_skia::PixmapMut<'_>,
    clip_mask: Option<&tiny_skia::Mask>,
    transformation: Transformation,
) {
    let position = position * transformation;
    let scale = transformation.scale_factor();

    let mut swash = cosmic_text::SwashCache::new();
    let scroll = buffer.scroll();

    // Shadows, then outlines, then glyphs, so that no effect covers a neighbouring glyph.
    let passes: &[Pass] = if effects.is_empty() {
        &[Pass::Glyph]
    } else {
        &[Pass::Shadow, Pass::Outline, Pass::Glyph]
    };

    for pass in passes {
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical_glyph =
                    glyph.physical((position.x - scroll.horizontal, position.y), scale);

                let glyph_color = glyph.color_opt.map(from_color).unwrap_or(color);
                let glyph_key = GlyphKey {
                    glyph: physical_glyph.cache_key,
                    outline: 0,
                };

                let (key, paint, offset) = match pass {
                    Pass::Glyph => (glyph_key, glyph_color, [0, 0]),
                    Pass::Outline | Pass::Shadow => {
                        let Some(effects) = effects.get(glyph.metadata) else {
                            continue;
                        };

                        // Colour glyphs, such as emoji, have no effects.
                        let is_mask = glyph_cache
                            .allocate(glyph_key, glyph_color, font_system, &mut swash)
                            .is_some_and(|(_, _, is_mask)| is_mask);

                        if !is_mask {
                            continue;
                        }

                        let silhouette = GlyphKey {
                            outline: effects
                                .outline
                                .map_or(0, |outline| outline_key(outline.width * scale)),
                            ..glyph_key
                        };

                        match (pass, effects.outline, effects.shadow) {
                            (Pass::Outline, Some(outline), _) if silhouette.outline > 0 => {
                                (silhouette, outline.color, [0, 0])
                            }
                            (Pass::Shadow, _, Some(shadow)) => (
                                silhouette,
                                shadow.color,
                                [
                                    (shadow.offset.x * scale).round() as i32,
                                    (shadow.offset.y * scale).round() as i32,
                                ],
                            ),
                            _ => continue,
                        }
                    }
                };

                if let Some((buffer, placement, _)) =
                    glyph_cache.allocate(key, paint, font_system, &mut swash)
                {
                    let pixmap =
                        tiny_skia::PixmapRef::from_bytes(buffer, placement.width, placement.height)
                            .expect("Create glyph pixel map");

                    let opacity = match pass {
                        Pass::Glyph => {
                            color.a * glyph.color_opt.map(|c| c.a() as f32 / 255.0).unwrap_or(1.0)
                        }
                        Pass::Outline | Pass::Shadow => color.a * paint.a,
                    };

                    pixels.draw_pixmap(
                        physical_glyph.x + placement.left + offset[0],
                        physical_glyph.y - placement.top
                            + (run.line_y * scale).round() as i32
                            + offset[1],
                        pixmap,
                        &tiny_skia::PixmapPaint {
                            opacity,
                            ..tiny_skia::PixmapPaint::default()
                        },
                        tiny_skia::Transform::identity(),
                        clip_mask,
                    );
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Shadow,
    Outline,
    Glyph,
}

fn from_color(color: cosmic_text::Color) -> Color {
    let [r, g, b, a] = color.as_rgba();

    Color::from_rgba8(r, g, b, a as f32 / 255.0)
}

/// Identifies a glyph image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    glyph: cosmic_text::CacheKey,
    /// The outline width in quarters of a physical pixel, or 0 for the plain glyph.
    outline: u16,
}

const OUTLINE_STEPS_PER_PIXEL: f32 = 4.0;
const MAX_OUTLINE_WIDTH: f32 = 8.0;

/// The [`GlyphKey::outline`] of an outline `width` in physical pixels.
fn outline_key(width: f32) -> u16 {
    (width.clamp(0.0, MAX_OUTLINE_WIDTH) * OUTLINE_STEPS_PER_PIXEL).round() as u16
}

/// Rasterises a glyph grown by its outline: its outline path stroked and filled, so that the
/// result is solid under translucent text and as a shadow.
fn rasterize_outlined(
    font_system: &mut cosmic_text::FontSystem,
    swash: &mut cosmic_text::SwashCache,
    key: GlyphKey,
) -> Option<(Vec<u8>, cosmic_text::Placement)> {
    use cosmic_text::Command;

    let commands = swash.get_outline_commands_uncached(font_system, key.glyph)?;
    let width = f32::from(key.outline) / OUTLINE_STEPS_PER_PIXEL;

    // The path is in pixels with y up, without the subpixel offset its image is rendered at.
    let [dx, dy] = {
        let [x, y] = [key.glyph.x_bin.as_float(), key.glyph.y_bin.as_float()];

        if key
            .glyph
            .flags
            .contains(cosmic_text::CacheKeyFlags::PIXEL_FONT)
        {
            [x.round(), y.round()]
        } else {
            [x, y]
        }
    };
    let point = |x: f32, y: f32| (x + dx, -(y + dy));

    let mut builder = tiny_skia::PathBuilder::new();

    for command in commands.iter() {
        match *command {
            Command::MoveTo(p) => {
                let (x, y) = point(p.x, p.y);
                builder.move_to(x, y);
            }
            Command::LineTo(p) => {
                let (x, y) = point(p.x, p.y);
                builder.line_to(x, y);
            }
            Command::QuadTo(c, p) => {
                let ((cx, cy), (x, y)) = (point(c.x, c.y), point(p.x, p.y));
                builder.quad_to(cx, cy, x, y);
            }
            Command::CurveTo(c1, c2, p) => {
                let ((x1, y1), (x2, y2), (x, y)) =
                    (point(c1.x, c1.y), point(c2.x, c2.y), point(p.x, p.y));
                builder.cubic_to(x1, y1, x2, y2, x, y);
            }
            Command::Close => builder.close(),
        }
    }

    let path = builder.finish()?;
    let bounds = path.bounds();

    let left = (bounds.left() - width).floor() as i32;
    let top = (bounds.top() - width).floor() as i32;
    let right = (bounds.right() + width).ceil() as i32;
    let bottom = (bounds.bottom() + width).ceil() as i32;

    let mut pixmap = tiny_skia::Pixmap::new((right - left) as u32, (bottom - top) as u32)?;
    let paint = tiny_skia::Paint::default();
    let transform = tiny_skia::Transform::from_translate(-left as f32, -top as f32);

    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
    pixmap.stroke_path(
        &path,
        &paint,
        &tiny_skia::Stroke {
            width: 2.0 * width,
            // Miter joins spike at sharp corners at small sizes.
            line_join: tiny_skia::LineJoin::Round,
            ..tiny_skia::Stroke::default()
        },
        transform,
        None,
    );

    let coverage = pixmap.pixels().iter().map(|pixel| pixel.alpha()).collect();

    Some((
        coverage,
        cosmic_text::Placement {
            left,
            top: -top,
            width: pixmap.width(),
            height: pixmap.height(),
        },
    ))
}

#[derive(Debug, Clone, Default)]
struct GlyphCache {
    entries: FxHashMap<(GlyphKey, [u8; 3]), (Vec<u32>, cosmic_text::Placement, bool)>,
    recently_used: FxHashSet<(GlyphKey, [u8; 3])>,
    trim_count: usize,
}

impl GlyphCache {
    const TRIM_INTERVAL: usize = 300;
    const CAPACITY_LIMIT: usize = 16 * 1024;

    fn new() -> Self {
        GlyphCache::default()
    }

    /// Returns the glyph image in `color`, and whether the glyph is a mask rather than a colour
    /// glyph.
    fn allocate(
        &mut self,
        key: GlyphKey,
        color: Color,
        font_system: &mut cosmic_text::FontSystem,
        swash: &mut cosmic_text::SwashCache,
    ) -> Option<(&[u8], cosmic_text::Placement, bool)> {
        let [r, g, b, _a] = color.into_rgba8();
        let key = (key, [r, g, b]);

        if let hash_map::Entry::Vacant(entry) = self.entries.entry(key) {
            let (content, placement, data) = if key.0.outline == 0 {
                // TODO: Outline support
                let image = swash.get_image_uncached(font_system, key.0.glyph)?;

                (image.content, image.placement, image.data)
            } else {
                let (data, placement) = rasterize_outlined(font_system, swash, key.0)?;

                (cosmic_text::SwashContent::Mask, placement, data)
            };

            let glyph_size = placement.width as usize * placement.height as usize;

            if glyph_size == 0 {
                return None;
            }

            let mut buffer = vec![0u32; glyph_size];

            match content {
                cosmic_text::SwashContent::Mask => {
                    let mut i = 0;

                    // TODO: Blend alpha

                    for _y in 0..placement.height {
                        for _x in 0..placement.width {
                            buffer[i] = bytemuck::cast(
                                tiny_skia::ColorU8::from_rgba(b, g, r, data[i]).premultiply(),
                            );

                            i += 1;
                        }
                    }
                }
                cosmic_text::SwashContent::Color => {
                    let mut i = 0;

                    for _y in 0..placement.height {
                        for _x in 0..placement.width {
                            // TODO: Blend alpha
                            buffer[i >> 2] = bytemuck::cast(
                                tiny_skia::ColorU8::from_rgba(
                                    data[i + 2],
                                    data[i + 1],
                                    data[i],
                                    data[i + 3],
                                )
                                .premultiply(),
                            );

                            i += 4;
                        }
                    }
                }
                cosmic_text::SwashContent::SubpixelMask => {
                    // TODO
                }
            }

            let is_mask = content == cosmic_text::SwashContent::Mask;
            let _ = entry.insert((buffer, placement, is_mask));
        }

        let _ = self.recently_used.insert(key);

        self.entries.get(&key).map(|(buffer, placement, is_mask)| {
            (
                bytemuck::cast_slice(buffer.as_slice()),
                *placement,
                *is_mask,
            )
        })
    }

    pub fn trim(&mut self) {
        if self.trim_count > Self::TRIM_INTERVAL || self.recently_used.len() >= Self::CAPACITY_LIMIT
        {
            self.entries
                .retain(|key, _| self.recently_used.contains(key));

            self.recently_used.clear();

            self.entries.shrink_to(Self::CAPACITY_LIMIT);
            self.recently_used.shrink_to(Self::CAPACITY_LIMIT);

            self.trim_count = 0;
        } else {
            self.trim_count += 1;
        }
    }
}
