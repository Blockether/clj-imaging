//! Plotting: a declarative chart spec in, a rendered image out.
//!
//! This is a first-class charting engine, not a matplotlib tribute act. One JSON
//! spec describes the figure; the renderer does the layout (margins are measured
//! from real text metrics, never guessed), picks nice ticks, and paints with the
//! same tiny-skia canvas everything else in this library uses.
//!
//! ```json
//! {"width": 900, "height": 520, "theme": "light", "title": "Latency",
//!  "panels": [{"x_label": "day", "y_label": "ms",
//!              "series": [{"type": "line", "name": "p50", "x": [1,2,3], "y": [8,7,9]}]}]}
//! ```
//!
//! Series types: line, step, area, scatter, bar, hist, box(→bar-like), pie,
//! donut, heatmap, hline, vline. Everything else (axes, grid, legend, titles,
//! subplot grid, log scales, categorical axes, stacking) is panel-level.

use resvg::tiny_skia::{LineCap, LineJoin, Rect, Stroke};

use crate::canvas::Canvas;
use crate::imgops::Image;
use crate::text::{Anchor, Baseline, DEFAULT_FAMILY, TextStyle};
use crate::util::{R, Rgba, err, jb, jf, jf_or, js, parse_color};

// ---------------------------------------------------------------------------
// theme
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Theme {
    pub bg: Rgba,
    pub panel: Rgba,
    pub fg: Rgba,
    pub muted: Rgba,
    pub grid: Rgba,
    pub axis: Rgba,
    pub palette: Vec<Rgba>,
}

fn hex(s: &str) -> Rgba {
    parse_color(s).unwrap_or(Rgba::BLACK)
}

fn palette() -> Vec<Rgba> {
    [
        "#4269d0", "#efb118", "#ff725c", "#6cc5b0", "#3ca951", "#ff8ab7", "#a463f2", "#97bbf5",
        "#9c6b4e", "#9498a0",
    ]
    .iter()
    .map(|s| hex(s))
    .collect()
}

pub fn theme(name: &str) -> Theme {
    match name {
        "dark" | "night" => Theme {
            bg: hex("#0f1216"),
            panel: hex("#0f1216"),
            fg: hex("#e7eaee"),
            muted: hex("#98a2ad"),
            grid: hex("#212832"),
            axis: hex("#3a434f"),
            palette: palette(),
        },
        _ => Theme {
            bg: hex("#ffffff"),
            panel: hex("#ffffff"),
            fg: hex("#1f2430"),
            muted: hex("#6b7280"),
            grid: hex("#e8eaee"),
            axis: hex("#c8ccd3"),
            palette: palette(),
        },
    }
}

// ---------------------------------------------------------------------------
// scales + ticks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Linear,
    Log,
    Category,
}

struct Axis {
    kind: Kind,
    lo: f64,
    hi: f64,
    ticks: Vec<(f64, String)>,
}

impl Axis {
    /// Data value -> [0,1] along the axis.
    fn t(&self, v: f64) -> f64 {
        match self.kind {
            Kind::Log => {
                let (l, h, x) = (
                    self.lo.max(1e-12).log10(),
                    self.hi.max(1e-12).log10(),
                    v.max(1e-12).log10(),
                );
                if (h - l).abs() < f64::EPSILON {
                    0.0
                } else {
                    (x - l) / (h - l)
                }
            }
            _ => {
                if (self.hi - self.lo).abs() < f64::EPSILON {
                    0.0
                } else {
                    (v - self.lo) / (self.hi - self.lo)
                }
            }
        }
    }
}

