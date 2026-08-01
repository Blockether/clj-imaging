//! Colour quantisation: full-colour RGBA down to an indexed palette.
//!
//! Median cut (the same algorithm Pillow's `convert("P")` uses) with optional
//! Floyd–Steinberg error diffusion. This is what makes an indexed PNG or a GIF
//! frame possible at all, and it is the lossy half of `optimize`: dropping
//! 16 million colours to 64 well-chosen ones is worth far more bytes than any
//! DEFLATE setting.
//!
//! Fully see-through pixels never enter the palette. When the source has any,
//! the LAST slot is reserved as the transparent index, exactly as GIF does.

use std::collections::HashMap;

use crate::imgops::Image;

/// A pixel is "see-through" (and so quantises to the transparent index) below
/// this alpha. Palette formats have one bit of alpha, so the cut has to be
/// somewhere; Pillow puts it here too.
const ALPHA_CUTOFF: u8 = 128;

pub struct Quantized {
    /// Packed `0xRRGGBB` per slot. The transparent slot (when present) is 0.
    pub palette: Vec<u32>,
    /// One palette index per pixel, row major.
    pub indices: Vec<u8>,
    /// Index reserved for see-through pixels, when the source had any.
    pub transparent: Option<usize>,
    /// The quantised image as straight RGBA8 — the palette applied back.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Distinct opaque colours and how often each occurs.
fn counts(rgba: &[u8], alpha_min: u8) -> Vec<(u32, u32)> {
    let mut m: HashMap<u32, u32> = HashMap::new();
    for px in rgba.chunks_exact(4) {
        if px[3] < alpha_min {
            continue;
        }
        let key = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | px[2] as u32;
        *m.entry(key).or_insert(0) += 1;
    }
    m.into_iter().collect()
}

struct Cube {
    cols: Vec<(u32, u32)>,
    weight: u64,
    lo: [u32; 3],
    hi: [u32; 3],
}

fn chan(c: u32, i: usize) -> u32 {
    (c >> (16 - 8 * i as u32)) & 255
}

impl Cube {
    fn new(cols: Vec<(u32, u32)>) -> Cube {
        let mut lo = [255u32; 3];
        let mut hi = [0u32; 3];
        let mut weight = 0u64;
        for (c, n) in &cols {
            weight += *n as u64;
            for i in 0..3 {
                let v = chan(*c, i);
                lo[i] = lo[i].min(v);
                hi[i] = hi[i].max(v);
            }
        }
        Cube {
            cols,
            weight,
            lo,
            hi,
        }
    }

    /// The channel this cube is widest in — the one worth cutting.
    fn longest(&self) -> usize {
        let mut best = 0;
        for i in 1..3 {
            if self.hi[i].saturating_sub(self.lo[i]) > self.hi[best].saturating_sub(self.lo[best]) {
                best = i;
            }
        }
        best
    }

    /// Cut priority: how many pixels are misrepresented by how much.
    fn score(&self) -> u64 {
        let i = self.longest();
        self.weight * (self.hi[i].saturating_sub(self.lo[i]) as u64 + 1)
    }

    fn splittable(&self) -> bool {
        self.cols.len() > 1 && {
            let i = self.longest();
            self.hi[i] > self.lo[i]
        }
    }

    /// The pixel-weighted mean colour — the palette entry this cube becomes.
    fn average(&self) -> u32 {
        if self.weight == 0 {
            return 0;
        }
        let mut acc = [0u64; 3];
        for (c, n) in &self.cols {
            for (i, a) in acc.iter_mut().enumerate() {
                *a += chan(*c, i) as u64 * *n as u64;
            }
        }
        let mut out = 0u32;
        for a in acc {
            out = (out << 8) | ((a / self.weight).min(255) as u32);
        }
        out
    }

    /// Split at the weighted median of the widest channel.
    fn split(mut self) -> (Cube, Cube) {
        let i = self.longest();
        self.cols.sort_by_key(|(c, _)| chan(*c, i));
        let half = self.weight / 2;
        let mut acc = 0u64;
        let mut at = 0usize;
        for (k, (_, n)) in self.cols.iter().enumerate() {
            acc += *n as u64;
            at = k + 1;
            if acc >= half {
                break;
            }
        }
        let at = at.clamp(1, self.cols.len() - 1);
        let right = self.cols.split_off(at);
        (Cube::new(self.cols), Cube::new(right))
    }
}

/// Median-cut `cols` (colour -> pixel count) down to at most `n` entries.
pub fn median_cut(cols: Vec<(u32, u32)>, n: usize) -> Vec<u32> {
    let n = n.max(1);
    if cols.is_empty() {
        return vec![0];
    }
    let mut cubes = vec![Cube::new(cols)];
    while cubes.len() < n {
        let Some(at) = cubes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.splittable())
            .max_by_key(|(_, c)| c.score())
            .map(|(i, _)| i)
        else {
            break;
        };
        let (a, b) = cubes.swap_remove(at).split();
        cubes.push(a);
        cubes.push(b);
    }
    let mut pal: Vec<u32> = cubes.iter().map(Cube::average).collect();
    pal.sort_unstable();
    pal.dedup();
    pal
}

