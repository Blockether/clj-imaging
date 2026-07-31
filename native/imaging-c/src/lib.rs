//! C ABI over resvg + tiny-skia + the `image` crate, so clj-imaging can do every
//! raster/vector/plot job in-process through the JDK Foreign Function & Memory
//! API — no AWT, no ImageIO, no java.desktop, no subprocess.
//!
//! Conventions, uniform across every symbol:
//!
//!   * A `*mut Image` is an opaque handle; free it with `imaging_image_free`.
//!   * Bytes come back as a `*mut Buf` handle: read `imaging_buf_ptr` /
//!     `imaging_buf_len`, then `imaging_buf_free`. Never free a `Buf` pointer
//!     with `free`.
//!   * `char*` returns are heap C strings; free with `imaging_free_string`.
//!     `imaging_version` is the one exception (static, never freed).
//!   * NULL (or -1 for the `i32` returns) signals failure, and
//!     `imaging_last_error()` then explains it for the CALLING thread.
//!   * Colours cross the ABI as packed `0xRRGGBBAA` u32.
//!   * Panics never unwind into the JVM: every entry point is caught and turned
//!     into a failure with the panic message as the error.

mod canvas;
mod imgops;
mod plot;
mod svg;
mod text;
mod util;

use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use imgops::Image;
use util::{Buf, R, Rgba, clear_error, err, into_c_string, set_error, take_error};

const VERSION: &str = concat!(
    "clj-imaging-cdylib ",
    env!("CARGO_PKG_VERSION"),
    " (resvg 0.47, image 0.25)\0"
);

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

/// Run `f` with the error channel cleared and panics contained.
fn guard<T>(f: impl FnOnce() -> R<T>) -> Option<T> {
    clear_error();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            set_error(e);
            None
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic in imaging-c".to_string());
            set_error(format!("panic: {msg}"));
            None
        }
    }
}

fn img_out(r: Option<Image>) -> *mut Image {
    r.map(|i| Box::into_raw(Box::new(i)))
        .unwrap_or(ptr::null_mut())
}

fn buf_out(r: Option<Vec<u8>>) -> *mut Buf {
    r.map(Buf::into_raw).unwrap_or(ptr::null_mut())
}

fn str_out(r: Option<String>) -> *mut c_char {
    r.map(into_c_string).unwrap_or(ptr::null_mut())
}

fn json_out(r: Option<serde_json::Value>) -> *mut c_char {
    str_out(r.map(|v| v.to_string()))
}

/// Borrow a NUL-terminated C string.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated C string.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// # Safety
/// `p`/`len` must describe a readable byte range, or `p` must be null.
unsafe fn bytes<'a>(p: *const u8, len: u64) -> R<&'a [u8]> {
    if p.is_null() {
        return err("null data pointer");
    }
    Ok(unsafe { std::slice::from_raw_parts(p, len as usize) })
}

/// # Safety
/// `p` must be null or a live `*mut Image` from this library.
unsafe fn image<'a>(p: *mut Image) -> R<&'a mut Image> {
    if p.is_null() {
        return err("null image handle");
    }
    Ok(unsafe { &mut *p })
}

fn json_arg(p: *const c_char) -> R<serde_json::Value> {
    match unsafe { cstr(p) } {
        None => Ok(serde_json::Value::Null),
        Some(s) if s.trim().is_empty() => Ok(serde_json::Value::Null),
        Some(s) => serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}")),
    }
}

// ---------------------------------------------------------------------------
// lifecycle + diagnostics
// ---------------------------------------------------------------------------

/// Static version string. Do NOT free.
#[unsafe(no_mangle)]
pub extern "C" fn imaging_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

/// Take this thread's last failure reason (heap; free with
/// `imaging_free_string`), or NULL when the last call succeeded.
#[unsafe(no_mangle)]
pub extern "C" fn imaging_last_error() -> *mut c_char {
    match take_error() {
        Some(m) => into_c_string(m),
        None => ptr::null_mut(),
    }
}

/// Free a `char*` returned by this library.
///
/// # Safety
/// `p` must come from this library and be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { std::ffi::CString::from_raw(p) });
    }
}

