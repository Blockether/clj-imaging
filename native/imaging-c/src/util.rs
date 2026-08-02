//! Error channel, owned byte buffers, colour parsing — the pieces every module
//! and the C ABI share.

use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::ptr;

use resvg::tiny_skia;

thread_local! {
    /// Why the LAST call on this thread failed. Cleared at the entry of every
    /// public entry point so a stale failure is never reported.
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

pub fn set_error(message: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(message.into()));
}

pub fn take_error() -> Option<String> {
    LAST_ERROR.with(|e| e.borrow_mut().take())
}

pub fn into_c_string(s: impl Into<Vec<u8>>) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// An owned byte buffer handed to the caller as an opaque handle. The caller
/// reads `imaging_buf_ptr`/`imaging_buf_len` and then frees with
/// `imaging_buf_free` — no length out-parameters, no double allocation.
pub struct Buf {
    pub data: Vec<u8>,
}

impl Buf {
    pub fn into_raw(data: Vec<u8>) -> *mut Buf {
        Box::into_raw(Box::new(Buf { data }))
    }
}

/// Result shorthand: every internal function reports failure as a message that
/// the ABI layer parks in the thread-local error channel.
pub type R<T> = Result<T, String>;

pub fn err<T>(msg: impl Into<String>) -> R<T> {
    Err(msg.into())
}

// ---------------------------------------------------------------------------
// colour
// ---------------------------------------------------------------------------

/// A non-premultiplied RGBA8 colour — the representation every public surface
/// (JSON, `u32` ABI arguments, pixel accessors) uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba(0, 0, 0, 0);
    pub const BLACK: Rgba = Rgba(0, 0, 0, 255);
    pub const WHITE: Rgba = Rgba(255, 255, 255, 255);

    /// From the packed 0xRRGGBBAA the ABI passes as `u32`.
    pub fn from_u32(v: u32) -> Rgba {
        Rgba(
            ((v >> 24) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
        )
    }

    pub fn to_u32(self) -> u32 {
        ((self.0 as u32) << 24) | ((self.1 as u32) << 16) | ((self.2 as u32) << 8) | self.3 as u32
    }

    pub fn with_alpha(self, a: f32) -> Rgba {
        Rgba(
            self.0,
            self.1,
            self.2,
            (self.3 as f32 * a).clamp(0.0, 255.0) as u8,
        )
    }

    pub fn to_color(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.0, self.1, self.2, self.3)
    }

    /// Blend towards `other` by `t` ∈ [0,1] — the primitive behind colour maps,
    /// tints and hover-free chart shading.
    pub fn mix(self, other: Rgba, t: f32) -> Rgba {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| {
            (a as f32 + (b as f32 - a as f32) * t)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Rgba(
            m(self.0, other.0),
            m(self.1, other.1),
            m(self.2, other.2),
            m(self.3, other.3),
        )
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}{:02x}", self.0, self.1, self.2, self.3)
    }
}

fn named(name: &str) -> Option<Rgba> {
    let c = match name {
        "transparent" | "none" => Rgba::TRANSPARENT,
        "black" => Rgba::BLACK,
        "white" => Rgba::WHITE,
        "red" => Rgba(255, 0, 0, 255),
        "green" => Rgba(0, 128, 0, 255),
        "lime" => Rgba(0, 255, 0, 255),
        "blue" => Rgba(0, 0, 255, 255),
        "yellow" => Rgba(255, 255, 0, 255),
        "cyan" | "aqua" => Rgba(0, 255, 255, 255),
        "magenta" | "fuchsia" => Rgba(255, 0, 255, 255),
        "gray" | "grey" => Rgba(128, 128, 128, 255),
        "lightgray" | "lightgrey" => Rgba(211, 211, 211, 255),
        "darkgray" | "darkgrey" => Rgba(169, 169, 169, 255),
        "orange" => Rgba(255, 165, 0, 255),
        "purple" => Rgba(128, 0, 128, 255),
        "brown" => Rgba(165, 42, 42, 255),
        "pink" => Rgba(255, 192, 203, 255),
        "navy" => Rgba(0, 0, 128, 255),
        "teal" => Rgba(0, 128, 128, 255),
        "olive" => Rgba(128, 128, 0, 255),
        "silver" => Rgba(192, 192, 192, 255),
        "maroon" => Rgba(128, 0, 0, 255),
        _ => return None,
    };
    Some(c)
}

