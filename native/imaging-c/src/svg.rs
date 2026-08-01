//! SVG rasterisation through resvg — the replacement for jsvg + Graphics2D.
//!
//! Beyond plain rendering this module owns the BROWSER-COMPATIBLE sizing of a
//! document resvg refuses to size the way a viewer does ([`canvas`]): a zero or
//! negative declared size, and a document that declares no size at all. That
//! repair is ON by default ([`render`] with `repair: false` opts out), because
//! a caller rasterising user-supplied artwork wants the picture a browser
//! shows, not resvg's refusal.

use std::io::Read;

use resvg::tiny_skia;
use serde_json::Value;

use crate::imgops::Image;
use crate::text;
use crate::util::{R, Rgba, err, jb, jf, js};

/// How the caller wants the document sized.
#[derive(Clone, Copy, Debug, Default)]
pub struct SvgSize {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub scale: Option<f32>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
}

/// SVG's own default viewport, and the LAST resort: used only for a document
/// that declares no size, carries no `viewBox`, and paints nothing measurable.
const DEFAULT_CANVAS: (f64, f64) = (300.0, 150.0);

/// Side of the throwaway square a size-less document is MEASURED on. Big enough
/// that a one-unit shape still covers a pixel, small enough that the scan is a
/// megapixel and not a memory event.
const PROBE_CANVAS: u32 = 1024;

/// Long edge every ink-derived canvas is grown to. A 12x8 icon measured exactly
/// is a correct crop and a useless picture — nothing downstream upscales, so a
/// document sized from its own shapes still has to arrive legible.
const DEFAULT_MIN_DIMENSION: f64 = 512.0;

/// Ceiling for an ink-derived canvas: a vector document can declare any size at
/// all, and a 40000px canvas is an OOM, not a picture.
const DEFAULT_MAX_DIMENSION: f64 = 4096.0;

/// usvg parsing options from the shared option map.
fn options(opts: &Value) -> usvg::Options<'static> {
    let mut o = usvg::Options {
        fontdb: text::database(),
        ..usvg::Options::default()
    };
    if let Some(dpi) = jf(opts, "dpi") {
        o.dpi = dpi as f32;
    }
    if let Some(f) = js(opts, "font_family") {
        o.font_family = f.to_string();
    } else {
        o.font_family = text::DEFAULT_FAMILY.to_string();
    }
    if let Some(s) = jf(opts, "font_size") {
        o.font_size = s as f32;
    }
    if let Some(dir) = js(opts, "base_url") {
        o.resources_dir = Some(std::path::PathBuf::from(dir));
    }
    o
}

/// Rasterise SVG (or gzipped SVGZ) bytes.
///
/// The canvas is REPAIRED by default: a document with a zero, negative or
/// absent size is sized the way a BROWSER would ([`canvas`]) instead of being
/// refused by resvg's size check. Pass `repair: false` for resvg's literal
/// answer.
///
/// Options JSON: `width`, `height`, `scale`, `max_width`, `max_height`,
/// `background` (colour, default transparent), `dpi`, `font_family`,
/// `font_size`, `base_url` (directory used to resolve relative `href`s),
/// `repair` (default true, see [`canvas`]) plus that repair's
/// `min_dimension` / `max_dimension` band.
pub fn render(data: &[u8], opts: &Value) -> R<Image> {
    if jb(opts, "repair", true) {
        let c = canvas(data, opts)?;
        if let Some(markup) = c.markup {
            return render_exact(&markup, opts);
        }
    }
    render_exact(data, opts)
}