/// Ticks at 1/2/2.5/5 × 10ⁿ — the spacing humans actually read.
pub(crate) fn nice_step(range: f64, target: usize) -> f64 {
    if range <= 0.0 || target == 0 {
        return 1.0;
    }
    let raw = range / target as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let mult = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 2.5 {
        2.5
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    mult * mag
}

pub(crate) fn fmt_num(v: f64, step: f64) -> String {
    let a = v.abs();
    if a != 0.0 && (a >= 1e6 || a < 1e-4) {
        return format!("{v:.1e}");
    }
    let decimals = if step >= 1.0 {
        0
    } else {
        (-step.log10().floor()).clamp(0.0, 6.0) as usize
    };
    let s = format!("{v:.decimals$}");
    if s == "-0" { "0".to_string() } else { s }
}

fn linear_axis(lo: f64, hi: f64, target: usize, log: bool) -> Axis {
    let (mut lo, mut hi) = (lo, hi);
    if !lo.is_finite() || !hi.is_finite() {
        lo = 0.0;
        hi = 1.0;
    }
    if (hi - lo).abs() < f64::EPSILON {
        let pad = if lo.abs() > 0.0 { lo.abs() * 0.1 } else { 1.0 };
        lo -= pad;
        hi += pad;
    }
    if log {
        let l = lo.max(1e-12).log10().floor();
        let h = hi.max(1e-11).log10().ceil();
        let mut ticks = Vec::new();
        let mut e = l;
        while e <= h {
            let v = 10f64.powf(e);
            ticks.push((v, fmt_num(v, v)));
            e += 1.0;
        }
        return Axis {
            kind: Kind::Log,
            lo: 10f64.powf(l),
            hi: 10f64.powf(h),
            ticks,
        };
    }
    let step = nice_step(hi - lo, target);
    let start = (lo / step).floor() * step;
    let end = (hi / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut v = start;
    let mut guard = 0;
    while v <= end + step * 1e-6 && guard < 1000 {
        ticks.push((v, fmt_num(v, step)));
        v += step;
        guard += 1;
    }
    Axis {
        kind: Kind::Linear,
        lo: start,
        hi: end,
        ticks,
    }
}

fn category_axis(labels: &[String]) -> Axis {
    let ticks = labels
        .iter()
        .enumerate()
        .map(|(i, s)| (i as f64, s.clone()))
        .collect::<Vec<_>>();
    Axis {
        kind: Kind::Category,
        lo: -0.5,
        hi: labels.len() as f64 - 0.5,
        ticks,
    }
}

// ---------------------------------------------------------------------------
// series
// ---------------------------------------------------------------------------

struct Series {
    kind: String,
    name: Option<String>,
    x: Vec<f64>,
    y: Vec<f64>,
    labels: Vec<String>,
    z: Vec<Vec<f64>>,
    color: Option<Rgba>,
    colors: Vec<Rgba>,
    width: f32,
    dash: Option<Vec<f32>>,
    marker: Option<String>,
    marker_size: f32,
    fill_alpha: f32,
    bar_width: f64,
    stack: Option<String>,
    bins: usize,
    value: f64,
    colormap: String,
    label_points: bool,
}

fn nums(v: Option<&serde_json::Value>) -> Vec<f64> {
    match v.and_then(|v| v.as_array()) {
        Some(a) => a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect(),
        None => Vec::new(),
    }
}

fn strs(v: Option<&serde_json::Value>) -> Vec<String> {
    match v.and_then(|v| v.as_array()) {
        Some(a) => a
            .iter()
            .map(|x| match x {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        None => Vec::new(),
    }
}

fn parse_series(v: &serde_json::Value, idx: usize, th: &Theme) -> Series {
    let kind = js(v, "type").unwrap_or("line").to_ascii_lowercase();
    // `x` doubles as a category axis: strings become 0..n-1 with labels.
    let raw_x = v
        .get("x")
        .or_else(|| v.get("labels"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let (x, labels) = match &raw_x {
        serde_json::Value::Array(a) if a.iter().any(|e| e.is_string()) => {
            let l = strs(Some(&raw_x));
            ((0..l.len()).map(|i| i as f64).collect(), l)
        }
        serde_json::Value::Array(_) => (nums(Some(&raw_x)), strs(v.get("labels"))),
        _ => (Vec::new(), strs(v.get("labels"))),
    };
    let y = if v.get("y").is_some() {
        nums(v.get("y"))
    } else {
        nums(v.get("values"))
    };
    Series {
        color: v
            .get("color")
            .and_then(|c| crate::util::color_from_json(c).ok()),
        colors: v
            .get("colors")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| crate::util::color_from_json(c).ok())
                    .collect()
            })
            .unwrap_or_default(),
        name: js(v, "name").map(|s| s.to_string()),
        width: jf_or(v, "width", 2.0) as f32,
        dash: v.get("dash").and_then(|d| d.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64())
                .map(|x| x as f32)
                .collect()
        }),
        marker: js(v, "marker").map(|s| s.to_string()),
        marker_size: jf_or(v, "marker_size", if kind == "scatter" { 5.0 } else { 0.0 }) as f32,
        fill_alpha: jf_or(v, "fill_alpha", if kind == "area" { 0.18 } else { 0.0 }) as f32,
        bar_width: jf_or(v, "bar_width", 0.8),
        stack: js(v, "stack").map(|s| s.to_string()),
        bins: jf_or(v, "bins", 20.0).max(1.0) as usize,
        value: jf_or(v, "value", f64::NAN),
        colormap: js(v, "colormap").unwrap_or("viridis").to_string(),
        label_points: jb(v, "labels_on", false),
        z: v.get("z")
            .and_then(|z| z.as_array())
            .map(|rows| rows.iter().map(|r| nums(Some(r))).collect())
            .unwrap_or_default(),
        kind,
        x,
        y,
        labels,
    }
    .defaulted(idx, th)
}

impl Series {
    fn defaulted(mut self, idx: usize, th: &Theme) -> Series {
        if self.color.is_none() {
            self.color = Some(th.palette[idx % th.palette.len()]);
        }
        if self.kind == "hist" && self.y.is_empty() {
            self.y = std::mem::take(&mut self.x);
        }
        if self.x.is_empty() && !self.y.is_empty() && self.kind != "hist" {
            self.x = (0..self.y.len()).map(|i| i as f64).collect();
        }
        self
    }
    fn color(&self) -> Rgba {
        self.color.unwrap_or(Rgba::BLACK)
    }
    fn is_categorical(&self) -> bool {
        !self.labels.is_empty()
            && (self.kind == "bar"
                || self.kind == "barh"
                || self.kind == "pie"
                || self.kind == "donut")
    }
}

// ---------------------------------------------------------------------------
// colour maps (heatmaps)
// ---------------------------------------------------------------------------

pub(crate) fn colormap(name: &str, t: f32) -> Rgba {
    let stops: &[&str] = match name {
        "magma" => &[
            "#000004", "#3b0f70", "#8c2981", "#de4968", "#fe9f6d", "#fcfdbf",
        ],
        "plasma" => &[
            "#0d0887", "#6a00a8", "#b12a90", "#e16462", "#fca636", "#f0f921",
        ],
        "inferno" => &[
            "#000004", "#420a68", "#932667", "#dd513a", "#fca50a", "#fcffa4",
        ],
        "blues" => &["#f7fbff", "#c6dbef", "#6baed6", "#2171b5", "#08306b"],
        "reds" => &["#fff5f0", "#fcbba1", "#fb6a4a", "#cb181d", "#67000d"],
        "greys" | "grays" => &["#ffffff", "#000000"],
        "coolwarm" => &["#3b4cc0", "#8db0fe", "#f2f2f2", "#f4987a", "#b40426"],
        _ => &["#440154", "#3b528b", "#21918c", "#5ec962", "#fde725"], // viridis
    };
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = t.floor() as usize;
    let f = t - i as f32;
    if i + 1 >= stops.len() {
        hex(stops[stops.len() - 1])
    } else {
        hex(stops[i]).mix(hex(stops[i + 1]), f)
    }
}

// ---------------------------------------------------------------------------
// figure
// ---------------------------------------------------------------------------

/// Render a figure spec. Returns the rendered image (encode it as you like).
pub fn render(spec: &serde_json::Value) -> R<Image> {
    let scale = jf_or(spec, "scale", 1.0).clamp(0.5, 4.0) as f32;
    let w = jf_or(spec, "width", 800.0).clamp(32.0, 8000.0) as f32;
    let h = jf_or(spec, "height", 500.0).clamp(32.0, 8000.0) as f32;
    let th = {
        let mut t = theme(js(spec, "theme").unwrap_or("light"));
        if let Some(c) = spec
            .get("background")
            .and_then(|v| crate::util::color_from_json(v).ok())
        {
            t.bg = c;
            t.panel = c;
        }
        if let Some(p) = spec.get("palette").and_then(|v| v.as_array())
            && !p.is_empty()
        {
            t.palette = p
                .iter()
                .filter_map(|c| crate::util::color_from_json(c).ok())
                .collect();
        }
        t
    };
    let family = js(spec, "font_family")
        .unwrap_or(DEFAULT_FAMILY)
        .to_string();
    let base = jf_or(spec, "font_size", 13.0) as f32;

    let mut cv = Canvas::new(
        (w * scale).round() as u32,
        (h * scale).round() as u32,
        th.bg,
    )?;

    let panels: Vec<serde_json::Value> = match spec.get("panels").and_then(|p| p.as_array()) {
        Some(a) if !a.is_empty() => a.clone(),
        _ => vec![spec.clone()],
    };
    let n = panels.len();
    let cols = jf(spec, "cols")
        .map(|c| c as usize)
        .unwrap_or_else(|| match n {
            1 => 1,
            2 => 2,
            3 | 4 => 2,
            _ => 3,
        });
    let cols = cols.max(1);
    let rows = jf(spec, "rows")
        .map(|r| r as usize)
        .unwrap_or((n + cols - 1) / cols)
        .max(1);

    let f = Fig {
        th: th.clone(),
        family: family.clone(),
        base,
        scale,
    };

    // Figure furniture first — the panel band is what is left over.
    let pad = 16.0 * scale;
    let mut top = pad;
    if let Some(t) = js(spec, "title") {
        let st = f.style(base * 1.45, 700, th.fg);
        let (_, hh) = cv.text(
            pad,
            top + base * 1.45,
            t,
            &st,
            Anchor::Start,
            Baseline::Alphabetic,
            0.0,
        )?;
        top += hh.max(base * 1.45) + 6.0 * scale;
    }
    if let Some(t) = js(spec, "subtitle") {
        let st = f.style(base * 1.05, 400, th.muted);
        let (_, hh) = cv.text(
            pad,
            top + base * 1.05,
            t,
            &st,
            Anchor::Start,
            Baseline::Alphabetic,
            0.0,
        )?;
        top += hh.max(base) + 4.0 * scale;
    }
    top += 6.0 * scale;
    let mut bottom = (h * scale) - pad;
    if let Some(t) = js(spec, "caption") {
        let st = f.style(base * 0.85, 400, th.muted);
        cv.text(
            pad,
            bottom,
            t,
            &st,
            Anchor::Start,
            Baseline::Alphabetic,
            0.0,
        )?;
        bottom -= base * 1.4;
    }

    let band_w = (w * scale) - pad * 2.0;
    let band_h = (bottom - top).max(40.0);
    let cell_w = band_w / cols as f32;
    let cell_h = band_h / rows as f32;

    for (i, p) in panels.iter().enumerate() {
        let r = i / cols;
        let c = i % cols;
        let rect = Rect::from_xywh(
            pad + c as f32 * cell_w,
            top + r as f32 * cell_h,
            cell_w - 8.0 * scale,
            cell_h - 8.0 * scale,
        )
        .ok_or_else(|| "degenerate panel rectangle".to_string())?;
        f.panel(&mut cv, p, rect)?;
    }

    cv.into_image()
}

pub(crate) struct Fig {
    pub(crate) th: Theme,
    pub(crate) family: String,
    pub(crate) base: f32,
    pub(crate) scale: f32,
}

impl Fig {
    pub(crate) fn style(&self, size: f32, weight: u16, fill: Rgba) -> TextStyle {
        TextStyle {
            size,
            family: self.family.clone(),
            weight,
            fill,
            ..TextStyle::default()
        }
    }

    pub(crate) fn stroke(&self, width: f32, dash: Option<&[f32]>) -> Stroke {
        Canvas::stroke(width * self.scale, dash, LineCap::Round, LineJoin::Round)
    }

    fn panel(&self, cv: &mut Canvas, p: &serde_json::Value, outer: Rect) -> R<()> {
        // A 3-D panel owns its own projection, panes and painter's-algorithm
        // draw order, so it never touches the 2-D axis machinery below.
        if js(p, "projection") == Some("3d") {
            return crate::plot3d::panel(self, cv, p, outer);
        }
        let th = &self.th;
        let s = self.scale;
        let base = self.base;
        let series: Vec<Series> = p
            .get("series")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, v)| parse_series(v, i, th))
                    .collect()
            })
            .unwrap_or_default();
        if series.is_empty() {
            return err("panel has no `series`");
        }

        // Pie/donut own the whole panel: no axes, no grid.
        if series[0].kind == "pie" || series[0].kind == "donut" {
            return self.pie(cv, p, outer, &series[0]);
        }

        let horizontal = series.iter().any(|x| x.kind == "barh");
        let categories: Vec<String> = series
            .iter()
            .find(|x| x.is_categorical() || (!x.labels.is_empty() && x.labels.len() == x.y.len()))
            .map(|x| x.labels.clone())
            .unwrap_or_default();

        // --- data ranges -----------------------------------------------------
        let (mut xmin, mut xmax, mut ymin, mut ymax) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        let mut hists: Vec<(Vec<f64>, Vec<f64>)> = Vec::new(); // (edges, counts) per hist series
        for sr in &series {
            match sr.kind.as_str() {
                "hist" => {
                    let (edges, counts) = histogram(&sr.y, sr.bins);
                    if let (Some(&e0), Some(&e1)) = (edges.first(), edges.last()) {
                        xmin = xmin.min(e0);
                        xmax = xmax.max(e1);
                    }
                    ymin = ymin.min(0.0);
                    ymax = ymax.max(counts.iter().cloned().fold(0.0, f64::max));
                    hists.push((edges, counts));
                }
                "heatmap" => {
                    let rows = sr.z.len().max(1);
                    let cols = sr.z.first().map(|r| r.len()).unwrap_or(1);
                    xmin = xmin.min(-0.5);
                    xmax = xmax.max(cols as f64 - 0.5);
                    ymin = ymin.min(-0.5);
                    ymax = ymax.max(rows as f64 - 0.5);
                }
                "hline" => {
                    ymin = ymin.min(sr.value);
                    ymax = ymax.max(sr.value);
                }
                "vline" => {
                    xmin = xmin.min(sr.value);
                    xmax = xmax.max(sr.value);
                }
                _ => {
                    for &v in &sr.x {
                        if v.is_finite() {
                            xmin = xmin.min(v);
                            xmax = xmax.max(v);
                        }
                    }
                    for &v in &sr.y {
                        if v.is_finite() {
                            ymin = ymin.min(v);
                            ymax = ymax.max(v);
                        }
                    }
                    if sr.kind == "bar" || sr.kind == "barh" || sr.kind == "area" {
                        ymin = ymin.min(0.0);
                        ymax = ymax.max(0.0);
                    }
                }
            }
        }
        if xmin > xmax {
            xmin = 0.0;
            xmax = 1.0;
        }
        if ymin > ymax {
            ymin = 0.0;
            ymax = 1.0;
        }
        // Stacked bars need the stacked total, not the tallest single bar.
        for tot in stacked_totals(&series) {
            ymax = ymax.max(tot);
            ymin = ymin.min(tot.min(0.0));
        }
        // Breathing room on a value axis (never on a categorical one).
        let bars = series
            .iter()
            .any(|x| x.kind == "bar" || x.kind == "barh" || x.kind == "hist");
        if !bars {
            let dy = (ymax - ymin) * 0.05;
            ymin -= dy;
            ymax += dy;
        }

        let logx = js(p, "x_scale").unwrap_or("linear") == "log";
        let logy = js(p, "y_scale").unwrap_or("linear") == "log";
        let lim = |k: &str| {
            p.get(k)
                .and_then(|v| v.as_array())
                .map(|a| (a[0].as_f64(), a.get(1).and_then(|x| x.as_f64())))
        };
        if let Some((lo, hi)) = lim("x_lim") {
            xmin = lo.unwrap_or(xmin);
            xmax = hi.unwrap_or(xmax);
        }
        if let Some((lo, hi)) = lim("y_lim") {
            ymin = lo.unwrap_or(ymin);
            ymax = hi.unwrap_or(ymax);
        }

        let cat_axis = !categories.is_empty();
        let mut xa = if cat_axis && !horizontal {
            category_axis(&categories)
        } else {
            linear_axis(xmin, xmax, 6, logx)
        };
        let mut ya = if cat_axis && horizontal {
            category_axis(&categories)
        } else {
            linear_axis(ymin, ymax, 5, logy)
        };
        if let Some(t) = p.get("x_ticks").and_then(|v| v.as_array()) {
            xa.ticks = t
                .iter()
                .filter_map(|v| v.as_f64())
                .map(|v| (v, fmt_num(v, 1.0)))
                .collect();
        }
        if let Some(t) = p.get("y_ticks").and_then(|v| v.as_array()) {
            ya.ticks = t
                .iter()
                .filter_map(|v| v.as_f64())
                .map(|v| (v, fmt_num(v, 1.0)))
                .collect();
        }

        // --- margins measured from real text --------------------------------
        let tick_style = self.style(base * 0.85, 400, th.muted);
        let label_style = self.style(base * 0.95, 600, th.fg);
        let mut left = 8.0 * s;
        for (_, t) in &ya.ticks {
            let (tw, _) = crate::text::measure(t, &tick_style).map(|(w, h, _, _, _)| (w, h))?;
            left = left.max(tw + 14.0 * s);
        }
        if js(p, "y_label").is_some() {
            left += base * 1.5;
        }
        let mut bottom_m = base * 2.0;
        if js(p, "x_label").is_some() {
            bottom_m += base * 1.6;
        }
        let mut top_m = 8.0 * s;
        if js(p, "title").is_some() {
            top_m += base * 1.9;
        }
        let legend_pos = js(p, "legend").unwrap_or("auto");
        let named: Vec<&Series> = series.iter().filter(|x| x.name.is_some()).collect();
        let show_legend = legend_pos != "none" && legend_pos != "false" && !named.is_empty();
        let legend_top = show_legend && matches!(legend_pos, "top" | "auto");
        let legend_right = show_legend && legend_pos == "right";
        if legend_top {
            top_m += base * 1.8;
        }
        let mut right_m = 12.0 * s;
        if legend_right {
            let mut lw: f32 = 0.0;
            for sr in &named {
                let (tw, ..) = crate::text::measure(sr.name.as_deref().unwrap_or(""), &tick_style)?;
                lw = lw.max(tw);
            }
            right_m += lw + 28.0 * s;
        }

        let plot = Rect::from_xywh(
            outer.x() + left,
            outer.y() + top_m,
            (outer.width() - left - right_m).max(20.0),
            (outer.height() - top_m - bottom_m).max(20.0),
        )
        .ok_or_else(|| "panel too small".to_string())?;

        // --- panel title ------------------------------------------------------
        if let Some(t) = js(p, "title") {
            let st = self.style(base * 1.1, 700, th.fg);
            cv.text(
                outer.x(),
                outer.y() + base * 1.1,
                t,
                &st,
                Anchor::Start,
                Baseline::Alphabetic,
                0.0,
            )?;
        }

        // --- grid + axes ------------------------------------------------------
        let grid = js(p, "grid").unwrap_or("y");
        let grid_stroke = self.stroke(1.0, None);
        let px = |v: f64| plot.x() + (xa.t(v) as f32) * plot.width();
        let py = |v: f64| plot.y() + plot.height() - (ya.t(v) as f32) * plot.height();
        if grid == "y" || grid == "both" || grid == "true" {
            for (v, _) in &ya.ticks {
                let y = py(*v);
                cv.line(plot.x(), y, plot.right(), y, th.grid, &grid_stroke);
            }
        }
        if grid == "x" || grid == "both" {
            for (v, _) in &xa.ticks {
                let x = px(*v);
                cv.line(x, plot.y(), x, plot.bottom(), th.grid, &grid_stroke);
            }
        }
        let axis_stroke = self.stroke(1.0, None);
        cv.line(
            plot.x(),
            plot.bottom(),
            plot.right(),
            plot.bottom(),
            th.axis,
            &axis_stroke,
        );
        if !cat_axis {
            cv.line(
                plot.x(),
                plot.y(),
                plot.x(),
                plot.bottom(),
                th.axis,
                &axis_stroke,
            );
        }

        // --- tick labels ------------------------------------------------------
        let rotate = jf_or(p, "x_tick_rotate", 0.0) as f32;
        for (v, t) in &xa.ticks {
            if *v < xa.lo - 1e-9 || *v > xa.hi + 1e-9 {
                continue;
            }
            let x = px(*v);
            cv.line(
                x,
                plot.bottom(),
                x,
                plot.bottom() + 4.0 * s,
                th.axis,
                &axis_stroke,
            );
            let anchor = if rotate != 0.0 {
                Anchor::End
            } else {
                Anchor::Middle
            };
            cv.text(
                x,
                plot.bottom() + base * 1.15,
                t,
                &tick_style,
                anchor,
                Baseline::Alphabetic,
                rotate,
            )?;
        }
        for (v, t) in &ya.ticks {
            if *v < ya.lo - 1e-9 || *v > ya.hi + 1e-9 {
                continue;
            }
            let y = py(*v);
            cv.text(
                plot.x() - 8.0 * s,
                y,
                t,
                &tick_style,
                Anchor::End,
                Baseline::Middle,
                0.0,
            )?;
        }
        if let Some(t) = js(p, "x_label") {
            cv.text(
                plot.x() + plot.width() / 2.0,
                outer.bottom() - 2.0 * s,
                t,
                &label_style,
                Anchor::Middle,
                Baseline::Bottom,
                0.0,
            )?;
        }
        if let Some(t) = js(p, "y_label") {
            cv.text(
                outer.x() + base * 0.9,
                plot.y() + plot.height() / 2.0,
                t,
                &label_style,
                Anchor::Middle,
                Baseline::Top,
                -90.0,
            )?;
        }

        // --- series ------------------------------------------------------------
        cv.set_clip(Rect::from_xywh(
            plot.x() - 2.0,
            plot.y() - 2.0,
            plot.width() + 4.0,
            plot.height() + 4.0,
        ));
        let bar_series: Vec<usize> = series
            .iter()
            .enumerate()
            .filter(|(_, x)| x.kind == "bar" || x.kind == "barh")
            .map(|(i, _)| i)
            .collect();
        let groups = bar_series
            .iter()
            .filter(|i| series[**i].stack.is_none())
            .count()
            .max(1);
        let mut group_idx = 0usize;
        let mut stack_acc: std::collections::HashMap<String, Vec<f64>> =
            std::collections::HashMap::new();
        let mut hist_i = 0usize;

        for sr in &series {
            let color = sr.color();
            match sr.kind.as_str() {
                "line" | "step" | "area" => {
                    let mut pts: Vec<(f32, f32)> = Vec::new();
                    for (i, (&xv, &yv)) in sr.x.iter().zip(sr.y.iter()).enumerate() {
                        if !xv.is_finite() || !yv.is_finite() {
                            continue;
                        }
                        let (cx, cy) = (px(xv), py(yv));
                        if sr.kind == "step" && i > 0 {
                            let prev = *pts.last().unwrap();
                            pts.push((cx, prev.1));
                        }
                        pts.push((cx, cy));
                    }
                    if sr.kind == "area" || sr.fill_alpha > 0.0 {
                        let mut poly = pts.clone();
                        if let (Some(first), Some(last)) = (pts.first(), pts.last()) {
                            let base_y = py(0.0f64.clamp(ya.lo, ya.hi));
                            poly.push((last.0, base_y));
                            poly.push((first.0, base_y));
                            cv.fill_polygon(&poly, color.with_alpha(sr.fill_alpha.max(0.15)));
                        }
                    }
                    let st = self.stroke(sr.width, sr.dash.as_deref());
                    cv.polyline(&pts, color, &st, false);
                    if sr.marker_size > 0.0 {
                        for (cx, cy) in &pts {
                            self.marker(cv, *cx, *cy, sr, color);
                        }
                    }
                }
                "scatter" => {
                    for (i, (&xv, &yv)) in sr.x.iter().zip(sr.y.iter()).enumerate() {
                        if !xv.is_finite() || !yv.is_finite() {
                            continue;
                        }
                        let c = sr.colors.get(i).copied().unwrap_or(color);
                        self.marker(cv, px(xv), py(yv), sr, c);
                    }
                }
                "bar" | "barh" => {
                    let slot_w = if cat_axis {
                        plot.width() / (xa.hi - xa.lo) as f32
                    } else if sr.x.len() > 1 {
                        (px(sr.x[1]) - px(sr.x[0])).abs()
                    } else {
                        plot.width() * 0.5
                    };
                    let total = (slot_w * sr.bar_width as f32).max(1.0);
                    let bw = if sr.stack.is_some() {
                        total
                    } else {
                        total / groups as f32
                    };
                    for (i, (&xv, &yv)) in sr.x.iter().zip(sr.y.iter()).enumerate() {
                        if !yv.is_finite() {
                            continue;
                        }
                        let (y0, y1) = match &sr.stack {
                            Some(key) => {
                                let acc = stack_acc
                                    .entry(key.clone())
                                    .or_insert_with(|| vec![0.0; sr.y.len()]);
                                if acc.len() < sr.y.len() {
                                    acc.resize(sr.y.len(), 0.0);
                                }
                                let b = acc[i];
                                acc[i] += yv;
                                (b, b + yv)
                            }
                            None => (0.0, yv),
                        };
                        let cx = px(xv) - total / 2.0
                            + if sr.stack.is_some() {
                                0.0
                            } else {
                                group_idx as f32 * bw
                            };
                        let (top_y, bot_y) = (py(y1.max(y0)), py(y1.min(y0)));
                        cv.rect(
                            cx,
                            top_y,
                            bw - 1.5 * s,
                            (bot_y - top_y).max(1.0),
                            Some(color),
                            None,
                            (jf_or(p, "bar_radius", 3.0) as f32 * s).min(bw / 2.5),
                        );
                        if sr.label_points {
                            let st = self.style(base * 0.8, 600, th.muted);
                            cv.text(
                                cx + bw / 2.0,
                                top_y - 4.0 * s,
                                &fmt_num(yv, 0.01),
                                &st,
                                Anchor::Middle,
                                Baseline::Bottom,
                                0.0,
                            )?;
                        }
                    }
                    if sr.stack.is_none() {
                        group_idx += 1;
                    }
                }
                "hist" => {
                    let (edges, counts) = &hists[hist_i];
                    hist_i += 1;
                    for (i, c) in counts.iter().enumerate() {
                        let x0 = px(edges[i]);
                        let x1 = px(edges[i + 1]);
                        let y0 = py(*c);
                        let y1 = py(0.0);
                        cv.rect(
                            x0,
                            y0,
                            (x1 - x0 - 1.0 * s).max(1.0),
                            (y1 - y0).max(1.0),
                            Some(color.with_alpha(0.9)),
                            None,
                            2.0 * s,
                        );
                    }
                }
                "heatmap" => {
                    let rows = sr.z.len();
                    let cols = sr.z.first().map(|r| r.len()).unwrap_or(0);
                    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
                    for r in &sr.z {
                        for v in r {
                            if v.is_finite() {
                                lo = lo.min(*v);
                                hi = hi.max(*v);
                            }
                        }
                    }
                    let span = if (hi - lo).abs() < f64::EPSILON {
                        1.0
                    } else {
                        hi - lo
                    };
                    let cw = plot.width() / cols.max(1) as f32;
                    let ch = plot.height() / rows.max(1) as f32;
                    for (r, row) in sr.z.iter().enumerate() {
                        for (c, v) in row.iter().enumerate() {
                            let t = ((v - lo) / span) as f32;
                            cv.rect(
                                plot.x() + c as f32 * cw,
                                plot.y() + r as f32 * ch,
                                cw + 0.5,
                                ch + 0.5,
                                Some(colormap(&sr.colormap, t)),
                                None,
                                0.0,
                            );
                        }
                    }
                }
                "hline" => {
                    let st = self.stroke(
                        sr.width,
                        Some(sr.dash.clone().unwrap_or(vec![6.0, 4.0]).as_slice()),
                    );
                    let y = py(sr.value);
                    cv.line(plot.x(), y, plot.right(), y, color, &st);
                }
                "vline" => {
                    let st = self.stroke(
                        sr.width,
                        Some(sr.dash.clone().unwrap_or(vec![6.0, 4.0]).as_slice()),
                    );
                    let x = px(sr.value);
                    cv.line(x, plot.y(), x, plot.bottom(), color, &st);
                }
                other => return err(format!("unknown series type: {other}")),
            }
        }
        cv.set_clip(None);

        // --- legend -----------------------------------------------------------
        if legend_top {
            let mut x = plot.x();
            let y = outer.y() + top_m - base * 0.9;
            for sr in &named {
                let c = sr.color();
                cv.rect(
                    x,
                    y - base * 0.42,
                    base * 0.8,
                    base * 0.8,
                    Some(c),
                    None,
                    base * 0.2,
                );
                let name = sr.name.as_deref().unwrap_or("");
                let (tw, _) = cv.text(
                    x + base * 1.1,
                    y + base * 0.28,
                    name,
                    &self.style(base * 0.9, 500, th.fg),
                    Anchor::Start,
                    Baseline::Alphabetic,
                    0.0,
                )?;
                x += base * 1.1 + tw + base * 1.2;
            }
        } else if legend_right {
            let mut y = plot.y() + base;
            for sr in &named {
                let c = sr.color();
                cv.rect(
                    plot.right() + 12.0 * s,
                    y - base * 0.42,
                    base * 0.8,
                    base * 0.8,
                    Some(c),
                    None,
                    base * 0.2,
                );
                cv.text(
                    plot.right() + 12.0 * s + base * 1.1,
                    y + base * 0.28,
                    sr.name.as_deref().unwrap_or(""),
                    &self.style(base * 0.9, 500, th.fg),
                    Anchor::Start,
                    Baseline::Alphabetic,
                    0.0,
                )?;
                y += base * 1.6;
            }
        }
        Ok(())
    }

    fn marker(&self, cv: &mut Canvas, x: f32, y: f32, sr: &Series, color: Rgba) {
        let r = (if sr.marker_size > 0.0 {
            sr.marker_size
        } else {
            4.0
        }) * self.scale;
        match sr.marker.as_deref().unwrap_or("circle") {
            "square" => cv.rect(x - r, y - r, r * 2.0, r * 2.0, Some(color), None, 0.0),
            "diamond" => cv.fill_polygon(&[(x, y - r), (x + r, y), (x, y + r), (x - r, y)], color),
            "triangle" => cv.fill_polygon(&[(x, y - r), (x + r, y + r), (x - r, y + r)], color),
            "cross" => {
                let st = Canvas::stroke(1.6 * self.scale, None, LineCap::Round, LineJoin::Round);
                cv.line(x - r, y - r, x + r, y + r, color, &st);
                cv.line(x - r, y + r, x + r, y - r, color, &st);
            }
            "none" => {}
            _ => cv.ellipse(x, y, r, r, Some(color), None),
        }
    }

    fn pie(&self, cv: &mut Canvas, p: &serde_json::Value, outer: Rect, sr: &Series) -> R<()> {
        let th = &self.th;
        let base = self.base;
        let s = self.scale;
        let mut top = outer.y();
        if let Some(t) = js(p, "title") {
            cv.text(
                outer.x(),
                outer.y() + base * 1.1,
                t,
                &self.style(base * 1.1, 700, th.fg),
                Anchor::Start,
                Baseline::Alphabetic,
                0.0,
            )?;
            top += base * 2.0;
        }
        let values: Vec<f64> =
            sr.y.iter()
                .cloned()
                .filter(|v| v.is_finite() && *v > 0.0)
                .collect();
        let total: f64 = values.iter().sum();
        if total <= 0.0 {
            return err("pie needs positive values");
        }
        let cx = outer.x() + outer.width() / 2.0;
        let cy = top + (outer.bottom() - top) / 2.0;
        let r = ((outer.width().min(outer.bottom() - top)) / 2.0 - base * 2.2).max(10.0);
        let inner = if sr.kind == "donut" {
            r * jf_or(p, "hole", 0.55) as f32
        } else {
            0.0
        };
        let mut a = -90.0f32;
        for (i, v) in values.iter().enumerate() {
            let sweep = (*v / total) as f32 * 360.0;
            let c = sr
                .colors
                .get(i)
                .copied()
                .unwrap_or(th.palette[i % th.palette.len()]);
            cv.wedge(cx, cy, r, inner, a, a + sweep, c);
            let mid = (a + sweep / 2.0).to_radians();
            let lr = r + base * 0.9;
            let label = sr
                .labels
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("{:.0}%", *v / total * 100.0));
            let anchor = if mid.cos() >= 0.0 {
                Anchor::Start
            } else {
                Anchor::End
            };
            cv.text(
                cx + mid.cos() * lr,
                cy + mid.sin() * lr,
                &label,
                &self.style(base * 0.9, 500, th.fg),
                anchor,
                Baseline::Middle,
                0.0,
            )?;
            a += sweep;
        }
        if sr.kind == "donut"
            && let Some(center) = js(p, "center_label")
        {
            cv.text(
                cx,
                cy,
                center,
                &self.style(base * 1.4, 700, th.fg),
                Anchor::Middle,
                Baseline::Middle,
                0.0,
            )?;
        }
        let _ = s;
        Ok(())
    }
}

