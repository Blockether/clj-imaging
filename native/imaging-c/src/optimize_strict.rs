//! Optimize only when the candidate passes the format's lossless checks.

use std::io::Cursor;

use png::{BitDepth, ColorType, OutputInfo};
use tzim::formats::{Allow, Cand, Options, jpeg};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
// Bound work and allocations before starting either background optimizer.
const MAX_PIXELS: u64 = 16_000_000;

type PngMetadata<'a> = Vec<(bool, &'a [u8])>;

/// Record complete ancillary chunks in order, including their position before
/// or after IDAT. An indexed PLTE and tRNS are sample data; the decoder checks
/// them below. For truecolor PNG, PLTE is an optional suggested palette.
fn png_metadata(data: &[u8]) -> Option<PngMetadata<'_>> {
    let mut rest = data.strip_prefix(PNG_SIGNATURE)?;
    if rest.get(..8)? != b"\0\0\0\rIHDR" {
        return None;
    }
    let width = u32::from_be_bytes(rest.get(8..12)?.try_into().ok()?);
    let height = u32::from_be_bytes(rest.get(12..16)?.try_into().ok()?);
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let mut metadata = Vec::new();
    let mut ihdr = false;
    let mut idat = false;
    loop {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let (chunk, tail) = rest.split_at_checked(12usize.checked_add(len)?)?;
        match &chunk[4..8] {
            b"IEND" if len == 0 && idat && tail.is_empty() => return Some(metadata),
            b"acTL" | b"fcTL" | b"fdAT" | b"IEND" => return None,
            b"IHDR" if !ihdr => ihdr = true,
            b"IDAT" => idat = true,
            b"PLTE" if data[25] != 3 => metadata.push((idat, chunk)),
            b"PLTE" | b"tRNS" => (),
            // These are known chunks, even if the safe-to-copy bit is off.
            b"gAMA" | b"cHRM" | b"sRGB" | b"iCCP" | b"eXIf" | b"sBIT" | b"bKGD" | b"pHYs"
            | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME" | b"sPLT" => {
                metadata.push((idat, chunk));
            }
            // Unknown unsafe-to-copy chunks may depend on the encoding.
            kind if kind[0].is_ascii_lowercase() && kind[3].is_ascii_lowercase() => {
                metadata.push((idat, chunk));
            }
            _ => return None,
        }
        rest = tail;
    }
}

fn png_samples(data: &[u8]) -> Option<(OutputInfo, Vec<u8>)> {
    let mut checks = png::DecodeOptions::default();
    checks.set_ignore_adler32(false);
    checks.set_skip_ancillary_crc_failures(false);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(data), checks);
    decoder.set_transformations(png::Transformations::ALPHA);
    // Ancillary bytes are checked separately. Do not inflate text or profiles.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder.read_info().ok()?;
    let mut samples = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut samples).ok()?;
    reader.finish().ok()?;
    Some((info, samples))
}

/// Compare color-type reductions and bit-depth reductions after exact widening.
/// The caller uses byte equality for the common case with the same layout.
fn rgba16(data: &[u8], info: &OutputInfo) -> impl Iterator<Item = [u16; 4]> {
    let wide = info.bit_depth == BitDepth::Sixteen;
    let stride = info.color_type.samples() * if wide { 2 } else { 1 };
    let sample = move |p: &[u8], i: usize| {
        if wide {
            u16::from_be_bytes([p[2 * i], p[2 * i + 1]])
        } else {
            u16::from(p[i]) * 257
        }
    };
    let kind = info.color_type;
    data.chunks_exact(stride).map(move |p| {
        let first = sample(p, 0);
        match kind {
            ColorType::Grayscale => [first, first, first, u16::MAX],
            ColorType::GrayscaleAlpha => [first, first, first, sample(p, 1)],
            ColorType::Rgb => [first, sample(p, 1), sample(p, 2), u16::MAX],
            ColorType::Rgba => [first, sample(p, 1), sample(p, 2), sample(p, 3)],
            ColorType::Indexed => unreachable!("ALPHA expands the palette"),
        }
    })
}

