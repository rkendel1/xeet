//! Browser OCR: a reading made by a WebAssembly engine (tesseract.js) becomes the same derived artifact the native
//! `vision.read` path makes. Run natively: these are the pure functions the WASM build exports.

use serde_json::{json, Value};
use xeet_engine::{Provider, Session};
use xeet_ocr::{OcrProvider, TesseractBackend};
use xeet_wasm::{annotate_ocr_json, capture_screenshot_json, understand_artifact_json, verify_screenshot_json};

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/gateway/invoices.png");
const CONTEXT: &str = include_str!("../../../tests/fixtures/capture/context-region-retina.json");
/// `tesseract invoices.png - -l eng --psm 3 tsv` (5.3.4), exactly as `TesseractBackend` runs it.
const TSV: &str = include_str!("../../../tests/fixtures/capture/invoices-tesseract-5.3.4.tsv");
const AT: &str = "2026-11-25T17:00:05Z";

/// The original screenshot artifact: `(documentText, contextText)`.
fn original() -> (String, String) {
    let request = json!({"capturedAt": "2026-11-25T17:00:00Z", "method": "getDisplayMedia", "context": serde_json::from_str::<Value>(CONTEXT).unwrap()});
    let out: Value = serde_json::from_str(&capture_screenshot_json(PNG, &request.to_string()).unwrap()).unwrap();
    (out["documentText"].as_str().unwrap().into(), out["contextText"].as_str().unwrap().into())
}

/// tesseract.js returns `GetTSVText` as `data.tsv`: the rows without the header line.
fn browser_tsv() -> String {
    TSV.split_once('\n').unwrap().1.to_string()
}

fn wasm_engine() -> Value {
    json!({"name": "tesseract", "version": "5.3.4", "languages": ["eng"], "config": {"psm": 3, "oem": "default", "output": "tsv", "build": "tesseract.js"},
        "formats": ["image/png"], "platform": "wasm", "unsupported": ["orientation detection (page segmentation mode 3 assumes upright text)", "handwriting"]})
}

fn annotate(document: &str, context: &str, request: Value) -> Result<Value, String> {
    annotate_ocr_json(document, PNG, Some(context), &request.to_string()).map(|s| serde_json::from_str(&s).unwrap())
}

fn derived() -> (String, String, Value) {
    let (document, context) = original();
    let out = annotate(&document, &context, json!({"at": AT, "engine": wasm_engine(), "tsv": browser_tsv()})).unwrap();
    (document, context, out)
}

fn elements(document: &str) -> Vec<Value> {
    let d: Value = serde_json::from_str(document).unwrap();
    d["semantic"]["elements"].as_array().unwrap().iter().filter(|e| e["id"].as_str().unwrap().starts_with("ocr")).cloned().collect()
}

