//! PowerPoint (`.pptx`) writing — PresentationML, hand-rolled.
//!
//! There is no Rust crate that writes PowerPoint, so this module emits the
//! OOXML itself. The package layout mirrors what PowerPoint itself produces
//! for a deck built from the built-in Office theme: one slide master, the
//! caller's slide layouts, one theme, and one part per slide.
//!
//! Everything the caller controls arrives as a single JSON spec — see
//! [`build`] — so the Clojure/Python side stays a pure data transformation and
//! all the XML lives here.
//!
//! Units are EMU (English Metric Units) everywhere a length is expected, and
//! centipoints (`sz="1800"` = 18 pt) everywhere a font size is, which is
//! exactly how `python-pptx` normalises its own `Length`/`Pt` values.

use serde_json::Value;

use crate::office::{
    Pack, RELS_OPEN, ROOT_RELS_TAIL, app_xml, arr, b, b64, core_xml, esc, f, i, office_image, s,
    shex,
};
use crate::util::{R, err};

const A_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const C_NS: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";
const P_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// The stock "Medium Style 2 - Accent 1" table style PowerPoint defaults to.
const TABLE_STYLE_ID: &str = "{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}";

// ---------------------------------------------------------------------------
// relationship parts
// ---------------------------------------------------------------------------

/// The `_rels` file of a single part, built as relationships are needed so
/// every `r:id` in the XML is guaranteed to exist.
#[derive(Default)]
struct Rels(Vec<String>);

impl Rels {
    /// Add an internal relationship, returning the `rIdN` to reference it by.
    fn add(&mut self, kind: &str, target: &str) -> String {
        self.push(kind, target, false)
    }

    /// Add an external relationship (hyperlinks, and nothing else here).
    fn external(&mut self, kind: &str, target: &str) -> String {
        self.push(kind, target, true)
    }

    fn push(&mut self, kind: &str, target: &str, external: bool) -> String {
        let id = format!("rId{}", self.0.len() + 1);
        let mode = if external {
            r#" TargetMode="External""#
        } else {
            ""
        };
        self.0.push(format!(
            r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{}"{mode}/>"#,
            esc(target)
        ));
        id
    }

    fn xml(&self) -> String {
        format!("{RELS_OPEN}{}</Relationships>", self.0.concat())
    }
}

// ---------------------------------------------------------------------------
// small XML helpers
// ---------------------------------------------------------------------------

