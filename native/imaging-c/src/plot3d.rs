//! Three-dimensional panels: a painter's-algorithm renderer for `surface3d`,
//! `wire3d`, `line3d`, `scatter3d`, `bar3d` and `seg3d` series.
//!
//! A panel opts in with `"projection": "3d"`. The data is normalised into the
//! unit cube, rotated by `elev`/`azim` (matplotlib's own defaults, 30 and -60),
//! projected orthographically onto the camera's right/up basis and fitted to the
//! panel by the projected extent of the cube's eight corners. Every facet,
//! segment and marker carries the depth of its centroid along the view axis and
//! the whole batch is drawn far-to-near, so geometry occludes correctly without
//! a z-buffer. Surface facets are additionally shaded by the angle between their
//! normal and the light, which is what makes a flat colormap read as a solid
//! surface. Panes, gridlines and tick labels sit on the three BACK walls (chosen
//! from the sign of the view vector), like matplotlib.
//!
//! ```json
//! {"projection": "3d", "title": "peaks", "elev": 32, "azim": -58,
//!  "series": [{"type": "surface3d", "X": [[...]], "Y": [[...]], "Z": [[...]],
//!              "cmap": "viridis"}]}
//! ```

use resvg::tiny_skia::Rect;
use serde_json::Value;

use crate::canvas::Canvas;
use crate::plot::{Fig, colormap, fmt_num, nice_step};
use crate::text::{Anchor, Baseline};
use crate::util::{R, Rgba, color_from_json, jb, jf, jf_or, js};

// ---------------------------------------------------------------------------
// small vector helpers
// ---------------------------------------------------------------------------

type V3 = [f64; 3];

fn dot(v: V3, x: f64, y: f64, z: f64) -> f64 {
    v[0] * x + v[1] * y + v[2] * z
}

fn norm(v: V3) -> V3 {
    let m = dot(v, v[0], v[1], v[2]).sqrt().max(1e-9);
    [v[0] / m, v[1] / m, v[2] / m]
}

fn shade(c: Rgba, f: f32) -> Rgba {
    let s = |v: u8| ((v as f32 * f).clamp(0.0, 255.0)) as u8;
    Rgba(s(c.0), s(c.1), s(c.2), c.3)
}

fn finite(v: f64) -> bool {
    v.is_finite()
}

// ---------------------------------------------------------------------------
// spec readers
// ---------------------------------------------------------------------------

fn nums(v: Option<&Value>) -> Vec<f64> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect())
        .unwrap_or_default()
}

/// A 2-D field (`X`/`Y`/`Z`, lowercase accepted) as rows of doubles.
fn grid(s: &Value, upper: &str, lower: &str) -> Vec<Vec<f64>> {
    let v = s
        .get(upper)
        .filter(|v| v.is_array())
        .or_else(|| s.get(lower).filter(|v| v.is_array()));
    let Some(a) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    if !a.first().map(Value::is_array).unwrap_or(false) {
        return Vec::new();
    }
    a.iter().map(|r| nums(Some(r))).collect()
}

fn at(g: &[Vec<f64>], i: usize, j: usize) -> f64 {
    g.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0)
}

fn kind_of(s: &Value) -> &str {
    js(s, "kind").or_else(|| js(s, "type")).unwrap_or("line3d")
}

fn series_color(s: &Value, idx: usize, f: &Fig) -> Rgba {
    s.get("color")
        .and_then(|v| color_from_json(v).ok())
        .unwrap_or_else(|| {
            let p = &f.th.palette;
            p[idx % p.len().max(1)]
        })
}

