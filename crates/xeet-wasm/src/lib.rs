//! `xeet-wasm`: the renderer, compiled to WebAssembly for the Node package and
//! the HTTP function.
//!
//! # Why wasm-bindgen
//!
//! This crate used to export a hand-rolled ABI (`xeet_alloc`, `xeet_call`, …)
//! that copied bytes across the linear-memory boundary by hand. That is four
//! `unsafe` functions in a project whose PLAN.md §13 says the crates forbid
//! `unsafe`, and it meant every host had to reimplement the protocol. It is now
//! `wasm-bindgen`: one exported function takes a JSON string and returns bytes,
//! and the glue does the memory work.
//!
//! **On `unsafe`:** this crate has no `unsafe` of its own — every line written
//! here is safe code and the crate is `#![deny(unsafe_code)]`. The exception is
//! the `ffi` module below, which carries the `#[wasm_bindgen]` attribute: the
//! macro expands to shims that read and write WebAssembly memory, and those
//! shims contain `unsafe`. Keeping `#![forbid(unsafe_code)]` there is
//! impossible without giving up wasm-bindgen (its expansion is generated inside
//! the annotated item's scope), so the `allow` is scoped to that one module and
//! nothing else in the workspace may opt out. `tools/ship/check-unsafe.mjs`
//! enforces both halves: cores forbid, the glue module is the only place that
//! may allow.
//!
//! # The wire format
//!
//! One exported function, [`ffi::call`], takes `(op, request_json)` and returns
//! a framed buffer:
//!
//! ```text
//! offset 0   u8   status: 0 = ok, 1 = error
//! offset 1   u32  payload length  (little endian)   PNG bytes, or JSON
//! offset 5   u32  report length   (little endian)   JSON, render only, else 0
//! offset 9   u32  total length    (little endian)   the whole buffer
//! offset 13  ...  payload, then report
//! ```
//!
//! `op` is 0 (`explain`), 1 (`render`), 2 (`sheet`), 3 (`renderRgba`) or 4
//! (`renderBatch`); the request body may also name its op, which is what the JS
//! wrapper does. On success `explain` returns the report as the payload;
//! `render` returns the PNG and the report JSON after it. On error the payload
//! is the JSON body from `render::Error::to_json`.
//!
//! The two last ops exist so a browser does not have to pay for the same work
//! four times over a page of previews:
//!
//! - `renderRgba` (3) is `render` without the PNG encode: the payload is
//!   `[width_u32_le, height_u32_le, RGBA8…]`, and the report rides along in the
//!   report slot exactly as `render` sends it.
//! - `renderBatch` (4) renders one post at several option sets from a single
//!   parse of the post. Its payload is
//!
//!   ```text
//!   u32 count
//!   per item: u32 png_len, u32 report_len, u32 duration_us, png, report
//!   ```
//!
//!   and the duration is how long that item's render took, measured on the host
//!   clock (`now_us`) because the page shows it under each preview. It is
//!   display data: it never reaches a PNG or a report.
//!
//! The frame costs one 13-byte header and saves a second call for the report,
//! which is the part agents actually read.
//!
//! # Resolution
//!
//! The same crate also exports [`resolve`], which is `xeet-resolve` for a JS
//! host: the crawler runs here (so the rules are the CLI's and the playground's),
//! and every byte of I/O comes back through functions the caller passed in. It
//! is a separate export on purpose — the render ABI (`call`, `upstream_version`)
//! is unchanged, so a determinism check does not have to know resolution exists.
//!
//! # Determinism
//!
//! The same input produces byte-identical PNGs and reports here and in the
//! native build; `tools/ship/determinism.mjs` checks that on every platform in
//! CI. Nothing in this crate or the core reads a clock, the environment, the
//! filesystem or the network — the one exception is the per-item duration a
//! `renderBatch` reports, which is timing for the caller's UI and never part of
//! a PNG or a report.

#![deny(unsafe_code)]

pub mod js;

use xeet_core::limits::{self, Limits};
use xeet_core::model::{RenderOptions, Theme};
use xeet_core::render;

/// `explain`: JSON in, report JSON out.
pub const OP_EXPLAIN: u32 = 0;
/// `render`: JSON in, PNG plus report out.
pub const OP_RENDER: u32 = 1;
/// `sheet`: JSON in, PNG of a labeled grid out.
pub const OP_SHEET: u32 = 2;
/// `renderRgba`: JSON in, `width | height | RGBA8` plus report out.
pub const OP_RENDER_RGBA: u32 = 3;
/// `renderBatch`: one post, several option sets, one PNG plus report each.
pub const OP_RENDER_BATCH: u32 = 4;

