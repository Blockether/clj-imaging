//! DrawingML charts (`ppt/charts/chartN.xml`) for the PowerPoint writer.
//!
//! PowerPoint reads a chart from the *cached* data in the chart part; the
//! embedded workbook only matters when a user edits the data in place. This
//! module therefore writes caches only, which keeps a chart a pure function of
//! its JSON spec and keeps the package free of a nested `.xlsx`.
//!
//! Element order is load-bearing: every sequence below follows the CT_* order
//! in the ECMA-376 chart schema, because PowerPoint rejects a chart part whose
//! children are out of order rather than repairing it.

use serde_json::Value;

use crate::office::{arr, b, esc, f, i, s, shex};
use crate::pptx::{fill_xml, line_xml, srgb};

const C_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";
const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Stable, arbitrary axis ids — one deck never mixes two charts' axes.
const AX_CAT: i64 = 111_111_111;
const AX_VAL: i64 = 222_222_222;

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// A number out of JSON, tolerating numbers spelled as strings.
fn nf(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Render a number the way Excel caches it: integers without a decimal point.
fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let mut s = format!("{x}");
        if s.ends_with(".0") {
            s.truncate(s.len() - 2);
        }
        s
    }
}

/// A cached label, however the caller spelled it.
fn label(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => nf(other).map(fmt_num).unwrap_or_else(|| other.to_string()),
    }
}

/// `A`, `B`, … `AA` — the column of a value series in the phantom worksheet.
fn col_letter(n: usize) -> String {
    let mut out = String::new();
    let mut n = n + 1;
    while n > 0 {
        let r = (n - 1) % 26;
        out.insert(0, (b'A' + r as u8) as char);
        n = (n - 1) / 26;
    }
    out
}

