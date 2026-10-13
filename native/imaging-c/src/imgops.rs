//! The `Image` handle: decode, encode, probe and every pixel transform the PIL
//! surface needs. Pixels are always straight (non-premultiplied) RGBA8.

use std::io::Cursor;

use base64::Engine as _;
use image::imageops::FilterType;
use image::metadata::LoopCount;
use image::{
    AnimationDecoder, ColorType, Delay, DynamicImage, Frame, ImageDecoder, ImageFormat, RgbaImage,
};
use resvg::tiny_skia;

use crate::util::{R, Rgba, err, from_pixmap, to_pixmap};

pub struct Image {
    pub buf: RgbaImage,
}

impl Image {
    pub fn new(w: u32, h: u32, fill: Rgba) -> R<Image> {
        if w == 0 || h == 0 || (w as u64) * (h as u64) > 512 * 1024 * 1024 {
            return err(format!("refusing to allocate a {w}x{h} image"));
        }
        Ok(Image {
            buf: RgbaImage::from_pixel(w, h, image::Rgba([fill.0, fill.1, fill.2, fill.3])),
        })
    }

    pub fn from_rgba(w: u32, h: u32, data: &[u8]) -> R<Image> {
        let need = (w as usize) * (h as usize) * 4;
        if data.len() < need {
            return err(format!("pixel buffer too small: {} < {need}", data.len()));
        }
        RgbaImage::from_raw(w, h, data[..need].to_vec())
            .map(|buf| Image { buf })
            .ok_or_else(|| "cannot build image from pixels".to_string())
    }

    pub fn width(&self) -> u32 {
        self.buf.width()
    }

    pub fn height(&self) -> u32 {
        self.buf.height()
    }

    pub fn from_pixmap(pm: &tiny_skia::Pixmap) -> R<Image> {
        Image::from_rgba(pm.width(), pm.height(), &from_pixmap(pm))
    }

    pub fn to_pixmap(&self) -> R<tiny_skia::Pixmap> {
        to_pixmap(self.width(), self.height(), self.buf.as_raw())
    }

    pub fn is_opaque(&self) -> bool {
        self.buf.pixels().all(|p| p.0[3] == 255)
    }

    /// True when every pixel has r == g == b, i.e. the RGBA buffer carries no
    /// colour and could be stored as one channel. Answered from the PIXELS
    /// (the handle no longer remembers what it was decoded from), so it is a
    /// fact about the image, not about the file it came from — use `probe` for
    /// the source's own colour type.
    pub fn is_grayscale(&self) -> bool {
        self.buf
            .pixels()
            .all(|p| p.0[0] == p.0[1] && p.0[1] == p.0[2])
    }
}

// ---------------------------------------------------------------------------
// decode / probe
// ---------------------------------------------------------------------------

/// How much a DECODE may allocate. The `image` crate's own default is 512 MiB,
/// which silently rejects any image whose RGBA8 buffer is bigger than that —
/// roughly 134 MP, so an 11600x11600 PNG (537 MiB) comes back as "cannot
/// identify image file" even though the file is perfectly valid. Decoded
/// buffers are already bounded by `Image::new`'s 512 Mi-PIXEL cap (2 GiB of
/// RGBA8), so that is the ceiling decoding gets too: one limit, stated once,
/// instead of two that disagree.
const DECODE_MAX_ALLOC: u64 = 2 * 1024 * 1024 * 1024;