fn png(data: &[u8], mut opts: oxipng::Options) -> Option<Vec<u8>> {
    let metadata = png_metadata(data)?;
    opts.max_decompressed_size = Some(MAX_PIXELS as usize * 9);
    opts.optimize_alpha = false;
    for &(_, chunk) in &metadata {
        match &chunk[4..8] {
            b"iCCP" => opts.grayscale_reduction = false,
            b"sBIT" | b"bKGD" | b"PLTE" => {
                opts.bit_depth_reduction = false;
                opts.color_type_reduction = false;
                opts.grayscale_reduction = false;
                opts.palette_reduction = false;
            }
            _ => (),
        }
    }
    let out = oxipng::optimize_from_memory(data, &opts).ok()?;
    if out.len() >= data.len() || png_metadata(&out)? != metadata {
        return None;
    }
    // Significant bits and background use the IHDR's sample representation.
    // Indexed backgrounds also refer to the palette. Keep those bytes intact.
    if metadata
        .iter()
        .any(|(_, c)| matches!(&c[4..8], b"sBIT" | b"bKGD"))
        && (out[24..26] != data[24..26] || data[25] == 3)
    {
        return None;
    }
    let (before, a) = png_samples(data)?;
    let (after, b) = png_samples(&out)?;
    if (before.width, before.height) != (after.width, after.height) {
        return None;
    }
    let same = if before == after {
        a == b
    } else {
        rgba16(&a, &before).eq(rgba16(&b, &after))
    };
    same.then_some(out)
}

/// Refuse unknown APP markers and multi-picture files. A byte-for-byte marker
/// copy alone is not enough when the payload stores offsets into the JPEG.
fn jpeg_supported(data: &[u8]) -> Option<()> {
    let mut rest = data.strip_prefix(b"\xff\xd8")?;
    loop {
        rest = rest.strip_prefix(b"\xff")?;
        while let Some(tail) = rest.strip_prefix(b"\xff") {
            rest = tail;
        }
        let (marker, rest_after_marker) = rest.split_first()?;
        let len = u16::from_be_bytes(rest_after_marker.get(..2)?.try_into().ok()?) as usize;
        let (segment, tail) = rest_after_marker.split_at_checked(len)?;
        let payload = segment.get(2..)?;
        match marker {
            0xda => return jpeg_scan_ends_at_eoi(tail),
            0xc0 | 0xc4 | 0xdb | 0xdd | 0xfe => (),
            0xe0 if payload.starts_with(b"JFIF\0") || payload.starts_with(b"JFXX\0") => (),
            0xe1 if payload.starts_with(b"Exif\0\0") => (),
            0xe2 if payload.starts_with(b"ICC_PROFILE\0") => (),
            0xee if payload.starts_with(b"Adobe") => (),
            _ => return None,
        }
        rest = tail;
    }
}

fn jpeg_scan_ends_at_eoi(mut scan: &[u8]) -> Option<()> {
    loop {
        let end = scan.iter().position(|&b| b == 0xff)?;
        scan = &scan[end + 1..];
        let (&marker, tail) = scan.split_first()?;
        match marker {
            0xd9 => return tail.is_empty().then_some(()),
            0x00 | 0xd0..=0xd7 => scan = tail,
            0xff => (),
            _ => return None,
        }
    }
}

fn jpeg(data: &[u8]) -> Option<Vec<u8>> {
    jpeg_supported(data)?;
    let opts = Options {
        max_pixels: MAX_PIXELS,
        strip_metadata: false,
        zopfli: false,
        max_uncompressed: 0,
        max_ratio: 0,
        nested: false,
        max_depth: 0,
        depth: 0,
        allow: Allow::default(),
        fits_tile: false,
    };
    let mut out = Vec::new();
    match jpeg::optimize(data, &mut out, &opts).ok()? {
        Cand::Written(_) if out.len() < data.len() => {
            // Verify markers, coefficients and decoded pixels independently.
            jpeg::verify(data, &out, &opts).ok()?;
            Some(out)
        }
        _ => None,
    }
}

/// A codec error or panic must not prevent the caller from keeping the input.
pub fn optimize(data: &[u8], png_opts: impl FnOnce() -> oxipng::Options) -> Vec<u8> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if data.starts_with(PNG_SIGNATURE) {
            png(data, png_opts())
        } else if data.starts_with(b"\xff\xd8") {
            jpeg(data)
        } else {
            None
        }
    }))
    .ok()
    .flatten()
    .unwrap_or_else(|| data.to_vec())
}
