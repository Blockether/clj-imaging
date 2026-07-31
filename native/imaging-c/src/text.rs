//! Text: shaping, measuring and rasterisation.
//!
//! There is no second text engine here. A run is emitted as a tiny SVG document
//! and handed to usvg, which shapes it with rustybuzz and converts it to paths
//! using the SAME font database that rasterises real SVG files. So text in a
//! chart, text drawn on an image and text inside an `<svg>` all shape, kern,
//! fall back and hint identically — and we carry no extra dependency for it.
//!
//! Four Noto faces are compiled into the library, so a fresh container with no
//! fonts installed still renders Latin/Greek/Cyrillic text. System fonts are
//! loaded on top (they win by name) unless IMAGING_NO_SYSTEM_FONTS is set.

use std::sync::{Arc, Mutex, OnceLock};

use resvg::tiny_skia;
use usvg::fontdb;

use crate::util::{R, Rgba, err};

const NOTO_SANS: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
const NOTO_SANS_BOLD: &[u8] = include_bytes!("../assets/NotoSans-Bold.ttf");
const NOTO_SANS_ITALIC: &[u8] = include_bytes!("../assets/NotoSans-Italic.ttf");
const NOTO_SANS_MONO: &[u8] = include_bytes!("../assets/NotoSansMono-Regular.ttf");

/// The default family for every drawing surface: "sans-serif" resolves to the
/// embedded Noto Sans when the host has no fonts at all.
pub const DEFAULT_FAMILY: &str = "Noto Sans";
pub const DEFAULT_MONO: &str = "Noto Sans Mono";

fn build_db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_font_data(NOTO_SANS.to_vec());
    db.load_font_data(NOTO_SANS_BOLD.to_vec());
    db.load_font_data(NOTO_SANS_ITALIC.to_vec());
    db.load_font_data(NOTO_SANS_MONO.to_vec());
    if std::env::var_os("IMAGING_NO_SYSTEM_FONTS").is_none() {
        db.load_system_fonts();
    }
    db.set_sans_serif_family(DEFAULT_FAMILY);
    db.set_serif_family(DEFAULT_FAMILY);
    db.set_monospace_family(DEFAULT_MONO);
    db.set_cursive_family(DEFAULT_FAMILY);
    db.set_fantasy_family(DEFAULT_FAMILY);
    db
}

fn slot() -> &'static Mutex<Arc<fontdb::Database>> {
    static DB: OnceLock<Mutex<Arc<fontdb::Database>>> = OnceLock::new();
    DB.get_or_init(|| Mutex::new(Arc::new(build_db())))
}

/// The shared font database — cheap to clone, shared by every render.
pub fn database() -> Arc<fontdb::Database> {
    slot()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// Add a font (TTF/OTF/TTC bytes) to the shared database. Returns the families
/// the file provided.
pub fn register_font(data: Vec<u8>) -> R<Vec<String>> {
    let mut guard = slot()
        .lock()
        .map_err(|_| "font database is poisoned".to_string())?;
    let db = Arc::make_mut(&mut guard);
    let before = db.len();
    db.load_font_data(data);
    if db.len() == before {
        return err("no font faces found in the given data");
    }
    let names: Vec<String> = db
        .faces()
        .skip(before)
        .flat_map(|f| f.families.iter().map(|(n, _)| n.clone()))
        .collect();
    Ok(names)
}

/// Every family name the database knows, sorted and de-duplicated.
pub fn families() -> Vec<String> {
    let db = database();
    let mut v: Vec<String> = db
        .faces()
        .flat_map(|f| f.families.iter().map(|(n, _)| n.clone()))
        .collect();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------------------
// style + layout
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TextStyle {
    pub size: f32,
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    pub letter_spacing: f32,
    /// Multiple of `size` between baselines of a multi-line run.
    pub line_height: f32,
    pub fill: Rgba,
    pub stroke: Option<(Rgba, f32)>,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            size: 14.0,
            family: DEFAULT_FAMILY.to_string(),
            weight: 400,
            italic: false,
            letter_spacing: 0.0,
            line_height: 1.25,
            fill: Rgba::BLACK,
            stroke: None,
        }
    }
}

/// Horizontal anchoring, SVG's own vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

impl Anchor {
    pub fn parse(s: &str) -> Anchor {
        match s {
            "middle" | "center" | "centre" => Anchor::Middle,
            "end" | "right" => Anchor::End,
            _ => Anchor::Start,
        }
    }
    fn svg(self) -> &'static str {
        match self {
            Anchor::Start => "start",
            Anchor::Middle => "middle",
            Anchor::End => "end",
        }
    }
}

