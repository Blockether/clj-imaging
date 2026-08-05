//! The document's STRUCTURE, not just its Markdown.
//!
//! `doc.rs` renders a document to GitHub-Flavored Markdown, which is what a
//! human reads. This module answers the other question — *where in the
//! document is this?* — by exporting the block list `anydoc` already parsed:
//! headings with their outline level and the breadcrumb above them, list items
//! with their resolved marker, table rows with every cell's own column, code
//! blocks, block quotes and footnote bodies.
//!
//! Two things follow, and both are the point:
//!
//! * Every block carries `text`, the PLAIN text of its inlines
//!   (`anydoc::model::inlines_to_plain_text`) — no `**`, no `[..](..)`, no
//!   table pipes. Searching that instead of the Markdown is what makes
//!   `March 2024` match `Revenue in **March** 2024`.
//! * Every block carries `start`/`end`/`line`: character offsets into ONE
//!   plain-text rendering of the document (`text` on the result), built here by
//!   joining the blocks in reading order. A citation can therefore name a
//!   character span, a line, a section path, a table cell — and, for a PDF, the
//!   page.
//!
//! PDFs have no document model at all (`anydoc::to_document` refuses them), so
//! their structure comes from `pdf-inspector`'s own per-page Markdown, split
//! back into blocks here. That is where `page` comes from, and it is the only
//! reason this module talks to `pdf-inspector` directly.

use anydoc::model::{Block, CellSlot, Document, NoteKind, Table, inlines_to_plain_text};
use serde_json::{Map, Value, json};

use crate::util::{R, err};

/// Blank line between two blocks of the plain-text rendering. Keeping ONE
/// separator everywhere is what lets `start`/`end` be checked from Python:
/// `text[start:end]` is exactly the block's own `text`.
const GAP: &str = "\n\n";

/// Accumulates the plain-text rendering and the block list side by side, so a
/// block's offsets are stamped by the same code that appends its text.
#[derive(Default)]
struct Walk {
    /// Plain-text rendering of everything pushed so far.
    text: String,
    /// `text` length in CHARACTERS — Python indexes by code point, not byte.
    chars: usize,
    /// 1-based line number the next pushed block starts on.
    line: usize,
    /// Emitted blocks.
    blocks: Vec<Value>,
    /// Heading breadcrumb: (level, text) of every heading still open.
    stack: Vec<(u8, String)>,
    /// Marker of the list item whose first block has not been pushed yet.
    marker: Option<String>,
    /// Checkbox state of that same pending list item.
    checked: Option<bool>,
    /// Nesting depth inside `Block::List`.
    list_depth: u32,
    /// Nesting depth inside `Block::BlockQuote`.
    quote_depth: u32,
    /// Id and kind of the note whose body is being walked.
    note: Option<(String, &'static str)>,
}

impl Walk {
    fn new() -> Walk {
        Walk {
            line: 1,
            ..Walk::default()
        }
    }

    /// Append `body` to the plain-text rendering and return the block map it
    /// occupies, with `kind` and every positional field already stamped.
    /// Empty content is refused (returns `None`) so blank paragraphs, spacer
    /// rows and rules never become citable positions.
    fn push(&mut self, kind: &str, body: &str) -> Option<Map<String, Value>> {
        let body = body.trim_matches(|c: char| c == '\n' || c == '\r');
        if body.trim().is_empty() {
            return None;
        }
        if !self.text.is_empty() {
            self.text.push_str(GAP);
            self.chars += GAP.chars().count();
            self.line += GAP.matches('\n').count();
        }
        let start = self.chars;
        let line = self.line;
        self.text.push_str(body);
        self.chars += body.chars().count();
        self.line += body.matches('\n').count();

        let mut m = Map::new();
        m.insert("index".into(), json!(self.blocks.len()));
        m.insert("kind".into(), json!(kind));
        m.insert("text".into(), json!(body));
        m.insert("start".into(), json!(start));
        m.insert("end".into(), json!(self.chars));
        m.insert("line".into(), json!(line));
        m.insert("path".into(), json!(self.path()));
        if self.quote_depth > 0 {
            m.insert("quote-depth".into(), json!(self.quote_depth));
        }
        if self.list_depth > 0 {
            m.insert("list-depth".into(), json!(self.list_depth));
        }
        if let Some((id, kind)) = &self.note {
            m.insert("note".into(), json!(id));
            m.insert("note-kind".into(), json!(kind));
        }
        if let Some(marker) = self.marker.take() {
            m.insert("marker".into(), json!(marker));
        }
        if let Some(checked) = self.checked.take() {
            m.insert("checked".into(), json!(checked));
        }
        Some(m)
    }