/// A format-guessing reader carrying OUR allocation ceiling. One place, so
/// `decode` and `probe` can never drift apart on how big an image may be.
#[allow(clippy::field_reassign_with_default)]
fn reader(data: &[u8]) -> R<image::ImageReader<Cursor<&[u8]>>> {
    let mut r = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| format!("decode failed: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    r.limits(limits);
    Ok(r)
}

pub fn decode(data: &[u8]) -> R<Image> {
    if is_heif(data) {
        return decode_heif(data);
    }
    let img = reader(data)?
        .decode()
        .map_err(|e| format!("decode failed: {e}"))?;
    Ok(Image {
        buf: img.to_rgba8(),
    })
}

/// Format, geometry and COLOUR TYPE without decoding pixels — the cheap
/// question ("is this a 40 MP TIFF?", "is this file grayscale?") answered
/// before committing memory to it. The colour fields describe the SOURCE, which
/// a decoded handle can no longer tell you: `decode` normalises everything to
/// RGBA8.
pub fn probe(data: &[u8]) -> R<serde_json::Value> {
    if is_heif(data) {
        return probe_heif(data);
    }
    let reader = reader(data)?;
    let format = reader
        .format()
        .map(format_name)
        .unwrap_or("unknown")
        .to_string();
    let decoder = reader
        .into_decoder()
        .map_err(|e| format!("probe failed: {e}"))?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    let (is_animated, frames) = animation(data, &format);
    let mut v = serde_json::json!({
        "format": format,
        "width": width,
        "height": height,
        "bytes": data.len(),
        "is_animated": is_animated,
        "frames": frames,
    });
    merge_color(&mut v, color);
    Ok(v)
}

// ---------------------------------------------------------------------------
// HEIC / HEIF
// ---------------------------------------------------------------------------

/// `ftyp` brands of a HEIF file: the HEVC still and sequence brands (`heic`
/// `heix` `heim` `heis` / `hevc` `hevx`), the AV1 ones (`avif` `avis`), the
/// H.264 ones (`avci` `avcs`), JPEG (`jpeg`), uncompressed (`unif`) and the
/// generic MIAF ones (`mif1` `msf1`) that many encoders put first.
const HEIF_BRANDS: [&[u8; 4]; 14] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"avif", b"avis", b"avci", b"avcs",
    b"jpeg", b"unif", b"mif1", b"msf1",
];

/// True when `data` is a HEIF container. `image` has no HEIF decoder, so these
/// bytes go to `heif::decode`, which picks the codec of the primary item.
pub fn is_heif(data: &[u8]) -> bool {
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    let major = &data[8..12];
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let (compatible, _) = data[16..size.clamp(16, data.len())].as_chunks::<4>();
    std::iter::once(major)
        .chain(compatible.iter().map(|b| &b[..]))
        .any(|b| HEIF_BRANDS.iter().any(|h| &h[..] == b))
}

/// The primary image of a HEIF file, display-ready: grid tiles joined,
/// `clap`/`irot`/`imir` applied and any alpha plane attached. The pixel cap is
/// the same one `reader` gives the `image` crate.
fn decode_heif(data: &[u8]) -> R<Image> {
    let img = crate::heif::decode(data, DECODE_MAX_ALLOC / 4)
        .map_err(|e| format!("decode failed: {e}"))?;
    Image::from_rgba(img.width, img.height, &img.data)
}

/// `probe` for HEIF, read from the boxes alone. The size is the DISPLAY size
/// (after rotation), so it matches what `decode` returns. Only the primary
/// image is decoded, so a sequence (`msf1`) reports one still frame.
fn probe_heif(data: &[u8]) -> R<serde_json::Value> {
    let info = crate::heif::probe(data).map_err(|e| format!("probe failed: {e}"))?;
    let color = match (info.is_gray, info.has_alpha, info.bit_depth > 8) {
        (true, false, false) => ColorType::L8,
        (true, true, false) => ColorType::La8,
        (true, false, true) => ColorType::L16,
        (true, true, true) => ColorType::La16,
        (false, false, false) => ColorType::Rgb8,
        (false, true, false) => ColorType::Rgba8,
        (false, false, true) => ColorType::Rgb16,
        (false, true, true) => ColorType::Rgba16,
    };
    let mut v = serde_json::json!({
        "format": info.format,
        "width": info.width,
        "height": info.height,
        "bytes": data.len(),
        "is_animated": false,
        "frames": 1,
    });
    merge_color(&mut v, color);
    Ok(v)
}