fn ns_decl() -> String {
    format!(r#"xmlns:a="{A_NS}" xmlns:r="{R_NS}" xmlns:p="{P_NS}""#)
}

/// The empty group-shape header every `spTree` opens with.
fn sp_tree_head() -> String {
    r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#
        .to_string()
}

/// `<a:srgbClr>` with an optional alpha child (`alpha` is 0..1).
pub(crate) fn srgb(hex: &str, alpha: Option<f64>) -> String {
    match alpha {
        Some(a) if a < 1.0 => format!(
            r#"<a:srgbClr val="{hex}"><a:alpha val="{}"/></a:srgbClr>"#,
            (a.clamp(0.0, 1.0) * 100_000.0).round() as i64
        ),
        _ => format!(r#"<a:srgbClr val="{hex}"/>"#),
    }
}

/// Fill spec → DrawingML. Accepts a bare colour, `{"type":"none"}`,
/// `{"type":"solid","color":…,"alpha":…}` or a gradient with stops.
pub(crate) fn fill_xml(v: Option<&Value>) -> String {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return String::new();
    };
    if v.is_string() || v.is_array() {
        return match crate::office::hex6(v) {
            Some(h) => format!("<a:solidFill>{}</a:solidFill>", srgb(&h, None)),
            None => String::new(),
        };
    }
    match s(v, "type").unwrap_or("solid") {
        "none" | "background" => "<a:noFill/>".to_string(),
        "gradient" => {
            let stops = arr(v, "stops");
            if stops.is_empty() {
                return String::new();
            }
            let mut gs = String::new();
            for (n, st) in stops.iter().enumerate() {
                let pos = f(st, "position")
                    .or_else(|| f(st, "pos"))
                    .unwrap_or(n as f64 / (stops.len().max(2) - 1) as f64);
                let hex = shex(st, "color").unwrap_or_else(|| "FFFFFF".into());
                gs.push_str(&format!(
                    r#"<a:gs pos="{}">{}</a:gs>"#,
                    (pos.clamp(0.0, 1.0) * 100_000.0).round() as i64,
                    srgb(&hex, f(st, "alpha"))
                ));
            }
            let ang =
                ((f(v, "angle").unwrap_or(90.0) * 60_000.0).round() as i64).rem_euclid(21_600_000);
            format!(
                r#"<a:gradFill rotWithShape="1"><a:gsLst>{gs}</a:gsLst><a:lin ang="{ang}" scaled="0"/></a:gradFill>"#
            )
        }
        _ => match shex(v, "color") {
            Some(h) => format!("<a:solidFill>{}</a:solidFill>", srgb(&h, f(v, "alpha"))),
            None => String::new(),
        },
    }
}

/// Outline spec → `<a:ln>`.
pub(crate) fn line_xml(v: Option<&Value>) -> String {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return String::new();
    };
    if v.is_string() || v.is_array() {
        return match crate::office::hex6(v) {
            Some(h) => format!("<a:ln><a:solidFill>{}</a:solidFill></a:ln>", srgb(&h, None)),
            None => String::new(),
        };
    }
    let w = i(v, "width")
        .map(|w| format!(r#" w="{w}""#))
        .unwrap_or_default();
    let mut body = String::new();
    if s(v, "type") == Some("none") || b(v, "none") == Some(true) {
        body.push_str("<a:noFill/>");
    } else if let Some(h) = shex(v, "color") {
        body.push_str(&format!(
            "<a:solidFill>{}</a:solidFill>",
            srgb(&h, f(v, "alpha"))
        ));
    }
    if let Some(d) = s(v, "dash") {
        body.push_str(&format!(r#"<a:prstDash val="{}"/>"#, esc(d)));
    }
    if body.is_empty() && w.is_empty() {
        return String::new();
    }
    format!("<a:ln{w}>{body}</a:ln>")
}

/// `<a:xfrm>` for a shape, when it carries geometry of its own. A placeholder
/// without geometry deliberately renders nothing: it then inherits position
/// from the layout, which is the whole point of placeholders.
fn xfrm_xml(v: &Value, tag: &str) -> String {
    let (x, y) = (i(v, "left"), i(v, "top"));
    let (cx, cy) = (i(v, "width"), i(v, "height"));
    if x.is_none() && y.is_none() && cx.is_none() && cy.is_none() {
        return String::new();
    }
    let rot = f(v, "rotation")
        .filter(|r| *r != 0.0)
        .map(|r| {
            format!(
                r#" rot="{}""#,
                ((r * 60_000.0).round() as i64).rem_euclid(21_600_000)
            )
        })
        .unwrap_or_default();
    let fh = if b(v, "flip_h") == Some(true) {
        r#" flipH="1""#
    } else {
        ""
    };
    let fv = if b(v, "flip_v") == Some(true) {
        r#" flipV="1""#
    } else {
        ""
    };
    format!(
        r#"<{tag}{rot}{fh}{fv}><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></{tag}>"#,
        x.unwrap_or(0),
        y.unwrap_or(0),
        cx.unwrap_or(0),
        cy.unwrap_or(0)
    )
}

/// Preset geometry with optional adjustment values.
fn geom_xml(v: &Value) -> String {
    let preset = s(v, "preset").unwrap_or("rect");
    let adj = v.get("adjustments").and_then(|a| a.as_object());
    let av = adj
        .map(|m| {
            m.iter()
                .filter_map(|(k, x)| x.as_f64().map(|n| (k, n)))
                .map(|(k, n)| {
                    format!(
                        r#"<a:gd name="{}" fmla="val {}"/>"#,
                        esc(k),
                        (n * 100_000.0).round() as i64
                    )
                })
                .collect::<String>()
        })
        .unwrap_or_default();
    let av = if av.is_empty() {
        "<a:avLst/>".to_string()
    } else {
        format!("<a:avLst>{av}</a:avLst>")
    };
    format!(r#"<a:prstGeom prst="{}">{av}</a:prstGeom>"#, esc(preset))
}

// ---------------------------------------------------------------------------
// text
// ---------------------------------------------------------------------------

/// Run properties, shared by runs, `endParaRPr` and paragraph-level defaults.
fn rpr(v: &Value, tag: &str, rels: &mut Rels) -> String {
    let mut at = String::from(r#" lang="en-US""#);
    if let Some(sz) = i(v, "size") {
        at.push_str(&format!(r#" sz="{sz}""#));
    }
    for (k, a) in [("bold", "b"), ("italic", "i")] {
        if let Some(x) = b(v, k) {
            at.push_str(&format!(r#" {a}="{}""#, if x { 1 } else { 0 }));
        }
    }
    if let Some(u) = b(v, "underline") {
        at.push_str(&format!(r#" u="{}""#, if u { "sng" } else { "none" }));
    } else if let Some(u) = s(v, "underline") {
        at.push_str(&format!(r#" u="{}""#, esc(u)));
    }
    if b(v, "strike") == Some(true) {
        at.push_str(r#" strike="sngStrike""#);
    }
    match s(v, "baseline") {
        Some("superscript") | Some("sup") => at.push_str(r#" baseline="30000""#),
        Some("subscript") | Some("sub") => at.push_str(r#" baseline="-25000""#),
        _ => {}
    }
    at.push_str(r#" dirty="0""#);

    let mut kids = String::new();
    kids.push_str(&fill_xml(v.get("color")));
    if let Some(name) = s(v, "font").or_else(|| s(v, "typeface")) {
        let t = esc(name);
        kids.push_str(&format!(
            r#"<a:latin typeface="{t}"/><a:cs typeface="{t}"/>"#
        ));
    }
    if let Some(url) = s(v, "hyperlink") {
        let id = rels.external("hyperlink", url);
        kids.push_str(&format!(r#"<a:hlinkClick r:id="{id}"/>"#));
    }
    if kids.is_empty() {
        format!("<{tag}{at}/>")
    } else {
        format!("<{tag}{at}>{kids}</{tag}>")
    }
}

/// Bullet spec → `<a:buNone/>` / character bullet / auto-numbered bullet.
fn bullet_xml(v: &Value) -> String {
    let Some(bu) = v.get("bullet").filter(|x| !x.is_null()) else {
        return String::new();
    };
    if bu.as_bool() == Some(false) {
        return "<a:buNone/>".to_string();
    }
    let mut out = String::new();
    if let Some(h) = shex(bu, "color") {
        out.push_str(&format!("<a:buClr>{}</a:buClr>", srgb(&h, None)));
    }
    if let Some(sz) = f(bu, "size_pct") {
        out.push_str(&format!(
            r#"<a:buSzPct val="{}"/>"#,
            (sz * 1000.0).round() as i64
        ));
    }
    if let Some(font) = s(bu, "font") {
        out.push_str(&format!(r#"<a:buFont typeface="{}"/>"#, esc(font)));
    }
    if let Some(num) = s(bu, "number").or_else(|| s(bu, "auto")) {
        let start = i(bu, "start")
            .filter(|n| *n != 1)
            .map(|n| format!(r#" startAt="{n}""#))
            .unwrap_or_default();
        out.push_str(&format!(r#"<a:buAutoNum type="{}"{start}/>"#, esc(num)));
    } else if let Some(ch) = s(bu, "char").or_else(|| bu.as_str()) {
        out.push_str(&format!(r#"<a:buChar char="{}"/>"#, esc(ch)));
    }
    out
}

/// One `<a:p>`.
fn para_xml(p: &Value, rels: &mut Rels) -> String {
    let mut at = String::new();
    if let Some(l) = i(p, "level").filter(|l| *l > 0) {
        at.push_str(&format!(r#" lvl="{l}""#));
    }
    if let Some(a) = s(p, "align") {
        at.push_str(&format!(r#" algn="{}""#, esc(a)));
    }
    for (k, a) in [("margin_left", "marL"), ("indent", "indent")] {
        if let Some(v) = i(p, k) {
            at.push_str(&format!(r#" {a}="{v}""#));
        }
    }

    let mut ppr_kids = String::new();
    if let Some(pct) = f(p, "line_spacing_pct") {
        ppr_kids.push_str(&format!(
            r#"<a:lnSpc><a:spcPct val="{}"/></a:lnSpc>"#,
            (pct * 100_000.0).round() as i64
        ));
    } else if let Some(pts) = i(p, "line_spacing_pts") {
        ppr_kids.push_str(&format!(r#"<a:lnSpc><a:spcPts val="{pts}"/></a:lnSpc>"#));
    }
    for (k, tag) in [("space_before", "spcBef"), ("space_after", "spcAft")] {
        if let Some(v) = i(p, k) {
            ppr_kids.push_str(&format!(r#"<a:{tag}><a:spcPts val="{v}"/></a:{tag}>"#));
        }
    }
    ppr_kids.push_str(&bullet_xml(p));
    if let Some(font) = p.get("font").filter(|x| x.is_object()) {
        ppr_kids.push_str(&rpr(font, "a:defRPr", rels));
    }

    let ppr = if at.is_empty() && ppr_kids.is_empty() {
        String::new()
    } else if ppr_kids.is_empty() {
        format!("<a:pPr{at}/>")
    } else {
        format!("<a:pPr{at}>{ppr_kids}</a:pPr>")
    };

    let mut runs = String::new();
    for r in arr(p, "runs") {
        if b(r, "break") == Some(true) {
            runs.push_str(&format!("<a:br>{}</a:br>", rpr(r, "a:rPr", rels)));
            continue;
        }
        if let Some(fld) = s(r, "field") {
            // slide numbers and dates are fields, not text
            runs.push_str(&format!(
                r#"<a:fld id="{{{}}}" type="{}">{}<a:t>{}</a:t></a:fld>"#,
                "B7C3A1F0-0000-4000-8000-00000000FFFF",
                esc(fld),
                rpr(r, "a:rPr", rels),
                esc(s(r, "text").unwrap_or(""))
            ));
            continue;
        }
        let text = r.get("text").and_then(|t| t.as_str()).unwrap_or("");
        // xml:space="preserve" or PowerPoint eats leading/trailing spaces.
        runs.push_str(&format!(
            r#"<a:r>{}<a:t xml:space="preserve">{}</a:t></a:r>"#,
            rpr(r, "a:rPr", rels),
            esc(text)
        ));
    }
    if runs.is_empty()
        && let Some(t) = s(p, "text")
    {
        runs.push_str(&format!(
            r#"<a:r>{}<a:t xml:space="preserve">{}</a:t></a:r>"#,
            rpr(p.get("font").unwrap_or(&Value::Null), "a:rPr", rels),
            esc(t)
        ));
    }
    format!("<a:p>{ppr}{runs}</a:p>")
}

/// The body of a `<p:txBody>` / `<a:txBody>`: `bodyPr`, `lstStyle`, paragraphs.
fn text_body(tf: Option<&Value>, rels: &mut Rels) -> String {
    let empty = Value::Null;
    let tf = tf.unwrap_or(&empty);

    let mut at = String::new();
    if let Some(a) = s(tf, "anchor") {
        at.push_str(&format!(r#" anchor="{}""#, esc(a)));
    }
    if b(tf, "anchor_center") == Some(true) {
        at.push_str(r#" anchorCtr="1""#);
    }
    match b(tf, "word_wrap") {
        Some(true) => at.push_str(r#" wrap="square""#),
        Some(false) => at.push_str(r#" wrap="none""#),
        None => {}
    }
    for (k, a) in [
        ("margin_left", "lIns"),
        ("margin_top", "tIns"),
        ("margin_right", "rIns"),
        ("margin_bottom", "bIns"),
    ] {
        if let Some(v) = i(tf, k) {
            at.push_str(&format!(r#" {a}="{v}""#));
        }
    }
    if let Some(v) = i(tf, "vertical") {
        let _ = v; // numeric alias kept out of the XML: `vert` is an enum
    }
    if let Some(v) = s(tf, "vertical") {
        at.push_str(&format!(r#" vert="{}""#, esc(v)));
    }
    let autofit = match s(tf, "auto_size") {
        Some("shape") | Some("spAutoFit") => "<a:spAutoFit/>",
        Some("text") | Some("normAutofit") => "<a:normAutofit/>",
        Some("none") => "<a:noAutofit/>",
        _ => "",
    };
    let body_pr = if autofit.is_empty() {
        format!("<a:bodyPr{at}/>")
    } else {
        format!("<a:bodyPr{at}>{autofit}</a:bodyPr>")
    };

    let paras = arr(tf, "paragraphs");
    let mut out = String::with_capacity(128);
    out.push_str(&body_pr);
    out.push_str("<a:lstStyle/>");
    if paras.is_empty() {
        // A txBody with no paragraph at all is invalid; PowerPoint repairs the
        // file. An empty paragraph is the correct spelling of "no text".
        match s(tf, "text") {
            Some(t) => out.push_str(&para_xml(&serde_json::json!({ "text": t }), rels)),
            None => out.push_str("<a:p/>"),
        }
    } else {
        for p in paras {
            out.push_str(&para_xml(p, rels));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// shapes
// ---------------------------------------------------------------------------

/// `<p:nvPr>` contents — the placeholder declaration, when there is one.
fn nv_pr(sp: &Value) -> String {
    let Some(ph) = sp.get("ph").filter(|x| !x.is_null()) else {
        return "<p:nvPr/>".to_string();
    };
    let mut at = String::new();
    if let Some(t) = s(ph, "type").filter(|t| *t != "obj") {
        at.push_str(&format!(r#" type="{}""#, esc(t)));
    }
    if let Some(idx) = i(ph, "idx").filter(|n| *n > 0) {
        at.push_str(&format!(r#" idx="{idx}""#));
    }
    if let Some(o) = s(ph, "orient") {
        at.push_str(&format!(r#" orient="{}""#, esc(o)));
    }
    format!("<p:nvPr><p:ph{at}/></p:nvPr>")
}

fn shape_props(sp: &Value, with_geom: bool) -> String {
    let mut out = String::from("<p:spPr>");
    out.push_str(&xfrm_xml(sp, "a:xfrm"));
    if with_geom && (sp.get("preset").is_some() || sp.get("ph").is_none()) {
        out.push_str(&geom_xml(sp));
    }
    out.push_str(&fill_xml(sp.get("fill")));
    out.push_str(&line_xml(sp.get("line")));
    if b(sp, "shadow") == Some(false) {
        out.push_str("<a:effectLst/>");
    }
    out.push_str("</p:spPr>");
    out
}

/// A picture: the bytes are staged into `ppt/media` by the caller, which hands
/// back the relationship id.
fn pic_xml(sp: &Value, id: i64, rid: &str) -> String {
    let name = s(sp, "name")
        .map(esc)
        .unwrap_or_else(|| format!("Picture {id}"));
    let crop = sp
        .get("image")
        .and_then(|im| im.get("crop"))
        .or_else(|| sp.get("crop"))
        .filter(|c| !c.is_null())
        .map(|c| {
            let g = |k: &str| (f(c, k).unwrap_or(0.0) * 100_000.0).round() as i64;
            format!(
                r#"<a:srcRect l="{}" t="{}" r="{}" b="{}"/>"#,
                g("left"),
                g("top"),
                g("right"),
                g("bottom")
            )
        })
        .unwrap_or_default();
    format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="{id}" name="{name}"{}/><p:cNvPicPr><a:picLocks noChangeAspect="1"/></p:cNvPicPr>{}</p:nvPicPr><p:blipFill><a:blip r:embed="{rid}"/>{crop}<a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{}{}{}</p:spPr></p:pic>"#,
        s(sp, "description")
            .map(|d| format!(r#" descr="{}""#, esc(d)))
            .unwrap_or_default(),
        nv_pr(sp),
        xfrm_xml(sp, "a:xfrm"),
        geom_xml(sp),
        line_xml(sp.get("line"))
    )
}

/// A table lives inside a graphic frame.
fn table_xml(sp: &Value, id: i64, rels: &mut Rels) -> R<String> {
    let t = sp.get("table").unwrap_or(&Value::Null);
    let rows = arr(t, "rows");
    if rows.is_empty() {
        return err("table shape needs at least one row");
    }
    let cols: Vec<i64> = t
        .get("col_widths")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .map(|x| x.as_f64().unwrap_or(0.0).round() as i64)
                .collect()
        })
        .unwrap_or_default();
    let ncols = if cols.is_empty() {
        arr(&rows[0], "cells").len()
    } else {
        cols.len()
    };
    let total = i(sp, "width").unwrap_or(0).max(1);
    let grid: String = (0..ncols)
        .map(|n| {
            let w = cols.get(n).copied().unwrap_or(total / ncols.max(1) as i64);
            format!(r#"<a:gridCol w="{w}"/>"#)
        })
        .collect();

    let flag = |k: &str, d: bool| if b(t, k).unwrap_or(d) { "1" } else { "0" };
    let mut body = format!(
        r#"<a:tbl><a:tblPr firstRow="{}" firstCol="{}" lastRow="{}" lastCol="{}" bandRow="{}" bandCol="{}"><a:tableStyleId>{TABLE_STYLE_ID}</a:tableStyleId></a:tblPr><a:tblGrid>{grid}</a:tblGrid>"#,
        flag("first_row", true),
        flag("first_col", false),
        flag("last_row", false),
        flag("last_col", false),
        flag("band_row", true),
        flag("band_col", false),
    );
    for row in rows {
        let h = i(row, "height").unwrap_or(370_840);
        body.push_str(&format!(r#"<a:tr h="{h}">"#));
        for cell in arr(row, "cells") {
            let mut at = String::new();
            if let Some(n) = i(cell, "grid_span").filter(|n| *n > 1) {
                at.push_str(&format!(r#" gridSpan="{n}""#));
            }
            if let Some(n) = i(cell, "row_span").filter(|n| *n > 1) {
                at.push_str(&format!(r#" rowSpan="{n}""#));
            }
            if b(cell, "h_merge") == Some(true) {
                at.push_str(r#" hMerge="1""#);
            }
            if b(cell, "v_merge") == Some(true) {
                at.push_str(r#" vMerge="1""#);
            }
            let mut tc_at = String::new();
            for (k, a) in [
                ("margin_left", "marL"),
                ("margin_right", "marR"),
                ("margin_top", "marT"),
                ("margin_bottom", "marB"),
            ] {
                if let Some(v) = i(cell, k) {
                    tc_at.push_str(&format!(r#" {a}="{v}""#));
                }
            }
            if let Some(a) = s(cell, "anchor") {
                tc_at.push_str(&format!(r#" anchor="{}""#, esc(a)));
            }
            let tc_fill = fill_xml(cell.get("fill"));
            let tc_pr = if tc_at.is_empty() && tc_fill.is_empty() {
                "<a:tcPr/>".to_string()
            } else {
                format!("<a:tcPr{tc_at}>{tc_fill}</a:tcPr>")
            };
            body.push_str(&format!(
                r#"<a:tc{at}><a:txBody>{}</a:txBody>{tc_pr}</a:tc>"#,
                text_body(cell.get("text_frame").or(Some(cell)), rels)
            ));
        }
        body.push_str("</a:tr>");
    }
    body.push_str("</a:tbl>");

    Ok(format!(
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="{}"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr>{}</p:nvGraphicFramePr>{}<a:graphic><a:graphicData uri="{A_NS}/table">{body}</a:graphicData></a:graphic></p:graphicFrame>"#,
        s(sp, "name")
            .map(esc)
            .unwrap_or_else(|| format!("Table {id}")),
        nv_pr(sp),
        xfrm_xml(sp, "p:xfrm")
    ))
}

/// Render one shape, staging any media it needs.
fn shape_xml(
    sp: &Value,
    id: i64,
    rels: &mut Rels,
    media: &mut Media,
    charts: &mut Vec<String>,
) -> R<String> {
    let kind = s(sp, "kind")
        .or_else(|| s(sp, "type"))
        .unwrap_or(if sp.get("image").is_some() {
            "picture"
        } else if sp.get("table").is_some() {
            "table"
        } else {
            "textbox"
        });

    match kind {
        "picture" | "image" => {
            let img = sp.get("image").unwrap_or(sp);
            let bytes = b64(img, "data")?;
            let rid = media.stage(&bytes, rels)?;
            Ok(pic_xml(sp, id, &rid))
        }
        "table" => table_xml(sp, id, rels),
        "chart" | "graph" => {
            // The chart lives in its own part; the slide only points at it.
            charts.push(crate::chart::chart_xml(sp.get("chart").unwrap_or(sp)));
            let n = charts.len();
            let rid = rels.add("chart", &format!("../charts/chart{n}.xml"));
            let name = s(sp, "name")
                .map(esc)
                .unwrap_or_else(|| format!("Chart {id}"));
            Ok(format!(
                r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="{id}" name="{name}"/><p:cNvGraphicFramePr/>{}</p:nvGraphicFramePr>{}<a:graphic><a:graphicData uri="{C_NS}"><c:chart xmlns:c="{C_NS}" xmlns:r="{R_NS}" r:id="{rid}"/></a:graphicData></a:graphic></p:graphicFrame>"#,
                nv_pr(sp),
                xfrm_xml(sp, "p:xfrm")
            ))
        }
        "connector" | "line" => {
            let name = s(sp, "name")
                .map(esc)
                .unwrap_or_else(|| format!("Connector {id}"));
            let preset = s(sp, "preset").unwrap_or("line");
            Ok(format!(
                r#"<p:cxnSp><p:nvCxnSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr><p:spPr>{}<a:prstGeom prst="{}"><a:avLst/></a:prstGeom>{}</p:spPr><p:style/></p:cxnSp>"#,
                xfrm_xml(sp, "a:xfrm"),
                esc(preset),
                if sp.get("line").is_some() {
                    line_xml(sp.get("line"))
                } else {
                    format!(
                        "<a:ln><a:solidFill>{}</a:solidFill></a:ln>",
                        srgb("000000", None)
                    )
                }
            ))
        }
        _ => {
            // textbox / auto shape / placeholder — all `<p:sp>`, differing only
            // in whether they declare themselves a text box.
            let name = s(sp, "name")
                .map(esc)
                .unwrap_or_else(|| format!("Shape {id}"));
            let txbox = if kind == "textbox" && sp.get("ph").is_none() {
                r#" txBox="1""#
            } else {
                ""
            };
            let locks = if sp.get("ph").is_some() {
                r#"<a:spLocks noGrp="1"/>"#
            } else {
                ""
            };
            Ok(format!(
                r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr{txbox}>{locks}</p:cNvSpPr>{}</p:nvSpPr>{}<p:txBody>{}</p:txBody></p:sp>"#,
                nv_pr(sp),
                shape_props(sp, kind != "textbox" || sp.get("preset").is_some()),
                text_body(sp.get("text_frame").or(Some(sp)), rels)
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// media staging
// ---------------------------------------------------------------------------

/// Image parts, deduplicated by content so the same picture used on ten slides
/// is stored once.
struct Media {
    parts: Vec<(String, Vec<u8>)>,
    keys: Vec<(u64, usize)>,
    exts: Vec<&'static str>,
}

impl Media {
    fn new() -> Media {
        Media {
            parts: Vec::new(),
            keys: Vec::new(),
            exts: Vec::new(),
        }
    }

    /// Store `data` (re-encoding it if Office cannot embed the format) and add
    /// the relationship to `rels`, returning its `rId`.
    fn stage(&mut self, data: &[u8], rels: &mut Rels) -> R<String> {
        let hash = fxhash(data);
        let idx = match self.keys.iter().find(|(h, _)| *h == hash) {
            Some((_, n)) => *n,
            None => {
                let (bytes, ext, _, _, _) = office_image(data)?;
                let n = self.parts.len();
                self.parts.push((format!("image{}.{ext}", n + 1), bytes));
                if !self.exts.contains(&ext) {
                    self.exts.push(ext);
                }
                self.keys.push((hash, n));
                n
            }
        };
        Ok(rels.add("image", &format!("../media/{}", self.parts[idx].0)))
    }
}

fn fxhash(data: &[u8]) -> u64 {
    // FNV-1a: only ever compared against itself, so speed beats cryptography.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h ^ (data.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

// ---------------------------------------------------------------------------
// masters, layouts, theme
// ---------------------------------------------------------------------------

fn theme_xml(name: &str) -> String {
    let accents = ["4472C4", "ED7D31", "A5A5A5", "FFC000", "5B9BD5", "70AD47"];
    let acc: String = accents
        .iter()
        .enumerate()
        .map(|(n, c)| {
            format!(
                r#"<a:accent{}><a:srgbClr val="{c}"/></a:accent{}>"#,
                n + 1,
                n + 1
            )
        })
        .collect();
    let ln = r#"<a:ln w="6350" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/><a:miter lim="800000"/></a:ln>"#;
    let solid = r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#;
    format!(
        r#"<a:theme xmlns:a="{A_NS}" name="{}"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>{acc}<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Office"><a:fillStyleLst>{solid}{solid}{solid}</a:fillStyleLst><a:lnStyleLst>{ln}{ln}{ln}</a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst>{solid}{solid}{solid}</a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#,
        esc(name)
    )
}

/// A layout's placeholder shapes. Layouts are pure geometry + defaults: the
/// caller supplies them so the layout table lives in one place (the shim) and
/// this module stays a renderer.
fn layout_xml(lay: &Value, n: usize) -> String {
    let mut rels = Rels::default(); // layouts never gain relationships here
    let mut tree = sp_tree_head();
    let mut id = 1;
    for ph in arr(lay, "placeholders") {
        id += 1;
        let name = s(ph, "name")
            .map(esc)
            .unwrap_or_else(|| format!("Placeholder {id}"));
        let mut body = String::new();
        body.push_str(&format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>{}</p:nvSpPr><p:spPr>{}</p:spPr><p:txBody>"#,
            nv_pr(ph),
            xfrm_xml(ph, "a:xfrm")
        ));
        let mut at = String::new();
        if let Some(a) = s(ph, "anchor") {
            at.push_str(&format!(r#" anchor="{}""#, esc(a)));
        }
        let defs = {
            let mut d = String::new();
            if let Some(sz) = i(ph, "size") {
                d.push_str(&format!(r#" sz="{sz}""#));
            }
            if let Some(a) = s(ph, "align") {
                d.push_str(&format!(r#" algn="{}""#, esc(a)));
            }
            if d.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<a:lvl1pPr{}><a:defRPr{}/></a:lvl1pPr>"#,
                    s(ph, "align")
                        .map(|a| format!(r#" algn="{}""#, esc(a)))
                        .unwrap_or_default(),
                    i(ph, "size")
                        .map(|sz| format!(r#" sz="{sz}""#))
                        .unwrap_or_default()
                )
            }
        };
        body.push_str(&format!(
            r#"<a:bodyPr{at}/><a:lstStyle>{defs}</a:lstStyle><a:p><a:endParaRPr lang="en-US"/></a:p>"#
        ));
        body.push_str("</p:txBody></p:sp>");
        tree.push_str(&body);
    }
    let _ = &mut rels;
    let ty = s(lay, "type").unwrap_or("blank");
    let name = s(lay, "name")
        .map(esc)
        .unwrap_or_else(|| format!("Layout {}", n + 1));
    format!(
        r#"<p:sldLayout {} type="{}" preserve="1"><p:cSld name="{name}"><p:spTree>{tree}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#,
        ns_decl(),
        esc(ty)
    )
}

fn master_xml(layout_ids: &[(u32, String)]) -> String {
    let ids: String = layout_ids
        .iter()
        .map(|(id, rid)| format!(r#"<p:sldLayoutId id="{id}" r:id="{rid}"/>"#))
        .collect();
    format!(
        r#"<p:sldMaster {}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree>{}</p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst>{ids}</p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr algn="l"><a:defRPr sz="4400"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr marL="228600" indent="-228600"><a:buChar char="•"/><a:defRPr sz="2800"/></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"#,
        ns_decl(),
        sp_tree_head()
    )
}

fn notes_master_xml() -> String {
    format!(
        r#"<p:notesMaster {}><p:cSld><p:spTree>{}</p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:notesStyle><a:lvl1pPr><a:defRPr sz="1200"/></a:lvl1pPr></p:notesStyle></p:notesMaster>"#,
        ns_decl(),
        sp_tree_head()
    )
}

fn notes_slide_xml(text: &str) -> String {
    let paras: String = text
        .split('\n')
        .map(|line| {
            format!(
                r#"<a:p><a:r><a:rPr lang="en-US" dirty="0"/><a:t xml:space="preserve">{}</a:t></a:r></a:p>"#,
                esc(line)
            )
        })
        .collect();
    format!(
        r#"<p:notes {}><p:cSld><p:spTree>{}<p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"#,
        ns_decl(),
        sp_tree_head()
    )
}

/// The one blank layout used when the caller declares none.
fn default_layouts() -> Value {
    serde_json::json!([{ "name": "Blank", "type": "blank", "placeholders": [] }])
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

/// Build a `.pptx` from a declarative spec.
///
/// ```text
/// {"width": 12192000, "height": 6858000,          // EMU, 16:9 by default
///  "properties": {"title": …, "author": …},
///  "layouts": [{"name": "Title Slide", "type": "title",
///               "placeholders": [{"type": "ctrTitle", "idx": 0,
///                                 "left": …, "top": …, "width": …, "height": …,
///                                 "size": 4400, "align": "ctr"}]}],
///  "slides":  [{"layout": 0, "background": "FFFFFF", "notes": "…",
///               "shapes": [{"kind": "textbox"|"auto"|"picture"|"table"|"connector",
///                           "left": …, "top": …, "width": …, "height": …,
///                           "preset": "roundRect", "fill": …, "line": …,
///                           "image": {"data": "<base64>"},
///                           "text_frame": {"paragraphs": [{"runs": [{"text": "hi",
///                                                                    "bold": true}]}]}}]}]}
/// ```
///
/// Lengths are EMU, font sizes centipoints; everything else is optional.
pub fn build(spec: &Value) -> R<Vec<u8>> {
    let cx = i(spec, "width").unwrap_or(12_192_000).max(1);
    let cy = i(spec, "height").unwrap_or(6_858_000).max(1);

    let layouts_owned;
    let layouts: &[Value] = match spec.get("layouts").and_then(|l| l.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => {
            layouts_owned = default_layouts();
            layouts_owned.as_array().unwrap()
        }
    };
    let slides = arr(spec, "slides");

    let mut pack = Pack::new();
    let mut media = Media::new();
    let mut charts: Vec<String> = Vec::new(); // one `ppt/charts/chartN.xml` each
    let mut ct_over = String::new(); // [Content_Types] overrides
    let has_notes = slides
        .iter()
        .any(|s| crate::office::s(s, "notes").is_some());

    // ---- slide master + layouts -------------------------------------------
    let mut master_rels = Rels::default();
    let mut layout_ids = Vec::new();
    for n in 0..layouts.len() {
        let rid = master_rels.add(
            "slideLayout",
            &format!("../slideLayouts/slideLayout{}.xml", n + 1),
        );
        layout_ids.push((2_147_483_649u32 + n as u32, rid));
    }
    master_rels.add("theme", "../theme/theme1.xml");
    pack.xml(
        "ppt/slideMasters/slideMaster1.xml",
        &master_xml(&layout_ids),
    )?;
    pack.xml(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        &master_rels.xml(),
    )?;
    ct_over.push_str(&over("/ppt/slideMasters/slideMaster1.xml", "slideMaster"));

    for (n, lay) in layouts.iter().enumerate() {
        let mut lr = Rels::default();
        lr.add("slideMaster", "../slideMasters/slideMaster1.xml");
        pack.xml(
            &format!("ppt/slideLayouts/slideLayout{}.xml", n + 1),
            &layout_xml(lay, n),
        )?;
        pack.xml(
            &format!("ppt/slideLayouts/_rels/slideLayout{}.xml.rels", n + 1),
            &lr.xml(),
        )?;
        ct_over.push_str(&over(
            &format!("/ppt/slideLayouts/slideLayout{}.xml", n + 1),
            "slideLayout",
        ));
    }

    // ---- slides ------------------------------------------------------------
    for (n, sl) in slides.iter().enumerate() {
        let li = i(sl, "layout")
            .unwrap_or(0)
            .clamp(0, layouts.len() as i64 - 1) as usize;
        let mut rels = Rels::default();
        rels.add(
            "slideLayout",
            &format!("../slideLayouts/slideLayout{}.xml", li + 1),
        );
        if crate::office::s(sl, "notes").is_some() {
            rels.add(
                "notesSlide",
                &format!("../notesSlides/notesSlide{}.xml", n + 1),
            );
        }

        let mut tree = sp_tree_head();
        let mut id = 1;
        for sp in arr(sl, "shapes") {
            id += 1;
            tree.push_str(&shape_xml(sp, id, &mut rels, &mut media, &mut charts)?);
        }
        let bg = sl
            .get("background")
            .filter(|x| !x.is_null())
            .map(|v| {
                format!(
                    "<p:bg><p:bgPr>{}<a:effectLst/></p:bgPr></p:bg>",
                    fill_xml(Some(v))
                )
            })
            .unwrap_or_default();
        let show = match b(sl, "show") {
            Some(false) => r#" show="0""#,
            _ => "",
        };
        pack.xml(
            &format!("ppt/slides/slide{}.xml", n + 1),
            &format!(
                r#"<p:sld {}{show}><p:cSld>{bg}<p:spTree>{tree}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#,
                ns_decl()
            ),
        )?;
        pack.xml(
            &format!("ppt/slides/_rels/slide{}.xml.rels", n + 1),
            &rels.xml(),
        )?;
        ct_over.push_str(&over(&format!("/ppt/slides/slide{}.xml", n + 1), "slide"));

        if let Some(notes) = crate::office::s(sl, "notes") {
            let mut nr = Rels::default();
            nr.add("notesMaster", "../notesMasters/notesMaster1.xml");
            nr.add("slide", &format!("../slides/slide{}.xml", n + 1));
            pack.xml(
                &format!("ppt/notesSlides/notesSlide{}.xml", n + 1),
                &notes_slide_xml(notes),
            )?;
            pack.xml(
                &format!("ppt/notesSlides/_rels/notesSlide{}.xml.rels", n + 1),
                &nr.xml(),
            )?;
            ct_over.push_str(&over(
                &format!("/ppt/notesSlides/notesSlide{}.xml", n + 1),
                "notesSlide",
            ));
        }
    }

    // ---- notes master ------------------------------------------------------
    if has_notes {
        let mut nmr = Rels::default();
        nmr.add("theme", "../theme/theme2.xml");
        pack.xml("ppt/notesMasters/notesMaster1.xml", &notes_master_xml())?;
        pack.xml("ppt/notesMasters/_rels/notesMaster1.xml.rels", &nmr.xml())?;
        pack.xml("ppt/theme/theme2.xml", &theme_xml("Office Theme"))?;
        ct_over.push_str(&over("/ppt/notesMasters/notesMaster1.xml", "notesMaster"));
        ct_over.push_str(&over("/ppt/theme/theme2.xml", "theme"));
    }

    // ---- charts --------------------------------------------------------------
    for (n, body) in charts.iter().enumerate() {
        pack.xml(&format!("ppt/charts/chart{}.xml", n + 1), body)?;
        ct_over.push_str(&over(&format!("/ppt/charts/chart{}.xml", n + 1), "chart"));
    }

    // ---- media -------------------------------------------------------------
    for (name, bytes) in &media.parts {
        pack.file(&format!("ppt/media/{name}"), bytes)?;
    }

    // ---- theme + tableStyles ----------------------------------------------
    pack.xml(
        "ppt/theme/theme1.xml",
        &theme_xml(s(spec, "theme").unwrap_or("Office Theme")),
    )?;
    ct_over.push_str(&over("/ppt/theme/theme1.xml", "theme"));
    pack.xml(
        "ppt/tableStyles.xml",
        &format!(r#"<a:tblStyleLst xmlns:a="{A_NS}" def="{TABLE_STYLE_ID}"/>"#),
    )?;
    ct_over.push_str(&over("/ppt/tableStyles.xml", "tableStyles"));

    // ---- presentation ------------------------------------------------------
    let mut pres_rels = Rels::default();
    let master_rid = pres_rels.add("slideMaster", "slideMasters/slideMaster1.xml");
    let mut sld_ids = String::new();
    for n in 0..slides.len() {
        let rid = pres_rels.add("slide", &format!("slides/slide{}.xml", n + 1));
        sld_ids.push_str(&format!(
            r#"<p:sldId id="{}" r:id="{rid}"/>"#,
            256 + n as u32
        ));
    }
    let notes_master = if has_notes {
        let rid = pres_rels.add("notesMaster", "notesMasters/notesMaster1.xml");
        format!(r#"<p:notesMasterIdLst><p:notesMasterId r:id="{rid}"/></p:notesMasterIdLst>"#)
    } else {
        String::new()
    };
    pres_rels.add("theme", "theme/theme1.xml");
    pres_rels.add("tableStyles", "tableStyles.xml");

    pack.xml(
        "ppt/presentation.xml",
        &format!(
            r#"<p:presentation {} saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="{master_rid}"/></p:sldMasterIdLst>{notes_master}<p:sldIdLst>{sld_ids}</p:sldIdLst><p:sldSz cx="{cx}" cy="{cy}"/><p:notesSz cx="6858000" cy="9144000"/><p:defaultTextStyle><a:defPPr><a:defRPr lang="en-US"/></a:defPPr></p:defaultTextStyle></p:presentation>"#,
            ns_decl()
        ),
    )?;
    pack.xml("ppt/_rels/presentation.xml.rels", &pres_rels.xml())?;
    ct_over.push_str(&over("/ppt/presentation.xml", "presentation"));

    // ---- docProps + root ---------------------------------------------------
    pack.xml("docProps/core.xml", &core_xml(spec))?;
    pack.xml(
        "docProps/app.xml",
        &app_xml(
            "Microsoft Office PowerPoint",
            &format!(
                "<PresentationFormat>On-screen Show</PresentationFormat><Slides>{}</Slides><Company></Company>",
                slides.len()
            ),
        ),
    )?;
    ct_over.push_str(&over("/docProps/core.xml", "coreProps"));
    ct_over.push_str(&over("/docProps/app.xml", "appProps"));

    let mut defaults = String::from(
        r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>"#,
    );
    for ext in &media.exts {
        let ct = match *ext {
            "png" => "image/png",
            "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            _ => "image/tiff",
        };
        defaults.push_str(&format!(
            r#"<Default Extension="{ext}" ContentType="{ct}"/>"#
        ));
    }
    pack.xml(
        "[Content_Types].xml",
        &format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">{defaults}{ct_over}</Types>"#
        ),
    )?;
    pack.xml(
        "_rels/.rels",
        &format!(
            r#"{RELS_OPEN}<Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/>{ROOT_RELS_TAIL}"#
        ),
    )?;

    pack.finish()
}

/// One `[Content_Types].xml` override, by short part kind.
fn over(part: &str, kind: &str) -> String {
    let ct = match kind {
        "presentation" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
        }
        "slide" => "application/vnd.openxmlformats-officedocument.presentationml.slide+xml",
        "slideLayout" => {
            "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"
        }
        "slideMaster" => {
            "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"
        }
        "notesSlide" => {
            "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"
        }
        "notesMaster" => {
            "application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"
        }
        "tableStyles" => {
            "application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"
        }
        "chart" => "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
        "theme" => "application/vnd.openxmlformats-officedocument.theme+xml",
        "coreProps" => "application/vnd.openxmlformats-package.core-properties+xml",
        _ => "application/vnd.openxmlformats-officedocument.extended-properties+xml",
    };
    format!(r#"<Override PartName="{part}" ContentType="{ct}"/>"#)
}
