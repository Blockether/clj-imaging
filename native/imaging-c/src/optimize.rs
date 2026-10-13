//! Real, library-backed byte-size optimisation — the thing "save it smaller"
//! actually means.
//!
//! Nothing here is a hand-rolled approximation of a C tool:
//!
//! * **PNG** goes through [`oxipng`] (the Rust rewrite of OptiPNG): filter
//!   search, bit-depth / colour-type / palette reduction, alpha optimisation,
//!   IDAT recompression with libdeflate, and Zopfli at the top level. In lossy
//!   mode the image is first palette-reduced by [`crate::quant`] (median cut +
//!   Floyd–Steinberg) and handed to oxipng as an indexed `RawImage`, which is
//!   how pngquant-class savings are reached without leaving Rust.
//! * **JPEG** is optimised LOSSLESSLY by default — the metadata segments go
//!   (jpegtran's `-copy none`) and every DCT coefficient is copied byte for
//!   byte. A caller who asks for `lossy`, a `quality` or a `max_bytes` it
//!   cannot otherwise meet gets a re-encode through [`jpeg_encoder`] with
//!   optimised Huffman tables and progressive scans — the mozjpeg tricks that
//!   are pure Rust.
//! * **GIF** is rebuilt with the [`gif`] crate the way gifsicle does it:
//!   per-frame median-cut palettes, inter-frame differencing (unchanged pixels
//!   become the transparent index) and cropping to the changed bounding box.
//! * Everything else round-trips through [`crate::imgops`].
//!
//! Optimising never makes a file bigger: when the output format matches the
//! input and the result is not smaller, the original bytes are returned
//! untouched (unless `force` is set).

use base64::Engine;
use serde_json::Value;

use crate::imgops::{self, Image};
use crate::quant;
use crate::util::{R, Rgba, err, jb, jf, js};

/// How many quality/colour/scale steps `max_bytes` may take before giving up.
const MAX_STEPS: usize = 14;

/// Canonical format key: what `probe` reports and what callers pass.
fn normalise(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg".to_string(),
        "tif" | "tiff" => "tiff".to_string(),
        other => other.to_string(),
    }
}

fn png_options(level: u8, strip: bool) -> oxipng::Options {
    let mut opts = oxipng::Options::from_preset(level);
    opts.strip = if strip {
        oxipng::StripChunks::Safe
    } else {
        oxipng::StripChunks::None
    };
    // Interlacing costs ~15% for no benefit in a bot pipeline.
    opts.interlace = Some(false);
    // Rewrites the colour of fully transparent pixels so they compress; the
    // rendered image is bit-identical.
    opts.optimize_alpha = true;
    if level >= 6 {
        opts.deflater = oxipng::Deflater::Zopfli(oxipng::ZopfliOptions::default());
    }
    opts
}

/// Lossless PNG: recompress the existing bytes when we have them, otherwise
/// encode first. oxipng does the rest.
fn png_lossless(src: Option<&[u8]>, img: &Image, opts: &oxipng::Options) -> R<Vec<u8>> {
    let bytes = match src {
        Some(b) => b.to_vec(),
        None => imgops::encode(img, "png", 0)?,
    };
    oxipng::optimize_from_memory(&bytes, opts).map_err(|e| format!("oxipng failed: {e}"))
}

/// Lossy PNG: median-cut palette, then oxipng over an indexed raw image.
fn png_lossy(img: &Image, colors: usize, dither: bool, opts: &oxipng::Options) -> R<Vec<u8>> {
    let q = quant::quantize(img, colors.clamp(2, 256), dither);
    let palette: Vec<oxipng::RGBA8> = q
        .palette
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let a = if Some(i) == q.transparent { 0 } else { 255 };
            oxipng::RGBA8::new(
                ((c >> 16) & 0xff) as u8,
                ((c >> 8) & 0xff) as u8,
                (c & 0xff) as u8,
                a,
            )
        })
        .collect();
    let raw = oxipng::RawImage::new(
        q.width,
        q.height,
        oxipng::ColorType::Indexed { palette },
        oxipng::BitDepth::Eight,
        q.indices,
    )
    .map_err(|e| format!("oxipng raw image failed: {e}"))?;
    raw.create_optimized_png(opts)
        .map_err(|e| format!("oxipng failed: {e}"))
}

