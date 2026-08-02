//! Reading a `.pptx` back into JSON.
//!
//! [`crate::office::read`] used to return nothing but a flat list of `<a:t>`
//! strings per slide. That is enough to grep a deck, but not enough to drive a
//! `python-pptx`-shaped object model, so this module walks the DrawingML shape
//! tree instead and reports every shape with its kind, name, geometry, text,
//! table and picture/chart reference — the read side of `pptx::build`.

use std::io::{Cursor, Read};

use serde_json::{Map, Value};

use crate::util::R;

// ---------------------------------------------------------------------------
// a very small XML DOM
// ---------------------------------------------------------------------------

/// One element: its tag, attributes, direct text and children. Namespaces are
/// kept verbatim (`a:off`) because OOXML prefixes are fixed by the spec.
pub(crate) struct Node {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub kids: Vec<Node>,
}

impl Node {
    fn new(tag: String, attrs: Vec<(String, String)>) -> Node {
        Node {
            tag,
            attrs,
            text: String::new(),
            kids: Vec::new(),
        }
    }

    /// Attribute by exact name.
    pub(crate) fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }

    fn num(&self, k: &str) -> Option<i64> {
        self.attr(k).and_then(|v| v.parse::<i64>().ok())
    }

    /// Direct child by tag.
    pub(crate) fn kid(&self, tag: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.tag == tag)
    }

    /// First descendant (depth-first) by tag, self excluded.
    pub(crate) fn find(&self, tag: &str) -> Option<&Node> {
        for k in &self.kids {
            if k.tag == tag {
                return Some(k);
            }
            if let Some(n) = k.find(tag) {
                return Some(n);
            }
        }
        None
    }

    /// Every descendant with that tag, without descending into a match.
    fn find_all<'a>(&'a self, tag: &str, out: &mut Vec<&'a Node>) {
        for k in &self.kids {
            if k.tag == tag {
                out.push(k);
            } else {
                k.find_all(tag, out);
            }
        }
    }

    /// Concatenated text of this element and everything below it.
    fn all_text(&self) -> String {
        let mut s = self.text.clone();
        for k in &self.kids {
            s.push_str(&k.all_text());
        }
        s
    }
}

