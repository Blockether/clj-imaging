//! Office Open XML: the shared package plumbing behind `xlsx`, `docx` and
//! `pptx`, plus a reader that turns any of them (and `.xls`/`.ods`) back into
//! JSON.
//!
//! Every OOXML file is a ZIP of XML parts wired together by relationship files.
//! [`Pack`] is that ZIP — deterministic (no timestamps, no host paths), pure
//! Rust (`zip` with the flate2/zlib-rs backend), so a document written here is
//! byte-identical from run to run and the cdylib still links against nothing.
//!
//! Nothing in this module is Java, and nothing in it shells out.

use std::io::{Cursor, Write};

use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::util::{R, err};

pub const DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n";

/// EMU per point (72 dpi) — the unit every DrawingML offset and extent is
/// measured in.
pub const EMU_PER_PT: f64 = 12700.0;

// ---------------------------------------------------------------------------
// package
// ---------------------------------------------------------------------------

/// An OOXML package under construction.
pub struct Pack {
    zip: ZipWriter<Cursor<Vec<u8>>>,
    opt: SimpleFileOptions,
}

impl Pack {
    pub fn new() -> Pack {
        Pack {
            zip: ZipWriter::new(Cursor::new(Vec::new())),
            // Deflated + a fixed 1980 timestamp: the same spec always produces
            // the same bytes, which makes documents diffable and cacheable.
            opt: SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        }
    }

    /// Add a binary part (media, mostly).
    pub fn file(&mut self, name: &str, bytes: &[u8]) -> R<()> {
        let opt = self.opt;
        self.zip
            .start_file(name, opt)
            .map_err(|e| format!("zip entry {name}: {e}"))?;
        self.zip
            .write_all(bytes)
            .map_err(|e| format!("zip write {name}: {e}"))?;
        Ok(())
    }

    /// Add an XML part, prefixing the declaration every OOXML consumer expects.
    pub fn xml(&mut self, name: &str, body: &str) -> R<()> {
        let mut s = String::with_capacity(DECL.len() + body.len());
        s.push_str(DECL);
        s.push_str(body);
        self.file(name, s.as_bytes())
    }

    pub fn finish(self) -> R<Vec<u8>> {
        let cur = self
            .zip
            .finish()
            .map_err(|e| format!("finishing package: {e}"))?;
        Ok(cur.into_inner())
    }
}

impl Default for Pack {
    fn default() -> Self {
        Pack::new()
    }
}

// ---------------------------------------------------------------------------
// XML + value helpers
// ---------------------------------------------------------------------------

/// Escape text for an XML text node or attribute value.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters outright; dropping them
            // is the difference between a document that opens and one that does
            // not.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// `RRGGBB` (uppercase, no `#`) — the only colour spelling OOXML accepts.
pub fn hex6(v: &Value) -> Option<String> {
    let c = crate::util::color_from_json(v).ok()?;
    Some(format!("{:02X}{:02X}{:02X}", c.0, c.1, c.2))
}

pub fn shex(v: &Value, k: &str) -> Option<String> {
    v.get(k).filter(|x| !x.is_null()).and_then(hex6)
}

/// A JSON string field, empty treated as absent.
pub fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty())
}

pub fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

pub fn i(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_f64()).map(|n| n.round() as i64)
}

pub fn b(v: &Value, k: &str) -> Option<bool> {
    v.get(k).and_then(|x| x.as_bool())
}

pub fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(|x| x.as_array()).map_or(&[], |a| &a[..])
}

/// Decode a base64 payload carried in `k` (bytes cross the boundary that way).
pub fn b64(v: &Value, k: &str) -> R<Vec<u8>> {
    use base64::Engine;
    let Some(t) = s(v, k) else {
        return err(format!("`{k}` is required and must be base64 bytes"));
    };
    base64::engine::general_purpose::STANDARD
        .decode(t.trim())
        .map_err(|e| format!("`{k}` is not valid base64: {e}"))
}

