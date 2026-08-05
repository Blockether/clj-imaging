//! Any document -> GitHub-Flavored Markdown, over Firecrawl's `anydoc`.
//!
//! `office.rs` reads the three formats this library WRITES, and reads them as
//! data (`{sheets rows}`). This module is the other job: turn a document a
//! human sent — `.doc`, `.docx`, `.odt`, `.rtf`, `.pdf`, `.epub`, `.ppt`,
//! `.pptx`, `.odp`, `.xls*`, `.ods`, `.csv` — into Markdown an LLM can read,
//! with its tables, lists, headings and links intact.
//!
//! `anydoc` parses every one of those itself: no headless office, no Java, no
//! subprocess and no system library, so the cdylib still links against
//! nothing and still fits inside a GraalVM native image.

use anydoc::Format;
use base64::Engine as _;
use serde_json::{Value, json};

use crate::util::{R, err, jb, js};

/// The wire name of a format — the lowercase variant name, which is also the
/// extension for every variant except `excel` (`.xlsx`/`.xlsm`/`.xlsb`/`.xls`).
pub fn format_name(f: Format) -> &'static str {
    match f {
        Format::Doc => "doc",
        Format::Docx => "docx",
        Format::Odt => "odt",
        Format::Pdf => "pdf",
        Format::Ppt => "ppt",
        Format::Pptx => "pptx",
        Format::Rtf => "rtf",
        Format::Epub => "epub",
        Format::Excel => "excel",
        Format::Ods => "ods",
        Format::Odp => "odp",
        Format::Csv => "csv",
    }
}

/// Every format name this ABI accepts and reports, in `Format`'s own order.
pub const FORMATS: [&str; 12] = [
    "doc", "docx", "odt", "pdf", "ppt", "pptx", "rtf", "epub", "excel", "ods", "odp", "csv",
];

/// Resolve a caller-supplied name: a wire name (`excel`), a bare extension
/// (`xlsm`, `.docx`) or a whole file name (`report.final.docx`) all work, so
/// callers never have to strip a path before asking.
pub fn format_from_name(name: &str) -> Option<Format> {
    let name = name.trim().trim_start_matches('.');
    let ext = name.rsplit(['.', '/', '\\']).next().unwrap_or(name);
    if ext.eq_ignore_ascii_case("excel") {
        return Some(Format::Excel);
    }
    Format::from_extension(ext)
}

/// The format for this call: an explicit `format`, else the content's own
/// signature, else the extension of `name`. `Format::from_bytes` is asked
/// FIRST because a signature cannot lie and an extension routinely does; CSV
/// carries no signature at all, so a name is its only route.
fn resolve(data: &[u8], opts: &Value) -> R<(Format, &'static str)> {
    if let Some(want) = js(opts, "format") {
        return match format_from_name(want) {
            Some(f) => Ok((f, "format")),
            None => err(format!(
                "unknown document format: {want} (expected one of {})",
                FORMATS.join(", ")
            )),
        };
    }
    if let Some(f) = Format::from_bytes(data) {
        return Ok((f, "content"));
    }
    if let Some(f) = js(opts, "name").and_then(format_from_name) {
        return Ok((f, "name"));
    }
    err("unrecognized document: no known signature and no usable `name`; pass `format`")
}

/// Identify a document without converting it: `{format source name}` with
/// `format` null when nothing recognised it. `source` says WHICH evidence
/// won — `content` (signature), `name` (extension) or `format` (told).
pub fn detect(data: &[u8], opts: &Value) -> R<Value> {
    let (format, source) = match resolve(data, opts) {
        Ok(v) => v,
        Err(_) => {
            return Ok(json!({
                "format": Value::Null,
                "source": Value::Null,
                "formats": FORMATS,
            }));
        }
    };
    Ok(json!({
        "format": format_name(format),
        "source": source,
        "formats": FORMATS,
    }))
}

/// Convert a document to Markdown: `{format source markdown chars assets}`.
///
/// Options: `format` (force the parser), `name` (file name, the fallback for
/// signature-less CSV), `assets` (also return the embedded binaries, base64),
/// `max_assets`.
pub fn markdown(data: &[u8], opts: &Value) -> R<Value> {
    let (format, source) = resolve(data, opts)?;
    let md = anydoc::to_markdown_bytes(data, format).map_err(|e| format!("{e}"))?;
    let mut out = json!({
        "format": format_name(format),
        "source": source,
        "chars": md.chars().count(),
        "markdown": md,
    });
    if jb(opts, "assets", false) {
        out["assets"] = Value::Array(assets(data, format, opts)?);
    }
    Ok(out)
}

/// The document's embedded binaries, each `{id media-type origin-part size
/// bytes}` with `bytes` base64. A PDF converts straight to Markdown and has
/// no document model, so it never reports assets.
fn assets(data: &[u8], format: Format, opts: &Value) -> R<Vec<Value>> {
    if format == Format::Pdf {
        return Ok(Vec::new());
    }
    let doc = anydoc::to_document(data, format).map_err(|e| format!("{e}"))?;
    let max = opts
        .get("max_assets")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX)
        .min(usize::MAX as u64) as usize;
    Ok(doc
        .assets
        .iter()
        .take(max)
        .map(|a| {
            json!({
                "id": a.id.0,
                "media-type": a.media_type,
                "origin-part": a.origin_part,
                "size": a.bytes.len(),
                "bytes": base64::engine::general_purpose::STANDARD.encode(&a.bytes),
            })
        })
        .collect())
}