/// JPEG with optimised Huffman tables (and progressive scans by default) —
/// typically 5-20% under a baseline encoder at the same quality.
fn jpeg_bytes(img: &Image, quality: u8, progressive: bool, background: Rgba) -> R<Vec<u8>> {
    let (w, h) = (img.width(), img.height());
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        return err("jpeg: dimensions above 65535 px");
    }
    let flat = imgops::flatten(img, background);
    let rgb: Vec<u8> = flat
        .buf
        .pixels()
        .flat_map(|p| [p.0[0], p.0[1], p.0[2]])
        .collect();
    let mut out: Vec<u8> = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_optimized_huffman_tables(true);
    enc.set_progressive(progressive);
    if quality >= 90 {
        // No chroma loss where the caller asked for near-original quality.
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    }
    enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| format!("jpeg encode failed: {e}"))?;
    Ok(out)
}

/// Lossless JPEG: drop the metadata segments and keep every scan byte —
/// jpegtran's `-copy none`, with no decode/re-encode round trip, so not one DCT
/// coefficient moves. JFIF (APP0), ICC (APP2) and Adobe (APP14) stay: those
/// change how the image is DISPLAYED. Anything that does not parse as a clean
/// marker stream is handed back untouched.
fn jpeg_strip(data: &[u8], strip: bool) -> R<Vec<u8>> {
    if !strip || data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Ok(data.to_vec());
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]);
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return Ok(data.to_vec());
        }
        let marker = data[i + 1];
        // Fill bytes and the standalone markers carry no length.
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            out.extend_from_slice(&data[i..i + 2]);
            i += 2;
            if marker == 0xD9 {
                break;
            }
            continue;
        }
        if i + 3 >= data.len() {
            return Ok(data.to_vec());
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 || i + 2 + len > data.len() {
            return Ok(data.to_vec());
        }
        let end = i + 2 + len;
        // COM, EXIF/XMP (APP1), Photoshop/IPTC (APP3-APP13) and APP15 are
        // pure metadata.
        let drop = matches!(marker, 0xFE | 0xE1 | 0xE3..=0xED | 0xEF);
        if !drop {
            out.extend_from_slice(&data[i..end]);
        }
        i = end;
        if marker == 0xDA {
            // Start of scan: entropy-coded data (and any further scans) to the
            // end of the file, copied byte for byte.
            out.extend_from_slice(&data[i..]);
            break;
        }
    }
    if out.len() >= 4 {
        Ok(out)
    } else {
        Ok(data.to_vec())
    }
}

/// The scale that fits an image inside `max_width` x `max_height`. Never
/// enlarges: a smaller image is left alone.
fn fit_scale(img: &Image, o: &Value) -> f64 {
    let mut s = 1.0f64;
    if let Some(mw) = jf(o, "max_width").filter(|v| *v > 0.0) {
        s = s.min(mw / img.width() as f64);
    }
    if let Some(mh) = jf(o, "max_height").filter(|v| *v > 0.0) {
        s = s.min(mh / img.height() as f64);
    }
    s.clamp(0.0001, 1.0)
}

/// Palette bytes for the GIF colour table, padded to the power of two GIF
/// requires. Returns the RGB triples and the padded entry count.
fn gif_palette(palette: &[u32], extra: usize) -> (Vec<u8>, usize) {
    let want = (palette.len() + extra).clamp(2, 256);
    let mut size = 2usize;
    while size < want {
        size *= 2;
    }
    let mut bytes = Vec::with_capacity(size * 3);
    for &c in palette {
        bytes.push(((c >> 16) & 0xff) as u8);
        bytes.push(((c >> 8) & 0xff) as u8);
        bytes.push((c & 0xff) as u8);
    }
    bytes.resize(size * 3, 0);
    (bytes, size)
}

