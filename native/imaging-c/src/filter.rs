//! Neighbourhood filters: the engine behind Pillow's `ImageFilter.Kernel`
//! (arbitrary odd convolution kernels — blur, sharpen, emboss, edge detect) and
//! its rank family (`MedianFilter`, `MinFilter`, `MaxFilter`).
//!
//! Contract, chosen to match Pillow's observable behaviour:
//!
//!   * the window is CORRELATED, not flipped: output(x,y) sums
//!     `kernel[ky*k + kx] * src(x + kx - k/2, y + ky - k/2)`;
//!   * out-of-bounds samples CLAMP to the edge pixel, so a border never darkens
//!     towards transparent black;
//!   * `convolve` touches the colour channels only and carries the source alpha
//!     through untouched — a blur must not eat the mask;
//!   * `scale` 0 means "no division" (1.0), matching Pillow's `scale=None`, and
//!     the result is `offset + sum/scale` rounded half-up then clamped to 0..255;
//!   * `rank` sorts EVERY channel independently, alpha included, and picks the
//!     `rank`-th smallest — index 0 is a min filter, `k*k - 1` a max filter.

use image::{Rgba, RgbaImage};

use crate::imgops::Image;
use crate::util::{R, err};

/// Kernels beyond this are a caller mistake, not a filter: k*k weights and a
/// k*k sort per pixel get quadratically expensive.
const MAX_KERNEL: u32 = 31;

fn window(size: u32, what: &str) -> R<i64> {
    if size == 0 || size % 2 == 0 || size > MAX_KERNEL {
        return err(format!(
            "{what}: window size must be odd and 1..={MAX_KERNEL}, got {size}"
        ));
    }
    Ok(size as i64)
}

/// Round half-up (Java's `Math.round`) and clamp into a byte.
fn clamp_round(v: f64) -> u8 {
    if v.is_nan() {
        return 0;
    }
    let r = (v + 0.5).floor();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        r as u8
    }
}

/// Correlate the colour channels with `kernel`, keeping alpha.
pub fn convolve(img: &Image, size: u32, kernel: &[f64], scale: f64, offset: f64) -> R<Image> {
    let k = window(size, "convolve")?;
    let need = (k * k) as usize;
    if kernel.len() != need {
        return err(format!(
            "convolve: a {k}x{k} kernel needs {need} weights, got {}",
            kernel.len()
        ));
    }
    let sc = if scale == 0.0 { 1.0 } else { scale };
    let (w, h) = (img.buf.width() as i64, img.buf.height() as i64);
    let half = k / 2;
    let mut out = RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f64; 3];
            for ky in 0..k {
                let sy = (y + ky - half).clamp(0, h - 1) as u32;
                for kx in 0..k {
                    let sx = (x + kx - half).clamp(0, w - 1) as u32;
                    let p = img.buf.get_pixel(sx, sy).0;
                    let wgt = kernel[(ky * k + kx) as usize];
                    acc[0] += wgt * p[0] as f64;
                    acc[1] += wgt * p[1] as f64;
                    acc[2] += wgt * p[2] as f64;
                }
            }
            let a = img.buf.get_pixel(x as u32, y as u32).0[3];
            out.put_pixel(
                x as u32,
                y as u32,
                Rgba([
                    clamp_round(offset + acc[0] / sc),
                    clamp_round(offset + acc[1] / sc),
                    clamp_round(offset + acc[2] / sc),
                    a,
                ]),
            );
        }
    }
    Ok(Image { buf: out })
}

/// Per-channel rank filter over a `size` x `size` window.
pub fn rank(img: &Image, size: u32, rank: usize) -> R<Image> {
    let k = window(size, "rank")?;
    let n = (k * k) as usize;
    let idx = rank.min(n - 1);
    let (w, h) = (img.buf.width() as i64, img.buf.height() as i64);
    let half = k / 2;
    let mut out = RgbaImage::new(w as u32, h as u32);
    let mut chans = [vec![0u8; n], vec![0u8; n], vec![0u8; n], vec![0u8; n]];
    for y in 0..h {
        for x in 0..w {
            let mut i = 0usize;
            for ky in 0..k {
                let sy = (y + ky - half).clamp(0, h - 1) as u32;
                for kx in 0..k {
                    let sx = (x + kx - half).clamp(0, w - 1) as u32;
                    let p = img.buf.get_pixel(sx, sy).0;
                    for c in 0..4 {
                        chans[c][i] = p[c];
                    }
                    i += 1;
                }
            }
            for c in &mut chans {
                c.sort_unstable();
            }
            out.put_pixel(
                x as u32,
                y as u32,
                Rgba([chans[0][idx], chans[1][idx], chans[2][idx], chans[3][idx]]),
            );
        }
    }
    Ok(Image { buf: out })
}

/// JSON entry point: `{"kind":"convolve"|"rank"|"median"|"min"|"max",
///  "size":3, "kernel":[...], "scale":0, "offset":0, "rank":N}`.
pub fn filter(img: &Image, opts: &serde_json::Value) -> R<Image> {
    let kind = opts
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("convolve")
        .trim()
        .to_ascii_lowercase();
    let size = opts.get("size").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
    match kind.as_str() {
        "convolve" | "kernel" => {
            let kernel: Vec<f64> = opts
                .get("kernel")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect())
                .unwrap_or_default();
            let scale = opts.get("scale").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let offset = opts.get("offset").and_then(|v| v.as_f64()).unwrap_or(0.0);
            convolve(img, size, &kernel, scale, offset)
        }
        "rank" | "median" | "min" | "max" => {
            let n = (size as usize).saturating_mul(size as usize);
            let default = match kind.as_str() {
                "min" => 0,
                "max" => n.saturating_sub(1),
                _ => n / 2,
            };
            let r = opts
                .get("rank")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(default);
            rank(img, size, r)
        }
        other => err(format!("unknown filter kind: {other}")),
    }
}
