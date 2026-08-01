//! `.xlsx` writing on top of `rust_xlsxwriter` — the whole workbook described by
//! one JSON spec, so a caller crosses the FFI boundary once instead of once per
//! cell.
//!
//! ```json
//! {"properties": {"title": "Q3", "author": "vis"},
//!  "sheets": [{"name": "Sales",
//!              "columns": [{"first": 0, "last": 3, "width": 18}],
//!              "freeze": [1, 0],
//!              "cells": [{"row": 0, "col": 0, "type": "string", "value": "Region",
//!                         "format": {"bold": true, "bg_color": "#eeeeee"}},
//!                        {"row": 1, "col": 0, "value": 42.5,
//!                         "format": {"num_format": "0.00"}}]}]}
//! ```
//!
//! Cell `type` is optional: a JSON string/number/bool/null is written as itself.
//! The explicit types are `string`, `number`, `boolean`, `formula`, `datetime`,
//! `date`, `time`, `url`, `blank` and `rich` (a run list of `[format?, text]`).

use std::collections::HashMap;

use rust_xlsxwriter::{
    Color, DocProperties, ExcelDateTime, Format, FormatAlign, FormatBorder, FormatPattern,
    FormatScript, FormatUnderline, Formula, Image, Url, Workbook, Worksheet,
};
use serde_json::Value;

use crate::office::{arr, b, b64, f, hex6, i, s};
use crate::util::{R, err};

pub fn write(spec: &Value) -> R<Vec<u8>> {
    let mut wb = Workbook::new();
    if let Some(p) = spec.get("properties").filter(|v| v.is_object()) {
        wb.set_properties(&properties(p));
    }
    // One Format per distinct format spec: Excel deduplicates styles anyway, and
    // this keeps the shared header format from becoming 5000 style records.
    let mut cache: HashMap<String, Format> = HashMap::new();
    let sheets = arr(spec, "sheets");
    if sheets.is_empty() {
        return err("xlsx: `sheets` is empty");
    }
    for sheet in sheets {
        let ws = wb.add_worksheet();
        write_sheet(ws, sheet, &mut cache)?;
    }
    wb.save_to_buffer()
        .map_err(|e| format!("xlsx: writing the workbook failed: {e}"))
}

fn properties(p: &Value) -> DocProperties {
    let mut d = DocProperties::new();
    for (k, set) in [
        ("title", 0),
        ("subject", 1),
        ("author", 2),
        ("manager", 3),
        ("company", 4),
        ("category", 5),
        ("keywords", 6),
        ("comments", 7),
        ("status", 8),
    ] {
        if let Some(v) = s(p, k) {
            d = match set {
                0 => d.set_title(v),
                1 => d.set_subject(v),
                2 => d.set_author(v),
                3 => d.set_manager(v),
                4 => d.set_company(v),
                5 => d.set_category(v),
                6 => d.set_keywords(v),
                7 => d.set_comment(v),
                _ => d.set_status(v),
            };
        }
    }
    d
}

/// Look a format spec up in the cache, building it on first sight.
fn format<'a>(cache: &'a mut HashMap<String, Format>, v: Option<&Value>) -> Option<&'a Format> {
    let v = v?;
    if !v.is_object() {
        return None;
    }
    let key = v.to_string();
    if !cache.contains_key(&key) {
        cache.insert(key.clone(), build_format(v));
    }
    cache.get(&key)
}

fn color(spec: &Value, k: &str) -> Option<Color> {
    hex6(spec.get(k)?).and_then(|h| u32::from_str_radix(&h, 16).ok().map(Color::RGB))
}

