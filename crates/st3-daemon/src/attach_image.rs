//! Prepare a file for `--attach`. A message carries PNG, JPEG, GIF and WebP images of at most
//! [`MAX_BLOB_BYTES`]; this turns the other still images a seat makes into one of those, and
//! scales an image over the limit down until it fits. The daemon still checks every upload; this
//! only runs in the CLI, before the upload.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::blobs::{MAX_BLOB_BYTES, sniff_media_type};

/// `--attach` in `conversations send` and `reply`.
pub const HELP: &str = "Attach an image (PNG, JPEG, GIF, WebP, SVG or BMP; up to 4)";

pub const LONG_HELP: &str = r#"Attach an image (PNG, JPEG, GIF, WebP, SVG or BMP; repeat for up to 4).

An image shows what words describe poorly: a screenshot of a UI problem with the problem marked, one screenshot per step of a tutorial, or a chart in place of a table of numbers.

PNG, JPEG, GIF and WebP up to 10 MiB are sent as they are. An SVG is rendered to a PNG on white at twice its size, drawing only images inside its own folder or in data: URLs; a BMP becomes a PNG, and a still image over 10 MiB is scaled down until it fits; the command says when it changed a file. HEIC, AVIF, TIFF and PDF are refused with a way to convert them.

To mark up a screenshot, write an SVG beside it that draws the screenshot and the marks, and attach the SVG:

  <svg xmlns="http://www.w3.org/2000/svg" width="1280" height="800">
    <image href="shot.png" width="1280" height="800"/>
    <rect x="40" y="120" width="300" height="48" fill="none" stroke="red" stroke-width="4"/>
    <text x="40" y="200" font-family="sans-serif" font-size="28" fill="red">Save overlaps the field</text>
  </svg>

  st conversations send agent/example/reviewer --from "$ST_AGENT" --body "Save overlaps the email field at 1280 px" --attach marked.svg

A screenshot can come from a headless browser (`chromium --headless --screenshot=shot.png --window-size=1280,800 URL`, or Playwright's `page.screenshot`) or the system's screenshot tool, and a chart from the project's plotting library or an SVG written by hand. The file is uploaded to this member and fetched by the machine that reads or delivers the message."#;

/// The largest file `prepare` reads: a screenshot this size still scales down to fit.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
/// The longest side an SVG is rendered at.
const MAX_SVG_SIDE: f32 = 4096.0;
/// SVGs are drawn at twice their own size, so text stays sharp on a high-density display.
const SVG_SCALE: f32 = 2.0;
/// The smallest side a shrunk image may have; anything smaller is no longer worth sending.
const MIN_SIDE: u32 = 64;

/// A file ready to upload, and what was changed to make it so.
#[derive(Debug)]
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
    pub name: Option<String>,
    /// One sentence for the sender when the file was converted or scaled.
    pub note: Option<String>,
}

