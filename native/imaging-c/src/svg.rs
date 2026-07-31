//! SVG rasterisation through resvg — the replacement for jsvg + Graphics2D.

use resvg::tiny_skia;

use crate::imgops::Image;
use crate::text;
use crate::util::{R, Rgba, err, jf, js};

/// How the caller wants the document sized.
#[derive(Clone, Copy, Debug, Default)]
pub struct SvgSize {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub scale: Option<f32>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
}

/// Rasterise SVG (or gzipped SVGZ) bytes.
///
/// Options JSON: `width`, `height`, `scale`, `max_width`, `max_height`,
/// `background` (colour, default transparent), `dpi`, `font_family`,
/// `font_size`, `base_url` (directory used to resolve relative `href`s).
pub fn render(data: &[u8], opts: &serde_json::Value) -> R<Image> {
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