fn border(v: &Value) -> FormatBorder {
    // xlsxwriter's numeric border styles, plus their names.
    let name = match v {
        Value::String(s) => s.to_ascii_lowercase(),
        Value::Number(n) => match n.as_i64().unwrap_or(1) {
            0 => "none".into(),
            1 => "thin".into(),
            2 => "medium".into(),
            3 => "dashed".into(),
            4 => "dotted".into(),
            5 => "thick".into(),
            6 => "double".into(),
            7 => "hair".into(),
            8 => "medium_dashed".into(),
            9 => "dash_dot".into(),
            10 => "medium_dash_dot".into(),
            11 => "dash_dot_dot".into(),
            12 => "medium_dash_dot_dot".into(),
            13 => "slant_dash_dot".into(),
            _ => "thin".into(),
        },
        Value::Bool(true) => "thin".into(),
        _ => "none".into(),
    };
    match name.as_str() {
        "none" => FormatBorder::None,
        "thin" => FormatBorder::Thin,
        "medium" => FormatBorder::Medium,
        "dashed" => FormatBorder::Dashed,
        "dotted" => FormatBorder::Dotted,
        "thick" => FormatBorder::Thick,
        "double" => FormatBorder::Double,
        "hair" => FormatBorder::Hair,
        "medium_dashed" => FormatBorder::MediumDashed,
        "dash_dot" => FormatBorder::DashDot,
        "medium_dash_dot" => FormatBorder::MediumDashDot,
        "dash_dot_dot" => FormatBorder::DashDotDot,
        "medium_dash_dot_dot" => FormatBorder::MediumDashDotDot,
        "slant_dash_dot" => FormatBorder::SlantDashDot,
        _ => FormatBorder::Thin,
    }
}

fn align(name: &str) -> Option<FormatAlign> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => FormatAlign::Left,
        "center" | "centre" => FormatAlign::Center,
        "right" => FormatAlign::Right,
        "fill" => FormatAlign::Fill,
        "justify" => FormatAlign::Justify,
        "center_across" | "centre_across" => FormatAlign::CenterAcross,
        "distributed" => FormatAlign::Distributed,
        "top" => FormatAlign::Top,
        "bottom" => FormatAlign::Bottom,
        "vcenter" | "vcentre" => FormatAlign::VerticalCenter,
        "vjustify" => FormatAlign::VerticalJustify,
        "vdistributed" => FormatAlign::Distributed,
        "general" => FormatAlign::General,
        _ => return None,
    })
}

