//! Vector drawing on a pixmap: the tiny-skia replacement for `Graphics2D`.
//!
//! `Canvas` is the Rust-side API (used by `plot`), and `draw_ops` executes the
//! same primitives from a JSON op list so a caller can batch a whole frame in
//! one FFI hop instead of one call per line.

use base64::Engine as _;
use resvg::tiny_skia::{
    self, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pattern, Pixmap, PixmapPaint,
    Rect, SpreadMode, Stroke, StrokeDash, Transform,
};

use crate::imgops::{self, Image};
use crate::text::{self, Anchor, Baseline, TextStyle};
use crate::util::{R, Rgba, color_from_json, err, jb, jf, jf_or, jpoints, js};

pub struct Canvas {
    pub pm: Pixmap,
    clip: Option<tiny_skia::Mask>,
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Rgba) -> R<Canvas> {
        let mut pm =
            Pixmap::new(w.max(1), h.max(1)).ok_or_else(|| format!("cannot allocate {w}x{h}"))?;
        if bg.3 > 0 {
            pm.fill(bg.to_color());
        }
        Ok(Canvas { pm, clip: None })
    }

    pub fn from_image(img: &Image) -> R<Canvas> {
        Ok(Canvas {
            pm: img.to_pixmap()?,
            clip: None,
        })
    }

    pub fn into_image(self) -> R<Image> {
        Image::from_pixmap(&self.pm)
    }

    pub fn width(&self) -> f32 {
        self.pm.width() as f32
    }

    pub fn height(&self) -> f32 {
        self.pm.height() as f32
    }

    pub fn clear(&mut self, c: Rgba) {
        self.pm.fill(c.to_color());
    }

    /// Restrict every following op to `rect` (None clears the restriction).
    pub fn set_clip(&mut self, rect: Option<Rect>) {
        self.clip = rect.and_then(|r| {
            let mut mask = tiny_skia::Mask::new(self.pm.width(), self.pm.height())?;
            let path = PathBuilder::from_rect(r);
            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
            Some(mask)
        });
    }

    fn paint(c: Rgba, anti_alias: bool) -> Paint<'static> {
        let mut p = Paint {
            anti_alias,
            ..Paint::default()
        };
        p.set_color(c.to_color());
        p
    }

    pub fn fill_path(&mut self, path: &Path, color: Rgba, even_odd: bool) {
        if color.3 == 0 {
            return;
        }
        let rule = if even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        self.pm.fill_path(
            path,
            &Self::paint(color, true),
            rule,
            Transform::identity(),
            self.clip.as_ref(),
        );
    }

    pub fn stroke_path(&mut self, path: &Path, color: Rgba, stroke: &Stroke) {
        if color.3 == 0 || stroke.width <= 0.0 {
            return;
        }
        self.pm.stroke_path(
            path,
            &Self::paint(color, true),
            stroke,
            Transform::identity(),
            self.clip.as_ref(),
        );
    }

    pub fn stroke(width: f32, dash: Option<&[f32]>, cap: LineCap, join: LineJoin) -> Stroke {
        Stroke {
            width: width.max(0.01),
            line_cap: cap,
            line_join: join,
            dash: dash.and_then(|d| StrokeDash::new(d.to_vec(), 0.0)),
            ..Stroke::default()
        }
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Rgba, stroke: &Stroke) {
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        if let Some(p) = pb.finish() {
            self.stroke_path(&p, color, stroke);
        }
    }

    pub fn polyline(&mut self, pts: &[(f32, f32)], color: Rgba, stroke: &Stroke, close: bool) {
        if let Some(p) = path_of(pts, close) {
            self.stroke_path(&p, color, stroke);
        }
    }

    pub fn fill_polygon(&mut self, pts: &[(f32, f32)], color: Rgba) {
        if let Some(p) = path_of(pts, true) {
            self.fill_path(&p, color, false);
        }
    }

    pub fn rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        fill: Option<Rgba>,
        stroke: Option<(Rgba, Stroke)>,
        radius: f32,
    ) {
        let Some(path) = rect_path(x, y, w, h, radius) else {
            return;
        };
        if let Some(c) = fill {
            self.fill_path(&path, c, false);
        }
        if let Some((c, s)) = stroke {
            self.stroke_path(&path, c, &s);
        }
    }

    pub fn ellipse(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        fill: Option<Rgba>,
        stroke: Option<(Rgba, Stroke)>,
    ) {
        let Some(r) = Rect::from_xywh(cx - rx, cy - ry, rx * 2.0, ry * 2.0) else {
            return;
        };
        let mut pb = PathBuilder::new();
        pb.push_oval(r);
        let Some(path) = pb.finish() else { return };
        if let Some(c) = fill {
            self.fill_path(&path, c, false);
        }
        if let Some((c, s)) = stroke {
            self.stroke_path(&path, c, &s);
        }
    }

    /// A pie/donut wedge, angles in degrees, 0° at 3 o'clock, clockwise.
    pub fn wedge(
        &mut self,
        cx: f32,
        cy: f32,
        r_outer: f32,
        r_inner: f32,
        a0: f32,
        a1: f32,
        fill: Rgba,
    ) {
        let Some(path) = wedge_path(cx, cy, r_outer, r_inner, a0, a1) else {
            return;
        };
        self.fill_path(&path, fill, false);
    }

    /// Draw `img` into the rect (x, y, w, h), scaled with bicubic filtering.
    pub fn image(&mut self, img: &Image, x: f32, y: f32, w: f32, h: f32, opacity: f32) -> R<()> {
        let src = img.to_pixmap()?;
        if (w - src.width() as f32).abs() < 0.01
            && (h - src.height() as f32).abs() < 0.01
            && opacity >= 1.0
        {
            self.pm.draw_pixmap(
                x.round() as i32,
                y.round() as i32,
                src.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                self.clip.as_ref(),
            );
            return Ok(());
        }
        let sx = w / src.width() as f32;
        let sy = h / src.height() as f32;
        let pattern = Pattern::new(
            src.as_ref(),
            SpreadMode::Pad,
            tiny_skia::FilterQuality::Bicubic,
            opacity.clamp(0.0, 1.0),
            Transform::from_row(sx, 0.0, 0.0, sy, x, y),
        );
        let Some(r) = Rect::from_xywh(x, y, w.max(0.01), h.max(0.01)) else {
            return Ok(());
        };
        let paint = Paint {
            shader: pattern,
            anti_alias: true,
            ..Paint::default()
        };
        self.pm
            .fill_rect(r, &paint, Transform::identity(), self.clip.as_ref());
        Ok(())
    }

    /// Draw a (possibly multi-line) text run. Returns its ink bounds.
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        s: &str,
        style: &TextStyle,
        anchor: Anchor,
        baseline: Baseline,
        rotate_deg: f32,
    ) -> R<(f32, f32)> {
        if s.is_empty() {
            return Ok((0.0, 0.0));
        }
        let l = text::layout(s, style, anchor)?;
        let dy = l.baseline_shift(baseline);
        let mut t = Transform::from_translate(x, y);
        if rotate_deg != 0.0 {
            t = t.pre_concat(Transform::from_rotate(rotate_deg));
        }
        t = t.pre_concat(Transform::from_translate(0.0, dy));
        text::render(&l, &mut self.pm, t);
        Ok((l.width, l.height))
    }
}

