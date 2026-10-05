//! Owned graphics snapshots from the PTY engine, composited as kitty Unicode placeholders.
//! The child's ids never reach the outer terminal. Placeholders live in ratatui's cell buffer,
//! so pane moves, overlays and tab switches erase them through the ordinary frame diff.
use image::{DynamicImage, RgbaImage};
use pty_terminal::{
    Compression, GraphicsState, ImageBytes, PixelFormat, PlacementPosition, TerminalActor,
};
use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    widgets::Widget,
};
use ratatui_image::{
    Image,
    picker::{Picker, ProtocolType},
    protocol::{Protocol, kitty::Kitty},
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

#[derive(Clone, Default)]
pub(super) struct Frame {
    state: GraphicsState,
    images: HashMap<u32, Arc<ImageBytes>>,
}

impl Frame {
    pub(super) fn update(&mut self, actor: &TerminalActor, offset: usize) {
        self.state = actor.graphics_state(offset);
        self.images.retain(|id, _| self.state.image(*id).is_some());
        for desc in &self.state.images {
            if self
                .images
                .get(&desc.id)
                .is_none_or(|old| old.desc.generation != desc.generation)
                && let Some(image) = actor.image_bytes(desc.id)
            {
                self.images.insert(desc.id, Arc::new(image));
            }
        }
    }
}

static NEXT_IMAGE: AtomicU32 = AtomicU32::new(0x73000000);
struct Cached {
    id: u32,
    protocol: Protocol,
}
impl Drop for Cached {
    fn drop(&mut self) {
        if self.id == 0 {
            return;
        }
        // Delete only the id this compositor allocated, never the child's or a global set.
        // Uppercase I frees its stored pixels as well as its virtual placement.
        let _ = write!(std::io::stdout(), "\x1b_Ga=d,d=I,i={},q=2\x1b\\", self.id);
    }
}

#[derive(Default)]
pub(super) struct Painter {
    cache: HashMap<String, Cached>,
}
impl Painter {
    pub(super) fn draw(&mut self, frame: &Frame, buf: &mut Buffer, area: Rect, picker: &Picker) {
        let font = if picker.protocol_type() == ProtocolType::Kitty {
            picker.font_size()
        } else {
            ratatui_image::FontSize::new(
                frame.state.cell.width as u16,
                frame.state.cell.height as u16,
            )
        };
        if font.width == 0 || font.height == 0 {
            return;
        }
        let original = buf.clone();
        let mut keep = Vec::new();
        let mut remaining_pixels = 16 * 1024 * 1024u64;
        for place in frame.state.visible() {
            let (column, row, cols, rows, virtual_image) = match place.position {
                PlacementPosition::Direct {
                    col,
                    row,
                    cols,
                    rows,
                } => (col, row, cols, rows, false),
                PlacementPosition::Placeholder(p) => (
                    p.origin_col,
                    p.origin_row,
                    place.cell_size.0,
                    place.cell_size.1,
                    true,
                ),
                PlacementPosition::Offscreen => continue,
            };
            let Some((visible, skip)) = clipped(column, row, cols, rows, area) else {
                continue;
            };
            let pixels = u64::from(visible.width)
                * u64::from(visible.height)
                * u64::from(font.width)
                * u64::from(font.height);
            if pixels > remaining_pixels {
                continue;
            }
            remaining_pixels -= pixels;
            let key = format!(
                "{place:?}/{skip:?}/{}x{}/{font:?}/{:?}",
                visible.width,
                visible.height,
                picker.protocol_type()
            );
            if !self.cache.contains_key(&key) {
                let Some(bytes) = frame.images.get(&place.image_id) else {
                    continue;
                };
                let Some(decoded) = decode(bytes) else {
                    continue;
                };
                let crop = place.source;
                let Some(source) = crop_checked(&decoded, crop.x, crop.y, crop.width, crop.height)
                else {
                    continue;
                };
                let (pw, ph) = place.pixel_size;
                let canvas_width = cols.checked_mul(u32::from(font.width));
                let canvas_height = rows.checked_mul(u32::from(font.height));
                let (Some(cw), Some(ch)) = (canvas_width, canvas_height) else {
                    continue;
                };
                if !bounded(cw, ch) || !bounded(pw, ph) {
                    continue;
                }
                let resized = source
                    .resize_exact(pw, ph, image::imageops::FilterType::Triangle)
                    .into_rgba8();
                let mut canvas = RgbaImage::new(cw, ch);
                image::imageops::overlay(
                    &mut canvas,
                    &resized,
                    i64::from(place.cell_offset.0),
                    i64::from(place.cell_offset.1),
                );
                let cropped = image::imageops::crop_imm(
                    &canvas,
                    u32::from(skip.0) * u32::from(font.width),
                    u32::from(skip.1) * u32::from(font.height),
                    u32::from(visible.width) * u32::from(font.width),
                    u32::from(visible.height) * u32::from(font.height),
                )
                .to_image();
                let image = DynamicImage::ImageRgba8(cropped);
                let size = Size::new(visible.width, visible.height);
                let (id, protocol) = if picker.protocol_type() == ProtocolType::Kitty {
                    let id = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
                    let Ok(kitty) = Kitty::new(
                        image,
                        size,
                        id,
                        picker.tmux_detected(),
                        picker
                            .capabilities()
                            .contains(&ratatui_image::picker::Capability::KittyCompression),
                    ) else {
                        continue;
                    };
                    (id, Protocol::Kitty(kitty))
                } else {
                    // Halfblocks are ordinary cells; sixel/iTerm frames cannot be safely
                    // clipped or removed by a pane's cell diff.
                    let Ok(blocks) =
                        ratatui_image::protocol::halfblocks::Halfblocks::new(image, size)
                    else {
                        continue;
                    };
                    (0, Protocol::Halfblocks(blocks))
                };
                self.cache.insert(key.clone(), Cached { id, protocol });
            }
            let cached = &self.cache[&key];
            // Match the child's original image colours before painting any placement.
            // A virtual rectangle can contain holes or placeholders for another image.
            let paint: Vec<_> = (visible.y..visible.bottom())
                .flat_map(|y| (visible.x..visible.right()).map(move |x| (x, y)))
                .filter(|&pos| {
                    let cell = &original[pos];
                    let placeholder = cell.symbol().contains('\u{10eeee}');
                    let id = match cell.fg {
                        ratatui::style::Color::Rgb(r, g, b) => {
                            Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
                        }
                        ratatui::style::Color::Indexed(id) => Some(u32::from(id)),
                        _ => None,
                    };
                    let placement = match cell.underline_color {
                        ratatui::style::Color::Rgb(r, g, b) => {
                            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
                        }
                        ratatui::style::Color::Indexed(id) => u32::from(id),
                        _ => 0,
                    };
                    (!virtual_image
                        || (placeholder
                            && id == Some(place.image_id & 0xffffff)
                            && placement == place.placement_id))
                        && (place.z >= 0 || cell.symbol().trim().is_empty() || placeholder)
                })
                .collect();
            if paint.is_empty() {
                continue;
            }
            let mut rendered = buf.clone();
            Image::new(&cached.protocol).render(visible, &mut rendered);
            // The protocol attaches transmission to its first cell. If that cell is a
            // hole or covered by an overlay, transmit independently of the frame cells.
            let first = (visible.x, visible.y);
            let symbol = rendered[first].symbol().to_owned();
            if symbol.starts_with('\x1b')
                && let Some(index) = symbol.find('\u{10eeee}')
            {
                rendered[first].set_symbol(&symbol[index..]);
                // Send pixels independently of frame cells: a later palette overlay may
                // erase the first placeholder before the backend draws it.
                let _ = std::io::stdout().write_all(symbol[..index].as_bytes());
            }
            for position in paint {
                buf[position] = rendered[position].clone();
            }
            keep.push(key);
        }
        self.cache.retain(|key, _| keep.contains(key));
    }
}

fn bounded(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && u64::from(width) * u64::from(height) <= 16 * 1024 * 1024
}
fn crop_checked(
    image: &DynamicImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Option<DynamicImage> {
    (width > 0
        && height > 0
        && x.checked_add(width)? <= image.width()
        && y.checked_add(height)? <= image.height())
    .then(|| image.crop_imm(x, y, width, height))
}
fn decode(bytes: &ImageBytes) -> Option<DynamicImage> {
    if !bounded(bytes.desc.width, bytes.desc.height) {
        return None;
    }
    let expanded;
    let data = match bytes.desc.compression {
        Compression::None => bytes.data.as_slice(),
        Compression::ZlibDeflate => {
            let mut decoder =
                flate2::read::ZlibDecoder::new(bytes.data.as_slice()).take(64 * 1024 * 1024 + 1);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).ok()?;
            if output.len() > 64 * 1024 * 1024 {
                return None;
            }
            expanded = output;
            expanded.as_slice()
        }
    };
    let (width, height) = (bytes.desc.width, bytes.desc.height);
    Some(match bytes.desc.format {
        PixelFormat::Rgba => {
            DynamicImage::ImageRgba8(image::RgbaImage::from_raw(width, height, data.to_vec())?)
        }
        PixelFormat::Rgb => {
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, data.to_vec())?)
        }
        PixelFormat::Gray => {
            DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, data.to_vec())?)
        }
        PixelFormat::GrayAlpha => DynamicImage::ImageLumaA8(image::GrayAlphaImage::from_raw(
            width,
            height,
            data.to_vec(),
        )?),
        PixelFormat::Png => {
            image::load_from_memory_with_format(data, image::ImageFormat::Png).ok()?
        }
    })
}