fn build_format(v: &Value) -> Format {
    let mut f0 = Format::new();
    if b(v, "bold").unwrap_or(false) {
        f0 = f0.set_bold();
    }
    if b(v, "italic").unwrap_or(false) {
        f0 = f0.set_italic();
    }
    if b(v, "font_strikeout").unwrap_or(false) {
        f0 = f0.set_font_strikethrough();
    }
    if b(v, "text_wrap").unwrap_or(false) {
        f0 = f0.set_text_wrap();
    }
    if b(v, "shrink").unwrap_or(false) {
        f0 = f0.set_shrink();
    }
    if b(v, "hidden").unwrap_or(false) {
        f0 = f0.set_hidden();
    }
    if b(v, "locked") == Some(false) {
        f0 = f0.set_unlocked();
    }
    if b(v, "quote_prefix").unwrap_or(false) {
        f0 = f0.set_quote_prefix();
    }
    if let Some(n) = s(v, "num_format") {
        f0 = f0.set_num_format(n);
    } else if let Some(n) = i(v, "num_format_index") {
        f0 = f0.set_num_format_index(n as u8);
    }
    if let Some(n) = s(v, "font_name") {
        f0 = f0.set_font_name(n);
    }
    if let Some(n) = f(v, "font_size") {
        f0 = f0.set_font_size(n);
    }
    if let Some(c) = color(v, "font_color") {
        f0 = f0.set_font_color(c);
    }
    if let Some(u) = v.get("underline") {
        let n = match u {
            Value::Bool(true) => 1,
            Value::Bool(false) => 0,
            Value::Number(n) => n.as_i64().unwrap_or(1),
            _ => 1,
        };
        f0 = f0.set_underline(match n {
            0 => FormatUnderline::None,
            2 => FormatUnderline::Double,
            33 => FormatUnderline::SingleAccounting,
            34 => FormatUnderline::DoubleAccounting,
            _ => FormatUnderline::Single,
        });
    }
    if b(v, "font_script_super").unwrap_or(false) {
        f0 = f0.set_font_script(FormatScript::Superscript);
    }
    if b(v, "font_script_sub").unwrap_or(false) {
        f0 = f0.set_font_script(FormatScript::Subscript);
    }
    if let Some(a) = s(v, "align").and_then(align) {
        f0 = f0.set_align(a);
    }
    if let Some(a) = s(v, "valign").and_then(align) {
        f0 = f0.set_align(a);
    }
    if let Some(n) = i(v, "rotation") {
        f0 = f0.set_rotation(n as i16);
    }
    if let Some(n) = i(v, "indent") {
        f0 = f0.set_indent(n.clamp(0, 250) as u8);
    }
    // Excel's fill is pattern + foreground/background; a plain `bg_color` means
    // "solid fill in this colour", which is what every caller actually wants.
    let pattern = i(v, "pattern").unwrap_or(0);
    if let Some(c) = color(v, "bg_color") {
        if pattern > 1 {
            f0 = f0.set_background_color(c);
        } else {
            f0 = f0.set_pattern(FormatPattern::Solid).set_background_color(c);
        }
    }
    if let Some(c) = color(v, "fg_color") {
        f0 = f0.set_foreground_color(c);
        if pattern == 0 {
            f0 = f0.set_pattern(FormatPattern::Solid);
        }
    }
    if pattern > 1 {
        f0 = f0.set_pattern(match pattern {
            2 => FormatPattern::MediumGray,
            3 => FormatPattern::DarkGray,
            4 => FormatPattern::LightGray,
            5 => FormatPattern::DarkHorizontal,
            6 => FormatPattern::DarkVertical,
            7 => FormatPattern::DarkDown,
            8 => FormatPattern::DarkUp,
            9 => FormatPattern::DarkGrid,
            10 => FormatPattern::DarkTrellis,
            11 => FormatPattern::LightHorizontal,
            12 => FormatPattern::LightVertical,
            _ => FormatPattern::Solid,
        });
    }
    if let Some(bv) = v.get("border") {
        f0 = f0.set_border(border(bv));
    }
    for (k, side) in [("top", 0), ("bottom", 1), ("left", 2), ("right", 3)] {
        if let Some(bv) = v.get(k) {
            let s = border(bv);
            f0 = match side {
                0 => f0.set_border_top(s),
                1 => f0.set_border_bottom(s),
                2 => f0.set_border_left(s),
                _ => f0.set_border_right(s),
            };
        }
    }
    if let Some(c) = color(v, "border_color") {
        f0 = f0.set_border_color(c);
    }
    for (k, side) in [
        ("top_color", 0),
        ("bottom_color", 1),
        ("left_color", 2),
        ("right_color", 3),
    ] {
        if let Some(c) = color(v, k) {
            f0 = match side {
                0 => f0.set_border_top_color(c),
                1 => f0.set_border_bottom_color(c),
                2 => f0.set_border_left_color(c),
                _ => f0.set_border_right_color(c),
            };
        }
    }
    f0
}

fn datetime(v: &Value) -> R<ExcelDateTime> {
    match v {
        Value::Number(n) => ExcelDateTime::from_serial_datetime(n.as_f64().unwrap_or(0.0))
            .map_err(|e| format!("xlsx: bad date serial: {e}")),
        Value::String(s) => {
            ExcelDateTime::parse_from_str(s).map_err(|e| format!("xlsx: bad date {s:?}: {e}"))
        }
        _ => err("xlsx: a datetime cell needs a serial number or an ISO 8601 string"),
    }
}