/// Add the `color`/`channels`/`bits`/`is_grayscale`/`has_alpha` view of a
/// colour type to a JSON object. One place, so `probe` and the SVG branch in
/// `lib.rs` can never drift apart.
pub fn merge_color(v: &mut serde_json::Value, c: ColorType) {
    let channels = u32::from(c.channel_count());
    if let Some(o) = v.as_object_mut() {
        o.insert("color".into(), color_name(c).into());
        o.insert("channels".into(), channels.into());
        o.insert(
            "bits".into(),
            (u32::from(c.bits_per_pixel()) / channels).into(),
        );
        o.insert("is_grayscale".into(), (!c.has_color()).into());
        o.insert("has_alpha".into(), c.has_alpha().into());
    }
}

/// The colour type a decoder would hand us, named the way the pixel layout
/// reads: channels then bit depth. Indexed/palette sources report the type
/// they expand into (`rgb8`/`rgba8`), because that is what any caller will
/// actually receive.
pub fn color_name(c: ColorType) -> &'static str {
    match c {
        ColorType::L8 => "l8",
        ColorType::La8 => "la8",
        ColorType::Rgb8 => "rgb8",
        ColorType::Rgba8 => "rgba8",
        ColorType::L16 => "l16",
        ColorType::La16 => "la16",
        ColorType::Rgb16 => "rgb16",
        ColorType::Rgba16 => "rgba16",
        ColorType::Rgb32F => "rgb32f",
        ColorType::Rgba32F => "rgba32f",
        _ => "rgba8",
    }
}

/// Animation facts read from the CONTAINER, never by decoding frames: APNG's
/// `acTL` chunk carries the frame count outright, an animated WebP is a RIFF
/// with an `ANIM` chunk and one `ANMF` per frame, and a GIF frame is introduced
/// by a Graphic Control Extension. Cheap and exact for the three formats that
/// can animate; everything else is a single still frame.
fn animation(data: &[u8], format: &str) -> (bool, u32) {
    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
    match format {
        "png" => match find(data, b"acTL") {
            Some(i) if data.len() >= i + 8 => {
                let n = u32::from_be_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]);
                (n > 1, n.max(1))
            }
            _ => (false, 1),
        },
        "webp" => {
            if find(data, b"ANIM").is_none() {
                return (false, 1);
            }
            let n = data.windows(4).filter(|w| *w == b"ANMF").count() as u32;
            (true, n.max(1))
        }
        "gif" => {
            let n = data.windows(3).filter(|w| *w == [0x21, 0xF9, 0x04]).count() as u32;
            (n > 1, n.max(1))
        }
        _ => (false, 1),
    }
}

pub fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Gif => "gif",
        ImageFormat::WebP => "webp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Ico => "ico",
        ImageFormat::Tga => "tga",
        ImageFormat::Qoi => "qoi",
        ImageFormat::Pnm => "pnm",
        ImageFormat::Hdr => "hdr",
        ImageFormat::OpenExr => "exr",
        _ => "unknown",
    }
}

fn format_by_name(name: &str) -> R<ImageFormat> {
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        "tga" => ImageFormat::Tga,
        "qoi" => ImageFormat::Qoi,
        // WebP is LOSSLESS-only here: image-webp ships a lossless encoder and no
        // lossy one, and a lossy encoder would mean libwebp (a C dependency),
        // which would defeat the whole point of a pure-Rust cdylib. `quality` is
        // therefore ignored for webp.
        "webp" => ImageFormat::WebP,
        "pnm" | "ppm" => ImageFormat::Pnm,
        other => return err(format!("unsupported output format: {other}")),
    })
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

/// Encode to `format`. JPEG has no alpha, so a transparent image is flattened
/// onto white first rather than failing or going black.
pub fn encode(img: &Image, format: &str, quality: u8) -> R<Vec<u8>> {
    let fmt = format_by_name(format)?;
    let mut out = Cursor::new(Vec::new());
    match fmt {
        ImageFormat::Jpeg => {
            let flat = flatten(img, Rgba::WHITE);
            let rgb = DynamicImage::ImageRgba8(flat.buf).to_rgb8();
            let q = if quality == 0 {
                85
            } else {
                quality.clamp(1, 100)
            };
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q);
            enc.encode_image(&rgb)
                .map_err(|e| format!("jpeg encode failed: {e}"))?;
        }
        ImageFormat::WebP => {
            // `write_to` refuses WebP; the encoder must be driven directly.
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .encode(
                    img.buf.as_raw(),
                    img.width(),
                    img.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("webp encode failed: {e}"))?;
        }
        _ => {
            DynamicImage::ImageRgba8(img.buf.clone())
                .write_to(&mut out, fmt)
                .map_err(|e| format!("{format} encode failed: {e}"))?;
        }
    }
    Ok(out.into_inner())
}

