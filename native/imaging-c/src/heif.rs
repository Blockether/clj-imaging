//! HEIF beyond HEVC. The container is the same for every codec, so `heic-rs`
//! still parses the boxes, joins grid tiles, converts colour and applies the
//! transforms. Only the coded pictures differ:
//!
//! * `hvc1`/`hev1` (HEIC): the whole file goes to `heic_rs::decode`, as before.
//! * `av01` (AVIF): `rav1d`, the pure-Rust port of dav1d, built without assembly.
//! * `avc1` (H.264): the vendored `rust_h264` that the video path also uses.
//! * `jpeg`: the `image` crate's JPEG decoder.
//! * `unci` (ISO/IEC 23001-17 uncompressed): read here, from `uncC` and `cmpd`.
//!
//! Every codec hands back a `heic_rs::hevc::Frame`, so a grid of AV1 tiles or a
//! JPEG with an alpha item takes the same path as a HEIC photo.

use std::mem::MaybeUninit;
use std::ptr::NonNull;

use heic_rs::context::Context;
use heic_rs::ftyp::{Brand, FileType};
use heic_rs::hevc::Frame;
use heic_rs::props::ItemProps;
use heic_rs::{ChromaFormat, DecodeOptions, MatrixCoefficients, Nclx, PixelLayout, Range};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
    DAV1D_PIXEL_LAYOUT_I444,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib as dav1d;
use rust_h264::decoder::Decoder;
use rust_h264::nal::{parse_avcc, parse_avcc_config};

use crate::util::{R, err};

/// The codec of one coded image item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Hevc,
    Av1,
    Avc,
    Jpeg,
    Uncompressed,
}

impl Codec {
    fn of(item_type: &[u8; 4]) -> R<Codec> {
        Ok(match item_type {
            b"hvc1" | b"hev1" => Codec::Hevc,
            b"av01" => Codec::Av1,
            b"avc1" => Codec::Avc,
            b"jpeg" => Codec::Jpeg,
            b"unci" => Codec::Uncompressed,
            other => {
                return err(format!(
                    "heif: image item type `{}` is not supported",
                    String::from_utf8_lossy(other)
                ));
            }
        })
    }

    /// The `format` that `probe` reports.
    fn format(self) -> &'static str {
        match self {
            Codec::Hevc => "heic",
            Codec::Av1 => "avif",
            _ => "heif",
        }
    }
}

/// How the samples of a decoded frame relate to colour.
#[derive(Debug, Clone, Copy)]
enum Samples {
    /// Planes are G, B, R (or one gray plane) at full range. A `colr` box
    /// cannot change that, because the codec already produced RGB.
    Rgb,
    /// Planes are Y, Cb, Cr. `colr` wins; else the codec's own signal; else
    /// the codec's documented default.
    Ycc(Option<Nclx>),
}

/// One decoded item: the picture, its colour meaning and, for `unci`, an
/// alpha component stored in the same item.
struct Coded {
    frame: Frame,
    samples: Samples,
    alpha: Option<Frame>,
}

/// What `probe` reports for a HEIF file.
pub struct Info {
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub is_gray: bool,
    pub has_alpha: bool,
}

/// Parse the container. `heic_rs::context::Context::open` refuses AVIF brands
/// by name, so the context is built from `meta` alone; the brand is not used
/// for anything after this point.
fn open(data: &[u8]) -> R<Context<'_>> {
    let meta = heic_rs::meta::parse(data).map_err(|e| format!("heif: {e}"))?;
    Ok(Context {
        file: data,
        ftyp: FileType {
            major: Brand::Mif1,
            effective: Brand::Mif1,
            minor_version: 0,
            compatible_count: 0,
        },
        meta,
    })
}

fn he(e: heic_rs::Error) -> String {
    format!("heif: {e}")
}

fn props<'a>(ctx: &Context<'a>, id: u32) -> R<ItemProps<'a>> {
    ctx.props(id).map_err(he)
}

/// The codec of an item; a grid answers with the codec of its first tile.
fn codec(ctx: &Context<'_>, id: u32) -> R<Codec> {
    let id = match ctx.grid(id).map_err(he)? {
        Some((_, tiles)) => *tiles.first().ok_or("heif: grid has no tiles")?,
        None => id,
    };
    let info = ctx
        .meta
        .item(id)
        .ok_or(format!("heif: item {id} is missing"))?;
    Codec::of(&info.item_type)
}