/// Turn `bytes`, read from `path`, into an image a message may carry.
pub fn prepare(path: &Path, bytes: Vec<u8>) -> Result<Prepared> {
    let shown = path.display();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    if let Some(media_type) = sniff_media_type(&bytes) {
        if bytes.len() <= MAX_BLOB_BYTES {
            return Ok(Prepared {
                bytes,
                media_type,
                name,
                note: None,
            });
        }
        if media_type == "image/gif" {
            bail!(
                "{shown} is a {} GIF and an attachment is at most {}; send fewer frames, or a still PNG",
                mib(bytes.len()),
                mib(MAX_BLOB_BYTES)
            );
        }
        let image = decode(&bytes).with_context(|| format!("decode {shown}"))?;
        let jpeg = media_type != "image/png";
        let (bytes, media_type, size) = fit(image, jpeg, &shown.to_string())?;
        return Ok(Prepared {
            note: Some(format!(
                "{shown} is larger than {}, so a {}×{} {} copy ({}) is attached",
                mib(MAX_BLOB_BYTES),
                size.0,
                size.1,
                if jpeg { "JPEG" } else { "PNG" },
                mib(bytes.len())
            )),
            name: renamed(path, media_type),
            bytes,
            media_type,
        });
    }
    if is_svg(path, &bytes) {
        let (png, size) = render_svg(path, &bytes)?;
        let (bytes, media_type, size) = if png.len() <= MAX_BLOB_BYTES {
            (png, "image/png", size)
        } else {
            fit(decode(&png)?, false, &shown.to_string())?
        };
        return Ok(Prepared {
            note: Some(format!(
                "{shown} is rendered to a {}×{} PNG ({})",
                size.0,
                size.1,
                mib(bytes.len())
            )),
            name: renamed(path, media_type),
            bytes,
            media_type,
        });
    }
    if bytes.starts_with(b"BM") {
        let image = decode(&bytes).with_context(|| format!("decode {shown} as a BMP image"))?;
        let (bytes, media_type, size) = fit(image, false, &shown.to_string())?;
        return Ok(Prepared {
            note: Some(format!(
                "{shown} is converted to a {}×{} PNG ({})",
                size.0,
                size.1,
                mib(bytes.len())
            )),
            name: renamed(path, media_type),
            bytes,
            media_type,
        });
    }
    match unsupported_kind(&bytes) {
        Some(kind) => bail!(
            "{shown} is {kind}, which a message does not carry; convert it to PNG or JPEG first ({})",
            conversion_hint(kind)
        ),
        None => bail!("{shown} is not an image st attaches: PNG, JPEG, GIF, WebP, SVG or BMP"),
    }
}

fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    // The decoder's default limits refuse an image that would need more than 512 MiB.
    reader.limits(image::Limits::default());
    Ok(reader.decode()?)
}

/// Encode `image`, scaling it down until it fits the attachment limit. Screenshots stay PNG so
/// their text stays sharp; photographs become JPEG.
fn fit(
    image: DynamicImage,
    jpeg: bool,
    shown: &str,
) -> Result<(Vec<u8>, &'static str, (u32, u32))> {
    let mut current = image;
    loop {
        let bytes = encode(&current, jpeg)?;
        let size = (current.width(), current.height());
        if bytes.len() <= MAX_BLOB_BYTES {
            return Ok((bytes, if jpeg { "image/jpeg" } else { "image/png" }, size));
        }
        // Encoded size follows the pixel count, so aim for the limit with a margin.
        let ratio = ((MAX_BLOB_BYTES as f64 * 0.9) / bytes.len() as f64)
            .sqrt()
            .min(0.9);
        let width = (f64::from(size.0) * ratio) as u32;
        let height = (f64::from(size.1) * ratio) as u32;
        if width < MIN_SIDE || height < MIN_SIDE {
            bail!(
                "{shown} does not fit in {} even scaled down",
                mib(MAX_BLOB_BYTES)
            );
        }
        current = current.resize(width, height, FilterType::Lanczos3);
    }
}

fn encode(image: &DynamicImage, jpeg: bool) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    if jpeg {
        DynamicImage::ImageRgb8(image.to_rgb8()).write_to(&mut out, ImageFormat::Jpeg)?;
    } else {
        image.write_to(&mut out, ImageFormat::Png)?;
    }
    Ok(out.into_inner())
}

fn is_svg(path: &Path, bytes: &[u8]) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_ascii_lowercase());
    if extension
        .as_deref()
        .is_some_and(|extension| extension == "svg" || extension == "svgz")
    {
        return true;
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with("<svg") || (head.starts_with("<?xml") && head.contains("<svg"))
}

/// Draw an SVG on white, the way a browser shows one on its own page. An `<image href>`, such as a
/// screenshot the SVG marks up, is a `data:` URL or a file inside the SVG's own folder.
fn render_svg(path: &Path, bytes: &[u8]) -> Result<(Vec<u8>, (u32, u32))> {
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    render_svg_with(path, bytes, fonts)
}

