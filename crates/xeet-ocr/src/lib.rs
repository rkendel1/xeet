//! `xeet-ocr`: local OCR as a `vision.read` provider.
//!
//! An on-device engine reads text, positions and confidence from real image
//! bytes; the provider turns that into an annotation whose every element is
//! `inferred` (recognition can be wrong), attributed to the engine, its
//! version and configuration, and the time. The original image is never
//! modified: the annotation is applied by the engine as a new artifact derived
//! from the original.
//!
//! Engines plug in through [`Backend`]:
//!
//! - [`TesseractBackend`] runs the `tesseract` executable (verified in this
//!   repository's tests, Linux and macOS).
//! - [`HelperBackend`] runs any executable that speaks the `xeet-ocr/1`
//!   protocol — how Apple's Vision framework is reached on macOS
//!   (`apps/macos`, `xeet-vision-ocr`), without linking Objective-C into Rust.
//!
//! Scope: text recognition only. OCR does not describe scenes, objects or
//! relationships, and this provider does not claim to.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use xeet_engine::contract::{self, modality, Constraints, Contract, Manifest, Mode};
use xeet_engine::{schema, Outcome, Provider, Session};

pub const PROTOCOL: &str = "xeet-ocr/1";
/// The scope a host grants to let images be read by an on-device engine.
/// Distinct from `vision:read` (which may send pixels to a remote model).
pub const SCOPE: &str = "ocr:local";

// ---------------------------------------------------------------------------
// The xeet-ocr/1 protocol
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    pub name: String,
    pub version: String,
    pub languages: Vec<String>,
    /// Exact settings that affect output (mode, level, language correction…).
    pub config: Value,
    /// Media types the engine can decode, as the engine reports them.
    pub formats: Vec<String>,
    pub platform: String,
    /// What this engine does not do (e.g. orientation detection).
    #[serde(default)]
    pub unsupported: Vec<String>,
}

/// A box in normalized image coordinates: origin top-left, x right, y down,
/// each in 0..=1 of the image's width or height.
pub type NBox = [f64; 4];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Word {
    pub text: String,
    #[serde(rename = "box")]
    pub bx: NBox,
    /// 0..=1, as the engine reports it.
    pub confidence: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub text: String,
    #[serde(rename = "box")]
    pub bx: NBox,
    pub confidence: f64,
    /// Reading order, as the engine determines it (see `readingOrder`).
    pub order: u32,
    #[serde(default)]
    pub words: Vec<Word>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrResult {
    pub protocol: String,
    pub engine: EngineInfo,
    pub image: ImageInfo,
    /// How `order` was determined: `engine-layout` or `geometric`.
    pub reading_order: String,
    pub lines: Vec<Line>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    /// `up` when the engine assumed or detected upright text; `unknown` otherwise.
    pub orientation: String,
}

pub trait Backend {
    fn engine(&self) -> &EngineInfo;
    /// Recognizes text in the image file at `path`.
    fn recognize(&self, path: &Path) -> Result<OcrResult, String>;
}

// ---------------------------------------------------------------------------
// Tesseract
// ---------------------------------------------------------------------------

pub struct TesseractBackend {
    pub binary: PathBuf,
    info: EngineInfo,
}

impl TesseractBackend {
    /// Probes the installed engine; fails if it is not there.
    pub fn new(binary: Option<PathBuf>, languages: &[&str]) -> Result<Self, String> {
        let binary = binary.unwrap_or_else(|| PathBuf::from("tesseract"));
        let out = Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|e| format!("tesseract is not available ({}): {e}", binary.display()))?;
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        let version = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("tesseract "))
            .ok_or("unrecognized `tesseract --version` output")?
            .trim()
            .to_string();
        let libs = text.to_lowercase();
        let mut formats = vec![];
        for (lib, mt) in [
            ("libpng", "image/png"),
            ("libjpeg", "image/jpeg"),
            ("libgif", "image/gif"),
            ("libwebp", "image/webp"),
            ("libtiff", "image/tiff"),
        ] {
            if libs.contains(lib) {
                formats.push(mt.to_string());
            }
        }
        let leptonica = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("leptonica-"))
            .unwrap_or("unknown")
            .to_string();
        Ok(TesseractBackend {
            binary,
            info: EngineInfo {
                name: "tesseract".into(),
                version,
                languages: languages.iter().map(|s| s.to_string()).collect(),
                config: json!({"psm": 3, "oem": "default", "output": "tsv", "leptonica": leptonica, "threads": 1}),
                formats,
                platform: std::env::consts::OS.into(),
                unsupported: vec![
                    "orientation detection (page segmentation mode 3 assumes upright text)".into(),
                    "handwriting".into(),
                ],
            },
        })
    }
}

