//! `.docx` (WordprocessingML) writer.
//!
//! One JSON document spec in, a Word file out — no Java, no `python-docx`, no
//! templates on disk. The XML is written by hand because WordprocessingML for
//! the shapes people actually need (styled runs, headings, lists, tables,
//! images, page breaks) is small and completely specified, and hand-writing it
//! keeps the dependency list at `zip` + `flate2`.
//!
//! ```json
//! {"title": "Report",
//!  "blocks": [{"type": "heading", "level": 1, "text": "Q3"},
//!             {"type": "paragraph",
//!              "runs": [{"text": "revenue "}, {"text": "up", "bold": true}]},
//!             {"type": "table", "header": true,
//!              "rows": [{"cells": [{"text": "a"}, {"text": "b"}]}]}]}
//! ```
//!
//! Units are the ones the format uses: half-points for font sizes, twips
//! (1/1440 in) for spacing and table widths, EMU (1/914400 in) for images.

use serde_json::Value;

use crate::office::{
    EMU_PER_PT, Pack, arr, b, b64, esc, f, hex6, i, image_info, office_image, s, shex,
};
use crate::util::R;

const PT: f64 = 20.0; // twips per point

/// Render `spec` to the bytes of a `.docx` package.
pub fn build(spec: &Value) -> R<Vec<u8>> {
    let mut p = Pack::new();
    let dflt_font = s(spec, "default_font").unwrap_or("Calibri").to_string();
    let dflt_size = f(spec, "default_size").unwrap_or(11.0);

    // Media + hyperlinks are collected while the body is built: both need a
    // relationship id that only the rels part knows about.
    let mut rels: Vec<String> = Vec::new();
    let mut media: Vec<(String, Vec<u8>)> = Vec::new();
    let mut body = String::new();
    // `blocks` is the canonical key; `body` is accepted as the obvious synonym.
    let blocks = match arr(spec, "blocks") {
        [] => arr(spec, "body"),
        b => b,
    };
    for blk in blocks {
        body.push_str(&block(blk, &mut rels, &mut media)?);
    }
    body.push_str(&sect_pr(spec));

    let doc = format!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>{body}</w:body></w:document>"#
    );

    let mut rel_xml = String::from(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>"#,
    );
    for r in &rels {
        rel_xml.push_str(r);
    }
    rel_xml.push_str("</Relationships>");

    let mut types = String::from(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/><Default Extension="gif" ContentType="image/gif"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/><Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#,
    );
    types.push_str("</Types>");

    p.xml("[Content_Types].xml", &types)?;
    p.xml(
        "_rels/.rels",
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#,
    )?;
    p.xml("docProps/core.xml", &crate::office::core_xml(spec))?;
    p.xml(
        "docProps/app.xml",
        &crate::office::app_xml("clj-imaging", ""),
    )?;
    p.xml("word/document.xml", &doc)?;
    p.xml("word/_rels/document.xml.rels", &rel_xml)?;
    p.xml("word/styles.xml", &styles(&dflt_font, dflt_size))?;
    p.xml("word/numbering.xml", NUMBERING)?;
    for (name, bytes) in media {
        p.file(&format!("word/media/{name}"), &bytes)?;
    }
    p.finish()
}

/// Page size + margins. A4 portrait unless the spec says otherwise; `landscape`
/// swaps the page dimensions, which is all Word means by the word.
fn sect_pr(spec: &Value) -> String {
    let page = spec.get("page").cloned().unwrap_or(Value::Null);
    let (mut w, mut h) = (
        f(&page, "width_pt").unwrap_or(595.0) * PT,
        f(&page, "height_pt").unwrap_or(842.0) * PT,
    );
    if b(spec, "landscape").unwrap_or(false) || b(&page, "landscape").unwrap_or(false) {
        std::mem::swap(&mut w, &mut h);
    }
    let m = f(&page, "margin_pt").unwrap_or(72.0) * PT;
    let orient = if w > h { " w:orient=\"landscape\"" } else { "" };
    format!(
        r#"<w:sectPr><w:pgSz w:w="{}" w:h="{}"{orient}/><w:pgMar w:top="{}" w:right="{}" w:bottom="{}" w:left="{}" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr>"#,
        w.round() as i64,
        h.round() as i64,
        f(&page, "margin_top_pt").unwrap_or(m / PT).max(0.0) * PT,
        f(&page, "margin_right_pt").unwrap_or(m / PT).max(0.0) * PT,
        f(&page, "margin_bottom_pt").unwrap_or(m / PT).max(0.0) * PT,
        f(&page, "margin_left_pt").unwrap_or(m / PT).max(0.0) * PT,
    )
}