/// Sniff an image's extension + content type + pixel size, so a picture part
/// can be declared correctly without the caller telling us anything.
pub fn image_info(data: &[u8]) -> R<(&'static str, &'static str, u32, u32)> {
    let v = crate::imgops::probe(data)?;
    let fmt = v.get("format").and_then(|x| x.as_str()).unwrap_or("");
    let w = v.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let h = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let (ext, ct) = match fmt {
        "png" => ("png", "image/png"),
        "jpeg" => ("jpeg", "image/jpeg"),
        "gif" => ("gif", "image/gif"),
        "bmp" => ("bmp", "image/bmp"),
        "tiff" => ("tiff", "image/tiff"),
        "webp" => ("png", "image/png"), // Office cannot embed WebP: re-encoded below
        "heic" => ("png", "image/png"), // nor HEIC: re-encoded below
        other => return err(format!("unsupported image format for Office: {other}")),
    };
    Ok((ext, ct, w, h))
}

/// Office accepts PNG/JPEG/GIF/BMP/TIFF only; anything else is re-encoded to
/// PNG so callers can pass whatever `imaging` can decode.
pub fn office_image(data: &[u8]) -> R<(Vec<u8>, &'static str, &'static str, u32, u32)> {
    let (ext, ct, w, h) = image_info(data)?;
    let probe = crate::imgops::probe(data)?;
    let source = probe.get("format").and_then(Value::as_str).unwrap_or("");
    if ct == "image/png" && source != "png" {
        let img = crate::imgops::decode(data)?;
        let png = crate::imgops::encode(&img, "png", 90)?;
        return Ok((png, ext, ct, w, h));
    }
    Ok((data.to_vec(), ext, ct, w, h))
}

// ---------------------------------------------------------------------------
// docProps — shared by every document kind
// ---------------------------------------------------------------------------

pub fn core_xml(spec: &Value) -> String {
    let p = spec.get("properties").cloned().unwrap_or(Value::Null);
    let g = |k: &str| s(&p, k).map(esc).unwrap_or_default();
    format!(
        r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:dcmitype="http://purl.org/dc/dcmitype/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title>{}</dc:title><dc:subject>{}</dc:subject><dc:creator>{}</dc:creator><cp:keywords>{}</cp:keywords><dc:description>{}</dc:description><cp:lastModifiedBy>{}</cp:lastModifiedBy><cp:revision>1</cp:revision></cp:coreProperties>"#,
        g("title"),
        g("subject"),
        g("author"),
        g("keywords"),
        g("comments"),
        g("author"),
    )
}

pub fn app_xml(application: &str, extra: &str) -> String {
    format!(
        r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><Application>{}</Application>{}</Properties>"#,
        esc(application),
        extra
    )
}

pub const ROOT_RELS_TAIL: &str = r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/></Relationships>"#;

pub const RELS_OPEN: &str =
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#;

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Read a workbook (`.xlsx`/`.xlsm`/`.xls`/`.xlsb`/`.ods`) or the text of a
/// `.docx`/`.pptx` back into JSON. Options: `sheet` (name or index) and
/// `max_rows`; `values` are strings, numbers, booleans or null.
pub fn read(data: &[u8], opts: &Value) -> R<Value> {
    match kind_of(data) {
        Kind::Word => read_word(data),
        Kind::Slides => crate::slides_read::read(data, opts),
        Kind::Sheet => read_sheets(data, opts),
    }
}

enum Kind {
    Sheet,
    Word,
    Slides,
}