fn render_exact(data: &[u8], opts: &Value) -> R<Image> {
    let o = options(opts);
    let tree = usvg::Tree::from_data(data, &o).map_err(|e| format!("invalid SVG: {e}"))?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return err("SVG has a zero-sized viewport");
    }

    let want = SvgSize {
        width: jf(opts, "width").map(|v| v as f32),
        height: jf(opts, "height").map(|v| v as f32),
        scale: jf(opts, "scale").map(|v| v as f32),
        max_width: jf(opts, "max_width").map(|v| v as f32),
        max_height: jf(opts, "max_height").map(|v| v as f32),
    };
    let (sx, sy) = fit(size.width(), size.height(), &want);
    let w = (size.width() * sx).round().max(1.0) as u32;
    let h = (size.height() * sy).round().max(1.0) as u32;
    if w > 20_000 || h > 20_000 {
        return err(format!(
            "refusing to rasterise a {w}x{h} SVG (max 20000 per side)"
        ));
    }

    let mut pm = tiny_skia::Pixmap::new(w, h).ok_or_else(|| format!("cannot allocate {w}x{h}"))?;
    if let Some(bg) = opts
        .get("background")
        .and_then(|v| crate::util::color_from_json(v).ok())
        && bg != Rgba::TRANSPARENT
    {
        pm.fill(bg.to_color());
    }
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(sx, sy),
        &mut pm.as_mut(),
    );
    Image::from_pixmap(&pm)
}

fn fit(w: f32, h: f32, want: &SvgSize) -> (f32, f32) {
    let mut sx = 1.0;
    let mut sy = 1.0;
    match (want.width, want.height) {
        (Some(tw), Some(th)) => {
            sx = tw / w;
            sy = th / h;
        }
        (Some(tw), None) => {
            sx = tw / w;
            sy = sx;
        }
        (None, Some(th)) => {
            sy = th / h;
            sx = sy;
        }
        (None, None) => {
            if let Some(s) = want.scale {
                sx = s;
                sy = s;
            }
        }
    }
    if want.width.is_none() && want.height.is_none() {
        if let Some(mw) = want.max_width
            && w * sx > mw
        {
            let k = mw / (w * sx);
            sx *= k;
            sy *= k;
        }
        if let Some(mh) = want.max_height
            && h * sy > mh
        {
            let k = mh / (h * sy);
            sx *= k;
            sy *= k;
        }
    }
    (sx.max(0.001), sy.max(0.001))
}

/// True when the bytes look like an SVG (or SVGZ) document.
pub fn looks_like_svg(data: &[u8]) -> bool {
    if data.starts_with(&[0x1f, 0x8b]) {
        return true; // svgz — gzip; usvg unzips it itself
    }
    let head = &data[..data.len().min(1024)];
    let s = String::from_utf8_lossy(head);
    let s = s.trim_start();
    s.starts_with("<svg")
        || (s.starts_with("<?xml") && s.contains("<svg"))
        || s.contains("<!DOCTYPE svg")
}

// ---------------------------------------------------------------------------
// Browser-compatible sizing
// ---------------------------------------------------------------------------

/// The canvas a viewer would give a document: its resolved size, where that
/// size came from, and — when the document had to be repaired to get one — the
/// rewritten markup to render instead of the original bytes.
#[derive(Clone, Debug)]
pub struct Canvas {
    pub width: f64,
    pub height: f64,
    /// `declared` (resvg's own answer), `view_box`, `content` (framed by the
    /// ink the document paints) or `default` (SVG's 300x150).
    pub source: &'static str,
    /// Rewritten document, or None when the original bytes size correctly.
    pub markup: Option<Vec<u8>>,
}