fn stacked_totals(series: &[Series]) -> Vec<f64> {
    let mut acc: std::collections::HashMap<&str, Vec<f64>> = std::collections::HashMap::new();
    for sr in series {
        if let Some(k) = sr.stack.as_deref() {
            let e = acc.entry(k).or_default();
            for (i, v) in sr.y.iter().enumerate() {
                if e.len() <= i {
                    e.resize(i + 1, 0.0);
                }
                if v.is_finite() {
                    e[i] += *v;
                }
            }
        }
    }
    acc.into_values().flatten().collect()
}

fn histogram(values: &[f64], bins: usize) -> (Vec<f64>, Vec<f64>) {
    let vs: Vec<f64> = values.iter().cloned().filter(|v| v.is_finite()).collect();
    if vs.is_empty() {
        return (vec![0.0, 1.0], vec![0.0]);
    }
    let lo = vs.iter().cloned().fold(f64::MAX, f64::min);
    let hi = vs.iter().cloned().fold(f64::MIN, f64::max);
    let (lo, hi) = if (hi - lo).abs() < f64::EPSILON {
        (lo - 0.5, hi + 0.5)
    } else {
        (lo, hi)
    };
    let step = (hi - lo) / bins as f64;
    let edges: Vec<f64> = (0..=bins).map(|i| lo + step * i as f64).collect();
    let mut counts = vec![0.0; bins];
    for v in vs {
        let mut i = ((v - lo) / step).floor() as isize;
        if i < 0 {
            i = 0;
        }
        if i as usize >= bins {
            i = bins as isize - 1;
        }
        counts[i as usize] += 1.0;
    }
    (edges, counts)
}