/// Parse a whole XML part into [`Node`]s. Malformed input yields whatever was
/// read so far — a reader should never be the thing that fails a pipeline.
pub(crate) fn parse(xml: &str) -> Node {
    use quick_xml::events::Event;
    let mut rd = quick_xml::Reader::from_str(xml);
    let mut root = Node::new("#root".into(), Vec::new());
    let mut stack: Vec<Node> = Vec::new();
    let decoder = rd.decoder();
    let attrs_of = |e: &quick_xml::events::BytesStart| -> Vec<(String, String)> {
        e.attributes()
            .filter_map(|a| a.ok())
            .map(|a| {
                (
                    String::from_utf8_lossy(a.key.as_ref()).to_string(),
                    a.decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                )
            })
            .collect()
    };
    loop {
        match rd.read_event() {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                stack.push(Node::new(tag, attrs_of(&e)));
            }
            Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let node = Node::new(tag, attrs_of(&e));
                stack.last_mut().unwrap_or(&mut root).kids.push(node);
            }
            Ok(Event::Text(t)) => {
                let s = t.decode().map(|v| v.to_string()).unwrap_or_default();
                if !s.is_empty() {
                    stack.last_mut().unwrap_or(&mut root).text.push_str(&s);
                }
            }
            Ok(Event::CData(t)) => {
                let s = String::from_utf8_lossy(t.as_ref()).to_string();
                stack.last_mut().unwrap_or(&mut root).text.push_str(&s);
            }
            Ok(Event::End(_)) => {
                if let Some(done) = stack.pop() {
                    stack.last_mut().unwrap_or(&mut root).kids.push(done);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    while let Some(done) = stack.pop() {
        stack.last_mut().unwrap_or(&mut root).kids.push(done);
    }
    root
}

// ---------------------------------------------------------------------------
// package access
// ---------------------------------------------------------------------------

struct Zip {
    inner: zip::ZipArchive<Cursor<Vec<u8>>>,
}

impl Zip {
    fn open(data: &[u8]) -> R<Zip> {
        Ok(Zip {
            inner: zip::ZipArchive::new(Cursor::new(data.to_vec()))
                .map_err(|e| format!("not a package: {e}"))?,
        })
    }

    fn names(&self) -> Vec<String> {
        self.inner.file_names().map(|n| n.to_string()).collect()
    }

    fn text(&mut self, name: &str) -> Option<String> {
        let mut f = self.inner.by_name(name).ok()?;
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok()?;
        Some(buf)
    }

    fn bytes(&mut self, name: &str) -> Option<Vec<u8>> {
        let mut f = self.inner.by_name(name).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

/// `_rels` sibling of a part: relationship id → resolved package path.
fn rels_of(z: &mut Zip, part: &str) -> Vec<(String, String)> {
    let (dir, file) = match part.rfind('/') {
        Some(i) => (&part[..i], &part[i + 1..]),
        None => ("", part),
    };
    let rels = format!("{dir}/_rels/{file}.rels");
    let Some(xml) = z.text(&rels) else {
        return Vec::new();
    };
    let root = parse(&xml);
    let mut out = Vec::new();
    let mut all = Vec::new();
    root.find_all("Relationship", &mut all);
    for r in all {
        let (Some(id), Some(target)) = (r.attr("Id"), r.attr("Target")) else {
            continue;
        };
        let mode = r.attr("TargetMode").unwrap_or("");
        let path = if mode == "External" || target.starts_with("http") {
            target.to_string()
        } else {
            resolve(dir, target)
        };
        out.push((id.to_string(), path));
    }
    out
}

/// Join a relationship target against the part's directory, folding `..`.
fn resolve(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

// ---------------------------------------------------------------------------
// shapes
// ---------------------------------------------------------------------------

fn put(m: &mut Map<String, Value>, k: &str, v: Value) {
    if !v.is_null() {
        m.insert(k.to_string(), v);
    }
}

/// `<a:xfrm>` → `left`/`top`/`width`/`height`/`rotation`/`flip_h`/`flip_v`.
fn geometry(sp: &Node, m: &mut Map<String, Value>) {
    let Some(x) = sp.find("a:xfrm") else { return };
    if let Some(off) = x.kid("a:off") {
        put(
            m,
            "left",
            off.num("x").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            m,
            "top",
            off.num("y").map(Value::from).unwrap_or(Value::Null),
        );
    }
    if let Some(ext) = x.kid("a:ext") {
        put(
            m,
            "width",
            ext.num("cx").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            m,
            "height",
            ext.num("cy").map(Value::from).unwrap_or(Value::Null),
        );
    }
    if let Some(rot) = x.num("rot") {
        put(m, "rotation", Value::from(rot as f64 / 60_000.0));
    }
    for (a, k) in [("flipH", "flip_h"), ("flipV", "flip_v")] {
        if x.attr(a) == Some("1") {
            put(m, k, Value::from(true));
        }
    }
}

/// One `<a:r>`/`<a:fld>`/`<a:br>` sequence → the run list of a paragraph.
fn runs_of(p: &Node) -> Vec<Value> {
    let mut out = Vec::new();
    for k in &p.kids {
        let mut r = Map::new();
        match k.tag.as_str() {
            "a:r" | "a:fld" => {
                let text = k.kid("a:t").map(|t| t.all_text()).unwrap_or_default();
                r.insert("text".into(), Value::from(text));
                if k.tag == "a:fld" {
                    put(
                        r_mut(&mut r),
                        "field",
                        k.attr("type").map(Value::from).unwrap_or(Value::Null),
                    );
                }
                if let Some(pr) = k.kid("a:rPr") {
                    font_of(pr, &mut r);
                }
            }
            "a:br" => {
                r.insert("break".into(), Value::from(true));
            }
            _ => continue,
        }
        out.push(Value::Object(r));
    }
    out
}

fn r_mut(m: &mut Map<String, Value>) -> &mut Map<String, Value> {
    m
}

/// `<a:rPr>`/`<a:defRPr>` → the run's font attributes.
fn font_of(pr: &Node, r: &mut Map<String, Value>) {
    for (a, k) in [
        ("b", "bold"),
        ("i", "italic"),
        ("strike", "strike"),
        ("u", "underline"),
    ] {
        match (a, pr.attr(a)) {
            ("u", Some(v)) => put(r, k, Value::from(v != "none")),
            (_, Some(v)) => put(r, k, Value::from(v == "1")),
            _ => {}
        }
    }
    if let Some(sz) = pr.num("sz") {
        put(r, "size", Value::from(sz));
    }
    if let Some(l) = pr.kid("a:latin").and_then(|n| n.attr("typeface")) {
        put(r, "font", Value::from(l));
    }
    if let Some(c) = pr
        .kid("a:solidFill")
        .and_then(|f| f.kid("a:srgbClr"))
        .and_then(|c| c.attr("val"))
    {
        put(r, "color", Value::from(c));
    }
    if let Some(h) = pr.kid("a:hlinkClick") {
        put(
            r,
            "hyperlink",
            h.attr("r:id").map(Value::from).unwrap_or(Value::Null),
        );
    }
}

/// `<p:txBody>`/`<a:txBody>` → `{paragraphs:[{text,level,align,runs}]}`.
fn text_frame(body: &Node) -> Value {
    let mut paras = Vec::new();
    let mut ps = Vec::new();
    body.find_all("a:p", &mut ps);
    for p in ps {
        let mut m = Map::new();
        let runs = runs_of(p);
        let text: String = runs
            .iter()
            .map(|r| r.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect();
        m.insert("text".into(), Value::from(text));
        if let Some(ppr) = p.kid("a:pPr") {
            if let Some(l) = ppr.num("lvl") {
                put(&mut m, "level", Value::from(l));
            }
            put(
                &mut m,
                "align",
                ppr.attr("algn").map(Value::from).unwrap_or(Value::Null),
            );
        }
        m.insert("runs".into(), Value::Array(runs));
        paras.push(Value::Object(m));
    }
    let mut tf = Map::new();
    if let Some(bp) = body.kid("a:bodyPr") {
        put(
            &mut tf,
            "anchor",
            bp.attr("anchor").map(Value::from).unwrap_or(Value::Null),
        );
        if let Some(w) = bp.attr("wrap") {
            put(&mut tf, "word_wrap", Value::from(w != "none"));
        }
    }
    tf.insert("paragraphs".into(), Value::Array(paras));
    Value::Object(tf)
}

/// The plain text of a text body, one line per paragraph.
fn body_text(tf: &Value) -> String {
    tf.get("paragraphs")
        .and_then(|p| p.as_array())
        .map(|ps| {
            ps.iter()
                .map(|p| p.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `<a:tbl>` → `{rows:[{height,cells:[{text,row_span,grid_span,...}]}],col_widths}`.
fn table_of(tbl: &Node) -> Value {
    let mut m = Map::new();
    if let Some(grid) = tbl.kid("a:tblGrid") {
        let widths: Vec<Value> = grid
            .kids
            .iter()
            .filter(|k| k.tag == "a:gridCol")
            .filter_map(|k| k.num("w").map(Value::from))
            .collect();
        if !widths.is_empty() {
            m.insert("col_widths".into(), Value::Array(widths));
        }
    }
    if let Some(pr) = tbl.kid("a:tblPr") {
        for (a, k) in [
            ("firstRow", "first_row"),
            ("firstCol", "first_col"),
            ("lastRow", "last_row"),
            ("lastCol", "last_col"),
            ("bandRow", "band_row"),
            ("bandCol", "band_col"),
        ] {
            if pr.attr(a) == Some("1") {
                put(&mut m, k, Value::from(true));
            }
        }
    }
    let mut rows = Vec::new();
    for tr in tbl.kids.iter().filter(|k| k.tag == "a:tr") {
        let mut rm = Map::new();
        put(
            &mut rm,
            "height",
            tr.num("h").map(Value::from).unwrap_or(Value::Null),
        );
        let mut cells = Vec::new();
        for tc in tr.kids.iter().filter(|k| k.tag == "a:tc") {
            let mut cm = Map::new();
            let tf = tc.kid("a:txBody").map(text_frame).unwrap_or(Value::Null);
            cm.insert("text".into(), Value::from(body_text(&tf)));
            if !tf.is_null() {
                cm.insert("text_frame".into(), tf);
            }
            for (a, k) in [("gridSpan", "grid_span"), ("rowSpan", "row_span")] {
                put(
                    &mut cm,
                    k,
                    tc.num(a)
                        .filter(|n| *n > 1)
                        .map(Value::from)
                        .unwrap_or(Value::Null),
                );
            }
            for (a, k) in [("hMerge", "h_merge"), ("vMerge", "v_merge")] {
                if tc.attr(a) == Some("1") {
                    put(&mut cm, k, Value::from(true));
                }
            }
            if let Some(anchor) = tc.kid("a:tcPr").and_then(|p| p.attr("anchor")) {
                put(&mut cm, "anchor", Value::from(anchor));
            }
            cells.push(Value::Object(cm));
        }
        rm.insert("cells".into(), Value::Array(cells));
        rows.push(Value::Object(rm));
    }
    m.insert("rows".into(), Value::Array(rows));
    Value::Object(m)
}

/// `<a:srgbClr val="RRGGBB"><a:alpha val="…"/></a:srgbClr>` → `(hex, alpha)`.
fn srgb_of(n: &Node) -> Option<(String, Option<f64>)> {
    let c = n.kid("a:srgbClr")?;
    let hex = c.attr("val")?.to_string();
    let alpha = c
        .kid("a:alpha")
        .and_then(|a| a.num("val"))
        .map(|v| v as f64 / 100_000.0);
    Some((hex, alpha))
}

/// A `<p:spPr>` fill → exactly the spec [`crate::pptx::fill_xml`] writes, so a
/// deck that is read, edited and saved keeps the fill it came in with.
fn fill_of(sp: &Node) -> Option<Value> {
    let mut m = Map::new();
    if sp.kid("a:noFill").is_some() {
        m.insert("type".into(), Value::from("none"));
    } else if let Some(g) = sp.kid("a:gradFill") {
        let mut nodes = Vec::new();
        g.find_all("a:gs", &mut nodes);
        let mut stops = Vec::new();
        for st in nodes {
            let mut sm = Map::new();
            if let Some(p) = st.num("pos") {
                sm.insert("position".into(), Value::from(p as f64 / 100_000.0));
            }
            if let Some((hex, alpha)) = srgb_of(st) {
                sm.insert("color".into(), Value::from(hex));
                put(
                    &mut sm,
                    "alpha",
                    alpha.map(Value::from).unwrap_or(Value::Null),
                );
            }
            stops.push(Value::Object(sm));
        }
        if stops.is_empty() {
            return None;
        }
        m.insert("type".into(), Value::from("gradient"));
        m.insert("stops".into(), Value::Array(stops));
        if let Some(a) = g.find("a:lin").and_then(|l| l.num("ang")) {
            m.insert("angle".into(), Value::from(a as f64 / 60_000.0));
        }
    } else {
        let (hex, alpha) = srgb_of(sp.kid("a:solidFill")?)?;
        m.insert("type".into(), Value::from("solid"));
        m.insert("color".into(), Value::from(hex));
        put(
            &mut m,
            "alpha",
            alpha.map(Value::from).unwrap_or(Value::Null),
        );
    }
    Some(Value::Object(m))
}

/// A `<p:spPr>` outline → the spec [`crate::pptx::line_xml`] writes.
fn line_of(sp: &Node) -> Option<Value> {
    let ln = sp.kid("a:ln")?;
    let mut m = Map::new();
    if let Some(w) = ln.num("w") {
        m.insert("width".into(), Value::from(w));
    }
    if ln.kid("a:noFill").is_some() {
        m.insert("type".into(), Value::from("none"));
    } else if let Some((hex, alpha)) = ln.kid("a:solidFill").and_then(srgb_of) {
        m.insert("color".into(), Value::from(hex));
        put(
            &mut m,
            "alpha",
            alpha.map(Value::from).unwrap_or(Value::Null),
        );
    }
    if let Some(d) = ln.kid("a:prstDash").and_then(|d| d.attr("val")) {
        m.insert("dash".into(), Value::from(d));
    }
    if m.is_empty() {
        None
    } else {
        Some(Value::Object(m))
    }
}

/// One member of a `<p:spTree>`.
fn shape_of(n: &Node, rels: &[(String, String)], z: &mut Zip, with_images: bool) -> Option<Value> {
    let mut m = Map::new();
    let nv = |tag: &str| -> Option<&Node> { n.kid(tag) };
    let cnv = n
        .kids
        .iter()
        .find(|k| {
            k.tag.ends_with("nvSpPr")
                || k.tag.ends_with("nvPicPr")
                || k.tag.ends_with("nvGrpSpPr")
                || k.tag.ends_with("nvCxnSpPr")
                || k.tag.ends_with("nvGraphicFramePr")
        })
        .and_then(|p| p.kid("p:cNvPr"));
    if let Some(c) = cnv {
        put(
            &mut m,
            "name",
            c.attr("name").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            &mut m,
            "shape_id",
            c.num("id").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            &mut m,
            "description",
            c.attr("descr").map(Value::from).unwrap_or(Value::Null),
        );
    }
    if let Some(ph) = n.find("p:ph") {
        let mut p = Map::new();
        put(
            &mut p,
            "type",
            ph.attr("type").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            &mut p,
            "idx",
            ph.num("idx").map(Value::from).unwrap_or(Value::Null),
        );
        m.insert("ph".into(), Value::Object(p));
    }
    geometry(n, &mut m);
    if let Some(sp) = n.kid("p:spPr") {
        put(&mut m, "fill", fill_of(sp).unwrap_or(Value::Null));
        put(&mut m, "line", line_of(sp).unwrap_or(Value::Null));
    }
    match n.tag.as_str() {
        "p:sp" => {
            let preset = n.find("a:prstGeom").and_then(|g| g.attr("prst"));
            let is_box = n
                .find("p:cNvSpPr")
                .map(|s| s.attr("txBox") == Some("1"))
                .unwrap_or(false);
            m.insert(
                "kind".into(),
                Value::from(if is_box { "textbox" } else { "auto" }),
            );
            put(
                &mut m,
                "preset",
                preset.map(Value::from).unwrap_or(Value::Null),
            );
            if let Some(body) = n.kid("p:txBody") {
                let tf = text_frame(body);
                m.insert("text".into(), Value::from(body_text(&tf)));
                m.insert("text_frame".into(), tf);
            }
        }
        "p:pic" => {
            m.insert("kind".into(), Value::from("picture"));
            let mut img = Map::new();
            if let Some(id) = n.find("a:blip").and_then(|b| b.attr("r:embed")) {
                if let Some((_, path)) = rels.iter().find(|(rid, _)| rid == id) {
                    img.insert("part".into(), Value::from(path.clone()));
                    if with_images {
                        if let Some(bytes) = z.bytes(path) {
                            use base64::Engine;
                            img.insert(
                                "data".into(),
                                Value::from(
                                    base64::engine::general_purpose::STANDARD.encode(bytes),
                                ),
                            );
                        }
                    }
                }
            }
            if let Some(sr) = n.find("a:srcRect") {
                let mut crop = Map::new();
                for (a, k) in [("l", "left"), ("t", "top"), ("r", "right"), ("b", "bottom")] {
                    if let Some(v) = sr.num(a) {
                        crop.insert(k.into(), Value::from(v as f64 / 100_000.0));
                    }
                }
                if !crop.is_empty() {
                    img.insert("crop".into(), Value::Object(crop));
                }
            }
            m.insert("image".into(), Value::Object(img));
        }
        "p:cxnSp" => {
            m.insert("kind".into(), Value::from("connector"));
            put(
                &mut m,
                "preset",
                n.find("a:prstGeom")
                    .and_then(|g| g.attr("prst"))
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
        }
        "p:graphicFrame" => {
            if let Some(tbl) = n.find("a:tbl") {
                m.insert("kind".into(), Value::from("table"));
                m.insert("table".into(), table_of(tbl));
            } else if let Some(ch) = n.find("c:chart") {
                m.insert("kind".into(), Value::from("chart"));
                let mut c = Map::new();
                if let Some(id) = ch.attr("r:id") {
                    if let Some((_, path)) = rels.iter().find(|(rid, _)| rid == id) {
                        if let Some(xml) = z.text(path) {
                            let root = parse(&xml);
                            if let Value::Object(spec) = chart_of(&root) {
                                c = spec;
                            }
                        }
                        c.insert("part".into(), Value::from(path.clone()));
                    }
                }
                m.insert("chart".into(), Value::Object(c));
            } else {
                m.insert("kind".into(), Value::from("graphic"));
            }
            if let Some(x) = n.kid("p:xfrm") {
                geometry_from(x, &mut m);
            }
        }
        "p:grpSp" => {
            m.insert("kind".into(), Value::from("group"));
            let kids: Vec<Value> = n
                .kids
                .iter()
                .filter_map(|k| shape_of(k, rels, z, with_images))
                .collect();
            m.insert("shapes".into(), Value::Array(kids));
        }
        _ => return None,
    }
    let _ = nv;
    Some(Value::Object(m))
}

/// Same as [`geometry`] but starting from an `<p:xfrm>`/`<a:xfrm>` element.
fn geometry_from(x: &Node, m: &mut Map<String, Value>) {
    if let Some(off) = x.kid("a:off") {
        put(
            m,
            "left",
            off.num("x").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            m,
            "top",
            off.num("y").map(Value::from).unwrap_or(Value::Null),
        );
    }
    if let Some(ext) = x.kid("a:ext") {
        put(
            m,
            "width",
            ext.num("cx").map(Value::from).unwrap_or(Value::Null),
        );
        put(
            m,
            "height",
            ext.num("cy").map(Value::from).unwrap_or(Value::Null),
        );
    }
}

/// `<c:barChart>` → `"column"` or `"bar"` (the bar direction decides), plus the
/// grouping suffix, and so on for every plot element we can write.
fn chart_kind(root: &Node) -> Option<String> {
    let plot = plot_node(root)?;
    let rest = plot.tag.strip_prefix("c:")?;
    let name = rest.strip_suffix("Chart")?;
    let base = if name == "bar" {
        match plot.kid("c:barDir").and_then(|d| d.attr("val")) {
            Some("bar") => "bar",
            _ => "column",
        }
    } else {
        name
    };
    let suffix = match plot.kid("c:grouping").and_then(|g| g.attr("val")) {
        Some("stacked") => "_stacked",
        Some("percentStacked") => "_percent_stacked",
        _ => "",
    };
    Some(format!("{base}{suffix}"))
}

/// The plot element a chart part is built around (`<c:barChart>` and friends).
fn plot_node(root: &Node) -> Option<&Node> {
    root.find("c:plotArea")?
        .kids
        .iter()
        .find(|k| k.tag.starts_with("c:") && k.tag.ends_with("Chart"))
}

/// The `<c:spPr>` solid fill colour of a chart element, as `RRGGBB`.
fn solid_of(n: &Node) -> Option<String> {
    Some(
        n.kid("c:spPr")?
            .kid("a:solidFill")?
            .kid("a:srgbClr")?
            .attr("val")?
            .to_string(),
    )
}

/// The rich text of a `<c:title>`/`<c:tx>`, however deeply it is nested.
fn title_text(n: &Node) -> Option<String> {
    let mut ts = Vec::new();
    n.find_all("a:t", &mut ts);
    let s: String = ts.iter().map(|t| t.all_text()).collect();
    if s.is_empty() { None } else { Some(s) }
}

/// `<c:legend>` → `{position, overlay}`.
fn legend_of(root: &Node) -> Option<Value> {
    let l = root.find("c:legend")?;
    let mut m = Map::new();
    let pos = l
        .kid("c:legendPos")
        .and_then(|p| p.attr("val"))
        .unwrap_or("r");
    m.insert("position".into(), Value::from(pos));
    if l.kid("c:overlay").and_then(|o| o.attr("val")) == Some("1") {
        m.insert("overlay".into(), Value::from(true));
    }
    Some(Value::Object(m))
}

/// `<c:catAx>`/`<c:valAx>` → the axis map the writer consumes.
fn axis_of(ax: &Node) -> Value {
    let mut m = Map::new();
    if ax.kid("c:delete").and_then(|d| d.attr("val")) == Some("1") {
        m.insert("visible".into(), Value::from(false));
    }
    for (tag, key) in [
        ("c:majorGridlines", "major_gridlines"),
        ("c:minorGridlines", "minor_gridlines"),
    ] {
        if ax.kid(tag).is_some() {
            m.insert(key.into(), Value::from(true));
        }
    }
    if let Some(t) = ax.kid("c:title").and_then(title_text) {
        m.insert("title".into(), Value::from(t));
    }
    for (tag, attr, key) in [
        ("c:numFmt", "formatCode", "number_format"),
        ("c:majorTickMark", "val", "major_tick_mark"),
        ("c:minorTickMark", "val", "minor_tick_mark"),
        ("c:tickLblPos", "val", "tick_label_position"),
    ] {
        if let Some(v) = ax.kid(tag).and_then(|n| n.attr(attr)) {
            m.insert(key.into(), Value::from(v));
        }
    }
    if let Some(sc) = ax.kid("c:scaling") {
        if sc.kid("c:orientation").and_then(|o| o.attr("val")) == Some("maxMin") {
            m.insert("reverse".into(), Value::from(true));
        }
        for (tag, key) in [
            ("c:max", "max"),
            ("c:min", "min"),
            ("c:logBase", "log_base"),
        ] {
            if let Some(v) = sc.kid(tag).and_then(|n| n.attr("val")) {
                if let Ok(f) = v.parse::<f64>() {
                    m.insert(key.into(), Value::from(f));
                }
            }
        }
    }
    for (tag, key) in [("c:majorUnit", "major_unit"), ("c:minorUnit", "minor_unit")] {
        if let Some(v) = ax.kid(tag).and_then(|n| n.attr("val")) {
            if let Ok(f) = v.parse::<f64>() {
                m.insert(key.into(), Value::from(f));
            }
        }
    }
    Value::Object(m)
}

/// The formatting a single `<c:ser>` carries beyond its numbers.
fn series_extras(s: &Node, m: &mut Map<String, Value>) {
    if let Some(c) = solid_of(s) {
        m.insert("fill".into(), Value::from(c));
    }
    if let Some(mk) = s.kid("c:marker") {
        let mut mm = Map::new();
        if let Some(sym) = mk.kid("c:symbol").and_then(|x| x.attr("val")) {
            mm.insert("symbol".into(), Value::from(sym));
        }
        if let Some(sz) = mk.kid("c:size").and_then(|x| x.num("val")) {
            mm.insert("size".into(), Value::from(sz));
        }
        if let Some(c) = solid_of(mk) {
            mm.insert("fill".into(), Value::from(c));
        }
        if !mm.is_empty() {
            m.insert("marker".into(), Value::Object(mm));
        }
    }
    if s.kid("c:smooth").and_then(|x| x.attr("val")) == Some("1") {
        m.insert("smooth".into(), Value::from(true));
    }
    if s.kid("c:invertIfNegative").and_then(|x| x.attr("val")) == Some("1") {
        m.insert("invert_if_negative".into(), Value::from(true));
    }
    if let Some(v) = s.kid("c:explosion").and_then(|x| x.num("val")) {
        m.insert("explosion".into(), Value::from(v));
    }
}

/// Series names, cached values and per-series formatting, out of the chart part.
fn chart_series(root: &Node) -> Vec<Value> {
    let Some(plot) = root.find("c:plotArea") else {
        return Vec::new();
    };
    let mut sers = Vec::new();
    plot.find_all("c:ser", &mut sers);
    sers.iter()
        .map(|s| {
            let mut m = Map::new();
            if let Some(tx) = s.kid("c:tx") {
                let name = tx.find("c:v").map(|v| v.all_text()).unwrap_or_default();
                put(&mut m, "name", Value::from(name));
            }
            for (tag, key) in [
                ("c:val", "values"),
                ("c:xVal", "x_values"),
                ("c:yVal", "y_values"),
                ("c:bubbleSize", "sizes"),
                ("c:cat", "categories"),
            ] {
                if let Some(v) = s.kid(tag) {
                    let mut pts = Vec::new();
                    v.find_all("c:pt", &mut pts);
                    let vals: Vec<Value> = pts
                        .iter()
                        .map(|p| {
                            let t = p.find("c:v").map(|v| v.all_text()).unwrap_or_default();
                            match t.parse::<f64>() {
                                Ok(n) if key != "categories" => Value::from(n),
                                _ => Value::from(t),
                            }
                        })
                        .collect();
                    if !vals.is_empty() {
                        m.insert(key.into(), Value::Array(vals));
                    }
                }
            }
            series_extras(s, &mut m);
            Value::Object(m)
        })
        .collect()
}

/// A whole chart part → the spec `pptx::build` consumes: type, categories,
/// series, title, legend, axes and the plot-level knobs.
fn chart_of(root: &Node) -> Value {
    let mut c = Map::new();
    put(
        &mut c,
        "type",
        chart_kind(root).map(Value::from).unwrap_or(Value::Null),
    );
    // Every series repeats the category labels; python-pptx (and the writer)
    // keep exactly one list per chart, so lift the first one out.
    let mut series = chart_series(root);
    let mut categories: Option<Value> = None;
    for s in series.iter_mut() {
        if let Some(obj) = s.as_object_mut() {
            if let Some(cats) = obj.remove("categories") {
                if categories.is_none() {
                    categories = Some(cats);
                }
            }
        }
    }
    if let Some(cats) = categories {
        c.insert("categories".into(), cats);
    }
    c.insert("series".into(), Value::Array(series));
    if let Some(t) = root.find("c:title").and_then(title_text) {
        c.insert("title".into(), Value::from(t));
    }
    if let Some(l) = legend_of(root) {
        c.insert("legend".into(), l);
    }
    if let Some(plot) = plot_node(root) {
        for (tag, key) in [
            ("c:gapWidth", "gap_width"),
            ("c:overlap", "overlap"),
            ("c:holeSize", "hole_size"),
            ("c:firstSliceAng", "first_slice_angle"),
            ("c:bubbleScale", "bubble_scale"),
        ] {
            if let Some(v) = plot.kid(tag).and_then(|n| n.num("val")) {
                c.insert(key.into(), Value::from(v));
            }
        }
        if plot.kid("c:varyColors").and_then(|n| n.attr("val")) == Some("1") {
            c.insert("vary_colors".into(), Value::from(true));
        }
        if plot.kid("c:marker").and_then(|n| n.attr("val")) == Some("1") {
            c.insert("markers".into(), Value::from(true));
        }
        for (tag, key) in [
            ("c:scatterStyle", "scatter_style"),
            ("c:radarStyle", "radar_style"),
        ] {
            if let Some(v) = plot.kid(tag).and_then(|n| n.attr("val")) {
                c.insert(key.into(), Value::from(v));
            }
        }
    }
    if let Some(pa) = root.find("c:plotArea") {
        let cat = pa.kids.iter().find(|k| k.tag == "c:catAx");
        let vals: Vec<&Node> = pa.kids.iter().filter(|k| k.tag == "c:valAx").collect();
        if let Some(cx) = cat {
            c.insert("category_axis".into(), axis_of(cx));
            if let Some(v) = vals.first() {
                c.insert("value_axis".into(), axis_of(v));
            }
        } else {
            if let Some(v) = vals.first() {
                c.insert("x_axis".into(), axis_of(v));
            }
            if let Some(v) = vals.get(1) {
                c.insert("y_axis".into(), axis_of(v));
            }
        }
    }
    if let Some(fmt) = root
        .find("c:ser")
        .and_then(|s| s.kid("c:val"))
        .and_then(|v| v.kid("c:numRef"))
        .and_then(|n| n.kid("c:formatCode"))
        .map(|n| n.all_text())
        .filter(|s| !s.is_empty() && s.as_str() != "General")
    {
        c.insert("number_format".into(), Value::from(fmt));
    }
    if let Some(v) = root.find("c:dispBlanksAs").and_then(|n| n.attr("val")) {
        c.insert("display_blanks".into(), Value::from(v));
    }
    Value::Object(c)
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Read a presentation package into
/// `{kind,width,height,properties,slides:[{name,index,layout,shapes,notes,paragraphs}]}`.
///
/// `opts` honours `with_images` (base64 every embedded picture, off by default).
pub fn read(data: &[u8], opts: &Value) -> R<Value> {
    let with_images = opts
        .get("with_images")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut z = Zip::open(data)?;
    let mut names: Vec<String> = z
        .names()
        .into_iter()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(0)
    });

    let mut out = Map::new();
    out.insert("kind".into(), Value::from("presentation"));
    if let Some(pres) = z.text("ppt/presentation.xml") {
        let root = parse(&pres);
        if let Some(sz) = root.find("p:sldSz") {
            put(
                &mut out,
                "width",
                sz.num("cx").map(Value::from).unwrap_or(Value::Null),
            );
            put(
                &mut out,
                "height",
                sz.num("cy").map(Value::from).unwrap_or(Value::Null),
            );
        }
    }
    if let Some(core) = z.text("docProps/core.xml") {
        let root = parse(&core);
        let mut props = Map::new();
        for (tag, key) in [
            ("dc:title", "title"),
            ("dc:creator", "author"),
            ("dc:subject", "subject"),
            ("dc:description", "comments"),
            ("cp:keywords", "keywords"),
            ("cp:category", "category"),
            ("cp:lastModifiedBy", "last_modified_by"),
        ] {
            if let Some(n) = root.find(tag) {
                let t = n.all_text();
                if !t.is_empty() {
                    props.insert(key.into(), Value::from(t));
                }
            }
        }
        if !props.is_empty() {
            out.insert("properties".into(), Value::Object(props));
        }
    }

    let mut slides = Vec::new();
    for (idx, name) in names.iter().enumerate() {
        let Some(xml) = z.text(name) else { continue };
        let rels = rels_of(&mut z, name);
        let root = parse(&xml);
        let mut m = Map::new();
        m.insert("name".into(), Value::from(name.clone()));
        m.insert("index".into(), Value::from(idx as i64));
        if let Some((_, layout)) = rels.iter().find(|(_, p)| p.contains("slideLayout")) {
            m.insert("layout_part".into(), Value::from(layout.clone()));
            if let Some(lx) = z.text(layout) {
                let lroot = parse(&lx);
                if let Some(n) = lroot.find("p:cSld").and_then(|c| c.attr("name")) {
                    m.insert("layout".into(), Value::from(n));
                }
            }
        }
        let shapes: Vec<Value> = root
            .find("p:spTree")
            .map(|tree| {
                tree.kids
                    .iter()
                    .filter_map(|k| shape_of(k, &rels, &mut z, with_images))
                    .collect()
            })
            .unwrap_or_default();
        // Legacy flat view: every non-empty paragraph, in reading order.
        let mut paragraphs = Vec::new();
        collect_text(&shapes, &mut paragraphs);
        m.insert("shapes".into(), Value::Array(shapes));
        m.insert("paragraphs".into(), Value::Array(paragraphs));
        if let Some((_, notes)) = rels.iter().find(|(_, p)| p.contains("notesSlide")) {
            if let Some(nx) = z.text(notes) {
                let nroot = parse(&nx);
                let mut ps = Vec::new();
                nroot.find_all("a:p", &mut ps);
                let text = ps
                    .iter()
                    .map(|p| p.all_text())
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    m.insert("notes".into(), Value::from(text));
                }
            }
        }
        slides.push(Value::Object(m));
    }
    out.insert("slides".into(), Value::Array(slides));
    Ok(Value::Object(out))
}

/// Flatten every paragraph of every shape (recursing into groups and tables).
fn collect_text(shapes: &[Value], out: &mut Vec<Value>) {
    for sh in shapes {
        if let Some(tf) = sh.get("text_frame") {
            if let Some(ps) = tf.get("paragraphs").and_then(|p| p.as_array()) {
                for p in ps {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        if !t.is_empty() {
                            out.push(Value::from(t));
                        }
                    }
                }
            }
        }
        if let Some(rows) = sh
            .get("table")
            .and_then(|t| t.get("rows"))
            .and_then(|r| r.as_array())
        {
            for row in rows {
                for c in row
                    .get("cells")
                    .and_then(|c| c.as_array())
                    .unwrap_or(&vec![])
                {
                    if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                        if !t.is_empty() {
                            out.push(Value::from(t));
                        }
                    }
                }
            }
        }
        if let Some(kids) = sh.get("shapes").and_then(|s| s.as_array()) {
            collect_text(kids, out);
        }
    }
}
