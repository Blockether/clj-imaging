//! Optimize only when the candidate passes the format's lossless checks.

use std::io::Cursor;

use png::{BitDepth, ColorType, Reader};
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

fn png_reader(data: &[u8]) -> Option<Reader<Cursor<&[u8]>>> {
    let mut checks = png::DecodeOptions::default();
    checks.set_ignore_adler32(false);
    checks.set_skip_ancillary_crc_failures(false);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(data), checks);
    decoder.set_transformations(png::Transformations::ALPHA);
    // Ancillary bytes are checked separately. Do not inflate text or profiles.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.read_info().ok()
}

fn png_samples(mut reader: Reader<Cursor<&[u8]>>) -> Option<Vec<u8>> {
    let mut samples = vec![0; reader.output_buffer_size()?];
    reader.next_frame(&mut samples).ok()?;
    reader.finish().ok()?;
    Some(samples)
}

/// Compare color-type reductions and bit-depth reductions after exact widening.
/// The caller uses byte equality for the common case with the same layout.
fn rgba16(data: &[u8], (kind, depth): (ColorType, BitDepth)) -> impl Iterator<Item = [u16; 4]> {
    let wide = depth == BitDepth::Sixteen;
    let stride = kind.samples() * if wide { 2 } else { 1 };
    let sample = move |p: &[u8], i: usize| {
        if wide {
            u16::from_be_bytes([p[2 * i], p[2 * i + 1]])
        } else {
            u16::from(p[i]) * 257
        }
    };
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

fn same_png_samples(before: &[u8], after: &[u8]) -> Option<bool> {
    let mut before = png_reader(before)?;
    let mut after = png_reader(after)?;
    let (width, height) = before.info().size();
    if (width, height) != after.info().size() {
        return Some(false);
    }
    let a_type = before.output_color_type();
    let b_type = after.output_color_type();
    let same = |a: &[u8], b: &[u8]| {
        if a_type == b_type {
            a == b
        } else {
            rgba16(a, a_type).eq(rgba16(b, b_type))
        }
    };

    // Adam7 scanlines are in pass order, not image order. Deinterlace first
    // if either image uses Adam7; otherwise retain just a row per image.
    if before.info().interlaced || after.info().interlaced {
        return Some(same(&png_samples(before)?, &png_samples(after)?));
    }
    let mut a = vec![0; before.output_line_size(width)?];
    let mut b = vec![0; after.output_line_size(width)?];
    for _ in 0..height {
        before.read_row(&mut a).ok()??;
        after.read_row(&mut b).ok()??;
        if !same(&a, &b) {
            return Some(false);
        }
    }
    // Read to the end of IDAT, including the Adler-32 checksum. Then check
    // any remaining chunks through IEND.
    if before.read_row(&mut a).ok()?.is_some() || after.read_row(&mut b).ok()?.is_some() {
        return None;
    }
    before.finish().ok()?;
    after.finish().ok()?;
    Some(true)
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
    same_png_samples(data, &out)?.then_some(out)
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

/// Return a smaller image, or `None` to keep the input (also on error or panic).
pub fn optimize(data: &[u8], png_opts: impl FnOnce() -> oxipng::Options) -> Option<Vec<u8>> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png_image(
        kind: ColorType,
        depth: BitDepth,
        interlaced: bool,
        samples: &[u8],
        bad_adler: bool,
    ) -> Vec<u8> {
        let mut info = png::Info::with_size(2, 2);
        info.color_type = kind;
        info.bit_depth = depth;
        info.interlaced = interlaced;
        // The fixture supplies filtered scanlines in either image or Adam7 order.
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        zlib.write_all(samples).unwrap();
        let mut idat = zlib.finish().unwrap();
        if bad_adler {
            *idat.last_mut().unwrap() ^= 1;
        }
        let mut bytes = Vec::new();
        let mut writer = png::Encoder::with_info(&mut bytes, info)
            .unwrap()
            .write_header()
            .unwrap();
        writer.write_chunk(png::chunk::IDAT, &idat).unwrap();
        writer.write_chunk(png::chunk::tEXt, b"key\0value").unwrap();
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn streaming_png_rejects_changes_and_bad_checksums() {
        let make = |samples, bad_adler| {
            png_image(
                ColorType::Grayscale,
                BitDepth::Eight,
                false,
                samples,
                bad_adler,
            )
        };
        let samples = &[0, 10, 20, 0, 30, 40];
        let original = make(samples, false);
        assert_eq!(same_png_samples(&original, &original), Some(true));
        let changed = make(&[0, 10, 20, 0, 30, 41], false);
        assert_eq!(same_png_samples(&original, &changed), Some(false));
        assert_eq!(same_png_samples(&original, &make(samples, true)), None);
        let mut bad_crc = original.clone();
        let crc_end = bad_crc.len() - 12; // tEXt CRC comes right before IEND.
        bad_crc[crc_end - 1] ^= 1;
        assert_eq!(same_png_samples(&original, &bad_crc), None);
    }

    #[test]
    fn streaming_png_compares_exactly_across_color_and_bit_depth_reductions() {
        let gray = png_image(
            ColorType::Grayscale,
            BitDepth::Eight,
            false,
            &[0, 10, 20, 0, 30, 40],
            false,
        );
        let mut rgb16_samples = vec![0];
        for (i, sample) in [10, 20, 30, 40].into_iter().enumerate() {
            if i == 2 {
                rgb16_samples.push(0);
            }
            rgb16_samples.extend_from_slice(&[sample; 6]);
        }
        let make =
            |samples: &[u8]| png_image(ColorType::Rgb, BitDepth::Sixteen, false, samples, false);
        assert_eq!(same_png_samples(&gray, &make(&rgb16_samples)), Some(true));
        rgb16_samples[1] ^= 1;
        assert_eq!(same_png_samples(&gray, &make(&rgb16_samples)), Some(false));
    }

    #[test]
    fn interlaced_png_compares_in_image_order() {
        let make = |interlaced, samples| {
            png_image(
                ColorType::Grayscale,
                BitDepth::Eight,
                interlaced,
                samples,
                false,
            )
        };
        let original = make(false, &[0, 10, 20, 0, 30, 40]);
        // Passes 1, 6 and 7 of a 2x2 Adam7 image, in order.
        let interlaced = make(true, &[0, 10, 0, 20, 0, 30, 40]);
        let changed = make(true, &[0, 10, 0, 20, 0, 30, 41]);
        assert_eq!(same_png_samples(&original, &interlaced), Some(true));
        assert_eq!(same_png_samples(&interlaced, &original), Some(true));
        assert_eq!(same_png_samples(&interlaced, &changed), Some(false));
    }
}