#[test]
fn a_browser_reading_gives_the_native_elements_boxes_confidences_and_doubts() {
    let (document, context, out) = derived();
    let browser = elements(out["documentText"].as_str().unwrap());

    // The native path: the vision.read provider with the installed Tesseract, then representation.annotate.
    let backend = match TesseractBackend::new(None, &["eng"]) {
        Ok(t) => t,
        Err(e) if std::env::var("XEET_REQUIRE_TESSERACT").as_deref() == Ok("1") => panic!("{e}"),
        Err(e) => return eprintln!("SKIPPED (no tesseract): {e}"),
    };
    let provider = OcrProvider::new(backend);
    let original: Value = serde_json::from_str(&document).unwrap();
    let uri = format!("artifact://sha256/{}", original["id"].as_str().unwrap().trim_start_matches("sha256:"));
    let session = Session { grants: [xeet_ocr::SCOPE.to_string()].into(), read_roots: vec![], now: Some(AT.into()) };
    let o = provider.invoke("vision.read", &json!({"artifact": uri, "parent": "image", "image": {"mediaType": "image/png", "dataBase64": xeet_engine::b64(PNG)}}), &session);
    assert!(o.ok, "{:?}", o.error);
    let ann: xeet_artifact::annotate::Annotation = serde_json::from_value(o.result.unwrap()["annotation"].clone()).unwrap();
    let mut files = std::collections::BTreeMap::new();
    let a: xeet_artifact::Artifact = serde_json::from_str(&document).unwrap();
    for asset in &a.assets {
        let bytes = if asset.media_type == "image/png" { PNG.to_vec() } else { context.as_bytes().to_vec() };
        files.insert(asset.path.clone(), bytes);
    }
    let package = xeet_artifact::Package { document: original, text: Some(document.clone()), files };
    let native = xeet_artifact::annotate::annotate(&package, &ann).unwrap();
    let native = elements(&native.document.to_string());

    assert_eq!(native.len(), browser.len());
    assert_eq!(native.len(), 15, "the root and 14 lines");
    for (n, b) in native.iter().zip(&browser) {
        let (mut n, mut b) = (n.clone(), b.clone());
        if n["id"] == "ocr" {
            // The one legitimate difference: the engine's own description (platform, build).
            assert_eq!(b["attrs"]["ocr.engine"]["platform"], "wasm");
            n["attrs"]["ocr.engine"] = Value::Null;
            b["attrs"]["ocr.engine"] = Value::Null;
        }
        assert_eq!(n, b, "texts, pixel boxes (1/64), confidences, words, order and doubts are the native ones");
    }
    let uncertain = browser.iter().filter(|e| e["attrs"].get("ocr.uncertain").is_some()).count();
    assert_eq!((uncertain, out["uncertainLines"].as_u64()), (5, Some(5)));
    assert_eq!(out["lines"], 14);
}

#[test]
fn the_reading_is_a_separate_artifact_that_verifies_against_the_original_bytes() {
    let (document, context, out) = derived();
    let derived_text = out["documentText"].as_str().unwrap();
    let ok = |r: String| serde_json::from_str::<Value>(&r).unwrap()["ok"] == true;
    assert!(ok(verify_screenshot_json(derived_text, PNG, Some(&context))), "{}", verify_screenshot_json(derived_text, PNG, Some(&context)));
    assert!(ok(verify_screenshot_json(&document, PNG, Some(&context))), "the original is untouched and still verifies");
    assert_eq!(original().0, document);

    let o: Value = serde_json::from_str(&document).unwrap();
    let d: Value = serde_json::from_str(derived_text).unwrap();
    assert_ne!(o["id"], d["id"]);
    assert_eq!(d["provenance"]["derivedFrom"], o["id"]);
    assert_eq!((&out["id"], &out["derivedFrom"]), (&d["id"], &o["id"]));
    assert_eq!(d["assets"], o["assets"], "same source-image and capture-context assets");
    assert_eq!(d["provenance"]["inputs"], o["provenance"]["inputs"]);
    assert!(elements(&document).is_empty(), "the original has no reading");

    let step = d["provenance"]["transformations"].as_array().unwrap().last().unwrap().clone();
    let image_sha = &o["provenance"]["inputs"][0]["asset"];
    assert_eq!(step["tool"]["name"], "xeet-ocr:tesseract");
    assert!(step["tool"]["version"].as_str().unwrap().starts_with("5.3.4+"), "engine version + configuration digest");
    assert_eq!((step["at"].as_str(), step["deterministic"].as_bool()), (Some(AT), Some(false)));
    assert_eq!(step["inputs"], json!([image_sha]), "names the source image's hash");
    let why = d["limitations"]["inferred"][0]["why"].as_str().unwrap();
    assert!(why.contains("tesseract 5.3.4 on wasm; config sha256 ") && why.contains(AT), "{why}");
    let line = &elements(derived_text)[1];
    assert_eq!(line["basis"], "inferred");
    assert_eq!(line["attrs"]["ocr.source"], format!("artifact://sha256/{}", o["id"].as_str().unwrap().trim_start_matches("sha256:")));

    // Same reading, same artifact: the header line and the xeet-ocr/1 form change nothing.
    let with_header = annotate(&document, &context, json!({"at": AT, "engine": wasm_engine(), "tsv": TSV})).unwrap();
    assert_eq!(with_header["documentText"], out["documentText"]);
    let reading = xeet_ocr::parse_tsv(TSV, serde_json::from_value(wasm_engine()).unwrap()).unwrap();
    let as_protocol = annotate(&document, &context, json!({"at": AT, "reading": reading})).unwrap();
    assert_eq!(as_protocol["documentText"], out["documentText"]);
}

