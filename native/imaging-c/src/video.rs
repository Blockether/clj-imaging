//! MP4 video: probe, decode to RGBA frames, transcode to animated GIF.
//!
//! Pure Rust, like every other engine in this cdylib — `re_mp4` demuxes the
//! ISO-BMFF container and `rust_h264` decodes the H.264/AVC bitstream (Baseline,
//! Main and High profiles, CAVLC + CABAC, B-frames). No ffmpeg, no system codec,
//! no subprocess, so a GraalVM native image keeps working and nothing has to be
//! installed on the host.
//!
//! Three things the callers must not get wrong, and so are handled here once:
//!
//!   * H.264 codes in macroblocks, so the CODED size is padded up to a multiple
//!     of 16 while the display size lives in the SPS cropping window. `Frame`'s
//!     `width`/`height` are already the cropped display size (stride == width),
//!     and the container's `tkhd` size can differ from both — `probe` reports the
//!     container's, a decode reports the bitstream's.
//!   * Decode order is not display order, and the bitstream's own picture order
//!     count cannot be trusted to fix that: POC derives from `frame_num`, which
//!     wraps at `MaxFrameNum` (16 as x264 writes it), so ranking by POC folds
//!     frame 16 back onto frame 0. Display order comes from the container's
//!     `ctts` composition timestamps, which are monotonic over the whole track.
//!   * MP4 samples are AVCC (length-prefixed) NAL units and the SPS/PPS live in
//!     the `avcC` box, NOT in the samples — feed the config first or the very
//!     first slice fails.

use std::io::Cursor;

use base64::Engine as _;
use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame as GifFrame};
use re_mp4::{Mp4, Track, TrackKind};
use rust_h264::decoder::{Decoder, Frame as H264Frame};
use rust_h264::nal::{parse_avcc, parse_avcc_config};
use serde_json::{Value, json};

use crate::imgops::{self, Image};
use crate::util::{R, err, jf, js};

/// Cheap container sniff: an ISO-BMFF file starts with a `ftyp` box.
pub fn is_mp4(data: &[u8]) -> bool {
    data.len() >= 12 && &data[4..8] == b"ftyp"
}

/// The one codec this cdylib can actually decode.
const H264: &str = "h264";

/// Family of a `codec_string` such as `avc1.420028` / `hvc1.1.6.L93.B0`.
fn codec_kind(codec: Option<&str>) -> &'static str {
    match codec.and_then(|c| c.split('.').next()) {
        Some("avc1" | "avc3") => H264,
        Some("hev1" | "hvc1") => "hevc",
        Some("av01") => "av1",
        Some("vp08") => "vp8",
        Some("vp09") => "vp9",
        _ => "unknown",
    }
}

fn parse(data: &[u8]) -> R<Mp4> {
    if !is_mp4(data) {
        return err("not an MP4: no ISO-BMFF `ftyp` box");
    }
    Mp4::read_bytes(data).map_err(|e| format!("mp4 parse failed: {e}"))
}

fn video_track(mp4: &Mp4) -> R<&Track> {
    mp4.tracks()
        .values()
        .find(|t| t.kind == Some(TrackKind::Video))
        .ok_or_else(|| "mp4 has no video track".to_string())
}

fn duration_s(t: &Track) -> f64 {
    if t.timescale == 0 {
        0.0
    } else {
        t.duration as f64 / t.timescale as f64
    }
}