/// Header size of a response buffer: status, payload length, report length,
/// total length.
pub const HEADER: usize = 13;

// ---------------------------------------------------------------------------
// The only allowed `unsafe` in the workspace: wasm-bindgen's macro expansion.
// ---------------------------------------------------------------------------
#[allow(unsafe_code)]
mod ffi {
    use super::*;
    use wasm_bindgen::prelude::*;

    /// Runs one request and returns a framed response buffer (see the module
    /// docs). Bad input, a limit overrun or a stage failure comes back as an
    /// error frame, never a trap.
    #[wasm_bindgen]
    pub fn call(op: u32, request: &str) -> Vec<u8> {
        let (status, payload, report) = super::handle(op, request);
        frame(status, &payload, &report)
    }

    /// The upstream app version this build embeds, so a caller can record what
    /// produced a render (PLAN.md §11: every preview says how fresh it is).
    /// Builds a screenshot artifact from the captured image's bytes and a
    /// capture request (`{ capturedAt, method, declaredName?, declaredType?,
    /// context? }`, the context in xeet-capture's `CaptureContext` shape). Pure:
    /// the same inputs give the same artifact id. Returns `{ documentText,
    /// assets, contextText }`: store and send `documentText` verbatim (the id
    /// hashes its number tokens as written); the image asset is the input bytes
    /// unchanged; `contextText` is the exact capture-context bytes it hashed.
    #[wasm_bindgen]
    pub fn capture_screenshot(image: &[u8], request: &str) -> Result<String, JsValue> {
        super::capture_screenshot_json(image, request).map_err(|e| JsValue::from_str(&e))
    }

    /// Verifies a screenshot artifact against the bytes it names: the
    /// document's identity, every asset's size and digest, its schema and
    /// provenance. Returns the verification report as JSON (`ok`, `checks`,
    /// `cannotEstablish`).
    #[wasm_bindgen]
    pub fn verify_screenshot(document: &str, image: &[u8], context_text: Option<String>) -> String {
        super::verify_screenshot_json(document, image, context_text.as_deref())
    }

    /// The artifact as a model that cannot see reads it: xeet's projection
    /// (`representation.understand`), the same one its gateway hands a
    /// text-only model. Each statement keeps its basis (from the source,
    /// observed, derived, inferred, unknown). `request` is `{ consumer:
    /// "text" | "vision", maxChars?, maxText?, sections?, focus?, depth?,
    /// format?: "text" | "json" }`. Returns `{ projection, delivery }`.
    #[wasm_bindgen]
    pub fn understand_artifact(document: &str, request: &str) -> Result<String, JsValue> {
        super::understand_artifact_json(document, request).map_err(|e| JsValue::from_str(&e))
    }

    /// Adds what a browser's OCR engine (tesseract.js) read from the pixels to
    /// a screenshot artifact, as the native `vision.read` path does: a new
    /// artifact `derivedFrom` the original, every line `inferred` and
    /// attributed to the engine. `document`, `image` and `context_text` are
    /// the original's verbatim text and bytes (checked before use). `request`
    /// is `{ at, engine, tsv }` (Tesseract TSV, with or without its header
    /// line) or `{ at, reading }` (an `xeet-ocr/1` result). Pure. Returns `{
    /// documentText, id, derivedFrom, lines, uncertainLines }`; store and send
    /// `documentText` verbatim.
    #[wasm_bindgen]
    pub fn annotate_ocr(document: &str, image: &[u8], context_text: Option<String>, request: &str) -> Result<String, JsValue> {
        super::annotate_ocr_json(document, image, context_text.as_deref(), request).map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen]
    pub fn upstream_version() -> String {
        xeet_core::fonts::UPSTREAM_APP_VERSION.to_string()
    }

    /// Registers one font file from a lazy pack.
    ///
    /// The host slices the pack's blob into its files (the offsets are in
    /// `packs.json`) and calls this once per file. The bytes are copied into
    /// wasm memory and kept for the rest of the module's life, exactly like an
    /// embedded face. The next render rebuilds the face table.
    #[wasm_bindgen]
    pub fn register_font(file: &str, data: &[u8]) -> Result<(), JsValue> {
        xeet_core::fonts::register_font(file, data.to_vec())
            .map_err(|message| JsValue::from_str(&message))
    }

    /// The lazy packs a request needs, as a JSON array.
    #[wasm_bindgen]
    pub fn packs_for(request: &str) -> String {
        serde_json::to_string(&xeet_core::fonts::packs_for_json(request))
            .unwrap_or_else(|_| "[]".to_string())
    }