pub fn path_of(pts: &[(f32, f32)], close: bool) -> Option<Path> {
    if pts.len() < 2 {
        return None;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        pb.line_to(p.0, p.1);
    }
    if close {
        pb.close();
    }
    pb.finish()
}

pub fn rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    let r = Rect::from_xywh(x, y, w.max(0.001), h.max(0.001))?;
    if radius <= 0.0 {
        return Some(PathBuilder::from_rect(r));
    }
    let rad = radius.min(w.abs() / 2.0).min(h.abs() / 2.0);
    let (l, t, rt, b) = (r.left(), r.top(), r.right(), r.bottom());
    let k = rad * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(l + rad, t);
    pb.line_to(rt - rad, t);
    pb.cubic_to(rt - rad + k, t, rt, t + rad - k, rt, t + rad);
    pb.line_to(rt, b - rad);
    pb.cubic_to(rt, b - rad + k, rt - rad + k, b, rt - rad, b);
    pb.line_to(l + rad, b);
    pb.cubic_to(l + rad - k, b, l, b - rad + k, l, b - rad);
    pb.line_to(l, t + rad);
    pb.cubic_to(l, t + rad - k, l + rad - k, t, l + rad, t);
    pb.close();
    pb.finish()
}

pub fn wedge_path(cx: f32, cy: f32, r_outer: f32, r_inner: f32, a0: f32, a1: f32) -> Option<Path> {
    let steps = ((a1 - a0).abs() / 2.0).ceil().max(2.0) as usize;
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(steps * 2 + 2);
    let at = |r: f32, a: f32| {
        let rad = a.to_radians();
        (cx + r * rad.cos(), cy + r * rad.sin())
    };
    for i in 0..=steps {
        let a = a0 + (a1 - a0) * (i as f32 / steps as f32);
        pts.push(at(r_outer, a));
    }
    if r_inner > 0.0 {
        for i in (0..=steps).rev() {
            let a = a0 + (a1 - a0) * (i as f32 / steps as f32);
            pts.push(at(r_inner, a));
        }
    } else {
        pts.push((cx, cy));
    }
    path_of(&pts, true)
}