/// Vertical placement of `y` relative to the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    /// `y` is the first line's alphabetic baseline (SVG/PIL default).
    Alphabetic,
    Top,
    Middle,
    Bottom,
}

impl Baseline {
    pub fn parse(s: &str) -> Baseline {
        match s {
            "top" | "hanging" => Baseline::Top,
            "middle" | "center" | "centre" | "central" => Baseline::Middle,
            "bottom" => Baseline::Bottom,
            _ => Baseline::Alphabetic,
        }
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// A shaped run, converted to paths and ready to rasterise.
pub struct Layout {
    tree: usvg::Tree,
    /// Ink bounds relative to the FIRST line's baseline at x=0.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub lines: usize,
    pub line_height: f32,
}

impl Layout {
    /// Offset added to `y` so the requested baseline mode lands where the caller
    /// means it to.
    pub fn baseline_shift(&self, b: Baseline) -> f32 {
        match b {
            Baseline::Alphabetic => 0.0,
            Baseline::Top => -self.y,
            Baseline::Middle => -(self.y + self.height / 2.0),
            Baseline::Bottom => -(self.y + self.height),
        }
    }
}

/// Half the layout canvas: the measuring viewport is centred on the run origin,
/// so usvg's root transform shifts every absolute coordinate by this much.
const ORIGIN: f32 = 50_000.0;

/// Shape `text` (newlines start a new line) and convert it to paths.
pub fn layout(text: &str, style: &TextStyle, anchor: Anchor) -> R<Layout> {
    let lines: Vec<&str> = text.split('\n').collect();
    let lh = style.size * style.line_height;
    let mut body = String::new();
    for (i, line) in lines.iter().enumerate() {
        body.push_str(&format!(
            r#"<text x="0" y="{y}" xml:space="preserve">{t}</text>"#,
            y = i as f32 * lh,
            t = escape(line)
        ));
    }
    let stroke = match style.stroke {
        Some((c, w)) => format!(
            r#" stroke="{}" stroke-width="{}" stroke-opacity="{}" paint-order="stroke""#,
            c.to_hex(),
            w,
            c.3 as f32 / 255.0
        ),
        None => String::new(),
    };
    // A generous canvas: usvg clips nothing at parse time, but a size of 0 is
    // rejected, and the viewport must not be smaller than the ink or the
    // absolute bounding box gets clamped.
    let doc = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000" viewBox="-50000 -50000 100000 100000"><g font-family="{fam}" font-size="{size}" font-weight="{weight}" font-style="{style_}" letter-spacing="{ls}" text-anchor="{anchor}" fill="{fill}" fill-opacity="{fo}"{stroke}>{body}</g></svg>"#,
        fam = escape(&style.family),
        size = style.size,
        weight = style.weight,
        style_ = if style.italic { "italic" } else { "normal" },
        ls = style.letter_spacing,
        anchor = anchor.svg(),
        fill = style.fill.to_hex(),
        fo = style.fill.3 as f32 / 255.0,
        stroke = stroke,
        body = body
    );
    let mut opt = usvg::Options {
        fontdb: database(),
        ..usvg::Options::default()
    };
    opt.font_family = style.family.clone();
    opt.font_size = style.size;
    let tree = usvg::Tree::from_str(&doc, &opt).map_err(|e| format!("text layout failed: {e}"))?;
    let bbox = tree.root().abs_bounding_box();
    Ok(Layout {
        tree,
        // usvg resolves the viewBox into the root transform, so every absolute
        // coordinate is shifted by ORIGIN; undo it so the box is relative to the
        // run's own origin (x = 0, first baseline).
        x: bbox.x() - ORIGIN,
        y: bbox.y() - ORIGIN,
        width: bbox.width(),
        height: bbox.height(),
        lines: lines.len(),
        line_height: lh,
    })
}

/// Rasterise a laid-out run into `pixmap` with `transform` already positioned at
/// the run's origin (x, alphabetic baseline of line 1).
pub fn render(layout: &Layout, pixmap: &mut tiny_skia::Pixmap, transform: tiny_skia::Transform) {
    // Cancel the viewBox shift baked into the tree's root transform, so the
    // caller's `transform` really is positioned at the run origin.
    let t = transform.pre_concat(tiny_skia::Transform::from_translate(-ORIGIN, -ORIGIN));
    resvg::render(&layout.tree, t, &mut pixmap.as_mut());
}

/// Convenience: measure only. Returns (width, height, x, y, lines).
pub fn measure(text: &str, style: &TextStyle) -> R<(f32, f32, f32, f32, usize)> {
    let l = layout(text, style, Anchor::Start)?;
    Ok((l.width, l.height, l.x, l.y, l.lines))
}