    /// The lazy packs a request needs that are not registered yet, as a JSON
    /// array. A host calls this before rendering, fetches the result, and
    /// registers every file in each pack.
    #[wasm_bindgen]
    pub fn missing_packs(request: &str) -> String {
        serde_json::to_string(&xeet_core::fonts::missing_packs_for_json(request))
            .unwrap_or_else(|_| "[]".to_string())
    }

    /// `xeet-resolve`, for a JS host.
    ///
    /// `input` is text, an x.com post URL or post JSON; `options` is the JSON
    /// form of [`xeet_resolve::Options`]; `host` is the object `js.rs` documents
    /// (a `fetch` function is the only required member). Resolves with the JSON
    /// form of `Resolved`, rejects with `{code, status, message, fields?}`.
    #[wasm_bindgen]
    pub fn resolve(input: String, options: String, host: JsValue) -> js_sys::Promise {
        let host = match crate::js::JsHost::new(&host) {
            Ok(host) => host,
            Err(message) => {
                return js_sys::Promise::reject(&serde_wasm_error(&message, "invalid_host", 400))
            }
        };
        let options: xeet_resolve::Options = match serde_json::from_str(&options) {
            Ok(options) => options,
            Err(error) => {
                return js_sys::Promise::reject(&serde_wasm_error(
                    &format!("the options do not parse: {error}"),
                    "invalid_json",
                    400,
                ))
            }
        };
        // Which optional members the host actually has decides whether the
        // library enforces the matching rule (see `xeet_resolve::Lookup`).
        let has_lookup = host.lookup.is_some();
        let has_cache = host.cache_get.is_some();
        let has_clock = host.now.is_some();
        wasm_bindgen_futures::future_to_promise(async move {
            let lookup = has_lookup.then_some(&host);
            let cache = has_cache.then_some(&host);
            let clock = has_clock.then_some(&host);
            match xeet_resolve::resolve(&input, options, &host, lookup, cache, clock).await {
                Ok(resolved) => Ok(JsValue::from(
                    serde_json::to_string(&wire(&resolved))
                        .map_err(|error| serde_wasm_error(&error.to_string(), "internal", 500))?,
                )),
                Err(error) => Err(JsValue::from_str(&error.to_json().to_string())),
            }
        })
    }

    /// `POST /resolve-links`, for a JS host: one URL in, one link out.
    ///
    /// Rejects with the same `{code, status, message}` object as [`resolve`].
    #[wasm_bindgen]
    pub fn resolve_links(url: String, options: String, host: JsValue) -> js_sys::Promise {
        let host = match crate::js::JsHost::new(&host) {
            Ok(host) => host,
            Err(message) => {
                return js_sys::Promise::reject(&serde_wasm_error(&message, "invalid_host", 400))
            }
        };
        let options: xeet_resolve::Options = match serde_json::from_str(&options) {
            Ok(options) => options,
            Err(error) => {
                return js_sys::Promise::reject(&serde_wasm_error(
                    &format!("the options do not parse: {error}"),
                    "invalid_json",
                    400,
                ))
            }
        };
        let has_lookup = host.lookup.is_some();
        let has_cache = host.cache_get.is_some();
        let has_clock = host.now.is_some();
        wasm_bindgen_futures::future_to_promise(async move {
            let lookup = has_lookup.then_some(&host);
            let cache = has_cache.then_some(&host);
            let clock = has_clock.then_some(&host);
            match xeet_resolve::resolve_link(&url, options, &host, lookup, cache, clock).await {
                Ok(link) => Ok(JsValue::from(
                    serde_json::to_string(&link).unwrap_or_default(),
                )),
                Err(error) => Err(JsValue::from_str(&error.to_json().to_string())),
            }
        })
    }

    /// The JSON an error crosses the boundary as: an object, so the JS side can
    /// read `.code` without parsing.
    fn serde_wasm_error(message: &str, code: &str, status: u16) -> JsValue {
        let object = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &object,
            &JsValue::from_str("code"),
            &JsValue::from_str(code),
        );
        let _ = js_sys::Reflect::set(
            &object,
            &JsValue::from_str("message"),
            &JsValue::from_str(message),
        );
        let _ = js_sys::Reflect::set(
            &object,
            &JsValue::from_str("status"),
            &JsValue::from_f64(status as f64),
        );
        object.into()
    }

    /// The wire shape: the same member names the CLI and the HTTP adapters use.
    fn wire(resolved: &xeet_resolve::Resolved) -> serde_json::Value {
        serde_json::json!({
            "post": resolved.post,
            "links": resolved.links,
            "media": resolved.media,
            "source": resolved.source,
            "warnings": resolved.warnings,
        })
    }
}

