//! The browser-facing screenshot exports, run natively (the same pure functions the WASM build exports).

use xeet_wasm::{capture_screenshot_json, verify_screenshot_json};

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/gateway/invoices.png");
const CONTEXT: &str = include_str!("../../../tests/fixtures/capture/context-region-retina.json");

fn request() -> String {
    serde_json::json!({
        "capturedAt": "2026-11-25T17:00:00Z",
        "method": "getDisplayMedia",
        "context": serde_json::from_str::<serde_json::Value>(CONTEXT).unwrap(),
    })
    .to_string()
}

fn ok(report: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(report).unwrap()["ok"].as_bool().unwrap()
}

#[test]
fn a_capture_is_deterministic_and_verifies_against_its_bytes() {
    let a: serde_json::Value = serde_json::from_str(&capture_screenshot_json(PNG, &request()).unwrap()).unwrap();
    let b: serde_json::Value = serde_json::from_str(&capture_screenshot_json(PNG, &request()).unwrap()).unwrap();
    assert_eq!(a["document"]["id"], b["document"]["id"], "same inputs, same artifact");
    assert_eq!(a["document"]["subject"]["type"], "screenshot");
    let roles: Vec<&str> = a["assets"].as_array().unwrap().iter().map(|x| x["role"].as_str().unwrap()).collect();
    assert!(roles.contains(&"source-image") && roles.contains(&"capture-context"), "{roles:?}");

    let document = a["document"].to_string();
    let context = a["contextText"].as_str().unwrap();
    assert!(ok(&verify_screenshot_json(&document, PNG, Some(context))), "{}", verify_screenshot_json(&document, PNG, Some(context)));

    let mut tampered = PNG.to_vec();
    let last = tampered.len() - 20;
    tampered[last] ^= 0xff;
    assert!(!ok(&verify_screenshot_json(&document, &tampered, Some(context))), "a changed pixel byte fails");
    assert!(!ok(&verify_screenshot_json(&document, PNG, Some(&context.replace("region", "window")))), "a changed context fails");
}

#[test]
fn a_request_is_strict_and_an_image_is_identified_by_content() {
    assert!(capture_screenshot_json(PNG, r#"{"capturedAt":"x","method":"m","surprise":1}"#).is_err());
    assert!(capture_screenshot_json(b"not an image", &request()).is_err());
}