/// Intersect pane-relative image cells with the pane. Skip is in image cells, never outer cells.
fn clipped(col: i32, row: i32, cols: u32, rows: u32, area: Rect) -> Option<(Rect, (u16, u16))> {
    let left = col.max(0);
    let top = row.max(0);
    let right = (i64::from(col) + i64::from(cols)).min(i64::from(area.width));
    let bottom = (i64::from(row) + i64::from(rows)).min(i64::from(area.height));
    if right <= i64::from(left) || bottom <= i64::from(top) {
        return None;
    }
    Some((
        Rect::new(
            area.x + left as u16,
            area.y + top as u16,
            (right - i64::from(left)) as u16,
            (bottom - i64::from(top)) as u16,
        ),
        (
            u16::try_from(i64::from(left) - i64::from(col)).ok()?,
            u16::try_from(i64::from(top) - i64::from(row)).ok()?,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_pixels_have_a_cell_fallback_inside_the_pane() {
        use base64::Engine;
        let mut actor = TerminalActor::new(6, 10, 20);
        actor.enable_graphics(Default::default());
        let pixels =
            base64::engine::general_purpose::STANDARD.encode([220u8, 90, 40, 255].repeat(16 * 32));
        actor.write(
            format!("\x1b_Ga=T,t=d,f=32,s=16,v=32,i=1,c=2,r=2,C=1;{pixels}\x1b\\").as_bytes(),
        );
        let mut frame = Frame::default();
        frame.update(&actor, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        let pane = Rect::new(20, 8, 10, 6);
        Painter::default().draw(&frame, &mut buf, pane, &Picker::halfblocks());
        let pixels: Vec<_> = (0..20)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .filter(|&pos| buf[pos].bg == ratatui::style::Color::Rgb(220, 90, 40))
            .collect();
        assert_eq!(pixels.len(), 4);
        for pos in pixels {
            assert!(pane.contains(pos.into()));
            assert!(!buf[pos].symbol().contains('\x1b'));
        }
    }
    #[test]
    fn clipping_tracks_a_scrolled_image_without_touching_adjacent_panes() {
        let pane = Rect::new(20, 8, 10, 6);
        assert_eq!(
            clipped(-2, -1, 8, 4, pane),
            Some((Rect::new(20, 8, 6, 3), (2, 1)))
        );
        assert_eq!(
            clipped(8, 4, 8, 4, pane),
            Some((Rect::new(28, 12, 2, 2), (0, 0)))
        );
        assert_eq!(clipped(10, 0, 8, 4, pane), None);
    }
}