fn write_sheet(ws: &mut Worksheet, sheet: &Value, cache: &mut HashMap<String, Format>) -> R<()> {
    if let Some(n) = s(sheet, "name") {
        ws.set_name(n)
            .map_err(|e| format!("xlsx: bad sheet name {n:?}: {e}"))?;
    }
    if b(sheet, "hidden").unwrap_or(false) {
        ws.set_hidden(true);
    }
    if b(sheet, "active").unwrap_or(false) {
        ws.set_active(true);
    }
    if b(sheet, "selected").unwrap_or(false) {
        ws.set_selected(true);
    }
    if b(sheet, "landscape").unwrap_or(false) {
        ws.set_landscape();
    }
    if let Some(z) = i(sheet, "zoom") {
        ws.set_zoom(z.clamp(10, 400) as u16);
    }
    if let Some(c) = color(sheet, "tab_color") {
        ws.set_tab_color(c);
    }
    if let Some(g) = i(sheet, "hide_gridlines") {
        // xlsxwriter: 0 = show both, 1 = hide printed, 2 = hide both.
        if g >= 2 {
            ws.set_screen_gridlines(false);
        }
        if g >= 1 {
            ws.set_print_gridlines(false);
        }
    }
    if b(sheet, "protect").unwrap_or(false) {
        ws.protect();
    }
    if let Some(fz) = sheet.get("freeze").and_then(|v| v.as_array())
        && fz.len() >= 2
    {
        ws.set_freeze_panes(
            fz[0].as_u64().unwrap_or(0) as u32,
            fz[1].as_u64().unwrap_or(0) as u16,
        )
        .map_err(|e| format!("xlsx: freeze panes: {e}"))?;
    }

    for c in arr(sheet, "columns") {
        let first = i(c, "first").unwrap_or(0).max(0) as u16;
        let last = i(c, "last").unwrap_or(first as i64).max(0) as u16;
        if let Some(w) = f(c, "width") {
            ws.set_column_range_width(first, last, w)
                .map_err(|e| format!("xlsx: column width: {e}"))?;
        }
        if let Some(fm) = format(cache, c.get("format")).cloned() {
            ws.set_column_range_format(first, last, &fm)
                .map_err(|e| format!("xlsx: column format: {e}"))?;
        }
        if b(c, "hidden").unwrap_or(false) {
            ws.set_column_range_hidden(first, last)
                .map_err(|e| format!("xlsx: hide column: {e}"))?;
        }
    }
    for r in arr(sheet, "rows") {
        let row = i(r, "index").unwrap_or(0).max(0) as u32;
        if let Some(h) = f(r, "height") {
            ws.set_row_height(row, h)
                .map_err(|e| format!("xlsx: row height: {e}"))?;
        }
        if let Some(fm) = format(cache, r.get("format")).cloned() {
            ws.set_row_format(row, &fm)
                .map_err(|e| format!("xlsx: row format: {e}"))?;
        }
        if b(r, "hidden").unwrap_or(false) {
            ws.set_row_hidden(row)
                .map_err(|e| format!("xlsx: hide row: {e}"))?;
        }
    }

    for cell in arr(sheet, "cells") {
        write_cell(ws, cell, cache)?;
    }

    for m in arr(sheet, "merges") {
        let fm = format(cache, m.get("format"))
            .cloned()
            .unwrap_or_else(Format::new);
        ws.merge_range(
            i(m, "first_row").unwrap_or(0).max(0) as u32,
            i(m, "first_col").unwrap_or(0).max(0) as u16,
            i(m, "last_row").unwrap_or(0).max(0) as u32,
            i(m, "last_col").unwrap_or(0).max(0) as u16,
            s(m, "text").unwrap_or(""),
            &fm,
        )
        .map_err(|e| format!("xlsx: merge_range: {e}"))?;
    }

    if let Some(a) = sheet.get("autofilter").and_then(|v| v.as_array())
        && a.len() >= 4
    {
        let n = |k: usize| a[k].as_i64().unwrap_or(0).max(0);
        ws.autofilter(n(0) as u32, n(1) as u16, n(2) as u32, n(3) as u16)
            .map_err(|e| format!("xlsx: autofilter: {e}"))?;
    }

    for im in arr(sheet, "images") {
        let bytes = b64(im, "data")?;
        let mut image = Image::new_from_buffer(&bytes).map_err(|e| format!("xlsx: image: {e}"))?;
        if let Some(w) = f(im, "scale_x") {
            image = image.set_scale_width(w);
        }
        if let Some(h) = f(im, "scale_y") {
            image = image.set_scale_height(h);
        }
        if let Some(t) = s(im, "alt_text") {
            image = image.set_alt_text(t);
        }
        let row = i(im, "row").unwrap_or(0).max(0) as u32;
        let col = i(im, "col").unwrap_or(0).max(0) as u16;
        let (dx, dy) = (
            i(im, "x_offset").unwrap_or(0).max(0) as u32,
            i(im, "y_offset").unwrap_or(0).max(0) as u32,
        );
        if dx == 0 && dy == 0 {
            ws.insert_image(row, col, &image)
        } else {
            ws.insert_image_with_offset(row, col, &image, dx, dy)
        }
        .map_err(|e| format!("xlsx: insert image: {e}"))?;
    }
    if b(sheet, "autofit").unwrap_or(false) {
        ws.autofit();
    }
    Ok(())
}