// ---------------------------------------------------------------------------
// animated GIF — the only multi-frame format with a pure-Rust path. Decoded as
// JSON so the handle-free Clojure surface can stay Opaque: every frame's RGBA
// rows are base64, so the contract is {width height loop-count frames[]}, each
// frame {delay-ms rgba}. This is where animated GIF support actually lives
// now, instead of a hand-rolled Clojure codec that could not survive a Rust
// decode-limit change.
// ---------------------------------------------------------------------------

/// Decode an animated GIF into `{width height loop-count frames[]}`, each frame
/// `{delay-ms rgba}` with `rgba` the straight RGBA8 rows as base64. A still GIF
/// (one frame) decodes the same way, with a single entry. GIFs are always RGBA8
/// (the decoder expands the palette), so there is no colour-type branch here.
pub fn decode_gif(data: &[u8]) -> R<serde_json::Value> {
    use image::codecs::gif::GifDecoder;
    let dec = GifDecoder::new(Cursor::new(data)).map_err(|e| format!("gif decode failed: {e}"))?;
    let loop_count = match dec.loop_count() {
        LoopCount::Infinite => -1,
        LoopCount::Finite(n) => n.get() as i64,
    };
    let (width, height) = dec.dimensions();
    let frames = dec
        .into_frames()
        .collect_frames()
        .map_err(|e| format!("gif decode failed: {e}"))?;
    if frames.is_empty() {
        return err("gif has no frames");
    }
    let frames_json: Vec<serde_json::Value> = frames
        .iter()
        .map(|f| {
            let (numer, denom) = f.delay().numer_denom_ms();
            let delay_ms = if denom == 0 { 0 } else { numer / denom };
            let rgba = f.buffer().as_raw();
            serde_json::json!({
                "delay_ms": delay_ms,
                "rgba": base64::engine::general_purpose::STANDARD.encode(rgba),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "width": width,
        "height": height,
        "loop_count": loop_count,
        "frames": frames_json,
    }))
}

/// Encode an animated GIF from the same JSON shape `decode_gif` produces. Each
/// frame is `{delay-ms rgba}`; `rgba` is base64 straight RGBA8 sized to
/// `width`x`height` (sub-frames are not supported — every frame is a full
/// canvas, which is what PIL's save_all and the old Clojure codec produced).
/// `loop-count` is the iteration count; -1 (or omitted) means loop forever.
pub fn encode_gif(spec: &serde_json::Value) -> R<Vec<u8>> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let width = spec
        .get("width")
        .and_then(|v| v.as_u64())
        .ok_or("gif spec needs width")? as u32;
    let height = spec
        .get("height")
        .and_then(|v| v.as_u64())
        .ok_or("gif spec needs height")? as u32;
    let frames = spec
        .get("frames")
        .and_then(|v| v.as_array())
        .ok_or("gif spec needs frames")?;
    if frames.is_empty() {
        return err("gif needs at least one frame");
    }
    let loop_count = spec
        .get("loop_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);

    let mut out = Cursor::new(Vec::new());
    let mut enc = GifEncoder::new(&mut out);
    if loop_count < 0 {
        enc.set_repeat(Repeat::Infinite)
            .map_err(|e| format!("gif encode failed: {e}"))?;
    } else {
        enc.set_repeat(Repeat::Finite(loop_count.min(u16::MAX as i64) as u16))
            .map_err(|e| format!("gif encode failed: {e}"))?;
    }

    let need = (width as usize) * (height as usize) * 4;
    for (i, f) in frames.iter().enumerate() {
        let delay_ms = f.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let rgba_b64 = f
            .get("rgba")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("gif frame {i} needs rgba"))?;
        let rgba = base64::engine::general_purpose::STANDARD
            .decode(rgba_b64)
            .map_err(|e| format!("gif frame {i} bad base64: {e}"))?;
        if rgba.len() < need {
            return err(format!(
                "gif frame {i}: {need} bytes expected, got {}",
                rgba.len()
            ));
        }
        let buf = RgbaImage::from_raw(width, height, rgba[..need].to_vec())
            .ok_or_else(|| format!("gif frame {i}: cannot build image from pixels"))?;
        let frame = Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
        enc.encode_frame(frame)
            .map_err(|e| format!("gif encode failed: {e}"))?;
    }

    drop(enc);
    Ok(out.into_inner())
}