/// The primary image, as straight RGBA8, with the pixel cap of `max_pixels`.
pub fn decode(data: &[u8], max_pixels: u64) -> R<heic_rs::Image> {
    let ctx = open(data)?;
    let id = ctx.meta.primary;
    if codec(&ctx, id)? == Codec::Hevc {
        let opts = DecodeOptions {
            layout: PixelLayout::Rgba8,
            max_pixels: Some(max_pixels),
            ..Default::default()
        };
        return heic_rs::decode(data, &opts).map_err(|e| format!("heic: {e}"));
    }
    let p = props(&ctx, id)?;
    let (cw, ch) = ctx.coded_size(id, &p).map_err(he)?;
    heic_rs::image::check_pixels(cw, ch, max_pixels).map_err(he)?;
    let (tw, th) = heic_rs::transform::transformed_size(cw, ch, &p.transforms).map_err(he)?;
    heic_rs::image::check_pixels(tw, th, max_pixels).map_err(he)?;

    let coded = decode_item(&ctx, id, max_pixels)?;
    let alpha = match ctx.alpha_item(id).map_err(he)? {
        Some(aux) => Some(decode_item(&ctx, aux, max_pixels)?.frame),
        None => coded.alpha,
    };
    let nclx = match coded.samples {
        Samples::Rgb => rgb_nclx(),
        Samples::Ycc(signal) => p.nclx.or(signal).unwrap_or_default(),
    };
    let image = heic_rs::color::convert(
        &coded.frame,
        alpha.as_ref(),
        nclx,
        PixelLayout::Rgba8,
        max_pixels,
        Some(1),
    )
    .map_err(he)?;
    heic_rs::transform::apply_all(image, &p.transforms).map_err(he)
}

/// Format, display size, depth and alpha, from the boxes alone.
pub fn probe(data: &[u8]) -> R<Info> {
    let ctx = open(data)?;
    let id = ctx.meta.primary;
    let kind = codec(&ctx, id)?;
    if kind == Codec::Hevc {
        let info = heic_rs::probe(data).map_err(|e| format!("heic: {e}"))?;
        return Ok(Info {
            format: kind.format(),
            width: info.width,
            height: info.height,
            bit_depth: info.bit_depth,
            is_gray: info.chroma == ChromaFormat::Monochrome,
            has_alpha: info.has_alpha,
        });
    }
    let p = props(&ctx, id)?;
    let (cw, ch) = ctx.coded_size(id, &p).map_err(he)?;
    let (width, height) =
        heic_rs::transform::transformed_size(cw, ch, &p.transforms).map_err(he)?;
    // A grid's own item has no codec configuration; its first tile does.
    let coded = match ctx.grid(id).map_err(he)? {
        Some((_, tiles)) => tiles[0],
        None => id,
    };
    let find = |code: &[u8; 4]| ctx.meta.props.find(coded, code).map(|b| b.payload);
    let pixi = p.pixi.as_ref().map(|x| x.bits_per_channel.clone());
    let (mut bit_depth, mut is_gray, mut has_alpha) = (8, false, false);
    if let Some(bits) = &pixi {
        bit_depth = bits.first().copied().unwrap_or(8);
        is_gray = bits.len() == 1;
    }
    match kind {
        Codec::Av1 => {
            if let Some(c) = find(b"av1C").filter(|c| c.len() >= 4) {
                let high = c[2] & 0x40 != 0;
                let twelve = c[2] & 0x20 != 0;
                bit_depth = match (high, twelve) {
                    (true, true) => 12,
                    (true, false) => 10,
                    _ => 8,
                };
                is_gray = c[2] & 0x10 != 0;
            }
        }
        Codec::Uncompressed => {
            let layout = UncLayout::parse(find(b"uncC"), find(b"cmpd"))?;
            bit_depth = layout.bits();
            is_gray = layout.is_gray();
            has_alpha = layout.has_alpha();
        }
        _ => {}
    }
    has_alpha |= ctx.alpha_item(id).map_err(he)?.is_some();
    Ok(Info {
        format: kind.format(),
        width,
        height,
        bit_depth,
        is_gray,
        has_alpha,
    })
}

/// Full-range identity: the planes already hold G, B and R.
fn rgb_nclx() -> Nclx {
    Nclx {
        primaries: 1,
        transfer: 13,
        matrix: MatrixCoefficients::Identity,
        matrix_code: 0,
        range: Range::Full,
    }
}

/// Decode an item, joining a grid's tiles when it is one.
fn decode_item(ctx: &Context<'_>, id: u32, max_pixels: u64) -> R<Coded> {
    let Some((grid, tiles)) = ctx.grid(id).map_err(he)? else {
        return decode_coded(ctx, id, max_pixels);
    };
    let mut frames = Vec::with_capacity(tiles.len());
    let mut alphas = Vec::with_capacity(tiles.len());
    let mut samples = Samples::Ycc(None);
    for (i, t) in tiles.iter().enumerate() {
        let c = decode_coded(ctx, *t, max_pixels)?;
        if i == 0 {
            samples = c.samples;
        }
        frames.push(c.frame);
        alphas.extend(c.alpha);
    }
    let frame = heic_rs::grid::compose(&grid, &frames, max_pixels).map_err(he)?;
    let alpha = if alphas.len() == frames.len() {
        Some(heic_rs::grid::compose(&grid, &alphas, max_pixels).map_err(he)?)
    } else {
        None
    };
    Ok(Coded {
        frame,
        samples,
        alpha,
    })
}