fn c_val(tag: &str, val: &str) -> String {
    format!(r#"<c:{tag} val="{val}"/>"#)
}

fn str_cache(vals: &[Value]) -> String {
    let pts: String = vals
        .iter()
        .enumerate()
        .map(|(n, v)| format!(r#"<c:pt idx="{n}"><c:v>{}</c:v></c:pt>"#, esc(&label(v))))
        .collect();
    format!(
        r#"<c:strCache><c:ptCount val="{}"/>{pts}</c:strCache>"#,
        vals.len()
    )
}

fn num_cache(vals: &[Value], fmt: &str) -> String {
    let pts: String = vals
        .iter()
        .enumerate()
        .filter_map(|(n, v)| {
            nf(v).map(|x| format!(r#"<c:pt idx="{n}"><c:v>{}</c:v></c:pt>"#, fmt_num(x)))
        })
        .collect();
    format!(
        r#"<c:numCache><c:formatCode>{}</c:formatCode><c:ptCount val="{}"/>{pts}</c:numCache>"#,
        esc(fmt),
        vals.len()
    )
}

/// `Sheet1!$B$2:$B$5` — the reference Excel would have written.
fn sheet_ref(col: &str, first: usize, last: usize) -> String {
    if last <= first {
        format!("Sheet1!${col}${first}")
    } else {
        format!("Sheet1!${col}${first}:${col}${last}")
    }
}

/// Categories, cached as text unless every one of them is a number.
fn cat_xml(cats: &[Value], fmt: &str) -> String {
    if cats.is_empty() {
        return String::new();
    }
    let r = sheet_ref("A", 2, cats.len() + 1);
    if cats.iter().all(|c| c.is_number()) {
        format!(
            r#"<c:cat><c:numRef><c:f>{r}</c:f>{}</c:numRef></c:cat>"#,
            num_cache(cats, fmt)
        )
    } else {
        format!(
            r#"<c:cat><c:strRef><c:f>{r}</c:f>{}</c:strRef></c:cat>"#,
            str_cache(cats)
        )
    }
}

fn num_ref(tag: &str, col: &str, vals: &[Value], fmt: &str) -> String {
    let r = sheet_ref(col, 2, vals.len() + 1);
    format!(
        r#"<c:{tag}><c:numRef><c:f>{r}</c:f>{}</c:numRef></c:{tag}>"#,
        num_cache(vals, fmt)
    )
}

/// The series name, cached from its header cell.
fn tx_xml(name: Option<&str>, col: &str) -> String {
    let Some(name) = name else {
        return String::new();
    };
    format!(
        r#"<c:tx><c:strRef><c:f>Sheet1!${col}$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>{}</c:v></c:pt></c:strCache></c:strRef></c:tx>"#,
        esc(name)
    )
}

/// `<a:defRPr>` attributes and children for a chart-text font spec.
fn def_rpr(font: Option<&Value>) -> String {
    let Some(font) = font.filter(|v| !v.is_null()) else {
        return String::new();
    };
    let mut at = String::new();
    if let Some(sz) = i(font, "size") {
        at.push_str(&format!(r#" sz="{sz}""#));
    }
    for (k, a) in [("bold", "b"), ("italic", "i")] {
        if let Some(x) = b(font, k) {
            at.push_str(&format!(r#" {a}="{}""#, if x { 1 } else { 0 }));
        }
    }
    let mut kids = String::new();
    if let Some(h) = shex(font, "color") {
        kids.push_str(&format!("<a:solidFill>{}</a:solidFill>", srgb(&h, None)));
    }
    if let Some(name) = s(font, "font").or_else(|| s(font, "typeface")) {
        let t = esc(name);
        kids.push_str(&format!(
            r#"<a:latin typeface="{t}"/><a:cs typeface="{t}"/>"#
        ));
    }
    if kids.is_empty() {
        format!("<a:defRPr{at}/>")
    } else {
        format!("<a:defRPr{at}>{kids}</a:defRPr>")
    }
}

/// `<c:txPr>` — the text properties of an axis, legend or label set.
fn tx_pr(font: Option<&Value>) -> String {
    let rpr = def_rpr(font);
    if rpr.is_empty() {
        return String::new();
    }
    format!(
        r#"<c:txPr><a:bodyPr/><a:lstStyle/><a:p><a:pPr>{rpr}</a:pPr><a:endParaRPr lang="en-US"/></a:p></c:txPr>"#
    )
}

/// Shape properties of a series, point, axis line or the plot area itself.
fn sp_pr(v: &Value) -> String {
    let fill = fill_xml(v.get("fill"));
    let line = if v.get("line").is_some() {
        line_xml(v.get("line"))
    } else {
        String::new()
    };
    if fill.is_empty() && line.is_empty() {
        String::new()
    } else {
        format!("<c:spPr>{fill}{line}</c:spPr>")
    }
}

/// `<c:dLbls>` — data labels, either switched off or configured.
fn dlbls(v: Option<&Value>) -> String {
    let Some(v) = v.filter(|x| !x.is_null()) else {
        return String::new();
    };
    if v.as_bool() == Some(false) {
        return r#"<c:dLbls><c:delete val="1"/></c:dLbls>"#.to_string();
    }
    let on = |k: &str, dflt: bool| b(v, k).unwrap_or(dflt);
    let mut out = String::from("<c:dLbls>");
    if let Some(fmt) = s(v, "number_format") {
        out.push_str(&format!(
            r#"<c:numFmt formatCode="{}" sourceLinked="0"/>"#,
            esc(fmt)
        ));
    }
    out.push_str(&sp_pr(v));
    out.push_str(&tx_pr(v.get("font")));
    if let Some(pos) = s(v, "position") {
        out.push_str(&c_val("dLblPos", &esc(pos)));
    }
    out.push_str(&c_val(
        "showLegendKey",
        if on("show_legend_key", false) {
            "1"
        } else {
            "0"
        },
    ));
    out.push_str(&c_val(
        "showVal",
        if on("show_value", true) { "1" } else { "0" },
    ));
    out.push_str(&c_val(
        "showCatName",
        if on("show_category", false) { "1" } else { "0" },
    ));
    out.push_str(&c_val(
        "showSerName",
        if on("show_series", false) { "1" } else { "0" },
    ));
    out.push_str(&c_val(
        "showPercent",
        if on("show_percent", false) { "1" } else { "0" },
    ));
    out.push_str(&c_val(
        "showBubbleSize",
        if on("show_bubble_size", false) {
            "1"
        } else {
            "0"
        },
    ));
    if let Some(sep) = s(v, "separator") {
        out.push_str(&format!("<c:separator>{}</c:separator>", esc(sep)));
    }
    out.push_str("</c:dLbls>");
    out
}

/// `<c:marker>` on a line/scatter/radar series.
fn marker(v: &Value, dflt: &str) -> String {
    let m = v.get("marker").filter(|x| !x.is_null());
    let sym = m
        .and_then(|m| s(m, "symbol").or_else(|| m.as_str()))
        .unwrap_or(match m.and_then(|m| m.as_bool()) {
            Some(false) => "none",
            Some(true) => "circle",
            None => dflt,
        });
    let mut kids = format!(r#"<c:symbol val="{}"/>"#, esc(sym));
    if let Some(m) = m {
        if let Some(sz) = i(m, "size") {
            kids.push_str(&c_val("size", &sz.clamp(2, 72).to_string()));
        }
        let fill = fill_xml(m.get("fill").or_else(|| m.get("color")));
        if !fill.is_empty() {
            kids.push_str(&format!("<c:spPr>{fill}</c:spPr>"));
        }
    }
    format!("<c:marker>{kids}</c:marker>")
}

/// Per-point overrides (`dPt`), which is how pie slices get their own colours.
fn d_pts(ser: &Value) -> String {
    arr(ser, "points")
        .iter()
        .enumerate()
        .map(|(n, p)| {
            let sp = sp_pr(p);
            if sp.is_empty() {
                String::new()
            } else {
                format!(r#"<c:dPt><c:idx val="{n}"/><c:bubble3D val="0"/>{sp}</c:dPt>"#)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// series
// ---------------------------------------------------------------------------

struct SerCtx<'a> {
    cats: &'a [Value],
    fmt: &'a str,
    family: &'a str,
}

fn series_xml(n: usize, ser: &Value, cx: &SerCtx) -> String {
    let col = col_letter(n + 1); // column A holds the categories
    let head = format!(
        r#"<c:idx val="{n}"/><c:order val="{n}"/>{}{}"#,
        tx_xml(s(ser, "name"), &col),
        sp_pr(ser)
    );
    let vals = arr(ser, "values");
    let cat = cat_xml(cx.cats, "General");
    let labels = dlbls(ser.get("data_labels"));
    let pts = d_pts(ser);
    let smooth = c_val(
        "smooth",
        if b(ser, "smooth").unwrap_or(false) {
            "1"
        } else {
            "0"
        },
    );

    match cx.family {
        "bar" | "bubble" => {
            let inv = c_val(
                "invertIfNegative",
                if b(ser, "invert_if_negative").unwrap_or(false) {
                    "1"
                } else {
                    "0"
                },
            );
            if cx.family == "bubble" {
                let xs = arr(ser, "x_values");
                let ys = arr(ser, "y_values");
                let sizes = arr(ser, "sizes");
                format!(
                    "<c:ser>{head}{inv}{pts}{labels}{}{}{}<c:bubble3D val=\"0\"/></c:ser>",
                    num_ref("xVal", "A", xs, "General"),
                    num_ref("yVal", &col, ys, cx.fmt),
                    num_ref("bubbleSize", &col_letter(n + 2), sizes, "General")
                )
            } else {
                format!(
                    "<c:ser>{head}{inv}{pts}{labels}{cat}{}</c:ser>",
                    num_ref("val", &col, vals, cx.fmt)
                )
            }
        }
        "line" | "radar" => format!(
            "<c:ser>{head}{}{pts}{labels}{cat}{}{smooth}</c:ser>",
            marker(
                ser,
                if cx.family == "radar" {
                    "none"
                } else {
                    "circle"
                }
            ),
            num_ref("val", &col, vals, cx.fmt)
        ),
        "scatter" => {
            let xs = arr(ser, "x_values");
            let ys = if arr(ser, "y_values").is_empty() {
                vals
            } else {
                arr(ser, "y_values")
            };
            format!(
                "<c:ser>{head}{}{pts}{labels}{}{}{smooth}</c:ser>",
                marker(ser, "circle"),
                num_ref("xVal", "A", xs, "General"),
                num_ref("yVal", &col, ys, cx.fmt)
            )
        }
        // pie / doughnut
        _ => {
            let explosion = i(ser, "explosion")
                .map(|e| c_val("explosion", &e.to_string()))
                .unwrap_or_default();
            format!(
                "<c:ser>{head}{explosion}{pts}{labels}{cat}{}</c:ser>",
                num_ref("val", &col, vals, cx.fmt)
            )
        }
    }
}

// ---------------------------------------------------------------------------
// axes
// ---------------------------------------------------------------------------

fn scaling(ax: Option<&Value>) -> String {
    let mut out = String::from("<c:scaling>");
    if let Some(base) = ax.and_then(|a| f(a, "log_base")) {
        out.push_str(&c_val("logBase", &fmt_num(base)));
    }
    let rev = ax.and_then(|a| b(a, "reverse")).unwrap_or(false);
    out.push_str(&c_val("orientation", if rev { "maxMin" } else { "minMax" }));
    if let Some(max) = ax.and_then(|a| f(a, "max")) {
        out.push_str(&c_val("max", &fmt_num(max)));
    }
    if let Some(min) = ax.and_then(|a| f(a, "min")) {
        out.push_str(&c_val("min", &fmt_num(min)));
    }
    out.push_str("</c:scaling>");
    out
}

/// The shared head of every axis: id, scaling, visibility, position, grid,
/// title, number format, ticks and text.
fn axis_head(id: i64, ax: Option<&Value>, pos: &str, grid_default: bool) -> String {
    let deleted = ax
        .and_then(|a| b(a, "visible"))
        .map(|v| !v)
        .unwrap_or(false);
    let mut out = format!(
        r#"<c:axId val="{id}"/>{}<c:delete val="{}"/><c:axPos val="{pos}"/>"#,
        scaling(ax),
        if deleted { 1 } else { 0 }
    );
    let grid = ax
        .and_then(|a| b(a, "major_gridlines"))
        .unwrap_or(grid_default);
    if grid {
        out.push_str("<c:majorGridlines/>");
    }
    if ax.and_then(|a| b(a, "minor_gridlines")).unwrap_or(false) {
        out.push_str("<c:minorGridlines/>");
    }
    if let Some(t) = ax.and_then(|a| s(a, "title")) {
        out.push_str(&format!(
            r#"<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:pPr>{}</a:pPr><a:r><a:rPr lang="en-US"/><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val="0"/></c:title>"#,
            def_rpr(ax.and_then(|a| a.get("title_font"))),
            esc(t)
        ));
    }
    if let Some(fmt) = ax.and_then(|a| s(a, "number_format")) {
        out.push_str(&format!(
            r#"<c:numFmt formatCode="{}" sourceLinked="0"/>"#,
            esc(fmt)
        ));
    }
    out.push_str(&c_val(
        "majorTickMark",
        ax.and_then(|a| s(a, "major_tick_mark")).unwrap_or("out"),
    ));
    out.push_str(&c_val(
        "minorTickMark",
        ax.and_then(|a| s(a, "minor_tick_mark")).unwrap_or("none"),
    ));
    out.push_str(&c_val(
        "tickLblPos",
        ax.and_then(|a| s(a, "tick_label_position"))
            .unwrap_or("nextTo"),
    ));
    if let Some(ax) = ax {
        out.push_str(&sp_pr(ax));
    }
    out.push_str(&tx_pr(ax.and_then(|a| a.get("tick_labels_font"))));
    out
}

fn cat_ax(ax: Option<&Value>, cross: i64) -> String {
    format!(
        r#"<c:catAx>{}<c:crossAx val="{cross}"/><c:crosses val="autoZero"/><c:auto val="1"/><c:lblAlgn val="ctr"/><c:lblOffset val="100"/><c:noMultiLvlLbl val="0"/></c:catAx>"#,
        axis_head(AX_CAT, ax, "b", false)
    )
}

fn val_ax(ax: Option<&Value>, id: i64, cross: i64, pos: &str, grid: bool) -> String {
    let mut tail = format!(r#"<c:crossAx val="{cross}"/><c:crosses val="autoZero"/>"#);
    tail.push_str(&c_val("crossBetween", "between"));
    if let Some(u) = ax.and_then(|a| f(a, "major_unit")) {
        tail.push_str(&c_val("majorUnit", &fmt_num(u)));
    }
    if let Some(u) = ax.and_then(|a| f(a, "minor_unit")) {
        tail.push_str(&c_val("minorUnit", &fmt_num(u)));
    }
    format!("<c:valAx>{}{tail}</c:valAx>", axis_head(id, ax, pos, grid))
}

// ---------------------------------------------------------------------------
// chart part
// ---------------------------------------------------------------------------

/// Which XML chart group a spec's `type` belongs to, plus the attributes that
/// distinguish the variants inside it.
fn family(kind: &str) -> (&'static str, &'static str, &'static str) {
    // (family, bar direction, grouping)
    match kind {
        "bar" | "bar_clustered" | "barClustered" => ("bar", "bar", "clustered"),
        "bar_stacked" | "barStacked" => ("bar", "bar", "stacked"),
        "bar_percent_stacked" | "barPercentStacked" => ("bar", "bar", "percentStacked"),
        "column" | "column_clustered" | "columnClustered" => ("bar", "col", "clustered"),
        "column_stacked" | "columnStacked" => ("bar", "col", "stacked"),
        "column_percent_stacked" | "columnPercentStacked" => ("bar", "col", "percentStacked"),
        "line" | "line_markers" | "lineMarkers" => ("line", "", "standard"),
        "line_stacked" | "lineStacked" => ("line", "", "stacked"),
        "line_percent_stacked" | "linePercentStacked" => ("line", "", "percentStacked"),
        "area" => ("area", "", "standard"),
        "area_stacked" | "areaStacked" => ("area", "", "stacked"),
        "area_percent_stacked" | "areaPercentStacked" => ("area", "", "percentStacked"),
        "pie" => ("pie", "", ""),
        "doughnut" | "donut" => ("doughnut", "", ""),
        "radar" | "radar_markers" | "radarMarkers" | "radar_filled" | "radarFilled" => {
            ("radar", "", "")
        }
        "scatter" | "xy_scatter" | "xyScatter" | "scatter_lines" | "scatterLines" => {
            ("scatter", "", "")
        }
        "bubble" => ("bubble", "", ""),
        _ => ("bar", "col", "clustered"),
    }
}

/// Build one `ppt/charts/chartN.xml` body from a chart spec.
pub fn chart_xml(spec: &Value) -> String {
    let kind = s(spec, "type").unwrap_or("column");
    let (fam, bar_dir, grouping) = family(kind);
    let bar_dir = s(spec, "bar_direction").unwrap_or(bar_dir);
    let grouping = s(spec, "grouping").unwrap_or(grouping);
    let cats = arr(spec, "categories");
    let sers = arr(spec, "series");
    let fmt = s(spec, "number_format").unwrap_or("General");
    let vary = b(spec, "vary_colors").unwrap_or(matches!(fam, "pie" | "doughnut"));
    let cx = SerCtx {
        cats,
        fmt,
        family: fam,
    };
    let body: String = sers
        .iter()
        .enumerate()
        .map(|(n, ser)| series_xml(n, ser, &cx))
        .collect();
    let labels = dlbls(spec.get("data_labels"));
    let vary_x = c_val("varyColors", if vary { "1" } else { "0" });
    let axes_ids = format!(r#"<c:axId val="{AX_CAT}"/><c:axId val="{AX_VAL}"/>"#);

    let (group, axes) = match fam {
        "bar" => (
            format!(
                r#"<c:barChart><c:barDir val="{}"/><c:grouping val="{}"/>{vary_x}{body}{labels}{}{}{axes_ids}</c:barChart>"#,
                esc(bar_dir),
                esc(grouping),
                c_val("gapWidth", &i(spec, "gap_width").unwrap_or(150).to_string()),
                c_val(
                    "overlap",
                    &i(spec, "overlap")
                        .unwrap_or(if grouping == "clustered" { -27 } else { 100 })
                        .to_string()
                ),
            ),
            format!(
                "{}{}",
                cat_ax(spec.get("category_axis"), AX_VAL),
                val_ax(spec.get("value_axis"), AX_VAL, AX_CAT, "l", true)
            ),
        ),
        "line" => (
            format!(
                r#"<c:lineChart><c:grouping val="{}"/>{vary_x}{body}{labels}{}{}{axes_ids}</c:lineChart>"#,
                esc(grouping),
                c_val(
                    "marker",
                    if b(spec, "markers").unwrap_or(kind.contains("marker")) {
                        "1"
                    } else {
                        "0"
                    }
                ),
                c_val(
                    "smooth",
                    if b(spec, "smooth").unwrap_or(false) {
                        "1"
                    } else {
                        "0"
                    }
                ),
            ),
            format!(
                "{}{}",
                cat_ax(spec.get("category_axis"), AX_VAL),
                val_ax(spec.get("value_axis"), AX_VAL, AX_CAT, "l", true)
            ),
        ),
        "area" => (
            format!(
                r#"<c:areaChart><c:grouping val="{}"/>{vary_x}{body}{labels}{axes_ids}</c:areaChart>"#,
                esc(grouping)
            ),
            format!(
                "{}{}",
                cat_ax(spec.get("category_axis"), AX_VAL),
                val_ax(spec.get("value_axis"), AX_VAL, AX_CAT, "l", true)
            ),
        ),
        "pie" => (
            format!(
                r#"<c:pieChart>{vary_x}{body}{labels}<c:firstSliceAng val="{}"/></c:pieChart>"#,
                i(spec, "first_slice_angle").unwrap_or(0).rem_euclid(360)
            ),
            String::new(),
        ),
        "doughnut" => (
            format!(
                r#"<c:doughnutChart>{vary_x}{body}{labels}<c:firstSliceAng val="{}"/><c:holeSize val="{}"/></c:doughnutChart>"#,
                i(spec, "first_slice_angle").unwrap_or(0).rem_euclid(360),
                i(spec, "hole_size").unwrap_or(50).clamp(1, 90)
            ),
            String::new(),
        ),
        "radar" => (
            format!(
                r#"<c:radarChart><c:radarStyle val="{}"/>{vary_x}{body}{labels}{axes_ids}</c:radarChart>"#,
                esc(
                    s(spec, "radar_style").unwrap_or(if kind.contains("filled") {
                        "filled"
                    } else {
                        "marker"
                    })
                )
            ),
            format!(
                "{}{}",
                cat_ax(spec.get("category_axis"), AX_VAL),
                val_ax(spec.get("value_axis"), AX_VAL, AX_CAT, "l", true)
            ),
        ),
        "scatter" => (
            format!(
                r#"<c:scatterChart><c:scatterStyle val="{}"/>{vary_x}{body}{labels}{axes_ids}</c:scatterChart>"#,
                esc(s(spec, "scatter_style").unwrap_or("lineMarker"))
            ),
            format!(
                "{}{}",
                val_ax(spec.get("x_axis"), AX_CAT, AX_VAL, "b", false),
                val_ax(
                    spec.get("y_axis").or_else(|| spec.get("value_axis")),
                    AX_VAL,
                    AX_CAT,
                    "l",
                    true
                )
            ),
        ),
        // bubble
        _ => (
            format!(
                r#"<c:bubbleChart>{vary_x}{body}{labels}<c:bubble3D val="0"/><c:bubbleScale val="{}"/><c:showNegBubbles val="0"/>{axes_ids}</c:bubbleChart>"#,
                i(spec, "bubble_scale").unwrap_or(100).clamp(0, 300)
            ),
            format!(
                "{}{}",
                val_ax(spec.get("x_axis"), AX_CAT, AX_VAL, "b", false),
                val_ax(
                    spec.get("y_axis").or_else(|| spec.get("value_axis")),
                    AX_VAL,
                    AX_CAT,
                    "l",
                    true
                )
            ),
        ),
    };

    let title = match spec.get("title").filter(|t| !t.is_null()) {
        Some(t) if t.as_bool() == Some(false) => (String::new(), true),
        Some(t) => {
            let text = t.as_str().map(esc).unwrap_or_default();
            (
                format!(
                    r#"<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:pPr>{}</a:pPr><a:r><a:rPr lang="en-US"/><a:t>{text}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val="0"/></c:title>"#,
                    def_rpr(spec.get("title_font"))
                ),
                false,
            )
        }
        None => (String::new(), true),
    };
    let (title_xml, title_deleted) = title;

    let legend = match spec.get("legend").filter(|l| !l.is_null()) {
        None => String::new(),
        Some(l) if l.as_bool() == Some(false) => String::new(),
        Some(l) => {
            let pos = l.as_str().or_else(|| s(l, "position")).unwrap_or("r");
            let overlay = b(l, "overlay").unwrap_or(false);
            format!(
                r#"<c:legend><c:legendPos val="{}"/><c:overlay val="{}"/>{}</c:legend>"#,
                esc(pos),
                if overlay { 1 } else { 0 },
                tx_pr(l.get("font"))
            )
        }
    };

    format!(
        r#"<c:chartSpace xmlns:c="{C_NS}" xmlns:a="{A_NS}" xmlns:r="{R_NS}"><c:date1904 val="0"/><c:lang val="en-US"/><c:roundedCorners val="0"/><c:chart>{title_xml}<c:autoTitleDeleted val="{}"/><c:plotArea><c:layout/>{group}{axes}{}</c:plotArea>{legend}<c:plotVisOnly val="1"/><c:dispBlanksAs val="{}"/></c:chart>{}</c:chartSpace>"#,
        if title_deleted { 1 } else { 0 },
        spec.get("plot_area").map(sp_pr).unwrap_or_default(),
        esc(s(spec, "display_blanks").unwrap_or("gap")),
        sp_pr(spec)
    )
}