// transforms
// ---------------------------------------------------------------------------

pub fn filter_by_name(name: &str) -> FilterType {
    match name.trim().to_ascii_lowercase().as_str() {
        "nearest" | "point" => FilterType::Nearest,
        "triangle" | "bilinear" | "linear" => FilterType::Triangle,
        "catmullrom" | "bicubic" | "cubic" => FilterType::CatmullRom,
        "gaussian" => FilterType::Gaussian,
        _ => FilterType::Lanczos3,
    }
}

pub fn resize(img: &Image, w: u32, h: u32, filter: &str) -> R<Image> {
    if w == 0 || h == 0 {
        return err("resize: width and height must be > 0");
    }
    Ok(Image {
        buf: image::imageops::resize(&img.buf, w, h, filter_by_name(filter)),
    })
}

/// Largest size that fits inside `max_w` x `max_h` keeping the aspect ratio.
/// Never upscales — the thumbnail contract.
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }
    let s = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    (
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
    )
}

pub fn thumbnail(img: &Image, max_w: u32, max_h: u32) -> R<Image> {
    let (w, h) = fit_size(img.width(), img.height(), max_w, max_h);
    resize(img, w, h, "lanczos3")
}

pub fn crop(img: &Image, x: i64, y: i64, w: u32, h: u32) -> R<Image> {
    if w == 0 || h == 0 {
        return err("crop: width and height must be > 0");
    }
    // PIL allows a crop box outside the image; the outside is transparent.
    let mut out = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    for (dy, row) in (0..h).enumerate() {
        for (dx, col) in (0..w).enumerate() {
            let sx = x + col as i64;
            let sy = y + row as i64;
            if sx >= 0 && sy >= 0 && (sx as u32) < img.width() && (sy as u32) < img.height() {
                out.put_pixel(
                    dx as u32,
                    dy as u32,
                    *img.buf.get_pixel(sx as u32, sy as u32),
                );
            }
        }
    }
    Ok(Image { buf: out })
}

pub fn flip(img: &Image, mode: &str) -> R<Image> {
    let buf = match mode.trim().to_ascii_lowercase().as_str() {
        "h" | "horizontal" | "left_right" => image::imageops::flip_horizontal(&img.buf),
        "v" | "vertical" | "top_bottom" => image::imageops::flip_vertical(&img.buf),
        other => return err(format!("unknown flip mode: {other}")),
    };
    Ok(Image { buf })
}

/// Rotate by an arbitrary angle (degrees, counter-clockwise like PIL). Exact
/// multiples of 90° use the lossless integer path; anything else is resampled
/// through tiny-skia with a bilinear filter.
pub fn rotate(img: &Image, degrees: f64, expand: bool, bg: Rgba) -> R<Image> {
    let norm = ((degrees % 360.0) + 360.0) % 360.0;
    if (norm - norm.round()).abs() < 1e-9 && (norm.round() as i64) % 90 == 0 {
        let buf = match norm.round() as i64 {
            0 => img.buf.clone(),
            90 => image::imageops::rotate270(&img.buf),
            180 => image::imageops::rotate180(&img.buf),
            _ => image::imageops::rotate90(&img.buf),
        };
        return Ok(Image { buf });
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let rad = (norm as f32).to_radians();
    let (sin, cos) = (rad.sin().abs(), rad.cos().abs());
    let (out_w, out_h) = if expand {
        (
            (w * cos + h * sin).ceil().max(1.0),
            (w * sin + h * cos).ceil().max(1.0),
        )
    } else {
        (w, h)
    };
    let mut pm = tiny_skia::Pixmap::new(out_w as u32, out_h as u32)
        .ok_or_else(|| "cannot allocate rotation target".to_string())?;
    if bg.3 > 0 {
        pm.fill(bg.to_color());
    }
    let src = img.to_pixmap()?;
    let ts = tiny_skia::Transform::from_translate(out_w / 2.0, out_h / 2.0)
        .pre_rotate(-(norm as f32))
        .pre_translate(-w / 2.0, -h / 2.0);
    pm.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        ts,
        None,
    );
    Image::from_pixmap(&pm)
}