/// Rebuild a GIF the way gifsicle does: per-frame palettes, unchanged pixels
/// dropped to the transparent index, and each frame cropped to what moved.
fn optimize_gif(data: &[u8], colors: usize, dither: bool) -> R<Vec<u8>> {
    let spec = imgops::decode_gif(data)?;
    let w = spec.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let h = spec.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if w == 0 || h == 0 {
        return err("gif has no dimensions");
    }
    let loop_count = spec
        .get("loop_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let frames = spec
        .get("frames")
        .and_then(|v| v.as_array())
        .ok_or("gif has no frames")?;
    let stride = w as usize * 4;

    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, w as u16, h as u16, &[])
            .map_err(|e| format!("gif encode failed: {e}"))?;
        enc.set_repeat(if loop_count < 0 {
            gif::Repeat::Infinite
        } else {
            gif::Repeat::Finite(loop_count.clamp(0, u16::MAX as i64) as u16)
        })
        .map_err(|e| format!("gif encode failed: {e}"))?;

        let mut prev: Option<Vec<u8>> = None;
        for fv in frames {
            let b64 = fv.get("rgba").and_then(|v| v.as_str()).unwrap_or("");
            let rgba = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("gif frame is not base64: {e}"))?;
            if rgba.len() < stride * h as usize {
                return err("gif frame is smaller than the canvas");
            }
            let delay_ms = fv.get("delay_ms").and_then(|v| v.as_i64()).unwrap_or(100);
            let img = Image::from_rgba(w, h, &rgba)?;
            // Leave one palette slot for transparency.
            let q = quant::quantize(&img, colors.clamp(2, 255), dither);
            let mut indices = q.indices;
            let (mut palette, size) = gif_palette(&q.palette, 1);
            let transparent = match q.transparent {
                Some(i) => i as u8,
                None if q.palette.len() < size => q.palette.len() as u8,
                None => (size - 1) as u8,
            };
            if palette.len() < size * 3 {
                palette.resize(size * 3, 0);
            }

            // Difference against the previous canvas: identical pixels become
            // transparent, and the frame shrinks to the changed rectangle.
            let (mut x0, mut y0, mut x1, mut y1) = (0u32, 0u32, w, h);
            if let Some(p) = prev.as_ref() {
                let (mut mnx, mut mny, mut mxx, mut mxy) = (u32::MAX, u32::MAX, 0u32, 0u32);
                for y in 0..h {
                    for x in 0..w {
                        let o = y as usize * stride + x as usize * 4;
                        if rgba[o..o + 4] == p[o..o + 4] {
                            indices[y as usize * w as usize + x as usize] = transparent;
                        } else {
                            mnx = mnx.min(x);
                            mny = mny.min(y);
                            mxx = mxx.max(x);
                            mxy = mxy.max(y);
                        }
                    }
                }
                if mnx == u32::MAX {
                    // Nothing moved: a 1x1 transparent frame just holds the delay.
                    mnx = 0;
                    mny = 0;
                    mxx = 0;
                    mxy = 0;
                }
                x0 = mnx;
                y0 = mny;
                x1 = mxx + 1;
                y1 = mxy + 1;
            }

            let (fw, fh) = ((x1 - x0) as usize, (y1 - y0) as usize);
            let mut buffer = Vec::with_capacity(fw * fh);
            for y in y0..y1 {
                let row = y as usize * w as usize;
                buffer.extend_from_slice(&indices[row + x0 as usize..row + x1 as usize]);
            }

            let mut frame = gif::Frame {
                width: fw as u16,
                height: fh as u16,
                left: x0 as u16,
                top: y0 as u16,
                // GIF delays are centiseconds; never emit 0 (browsers force 100ms).
                delay: ((delay_ms + 5) / 10).clamp(1, u16::MAX as i64) as u16,
                dispose: gif::DisposalMethod::Keep,
                transparent: Some(transparent),
                palette: Some(palette),
                buffer: buffer.into(),
                ..gif::Frame::default()
            };
            frame.make_lzw_pre_encoded();
            enc.write_lzw_pre_encoded_frame(&frame)
                .map_err(|e| format!("gif encode failed: {e}"))?;
            prev = Some(rgba);
        }
    }
    Ok(out)
}