fn write_cell(ws: &mut Worksheet, cell: &Value, cache: &mut HashMap<String, Format>) -> R<()> {
    let row = i(cell, "row").unwrap_or(0).max(0) as u32;
    let col = i(cell, "col").unwrap_or(0).max(0) as u16;
    let value = cell.get("value").unwrap_or(&Value::Null);
    let kind = s(cell, "type").map(str::to_string).unwrap_or_else(|| {
        if cell.get("formula").is_some() {
            return "formula".to_string();
        }
        match value {
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Null => "blank",
            // Mirrors xlsxwriter's `write()`: a leading `=` means a formula.
            Value::String(t) if t.starts_with('=') => "formula",
            _ => "string",
        }
        .to_string()
    });
    let fmt = format(cache, cell.get("format")).cloned();
    let e = |what: &str, err: rust_xlsxwriter::XlsxError| {
        format!("xlsx: {what} at row {row} col {col}: {err}")
    };
    match kind.as_str() {
        "number" => {
            let n = value.as_f64().unwrap_or(0.0);
            match &fmt {
                Some(f0) => ws.write_number_with_format(row, col, n, f0),
                None => ws.write_number(row, col, n),
            }
            .map_err(|x| e("number", x))?;
        }
        "boolean" => {
            let v = value.as_bool().unwrap_or(false);
            match &fmt {
                Some(f0) => ws.write_boolean_with_format(row, col, v, f0),
                None => ws.write_boolean(row, col, v),
            }
            .map_err(|x| e("boolean", x))?;
        }
        "formula" => {
            let src = s(cell, "formula")
                .or_else(|| value.as_str())
                .unwrap_or("=0");
            let mut f0 = Formula::new(src);
            if let Some(r) = cell.get("result") {
                f0 = f0.set_result(match r {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
            }
            match &fmt {
                Some(fm) => ws.write_formula_with_format(row, col, f0, fm),
                None => ws.write_formula(row, col, f0),
            }
            .map_err(|x| e("formula", x))?;
        }
        "datetime" | "date" | "time" => {
            let d = datetime(value)?;
            // A date without a number format renders as a bare serial number.
            let fm = fmt.clone().unwrap_or_else(|| {
                Format::new().set_num_format(match kind.as_str() {
                    "date" => "yyyy-mm-dd",
                    "time" => "hh:mm:ss",
                    _ => "yyyy-mm-dd hh:mm",
                })
            });
            ws.write_datetime_with_format(row, col, &d, &fm)
                .map_err(|x| e("datetime", x))?;
        }
        "url" => {
            let mut u = Url::new(value.as_str().or_else(|| s(cell, "url")).unwrap_or(""));
            if let Some(t) = s(cell, "text") {
                u = u.set_text(t);
            }
            if let Some(t) = s(cell, "tip") {
                u = u.set_tip(t);
            }
            match &fmt {
                Some(f0) => ws.write_url_with_format(row, col, u, f0),
                None => ws.write_url(row, col, u),
            }
            .map_err(|x| e("url", x))?;
        }
        "blank" => {
            if let Some(f0) = &fmt {
                ws.write_blank(row, col, f0).map_err(|x| e("blank", x))?;
            }
        }
        "rich" => {
            // `runs`: [{"format": {...}, "text": "..."}] — the segments of one cell.
            let runs = arr(cell, "runs");
            let mut owned: Vec<(Format, String)> = Vec::with_capacity(runs.len());
            for r in runs {
                owned.push((
                    format(cache, r.get("format")).cloned().unwrap_or_default(),
                    s(r, "text").unwrap_or("").to_string(),
                ));
            }
            let borrowed: Vec<(&Format, &str)> =
                owned.iter().map(|(f0, t)| (f0, t.as_str())).collect();
            match &fmt {
                Some(f0) => ws.write_rich_string_with_format(row, col, &borrowed, f0),
                None => ws.write_rich_string(row, col, &borrowed),
            }
            .map_err(|x| e("rich string", x))?;
        }
        _ => {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            match &fmt {
                Some(f0) => ws.write_string_with_format(row, col, &text, f0),
                None => ws.write_string(row, col, &text),
            }
            .map_err(|x| e("string", x))?;
        }
    }
    Ok(())
}