pub use ffi::{
    call, missing_packs, packs_for, register_font, resolve, resolve_links, upstream_version,
};

// ---------------------------------------------------------------------------
// The request, and the safe core that answers it.
// ---------------------------------------------------------------------------

/// The request shape shared by every op.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Request {
    op: String,
    post: serde_json::Value,
    options: Option<serde_json::Value>,
    /// `sheet` only.
    widths: Vec<f32>,
    themes: Vec<String>,
    dpr: f32,
    /// `renderBatch` only: one options object per layout. An empty list falls
    /// back to `options`, so a caller can send either shape.
    batch: Vec<serde_json::Value>,
    /// Limit overrides, for the oracle and for tests. A server never sets it.
    limits: Option<String>,
}

impl Default for Request {
    fn default() -> Request {
        Request {
            op: String::new(),
            post: serde_json::Value::Null,
            options: None,
            widths: vec![320.0, 390.0, 600.0],
            themes: vec!["light".into(), "dark".into()],
            dpr: 2.0,
            batch: Vec::new(),
            limits: None,
        }
    }
}

/// Handles one request. Returns `(status, payload, report)`.
fn handle(op: u32, json: &str) -> (u8, Vec<u8>, Vec<u8>) {
    let req: Request = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            return (
                1,
                error_body("invalid_json", &format!("{e}")).into_bytes(),
                Vec::new(),
            )
        }
    };
    let limits = match req.limits.as_deref() {
        None | Some("standard") => Limits::standard(),
        Some("unlimited") => Limits::unlimited(),
        Some(other) => {
            let msg = format!("unknown limits policy `{other}`; expected standard or unlimited");
            return (
                1,
                error_body("invalid_limit_policy", &msg).into_bytes(),
                Vec::new(),
            );
        }
    };

    let post = match limits::post_from_value(req.post) {
        Ok(v) => v,
        Err(e) => {
            return (
                1,
                error_body("invalid_json", &e.to_string()).into_bytes(),
                Vec::new(),
            )
        }
    };
    let opts = match &req.options {
        Some(v) if !v.is_null() => match limits::options_from_value(v.clone()) {
            Ok(v) => v,
            Err(e) => {
                return (
                    1,
                    error_body("invalid_json", &e.to_string()).into_bytes(),
                    Vec::new(),
                )
            }
        },
        _ => RenderOptions::default(),
    };
    if let Err(problems) = limits::validate(&post, &opts, &limits) {
        let err = render::Error::Invalid(problems);
        return (1, err.to_json().to_string().into_bytes(), Vec::new());
    }

    // The `op` in the body is a convenience for JS callers that share one entry
    // point; the argument wins when it is not `OP_EXPLAIN`.
    let op = match (op, req.op.as_str()) {
        (OP_EXPLAIN, "render") => OP_RENDER,
        (OP_EXPLAIN, "sheet") => OP_SHEET,
        (OP_EXPLAIN, "renderRgba") => OP_RENDER_RGBA,
        (OP_EXPLAIN, "renderBatch") => OP_RENDER_BATCH,
        (o, _) => o,
    };

    match op {
        OP_RENDER => match render::render_with(&post, &opts, &limits) {
            Ok((png, report)) => (0, png, serde_json::to_vec(&report).unwrap_or_default()),
            Err(e) => (1, e.to_json().to_string().into_bytes(), Vec::new()),
        },
        OP_RENDER_RGBA => match render::render_rgba_with(&post, &opts, &limits) {
            Ok((pixmap, report)) => {
                let mut payload = Vec::with_capacity(8 + pixmap.rgba.len());
                payload.extend_from_slice(&pixmap.width.to_le_bytes());
                payload.extend_from_slice(&pixmap.height.to_le_bytes());
                payload.extend_from_slice(&pixmap.rgba);
                (0, payload, serde_json::to_vec(&report).unwrap_or_default())
            }
            Err(e) => (1, e.to_json().to_string().into_bytes(), Vec::new()),
        },
        OP_RENDER_BATCH => {
            // One post, several layouts. The post was parsed (and validated)
            // once above; `xeet-core` keeps its prepared images in a
            // process-wide cache, so every layout after the first reuses the
            // decoded media and its mip pyramid instead of decoding it again.
            let mut options = Vec::with_capacity(req.batch.len().max(1));
            if req.batch.is_empty() {
                options.push(opts);
            }
            for (index, value) in req.batch.iter().enumerate() {
                if value.is_null() {
                    options.push(RenderOptions::default());
                    continue;
                }
                let opt = match limits::options_from_value(value.clone()) {
                    Ok(opt) => opt,
                    Err(e) => {
                        let message = format!("batch[{index}]: {e}");
                        return (
                            1,
                            error_body("invalid_json", &message).into_bytes(),
                            Vec::new(),
                        );
                    }
                };
                if let Err(problems) = limits::validate(&post, &opt, &limits) {
                    let err = render::Error::Invalid(problems);
                    return (1, err.to_json().to_string().into_bytes(), Vec::new());
                }
                options.push(opt);
            }
            let mut items: Vec<(Vec<u8>, Vec<u8>, u32)> = Vec::with_capacity(options.len());
            for opt in &options {
                let started = now_us();
                match render::render_with(&post, opt, &limits) {
                    Ok((png, report)) => {
                        let took = now_us().saturating_sub(started).min(u64::from(u32::MAX)) as u32;
                        let report = serde_json::to_vec(&report).unwrap_or_default();
                        items.push((png, report, took));
                    }
                    Err(e) => return (1, e.to_json().to_string().into_bytes(), Vec::new()),
                }
            }
            (0, batch_payload(&items), Vec::new())
        }
        OP_SHEET => {
            let themes = match req
                .themes
                .iter()
                .map(|t| parse_theme(t))
                .collect::<Result<Vec<Theme>, String>>()
            {
                Ok(t) => t,
                Err(msg) => {
                    return (
                        1,
                        error_body("invalid_theme", &msg).into_bytes(),
                        Vec::new(),
                    )
                }
            };
            // Straight to the core: `sheet_with` is the same function
            // `sheet_json_with` would parse its way to, and it validates the
            // widths, the themes and the dpr itself.
            match render::sheet_with(&post, &req.widths, &themes, req.dpr, &limits) {
                Ok(png) => (0, png, Vec::new()),
                Err(e) => (1, e.to_json().to_string().into_bytes(), Vec::new()),
            }
        }
        _ => {
            let report = match render::explain_with(&post, &opts, &limits) {
                Ok(r) => r,
                Err(e) => return (1, e.to_json().to_string().into_bytes(), Vec::new()),
            };
            (
                0,
                serde_json::to_vec(&report).unwrap_or_default(),
                Vec::new(),
            )
        }
    }
}