/// Size a document the way a viewer does.
///
/// resvg is stricter than the SVG spec in exactly two places, and both produce
/// a picture no one asked for:
///
///   * a zero or negative declared size is REFUSED outright ("SVG has an
///     invalid size") where browsers fall back to the `viewBox`;
///   * a document with no size at all gets resvg's 100x100 default, and its
///     bounds run from the ORIGIN — a figure at x=50 gains a 50px margin, and
///     content at negative coordinates is lost entirely.
///
/// Both are repaired by restating the root tag's size: the `viewBox` is the
/// author's own frame and wins; without one the document is rendered onto a
/// throwaway canvas, the INK it actually paints is measured, and that window
/// becomes the repaired root's `viewBox` at a size ([`fit_canvas`]) that is
/// neither a thumbnail nor a wall.
///
/// Options: `min_dimension` (default 512) and `max_dimension` (default 4096)
/// bound an ink-derived canvas; the parsing options of [`render`] apply too.
pub fn canvas(data: &[u8], opts: &Value) -> R<Canvas> {
    let markup = markup_of(data)?;
    let tag = root_tag(&markup);
    let declares_size = tag.as_ref().is_some_and(|t| {
        attr(t, "width").is_some() || attr(t, "height").is_some() || attr(t, "viewBox").is_some()
    });

    let o = options(opts);
    let probed = usvg::Tree::from_data(data, &o)
        .ok()
        .and_then(|t| usable(t.size().width() as f64, t.size().height() as f64));

    if let Some((w, h)) = probed
        && declares_size
    {
        return Ok(Canvas {
            width: w,
            height: h,
            source: "declared",
            markup: None,
        });
    }

    let Some(tag) = tag else {
        return err("not an SVG document: no <svg> root element");
    };

    let box_ = view_box_extent(&tag);
    let ink = if box_.is_none() {
        content_window(&markup, &tag, &o)
    } else {
        None
    };
    let min_dim = jf(opts, "min_dimension").unwrap_or(DEFAULT_MIN_DIMENSION);
    let max_dim = jf(opts, "max_dimension").unwrap_or(DEFAULT_MAX_DIMENSION);

    let (w, h, source, ink) = match (box_, ink) {
        (Some((w, h)), _) => (w, h, "view_box", None),
        (None, Some(b)) => {
            let (w, h) = fit_canvas(b[2], b[3], min_dim, max_dim);
            (
                w,
                h,
                "content",
                Some(device_margin(b, w, h, DEVICE_MARGIN_PX)),
            )
        }
        (None, None) => (DEFAULT_CANVAS.0, DEFAULT_CANVAS.1, "default", None),
    };

    let mut set = vec![
        ("width".to_string(), format!("{}", w as i64)),
        ("height".to_string(), format!("{}", h as i64)),
    ];
    if let Some(b) = ink {
        set.push((
            "viewBox".to_string(),
            format!("{} {} {} {}", b[0], b[1], b[2], b[3]),
        ));
    }

    Ok(Canvas {
        width: w,
        height: h,
        source,
        markup: Some(retag(&markup, &tag, &set).into_bytes()),
    })
}

/// Background kept around the ink of a size-less document, in DEVICE pixels of
/// the final canvas.
const DEVICE_MARGIN_PX: f64 = 2.0;

/// Grow the ink window `b` so at least `px` device pixels of background survive
/// on every side of a `w`x`h` canvas.
///
/// [`ink_extent`] already pads by two PROBE pixels, but a small figure is
/// upscaled onto the canvas and that pad shrinks with it — a 400x200 rect ends
/// up with a margin thinner than one output pixel, so its own antialiased edge
/// lands in the corner pixel. The canvas size is untouched here: only the
/// window into the document widens, so the ink shrinks INSIDE the frame it was
/// just fitted to.
fn device_margin(b: [f64; 4], w: f64, h: f64, px: f64) -> [f64; 4] {
    let mx = px * b[2] / w.max(1.0);
    let my = px * b[3] / h.max(1.0);
    [b[0] - mx, b[1] - my, b[2] + 2.0 * mx, b[3] + 2.0 * my]
}

/// [`canvas`] as the JSON the FFI hands back.
pub fn canvas_json(data: &[u8], opts: &Value) -> R<Value> {
    use base64::Engine as _;
    let c = canvas(data, opts)?;
    Ok(serde_json::json!({
        "width": c.width,
        "height": c.height,
        "source": c.source,
        "is_repaired": c.markup.is_some(),
        "svg": c.markup.map(|m| base64::engine::general_purpose::STANDARD.encode(m)),
    }))
}