fn render_svg_with(
    path: &Path,
    bytes: &[u8],
    fonts: resvg::usvg::fontdb::Database,
) -> Result<(Vec<u8>, (u32, u32))> {
    use resvg::{tiny_skia, usvg};
    let shown = path.display();
    let folder = std::fs::canonicalize(path)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let refused = Arc::new(Mutex::new(Vec::new()));
    let fontless = Arc::new(AtomicBool::new(false));
    let options = usvg::Options {
        resources_dir: folder.clone(),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: folder_resolver(folder, Arc::clone(&refused)),
        },
        font_resolver: any_font_resolver(Arc::clone(&fontless)),
        fontdb: Arc::new(fonts),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options)
        .map_err(|error| anyhow::anyhow!("{shown} is not an SVG st can draw: {error}"))?;
    let refused = std::mem::take(&mut *refused.lock().unwrap_or_else(|error| error.into_inner()));
    if !refused.is_empty() {
        bail!(
            "{shown} draws {}, outside the SVG's own folder; copy the image beside the SVG, or embed it as a data: URL",
            refused.join(", ")
        );
    }
    if fontless.load(Ordering::Relaxed) {
        bail!(
            "{shown} has text, and this machine has no fonts to draw it with; install a font, or turn the text into paths"
        );
    }
    let size = tree.size();
    let scale = SVG_SCALE.min(MAX_SVG_SIDE / size.width().max(size.height()));
    let width = (size.width() * scale).ceil() as u32;
    let height = (size.height() * scale).ceil() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .with_context(|| format!("{shown} has no area to draw"))?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let png = pixmap
        .encode_png()
        .with_context(|| format!("encode {shown} as PNG"))?;
    Ok((png, (width, height)))
}

/// Resolve an `<image href>` path only to a file inside `folder`, the SVG's own folder, after
/// following `..` and symbolic links. An SVG that came from elsewhere then cannot draw another
/// local image into the PNG a recipient sees. Each path refused is recorded for the error.
fn folder_resolver(
    folder: Option<PathBuf>,
    refused: Arc<Mutex<Vec<String>>>,
) -> resvg::usvg::ImageHrefStringResolverFn<'static> {
    let load = resvg::usvg::ImageHrefResolver::default_string_resolver();
    Box::new(move |href, options| {
        let inside = folder.as_ref().and_then(|folder| {
            let resolved = std::fs::canonicalize(folder.join(href)).ok()?;
            resolved.starts_with(folder).then_some(resolved)
        });
        match inside {
            Some(resolved) => load(&resolved.to_string_lossy(), options),
            None => {
                refused
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(href.to_owned());
                None
            }
        }
    })
}

/// Draw text in the font it names, or else in any font this machine has. usvg leaves out text
/// whose font-family no installed font matches, and its default family is Times New Roman, which a
/// server rarely has; the caption on a marked-up screenshot would then vanish without a word.
/// Text with no font at all to draw it is recorded for the error.
fn any_font_resolver(fontless: Arc<AtomicBool>) -> resvg::usvg::FontResolver<'static> {
    use resvg::usvg::{FontResolver, fontdb::Style};
    let named = FontResolver::default_font_selector();
    FontResolver {
        select_font: Box::new(move |font, fonts| {
            let id = named(font, fonts).or_else(|| {
                let face = fonts.faces().find(|face| face.style == Style::Normal);
                face.or_else(|| fonts.faces().next()).map(|face| face.id)
            });
            if id.is_none() {
                fontless.store(true, Ordering::Relaxed);
            }
            id
        }),
        select_fallback: FontResolver::default_fallback_selector(),
    }
}

/// Name a converted file for what it now is: `chart.svg` is attached as `chart.png`.
fn renamed(path: &Path, media_type: &str) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    Some(format!(
        "{stem}.{}",
        crate::blobs::file_extension(media_type)
    ))
}