fn kind_of(data: &[u8]) -> Kind {
    // Peek at the ZIP's central directory names; `.xls` is not a ZIP at all.
    let Ok(mut z) = zip::ZipArchive::new(Cursor::new(data.to_vec())) else {
        return Kind::Sheet;
    };
    let names: Vec<String> = (0..z.len())
        .filter_map(|i| z.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    if names.iter().any(|n| n == "word/document.xml") {
        Kind::Word
    } else if names
        .iter()
        .any(|n| n == "ppt/presentation.xml" || n.starts_with("ppt/slides/slide"))
    {
        // A deck is identified by its presentation part, NOT by its slides: a
        // freshly created presentation legally has none, and reading it as a
        // workbook only produced calamine's "Cannot detect file format".
        Kind::Slides
    } else {
        Kind::Sheet
    }
}

fn read_sheets(data: &[u8], opts: &Value) -> R<Value> {
    use calamine::{Data, Reader};
    let cur = Cursor::new(data.to_vec());
    let mut wb = calamine::open_workbook_auto_from_rs(cur)
        .map_err(|e| format!("not a readable workbook: {e}"))?;
    let want = opts.get("sheet");
    let max_rows = i(opts, "max_rows").unwrap_or(i64::MAX).max(0) as usize;
    let names = wb.sheet_names().to_vec();
    let chosen: Vec<String> = match want {
        Some(Value::String(n)) => vec![n.clone()],
        Some(Value::Number(n)) => names
            .get(n.as_u64().unwrap_or(0) as usize)
            .cloned()
            .into_iter()
            .collect(),
        _ => names.clone(),
    };
    let mut sheets = Vec::new();
    for name in chosen {
        let range = wb
            .worksheet_range(&name)
            .map_err(|e| format!("sheet {name}: {e}"))?;
        let mut rows = Vec::new();
        for row in range.rows().take(max_rows) {
            let cells: Vec<Value> = row
                .iter()
                .map(|c| match c {
                    Data::Empty => Value::Null,
                    Data::String(s) => Value::from(s.clone()),
                    Data::Float(f) => Value::from(*f),
                    Data::Int(n) => Value::from(*n),
                    Data::Bool(b) => Value::from(*b),
                    Data::Error(e) => Value::from(format!("{e:?}")),
                    Data::DateTime(d) => Value::from(d.as_f64()),
                    Data::DateTimeIso(s) => Value::from(s.clone()),
                    Data::DurationIso(s) => Value::from(s.clone()),
                })
                .collect();
            rows.push(Value::Array(cells));
        }
        sheets.push(serde_json::json!({
            "name": name,
            "rows": rows,
            "height": range.height(),
            "width": range.width(),
        }));
    }
    Ok(serde_json::json!({"kind": "workbook", "sheets": sheets, "sheet_names": names}))
}

/// Every `<w:t>` in reading order, grouped by paragraph.
fn read_word(data: &[u8]) -> R<Value> {
    let xml = part(data, "word/document.xml")?;
    Ok(serde_json::json!({"kind": "document", "paragraphs": paragraph_texts(&xml, "w:p", "w:t")}))
}

fn part(data: &[u8], name: &str) -> R<String> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(Cursor::new(data.to_vec()))
        .map_err(|e| format!("not a package: {e}"))?;
    let mut f = z
        .by_name(name)
        .map_err(|_| format!("package has no {name}"))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)
        .map_err(|e| format!("reading {name}: {e}"))?;
    Ok(buf)
}

/// Text runs collected per paragraph element — enough to diff, search or assert
/// on a generated document without a full DOM.
fn paragraph_texts(xml: &str, para: &str, text: &str) -> Vec<Value> {
    use quick_xml::events::Event;
    let mut rd = quick_xml::Reader::from_str(xml);
    let mut out: Vec<Value> = Vec::new();
    let mut cur = String::new();
    let mut in_text = false;
    let mut depth_open = false;
    loop {
        match rd.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == para {
                    cur.clear();
                    depth_open = true;
                } else if name == text {
                    in_text = true;
                }
            }
            Ok(Event::Text(t)) if in_text => {
                cur.push_str(&t.decode().unwrap_or_default());
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == text {
                    in_text = false;
                } else if name == para && depth_open {
                    out.push(Value::from(cur.clone()));
                    depth_open = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}