/// Decode one coded item through its codec, then cut it to its `ispe`.
fn decode_coded(ctx: &Context<'_>, id: u32, max_pixels: u64) -> R<Coded> {
    let info = ctx
        .meta
        .item(id)
        .ok_or(format!("heif: item {id} is missing"))?;
    let data = ctx.item_data(id).map_err(he)?;
    let find = |code: &[u8; 4]| ctx.meta.props.find(id, code).map(|b| b.payload);
    let p = props(ctx, id)?;
    let mut coded = match Codec::of(&info.item_type)? {
        Codec::Hevc => return err("heif: an HEVC tile in a non-HEVC image is not supported"),
        Codec::Av1 => decode_av1(find(b"av1C"), &data, max_pixels)?,
        Codec::Avc => decode_avc(find(b"avcC"), &data)?,
        Codec::Jpeg => decode_jpeg(find(b"jpgC"), &data)?,
        Codec::Uncompressed => {
            let (w, h) = p
                .ispe
                .map(|i| (i.width, i.height))
                .ok_or("heif: unci item has no ispe")?;
            heic_rs::image::check_pixels(w, h, max_pixels).map_err(he)?;
            UncLayout::parse(find(b"uncC"), find(b"cmpd"))?.decode(&data, w, h)?
        }
    };
    if let Some(ispe) = p.ispe {
        crop(&mut coded.frame, ispe.width, ispe.height)?;
        if let Some(a) = coded.alpha.as_mut() {
            crop(a, ispe.width, ispe.height)?;
        }
    }
    coded.frame.validate().map_err(he)?;
    Ok(coded)
}

/// Codecs code whole blocks; `ispe` is the size that the file shows.
fn crop(frame: &mut Frame, width: u32, height: u32) -> R<()> {
    if frame.width < width || frame.height < height {
        return err(format!(
            "heif: decoded picture is {}x{}, smaller than its ispe {width}x{height}",
            frame.width, frame.height
        ));
    }
    frame.width = width;
    frame.height = height;
    Ok(())
}

/// Copy a plane of `sw`x`sh` samples into `dw`x`dh`, repeating the last
/// column and row when the source is short (odd sizes with 4:2:0).
fn plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> R<Vec<u16>> {
    if sw == 0 || sh == 0 || src.len() < sw * sh {
        return err("heif: decoded plane is shorter than its size");
    }
    let mut out = Vec::with_capacity(dw * dh);
    for y in 0..dh {
        let row = &src[y.min(sh - 1) * sw..][..sw];
        out.extend((0..dw).map(|x| u16::from(row[x.min(sw - 1)])));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AV1
// ---------------------------------------------------------------------------

/// Closes the decoder on every exit path.
struct Av1Decoder(Option<Dav1dContext>);

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        // SAFETY: the context came from `dav1d_open` and is closed once here.
        unsafe { dav1d::dav1d_close(Some(NonNull::from(&mut self.0))) };
    }
}

/// Releases the picture on every exit path.
struct Av1Picture(Dav1dPicture);