fn unsupported_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF") {
        return Some("a PDF");
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some("a TIFF image");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return match &bytes[8..12] {
            b"avif" | b"avis" => Some("an AVIF image"),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                Some("a HEIC image")
            }
            _ => None,
        };
    }
    None
}

fn conversion_hint(kind: &str) -> &'static str {
    match kind {
        "a PDF" => "for example `pdftoppm -png -r 144 -f 1 -l 1 FILE page`",
        _ => {
            "for example `sips -s format png FILE --out FILE.png` on macOS, or `magick FILE FILE.png`"
        }
    }
}

fn mib(bytes: usize) -> String {
    let value = bytes as f64 / (1024.0 * 1024.0);
    if value >= 10.0 {
        format!("{value:.0} MiB")
    } else {
        format!("{value:.1} MiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, noisy: bool) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            if noisy {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let [r, g, b, _] = state.to_le_bytes();
                image::Rgb([r, g, b])
            } else {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
            }
        });
        encode(&DynamicImage::ImageRgb8(image), false).unwrap()
    }

    #[test]
    fn a_supported_image_within_the_limit_is_sent_as_it_is() {
        let bytes = png(40, 30, false);
        let prepared = prepare(Path::new("shot.png"), bytes.clone()).unwrap();
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(prepared.name.as_deref(), Some("shot.png"));
        assert!(prepared.note.is_none());
    }

    #[test]
    fn a_png_over_the_limit_is_scaled_down_until_it_fits() {
        // Noise does not compress: 2000×2000 RGB is about 11.5 MiB as PNG.
        let bytes = png(2000, 2000, true);
        assert!(bytes.len() > MAX_BLOB_BYTES);
        let prepared = prepare(Path::new("full-page.png"), bytes).unwrap();
        assert!(prepared.bytes.len() <= MAX_BLOB_BYTES);
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(sniff_media_type(&prepared.bytes), Some("image/png"));
        let image = image::load_from_memory(&prepared.bytes).unwrap();
        assert!(image.width() < 2000 && image.width() == image.height());
        assert!(prepared.note.unwrap().contains("larger than 10 MiB"));
    }

    #[test]
    fn an_svg_marking_up_a_screenshot_beside_it_is_rendered_to_png() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), png(64, 40, false)).unwrap();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="40">
  <image href="shot.png" width="64" height="40"/>
  <rect x="4" y="4" width="20" height="10" fill="none" stroke="red" stroke-width="2"/>