/// The document as markup, transparently gunzipping the `.svgz` container.
fn markup_of(data: &[u8]) -> R<String> {
    if data.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut out)
            .map_err(|e| format!("invalid SVGZ: {e}"))?;
        return Ok(String::from_utf8_lossy(&out).into_owned());
    }
    Ok(String::from_utf8_lossy(data).into_owned())
}

/// The root `<svg …>` element: where it sits in the markup and what it declares.
#[derive(Clone, Debug)]
struct Tag {
    start: usize,
    end: usize,
    is_self_closing: bool,
    attrs: Vec<(String, String)>,
}

fn root_tag(markup: &str) -> Option<Tag> {
    let lower = markup.to_ascii_lowercase();
    let bytes = markup.as_bytes();
    let mut from = 0usize;
    let start = loop {
        let i = lower[from..].find("<svg")? + from;
        match bytes.get(i + 4) {
            None => return None,
            Some(&b) if b.is_ascii_whitespace() || b == b'>' || b == b'/' => break i,
            _ => from = i + 4,
        }
    };

    let mut j = start + 4;
    let mut quote: Option<u8> = None;
    while j < bytes.len() {
        let b = bytes[j];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => break,
            None => {}
        }
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    let inner = &markup[start + 4..j];
    let is_self_closing = inner.trim_end().ends_with('/');
    Some(Tag {
        start,
        end: j + 1,
        is_self_closing,
        attrs: parse_attrs(inner),
    })
}

fn parse_attrs(inner: &str) -> Vec<(String, String)> {
    let b = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if i == ks {
            break;
        }
        let name = inner[ks..i].to_string();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                out.push((name, inner[vs..i].to_string()));
                i = (i + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push((name, inner[vs..i].to_string()));
            }
        } else {
            out.push((name, String::new()));
        }
    }
    out
}

fn attr<'a>(tag: &'a Tag, name: &str) -> Option<&'a str> {
    tag.attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// `markup` with the root tag's `set` attributes replaced (and its `width` /
/// `height` always restated), everything else left exactly as written.
fn retag(markup: &str, tag: &Tag, set: &[(String, String)]) -> String {
    let mut s = String::with_capacity(markup.len() + 64);
    s.push_str(&markup[..tag.start]);
    s.push_str("<svg");
    for (k, v) in &tag.attrs {
        if k.eq_ignore_ascii_case("width")
            || k.eq_ignore_ascii_case("height")
            || set.iter().any(|(n, _)| n.eq_ignore_ascii_case(k))
        {
            continue;
        }
        s.push(' ');
        s.push_str(k);
        s.push_str("=\"");
        s.push_str(v);
        s.push('"');
    }
    for (k, v) in set {
        s.push(' ');
        s.push_str(k);
        s.push_str("=\"");
        s.push_str(v);
        s.push('"');
    }
    if tag.is_self_closing {
        s.push_str("/>");
    } else {
        s.push('>');
    }
    s.push_str(&markup[tag.end..]);
    s
}

/// `(w, h)` when both are positive and finite — a document may declare 0, a
/// negative or a NaN, and every one of those is a broken canvas, not a size.
fn usable(w: f64, h: f64) -> Option<(f64, f64)> {
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w, h))
}

/// `(w, h)` from the root `viewBox`'s extent — its origin is irrelevant to size.
fn view_box_extent(tag: &Tag) -> Option<(f64, f64)> {
    let vb = attr(tag, "viewBox")?;
    let ns: Vec<f64> = vb
        .split([',', ' ', '\t', '\n', '\r'])
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();
    if ns.len() != 4 {
        return None;
    }
    usable(ns[2], ns[3])
}