impl Drop for Av1Picture {
    fn drop(&mut self) {
        // SAFETY: the picture is either default (a no-op) or from `dav1d_get_picture`.
        unsafe { dav1d::dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

fn eagain(code: i32) -> bool {
    code == -libc::EAGAIN
}

/// Decode one AV1 still: `av1C`'s configuration OBUs, then the item's OBUs.
fn decode_av1(config: Option<&[u8]>, data: &[u8], max_pixels: u64) -> R<Coded> {
    let config = config.ok_or("heif: av01 item has no av1C")?;
    if config.len() < 4 || config[0] & 0x80 == 0 {
        return err("heif: av1C is malformed");
    }
    let mut obus = config[4..].to_vec();
    obus.extend_from_slice(data);

    let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: `dav1d_default_settings` writes every field.
    let mut settings = unsafe {
        dav1d::dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
        settings.assume_init()
    };
    settings.n_threads = 1;
    settings.max_frame_delay = 1;
    settings.frame_size_limit = u32::try_from(max_pixels).unwrap_or(u32::MAX);
    let mut dec = Av1Decoder(None);
    // SAFETY: both pointers are valid for the call.
    let r = unsafe {
        dav1d::dav1d_open(
            Some(NonNull::from(&mut dec.0)),
            Some(NonNull::from(&mut settings)),
        )
    };
    if r.0 != 0 || dec.0.is_none() {
        return err(format!("heif: av1 decoder failed to start ({})", r.0));
    }

    let mut input = Dav1dData::default();
    // SAFETY: `input` is valid to write; the returned buffer holds `obus.len()` bytes.
    unsafe {
        let buf = dav1d::dav1d_data_create(Some(NonNull::from(&mut input)), obus.len());
        if buf.is_null() {
            return err("heif: av1 input allocation failed");
        }
        std::ptr::copy_nonoverlapping(obus.as_ptr(), buf, obus.len());
    }
    let mut pic = Av1Picture(Dav1dPicture::default());
    let mut drained = false;
    loop {
        if input.sz > 0 {
            // SAFETY: the context is open and `input` came from `dav1d_data_create`.
            let r = unsafe { dav1d::dav1d_send_data(dec.0, Some(NonNull::from(&mut input))) };
            if r.0 != 0 && !eagain(r.0) {
                unsafe { dav1d::dav1d_data_unref(Some(NonNull::from(&mut input))) };
                return err(format!("heif: av1 data rejected ({})", r.0));
            }
        }
        // SAFETY: the context is open and `pic` is valid to write.
        let r = unsafe { dav1d::dav1d_get_picture(dec.0, Some(NonNull::from(&mut pic.0))) };
        if r.0 == 0 {
            break;
        }
        if !eagain(r.0) {
            unsafe { dav1d::dav1d_data_unref(Some(NonNull::from(&mut input))) };
            return err(format!("heif: av1 decode failed ({})", r.0));
        }
        if input.sz == 0 {
            if drained {
                return err("heif: av1 item holds no picture");
            }
            drained = true;
        }
    }
    unsafe { dav1d::dav1d_data_unref(Some(NonNull::from(&mut input))) };
    av1_frame(&pic.0)
}

/// Copy a dav1d picture into a `Frame`, and read its colour signal.
fn av1_frame(pic: &Dav1dPicture) -> R<Coded> {
    let (w, h) = (pic.p.w as u32, pic.p.h as u32);
    let bpc = pic.p.bpc as u8;
    if w == 0 || h == 0 || !(8..=16).contains(&bpc) {
        return err("heif: av1 picture has no size");
    }
    let chroma = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => ChromaFormat::Monochrome,
        DAV1D_PIXEL_LAYOUT_I420 => ChromaFormat::Yuv420,
        DAV1D_PIXEL_LAYOUT_I422 => ChromaFormat::Yuv422,
        DAV1D_PIXEL_LAYOUT_I444 => ChromaFormat::Yuv444,
        _ => return err("heif: av1 picture has an unknown chroma layout"),
    };
    let read = |i: usize, pw: u32, ph: u32, stride: isize| -> R<Vec<u16>> {
        let base = pic.data[i]
            .ok_or("heif: av1 picture plane is missing")?
            .as_ptr() as *const u8;
        let mut out = Vec::with_capacity(pw as usize * ph as usize);
        for row in 0..ph as isize {
            // SAFETY: dav1d guarantees `ph` rows of `stride` bytes, each with
            // `pw` samples of `bpc` bits (one or two bytes).
            unsafe {
                let p = base.offset(row * stride);
                if bpc == 8 {
                    let s = std::slice::from_raw_parts(p, pw as usize);
                    out.extend(s.iter().map(|&v| u16::from(v)));
                } else {
                    let s = std::slice::from_raw_parts(p as *const u16, pw as usize);
                    out.extend_from_slice(s);
                }
            }
        }
        Ok(out)
    };
    let y = read(0, w, h, pic.stride[0])?;
    let (cw, ch) = chroma.chroma_size(w, h);
    let (cb, cr) = if chroma == ChromaFormat::Monochrome {
        (Vec::new(), Vec::new())
    } else {
        (
            read(1, cw, ch, pic.stride[1])?,
            read(2, cw, ch, pic.stride[1])?,
        )
    };
    let signal = pic.seq_hdr.map(|s| {
        // SAFETY: the sequence header lives as long as the picture.
        let s = unsafe { s.as_ref() };
        Nclx {
            primaries: s.pri as u16,
            transfer: s.trc as u16,
            matrix: MatrixCoefficients::from_code(s.mtrx as u16),
            matrix_code: s.mtrx as u16,
            range: if s.color_range != 0 {
                Range::Full
            } else {
                Range::Limited
            },
        }
    });
    Ok(Coded {
        frame: Frame {
            width: w,
            height: h,
            bit_depth: bpc,
            chroma,
            y,
            cb,
            cr,
            y_stride: w,
            c_stride: if chroma == ChromaFormat::Monochrome {
                0
            } else {
                cw
            },
        },
        samples: Samples::Ycc(signal),
        alpha: None,
    })
}

// ---------------------------------------------------------------------------
// AVC (H.264)
// ---------------------------------------------------------------------------

/// BT.601 limited range: the H.264 default when `colr` is absent.
fn avc_nclx() -> Nclx {
    Nclx {
        primaries: 6,
        transfer: 6,
        matrix: MatrixCoefficients::Bt601,
        matrix_code: 6,
        range: Range::Limited,
    }
}

fn decode_avc(config: Option<&[u8]>, data: &[u8]) -> R<Coded> {
    let config = config.ok_or("heif: avc1 item has no avcC")?;
    let cfg = parse_avcc_config(config).map_err(|e| format!("heif: bad avcC: {e}"))?;
    let mut dec = Decoder::new();
    for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
        dec.decode_nal(nal)
            .map_err(|e| format!("heif: h264 parameter set rejected: {e:?}"))?;
    }
    let mut picture = None;
    for nal in parse_avcc(data, cfg.length_size) {
        if let Some(f) = dec
            .decode_nal(&nal)
            .map_err(|e| format!("heif: h264 decode failed: {e:?}"))?
        {
            picture = Some(f);
        }
    }
    let f = picture
        .or_else(|| dec.flush())
        .ok_or("heif: avc1 item holds no picture")?;
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = ChromaFormat::Yuv420.chroma_size(f.width, f.height);
    let sh = (h / 2).max(1);
    let plane_c = |p: &[u8]| plane(p, p.len() / sh, sh, cw as usize, ch as usize);
    Ok(Coded {
        frame: Frame {
            width: f.width,
            height: f.height,
            bit_depth: 8,
            chroma: ChromaFormat::Yuv420,
            y: plane(&f.y, w, h, w, h)?,
            cb: plane_c(&f.u)?,
            cr: plane_c(&f.v)?,
            y_stride: f.width,
            c_stride: cw,
        },
        samples: Samples::Ycc(Some(avc_nclx())),
        alpha: None,
    })
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

/// A JPEG item is a whole JPEG stream; `jpgC`, when present, holds header
/// segments that the encoder moved out of the item.
fn decode_jpeg(prefix: Option<&[u8]>, data: &[u8]) -> R<Coded> {
    let mut bytes = Vec::new();
    let data = match prefix {
        Some(p) if !data.starts_with(&[0xFF, 0xD8]) => {
            bytes.extend_from_slice(p);
            bytes.extend_from_slice(data);
            &bytes[..]
        }
        _ => data,
    };
    let img = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)
        .map_err(|e| format!("heif: jpeg decode failed: {e}"))?;
    let (w, h) = (img.width(), img.height());
    let n = w as usize * h as usize;
    let frame = if img.color().has_color() {
        let rgb = img.to_rgb8();
        let px = rgb.as_raw();
        let ch = |i: usize| -> Vec<u16> { (0..n).map(|p| u16::from(px[p * 3 + i])).collect() };
        Frame {
            width: w,
            height: h,
            bit_depth: 8,
            chroma: ChromaFormat::Yuv444,
            y: ch(1),
            cb: ch(2),
            cr: ch(0),
            y_stride: w,
            c_stride: w,
        }
    } else {
        let l = img.to_luma8();
        Frame {
            width: w,
            height: h,
            bit_depth: 8,
            chroma: ChromaFormat::Monochrome,
            y: l.as_raw().iter().map(|&v| u16::from(v)).collect(),
            y_stride: w,
            ..Default::default()
        }
    };
    Ok(Coded {
        frame,
        samples: Samples::Rgb,
        alpha: None,
    })
}

// ---------------------------------------------------------------------------
// Uncompressed (unci)
// ---------------------------------------------------------------------------

const MONO: u16 = 0;
const Y: u16 = 1;
const CB: u16 = 2;
const CR: u16 = 3;
const RED: u16 = 4;
const GREEN: u16 = 5;
const BLUE: u16 = 6;
const ALPHA: u16 = 7;

/// The subset of `uncC` that this decoder reads: 4:4:4, unsigned 8- or 16-bit
/// components, planar or pixel-interleaved, one tile, no padding.
#[derive(Debug)]
struct UncLayout {
    /// Component type and bit depth, in storage order.
    components: Vec<(u16, u8)>,
    pixel_interleaved: bool,
    little_endian: bool,
}

struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        if self.0.len() < n {
            return err("heif: uncC or cmpd is truncated");
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> R<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> R<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

impl UncLayout {
    fn parse(uncc: Option<&[u8]>, cmpd: Option<&[u8]>) -> R<UncLayout> {
        let mut r = Bytes(uncc.ok_or("heif: unci item has no uncC")?);
        let version = r.u8()?;
        r.take(3)?;
        let profile = r.take(4)?;
        if version == 1 {
            let components = match profile {
                b"rgb3" => vec![RED, GREEN, BLUE],
                b"rgba" => vec![RED, GREEN, BLUE, ALPHA],
                b"abgr" => vec![ALPHA, BLUE, GREEN, RED],
                _ => return err("heif: uncC profile is not supported"),
            };
            return Ok(UncLayout {
                components: components.into_iter().map(|c| (c, 8)).collect(),
                pixel_interleaved: true,
                little_endian: false,
            });
        }
        if version != 0 {
            return err(format!("heif: uncC version {version} is not supported"));
        }
        let mut types = Vec::new();
        let mut c = Bytes(cmpd.ok_or("heif: unci item has no cmpd")?);
        for _ in 0..c.u32()? {
            let t = c.u16()?;
            if t >= 0x8000 {
                while c.u8()? != 0 {}
            }
            types.push(t);
        }
        let mut components = Vec::new();
        for _ in 0..r.u32()? {
            let index = usize::from(r.u16()?);
            let bits = r.u8()? + 1;
            let format = r.u8()?;
            let align = r.u8()?;
            let t = *types
                .get(index)
                .ok_or("heif: uncC names a component that cmpd lacks")?;
            if format != 0 || !(bits == 8 || bits == 16) || (align != 0 && align * 8 != bits) {
                return err(
                    "heif: uncC component format is not supported (unsigned 8 or 16 bit only)",
                );
            }
            components.push((t, bits));
        }
        let (sampling, interleave, block, flags) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
        let (pixel_size, row_align, tile_align) = (r.u32()?, r.u32()?, r.u32()?);
        let (cols, rows) = (r.u32()?, r.u32()?);
        if sampling != 0 {
            return err("heif: uncC chroma subsampling is not supported");
        }
        if interleave > 1 || block != 0 || pixel_size != 0 || row_align != 0 || tile_align != 0 {
            return err("heif: uncC layout is not supported (planar or pixel interleave only)");
        }
        if cols != 0 || rows != 0 {
            return err("heif: tiled unci is not supported");
        }
        Ok(UncLayout {
            components,
            pixel_interleaved: interleave == 1,
            little_endian: flags & 0x80 != 0,
        })
    }

    fn has(&self, t: u16) -> bool {
        self.components.iter().any(|c| c.0 == t)
    }

    fn is_gray(&self) -> bool {
        !self.has(RED) && !self.has(CB) && (self.has(MONO) || self.has(Y))
    }

    fn has_alpha(&self) -> bool {
        self.has(ALPHA)
    }

    fn bits(&self) -> u8 {
        self.components
            .iter()
            .find(|c| c.0 != ALPHA)
            .map_or(8, |c| c.1)
    }

    fn decode(&self, data: &[u8], w: u32, h: u32) -> R<Coded> {
        let n = w as usize * h as usize;
        let sizes: Vec<usize> = self
            .components
            .iter()
            .map(|c| usize::from(c.1 / 8))
            .collect();
        let pixel: usize = sizes.iter().sum();
        if data.len() < pixel * n {
            return err("heif: unci item is shorter than its size");
        }
        let le = self.little_endian;
        let sample = |b: &[u8]| -> u16 {
            match (b.len(), le) {
                (1, _) => u16::from(b[0]),
                (_, true) => u16::from_le_bytes([b[0], b[1]]),
                _ => u16::from_be_bytes([b[0], b[1]]),
            }
        };
        let read = |t: u16| -> Option<(Vec<u16>, u8)> {
            let i = self.components.iter().position(|c| c.0 == t)?;
            let size = sizes[i];
            let (start, step) = if self.pixel_interleaved {
                (sizes[..i].iter().sum::<usize>(), pixel)
            } else {
                (sizes[..i].iter().sum::<usize>() * n, size)
            };
            let v = (0..n)
                .map(|p| sample(&data[start + p * step..][..size]))
                .collect();
            Some((v, self.components[i].1))
        };
        let mono = |plane: (Vec<u16>, u8)| Frame {
            width: w,
            height: h,
            bit_depth: plane.1,
            chroma: ChromaFormat::Monochrome,
            y: plane.0,
            y_stride: w,
            ..Default::default()
        };
        let full = |a: (Vec<u16>, u8), b: Vec<u16>, c: Vec<u16>| Frame {
            width: w,
            height: h,
            bit_depth: a.1,
            chroma: ChromaFormat::Yuv444,
            y: a.0,
            cb: b,
            cr: c,
            y_stride: w,
            c_stride: w,
        };
        let (frame, samples) =
            if let (Some(r), Some(g), Some(b)) = (read(RED), read(GREEN), read(BLUE)) {
                (full(g, b.0, r.0), Samples::Rgb)
            } else if let (Some(y), Some(cb), Some(cr)) = (read(Y), read(CB), read(CR)) {
                (full(y, cb.0, cr.0), Samples::Ycc(None))
            } else if let Some(l) = read(MONO).or_else(|| read(Y)) {
                (mono(l), Samples::Rgb)
            } else {
                return err("heif: unci item has no colour components this decoder reads");
            };
        if self
            .components
            .iter()
            .any(|c| c.1 != frame.bit_depth && c.0 != ALPHA)
        {
            return err("heif: unci components with different bit depths are not supported");
        }
        Ok(Coded {
            frame,
            samples,
            alpha: read(ALPHA).map(mono),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u64 = 1 << 26;

    /// The 64x48 source of every fixture: red, green, blue and white quarters,
    /// and a transparent block at x >= 48, y >= 36 (only in the alpha files).
    fn expected(x: u32, y: u32) -> [u8; 3] {
        match (x >= 32, y >= 24) {
            (false, false) => [220, 30, 30],
            (true, false) => [30, 160, 40],
            (false, true) => [30, 60, 220],
            (true, true) => [240, 240, 240],
        }
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/../../test/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
    }

    /// Check the centre of each quarter within `tol`, and alpha when `alpha`.
    fn check(name: &str, tol: i32, alpha: bool) {
        let img = decode(&fixture(name), MAX).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((img.width, img.height), (64, 48), "{name}");
        for (x, y) in [(10, 10), (50, 10), (10, 34), (40, 30)] {
            let i = ((y * 64 + x) * 4) as usize;
            let got = &img.data[i..i + 4];
            for (c, want) in expected(x, y).iter().enumerate() {
                let d = (i32::from(got[c]) - i32::from(*want)).abs();
                assert!(d <= tol, "{name} ({x},{y}) channel {c}: {got:?} vs {want}");
            }
            assert_eq!(got[3], 255, "{name} ({x},{y}) is opaque");
        }
        let corner = img.data[((44 * 64 + 56) * 4 + 3) as usize];
        assert_eq!(corner == 0, alpha, "{name} transparent corner");
    }

    #[test]
    fn avif_lossy_with_alpha() {
        check("lossy-alpha.avif", 12, true);
    }

    #[test]
    fn avif_lossless_rgb_is_exact() {
        check("lossless-rgb.avif", 0, true);
    }

    #[test]
    fn avif_grid_joins_its_tiles() {
        check("grid.avif", 12, false);
    }

    #[test]
    fn jpeg_heif_with_alpha() {
        check("jpeg-alpha.heif", 12, true);
    }

    #[test]
    fn avc_heif_with_alpha() {
        check("avc-alpha.heif", 16, true);
    }

    #[test]
    fn probe_names_the_codec_family() {
        let cases = [
            ("lossy-alpha.avif", "avif", true),
            ("grid.avif", "avif", false),
            ("jpeg-alpha.heif", "heif", true),
            ("avc-alpha.heif", "heif", true),
            ("grid-alpha.heic", "heic", true),
        ];
        for (name, format, alpha) in cases {
            let info = probe(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(info.format, format, "{name}");
            assert_eq!(info.has_alpha, alpha, "{name}");
            if format != "heic" {
                assert_eq!((info.width, info.height), (64, 48), "{name}");
            }
        }
    }

    #[test]
    fn a_cut_avif_is_an_error_not_a_panic() {
        let data = fixture("lossy-alpha.avif");
        for n in [20, 200, data.len() - 40] {
            assert!(decode(&data[..n], MAX).is_err());
        }
    }

    // -- unci: no encoder here writes it, so the tests build the file -------

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        b.extend_from_slice(body);
        bx(kind, &b)
    }

    /// A one-item `unci` file; the pixels sit in `idat`.
    fn unci(w: u32, h: u32, cmpd: Option<&[u16]>, uncc: Vec<u8>, pixels: &[u8]) -> Vec<u8> {
        let hdlr = full(b"hdlr", 0, &[&[0; 4][..], b"pict", &[0; 13]].concat());
        let pitm = full(b"pitm", 0, &1u16.to_be_bytes());
        let infe = full(b"infe", 2, &[&[0, 1, 0, 0][..], b"unci", &[0]].concat());
        let iinf = full(b"iinf", 0, &[&1u16.to_be_bytes()[..], &infe].concat());
        let mut iloc = vec![0x44, 0x00, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1];
        iloc.extend_from_slice(&0u32.to_be_bytes());
        iloc.extend_from_slice(&(pixels.len() as u32).to_be_bytes());
        let iloc = full(b"iloc", 1, &iloc);
        let ispe = full(b"ispe", 0, &[w.to_be_bytes(), h.to_be_bytes()].concat());
        let mut ipco = ispe;
        let mut count = 1u8;
        if let Some(types) = cmpd {
            let mut b = (types.len() as u32).to_be_bytes().to_vec();
            types
                .iter()
                .for_each(|t| b.extend_from_slice(&t.to_be_bytes()));
            ipco.extend(bx(b"cmpd", &b));
            count += 1;
        }
        ipco.extend(uncc);
        count += 1;
        let mut ipma = vec![0, 0, 0, 1, 0, 1, count];
        ipma.extend((1..=count).map(|i| 0x80 | i));
        let iprp = bx(
            b"iprp",
            &[bx(b"ipco", &ipco), full(b"ipma", 0, &ipma)].concat(),
        );
        let idat = bx(b"idat", pixels);
        let meta = full(b"meta", 0, &[hdlr, pitm, iloc, iinf, iprp, idat].concat());
        [bx(b"ftyp", b"mif1\0\0\0\0mif1unif"), meta].concat()
    }

    /// `uncC` version 0: components as (cmpd index, bits), then the layout.
    fn uncc_v0(components: &[(u16, u8)], interleave: u8, flags: u8) -> Vec<u8> {
        let mut b = b"\0\0\0\0".to_vec();
        b.extend_from_slice(&(components.len() as u32).to_be_bytes());
        for (index, bits) in components {
            b.extend_from_slice(&index.to_be_bytes());
            b.extend_from_slice(&[bits - 1, 0, 0]);
        }
        b.extend_from_slice(&[0, interleave, 0, flags]);
        b.extend_from_slice(&[0; 20]);
        full(b"uncC", 0, &b)
    }

    /// 3x2 RGBA test pixels.
    const PX: [[u8; 4]; 6] = [
        [255, 0, 0, 255],
        [0, 255, 0, 128],
        [0, 0, 255, 0],
        [10, 20, 30, 255],
        [200, 100, 50, 255],
        [1, 2, 3, 4],
    ];

    fn rgba(img: &heic_rs::Image) -> Vec<[u8; 4]> {
        img.data
            .chunks(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect()
    }

    #[test]
    fn unci_planar_8_bit_rgba() {
        let planes: Vec<u8> = (0..4).flat_map(|c| PX.iter().map(move |p| p[c])).collect();
        let comps = [(0, 8), (1, 8), (2, 8), (3, 8)];
        let file = unci(
            3,
            2,
            Some(&[RED, GREEN, BLUE, ALPHA]),
            uncc_v0(&comps, 0, 0),
            &planes,
        );
        let img = decode(&file, MAX).unwrap();
        assert_eq!((img.width, img.height), (3, 2));
        assert_eq!(rgba(&img), PX.to_vec());
        let info = probe(&file).unwrap();
        assert_eq!(
            (info.format, info.bit_depth, info.has_alpha),
            ("heif", 8, true)
        );
    }

    #[test]
    fn unci_interleaved_16_bit_little_endian_rgb() {
        let mut pixels = Vec::new();
        for p in PX {
            for c in [2, 1, 0] {
                pixels.extend_from_slice(&(u16::from(p[c]) * 257).to_le_bytes());
            }
        }
        let comps = [(0, 16), (1, 16), (2, 16)];
        let file = unci(
            3,
            2,
            Some(&[BLUE, GREEN, RED]),
            uncc_v0(&comps, 1, 0x80),
            &pixels,
        );
        let want: Vec<[u8; 4]> = PX.iter().map(|p| [p[0], p[1], p[2], 255]).collect();
        assert_eq!(rgba(&decode(&file, MAX).unwrap()), want);
        let info = probe(&file).unwrap();
        assert_eq!(
            (info.bit_depth, info.has_alpha, info.is_gray),
            (16, false, false)
        );
    }

    #[test]
    fn unci_profile_rgba_needs_no_cmpd() {
        let pixels: Vec<u8> = PX.iter().flatten().copied().collect();
        let file = unci(3, 2, None, full(b"uncC", 1, b"rgba"), &pixels);
        assert_eq!(rgba(&decode(&file, MAX).unwrap()), PX.to_vec());
    }

    #[test]
    fn unci_monochrome() {
        let file = unci(
            3,
            2,
            Some(&[MONO]),
            uncc_v0(&[(0, 8)], 0, 0),
            &[0, 50, 100, 150, 200, 250],
        );
        let img = decode(&file, MAX).unwrap();
        let gray: Vec<u8> = rgba(&img).iter().map(|p| p[0]).collect();
        assert_eq!(gray, vec![0, 50, 100, 150, 200, 250]);
        assert!(probe(&file).unwrap().is_gray);
    }

    #[test]
    fn unci_refuses_what_it_cannot_read() {
        let short = unci(3, 2, Some(&[MONO]), uncc_v0(&[(0, 8)], 0, 0), &[0, 1]);
        assert!(decode(&short, MAX).unwrap_err().contains("shorter"));
        let ten_bit = unci(3, 2, Some(&[MONO]), uncc_v0(&[(0, 10)], 0, 0), &[0; 12]);
        assert!(decode(&ten_bit, MAX).unwrap_err().contains("not supported"));
    }

    /// libheif writes an AVC alpha item as High 4:0:0. These streams are x264
    /// 4:0:0 stills; the `.gray` files are ffmpeg's decode of the same streams.
    #[test]
    fn h264_monochrome_matches_the_reference_decoder() {
        for name in ["h264-gray-cabac-8x8", "h264-gray-cavlc-4x4"] {
            let stream = fixture(&format!("{name}.264"));
            let want = fixture(&format!("{name}.gray"));
            let mut dec = Decoder::new();
            let mut picture = None;
            for nal in rust_h264::nal::parse_annex_b(&stream) {
                picture = dec.decode_nal(&nal).unwrap().or(picture);
            }
            let f = picture.or_else(|| dec.flush()).unwrap();
            assert_eq!((f.width, f.height), (64, 48), "{name}");
            assert_eq!(f.y, want, "{name} luma");
            assert!(
                f.u.iter().chain(&f.v).all(|&c| c == 128),
                "{name} neutral chroma"
            );
        }
    }
}