</svg>"#;
        let path = dir.path().join("marked.svg");
        std::fs::write(&path, svg).unwrap();
        let prepared = prepare(&path, svg.as_bytes().to_vec()).unwrap();
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(prepared.name.as_deref(), Some("marked.png"));
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgb8();
        assert_eq!((image.width(), image.height()), (128, 80));
        // The red outline is drawn over the screenshot, which is drawn too.
        assert_eq!(image.get_pixel(8, 8).0, [255, 0, 0]);
        assert_eq!(image.get_pixel(100, 60).0[2], 128);
    }

    #[test]
    fn a_caption_in_a_font_this_machine_lacks_is_drawn_in_one_it_has() {
        let font = Path::new(test_env!("CARGO_MANIFEST_DIR")).join(
            "../../apps/fractal-web/src/terminal/assets/JetBrainsMonoNerdFontMono-Regular.ttf",
        );
        let mut fonts = resvg::usvg::fontdb::Database::new();
        fonts.load_font_data(std::fs::read(font).unwrap());
        for family in [
            "",
            r#" font-family="Invented Sans""#,
            r#" font-family="sans-serif""#,
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="40"><text x="4" y="30" font-size="28"{family} fill="red">WWWW</text></svg>"#
            );
            let (png, _) = render_svg_with(Path::new("caption.svg"), svg.as_bytes(), fonts.clone())
                .unwrap_or_else(|error| panic!("{family}: {error}"));
            let image = image::load_from_memory(&png).unwrap().to_rgb8();
            assert!(
                image.pixels().any(|pixel| pixel.0 == [255, 0, 0]),
                "{family}: the caption is not drawn"
            );
        }
    }

    #[test]
    fn text_with_no_font_to_draw_it_is_refused() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="40"><text x="4" y="30">Save</text></svg>"#;
        let error = render_svg_with(
            Path::new("caption.svg"),
            svg,
            resvg::usvg::fontdb::Database::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no fonts"), "{error}");
    }

    #[test]
    fn an_svg_draws_only_images_inside_its_own_folder() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("a/b");
        std::fs::create_dir_all(&folder).unwrap();
        let secret = root.path().join("secret.png");
        std::fs::write(&secret, png(8, 8, false)).unwrap();
        std::os::unix::fs::symlink(&secret, folder.join("link.png")).unwrap();
        for href in [
            "../../secret.png".to_owned(),
            secret.display().to_string(),
            "link.png".to_owned(),
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><image href="{href}" width="8" height="8"/></svg>"#
            );
            let path = folder.join("marked.svg");
            std::fs::write(&path, &svg).unwrap();
            let error = prepare(&path, svg.into_bytes()).unwrap_err().to_string();
            assert!(
                error.contains(&href) && error.contains("outside the SVG's own folder"),
                "{href}: {error}"
            );
        }
    }

    #[test]
    fn an_svg_without_an_extension_is_recognized_by_its_bytes() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#;
        let prepared = prepare(Path::new("chart"), svg.to_vec()).unwrap();
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(prepared.name.as_deref(), Some("chart.png"));
    }

    #[test]
    fn a_bmp_is_converted_to_png() {
        let mut bmp = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image::RgbImage::new(8, 8))
            .write_to(&mut bmp, ImageFormat::Bmp)
            .unwrap();
        let prepared = prepare(Path::new("capture.bmp"), bmp.into_inner()).unwrap();
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(prepared.name.as_deref(), Some("capture.png"));
        assert!(prepared.note.unwrap().contains("converted"));
    }

    #[test]
    fn the_markup_examples_in_the_skill_and_the_help_render() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), png(1280, 800, false)).unwrap();
        for (source, text) in [("skill", crate::skill::SKILL), ("help", LONG_HELP)] {
            let start = text.find("<svg ").expect(source);
            let end = text[start..].find("</svg>").expect(source) + start + "</svg>".len();
            let path = dir.path().join(format!("{source}.svg"));
            std::fs::write(&path, &text[start..end]).unwrap();
            match prepare(&path, text.as_bytes()[start..end].to_vec()) {
                Ok(prepared) => {
                    let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgb8();
                    assert_eq!((image.width(), image.height()), (2560, 1600), "{source}");
                    // The red outline at (40, 120) sits over the screenshot.
                    assert_eq!(image.get_pixel(80, 300).0, [255, 0, 0], "{source}");
                    if source == "help" {
                        use image::GenericImageView as _;
                        // So is its caption, below the outline.
                        let caption = image.view(80, 340, 600, 70).to_image();
                        assert!(
                            caption.pixels().any(|pixel| pixel.0 == [255, 0, 0]),
                            "the help example's caption is not drawn"
                        );
                    }
                }
                // A build sandbox may have no fonts for the help example's caption.
                Err(error) => assert!(
                    source == "help" && error.to_string().contains("no fonts"),
                    "{source}: {error}"
                ),
            }
        }
    }

    #[test]
    fn formats_a_message_does_not_carry_are_refused_with_how_to_convert_them() {
        let mut heic = vec![0, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0; 12]);
        let error = prepare(Path::new("photo.heic"), heic)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("a HEIC image") && error.contains("sips"),
            "{error}"
        );
        let error = prepare(Path::new("report.pdf"), b"%PDF-1.7".to_vec())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("a PDF") && error.contains("pdftoppm"),
            "{error}"
        );
        let error = prepare(Path::new("notes.txt"), b"hello".to_vec())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("PNG, JPEG, GIF, WebP, SVG or BMP"),
            "{error}"
        );
    }
}