/// Composite `src` onto `dst` at (x,y). `blend` picks source-over (PIL's
/// `paste` with a mask) versus a straight overwrite (`paste` without one).
pub fn paste(dst: &mut Image, src: &Image, x: i64, y: i64, blend: bool) {
    if blend {
        image::imageops::overlay(&mut dst.buf, &src.buf, x, y);
    } else {
        image::imageops::replace(&mut dst.buf, &src.buf, x, y);
    }
}

pub fn flatten(img: &Image, bg: Rgba) -> Image {
    let mut out = RgbaImage::from_pixel(
        img.width(),
        img.height(),
        image::Rgba([bg.0, bg.1, bg.2, bg.3]),
    );
    image::imageops::overlay(&mut out, &img.buf, 0, 0);
    Image { buf: out }
}

pub fn grayscale(img: &Image) -> Image {
    let mut out = img.buf.clone();
    for p in out.pixels_mut() {
        let [r, g, b, a] = p.0;
        let l = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32)
            .round()
            .clamp(0.0, 255.0) as u8;
        p.0 = [l, l, l, a];
    }
    Image { buf: out }
}

pub fn invert(img: &Image) -> Image {
    let mut out = img.buf.clone();
    for p in out.pixels_mut() {
        p.0 = [255 - p.0[0], 255 - p.0[1], 255 - p.0[2], p.0[3]];
    }
    Image { buf: out }
}

pub fn blur(img: &Image, sigma: f32) -> Image {
    Image {
        buf: image::imageops::blur(&img.buf, sigma.max(0.0)),
    }
}

/// Brightness / contrast / saturation / opacity in one pass, each neutral at
/// its identity so a partial spec never surprises.
pub fn adjust(img: &Image, brightness: f32, contrast: f32, saturation: f32, opacity: f32) -> Image {
    let mut out = img.buf.clone();
    for p in out.pixels_mut() {
        let mut c = [p.0[0] as f32, p.0[1] as f32, p.0[2] as f32];
        for v in c.iter_mut() {
            *v *= brightness;
            *v = (*v - 128.0) * contrast + 128.0;
        }
        if (saturation - 1.0).abs() > f32::EPSILON {
            let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            for v in c.iter_mut() {
                *v = l + (*v - l) * saturation;
            }
        }
        p.0 = [
            c[0].clamp(0.0, 255.0) as u8,
            c[1].clamp(0.0, 255.0) as u8,
            c[2].clamp(0.0, 255.0) as u8,
            (p.0[3] as f32 * opacity).clamp(0.0, 255.0) as u8,
        ];
    }
    Image { buf: out }
}

// ---------------------------------------------------------------------------
// one-shot convert (the "make this safe to ship" path)
// ---------------------------------------------------------------------------