/// # Safety
/// `p` must be a live `Buf` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_buf_ptr(p: *mut Buf) -> *const u8 {
    if p.is_null() {
        return ptr::null();
    }
    unsafe { (*p).data.as_ptr() }
}

/// # Safety
/// `p` must be a live `Buf` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_buf_len(p: *mut Buf) -> u64 {
    if p.is_null() {
        return 0;
    }
    unsafe { (*p).data.len() as u64 }
}

/// # Safety
/// `p` must be a `Buf` handle from this library, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_buf_free(p: *mut Buf) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

/// # Safety
/// `p` must be an `Image` handle from this library, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_image_free(p: *mut Image) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

// ---------------------------------------------------------------------------
// decode / encode / probe
// ---------------------------------------------------------------------------

/// Identify encoded bytes without fully decoding: JSON `{"format","width",
/// "height","bytes","is_animated","frames","color","channels","bits",
/// "is_grayscale","has_alpha"}`. The colour fields describe the SOURCE file;
/// a decoded handle is always RGBA8.
///
/// # Safety
/// `data`/`len` must describe a readable byte range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_probe(data: *const u8, len: u64) -> *mut c_char {
    json_out(guard(|| {
        let d = unsafe { bytes(data, len) }?;
        if svg::looks_like_svg(d) {
            let img = svg::render(d, &serde_json::Value::Null)?;
            let mut v = serde_json::json!({
                "format": "svg", "width": img.width(), "height": img.height(),
                "bytes": d.len(), "is_animated": false, "frames": 1
            });
            // A vector document has no stored colour type: it rasterises into
            // the same straight RGBA8 everything else decodes to.
            imgops::merge_color(&mut v, image::ColorType::Rgba8);
            return Ok(v);
        }
        imgops::probe(d)
    }))
}

/// Decode any supported raster format (or an SVG document) into an image handle.
///
/// # Safety
/// `data`/`len` must describe a readable byte range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_decode(data: *const u8, len: u64) -> *mut Image {
    img_out(guard(|| {
        let d = unsafe { bytes(data, len) }?;
        if svg::looks_like_svg(d) {
            svg::render(d, &serde_json::Value::Null)
        } else {
            imgops::decode(d)
        }
    }))
}

/// A new `w`x`h` image filled with the packed `0xRRGGBBAA` colour.
#[unsafe(no_mangle)]
pub extern "C" fn imaging_new(w: u32, h: u32, color: u32) -> *mut Image {
    img_out(guard(|| Image::new(w, h, Rgba::from_u32(color))))
}

/// An image from straight (non-premultiplied) RGBA8 rows.
///
/// # Safety
/// `data`/`len` must describe `w*h*4` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_from_pixels(
    data: *const u8,
    len: u64,
    w: u32,
    h: u32,
) -> *mut Image {
    img_out(guard(|| {
        Image::from_rgba(w, h, unsafe { bytes(data, len) }?)
    }))
}

/// The image's straight RGBA8 rows.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_pixels(p: *mut Image) -> *mut Buf {
    buf_out(guard(|| Ok(unsafe { image(p) }?.buf.as_raw().clone())))
}

/// `{"width","height","is_opaque","is_grayscale"}` — facts read off the RGBA8
/// pixels, not off whatever file the handle was decoded from (ask `probe` for
/// the source's colour type).
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_info(p: *mut Image) -> *mut c_char {
    json_out(guard(|| {
        let img = unsafe { image(p) }?;
        Ok(serde_json::json!({
            "width": img.width(), "height": img.height(),
            "is_opaque": img.is_opaque(), "is_grayscale": img.is_grayscale()
        }))
    }))
}

/// Encode to `format` ("png","jpeg","webp","gif","bmp","tiff","ico","qoi",
/// "pnm","tga"). `quality` ∈ 1..=100 applies to lossy formats (0 => 85).
///
/// # Safety
/// `p` must be a live image handle; `format` a C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_encode(
    p: *mut Image,
    format: *const c_char,
    quality: u32,
) -> *mut Buf {
    buf_out(guard(|| {
        let img = unsafe { image(p) }?;
        let f = unsafe { cstr(format) }.unwrap_or("png");
        imgops::encode(img, f, quality.min(100) as u8)
    }))
}