impl Backend for TesseractBackend {
    fn engine(&self) -> &EngineInfo {
        &self.info
    }

    fn recognize(&self, path: &Path) -> Result<OcrResult, String> {
        let out = Command::new(&self.binary)
            .arg(path)
            .arg("-")
            .args(["-l", &self.info.languages.join("+"), "--psm", "3", "tsv"])
            // One thread: the same input gives the same output.
            .env("OMP_THREAD_LIMIT", "1")
            .output()
            .map_err(|e| format!("tesseract: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "tesseract failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_tsv(&String::from_utf8_lossy(&out.stdout), self.info.clone())
    }
}

/// The column header Tesseract's TSV renderer writes. The library call behind
/// it (`GetTSVText`, which tesseract.js returns as `data.tsv`) omits it.
pub const TSV_HEADER: &str =
    "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext";

/// Parses Tesseract's TSV into lines in the engine's reading order. Rows
/// without the header line are read with [`TSV_HEADER`].
pub fn parse_tsv(tsv: &str, engine: EngineInfo) -> Result<OcrResult, String> {
    let first = tsv.lines().next().ok_or("empty TSV")?;
    let owned;
    let tsv = if first.starts_with("level\t") {
        tsv
    } else {
        owned = format!("{TSV_HEADER}\n{tsv}");
        &owned
    };
    let mut rows = tsv.lines();
    let header: Vec<&str> = rows.next().ok_or("empty TSV")?.split('\t').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .ok_or(format!("TSV has no {name} column"))
    };
    let (lv, bl, pa, li, le, to, wi, he, co, tx) = (
        col("level")?,
        col("block_num")?,
        col("par_num")?,
        col("line_num")?,
        col("left")?,
        col("top")?,
        col("width")?,
        col("height")?,
        col("conf")?,
        col("text")?,
    );
    let (mut w, mut h) = (0f64, 0f64);
    // (block, paragraph, line) → words as (text, pixel box, confidence).
    type Words = Vec<(String, [f64; 4], f64)>;
    let mut groups: Vec<((u32, u32, u32), Words)> = vec![];
    for r in rows {
        let f: Vec<&str> = r.split('\t').collect();
        if f.len() < header.len() {
            continue;
        }
        let n = |i: usize| f[i].trim().parse::<f64>().unwrap_or(0.0);
        match f[lv] {
            "1" => {
                w = n(wi);
                h = n(he);
            }
            "5" => {
                let text = f[tx].trim();
                if text.is_empty() || n(co) < 0.0 {
                    continue;
                }
                let key = (n(bl) as u32, n(pa) as u32, n(li) as u32);
                let word = (
                    text.to_string(),
                    [n(le), n(to), n(wi), n(he)],
                    n(co) / 100.0,
                );
                match groups.last_mut() {
                    Some((k, ws)) if *k == key => ws.push(word),
                    _ => groups.push((key, vec![word])),
                }
            }
            _ => {}
        }
    }
    if w <= 0.0 || h <= 0.0 {
        return Err("TSV has no page dimensions".into());
    }
    let norm = |b: [f64; 4]| -> NBox { [r4(b[0] / w), r4(b[1] / h), r4(b[2] / w), r4(b[3] / h)] };
    let lines = groups
        .into_iter()
        .enumerate()
        .map(|(i, (_, ws))| {
            let x0 = ws.iter().map(|x| x.1[0]).fold(f64::MAX, f64::min);
            let y0 = ws.iter().map(|x| x.1[1]).fold(f64::MAX, f64::min);
            let x1 = ws.iter().map(|x| x.1[0] + x.1[2]).fold(0.0, f64::max);
            let y1 = ws.iter().map(|x| x.1[1] + x.1[3]).fold(0.0, f64::max);
            Line {
                text: ws
                    .iter()
                    .map(|x| x.0.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                bx: norm([x0, y0, x1 - x0, y1 - y0]),
                confidence: r4(ws.iter().map(|x| x.2).fold(1.0, f64::min)),
                order: i as u32,
                words: ws
                    .iter()
                    .map(|x| Word {
                        text: x.0.clone(),
                        bx: norm(x.1),
                        confidence: r4(x.2),
                    })
                    .collect(),
            }
        })
        .collect();
    Ok(OcrResult {
        protocol: PROTOCOL.into(),
        engine,
        image: ImageInfo {
            width: w as u32,
            height: h as u32,
            orientation: "up".into(),
        },
        reading_order: "engine-layout".into(),
        lines,
        warnings: vec![],
    })
}

fn r4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------------------
// Helper executables (Apple Vision on macOS)
// ---------------------------------------------------------------------------

/// An executable that speaks `xeet-ocr/1`: `helper --identify` prints
/// [`EngineInfo`] as JSON; `helper IMAGE` prints an [`OcrResult`].
pub struct HelperBackend {
    pub path: PathBuf,
    info: EngineInfo,
}

impl HelperBackend {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        let out = Command::new(&path)
            .arg("--identify")
            .output()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{} --identify failed: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let info: EngineInfo = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("{} --identify: {e}", path.display()))?;
        Ok(HelperBackend { path, info })
    }
}