/// Index of the palette entry closest to `want` in RGB space.
fn nearest(pal: &[u32], want: u32) -> u8 {
    let (wr, wg, wb) = (
        chan(want, 0) as i32,
        chan(want, 1) as i32,
        chan(want, 2) as i32,
    );
    let mut best = 0usize;
    let mut best_d = i32::MAX;
    for (i, c) in pal.iter().enumerate() {
        let dr = wr - chan(*c, 0) as i32;
        let dg = wg - chan(*c, 1) as i32;
        let db = wb - chan(*c, 2) as i32;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i;
            if d == 0 {
                break;
            }
        }
    }
    best as u8
}

fn clamp255(v: f32) -> u32 {
    v.round().clamp(0.0, 255.0) as u32
}

/// Quantise `img` to at most `ncolors` colours, optionally dithering.
pub fn quantize(img: &Image, ncolors: usize, dither: bool) -> Quantized {
    let (w, h) = (img.width(), img.height());
    let src = img.buf.as_raw();
    let see_through = src.chunks_exact(4).any(|p| p[3] < ALPHA_CUTOFF);
    let room = ncolors
        .clamp(1, 256)
        .saturating_sub(usize::from(see_through))
        .max(1);

    let mut palette = median_cut(
        counts(src, if see_through { ALPHA_CUTOFF } else { 0 }),
        room,
    );
    let transparent = see_through.then(|| {
        palette.push(0);
        palette.len() - 1
    });

    let wn = w as usize;
    let mut indices = vec![0u8; wn * h as usize];
    let mut rgba = vec![0u8; wn * h as usize * 4];
    let mut cache: HashMap<u32, u8> = HashMap::new();
    // Floyd–Steinberg carries error into the current row and the next one.
    let mut err_cur = vec![0f32; 3 * (wn + 2)];
    let mut err_next = vec![0f32; 3 * (wn + 2)];

    for y in 0..h as usize {
        err_next.iter_mut().for_each(|e| *e = 0.0);
        for x in 0..wn {
            let o = (y * wn + x) * 4;
            let px = &src[o..o + 4];
            if let Some(t) = transparent
                && px[3] < ALPHA_CUTOFF
            {
                indices[y * wn + x] = t as u8;
                continue; // rgba stays 0,0,0,0
            }
            let i = 3 * (x + 1);
            let (r, g, b) = if dither {
                (
                    px[0] as f32 + err_cur[i],
                    px[1] as f32 + err_cur[i + 1],
                    px[2] as f32 + err_cur[i + 2],
                )
            } else {
                (px[0] as f32, px[1] as f32, px[2] as f32)
            };
            let want = (clamp255(r) << 16) | (clamp255(g) << 8) | clamp255(b);
            let idx = *cache.entry(want).or_insert_with(|| nearest(&palette, want));
            let q = palette[idx as usize];
            indices[y * wn + x] = idx;
            rgba[o] = chan(q, 0) as u8;
            rgba[o + 1] = chan(q, 1) as u8;
            rgba[o + 2] = chan(q, 2) as u8;
            rgba[o + 3] = 255;
            if dither {
                let e = [
                    r - chan(q, 0) as f32,
                    g - chan(q, 1) as f32,
                    b - chan(q, 2) as f32,
                ];
                for (c, e) in e.iter().enumerate() {
                    err_cur[i + 3 + c] += e * 0.4375; // 7/16 →
                    err_next[i - 3 + c] += e * 0.1875; // 3/16 ↙
                    err_next[i + c] += e * 0.3125; // 5/16 ↓
                    err_next[i + 3 + c] += e * 0.0625; // 1/16 ↘
                }
            }
        }
        std::mem::swap(&mut err_cur, &mut err_next);
    }

    Quantized {
        palette,
        indices,
        transparent,
        rgba,
        width: w,
        height: h,
    }
}

/// `quantize` as the JSON the FFI hands back: palette ints, base64 indices and
/// base64 RGBA of the quantised result.
pub fn quantize_json(img: &Image, ncolors: usize, dither: bool) -> serde_json::Value {
    use base64::Engine as _;
    let q = quantize(img, ncolors, dither);
    let b64 = base64::engine::general_purpose::STANDARD;
    serde_json::json!({
        "width": q.width,
        "height": q.height,
        "palette": q.palette,
        "transparent": q.transparent,
        "indices": b64.encode(&q.indices),
        "rgba": b64.encode(&q.rgba),
    })
}