fn parse_theme(s: &str) -> Result<Theme, String> {
    match s {
        "light" => Ok(Theme::Light),
        "dark" => Ok(Theme::Dark),
        other => Err(format!("unknown theme `{other}`; expected light or dark")),
    }
}

fn error_body(code: &str, message: &str) -> String {
    serde_json::json!({ "code": code, "message": message, "status": 400 }).to_string()
}

/// The `OP_RENDER_BATCH` payload: `count`, then per item its PNG length, its
/// report length, the microseconds it took, the PNG and the report.
///
/// ```text
/// u32 count
/// per item: u32 png_len, u32 report_len, u32 duration_us, png bytes, report bytes
/// ```
fn batch_payload(items: &[(Vec<u8>, Vec<u8>, u32)]) -> Vec<u8> {
    let total = 4 + items
        .iter()
        .map(|(png, report, _)| 12 + png.len() + report.len())
        .sum::<usize>();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (png, report, took) in items {
        buf.extend_from_slice(&(png.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(report.len() as u32).to_le_bytes());
        buf.extend_from_slice(&took.to_le_bytes());
        buf.extend_from_slice(png);
        buf.extend_from_slice(report);
    }
    buf
}

/// A host timestamp in microseconds, used to time one item of a batch.
///
/// This is the only clock read in the crate, and it exists because a page shows
/// "23 ms" under each preview: the number has to be the render call alone, and
/// only the side that owns the call can measure it. It never reaches a PNG or a
/// report, so the determinism gate is unaffected. The core stays clock-free;
/// outside WebAssembly (the unit tests) the duration is zero.
fn now_us() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(us) = host_now_us() {
            return us;
        }
    }
    0
}

/// `performance.now()`, when the host has it: sub-millisecond, and the same
/// clock the page's own timings use. `Date.now()` is the fallback.
#[cfg(target_arch = "wasm32")]
fn host_now_us() -> Option<u64> {
    use wasm_bindgen::{JsCast, JsValue};

    let global = js_sys::global();
    let performance = js_sys::Reflect::get(&global, &JsValue::from_str("performance")).ok()?;
    let now = js_sys::Reflect::get(&performance, &JsValue::from_str("now")).ok()?;
    let now = now.dyn_into::<js_sys::Function>().ok()?;
    let value = now.call0(&performance).ok()?;
    (value
        .as_f64()
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .map(|ms| (ms * 1000.0) as u64))
}