/// One-shot decode -> normalise -> re-encode, the "make this attachment safe to
/// send" path. Options JSON: `format`, `quality`, `max_width`, `max_height`,
/// `width`, `height`, `background` (flatten alpha onto it), `filter`,
/// `max_bytes` (retry with lower quality/scale until it fits).
///
/// # Safety
/// `data`/`len` must be readable; `opts` a C string or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_convert(
    data: *const u8,
    len: u64,
    opts: *const c_char,
) -> *mut Buf {
    buf_out(guard(|| {
        let d = unsafe { bytes(data, len) }?;
        let o = json_arg(opts)?;
        imgops::convert(d, &o)
    }))
}

// ---------------------------------------------------------------------------
// transforms
// ---------------------------------------------------------------------------

/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_resize(
    p: *mut Image,
    w: u32,
    h: u32,
    filter: *const c_char,
) -> *mut Image {
    img_out(guard(|| {
        imgops::resize(
            unsafe { image(p) }?,
            w,
            h,
            unsafe { cstr(filter) }.unwrap_or("lanczos3"),
        )
    }))
}

/// Scale down to fit inside `max_w` x `max_h`, preserving aspect ratio.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_thumbnail(p: *mut Image, max_w: u32, max_h: u32) -> *mut Image {
    img_out(guard(|| {
        imgops::thumbnail(unsafe { image(p) }?, max_w, max_h)
    }))
}

/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_crop(p: *mut Image, x: i64, y: i64, w: u32, h: u32) -> *mut Image {
    img_out(guard(|| imgops::crop(unsafe { image(p) }?, x, y, w, h)))
}

/// Rotate counter-clockwise by `degrees`. `expand` != 0 grows the canvas to hold
/// the rotated image; `bg` is the packed fill behind it.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_rotate(
    p: *mut Image,
    degrees: f64,
    expand: u32,
    bg: u32,
) -> *mut Image {
    img_out(guard(|| {
        imgops::rotate(
            unsafe { image(p) }?,
            degrees,
            expand != 0,
            Rgba::from_u32(bg),
        )
    }))
}

/// `mode`: "h"/"horizontal" or "v"/"vertical".
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_flip(p: *mut Image, mode: *const c_char) -> *mut Image {
    img_out(guard(|| {
        imgops::flip(unsafe { image(p) }?, unsafe { cstr(mode) }.unwrap_or("h"))
    }))
}

/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_grayscale(p: *mut Image) -> *mut Image {
    img_out(guard(|| Ok(imgops::grayscale(unsafe { image(p) }?))))
}

/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_invert(p: *mut Image) -> *mut Image {
    img_out(guard(|| Ok(imgops::invert(unsafe { image(p) }?))))
}

/// Gaussian blur.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_blur(p: *mut Image, sigma: f32) -> *mut Image {
    img_out(guard(|| Ok(imgops::blur(unsafe { image(p) }?, sigma))))
}

/// Composite the image onto an opaque `bg`.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_flatten(p: *mut Image, bg: u32) -> *mut Image {
    img_out(guard(|| {
        Ok(imgops::flatten(unsafe { image(p) }?, Rgba::from_u32(bg)))
    }))
}

/// Photometric adjustment. Options JSON: `brightness`, `contrast`,
/// `saturation` (1.0 = unchanged), `opacity` (1.0 = unchanged).
///
/// # Safety
/// `p` must be a live image handle; `opts` a C string or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_adjust(p: *mut Image, opts: *const c_char) -> *mut Image {
    img_out(guard(|| {
        let img = unsafe { image(p) }?;
        let o = json_arg(opts)?;
        let g = |k: &str| util::jf_or(&o, k, 1.0) as f32;
        Ok(imgops::adjust(
            img,
            g("brightness"),
            g("contrast"),
            g("saturation"),
            g("opacity"),
        ))
    }))
}

/// Paste `src` into `dst` at (x,y). `blend` != 0 alpha-composites, otherwise the
/// source replaces the destination pixels.
///
/// # Safety
/// Both handles must be live and distinct.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_paste(
    dst: *mut Image,
    src: *mut Image,
    x: i64,
    y: i64,
    blend: u32,
) -> i32 {
    match guard(|| {
        if dst == src {
            return err("cannot paste an image into itself");
        }
        let d = unsafe { image(dst) }?;
        let s = unsafe { image(src) }?;
        imgops::paste(d, s, x, y, blend != 0);
        Ok(())
    }) {
        Some(()) => 0,
        None => -1,
    }
}