#[test]
fn a_text_only_model_reads_the_lines_as_inferred_with_the_engine_named() {
    let (_, _, out) = derived();
    let p: Value = serde_json::from_str(&understand_artifact_json(out["documentText"].as_str().unwrap(), r#"{"consumer":"text"}"#).unwrap()).unwrap();
    let text = p["projection"].as_str().unwrap();
    assert!(text.contains("Invoices") && text.contains("!inferred"), "{text}");
    let invoices = text.lines().find(|l| l.contains("Invoices") && l.contains("!inferred"));
    assert!(invoices.is_some(), "the recognized line is marked inferred: {text}");
    assert!(text.contains("tesseract 5.3.4"), "the engine is named: {text}");
}

#[test]
fn a_malformed_reading_or_the_wrong_bytes_are_refused() {
    let (document, context) = original();
    let refuse = |request: Value| assert!(annotate(&document, &context, request.clone()).is_err(), "{request}");
    refuse(json!({"at": AT, "engine": wasm_engine()}));
    refuse(json!({"at": AT, "engine": wasm_engine(), "tsv": browser_tsv(), "surprise": 1}));
    refuse(json!({"at": "", "engine": wasm_engine(), "tsv": browser_tsv()}));
    refuse(json!({"at": AT, "engine": wasm_engine(), "tsv": ""}));
    refuse(json!({"at": AT, "engine": wasm_engine(), "tsv": "level\tleft\n5\t1"}));
    refuse(json!({"at": AT, "engine": wasm_engine(), "tsv": "5\t1\t1\t1\t1\t1\t10\t10\t40\t12\t96.5\tword"}));
    let mut nameless = wasm_engine();
    nameless["platform"] = json!("");
    refuse(json!({"at": AT, "engine": nameless, "tsv": browser_tsv()}));
    let mut reading = serde_json::to_value(xeet_ocr::parse_tsv(TSV, serde_json::from_value(wasm_engine()).unwrap()).unwrap()).unwrap();
    reading["lines"][0]["confidence"] = json!(96.5);
    refuse(json!({"at": AT, "reading": reading.clone()}));
    reading["lines"][0]["confidence"] = json!(0.9);
    reading["lines"][1]["words"][0]["box"] = json!([0.1, 0.1, 2.0, 0.1]);
    refuse(json!({"at": AT, "reading": reading.clone()}));
    reading["lines"][1]["words"][0]["box"] = json!([0.1, 0.1, 0.1, 0.1]);
    reading["protocol"] = json!("xeet-ocr/0");
    refuse(json!({"at": AT, "reading": reading}));

    let request = json!({"at": AT, "engine": wasm_engine(), "tsv": browser_tsv()}).to_string();
    let mut tampered = PNG.to_vec();
    let n = tampered.len() - 20;
    tampered[n] ^= 0xff;
    assert!(annotate_ocr_json(&document, &tampered, Some(&context), &request).is_err(), "other pixels than the original's");
    assert!(annotate_ocr_json(&document, PNG, None, &request).is_err(), "the capture context is part of the original");
    assert!(annotate_ocr_json(&document.replace("2026-11-25T17:00:00Z", "2026-11-25T17:00:01Z"), PNG, Some(&context), &request).is_err(), "an edited document");
}