/// `(w, h)` scaled — ratio intact — into the band where a raster is worth
/// looking at: never a thumbnail below `min`, never past the `max` ceiling.
fn fit_canvas(w: f64, h: f64, min: f64, max: f64) -> (f64, f64) {
    let longest = w.max(h);
    let scale = if longest < min {
        min / longest
    } else if longest > max {
        max / longest
    } else {
        1.0
    };
    ((w * scale).round().max(1.0), (h * scale).round().max(1.0))
}

/// `[x0 y0 x1 y1]` inclusive pixel bounds of every non-transparent pixel, or
/// None when nothing was painted at all.
fn alpha_bounds(px: &[u8], w: u32, h: u32) -> Option<[u32; 4]> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut seen = false;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) as usize) * 4 + 3;
            if px[i] == 0 {
                continue;
            }
            seen = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    seen.then_some([x0, y0, x1, y1])
}

/// What the document actually PAINTS inside the user-space window
/// `[vx, vx+span] x [vy, vy+span]`: `(box, is_clipped)` in user units, or None
/// when nothing was painted. `is_clipped` means the ink reached a border, i.e.
/// the window was too small and the caller should retry with a different one.
fn ink_extent(
    markup: &str,
    tag: &Tag,
    vx: f64,
    vy: f64,
    span: f64,
    o: &usvg::Options<'_>,
) -> Option<([f64; 4], bool)> {
    let side = PROBE_CANVAS;
    let probe = retag(
        markup,
        tag,
        &[
            ("width".to_string(), side.to_string()),
            ("height".to_string(), side.to_string()),
            ("viewBox".to_string(), format!("{vx} {vy} {span} {span}")),
        ],
    );
    let tree = usvg::Tree::from_data(probe.as_bytes(), o).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }
    let mut pm = tiny_skia::Pixmap::new(side, side)?;
    let sx = side as f32 / size.width();
    let sy = side as f32 / size.height();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(sx, sy),
        &mut pm.as_mut(),
    );
    let [x0, y0, x1, y1] = alpha_bounds(pm.data(), side, side)?;

    let u = span / side as f64;
    // Two probe pixels of margin: the ink's own antialiased edge lands inside
    // the bounds, and a hairline stroke should not touch the frame.
    let pad = 2.0 * u;
    let box_ = [
        vx + x0 as f64 * u - pad,
        vy + y0 as f64 * u - pad,
        (x1 - x0 + 1) as f64 * u + 2.0 * pad,
        (y1 - y0 + 1) as f64 * u + 2.0 * pad,
    ];
    let is_clipped = x0 == 0 || y0 == 0 || x1 == side - 1 || y1 == side - 1;
    Some((box_, is_clipped))
}

/// The user-space `[x y w h]` window that exactly frames a size-less document's
/// shapes, or None.
///
/// Three passes at most. A tight window around the origin covers the ordinary
/// document (and, being centred on nothing, also catches shapes at negative
/// coordinates). A 32x wider one runs when that found ink running off the edge
/// OR no ink at all — the figure may simply live far from the origin. Its box is
/// quantised to 32 units, which is enough to squash the aspect ratio of the very
/// figure it just found, so a third pass re-measures the box at its own scale.
fn content_window(markup: &str, tag: &Tag, o: &usvg::Options<'_>) -> Option<[f64; 4]> {
    let measure = |vx, vy, span| ink_extent(markup, tag, vx, vy, span, o);

    let tight = measure(-256.0, -256.0, 1024.0);
    let coarse = match tight {
        None => measure(-8192.0, -8192.0, 32768.0),
        Some((_, true)) => measure(-8192.0, -8192.0, 32768.0).or(tight),
        Some(_) => tight,
    };
    let refined = coarse.and_then(|([x, y, w, h], _)| {
        let span = 1.1 * w.max(h);
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        measure(cx - span / 2.0, cy - span / 2.0, span)
    });
    refined.or(coarse).or(tight).map(|(b, _)| b)
}