/// Parse a colour the way every sane graphics API does: `#rgb`, `#rgba`,
/// `#rrggbb`, `#rrggbbaa`, `rgb(…)`, `rgba(…)`, or a CSS basic colour name.
pub fn parse_color(s: &str) -> R<Rgba> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let h =
            |i: usize, n: usize| u8::from_str_radix(&hex[i..i + n], 16).map_err(|e| e.to_string());
        return match hex.len() {
            3 | 4 => {
                let d = |i: usize| h(i, 1).map(|v| v * 17);
                Ok(Rgba(
                    d(0)?,
                    d(1)?,
                    d(2)?,
                    if hex.len() == 4 { d(3)? } else { 255 },
                ))
            }
            6 | 8 => Ok(Rgba(
                h(0, 2)?,
                h(2, 2)?,
                h(4, 2)?,
                if hex.len() == 8 { h(6, 2)? } else { 255 },
            )),
            _ => err(format!("bad hex colour: {s}")),
        };
    }
    let lower = s.to_ascii_lowercase();
    if let Some(c) = named(&lower) {
        return Ok(c);
    }
    // OOXML (and plenty of hand-written specs) spell colours bare: `RRGGBB`.
    // Named colours win, so this only sees strings that are pure hex digits.
    if matches!(s.len(), 3 | 4 | 6 | 8) && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return parse_color(&format!("#{s}"));
    }
    if let Some(rest) = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
    {
        let rest = rest.trim_end_matches(')');
        let parts: Vec<&str> = rest
            .split(&[',', ' '][..])
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() == 3 || parts.len() == 4 {
            let n = |p: &str| p.trim().parse::<f32>().map_err(|e| e.to_string());
            let a = if parts.len() == 4 {
                let a = n(parts[3])?;
                (if a <= 1.0 { a * 255.0 } else { a }).clamp(0.0, 255.0) as u8
            } else {
                255
            };
            return Ok(Rgba(
                n(parts[0])?.clamp(0.0, 255.0) as u8,
                n(parts[1])?.clamp(0.0, 255.0) as u8,
                n(parts[2])?.clamp(0.0, 255.0) as u8,
                a,
            ));
        }
    }
    err(format!("unrecognised colour: {s}"))
}

/// Colour out of arbitrary JSON: a string, `[r,g,b]`, `[r,g,b,a]`, or a number
/// (packed 0xRRGGBBAA).
pub fn color_from_json(v: &serde_json::Value) -> R<Rgba> {
    match v {
        serde_json::Value::String(s) => parse_color(s),
        serde_json::Value::Number(n) => Ok(Rgba::from_u32(n.as_u64().unwrap_or(0) as u32)),
        serde_json::Value::Array(a) if a.len() == 3 || a.len() == 4 => {
            let n = |i: usize| a[i].as_f64().unwrap_or(0.0).clamp(0.0, 255.0) as u8;
            Ok(Rgba(
                n(0),
                n(1),
                n(2),
                if a.len() == 4 { n(3) } else { 255 },
            ))
        }
        serde_json::Value::Null => Ok(Rgba::TRANSPARENT),
        other => err(format!("not a colour: {other}")),
    }
}

// ---------------------------------------------------------------------------
// pixmap <-> straight RGBA
// ---------------------------------------------------------------------------

/// Straight (non-premultiplied) RGBA8 rows -> a tiny-skia pixmap.
pub fn to_pixmap(w: u32, h: u32, rgba: &[u8]) -> R<tiny_skia::Pixmap> {
    let mut pm = tiny_skia::Pixmap::new(w.max(1), h.max(1))
        .ok_or_else(|| format!("cannot allocate a {w}x{h} pixmap"))?;
    for (px, src) in pm.pixels_mut().iter_mut().zip(rgba.chunks_exact(4)) {
        *px = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
    }
    Ok(pm)
}

/// A tiny-skia pixmap back to straight RGBA8 rows.
pub fn from_pixmap(pm: &tiny_skia::Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity((pm.width() * pm.height() * 4) as usize);
    for px in pm.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

// ---------------------------------------------------------------------------
// small JSON accessors
// ---------------------------------------------------------------------------

pub fn jf(v: &serde_json::Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

pub fn jf_or(v: &serde_json::Value, k: &str, d: f64) -> f64 {
    jf(v, k).unwrap_or(d)
}

pub fn js<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

pub fn jb(v: &serde_json::Value, k: &str, d: bool) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(d)
}

pub fn jcolor(v: &serde_json::Value, k: &str) -> Option<Rgba> {
    v.get(k).and_then(|x| color_from_json(x).ok())
}

pub fn jnums(v: &serde_json::Value, k: &str) -> Option<Vec<f64>> {
    v.get(k)?
        .as_array()
        .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect())
}

pub fn jpoints(v: &serde_json::Value, k: &str) -> Option<Vec<(f32, f32)>> {
    Some(
        v.get(k)?
            .as_array()?
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32))
            })
            .collect(),
    )
}