/// Optimise `data` in place of re-encoding it blindly.
///
/// LOSSLESS unless asked otherwise: PNG goes through oxipng, and a JPEG nobody
/// asked to re-encode only loses its metadata segments ([`jpeg_strip`]) — not
/// one DCT coefficient moves. Passing `lossy`, a `quality`, a smaller
/// `format`, a `max_width`/`max_height` or a `max_bytes` it cannot otherwise
/// meet is what licenses a re-encode.
///
/// Options: `format` (default: the input's), `level` 0-6 (oxipng preset, 6 =
/// Zopfli), `lossy` (palette-reduce PNG, re-encode JPEG), `colors` (2-256),
/// `dither`, `quality` (JPEG/WebP), `progressive`, `background` (JPEG alpha
/// flatten), `strip` metadata (default true), `max_width` / `max_height`
/// (downscale to fit, never enlarge), `max_bytes` (walk quality/colours/scale
/// down until it fits) and `force` (accept a bigger result).
pub fn optimize(data: &[u8], o: &Value) -> R<Vec<u8>> {
    if data.is_empty() {
        return err("optimize: empty input");
    }
    let probe = imgops::probe(data)?;
    let src_fmt = normalise(
        probe
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    );
    // Nothing here encodes HEIC, so a HEIC source without a `format` becomes a
    // JPEG: the format the same camera picture would otherwise have.
    let src_default = if src_fmt == "heic" {
        "jpeg".to_string()
    } else {
        src_fmt.clone()
    };
    let target = normalise(&js(o, "format").map(str::to_string).unwrap_or(src_default));
    let level = jf(o, "level").unwrap_or(3.0).clamp(0.0, 6.0) as u8;
    let strip = jb(o, "strip", true);
    let opts = png_options(level, strip);
    let progressive = jb(o, "progressive", true);
    let dither = jb(o, "dither", true);
    let background = crate::util::jcolor(o, "background").unwrap_or(Rgba::WHITE);
    let max_bytes = jf(o, "max_bytes").filter(|v| *v > 0.0).map(|v| v as usize);
    let same_format = target == src_fmt;
    // An explicit quality IS the request to re-encode; without one a JPEG is
    // never silently re-compressed behind the caller's back.
    let asked_quality = jf(o, "quality").is_some();

    // Animated GIFs are never flattened to one frame.
    if target == "gif" && src_fmt == "gif" {
        let colors = jf(o, "colors").unwrap_or(256.0).clamp(2.0, 256.0) as usize;
        let out = optimize_gif(data, colors, dither)?;
        return Ok(keep_smaller(data, out, same_format, jb(o, "force", false)));
    }

    let source = imgops::decode(data)?;
    // `max_width`/`max_height` bound the PIXELS: the very first attempt is
    // already inside that box, and a downscale is always kept — handing back
    // the bigger original because it happened to have fewer bytes would ignore
    // the constraint the caller actually set.
    let fit = fit_scale(&source, o);
    let force = jb(o, "force", false) || fit < 0.999;
    let mut lossy = jb(o, "lossy", false);
    let mut colors = jf(o, "colors").unwrap_or(256.0).clamp(2.0, 256.0) as usize;
    let mut quality = jf(o, "quality").unwrap_or(82.0).clamp(1.0, 100.0) as u8;
    let mut scale = fit;
    let mut best: Option<Vec<u8>> = None;

    for _ in 0..MAX_STEPS {
        let scaled;
        let img = if scale < 0.999 {
            let w = ((source.width() as f64 * scale).round() as u32).max(1);
            let h = ((source.height() as f64 * scale).round() as u32).max(1);
            scaled = imgops::resize(&source, w, h, "lanczos3")?;
            &scaled
        } else {
            &source
        };
        let untouched = scale >= 0.999 && src_fmt == "png";
        let out = match target.as_str() {
            "png" => {
                if lossy {
                    png_lossy(img, colors, dither, &opts)?
                } else {
                    png_lossless(if untouched { Some(data) } else { None }, img, &opts)?
                }
            }
            "jpeg" => {
                if same_format && !lossy && !asked_quality && scale >= 0.999 {
                    // Same JPEG, no licence to re-encode: metadata only.
                    jpeg_strip(data, strip)?
                } else {
                    jpeg_bytes(img, quality, progressive, background)?
                }
            }
            "gif" => {
                let q = quant::quantize(img, colors.clamp(2, 256), dither);
                let single = serde_json::json!({
                    "width": q.width,
                    "height": q.height,
                    "frames": [{
                        "delay_ms": 0,
                        "rgba": base64::engine::general_purpose::STANDARD.encode(&q.rgba),
                    }],
                });
                imgops::encode_gif(&single)?
            }
            other => imgops::encode(img, other, quality)?,
        };
        let fits = max_bytes.is_none_or(|m| out.len() <= m);
        if best.as_ref().is_none_or(|b| out.len() < b.len()) {
            best = Some(out);
        }
        if fits {
            break;
        }
        // Turn one knob per step: quality/palette first, resolution last.
        match target.as_str() {
            "png" if !lossy => lossy = true,
            "png" if colors > 8 => colors /= 2,
            // What a too-big JPEG gives up first is bit-exactness.
            "jpeg" if !lossy && !asked_quality => lossy = true,
            "jpeg" | "webp" if quality > 35 => quality = quality.saturating_sub(10),
            _ if colors > 8 && target == "gif" => colors /= 2,
            _ => {
                if scale <= 0.15 {
                    break;
                }
                scale *= 0.8;
            }
        }
    }

    let out = best.ok_or("optimize produced nothing")?;
    Ok(keep_smaller(data, out, same_format, force))
}

/// Same format and no win? Hand back the original bytes.
fn keep_smaller(original: &[u8], candidate: Vec<u8>, same_format: bool, force: bool) -> Vec<u8> {
    if !force && same_format && candidate.len() >= original.len() {
        original.to_vec()
    } else {
        candidate
    }
}