    fn emit(&mut self, m: Map<String, Value>) {
        self.blocks.push(Value::Object(m));
    }

    /// The headings still open above the current position, outermost first.
    fn path(&self) -> Vec<String> {
        self.stack.iter().map(|(_, t)| t.clone()).collect()
    }

    fn walk(&mut self, blocks: &[Block]) {
        for block in blocks {
            self.walk_one(block);
        }
    }

    fn walk_one(&mut self, block: &Block) {
        match block {
            Block::Heading {
                level,
                anchor,
                content,
            } => {
                let body = inlines_to_plain_text(content);
                // The breadcrumb of a heading is its ANCESTORS: close every
                // sibling or deeper heading before stamping it.
                while self.stack.last().is_some_and(|(l, _)| *l >= *level) {
                    self.stack.pop();
                }
                if let Some(mut m) = self.push("heading", &body) {
                    m.insert("level".into(), json!(level));
                    if let Some(a) = anchor {
                        m.insert("anchor".into(), json!(a));
                    }
                    self.emit(m);
                }
                self.stack.push((*level, body.trim().to_string()));
            }
            Block::Paragraph(inlines) => {
                let body = inlines_to_plain_text(inlines);
                let kind = if self.marker.is_some() {
                    "list-item"
                } else {
                    "paragraph"
                };
                if let Some(m) = self.push(kind, &body) {
                    self.emit(m);
                }
            }
            Block::CodeBlock { lang, text } => {
                if let Some(mut m) = self.push("code", text) {
                    if let Some(lang) = lang {
                        m.insert("lang".into(), json!(lang));
                    }
                    self.emit(m);
                }
            }
            Block::BlockQuote(inner) => {
                self.quote_depth += 1;
                self.walk(inner);
                self.quote_depth -= 1;
            }
            Block::List(list) => {
                self.list_depth += 1;
                for (i, item) in list.items.iter().enumerate() {
                    let ordinal = list.start.saturating_add(i as u64);
                    self.marker = Some(
                        item.marker_label
                            .clone()
                            .unwrap_or_else(|| list.marker.label(ordinal)),
                    );
                    self.checked = item.checked;
                    self.walk(&item.blocks);
                    // An item whose content was blank never consumed them.
                    self.marker = None;
                    self.checked = None;
                }
                self.list_depth -= 1;
            }
            Block::Table(table) => self.walk_table(table),
            Block::Rule => {}
        }
    }