/// Autoscaled `[lo, hi]` for one axis: an explicit `lim` end wins, a degenerate
/// range is padded so the projection never divides by zero.
fn axis_range(vs: &[f64], lim: Option<&Value>) -> (f64, f64) {
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    for &v in vs.iter().filter(|v| finite(**v)) {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if lo > hi {
        lo = 0.0;
        hi = 1.0;
    }
    if let Some(a) = lim.and_then(|v| v.as_array()) {
        if let Some(v) = a.first().and_then(Value::as_f64) {
            lo = v;
        }
        if let Some(v) = a.get(1).and_then(Value::as_f64) {
            hi = v;
        }
    }
    if (hi - lo).abs() < 1e-12 {
        (lo - 0.5, hi + 0.5)
    } else {
        (lo, hi)
    }
}

/// Ticks on a "nice" step, matching the 2-D axis vocabulary.
fn ticks(lo: f64, hi: f64, target: usize) -> (Vec<f64>, f64) {
    let step = nice_step(hi - lo, target);
    let mut out = Vec::new();
    let mut t = (lo / step).ceil() * step;
    let mut guard = 0;
    while t <= hi + step * 1e-6 && guard < 200 {
        out.push(t);
        t += step;
        guard += 1;
    }
    (out, step)
}

/// Every `[x y z]` a series occupies — autoscaling only.
fn series_points(s: &Value) -> Vec<[f64; 3]> {
    let mut out = Vec::new();
    match kind_of(s) {
        "surface3d" | "wire3d" => {
            let (xg, yg, zg) = (grid(s, "X", "x"), grid(s, "Y", "y"), grid(s, "Z", "z"));
            for i in 0..zg.len() {
                for j in 0..zg[i].len() {
                    out.push([at(&xg, i, j), at(&yg, i, j), at(&zg, i, j)]);
                }
            }
        }
        "bar3d" => {
            let (xs, ys, zs) = (nums(s.get("x")), nums(s.get("y")), nums(s.get("z")));
            let (dx, dy, dz) = (nums(s.get("dx")), nums(s.get("dy")), nums(s.get("dz")));
            let g = |v: &[f64], i: usize, d: f64| v.get(i).copied().unwrap_or(d);
            for i in 0..xs.len() {
                let (x, y, z) = (g(&xs, i, 0.0), g(&ys, i, 0.0), g(&zs, i, 0.0));
                out.push([x, y, z]);
                out.push([x + g(&dx, i, 1.0), y + g(&dy, i, 1.0), z + g(&dz, i, 1.0)]);
            }
        }
        "seg3d" => {
            for seg in s.get("segs").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
                let v = nums(Some(seg));
                if v.len() >= 6 {
                    out.push([v[0], v[1], v[2]]);
                    out.push([v[3], v[4], v[5]]);
                }
            }
        }
        _ => {
            let (xs, ys, zs) = (nums(s.get("x")), nums(s.get("y")), nums(s.get("z")));
            let n = xs.len().min(ys.len()).min(zs.len());
            for i in 0..n {
                out.push([xs[i], ys[i], zs[i]]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// depth-sorted primitives
// ---------------------------------------------------------------------------

enum Prim {
    Poly {
        pts: Vec<(f32, f32)>,
        fill: Option<Rgba>,
        stroke: Option<(Rgba, f32)>,
    },
    Path {
        pts: Vec<(f32, f32)>,
        color: Rgba,
        width: f32,
    },
    Dot {
        c: (f32, f32),
        r: f32,
        fill: Rgba,
        stroke: Option<(Rgba, f32)>,
    },
}

impl Prim {
    fn draw(&self, cv: &mut Canvas, f: &Fig) {
        match self {
            Prim::Poly { pts, fill, stroke } => {
                if let Some(c) = fill {
                    cv.fill_polygon(pts, *c);
                }
                if let Some((c, w)) = stroke {
                    cv.polyline(pts, *c, &f.stroke(*w, None), true);
                }
            }
            Prim::Path { pts, color, width } => {
                cv.polyline(pts, *color, &f.stroke(*width, None), false);
            }
            Prim::Dot { c, r, fill, stroke } => {
                cv.ellipse(
                    c.0,
                    c.1,
                    *r,
                    *r,
                    Some(*fill),
                    stroke.map(|(sc, w)| (sc, f.stroke(w, None))),
                );
            }
        }
    }
}

/// The camera + cube fit for one panel: everything the projection needs.
struct Cam {
    r: V3,
    u: V3,
    v: V3,
    sc: f64,
    ox: f64,
    oy: f64,
}

impl Cam {
    /// Unit-cube point -> `(px, py, depth)`; larger depth is nearer the viewer.
    fn sp(&self, x: f64, y: f64, z: f64) -> (f32, f32, f64) {
        (
            (self.ox + self.sc * dot(self.r, x, y, z)) as f32,
            (self.oy - self.sc * dot(self.u, x, y, z)) as f32,
            dot(self.v, x, y, z),
        )
    }

    fn sp2(&self, x: f64, y: f64, z: f64) -> (f32, f32) {
        let p = self.sp(x, y, z);
        (p.0, p.1)
    }
}

// ---------------------------------------------------------------------------
// the panel
// ---------------------------------------------------------------------------

pub(crate) fn panel(f: &Fig, cv: &mut Canvas, p: &Value, outer: Rect) -> R<()> {
    let th = &f.th;
    let s = f.scale;
    let base = f.base;
    let empty: Vec<Value> = Vec::new();
    let series: &Vec<Value> = p.get("series").and_then(|v| v.as_array()).unwrap_or(&empty);

    let title = js(p, "title").filter(|t| !t.is_empty());
    let grid_on = jb(p, "grid", true);
    let axis_off = jb(p, "axis_off", false);

    // --- camera ----------------------------------------------------------
    let ar = jf_or(p, "azim", -60.0).to_radians();
    let er = jf_or(p, "elev", 30.0).to_radians();
    let (ca, sa, ce, se) = (ar.cos(), ar.sin(), er.cos(), er.sin());
    let cam_r: V3 = [-sa, ca, 0.0];
    let cam_u: V3 = [-ca * se, -sa * se, ce];
    let cam_v: V3 = [ca * ce, sa * ce, se];
    let light = norm([
        cam_v[0] + 0.45 * cam_u[0],
        cam_v[1] + 0.45 * cam_u[1],
        cam_v[2] + 0.45 * cam_u[2],
    ]);

    // --- data cube --------------------------------------------------------
    let pts: Vec<[f64; 3]> = series.iter().flat_map(series_points).collect();
    let col = |k: usize| pts.iter().map(|q| q[k]).collect::<Vec<f64>>();
    let (xmin, xmax) = axis_range(&col(0), p.get("xlim"));
    let (ymin, ymax) = axis_range(&col(1), p.get("ylim"));
    let (zmin, zmax) = axis_range(&col(2), p.get("zlim"));
    let (xspan, yspan, zspan) = (xmax - xmin, ymax - ymin, zmax - zmin);
    let nx = |v: f64| (v - xmin) / xspan - 0.5;
    let ny = |v: f64| (v - ymin) / yspan - 0.5;
    let nz = |v: f64| (v - zmin) / zspan - 0.5;

    // --- fit the projected cube into the panel ----------------------------
    let pad = 44.0 * s as f64;
    let top = if title.is_some() { 34.0 } else { 18.0 } * s as f64;
    let bot = 30.0 * s as f64;
    let mut us: Vec<f64> = Vec::with_capacity(8);
    let mut vs: Vec<f64> = Vec::with_capacity(8);
    for &x in &[-0.5f64, 0.5] {
        for &y in &[-0.5f64, 0.5] {
            for &z in &[-0.5f64, 0.5] {
                us.push(dot(cam_r, x, y, z));
                vs.push(dot(cam_u, x, y, z));
            }
        }
    }
    let mn = |v: &[f64]| v.iter().cloned().fold(f64::MAX, f64::min);
    let mx = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max);
    let (umin, umax, vmin, vmax) = (mn(&us), mx(&us), mn(&vs), mx(&vs));
    let avail_w = (outer.width() as f64 - 2.0 * pad).max(40.0);
    let avail_h = (outer.height() as f64 - top - bot).max(40.0);
    let sc = (avail_w / (umax - umin).max(1e-9)).min(avail_h / (vmax - vmin).max(1e-9));
    let cam = Cam {
        r: cam_r,
        u: cam_u,
        v: cam_v,
        sc,
        ox: outer.x() as f64 + pad + avail_w / 2.0 - sc * (umin + umax) / 2.0,
        oy: outer.y() as f64 + top + avail_h / 2.0 + sc * (vmin + vmax) / 2.0,
    };
    // data point -> screen
    let dp = |x: f64, y: f64, z: f64| cam.sp(nx(x), ny(y), nz(z));

    // --- back walls: the side of each pair the camera is NOT on ------------
    let zfloor = if cam_v[2] > 0.0 { -0.5 } else { 0.5 };
    let xback = if cam_v[0] > 0.0 { -0.5 } else { 0.5 };
    let yback = if cam_v[1] > 0.0 { -0.5 } else { 0.5 };
    let (xfront, yfront) = (-xback, -yback);

    let (xt, xstep) = match nums(p.get("xticks")) {
        v if !v.is_empty() => (v, nice_step(xspan, 5)),
        _ => ticks(xmin, xmax, 5),
    };
    let (yt, ystep) = match nums(p.get("yticks")) {
        v if !v.is_empty() => (v, nice_step(yspan, 5)),
        _ => ticks(ymin, ymax, 5),
    };
    let (zt, zstep) = match nums(p.get("zticks")) {
        v if !v.is_empty() => (v, nice_step(zspan, 5)),
        _ => ticks(zmin, zmax, 5),
    };

    // Themed pane furniture: a tinted wall, background-coloured gridlines and a
    // muted frame — matplotlib's look, but it follows the figure theme.
    let pane = th.panel.mix(th.fg, 0.07);
    let pane_grid = th.bg;
    let pane_edge = th.axis.mix(th.bg, 0.35);

    if !axis_off {
        for quad in [
            [
                (-0.5, -0.5, zfloor),
                (0.5, -0.5, zfloor),
                (0.5, 0.5, zfloor),
                (-0.5, 0.5, zfloor),
            ],
            [
                (xback, -0.5, -0.5),
                (xback, 0.5, -0.5),
                (xback, 0.5, 0.5),
                (xback, -0.5, 0.5),
            ],
            [
                (-0.5, yback, -0.5),
                (0.5, yback, -0.5),
                (0.5, yback, 0.5),
                (-0.5, yback, 0.5),
            ],
        ] {
            let ps: Vec<(f32, f32)> = quad.iter().map(|q| cam.sp2(q.0, q.1, q.2)).collect();
            cv.fill_polygon(&ps, pane);
        }
        if grid_on {
            let st = f.stroke(1.2, None);
            let mut wall = |a: (f64, f64, f64), b: (f64, f64, f64)| {
                let (x1, y1) = cam.sp2(a.0, a.1, a.2);
                let (x2, y2) = cam.sp2(b.0, b.1, b.2);
                cv.line(x1, y1, x2, y2, pane_grid, &st);
            };
            for &t in &xt {
                let v = nx(t);
                wall((v, -0.5, zfloor), (v, 0.5, zfloor));
                wall((v, yback, -0.5), (v, yback, 0.5));
            }
            for &t in &yt {
                let v = ny(t);
                wall((-0.5, v, zfloor), (0.5, v, zfloor));
                wall((xback, v, -0.5), (xback, v, 0.5));
            }
            for &t in &zt {
                let v = nz(t);
                wall((xback, -0.5, v), (xback, 0.5, v));
                wall((-0.5, yback, v), (0.5, yback, v));
            }
        }
        let st = f.stroke(1.0, None);
        let mut edge = |a: (f64, f64, f64), b: (f64, f64, f64)| {
            let (x1, y1) = cam.sp2(a.0, a.1, a.2);
            let (x2, y2) = cam.sp2(b.0, b.1, b.2);
            cv.line(x1, y1, x2, y2, pane_edge, &st);
        };
        edge((xback, yback, -0.5), (xback, yback, 0.5));
        edge((-0.5, -0.5, zfloor), (0.5, -0.5, zfloor));
        edge((0.5, -0.5, zfloor), (0.5, 0.5, zfloor));
        edge((0.5, 0.5, zfloor), (-0.5, 0.5, zfloor));
        edge((-0.5, 0.5, zfloor), (-0.5, -0.5, zfloor));
    }

    // --- series geometry, depth-tagged ------------------------------------
    let mut prims: Vec<(f64, Prim)> = Vec::new();
    for (idx, sr) in series.iter().enumerate() {
        let color = series_color(sr, idx, f);
        let cmap = js(sr, "cmap").map(str::to_ascii_lowercase);
        match kind_of(sr) {
            "surface3d" => {
                let (xg, yg, zg) = (grid(sr, "X", "x"), grid(sr, "Y", "y"), grid(sr, "Z", "z"));
                let nr = zg.len();
                let nc = zg.iter().map(Vec::len).max().unwrap_or(0);
                let cmap =
                    cmap.or_else(|| (sr.get("color").is_none()).then(|| "viridis".to_string()));
                let edges = jb(sr, "edges", true);
                let q =
                    |i: usize, j: usize| [nx(at(&xg, i, j)), ny(at(&yg, i, j)), nz(at(&zg, i, j))];
                for i in 0..nr.saturating_sub(1) {
                    for j in 0..nc.saturating_sub(1) {
                        let quad = [q(i, j), q(i + 1, j), q(i + 1, j + 1), q(i, j + 1)];
                        let scr: Vec<(f32, f32, f64)> =
                            quad.iter().map(|c| cam.sp(c[0], c[1], c[2])).collect();
                        let depth = scr.iter().map(|c| c.2).sum::<f64>() / 4.0;
                        let (a, b) = (
                            [
                                quad[1][0] - quad[0][0],
                                quad[1][1] - quad[0][1],
                                quad[1][2] - quad[0][2],
                            ],
                            [
                                quad[3][0] - quad[0][0],
                                quad[3][1] - quad[0][1],
                                quad[3][2] - quad[0][2],
                            ],
                        );
                        let n = [
                            a[1] * b[2] - a[2] * b[1],
                            a[2] * b[0] - a[0] * b[2],
                            a[0] * b[1] - a[1] * b[0],
                        ];
                        let nlen = dot(n, n[0], n[1], n[2]).sqrt();
                        let lam = if nlen < 1e-12 {
                            1.0
                        } else {
                            (dot(light, n[0], n[1], n[2]) / nlen).abs()
                        };
                        let lit = 0.62 + 0.42 * lam as f32;
                        let zavg = (at(&zg, i, j)
                            + at(&zg, i + 1, j)
                            + at(&zg, i + 1, j + 1)
                            + at(&zg, i, j + 1))
                            / 4.0;
                        let bcol = match &cmap {
                            Some(name) => colormap(name, ((zavg - zmin) / zspan) as f32),
                            None => color,
                        };
                        let pts2: Vec<(f32, f32)> = scr.iter().map(|c| (c.0, c.1)).collect();
                        prims.push((
                            depth,
                            Prim::Poly {
                                pts: pts2,
                                fill: Some(shade(bcol, lit)),
                                stroke: edges.then(|| (shade(bcol, lit * 0.82), 0.8)),
                            },
                        ));
                    }
                }
            }
            "wire3d" => {
                let (xg, yg, zg) = (grid(sr, "X", "x"), grid(sr, "Y", "y"), grid(sr, "Z", "z"));
                let nr = zg.len();
                let nc = zg.iter().map(Vec::len).max().unwrap_or(0);
                let q = |i: usize, j: usize| {
                    cam.sp(nx(at(&xg, i, j)), ny(at(&yg, i, j)), nz(at(&zg, i, j)))
                };
                let mut strand = |ps: Vec<(f32, f32, f64)>| {
                    if ps.len() > 1 {
                        let depth = ps.iter().map(|c| c.2).sum::<f64>() / ps.len() as f64;
                        prims.push((
                            depth,
                            Prim::Path {
                                pts: ps.iter().map(|c| (c.0, c.1)).collect(),
                                color,
                                width: 1.1,
                            },
                        ));
                    }
                };
                for i in 0..nr {
                    strand((0..nc).map(|j| q(i, j)).collect());
                }
                for j in 0..nc {
                    strand((0..nr).map(|i| q(i, j)).collect());
                }
            }
            "scatter3d" => {
                let (xs, ys, zs) = (nums(sr.get("x")), nums(sr.get("y")), nums(sr.get("z")));
                let cvals = nums(sr.get("c"));
                let cmin = cvals.iter().cloned().fold(f64::MAX, f64::min);
                let cmax = cvals.iter().cloned().fold(f64::MIN, f64::max);
                let cspan = if (cmax - cmin).abs() < 1e-12 {
                    1.0
                } else {
                    cmax - cmin
                };
                let sizes = nums(sr.get("sizes"));
                let dflt = jf_or(sr, "size", 20.0);
                let n = xs.len().min(ys.len()).min(zs.len());
                for i in 0..n {
                    let (px, py, d) = dp(xs[i], ys[i], zs[i]);
                    let c = match (&cmap, cvals.get(i)) {
                        (Some(name), Some(v)) => colormap(name, ((v - cmin) / cspan) as f32),
                        _ => color,
                    };
                    let sz = sizes.get(i).copied().unwrap_or(dflt);
                    let r = (sz.max(1.0) / std::f64::consts::PI).sqrt().max(1.5) as f32 * s;
                    prims.push((
                        d,
                        Prim::Dot {
                            c: (px, py),
                            r,
                            fill: c,
                            stroke: Some((shade(c, 0.55), 0.8)),
                        },
                    ));
                }
            }
            "bar3d" => {
                let (xs, ys, zs) = (nums(sr.get("x")), nums(sr.get("y")), nums(sr.get("z")));
                let (dxs, dys, dzs) = (nums(sr.get("dx")), nums(sr.get("dy")), nums(sr.get("dz")));
                let colors = sr.get("colors").and_then(|v| v.as_array());
                let g = |v: &[f64], i: usize, d: f64| v.get(i).copied().unwrap_or(d);
                for i in 0..xs.len() {
                    let (x0, y0, z0) = (g(&xs, i, 0.0), g(&ys, i, 0.0), g(&zs, i, 0.0));
                    let (x1, y1, z1) = (
                        x0 + g(&dxs, i, 1.0),
                        y0 + g(&dys, i, 1.0),
                        z0 + g(&dzs, i, 1.0),
                    );
                    let bcol = colors
                        .and_then(|a| a.get(i))
                        .and_then(|v| color_from_json(v).ok())
                        .unwrap_or(color);
                    let corner = |k: usize| {
                        dp(
                            if k & 1 == 0 { x0 } else { x1 },
                            if k & 2 == 0 { y0 } else { y1 },
                            if k & 4 == 0 { z0 } else { z1 },
                        )
                    };
                    // box faces as corner-index quads (bit 1 = x, 2 = y, 4 = z)
                    let faces = [
                        [0, 1, 3, 2],
                        [4, 5, 7, 6],
                        [0, 1, 5, 4],
                        [2, 3, 7, 6],
                        [0, 2, 6, 4],
                        [1, 3, 7, 5],
                    ];
                    let shades = [0.72f32, 1.0, 0.86, 0.8, 0.9, 0.78];
                    for (fi, face) in faces.iter().enumerate() {
                        let ps: Vec<(f32, f32, f64)> = face.iter().map(|&k| corner(k)).collect();
                        let depth = ps.iter().map(|c| c.2).sum::<f64>() / 4.0;
                        let fcol = shade(bcol, shades[fi]);
                        prims.push((
                            depth,
                            Prim::Poly {
                                pts: ps.iter().map(|c| (c.0, c.1)).collect(),
                                fill: Some(fcol),
                                stroke: Some((shade(fcol, 0.7), 0.8)),
                            },
                        ));
                    }
                }
            }
            // `contour`/`contour3D`: marching-squares segments, each already
            // carrying its own level (or the `offset=` plane) as z.
            "seg3d" => {
                for seg in sr.get("segs").and_then(|v| v.as_array()).unwrap_or(&empty) {
                    let v = nums(Some(seg));
                    if v.len() >= 6 {
                        let a = dp(v[0], v[1], v[2]);
                        let b = dp(v[3], v[4], v[5]);
                        prims.push((
                            (a.2 + b.2) / 2.0,
                            Prim::Path {
                                pts: vec![(a.0, a.1), (b.0, b.1)],
                                color,
                                width: 1.2,
                            },
                        ));
                    }
                }
            }
            // default: a 3-D polyline (`plot3D`), optionally with markers
            _ => {
                let (xs, ys, zs) = (nums(sr.get("x")), nums(sr.get("y")), nums(sr.get("z")));
                let n = xs.len().min(ys.len()).min(zs.len());
                let ps: Vec<(f32, f32, f64)> = (0..n).map(|i| dp(xs[i], ys[i], zs[i])).collect();
                if ps.len() > 1 {
                    let depth = ps.iter().map(|c| c.2).sum::<f64>() / ps.len() as f64;
                    prims.push((
                        depth,
                        Prim::Path {
                            pts: ps.iter().map(|c| (c.0, c.1)).collect(),
                            color,
                            width: jf_or(sr, "width", 2.0) as f32,
                        },
                    ));
                }
                if js(sr, "marker").map(|m| !m.is_empty()).unwrap_or(false) {
                    for q in &ps {
                        prims.push((
                            q.2,
                            Prim::Dot {
                                c: (q.0, q.1),
                                r: 3.0 * s,
                                fill: color,
                                stroke: None,
                            },
                        ));
                    }
                }
            }
        }
    }

    // Painter's algorithm: far to near.
    prims.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, prim) in &prims {
        prim.draw(cv, f);
    }

    // --- labels, title, annotations, legend --------------------------------
    let tick_style = f.style(base * 0.72, 400, th.muted);
    let label_style = f.style(base * 0.88, 600, th.fg);
    if !axis_off {
        for &t in &xt {
            let (px, py) = cam.sp2(nx(t), 1.16 * yfront, zfloor);
            cv.text(
                px,
                py + 4.0 * s,
                &fmt_num(t, xstep),
                &tick_style,
                Anchor::Middle,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
        for &t in &yt {
            let (px, py) = cam.sp2(1.16 * xfront, ny(t), zfloor);
            cv.text(
                px,
                py + 4.0 * s,
                &fmt_num(t, ystep),
                &tick_style,
                Anchor::Middle,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
        // z ticks belong on whichever back-corner vertical edge projects leftmost
        let cand = [(xback, 1.1 * yfront), (1.1 * xfront, yback)];
        let (zx, zy) =
            if cam.sp2(cand[0].0, cand[0].1, 0.0).0 <= cam.sp2(cand[1].0, cand[1].1, 0.0).0 {
                cand[0]
            } else {
                cand[1]
            };
        for &t in &zt {
            let (px, py) = cam.sp2(zx, zy, nz(t));
            cv.text(
                px - 6.0 * s,
                py + 4.0 * s,
                &fmt_num(t, zstep),
                &tick_style,
                Anchor::End,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
        if let Some(l) = js(p, "zlabel").filter(|l| !l.is_empty()) {
            let (px, py) = cam.sp2(zx, zy, 0.0);
            cv.text(
                px - 34.0 * s,
                py,
                l,
                &label_style,
                Anchor::Middle,
                Baseline::Alphabetic,
                -90.0,
            )?;
        }
        if let Some(l) = js(p, "xlabel")
            .or_else(|| js(p, "x_label"))
            .filter(|l| !l.is_empty())
        {
            let (px, py) = cam.sp2(0.0, 1.42 * yfront, zfloor);
            cv.text(
                px,
                py + 10.0 * s,
                l,
                &label_style,
                Anchor::Middle,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
        if let Some(l) = js(p, "ylabel")
            .or_else(|| js(p, "y_label"))
            .filter(|l| !l.is_empty())
        {
            let (px, py) = cam.sp2(1.42 * xfront, 0.0, zfloor);
            cv.text(
                px,
                py + 10.0 * s,
                l,
                &label_style,
                Anchor::Middle,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
    }

    for a in p
        .get("annotations")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
    {
        let (px, py, _) = dp(jf_or(a, "x", 0.0), jf_or(a, "y", 0.0), jf_or(a, "z", 0.0));
        if let Some(t) = js(a, "text") {
            cv.text(
                px,
                py,
                t,
                &f.style(base * 0.88, 400, th.fg),
                Anchor::Start,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
    }

    if let Some(t) = title {
        cv.text(
            outer.x() + outer.width() / 2.0,
            outer.y() + base * 1.1,
            t,
            &f.style(base * 1.1, 700, th.fg),
            Anchor::Middle,
            Baseline::Alphabetic,
            0.0,
        )?;
    }

    // legend: every series that carries a non-empty `label`/`name`
    let labelled: Vec<(usize, &str)> = series
        .iter()
        .enumerate()
        .filter_map(|(i, sr)| {
            js(sr, "label")
                .or_else(|| js(sr, "name"))
                .filter(|l| !l.is_empty())
                .map(|l| (i, l))
        })
        .collect();
    if !labelled.is_empty() && jb(p, "legend", true) {
        let st = f.style(base * 0.85, 500, th.fg);
        let mut tw: f32 = 0.0;
        for (_, l) in &labelled {
            tw = tw.max(crate::text::measure(l, &st)?.0);
        }
        let lw = tw + 34.0 * s;
        let row = base * 1.35;
        let lh = 8.0 * s + row * labelled.len() as f32;
        let lx = outer.x() + outer.width() - lw - 10.0 * s;
        let ly = outer.y() + 10.0 * s;
        cv.rect(
            lx,
            ly,
            lw,
            lh,
            Some(th.panel),
            Some((th.grid, f.stroke(1.0, None))),
            4.0 * s,
        );
        for (ri, (i, l)) in labelled.iter().enumerate() {
            let yy = ly + 6.0 * s + row * ri as f32;
            cv.rect(
                lx + 8.0 * s,
                yy + row * 0.35,
                14.0 * s,
                5.0 * s,
                Some(series_color(&series[*i], *i, f)),
                None,
                2.0 * s,
            );
            cv.text(
                lx + 28.0 * s,
                yy + row * 0.78,
                l,
                &st,
                Anchor::Start,
                Baseline::Alphabetic,
                0.0,
            )?;
        }
    }

    let _ = jf(p, "unused");
    Ok(())
}