pub fn line_cap(s: &str) -> LineCap {
    match s {
        "round" => LineCap::Round,
        "square" => LineCap::Square,
        _ => LineCap::Butt,
    }
}

pub fn line_join(s: &str) -> LineJoin {
    match s {
        "round" => LineJoin::Round,
        "bevel" => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

// ---------------------------------------------------------------------------
// JSON op list
// ---------------------------------------------------------------------------

pub fn text_style(op: &serde_json::Value) -> TextStyle {
    TextStyle {
        size: jf_or(op, "size", 14.0) as f32,
        family: js(op, "family").unwrap_or(text::DEFAULT_FAMILY).to_string(),
        weight: jf_or(op, "weight", 400.0) as u16,
        italic: jb(op, "italic", false),
        letter_spacing: jf_or(op, "letter_spacing", 0.0) as f32,
        line_height: jf_or(op, "line_height", 1.25) as f32,
        fill: op
            .get("fill")
            .and_then(|v| color_from_json(v).ok())
            .unwrap_or(Rgba::BLACK),
        stroke: op
            .get("stroke")
            .and_then(|v| color_from_json(v).ok())
            .map(|c| (c, jf_or(op, "stroke_width", 2.0) as f32)),
    }
}

fn stroke_of(op: &serde_json::Value) -> Stroke {
    let dash: Option<Vec<f32>> = op.get("dash").and_then(|d| d.as_array()).map(|a| {
        a.iter()
            .filter_map(|v| v.as_f64())
            .map(|v| v as f32)
            .collect()
    });
    Canvas::stroke(
        jf_or(op, "width", 1.0) as f32,
        dash.as_deref(),
        line_cap(js(op, "cap").unwrap_or("butt")),
        line_join(js(op, "join").unwrap_or("miter")),
    )
}

fn image_of(op: &serde_json::Value) -> R<Image> {
    if let Some(b64) = js(op, "b64") {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("bad base64 image: {e}"))?;
        return imgops::decode(&raw);
    }
    if let Some(path) = js(op, "path") {
        let raw = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        return imgops::decode(&raw);
    }
    err("image op needs `b64` or `path`")
}

/// Execute a JSON op list against `img`, in order.
///
/// `{"ops": [{"op": "rect", …}, …]}` — or a bare array of ops. Every op takes
/// `fill` / `stroke` colours (`#rgb`, `rgba()`, name, `[r,g,b,a]`), `width`,
/// `dash`, `cap`, `join`; see the README for the full table.
pub fn draw_ops(img: &mut Image, spec: &serde_json::Value) -> R<()> {
    let ops = match spec {
        serde_json::Value::Array(a) => a.clone(),
        v => v
            .get("ops")
            .and_then(|o| o.as_array())
            .cloned()
            .ok_or_else(|| "draw spec needs an `ops` array".to_string())?,
    };
    let mut c = Canvas::from_image(img)?;
    for op in &ops {
        let name = js(op, "op").ok_or_else(|| format!("op without a name: {op}"))?;
        let fill = op.get("fill").and_then(|v| color_from_json(v).ok());
        let stroke_color = op.get("stroke").and_then(|v| color_from_json(v).ok());
        let stroke = stroke_color.map(|sc| (sc, stroke_of(op)));
        match name {
            "clear" => c.clear(fill.unwrap_or(Rgba::TRANSPARENT)),
            "clip" => {
                let r = Rect::from_xywh(
                    jf_or(op, "x", 0.0) as f32,
                    jf_or(op, "y", 0.0) as f32,
                    jf_or(op, "w", 0.0) as f32,
                    jf_or(op, "h", 0.0) as f32,
                );
                c.set_clip(r);
            }
            "reset_clip" => c.set_clip(None),
            "line" => {
                let (col, st) = stroke.unwrap_or((Rgba::BLACK, stroke_of(op)));
                c.line(
                    jf_or(op, "x1", 0.0) as f32,
                    jf_or(op, "y1", 0.0) as f32,
                    jf_or(op, "x2", 0.0) as f32,
                    jf_or(op, "y2", 0.0) as f32,
                    col,
                    &st,
                );
            }
            "polyline" | "polygon" => {
                let pts = jpoints(op, "points").unwrap_or_default();
                let close = name == "polygon" || jb(op, "close", false);
                if let Some(f) = fill {
                    c.fill_polygon(&pts, f);
                }
                if let Some((col, st)) = stroke {
                    c.polyline(&pts, col, &st, close);
                }
            }
            "rect" => c.rect(
                jf_or(op, "x", 0.0) as f32,
                jf_or(op, "y", 0.0) as f32,
                jf_or(op, "w", 0.0) as f32,
                jf_or(op, "h", 0.0) as f32,
                fill,
                stroke,
                jf_or(op, "radius", 0.0) as f32,
            ),
            "ellipse" | "circle" => {
                let r = jf_or(op, "r", 0.0) as f32;
                c.ellipse(
                    jf_or(op, "cx", 0.0) as f32,
                    jf_or(op, "cy", 0.0) as f32,
                    jf(op, "rx").map(|v| v as f32).unwrap_or(r),
                    jf(op, "ry").map(|v| v as f32).unwrap_or(r),
                    fill,
                    stroke,
                );
            }
            "arc" | "wedge" => c.wedge(
                jf_or(op, "cx", 0.0) as f32,
                jf_or(op, "cy", 0.0) as f32,
                jf_or(op, "r", 0.0) as f32,
                jf_or(op, "r_inner", 0.0) as f32,
                jf_or(op, "start", 0.0) as f32,
                jf_or(op, "end", 0.0) as f32,
                fill.unwrap_or(Rgba::BLACK),
            ),
            "path" => {
                let d = js(op, "d").ok_or_else(|| "path op needs `d`".to_string())?;
                let path = svg_path(d).ok_or_else(|| format!("unparsable path: {d}"))?;
                if let Some(f) = fill {
                    c.fill_path(&path, f, js(op, "fill_rule") == Some("evenodd"));
                }
                if let Some((col, st)) = stroke {
                    c.stroke_path(&path, col, &st);
                }
            }
            "text" => {
                let s = js(op, "text").unwrap_or("");
                c.text(
                    jf_or(op, "x", 0.0) as f32,
                    jf_or(op, "y", 0.0) as f32,
                    s,
                    &text_style(op),
                    Anchor::parse(js(op, "anchor").unwrap_or("start")),
                    Baseline::parse(js(op, "baseline").unwrap_or("alphabetic")),
                    jf_or(op, "rotate", 0.0) as f32,
                )?;
            }
            "image" => {
                let src = image_of(op)?;
                let w = jf(op, "w").map(|v| v as f32).unwrap_or(src.width() as f32);
                let h = jf(op, "h").map(|v| v as f32).unwrap_or(src.height() as f32);
                c.image(
                    &src,
                    jf_or(op, "x", 0.0) as f32,
                    jf_or(op, "y", 0.0) as f32,
                    w,
                    h,
                    jf_or(op, "opacity", 1.0) as f32,
                )?;
            }
            other => return err(format!("unknown draw op: {other}")),
        }
    }
    *img = c.into_image()?;
    Ok(())
}

/// Parse an SVG path `d` string via usvg's own parser, so `path` ops accept
/// exactly what an SVG file does.
pub fn svg_path(d: &str) -> Option<Path> {
    let doc = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path d="{}" fill="#000"/></svg>"##,
        d.replace('"', "'")
    );
    let tree = usvg::Tree::from_str(&doc, &usvg::Options::default()).ok()?;
    fn first(group: &usvg::Group) -> Option<Path> {
        for node in group.children() {
            match node {
                usvg::Node::Path(p) => return Some(p.data().clone()),
                usvg::Node::Group(g) => {
                    if let Some(p) = first(g) {
                        return Some(p);
                    }
                }
                _ => {}
            }
        }
        None
    }
    first(tree.root())
}