// ---------------------------------------------------------------------------
// blocks
// ---------------------------------------------------------------------------

fn block(v: &Value, rels: &mut Vec<String>, media: &mut Vec<(String, Vec<u8>)>) -> R<String> {
    match s(v, "type").unwrap_or("paragraph") {
        "page_break" => Ok(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#.to_string()),
        "table" => table(v, rels, media),
        "image" => {
            let mut p = String::from("<w:p>");
            if let Some(a) = align_of(v) {
                p.push_str(&format!("<w:pPr><w:jc w:val=\"{a}\"/></w:pPr>"));
            }
            p.push_str(&image_run(v, rels, media)?);
            p.push_str("</w:p>");
            Ok(p)
        }
        _ => paragraph(v, rels, media),
    }
}

fn align_of(v: &Value) -> Option<&'static str> {
    match s(v, "align")? {
        "center" | "centre" => Some("center"),
        "right" | "end" => Some("right"),
        "justify" | "both" => Some("both"),
        "left" | "start" => Some("left"),
        _ => None,
    }
}

/// A paragraph: `style`/`level` pick the named style, `runs` (or a plain `text`)
/// carry the inline formatting, `bullet`/`number` attach the list numbering.
fn paragraph(v: &Value, rels: &mut Vec<String>, media: &mut Vec<(String, Vec<u8>)>) -> R<String> {
    let kind = s(v, "type").unwrap_or("paragraph");
    let level = i(v, "level").unwrap_or(1).clamp(1, 6);
    let style = match s(v, "style") {
        Some(st) => st.to_string(),
        None if kind == "heading" => format!("Heading{level}"),
        None if kind == "title" => "Title".to_string(),
        None => String::new(),
    };

    let mut pr = String::new();
    if !style.is_empty() {
        pr.push_str(&format!(r#"<w:pStyle w:val="{}"/>"#, esc(&style)));
    }
    let bullet = b(v, "bullet").unwrap_or(false);
    let number = b(v, "number").unwrap_or(false);
    if bullet || number {
        let ilvl = i(v, "indent_level").unwrap_or(0).clamp(0, 8);
        let numid = if number { 2 } else { 1 };
        pr.push_str(&format!(
            r#"<w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="{numid}"/></w:numPr>"#
        ));
    }
    if b(v, "page_break_before").unwrap_or(false) {
        pr.push_str("<w:pageBreakBefore/>");
    }
    if b(v, "keep_with_next").unwrap_or(false) {
        pr.push_str("<w:keepNext/>");
    }
    let (before, after, line) = (
        f(v, "space_before_pt"),
        f(v, "space_after_pt"),
        f(v, "line_spacing"),
    );
    if before.is_some() || after.is_some() || line.is_some() {
        let mut sp = String::from("<w:spacing");
        if let Some(x) = before {
            sp.push_str(&format!(r#" w:before="{}""#, (x * PT).round() as i64));
        }
        if let Some(x) = after {
            sp.push_str(&format!(r#" w:after="{}""#, (x * PT).round() as i64));
        }
        if let Some(x) = line {
            sp.push_str(&format!(
                r#" w:line="{}" w:lineRule="auto""#,
                (x * 240.0).round() as i64
            ));
        }
        sp.push_str("/>");
        pr.push_str(&sp);
    }
    let (il, ifirst, ihang) = (
        f(v, "indent_pt"),
        f(v, "indent_first_pt"),
        f(v, "indent_hanging_pt"),
    );
    if il.is_some() || ifirst.is_some() || ihang.is_some() {
        let mut ind = String::from("<w:ind");
        if let Some(x) = il {
            ind.push_str(&format!(r#" w:left="{}""#, (x * PT).round() as i64));
        }
        if let Some(x) = ifirst {
            ind.push_str(&format!(r#" w:firstLine="{}""#, (x * PT).round() as i64));
        }
        if let Some(x) = ihang {
            ind.push_str(&format!(r#" w:hanging="{}""#, (x * PT).round() as i64));
        }
        ind.push_str("/>");
        pr.push_str(&ind);
    }
    if let Some(a) = align_of(v) {
        pr.push_str(&format!(r#"<w:jc w:val="{a}"/>"#));
    }
    if let Some(c) = shex(v, "shading") {
        pr.push_str(&format!(
            r#"<w:shd w:val="clear" w:color="auto" w:fill="{c}"/>"#
        ));
    }

    let mut out = String::from("<w:p>");
    if !pr.is_empty() {
        out.push_str(&format!("<w:pPr>{pr}</w:pPr>"));
    }
    let runs = arr(v, "runs");
    if runs.is_empty() {
        if let Some(t) = s(v, "text") {
            out.push_str(&run(&serde_json::json!({"text": t}), v)?);
        }
    } else {
        for r in runs {
            if s(r, "type") == Some("image") || r.get("data").is_some() {
                out.push_str(&image_run(r, rels, media)?);
            } else if let Some(url) = s(r, "link") {
                let id = format!("rIdL{}", rels.len() + 10);
                rels.push(format!(
                    r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{}" TargetMode="External"/>"#,
                    esc(url)
                ));
                let mut lr = r.clone();
                if lr.get("color").is_none() {
                    lr["color"] = Value::from("#0563C1");
                }
                if lr.get("underline").is_none() {
                    lr["underline"] = Value::from(true);
                }
                out.push_str(&format!(
                    r#"<w:hyperlink r:id="{id}">{}</w:hyperlink>"#,
                    run(&lr, v)?
                ));
            } else {
                out.push_str(&run(r, v)?);
            }
        }
    }
    out.push_str("</w:p>");
    Ok(out)
}

/// One `<w:r>`: character properties then the text (or a break/tab).
fn run(r: &Value, parent: &Value) -> R<String> {
    let mut pr = String::new();
    let font = s(r, "font").or_else(|| s(parent, "font"));
    if let Some(fa) = font {
        pr.push_str(&format!(
            r#"<w:rFonts w:ascii="{f}" w:hAnsi="{f}" w:cs="{f}"/>"#,
            f = esc(fa)
        ));
    }
    if b(r, "bold").or_else(|| b(parent, "bold")).unwrap_or(false) {
        pr.push_str("<w:b/>");
    }
    if b(r, "italic")
        .or_else(|| b(parent, "italic"))
        .unwrap_or(false)
    {
        pr.push_str("<w:i/>");
    }
    if b(r, "strike").unwrap_or(false) {
        pr.push_str("<w:strike/>");
    }
    if b(r, "all_caps").unwrap_or(false) {
        pr.push_str("<w:caps/>");
    }
    if b(r, "small_caps").unwrap_or(false) {
        pr.push_str("<w:smallCaps/>");
    }
    match r.get("underline") {
        Some(Value::Bool(true)) => pr.push_str(r#"<w:u w:val="single"/>"#),
        Some(Value::String(u)) => pr.push_str(&format!(r#"<w:u w:val="{}"/>"#, esc(u))),
        _ => {}
    }
    if let Some(c) = shex(r, "color").or_else(|| shex(parent, "color")) {
        pr.push_str(&format!(r#"<w:color w:val="{c}"/>"#));
    }
    if let Some(sz) = f(r, "size").or_else(|| f(parent, "size")) {
        let hp = (sz * 2.0).round() as i64;
        pr.push_str(&format!(r#"<w:sz w:val="{hp}"/><w:szCs w:val="{hp}"/>"#));
    }
    if let Some(h) = s(r, "highlight") {
        pr.push_str(&format!(r#"<w:highlight w:val="{}"/>"#, esc(h)));
    }
    if let Some(c) = shex(r, "shading") {
        pr.push_str(&format!(
            r#"<w:shd w:val="clear" w:color="auto" w:fill="{c}"/>"#
        ));
    }
    if b(r, "superscript").unwrap_or(false) {
        pr.push_str(r#"<w:vertAlign w:val="superscript"/>"#);
    }
    if b(r, "subscript").unwrap_or(false) {
        pr.push_str(r#"<w:vertAlign w:val="subscript"/>"#);
    }
    if let Some(ls) = f(r, "letter_spacing_pt") {
        pr.push_str(&format!(
            r#"<w:spacing w:val="{}"/>"#,
            (ls * PT).round() as i64
        ));
    }

    let mut body = String::new();
    match s(r, "break") {
        Some("page") => body.push_str(r#"<w:br w:type="page"/>"#),
        Some("column") => body.push_str(r#"<w:br w:type="column"/>"#),
        Some(_) => body.push_str("<w:br/>"),
        None => {}
    }
    if b(r, "tab").unwrap_or(false) {
        body.push_str("<w:tab/>");
    }
    if let Some(t) = s(r, "text")
        && !t.is_empty()
    {
        // Split on newlines so a multi-line run reads as multi-line in Word.
        let mut first = true;
        for line in t.split('\n') {
            if !first {
                body.push_str("<w:br/>");
            }
            first = false;
            body.push_str(&format!(r#"<w:t xml:space="preserve">{}</w:t>"#, esc(line)));
        }
    }
    Ok(format!(
        "<w:r>{}{}</w:r>",
        if pr.is_empty() {
            String::new()
        } else {
            format!("<w:rPr>{pr}</w:rPr>")
        },
        body
    ))
}

/// An inline picture: the bytes go to `word/media`, the run carries the DrawingML.
fn image_run(v: &Value, rels: &mut Vec<String>, media: &mut Vec<(String, Vec<u8>)>) -> R<String> {
    let raw = b64(v, "data")?;
    let (bytes, ext, _mime, px_w, px_h) = office_image(&raw)?;
    let idx = media.len() + 1;
    let name = format!("image{idx}.{ext}");
    let id = format!("rIdI{idx}");
    rels.push(format!(
        r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/{name}"/>"#
    ));
    media.push((name, bytes));

    // Width/height in points, defaulting to the pixel size at 96 dpi.
    let (mut w, mut h) = (px_w as f64 * 0.75, px_h as f64 * 0.75);
    match (f(v, "width_pt"), f(v, "height_pt")) {
        (Some(a), Some(b)) => {
            w = a;
            h = b;
        }
        (Some(a), None) => {
            h *= a / w.max(1e-6);
            w = a;
        }
        (None, Some(b)) => {
            w *= b / h.max(1e-6);
            h = b;
        }
        (None, None) => {}
    }
    let (cx, cy) = (
        (w * EMU_PER_PT).round() as i64,
        (h * EMU_PER_PT).round() as i64,
    );
    let alt = esc(s(v, "alt").unwrap_or("image"));
    Ok(format!(
        r#"<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="{idx}" name="{alt}"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="{idx}" name="{alt}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{id}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
    ))
}

// ---------------------------------------------------------------------------
// tables
// ---------------------------------------------------------------------------

fn table(v: &Value, rels: &mut Vec<String>, media: &mut Vec<(String, Vec<u8>)>) -> R<String> {
    let rows = arr(v, "rows");
    let ncols = rows
        .iter()
        .map(|r| arr(r, "cells").len())
        .max()
        .unwrap_or(0)
        .max(1);
    let widths: Vec<f64> = match v.get("columns_pt").and_then(|c| c.as_array()) {
        Some(a) if !a.is_empty() => a.iter().filter_map(|x| x.as_f64()).collect(),
        _ => {
            let total = f(v, "width_pt").unwrap_or(468.0);
            vec![total / ncols as f64; ncols]
        }
    };
    let borders = b(v, "borders").unwrap_or(true);
    let bsz = f(v, "border_width_pt").unwrap_or(0.5) * 8.0; // eighths of a point
    let bcolor = shex(v, "border_color").unwrap_or_else(|| "BFBFBF".into());
    let bxml = if borders {
        let side = |n: &str| {
            format!(
                r#"<w:{n} w:val="single" w:sz="{}" w:space="0" w:color="{bcolor}"/>"#,
                bsz.round().max(2.0) as i64
            )
        };
        format!(
            "<w:tblBorders>{}{}{}{}{}{}</w:tblBorders>",
            side("top"),
            side("left"),
            side("bottom"),
            side("right"),
            side("insideH"),
            side("insideV")
        )
    } else {
        String::new()
    };

    let mut out = format!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="{}" w:type="dxa"/>{bxml}<w:tblLayout w:type="fixed"/></w:tblPr><w:tblGrid>"#,
        (widths.iter().sum::<f64>() * PT).round() as i64
    );
    for w in &widths {
        out.push_str(&format!(
            r#"<w:gridCol w:w="{}"/>"#,
            (w * PT).round() as i64
        ));
    }
    out.push_str("</w:tblGrid>");

    let header_rows = if b(v, "header").unwrap_or(false) {
        1
    } else {
        0
    };
    for (ri, row) in rows.iter().enumerate() {
        let is_head = ri < header_rows || b(row, "header").unwrap_or(false);
        out.push_str("<w:tr>");
        let mut rpr = String::new();
        if is_head {
            rpr.push_str("<w:tblHeader/>");
        }
        if let Some(h) = f(row, "height_pt") {
            rpr.push_str(&format!(
                r#"<w:trHeight w:val="{}"/>"#,
                (h * PT).round() as i64
            ));
        }
        if !rpr.is_empty() {
            out.push_str(&format!("<w:trPr>{rpr}</w:trPr>"));
        }
        let cells = arr(row, "cells");
        let mut ci = 0usize;
        for cell in cells {
            let span = i(cell, "colspan").unwrap_or(1).max(1) as usize;
            let w: f64 = widths
                .iter()
                .skip(ci)
                .take(span)
                .sum::<f64>()
                .max(widths.first().cloned().unwrap_or(72.0));
            let mut cpr = format!(r#"<w:tcW w:w="{}" w:type="dxa"/>"#, (w * PT).round() as i64);
            if span > 1 {
                cpr.push_str(&format!(r#"<w:gridSpan w:val="{span}"/>"#));
            }
            if let Some(c) = shex(cell, "shading")
                .or_else(|| {
                    if is_head {
                        shex(v, "header_shading")
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    if is_head {
                        Some("EDF2F7".to_string())
                    } else {
                        None
                    }
                })
            {
                cpr.push_str(&format!(
                    r#"<w:shd w:val="clear" w:color="auto" w:fill="{c}"/>"#
                ));
            }
            match s(cell, "valign") {
                Some("center") | Some("middle") => cpr.push_str(r#"<w:vAlign w:val="center"/>"#),
                Some("bottom") => cpr.push_str(r#"<w:vAlign w:val="bottom"/>"#),
                _ => {}
            }
            out.push_str(&format!("<w:tc><w:tcPr>{cpr}</w:tcPr>"));
            let paras = arr(cell, "paragraphs");
            if paras.is_empty() {
                let mut p = cell.clone();
                if is_head && p.get("bold").is_none() {
                    p["bold"] = Value::from(true);
                }
                out.push_str(&paragraph(&p, rels, media)?);
            } else {
                for p in paras {
                    out.push_str(&paragraph(p, rels, media)?);
                }
            }
            out.push_str("</w:tc>");
            ci += span;
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl><w:p/>");
    Ok(out)
}

// ---------------------------------------------------------------------------
// styles + numbering
// ---------------------------------------------------------------------------

fn styles(font: &str, size: f64) -> String {
    let hp = (size * 2.0).round() as i64;
    let f = esc(font);
    let mut s = format!(
        r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="{f}" w:hAnsi="{f}" w:cs="{f}"/><w:sz w:val="{hp}"/><w:szCs w:val="{hp}"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:before="0" w:after="240"/></w:pPr><w:rPr><w:b/><w:sz w:val="56"/><w:szCs w:val="56"/></w:rPr></w:style>"#
    );
    for (lvl, sz) in [(1, 32), (2, 26), (3, 24), (4, 22), (5, 20), (6, 20)] {
        s.push_str(&format!(
            r#"<w:style w:type="paragraph" w:styleId="Heading{lvl}"><w:name w:val="heading {lvl}"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:outlineLvl w:val="{}"/><w:spacing w:before="240" w:after="120"/></w:pPr><w:rPr><w:b/><w:color w:val="1F3864"/><w:sz w:val="{sz}"/><w:szCs w:val="{sz}"/></w:rPr></w:style>"#,
            lvl - 1
        ));
    }
    s.push_str(
        r#"<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:ind w:left="720"/></w:pPr><w:rPr><w:i/><w:color w:val="595959"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:shd w:val="clear" w:color="auto" w:fill="F5F5F5"/><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="20"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:ind w:left="720"/><w:contextualSpacing/></w:pPr></w:style><w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:tblPr><w:tblCellMar><w:top w:w="72" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="72" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style></w:styles>"#,
    );
    s
}

/// Two lists: `numId` 1 is the bullet, `numId` 2 the decimal outline.
const NUMBERING: &str = r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="1"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#9679;"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol" w:hint="default"/></w:rPr></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#9675;"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#9642;"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="2160" w:hanging="360"/></w:pPr></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="2"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%3."/><w:lvlJc w:val="right"/><w:pPr><w:ind w:left="2160" w:hanging="180"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="2"/></w:num></w:numbering>"#;

/// Exposed for the reader's round-trip test: a docx we wrote must parse back.
#[allow(dead_code)]
pub fn probe(data: &[u8]) -> R<(usize, u32, u32)> {
    let (_, _, w, h) = image_info(data)?;
    Ok((data.len(), w, h))
}

#[allow(dead_code)]
fn unused_hex(v: &Value) -> Option<String> {
    hex6(v)
}