impl Backend for HelperBackend {
    fn engine(&self) -> &EngineInfo {
        &self.info
    }

    fn recognize(&self, image: &Path) -> Result<OcrResult, String> {
        let out = Command::new(&self.path)
            .arg(image)
            .output()
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{} failed: {}",
                self.path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let r: OcrResult = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("{}: not {PROTOCOL}: {e}", self.path.display()))?;
        check(&r).map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(r)
    }
}

/// Checks a result that crossed a process or host boundary: its protocol,
/// and every line's box and confidence within 0..1.
pub fn check(r: &OcrResult) -> Result<(), String> {
    if r.protocol != PROTOCOL {
        return Err(format!("protocol {} is not {PROTOCOL}", r.protocol));
    }
    for l in &r.lines {
        let ok = l.bx.iter().all(|v| (-0.001..=1.001).contains(v))
            && (0.0..=1.0).contains(&l.confidence);
        if !ok {
            return Err(format!(
                "line {:?} has a box or confidence outside 0..1",
                l.text
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// Words below this confidence make their line `uncertain`.
pub const UNCERTAIN_BELOW: f64 = 0.6;

pub struct OcrProvider<B: Backend> {
    pub backend: B,
    manifest: Manifest,
}

fn digest(v: &Value) -> String {
    xeet_artifact::canon::sha256_id(xeet_artifact::canon::canonical(v).as_bytes())
}

impl<B: Backend> OcrProvider<B> {
    pub fn new(backend: B) -> Self {
        let e = backend.engine().clone();
        let (id, version) = identity(&e);
        let mut m = contract::manifest();
        m.provider = contract::Provider {
            id,
            name: format!("Local OCR ({} {})", e.name, e.version),
            version,
            description: format!("Reads text, positions and confidence from images on this device with {} {} ({}); results are inference. Text only: no scene understanding.", e.name, e.version, e.platform),
        };
        m.sources.clear();
        m.non_capabilities = vec![contract::NonCapability {
            outcomes: vec!["describe a picture".into(), "what objects are in this image".into(), "understand a scene".into(), "explain a chart".into()],
            why: "OCR recognizes text and where it is; it does not interpret scenes, objects or relationships".into(),
            alternative: "a separately declared scene-interpretation provider, whose output is inference".into(),
        }];
        m.limitations = vec![format!(
            "text recognition only; languages: {}",
            e.languages.join(", ")
        )];
        m.limitations
            .extend(e.unsupported.iter().map(|u| format!("unsupported: {u}")));
        m.authority.scopes = vec![contract::Scope {
            id: SCOPE.into(),
            meaning: "read images with an on-device OCR engine; nothing leaves this machine".into(),
        }];
        m.constraints = Constraints {
            network: "none".into(),
            deterministic: true,
            max_input_bytes: 32 << 20,
            notes: vec!["deterministic for a given engine version, configuration and input".into()],
        };
        m.capabilities = vec![Contract {
            id: "vision.read".into(),
            version: "0.1".into(),
            title: format!("Read text in an image ({} {}, on device)", e.name, e.version),
            summary: "Recognize text lines and words with positions (normalized and in pixels) and the engine's confidence; low-confidence and edge-touching text is marked uncertain.".into(),
            description: "Local, offline OCR. Every element is inferred, attributed to the engine, its version, configuration and time, and points back to the image it was read from.".into(),
            outcomes: vec!["read text from an image".into(), "ocr a screenshot".into(), "ocr an arbitrary image".into(), "extract text and positions from an image".into(), "find where text is in a screenshot".into()],
            input_modalities: e.formats.clone(),
            modes: vec![Mode { output: vec![modality::JSON.into()], select: json!({}), note: "annotation of inferred text elements".into() }],
            input_schema: json!({"type": "object", "required": ["image", "parent"], "properties": {
                "artifact": {"type": "string"}, "parent": {"type": "string"},
                "image": {"type": "object", "required": ["mediaType", "dataBase64"], "properties": {"mediaType": {"type": "string"}, "dataBase64": {"type": "string"}}}}}),
            output_schema: json!({"type": "object", "required": ["annotation", "engine"], "properties": {
                "annotation": {"type": "object", "required": ["parent", "by", "method", "at"]},
                "engine": {"type": "object"}, "lines": {"type": "integer"}, "uncertain": {"type": "integer"}}}),
            effects: "none".into(),
            consequential: false,
            authority: vec![SCOPE.into()],
            preconditions: vec![format!("the image is one of {:?}", e.formats)],
            guarantees: vec![
                "the image bytes are read, never modified".into(),
                "every element is inferred and attributed; confidence is the engine's, unaltered".into(),
                "no network access".into(),
            ],
            limitations: m.limitations.clone(),
            constraints: m.constraints.clone(),
            errors: vec!["invalid_input".into(), "unauthorized".into(), "unsupported".into(), "engine_error".into()],
        }];
        OcrProvider {
            backend,
            manifest: m,
        }
    }
}

/// The provider id and version that attribute an engine's readings. The
/// version names the engine *and* its configuration, so a cached reading is
/// reused only when both are unchanged.
pub fn identity(e: &EngineInfo) -> (String, String) {
    let cfg =
        digest(&json!({"engine": e, "uncertainBelow": UNCERTAIN_BELOW, "protocol": PROTOCOL}));
    (
        format!("xeet-ocr:{}", e.name),
        format!("{}+{}", e.version, &cfg[7..19]),
    )
}

/// Holds a reading to the dimensions in the image's header, warning when the
/// engine reported others.
pub fn reconcile(r: &mut OcrResult, width: u32, height: u32) {
    if (r.image.width, r.image.height) != (width, height) {
        r.warnings.push(format!(
            "engine reported {}x{}, the file header says {}x{}",
            r.image.width, r.image.height, width, height
        ));
        r.image.width = width;
        r.image.height = height;
    }
}

/// Applies a reading made outside this process (a browser's WebAssembly
/// engine) to a screenshot artifact, exactly as the `vision.read` provider
/// and `representation.annotate` do: a new artifact derived from `package`,
/// whose image and context assets are carried over byte for byte. Pure: `at`
/// is the reading's time, supplied by the caller.
pub fn derive(
    package: &xeet_artifact::Package,
    mut reading: OcrResult,
    at: &str,
) -> Result<xeet_artifact::Package, String> {
    check(&reading)?;
    let e = &reading.engine;
    if [&e.name, &e.version, &e.platform]
        .iter()
        .any(|f| f.trim().is_empty())
        || e.languages.is_empty()
    {
        return Err("the engine must name itself, its version, platform and languages".into());
    }
    for w in reading.lines.iter().flat_map(|l| &l.words) {
        if !w.bx.iter().all(|v| (-0.001..=1.001).contains(v))
            || !(0.0..=1.0).contains(&w.confidence)
        {
            return Err(format!(
                "word {:?} has a box or confidence outside 0..1",
                w.text
            ));
        }
    }
    let artifact = package.artifact()?;
    let image = artifact
        .provenance
        .inputs
        .iter()
        .find(|i| i.role == xeet_capture::image::INPUT_ROLE)
        .and_then(|i| package.asset_bytes(&i.asset))
        .ok_or("the artifact has no source image")?;
    let sniffed =
        xeet_capture::image::sniff(image).ok_or("the source image is not a recognized image")?;
    reconcile(&mut reading, sniffed.width, sniffed.height);
    let (id, version) = identity(&reading.engine);
    let source = xeet_engine::store::uri(&artifact.id);
    let ann = to_annotation(&reading, "image", (&id, &version), at, Some(&source));
    let ann: xeet_artifact::annotate::Annotation =
        serde_json::from_value(ann).map_err(|e| e.to_string())?;
    xeet_artifact::annotate::annotate(package, &ann)
}

/// The annotation for a recognition result: one inferred element per line.
pub fn to_annotation(
    r: &OcrResult,
    parent: &str,
    by: (&str, &str),
    at: &str,
    source: Option<&str>,
) -> Value {
    let (w, h) = (r.image.width as f64, r.image.height as f64);
    let px = |b: &NBox| json!([{"view": "v0", "x": (b[0] * w * 100.0).round() / 100.0, "y": (b[1] * h * 100.0).round() / 100.0, "w": (b[2] * w * 100.0).round() / 100.0, "h": (b[3] * h * 100.0).round() / 100.0}]);
    let mut children = vec![];
    let mut elements = vec![];
    let mut uncertain = 0;
    for l in &r.lines {
        let id = format!("ocr.line.{}", l.order);
        let mut attrs = json!({
            "ocr.confidence": l.confidence,
            "ocr.order": l.order,
            "ocr.boxNormalized": l.bx,
            "ocr.words": l.words.iter().map(|x| json!({"text": x.text, "confidence": x.confidence, "box": x.bx})).collect::<Vec<_>>(),
        });
        if let Some(s) = source {
            attrs["ocr.source"] = json!(s);
        }
        let mut doubts = vec![];
        let low: Vec<String> = l
            .words
            .iter()
            .filter(|x| x.confidence < UNCERTAIN_BELOW)
            .map(|x| format!("{:?} {:.2}", x.text, x.confidence))
            .collect();
        if !low.is_empty() {
            doubts.push(format!("low-confidence words: {}", low.join(", ")));
        }
        let eps = 1.5;
        let edges: Vec<&str> = [
            (l.bx[0] * w <= eps, "left"),
            (l.bx[1] * h <= eps, "top"),
            ((l.bx[0] + l.bx[2]) * w >= w - eps, "right"),
            ((l.bx[1] + l.bx[3]) * h >= h - eps, "bottom"),
        ]
        .into_iter()
        .filter_map(|(hit, side)| hit.then_some(side))
        .collect();
        if !edges.is_empty() {
            doubts.push(format!(
                "touches the {} edge of the image: text may be cut off",
                edges.join(" and ")
            ));
        }
        if !doubts.is_empty() {
            uncertain += 1;
            attrs["ocr.uncertain"] = json!(doubts.join("; "));
        }
        children.push(json!(id));
        elements.push(json!({"id": id, "kind": "text", "role": "ocr.line", "text": l.text, "visible": true, "basis": "inferred", "bounds": px(&l.bx), "attrs": attrs}));
    }
    let summary = if r.lines.is_empty() {
        format!(
            "No text recognized by {} {}",
            r.engine.name, r.engine.version
        )
    } else {
        format!(
            "Text recognized by {} {} ({}): {} lines, {} uncertain",
            r.engine.name,
            r.engine.version,
            r.engine.languages.join("+"),
            r.lines.len(),
            uncertain
        )
    };
    let mut root = json!({"id": "ocr", "kind": "list", "role": "ocr.text", "text": summary, "visible": true, "basis": "inferred", "children": children,
        "attrs": {"ocr.engine": r.engine, "ocr.readingOrder": r.reading_order, "ocr.orientation": r.image.orientation, "ocr.lines": r.lines.len(), "ocr.uncertainLines": uncertain}});
    if !r.warnings.is_empty() {
        root["attrs"]["ocr.warnings"] = json!(r.warnings);
    }
    let mut all = vec![root];
    all.extend(elements);
    json!({
        "parent": parent,
        "elements": all,
        "facts": [],
        "by": {"name": by.0, "version": by.1},
        "method": format!("{} {} on {}; config sha256 {}", r.engine.name, r.engine.version, r.engine.platform, &digest(&r.engine.config)[7..]),
        "at": at,
    })
}

impl<B: Backend> Provider for OcrProvider<B> {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn invoke(&self, cap: &str, input: &Value, session: &Session) -> Outcome {
        let c = &self.manifest.capabilities[0];
        let mut out = Outcome {
            capability: cap.into(),
            version: c.version.clone(),
            provider: self.manifest.provider.id.clone(),
            provider_version: self.manifest.provider.version.clone(),
            ok: false,
            result: None,
            error: None,
            input_digest: digest(input),
            result_digest: None,
            artifacts: vec![],
            authority_checked: c.authority.clone(),
        };
        let fail = |code: &str, m: String| {
            Some(xeet_engine::Error {
                code: code.into(),
                message: m,
                details: Value::Null,
            })
        };
        if cap != c.id {
            out.error = fail("not_found", format!("no capability {cap}"));
            return out;
        }
        let problems = schema::validate(&c.input_schema, input);
        if !problems.is_empty() {
            out.error = fail("invalid_input", format!("{problems:?}"));
            return out;
        }
        if !session.grants.contains(SCOPE) {
            out.error = fail("unauthorized", format!("needs {SCOPE}"));
            return out;
        }
        let Some(bytes) = xeet_engine::unb64(input["image"]["dataBase64"].as_str().unwrap_or(""))
        else {
            out.error = fail("invalid_input", "image.dataBase64 is not base64".into());
            return out;
        };
        let Some(sniffed) = xeet_capture::image::sniff(&bytes) else {
            out.error = fail("unsupported", "not a recognized image".into());
            return out;
        };
        if !c.input_modalities.iter().any(|m| m == sniffed.media_type) {
            out.error = fail(
                "unsupported",
                format!(
                    "{} cannot decode {}",
                    self.backend.engine().name,
                    sniffed.media_type
                ),
            );
            return out;
        }
        // The engine reads a file; write a private temporary copy and remove it.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("xeet-ocr-{}-{nanos}.img", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, &bytes) {
            out.error = fail("engine_error", e.to_string());
            return out;
        }
        let r = self.backend.recognize(&tmp);
        let _ = std::fs::remove_file(&tmp);
        let mut r = match r {
            Ok(r) => r,
            Err(e) => {
                out.error = fail("engine_error", e);
                return out;
            }
        };
        reconcile(&mut r, sniffed.width, sniffed.height);
        let at = session.now.clone().unwrap_or_default();
        let m = &self.manifest.provider;
        let ann = to_annotation(
            &r,
            input["parent"].as_str().unwrap_or("image"),
            (&m.id, &m.version),
            &at,
            input["artifact"].as_str(),
        );
        let uncertain = ann["elements"][0]["attrs"]["ocr.uncertainLines"].clone();
        let result = json!({"annotation": ann, "engine": r.engine, "lines": r.lines.len(), "uncertain": uncertain});
        out.result_digest = Some(digest(&result));
        out.result = Some(result);
        out.ok = true;
        out
    }
}