    /// One block per ROW, because a row is what a fact lives on: `March | 12.4`
    /// is a phrase a human searches for, and splitting it into two cell blocks
    /// would make it unfindable. The cells stay addressable — each carries its
    /// column index, its header name and its own span inside the row's text.
    fn walk_table(&mut self, table: &Table) {
        let headers: Vec<String> = if table.header_rows > 0 {
            table
                .grid
                .first()
                .map(|row| row.iter().map(slot_text).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for (r, row) in table.grid.iter().enumerate() {
            let cells: Vec<String> = row.iter().map(slot_text).collect();
            let body = cells.join(" | ");
            let is_header = r < table.header_rows;
            let Some(mut m) = self.push("table-row", &body) else {
                continue;
            };
            let row_start = m.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mut at = row_start;
            let mut wire = Vec::with_capacity(cells.len());
            for (c, cell) in cells.iter().enumerate() {
                let len = cell.chars().count();
                let mut cm = Map::new();
                cm.insert("column".into(), json!(c));
                if let Some(name) = headers.get(c).filter(|n| !n.trim().is_empty()) {
                    cm.insert("name".into(), json!(name));
                }
                cm.insert("text".into(), json!(cell));
                cm.insert("start".into(), json!(at));
                cm.insert("end".into(), json!(at + len));
                wire.push(Value::Object(cm));
                at += len + 3; // the " | " that joins it to the next cell
            }
            m.insert("row".into(), json!(r));
            m.insert("is-header".into(), json!(is_header));
            m.insert("cells".into(), json!(wire));
            self.emit(m);
        }
    }
}

/// A cell's plain text, or the empty string for a position a span swallowed.
fn slot_text(slot: &CellSlot) -> String {
    match slot {
        CellSlot::Origin(cell) => blocks_to_plain(&cell.blocks).trim().to_string(),
        CellSlot::Covered { .. } => String::new(),
    }
}

/// Plain text of a run of blocks, for the places a block nests inside another
/// (table cells) and no separate citation is wanted.
fn blocks_to_plain(blocks: &[Block]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let part = match block {
            Block::Heading { content, .. } => inlines_to_plain_text(content),
            Block::Paragraph(inlines) => inlines_to_plain_text(inlines),
            Block::CodeBlock { text, .. } => text.clone(),
            Block::BlockQuote(inner) => blocks_to_plain(inner),
            Block::List(list) => list
                .items
                .iter()
                .map(|i| blocks_to_plain(&i.blocks))
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Table(table) => table
                .grid
                .iter()
                .map(|row| row.iter().map(slot_text).collect::<Vec<_>>().join(" | "))
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Rule => String::new(),
        };
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n")
}

/// The document's plain text and its blocks, in reading order, notes last.
pub fn document_blocks(doc: &Document) -> (String, Vec<Value>) {
    let mut w = Walk::new();
    w.walk(&doc.blocks);
    for note in &doc.notes {
        w.stack.clear();
        w.note = Some((
            note.id.clone(),
            match note.kind {
                NoteKind::Footnote => "footnote",
                NoteKind::Endnote => "endnote",
            },
        ));
        w.walk(&note.blocks);
    }
    w.note = None;
    (w.text, w.blocks)
}

// ---------------------------------------------------------------------------
// PDF — the one format with no document model, and the only one with pages
// ---------------------------------------------------------------------------

/// Markdown, plain text, blocks and page count of a PDF.
///
/// `pdf-inspector` (already in the tree: it is how `anydoc` reads PDFs at all)
/// returns Markdown PER PAGE, which is the whole reason to call it directly —
/// `anydoc::to_markdown_bytes` concatenates the pages and the page number is
/// gone. Splitting each page's Markdown back into blocks here is what puts
/// `page` on a citation.
pub fn pdf_blocks(data: &[u8]) -> R<(String, String, Vec<Value>, usize)> {
    let pages = match pdf_inspector::extract_pages_markdown_mem(data, None) {
        Ok(p) => p,
        Err(e) => return err(format!("{e}")),
    };
    let mut w = Walk::new();
    let mut md_parts: Vec<String> = Vec::with_capacity(pages.pages.len());
    let count = pages.pages.len();
    for page in &pages.pages {
        // pdf-inspector numbers pages from 0; a citation says "page 1".
        let number = page.page.saturating_add(1);
        md_parts.push(page.markdown.clone());
        push_markdown_blocks(&mut w, &page.markdown, Some(number));
    }
    Ok((md_parts.join("\n\n"), w.text, w.blocks, count))
}

/// Split one chunk of Markdown back into blocks. Only used for PDFs, where the
/// Markdown IS the source of truth because no parsed model exists.
fn push_markdown_blocks(w: &mut Walk, markdown: &str, page: Option<u32>) {
    let mut para: Vec<String> = Vec::new();
    let mut fence: Option<(String, Vec<String>)> = None;
    let mut table: Vec<Vec<String>> = Vec::new();

    macro_rules! stamp {
        ($m:expr) => {{
            let mut m = $m;
            if let Some(p) = page {
                m.insert("page".into(), json!(p));
            }
            w.emit(m);
        }};
    }

    let lines = markdown.lines().peekable();
    for raw in lines {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        if let Some((lang, body)) = &mut fence {
            if trimmed.starts_with("```") {
                let text = body.join("\n");
                let lang = lang.clone();
                fence = None;
                if let Some(mut m) = w.push("code", &text) {
                    if !lang.is_empty() {
                        m.insert("lang".into(), json!(lang));
                    }
                    stamp!(m);
                }
            } else {
                body.push(line.to_string());
            }
            continue;
        }
        if trimmed.starts_with("```") {
            flush_paragraph(w, &mut para, page);
            flush_table(w, &mut table, page);
            fence = Some((
                trimmed.trim_start_matches('`').trim().to_string(),
                Vec::new(),
            ));
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(w, &mut para, page);
            flush_table(w, &mut table, page);
            continue;
        }
        if trimmed.starts_with('|') {
            flush_paragraph(w, &mut para, page);
            let cells = split_table_row(trimmed);
            // `|---|---|` is the header rule, not a row of content.
            if !cells.iter().all(|c| is_rule_cell(c)) {
                table.push(cells);
            }
            continue;
        }
        flush_table(w, &mut table, page);
        if let Some(level) = heading_level(trimmed) {
            flush_paragraph(w, &mut para, page);
            let body = strip_inline(trimmed[level as usize..].trim());
            while w.stack.last().is_some_and(|(l, _)| *l >= level) {
                w.stack.pop();
            }
            if let Some(mut m) = w.push("heading", &body) {
                m.insert("level".into(), json!(level));
                stamp!(m);
            }
            w.stack.push((level, body.trim().to_string()));
            continue;
        }
        if let Some((marker, rest)) = list_marker(trimmed) {
            flush_paragraph(w, &mut para, page);
            w.marker = Some(marker);
            if let Some(m) = w.push("list-item", &strip_inline(rest)) {
                stamp!(m);
            }
            w.marker = None;
            continue;
        }
        if trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') && trimmed.len() >= 3 {
            flush_paragraph(w, &mut para, page);
            continue;
        }
        para.push(strip_inline(line));
    }
    flush_paragraph(w, &mut para, page);
    flush_table(w, &mut table, page);
    if let Some((lang, body)) = fence {
        let text = body.join("\n");
        if let Some(mut m) = w.push("code", &text) {
            if !lang.is_empty() {
                m.insert("lang".into(), json!(lang));
            }
            stamp!(m);
        }
    }
}

fn flush_paragraph(w: &mut Walk, para: &mut Vec<String>, page: Option<u32>) {
    if para.is_empty() {
        return;
    }
    let body = std::mem::take(para).join("\n");
    if let Some(mut m) = w.push("paragraph", &body) {
        if let Some(p) = page {
            m.insert("page".into(), json!(p));
        }
        w.emit(m);
    }
}

fn flush_table(w: &mut Walk, rows: &mut Vec<Vec<String>>, page: Option<u32>) {
    if rows.is_empty() {
        return;
    }
    let rows = std::mem::take(rows);
    let headers = rows.first().cloned().unwrap_or_default();
    for (r, cells) in rows.iter().enumerate() {
        let body = cells.join(" | ");
        let Some(mut m) = w.push("table-row", &body) else {
            continue;
        };
        let mut at = m.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
        let mut wire = Vec::with_capacity(cells.len());
        for (c, cell) in cells.iter().enumerate() {
            let len = cell.chars().count();
            let mut cm = Map::new();
            cm.insert("column".into(), json!(c));
            if let Some(name) = headers.get(c).filter(|n| r > 0 && !n.trim().is_empty()) {
                cm.insert("name".into(), json!(name));
            }
            cm.insert("text".into(), json!(cell));
            cm.insert("start".into(), json!(at));
            cm.insert("end".into(), json!(at + len));
            wire.push(Value::Object(cm));
            at += len + 3;
        }
        m.insert("row".into(), json!(r));
        m.insert("is-header".into(), json!(r == 0));
        m.insert("cells".into(), json!(wire));
        if let Some(p) = page {
            m.insert("page".into(), json!(p));
        }
        w.emit(m);
    }
}

fn heading_level(line: &str) -> Option<u8> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    let rest = &line[hashes..];
    if (1..=6).contains(&hashes) && rest.starts_with(' ') {
        Some(hashes as u8)
    } else {
        None
    }
}

fn list_marker(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(("-".to_string(), rest));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &line[..digits]), rest));
        }
    }
    None
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim_matches('|')
        .split('|')
        .map(|c| strip_inline(c.trim()))
        .collect()
}

fn is_rule_cell(cell: &str) -> bool {
    let c = cell.trim();
    !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
}

/// Markdown inline syntax removed, so the text a query is matched against is
/// the text a human sees: `**March**` is `March`, `[report](x.pdf)` is
/// `report`, `![Q1 chart](c.png)` is `Q1 chart`.
fn strip_inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '!' if chars.get(i + 1) == Some(&'[') => i += 1,
            '[' => i += 1,
            ']' if chars.get(i + 1) == Some(&'(') => {
                // Drop the target, keep the text already emitted.
                let mut depth = 1;
                i += 2;
                while i < chars.len() && depth > 0 {
                    match chars[i] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }
            '*' | '`' | '~' => i += 1,
            '_' => {
                let before = if i == 0 { None } else { chars.get(i - 1) };
                let after = chars.get(i + 1);
                // `snake_case` keeps its underscores; `_emphasis_` loses them.
                let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric());
                if word(before) && word(after) {
                    out.push('_');
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}