/// Builds the framed response described in the module docs.
fn frame(status: u8, payload: &[u8], report: &[u8]) -> Vec<u8> {
    let total = HEADER + payload.len() + report.len();
    let mut buf = Vec::with_capacity(total);
    buf.push(status);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(report.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(total as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf.extend_from_slice(report);
    buf
}

// ---------------------------------------------------------------------------
// Screenshot artifacts (xeet-capture), for hosts that capture in a browser.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureRequest {
    captured_at: String,
    method: String,
    declared_name: Option<String>,
    declared_type: Option<String>,
    context: Option<serde_json::Value>,
}

/// The pure core of `capture_screenshot`: callable natively, so it is tested
/// without a JS engine.
pub fn capture_screenshot_json(image: &[u8], request: &str) -> Result<String, String> {
    let r: CaptureRequest = serde_json::from_str(request).map_err(|e| format!("capture request: {e}"))?;
    let context = match &r.context {
        Some(v) => Some(serde_json::to_vec(v).map_err(|e| format!("capture context: {e}"))?),
        None => None,
    };
    let package = xeet_capture::image::capture(&xeet_capture::image::Capture {
        bytes: image,
        captured_at: r.captured_at,
        method: r.method,
        declared_name: r.declared_name,
        declared_type: r.declared_type,
        context: context.as_deref(),
    })?;
    let artifact = package.artifact()?;
    let assets: Vec<serde_json::Value> = artifact
        .assets
        .iter()
        .map(|a| {
            let role = artifact
                .provenance
                .inputs
                .iter()
                .find(|i| i.asset == a.sha256)
                .map(|i| i.role.clone())
                .unwrap_or_default();
            serde_json::json!({ "path": a.path, "sha256": a.sha256, "mediaType": a.media_type, "bytes": a.bytes, "role": role })
        })
        .collect();
    let context_text = context.map(|c| String::from_utf8(c).expect("serde_json writes UTF-8"));
    // The document's text is returned verbatim: its id hashes number tokens as written, so a caller must store and
    // send this string, never a re-serialization of the parsed value (a JS round trip turns 1.0 into 1).
    Ok(serde_json::json!({ "documentText": package.document.to_string(), "assets": assets, "contextText": context_text }).to_string())
}

/// A screenshot artifact's package from its verbatim document text and the bytes its inputs name.
fn screenshot_package(document: &str, image: &[u8], context_text: Option<&str>) -> Result<xeet_artifact::Package, String> {
    let doc: serde_json::Value = serde_json::from_str(document).map_err(|e| format!("document: {e}"))?;
    let artifact: xeet_artifact::Artifact =
        serde_json::from_value(doc.clone()).map_err(|e| format!("document is not an artifact: {e}"))?;
    let mut files = std::collections::BTreeMap::new();
    for input in &artifact.provenance.inputs {
        let Some(asset) = artifact.assets.iter().find(|a| a.sha256 == input.asset) else { continue };
        let bytes = match input.role.as_str() {
            xeet_capture::image::INPUT_ROLE => image.to_vec(),
            xeet_capture::image::CONTEXT_ROLE => match context_text {
                Some(t) => t.as_bytes().to_vec(),
                None => continue,
            },
            _ => continue,
        };
        files.insert(asset.path.clone(), bytes);
    }
    Ok(xeet_artifact::Package { document: doc, text: Some(document.to_string()), files })
}

/// The pure core of `verify_screenshot`.
pub fn verify_screenshot_json(document: &str, image: &[u8], context_text: Option<&str>) -> String {
    let fail = |m: String| serde_json::json!({ "ok": false, "checks": [{ "name": "input", "status": "fail", "detail": [m] }] }).to_string();
    match screenshot_package(document, image, context_text) {
        Ok(package) => serde_json::to_string(&xeet_artifact::verify::verify(&package, None)).unwrap_or_else(|e| fail(e.to_string())),
        Err(e) => fail(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OcrRequest {
    at: String,
    engine: Option<xeet_ocr::EngineInfo>,
    tsv: Option<String>,
    reading: Option<xeet_ocr::OcrResult>,
}

/// The pure core of `annotate_ocr`.
pub fn annotate_ocr_json(document: &str, image: &[u8], context_text: Option<&str>, request: &str) -> Result<String, String> {
    let r: OcrRequest = serde_json::from_str(request).map_err(|e| format!("ocr request: {e}"))?;
    let reading = match (r.engine, r.tsv, r.reading) {
        (Some(engine), Some(tsv), None) => xeet_ocr::parse_tsv(&tsv, engine).map_err(|e| format!("tsv: {e}"))?,
        (None, None, Some(reading)) => reading,
        _ => return Err("ocr request: give either engine and tsv, or reading".into()),
    };
    let original = screenshot_package(document, image, context_text)?;
    let report = xeet_artifact::verify::verify(&original, None);
    if !report.ok {
        return Err(format!(
            "the original does not verify against these bytes: {}",
            serde_json::to_string(&report).map_err(|e| e.to_string())?
        ));
    }
    let derived = xeet_ocr::derive(&original, reading, &r.at)?;
    let ocr = derived.document["semantic"]["elements"]
        .as_array()
        .and_then(|els| els.iter().find(|e| e["id"] == "ocr"))
        .ok_or("the derived artifact has no ocr element")?;
    Ok(serde_json::json!({
        "documentText": derived.document.to_string(),
        "id": derived.document["id"],
        "derivedFrom": derived.document["provenance"]["derivedFrom"],
        "lines": ocr["attrs"]["ocr.lines"],
        "uncertainLines": ocr["attrs"]["ocr.uncertainLines"],
    })
    .to_string())
}

/// The pure core of `understand_artifact`.
pub fn understand_artifact_json(document: &str, request: &str) -> Result<String, String> {
    let artifact: xeet_artifact::Artifact = serde_json::from_str(document).map_err(|e| format!("document is not an artifact: {e}"))?;
    let mut fields: serde_json::Value = serde_json::from_str(request).map_err(|e| format!("request: {e}"))?;
    let form = match fields.get("format").and_then(|v| v.as_str()) {
        Some("json") => xeet_artifact::project::Form::Json,
        _ => xeet_artifact::project::Form::Text,
    };
    if let serde_json::Value::Object(m) = &mut fields {
        m.remove("format");
    }
    let req: xeet_artifact::project::Request = serde_json::from_value(fields).map_err(|e| format!("request: {e}"))?;
    let (body, delivery) = xeet_artifact::project::render_with_report(&artifact, &req, form)?;
    let projection = match form {
        xeet_artifact::project::Form::Json => serde_json::from_str(&body).map_err(|e| e.to_string())?,
        xeet_artifact::project::Form::Text => serde_json::Value::String(body),
    };
    Ok(serde_json::json!({ "projection": projection, "delivery": delivery }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a frame back the way the JS wrapper does.
    fn parse(buf: &[u8]) -> (u8, Vec<u8>, Vec<u8>) {
        assert!(buf.len() >= HEADER, "a frame is at least the header");
        let status = buf[0];
        let payload_len = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
        let report_len = u32::from_le_bytes(buf[5..9].try_into().unwrap()) as usize;
        let total = u32::from_le_bytes(buf[9..13].try_into().unwrap()) as usize;
        assert_eq!(
            total,
            buf.len(),
            "the header's total length is the buffer's length"
        );
        assert_eq!(total, HEADER + payload_len + report_len);
        (
            status,
            buf[HEADER..HEADER + payload_len].to_vec(),
            buf[HEADER + payload_len..].to_vec(),
        )
    }

    const POST: &str =
        r#"{"text":"We just shipped the thing.","author":{"name":"Vercel","handle":"vercel"}}"#;

    fn run(op: u32, json: &str) -> (u8, Vec<u8>, Vec<u8>) {
        parse(&call(op, json))
    }

    #[test]
    fn explain_returns_a_report() {
        let (status, payload, report) =
            run(OP_EXPLAIN, &format!(r#"{{"op":"explain","post":{POST}}}"#));
        assert_eq!(status, 0);
        assert!(report.is_empty());
        let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert!(v["size"]["height"].as_f64().unwrap() > 0.0);
        assert!(v["ruleset"].as_str().unwrap().starts_with("x-web@"));
    }

    #[test]
    fn render_returns_png_and_report() {
        let (status, payload, report) = run(
            OP_RENDER,
            &format!(
                r#"{{"op":"render","post":{POST},"options":{{"width":390,"theme":"dark","dpr":3}}}}"#
            ),
        );
        assert_eq!(status, 0);
        assert_eq!(&payload[..8], b"\x89PNG\r\n\x1a\n");
        let v: serde_json::Value = serde_json::from_slice(&report).unwrap();
        assert_eq!(v["size"]["width"].as_f64().unwrap(), 390.0);
    }

    #[test]
    fn errors_are_json_not_traps() {
        let (status, payload, _) = run(OP_RENDER, "{not json");
        assert_eq!(status, 1);
        let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v["code"], "invalid_json");

        let items = vec![r#"{"width":1,"height":1}"#; 21].join(",");
        let bad = format!(r#"{{"post":{{"text":"x","media":[{items}]}}}}"#);
        let (status, payload, _) = run(OP_EXPLAIN, &bad);
        assert_eq!(status, 1);
        let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v["code"], "invalid_post");
        assert_eq!(v["status"], 400);
        assert!(v["fields"][0]["path"]
            .as_str()
            .unwrap()
            .starts_with("post.media"));
    }

    #[test]
    fn sheet_returns_a_png() {
        let (status, payload, _) = run(
            OP_SHEET,
            &format!(r#"{{"post":{POST},"widths":[320,390],"themes":["light","dark"],"dpr":1}}"#),
        );
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&payload));
        assert_eq!(&payload[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn the_op_in_the_body_can_pick_the_operation() {
        let (status, payload, _) = run(OP_EXPLAIN, &format!(r#"{{"op":"render","post":{POST}}}"#));
        assert_eq!(status, 0);
        assert_eq!(&payload[..4], b"\x89PNG");
    }

    #[test]
    fn rgba_returns_the_raw_raster_and_a_report() {
        let (status, payload, report) = run(
            OP_EXPLAIN,
            &format!(
                r#"{{"op":"renderRgba","post":{POST},"options":{{"width":390,"theme":"dark","dpr":1}}}}"#
            ),
        );
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&payload));
        let width = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        let height = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
        assert_eq!(width, 390);
        assert!(height > 0);
        // The header is the whole of it: 8 bytes, then exactly RGBA8.
        assert_eq!(payload.len(), 8 + width * height * 4);
        let v: serde_json::Value = serde_json::from_slice(&report).unwrap();
        assert_eq!(v["size"]["width"].as_f64().unwrap(), 390.0);
        assert!(v["size"]["height"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn batch_renders_every_option_set_from_one_parse() {
        let (status, payload, report) = run(
            OP_RENDER_BATCH,
            &format!(
                r#"{{"op":"renderBatch","post":{POST},"batch":[
                    {{"width":320,"theme":"light","dpr":1}},
                    {{"width":390,"theme":"dark","dpr":2}}
                ]}}"#
            ),
        );
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&payload));
        assert!(report.is_empty());
        let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        assert_eq!(count, 2);
        let mut widths = Vec::new();
        let mut at = 4;
        for _ in 0..count {
            let png_len = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap()) as usize;
            let report_len =
                u32::from_le_bytes(payload[at + 4..at + 8].try_into().unwrap()) as usize;
            // The duration is measured on the host clock; a non-wasm build
            // reports zero, which is why only its width is checked here.
            let _took = u32::from_le_bytes(payload[at + 8..at + 12].try_into().unwrap());
            at += 12;
            let png = &payload[at..at + png_len];
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            at += png_len;
            let v: serde_json::Value =
                serde_json::from_slice(&payload[at..at + report_len]).unwrap();
            widths.push(v["size"]["width"].as_f64().unwrap());
            at += report_len;
        }
        assert_eq!(at, payload.len(), "the payload is exactly its items");
        assert_eq!(widths, vec![320.0, 390.0]);
    }

    #[test]
    fn a_batch_without_options_renders_the_one_layout() {
        let (status, payload, _) = run(
            OP_RENDER_BATCH,
            &format!(r#"{{"op":"renderBatch","post":{POST},"options":{{"width":390,"dpr":1}}}}"#),
        );
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&payload));
        assert_eq!(u32::from_le_bytes(payload[0..4].try_into().unwrap()), 1);
    }

    #[test]
    fn a_batch_with_a_bad_layout_is_an_error_frame() {
        let (status, payload, _) = run(
            OP_RENDER_BATCH,
            &format!(r#"{{"op":"renderBatch","post":{POST},"batch":[{{"widht":390}}]}}"#),
        );
        assert_eq!(status, 1);
        let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v["code"], "invalid_json");
        assert!(
            v["message"].as_str().unwrap().starts_with("batch[0]"),
            "{v}"
        );
    }

    #[test]
    fn the_upstream_version_is_available() {
        let v = upstream_version();
        assert!(v.len() >= 8, "{v}");
        assert!(v.chars().all(|c| c.is_ascii_hexdigit()), "{v}");
    }

    #[test]
    fn an_empty_request_is_an_error_frame() {
        let (status, payload, _) = run(OP_EXPLAIN, "");
        assert_eq!(status, 1);
        assert!(serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["code"].is_string());
    }
}