fn fps(t: &Track) -> f64 {
    let d = duration_s(t);
    if d <= 0.0 {
        0.0
    } else {
        t.samples.len() as f64 / d
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

/// Identify an MP4 without decoding a single frame:
/// `{format codec codec_kind is_decodable width height frames duration_s fps
///   timescale has_audio}`. `width`/`height` are the container's declared track
/// size; a decode reports the bitstream's own (they can differ).
pub fn probe(data: &[u8]) -> R<Value> {
    let mp4 = parse(data)?;
    let t = video_track(&mp4)?;
    let codec = t.codec_string(&mp4);
    let kind = codec_kind(codec.as_deref());
    let has_audio = mp4
        .tracks()
        .values()
        .any(|t| t.kind == Some(TrackKind::Audio));
    Ok(json!({
        "format": "mp4",
        "codec": codec,
        "codec_kind": kind,
        "is_decodable": kind == H264,
        "width": t.width,
        "height": t.height,
        "frames": t.samples.len(),
        "duration_s": duration_s(t),
        "fps": fps(t),
        "timescale": t.timescale,
        "has_audio": has_audio,
    }))
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

/// Straight (non-premultiplied) RGBA8 rows from one decoded 4:2:0 picture, using
/// the BT.601 limited-range matrix — the standard for the SD-sized content this
/// decoder sees, and what every other H.264 consumer assumes when a stream
/// carries no colour VUI.
fn yuv420_to_rgba(f: &H264Frame) -> R<Vec<u8>> {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 {
        return err("decoded frame has zero size");
    }
    if f.y.len() < w * h {
        return err(format!(
            "decoded frame luma plane too small: {} < {}",
            f.y.len(),
            w * h
        ));
    }
    let cw = w.div_ceil(2);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let yrow = y * w;
        let crow = (y / 2) * cw;
        for x in 0..w {
            let luma = f.y[yrow + x] as i32 - 16;
            let ci = crow + x / 2;
            let cb = *f.u.get(ci).unwrap_or(&128) as i32 - 128;
            let cr = *f.v.get(ci).unwrap_or(&128) as i32 - 128;
            let r = (298 * luma + 409 * cr + 128) >> 8;
            let g = (298 * luma - 100 * cb - 208 * cr + 128) >> 8;
            let b = (298 * luma + 516 * cb + 128) >> 8;
            let o = (yrow + x) * 4;
            out[o] = r.clamp(0, 255) as u8;
            out[o + 1] = g.clamp(0, 255) as u8;
            out[o + 2] = b.clamp(0, 255) as u8;
            out[o + 3] = 255;
        }
    }
    Ok(out)
}

/// How much of a clip to decode and at what size.
struct Opts {
    /// Stop after this many EMITTED frames; 0 = the whole clip.
    max_frames: usize,
    /// Emit every `stride`-th frame (every frame is still decoded — inter
    /// prediction needs its references).
    stride: usize,
    /// Longest edge of an emitted frame; 0 = the bitstream's own size. Never
    /// upscales.
    max_dimension: u32,
    /// Resampling filter used when `max_dimension` shrinks a frame.
    filter: String,
}

fn opts_of(v: &Value) -> Opts {
    Opts {
        max_frames: jf(v, "max_frames").unwrap_or(0.0).max(0.0) as usize,
        stride: (jf(v, "stride").unwrap_or(1.0).max(1.0)) as usize,
        max_dimension: jf(v, "max_dimension").unwrap_or(0.0).max(0.0) as u32,
        filter: js(v, "filter").unwrap_or("lanczos3").to_string(),
    }
}

/// What a decode run learned about the clip, alongside the frames it handed to
/// the sink.
struct Meta {
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    total_frames: usize,
    emitted: usize,
    duration_s: f64,
    fps: f64,
    codec: Option<String>,
}

/// Decode the video track and hand every KEPT frame to `sink` as
/// `(source index, presentation timestamp, image)`. Frames are streamed, never
/// collected here, so a caller that only re-encodes (`to_gif`) never holds the
/// whole clip in memory.
fn each_frame(
    data: &[u8],
    opts: &Opts,
    mut sink: impl FnMut(usize, f64, Image) -> R<()>,
) -> R<Meta> {
    let mp4 = parse(data)?;
    let t = video_track(&mp4)?;
    let codec = t.codec_string(&mp4);
    let kind = codec_kind(codec.as_deref());
    if kind != H264 {
        return err(format!(
            "cannot decode {} video: only H.264 (avc1) MP4 is supported",
            codec.as_deref().unwrap_or(kind)
        ));
    }
    let cfg_raw = t
        .raw_codec_config(&mp4)
        .ok_or_else(|| "mp4 video track has no avcC codec configuration".to_string())?;
    let cfg = parse_avcc_config(&cfg_raw).map_err(|e| format!("bad avcC box: {e}"))?;

    let mut dec = Decoder::new();
    for nal in cfg.sps_nals.iter().chain(cfg.pps_nals.iter()) {
        dec.decode_nal(nal)
            .map_err(|e| format!("h264 parameter set rejected: {e:?}"))?;
    }

    // Display order is taken from the CONTAINER, never from the bitstream's
    // picture order count. POC derives from `frame_num`, which wraps at
    // `MaxFrameNum` — 16 for every stream x264 writes — so ranking pictures by
    // POC silently folds frame 16 back onto frame 0 and interleaves the halves of
    // any GOP longer than the wrap. The `ctts` composition timestamps are
    // monotonic across the whole track, so rank the samples once here and release
    // each decoded picture at its own rank: right for B-frames, immune to wrap.
    let stamps: Vec<f64> = t
        .samples
        .iter()
        .map(|s| {
            if s.timescale == 0 {
                0.0
            } else {
                s.composition_timestamp as f64 / s.timescale as f64
            }
        })
        .collect();
    let mut by_time: Vec<usize> = (0..stamps.len()).collect();
    by_time.sort_by(|&a, &b| {
        stamps[a]
            .partial_cmp(&stamps[b])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });
    // `rank[i]` is the display position of the i-th picture in DECODE order, and
    // `shown[r]` the timestamp of the picture displayed r-th.
    let mut rank = vec![0usize; stamps.len()];
    for (r, &d) in by_time.iter().enumerate() {
        rank[d] = r;
    }
    let shown: Vec<f64> = by_time.iter().map(|&d| stamps[d]).collect();

    let mut meta = Meta {
        width: 0,
        height: 0,
        source_width: 0,
        source_height: 0,
        total_frames: t.samples.len(),
        emitted: 0,
        duration_s: duration_s(t),
        fps: fps(t),
        codec,
    };

    // Returns false once the caller has all the frames it asked for.
    let mut take = |index: usize, f: H264Frame, meta: &mut Meta| -> R<bool> {
        if !index.is_multiple_of(opts.stride) {
            return Ok(true);
        }
        if opts.max_frames > 0 && meta.emitted >= opts.max_frames {
            return Ok(false);
        }
        let mut img = Image::from_rgba(f.width, f.height, &yuv420_to_rgba(&f)?)?;
        meta.source_width = f.width;
        meta.source_height = f.height;
        if opts.max_dimension > 0 {
            let (w, h) = imgops::fit_size(
                img.width(),
                img.height(),
                opts.max_dimension,
                opts.max_dimension,
            );
            if (w, h) != (img.width(), img.height()) {
                img = imgops::resize(&img, w, h, &opts.filter)?;
            }
        }
        meta.width = img.width();
        meta.height = img.height();
        meta.emitted += 1;
        let ts = shown.get(index).copied().unwrap_or(0.0);
        sink(index, ts, img)?;
        Ok(opts.max_frames == 0 || meta.emitted < opts.max_frames)
    };

    // Pictures wait here until their display rank comes up. `Decoder` returns
    // them in decode order, so this holds at most the stream's reorder depth.
    let mut pending: Vec<(usize, H264Frame)> = Vec::new();
    let mut decoded = 0usize;
    let mut next = 0usize;
    let mut stopped = false;

    'samples: for s in &t.samples {
        let range = s.byte_range();
        if range.end > data.len() {
            return err("mp4 sample points outside the file");
        }
        for nal in parse_avcc(&data[range], cfg.length_size) {
            let decoded_frame = dec
                .decode_nal(&nal)
                .map_err(|e| format!("h264 decode failed: {e:?}"))?;
            let Some(f) = decoded_frame else { continue };
            pending.push((rank.get(decoded).copied().unwrap_or(decoded), f));
            decoded += 1;
            while let Some(i) = pending.iter().position(|(r, _)| *r == next) {
                let (r, f) = pending.swap_remove(i);
                next += 1;
                if !take(r, f, &mut meta)? {
                    stopped = true;
                    break 'samples;
                }
            }
        }
    }
    if !stopped {
        if let Some(f) = dec.flush() {
            pending.push((rank.get(decoded).copied().unwrap_or(decoded), f));
        }
        // Whatever is still buffered — a decode gap, or a track whose sample and
        // picture counts disagree — is released in display order all the same.
        pending.sort_by_key(|(r, _)| *r);
        for (r, f) in pending.drain(..) {
            if !take(r, f, &mut meta)? {
                break;
            }
        }
    }
    if meta.emitted == 0 {
        return err("mp4 video track decoded no frames");
    }
    Ok(meta)
}

/// Decode an MP4 into frames as JSON: `{width height source_width source_height
/// frames total_frames duration_s fps codec encoding frames[]}`, each frame
/// `{index timestamp_s data}` with `data` base64.
///
/// `encoding` picks what `data` holds: `"rgba"` (default) is straight RGBA8 rows
/// sized `width*height*4`; anything else (`"png"`, `"jpeg"`, `"webp"`, ...) is an
/// ENCODED still, which is both smaller on the wire and what a terminal or a
/// browser wants to display anyway.
pub fn decode(data: &[u8], opts: &Value) -> R<Value> {
    let o = opts_of(opts);
    let encoding = js(opts, "encoding").unwrap_or("rgba").to_string();
    let quality = jf(opts, "quality").unwrap_or(0.0).clamp(0.0, 100.0) as u8;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut frames: Vec<Value> = Vec::new();
    let meta = each_frame(data, &o, |index, ts, img| {
        let bytes = if encoding == "rgba" {
            img.buf.as_raw().clone()
        } else {
            imgops::encode(&img, &encoding, quality)?
        };
        frames.push(json!({
            "index": index,
            "timestamp_s": ts,
            "data": b64.encode(&bytes),
        }));
        Ok(())
    })?;
    Ok(json!({
        "format": "mp4",
        "codec": meta.codec,
        "encoding": encoding,
        "width": meta.width,
        "height": meta.height,
        "source_width": meta.source_width,
        "source_height": meta.source_height,
        "frames": meta.emitted,
        "total_frames": meta.total_frames,
        "duration_s": meta.duration_s,
        "fps": meta.fps,
        "frames_data": frames,
    }))
}

// ---------------------------------------------------------------------------
// transcode
// ---------------------------------------------------------------------------

/// Transcode an MP4 straight to an animated GIF, one frame at a time — the whole
/// clip never exists as RGBA in memory, and nothing crosses the FFI boundary
/// except the finished GIF.
///
/// Options: the decode ones (`max_frames` `stride` `max_dimension` `filter`) plus
/// `fps` (playback rate; default the source rate divided by `stride`) and
/// `loop_count` (-1 = forever).
pub fn to_gif(data: &[u8], opts: &Value) -> R<Vec<u8>> {
    let o = opts_of(opts);
    let loop_count = opts
        .get("loop_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let want_fps = jf(opts, "fps").unwrap_or(0.0);

    let mut out = Cursor::new(Vec::new());
    let meta = {
        let mut enc = GifEncoder::new(&mut out);
        if loop_count < 0 {
            enc.set_repeat(Repeat::Infinite)
        } else {
            enc.set_repeat(Repeat::Finite(loop_count.min(u16::MAX as i64) as u16))
        }
        .map_err(|e| format!("gif encode failed: {e}"))?;

        // The delay has to be known BEFORE the first frame is written, so it
        // comes from the container's rate (thinned by `stride`), not from the
        // gaps between the timestamps we are still streaming.
        let src_fps = probe(data)?
            .get("fps")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let rate = if want_fps > 0.0 {
            want_fps
        } else if src_fps > 0.0 {
            src_fps / o.stride as f64
        } else {
            10.0
        };
        let delay_ms = (1000.0 / rate).round().clamp(20.0, 60_000.0) as u32;

        each_frame(data, &o, |_, _, img| {
            let frame =
                GifFrame::from_parts(img.buf, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
            enc.encode_frame(frame)
                .map_err(|e| format!("gif encode failed: {e}"))
        })?
    };
    let _ = meta;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 64x48 H.264 clip: 10 frames at 8 fps, an 8px white bar sitting in
    /// block `i mod 8` of frame `i`. Encoded by jcodec, checked in so this test
    /// never needs an encoder.
    fn clip() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/bars.mp4"
        ))
        .expect("test fixture bars.mp4")
    }

    /// Which 8px block the white bar occupies, by scanning a middle row.
    fn bar_block(rgba: &[u8], w: usize, h: usize) -> Option<usize> {
        let row = h / 2;
        (0..w)
            .find(|x| rgba[(row * w + x) * 4] > 128)
            .map(|x| x / 8)
    }

    #[test]
    fn probes_without_decoding() {
        let v = probe(&clip()).unwrap();
        assert_eq!(v["format"], "mp4");
        assert_eq!(v["codec_kind"], "h264");
        assert_eq!(v["is_decodable"], true);
        assert_eq!(v["width"], 64);
        assert_eq!(v["height"], 48);
        assert_eq!(v["frames"], 10);
        assert_eq!(v["duration_s"], 1.25);
        assert_eq!(v["fps"], 8.0);
        assert_eq!(v["has_audio"], false);
    }

    #[test]
    fn rejects_non_mp4() {
        assert!(probe(b"not a video at all").is_err());
        assert!(!is_mp4(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn decodes_every_frame_in_display_order() {
        let v = decode(&clip(), &json!({})).unwrap();
        assert_eq!(v["frames"], 10);
        assert_eq!(v["width"], 64);
        assert_eq!(v["height"], 48);
        let b64 = base64::engine::general_purpose::STANDARD;
        for (i, f) in v["frames_data"].as_array().unwrap().iter().enumerate() {
            let rgba = b64.decode(f["data"].as_str().unwrap()).unwrap();
            assert_eq!(rgba.len(), 64 * 48 * 4);
            assert_eq!(bar_block(&rgba, 64, 48), Some(i % 8), "frame {i}");
        }
    }

    /// A 24-frame single-GOP clip whose luma ramps one step per frame. 24 frames
    /// is past `MaxFrameNum` (16), so a decoder that ranks pictures by the
    /// bitstream's wrapping picture order count interleaves the two halves of the
    /// GOP — which the ramp exposes as a non-monotonic sequence.
    fn ramp() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/ramp24.mp4"
        ))
        .expect("test fixture ramp24.mp4")
    }

    #[test]
    fn keeps_display_order_past_the_frame_num_wrap() {
        let v = decode(&ramp(), &json!({})).unwrap();
        assert_eq!(v["frames"], 24);
        let b64 = base64::engine::general_purpose::STANDARD;
        let levels: Vec<u8> = v["frames_data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let rgba = b64.decode(f["data"].as_str().unwrap()).unwrap();
                rgba[(24 * 64 + 32) * 4]
            })
            .collect();
        assert!(
            levels.windows(2).all(|w| w[0] < w[1]),
            "luma ramp must stay monotonic in display order, got {levels:?}"
        );
    }

    /// A 27-frame lossless (`-qp 0`) single-GOP clip built from three distinct
    /// noise images cycled A,B,C,A,B,C… and encoded with `-refs 3 -bf 0` and
    /// `weightp=0`. Noise is expensive to intra-code, so the encoder always
    /// predicts from the matching picture three frames back — i.e. from
    /// `ref_idx_l0 > 0`. Being lossless, every decoded frame `i` must come back
    /// byte-identical to frame `i % 3`, with no external reference decoder
    /// needed to say so.
    fn refcycle() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test/fixtures/refcycle27.mp4"
        ))
        .expect("test fixture refcycle27.mp4")
    }

    /// Guards the reference *list order*, not just output order: past picture 16
    /// `frame_num` wraps, and ranking the list by raw picture order count puts
    /// the oldest reference at index 0, so every `ref_idx_l0 > 0` block predicts
    /// from the wrong picture. The cycle makes that a visible mismatch.
    #[test]
    fn picks_the_right_reference_past_the_frame_num_wrap() {
        let v = decode(&refcycle(), &json!({})).unwrap();
        assert_eq!(v["frames"], 27);
        let b64 = base64::engine::general_purpose::STANDARD;
        let planes: Vec<Vec<u8>> = v["frames_data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| b64.decode(f["data"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(planes.len(), 27);
        // The three cycle members must really differ, or the check is vacuous.
        assert_ne!(planes[0], planes[1]);
        assert_ne!(planes[1], planes[2]);
        for (i, frame) in planes.iter().enumerate() {
            let mismatched = frame
                .iter()
                .zip(planes[i % 3].iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(mismatched, 0, "frame {i} must equal frame {}", i % 3);
        }
    }

    #[test]
    fn honours_max_frames_stride_and_scaling() {
        let v = decode(
            &clip(),
            &json!({"max_frames": 3, "stride": 2, "max_dimension": 32}),
        )
        .unwrap();
        assert_eq!(v["frames"], 3);
        assert_eq!(v["total_frames"], 10);
        assert_eq!(v["width"], 32);
        assert_eq!(v["height"], 24);
        assert_eq!(v["source_width"], 64);
        let idx: Vec<u64> = v["frames_data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["index"].as_u64().unwrap())
            .collect();
        assert_eq!(idx, vec![0, 2, 4]);
    }

    #[test]
    fn encodes_frames_as_png() {
        let v = decode(&clip(), &json!({"encoding": "png", "max_frames": 1})).unwrap();
        assert_eq!(v["encoding"], "png");
        let b64 = base64::engine::general_purpose::STANDARD;
        let png = b64
            .decode(v["frames_data"][0]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn transcodes_to_animated_gif() {
        let gif = to_gif(&clip(), &json!({})).unwrap();
        assert_eq!(&gif[..6], b"GIF89a");
        let back = imgops::decode_gif(&gif).unwrap();
        assert_eq!(back["width"], 64);
        assert_eq!(back["height"], 48);
        assert_eq!(back["frames"].as_array().unwrap().len(), 10);
        assert_eq!(back["loop_count"], -1);
        // 8 fps -> 125 ms per frame, which GIF cannot store: its delay field is
        // in CENTIseconds, so 125 ms is written as 12 cs and reads back as 120.
        assert_eq!(back["frames"][0]["delay_ms"], 120);
    }
}