/// Packed `0xRRGGBBAA` of one pixel, or -1 when out of bounds.
///
/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_get_pixel(p: *mut Image, x: u32, y: u32) -> i64 {
    match guard(|| {
        let img = unsafe { image(p) }?;
        if x >= img.width() || y >= img.height() {
            return err("pixel out of bounds");
        }
        let px = img.buf.get_pixel(x, y).0;
        Ok(Rgba(px[0], px[1], px[2], px[3]).to_u32() as i64)
    }) {
        Some(v) => v,
        None => -1,
    }
}

/// # Safety
/// `p` must be a live image handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_put_pixel(p: *mut Image, x: u32, y: u32, color: u32) -> i32 {
    match guard(|| {
        let img = unsafe { image(p) }?;
        if x >= img.width() || y >= img.height() {
            return err("pixel out of bounds");
        }
        let c = Rgba::from_u32(color);
        img.buf.put_pixel(x, y, image::Rgba([c.0, c.1, c.2, c.3]));
        Ok(())
    }) {
        Some(()) => 0,
        None => -1,
    }
}

// ---------------------------------------------------------------------------
// drawing, text, svg, plots
// ---------------------------------------------------------------------------

/// Execute a batch of drawing operations on the image (see `canvas::draw_ops`
/// for the op vocabulary). One call per batch keeps FFI chatter out of drawing
/// loops.
///
/// # Safety
/// `p` must be a live image handle; `spec` a JSON C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_draw(p: *mut Image, spec: *const c_char) -> i32 {
    match guard(|| {
        let img = unsafe { image(p) }?;
        let s = json_arg(spec)?;
        canvas::draw_ops(img, &s)
    }) {
        Some(()) => 0,
        None => -1,
    }
}

/// Measure a text run without drawing it. Spec JSON takes the same `text`,
/// `size`, `family`, `weight`, `italic`, `letter_spacing`, `line_height` keys as
/// the `text` draw op; returns `{"width","height","x","y","lines"}` where
/// `x`/`y` are the ink box relative to the first baseline.
///
/// # Safety
/// `spec` must be a JSON C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_text_measure(spec: *const c_char) -> *mut c_char {
    json_out(guard(|| {
        let s = json_arg(spec)?;
        let style = canvas::text_style(&s);
        let t = util::js(&s, "text").unwrap_or("");
        let (w, h, x, y, lines) = text::measure(t, &style)?;
        Ok(serde_json::json!({"width": w, "height": h, "x": x, "y": y, "lines": lines}))
    }))
}

/// Rasterise an SVG/SVGZ document. Options JSON: `width`, `height`, `scale`,
/// `max_width`, `max_height`, `background`, `dpi`, `font_family`, `font_size`,
/// `base_url`.
///
/// # Safety
/// `data`/`len` must be readable; `opts` a C string or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_svg_render(
    data: *const u8,
    len: u64,
    opts: *const c_char,
) -> *mut Image {
    img_out(guard(|| {
        let d = unsafe { bytes(data, len) }?;
        let o = json_arg(opts)?;
        svg::render(d, &o)
    }))
}

/// Render a plot specification (see `plot.rs`) to an image.
///
/// # Safety
/// `spec` must be a JSON C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_plot(spec: *const c_char) -> *mut Image {
    img_out(guard(|| {
        let s = json_arg(spec)?;
        plot::render(&s)
    }))
}

/// Add a font file (TTF/OTF/TTC bytes) to the shared database; returns the JSON
/// array of families it provided.
///
/// # Safety
/// `data`/`len` must describe readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn imaging_register_font(data: *const u8, len: u64) -> *mut c_char {
    json_out(guard(|| {
        let d = unsafe { bytes(data, len) }?;
        Ok(serde_json::json!(text::register_font(d.to_vec())?))
    }))
}

/// Every known font family, as a JSON array.
#[unsafe(no_mangle)]
pub extern "C" fn imaging_fonts() -> *mut c_char {
    json_out(guard(|| Ok(serde_json::json!(text::families()))))
}