/// Decode -> optional fit/resize -> optional flatten -> encode, in one call.
/// This is what a provider-safe attachment pipeline needs and the reason a
/// caller never has to hold a decoded handle at all.
///
/// opts: {"format":"png","quality":85,"max_width":N,"max_height":N,
///        "width":N,"height":N,"background":"#fff","flatten":bool,
///        "filter":"lanczos3","max_bytes":N}
pub fn convert(data: &[u8], opts: &serde_json::Value) -> R<Vec<u8>> {
    use crate::util::{jb, jcolor, jf, js};

    let mut img = decode(data)?;
    let filter = js(opts, "filter").unwrap_or("lanczos3");

    if let (Some(w), Some(h)) = (jf(opts, "width"), jf(opts, "height")) {
        img = resize(&img, w as u32, h as u32, filter)?;
    } else {
        let max_w = jf(opts, "max_width").unwrap_or(f64::INFINITY);
        let max_h = jf(opts, "max_height").unwrap_or(f64::INFINITY);
        if max_w.is_finite() || max_h.is_finite() {
            let (w, h) = fit_size(
                img.width(),
                img.height(),
                if max_w.is_finite() {
                    max_w as u32
                } else {
                    u32::MAX
                },
                if max_h.is_finite() {
                    max_h as u32
                } else {
                    u32::MAX
                },
            );
            if (w, h) != (img.width(), img.height()) {
                img = resize(&img, w, h, filter)?;
            }
        }
    }

    let bg = jcolor(opts, "background");
    if jb(opts, "flatten", false) || bg.is_some() {
        img = flatten(&img, bg.unwrap_or(Rgba::WHITE));
    }

    let format = js(opts, "format").unwrap_or("png");
    let quality = jf(opts, "quality").unwrap_or(85.0).clamp(1.0, 100.0) as u8;
    let mut out = encode(&img, format, quality)?;

    // Optional byte ceiling: step the quality (JPEG/WebP-ish formats) and then
    // the geometry down until it fits, so a caller-side limit is a guarantee
    // rather than a hope.
    if let Some(max_bytes) = jf(opts, "max_bytes") {
        let max_bytes = max_bytes as usize;
        let mut q = quality;
        while out.len() > max_bytes && q > 30 {
            q = q.saturating_sub(15).max(30);
            out = encode(&img, format, q)?;
        }
        while out.len() > max_bytes && img.width() > 64 && img.height() > 64 {
            img = resize(&img, img.width() * 3 / 4, img.height() * 3 / 4, filter)?;
            out = encode(&img, format, q)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1100x600 HEIC that macOS `sips` stored as a 3x2 grid of 512px tiles
    /// with an alpha plane: the layout of an iPhone photo.
    fn heic() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/grid-alpha.heic"
        ))
        .expect("test fixture grid-alpha.heic")
    }

    /// An `ftyp` box with `major` first and `compatible` after it.
    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compatible.len() as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 0]);
        compatible.iter().for_each(|b| v.extend_from_slice(&b[..]));
        v
    }

    #[test]
    fn heif_brands_are_recognised() {
        assert!(is_heif(&heic()));
        assert!(is_heif(&ftyp(b"mif1", &[b"mif1", b"heic"])));
        assert!(is_heif(&ftyp(b"msf1", &[b"msf1", b"hevc"])));
        assert!(is_heif(&ftyp(b"avif", &[b"mif1", b"miaf"])));
        assert!(is_heif(&ftyp(b"avci", &[b"mif1", b"miaf"])));
        assert!(is_heif(&ftyp(b"jpeg", &[b"mif1", b"jpeg"])));
        assert!(!is_heif(&ftyp(b"isom", &[b"isom", b"avc1", b"mp41"])));
        assert!(!is_heif(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn heic_probes_and_decodes_through_the_shared_entry_points() {
        let data = heic();
        let p = probe(&data).unwrap();
        assert_eq!(p["format"], "heic");
        assert_eq!(
            (p["width"].as_u64(), p["height"].as_u64()),
            (Some(1100), Some(600))
        );
        assert_eq!(p["has_alpha"], true);
        let img = decode(&data).unwrap();
        assert_eq!((img.width(), img.height()), (1100, 600));
        let alpha = |x, y| img.buf.get_pixel(x, y).0[3] as i32;
        assert!((alpha(50, 50) - 140).abs() <= 6);
        assert!((alpha(150, 50) - 255).abs() <= 6);
    }

    #[test]
    fn a_cut_heic_is_an_error_not_a_panic() {
        let data = heic();
        assert!(decode(&data[..600]).is_err());
        assert!(probe(&data[..40]).is_err());
    }
}
