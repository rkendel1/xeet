# Capture: every screenshot becomes an artifact

```
ScreenCaptureKit → original PNG + capture context → xeet capture
  → screenshot artifact (pixels authoritative; context observed; accessibility tree as its own source)
  → vision.read (local OCR, on device) → annotation (inferred, attributed) → derived artifact
  → modality gateway: text-only models get text and geometry; vision models get the original
```

The screenshot is the observation. OCR text, accessibility data and layout are
additional representations with their own provenance; none of them replaces
or edits the pixels.

## What is real, what is unverified

| part | where | status |
|---|---|---|
| Screenshot artifacts with capture context | `crates/xeet-capture/src/image.rs` | **verified** (Rust tests) |
| Local OCR provider (`vision.read`, `ocr:local` scope) | `crates/xeet-ocr` | **verified with real Tesseract 5.3.4** on real images |
| `xeet-ocr/1` helper protocol | `crates/xeet-ocr` `HelperBackend` | **verified** with a scripted helper (tests the protocol, not Vision) |
| Capture path, reuse, gateway delivery | `Gateway::derive`, `xeet capture` | **verified** (Rust + CLI tests; a headless-Chromium capture) |
| Browser OCR (a WebAssembly engine such as tesseract.js reads; xeet applies the reading) | `xeet_ocr::derive`, `annotate_ocr` in `crates/xeet-wasm` | **verified**: the real Tesseract 5.3.4 reading of `invoices.png`, given as header-less TSV the way tesseract.js returns it, yields the native path's elements, pixel boxes, confidences and `ocr.uncertain` doubts exactly; the derived artifact verifies against the original bytes (Rust tests, and the built module under Node). tesseract.js itself was not run here. Browser OCR reads only pixels: it does not produce the accessibility tree, which is separate evidence only the macOS app supplies |
| Apple Vision OCR helper `xeet-vision-ocr` | `apps/macos/Sources/XeetVisionOCR` | **unverified**: syntax-checked with `swiftc -parse` (Swift 6.0.3, Linux), never compiled against the macOS SDK or run |
| Menu-bar app (ScreenCaptureKit, shortcuts, region overlay, AX tree, clipboard/export) | `apps/macos/Sources/XeetCapture` | **unverified**: same |
| App bundling | `apps/macos/scripts/make-app.sh` | **unverified**: `bash -n` only |

This work was done in a Linux container with no Mac attached. Nothing here
claims native capture works; the manual acceptance tests below are what would
establish it.

## Local OCR (`crates/xeet-ocr`)

`OcrProvider<B: Backend>` implements the existing `vision.read` contract.

- **Real engine, declared honestly.** `TesseractBackend::new` probes the
  installed `tesseract` and declares its version, languages, the image
  formats its build decodes (from `tesseract --version`), its configuration
  (`--psm 3`, TSV, one thread) and what it does not do (orientation
  detection, handwriting). The provider version is
  `<engine version>+<config digest>`, so a reading is reused only when engine
  and configuration are unchanged.
- **Output.** One `inferred` element per recognized line (`ocr.line.N`) with
  pixel bounds (quantized to 1/64 px) and `ocr.boxNormalized` (top-left
  origin, 0..1), the engine's confidence unaltered (`ocr.confidence`, the
  line's lowest word), reading order (`ocr.order`; `engine-layout` for
  Tesseract), and per-word text, box and confidence. A line is
  `ocr.uncertain` when any word is below 0.6 confidence or the line touches an
  image edge (possibly cut off). A blank image yields "No text recognized" and
  no elements.
- **Attribution.** The annotation names the provider id and version, the
  engine, version, platform and a configuration digest, the time, and the
  source artifact. It is applied by `representation.annotate`, which creates a
  new artifact `derivedFrom` the original.
- **Scope.** Text only. The provider declares scene, object and chart
  understanding as non-capabilities. Its authority scope `ocr:local` is
  separate from `vision:read` (which may send pixels to a remote model).
- **Apple Vision** reaches the provider through `HelperBackend` and the
  `xeet-ocr/1` protocol (`helper --identify`, `helper IMAGE`), implemented by
  `xeet-vision-ocr` with `VNRecognizeTextRequest` (accurate, language
  correction, automatic language detection). It converts Vision's bottom-left
  boxes to top-left, orders lines geometrically, and declares that Vision
  gives one confidence per line, not per word.
- **In a browser**, the engine runs as WebAssembly in the page and xeet only
  applies its reading. `annotate_ocr(document, image, contextText, request)`
  (the `xeet-wasm` export) takes the original screenshot artifact's verbatim
  text and bytes (refused unless they verify), and `request` is `{ at, engine,
  tsv }` — `tsv` is tesseract.js's `data.tsv` (request it with `{ tsv: true }`;
  Tesseract's TSV with or without the header line), `engine` an `EngineInfo`
  (`platform: "wasm"`, the core version, the settings used: set
  `tessedit_pageseg_mode` to 3 to read like the native path) — or `{ at,
  reading }` with an `xeet-ocr/1` result. It runs the same code as the native
  provider (`parse_tsv`, `to_annotation`, `annotate`) and returns the derived
  artifact's `documentText`. It is pure: the caller supplies `at`.
- The hand-authored stand-in used by gateway tests is now
  `test-harness:hand-authored-reader`, and lives only under `tests/`.

## Screenshot artifacts (`source: "screenshot"`)

`representation.capture` with `source: "screenshot"` takes image bytes and a
**capture context** (`xeet_capture::image::CaptureContext`; example:
`tests/fixtures/capture/context-region-retina.json`). Both are preserved
verbatim as inputs (`source-image`, `capture-context`). The view is the
captured bytes (`fidelity: original`).

| evidence | basis | where |
|---|---|---|
| pixels, format, size, digest | observed | `image` element, view `v0` |
| method, mode, platform, display, frame, window and app identity, permissions | observed (by the capturing app) | `capture.*` elements and attributes, facts `permission.*` |
| accessibility tree | **source** (what the application reports) | `ax.*` elements, frames converted from global points to image pixels (frame origin, display scale), clipped; nodes outside the capture are kept, not visible |
| OCR text and positions | **inferred** | `ocr.*` elements, in the derived artifact |

Missing context is recorded, never invented: accessibility `denied` /
`not-requested` → "the accessibility tree" omitted with the reason; a window
without a reported title → "window title" omitted; capture-app notes →
unverified. Same pixels with a different context are a different observation
(different artifact).

## `xeet capture`

```bash
xeet capture --image shot.png --context context.json [--ocr auto|tesseract|helper:PATH|off] \
  [--store DIR] [--grant store:read,store:write,fs:read,ocr:local] [--allow-read DIR] [--report FILE]
```

`--ocr auto` uses `xeet-vision-ocr` beside the binary (the app bundle), else
Tesseract, else none, and says which in the report. The report
(`xeet-capture.report` 0.1) holds the artifact reference, whether text was
read and by which provider, line, uncertain-line and accessibility-node
counts, omissions, step digests and timings — no image bytes and no
recognized text. By default `fs:read` is limited to the image's own
directory.

The capture runs through `Gateway::derive`, the same path the gateway uses for
images a model cannot see: reuse by input digest and context, then ingest,
`vision.read`, annotate, and digest validation of every artifact. A later
model request that references the capture reuses it; OCR does not run again
unless the reader's identity (engine + configuration) differs, in which case
only the reading runs on the reused original.

## The macOS app (`apps/macos`, unverified)

- **Capture**: ScreenCaptureKit (`SCScreenshotManager.captureImage`, macOS
  14+): the display under the pointer, a dragged region (overlay; Escape
  cancels), or the frontmost application's front window. Output at the
  display's backing scale (`captureResolution = .best`).
- **Context**: Quartz global points throughout; display id, name, bounds and
  scale; window id, title, app; frontmost app; screen-recording and
  accessibility permission states; the focused window's accessibility tree
  (≤1500 nodes, depth ≤25) only when the user turned on "Include
  Accessibility Context" and granted the permission.
- **Pipeline**: writes the original PNG and context to
  `~/Library/Application Support/Xeet/captures/`, runs the bundled
  `xeet capture` with the store `~/Library/Application Support/Xeet/store`,
  then, as configured, copies the PNG to the clipboard (default off) and saves
  `Xeet Capture <date>.png` to the Desktop (default on, like macOS). Exported
  PNGs are projections; the artifact is the record.
- **Shortcuts**: Carbon `RegisterEventHotKey`. Defaults ⌥⇧⌘3 (screen), ⌥⇧⌘4
  (region), ⌥⇧⌘5 (window), which do not collide with macOS's ⇧⌘3/4/5 or
  ⌃⇧⌘3/4. A shortcut another app holds fails to register and is shown in the
  menu as a conflict; nothing is taken over silently.
- **Permissions**: screen recording is checked with
  `CGPreflightScreenCaptureAccess` and requested only from the first-run
  dialog; accessibility is requested only when the user turns the option on.

### Making Xeet the screenshot tool

macOS has no setting for a third-party default screenshot handler. Xeet takes
over by shortcut:

1. System Settings › Keyboard › Keyboard Shortcuts… › Screenshots: turn off
   "Save picture of screen as a file" (⇧⌘3), "Save picture of selected area as
   a file" (⇧⌘4) and "Screenshot and recording options" (⇧⌘5).
2. Give Xeet those keys and restart it:
   ```bash
   defaults write dev.xeet.capture shortcut.display "shift+cmd+3"
   ```
   ```bash
   defaults write dev.xeet.capture shortcut.region "shift+cmd+4"
   ```
   ```bash
   defaults write dev.xeet.capture shortcut.window "shift+cmd+5"
   ```
3. To go back: turn the built-in shortcuts on again, then
   ```bash
   defaults delete dev.xeet.capture shortcut.display
   ```
   (and the same for `shortcut.region`, `shortcut.window`), and restart Xeet.
   It returns to ⌥⇧⌘3/4/5.

## Tests

Automated (`cargo test`; run with `XEET_REQUIRE_TESSERACT=1` so OCR tests
cannot skip):

| test | proves |
|---|---|
| `real_ocr_reads_real_text_with_positions_and_confidence` | real text; INV-1041 within 8 px of its true y; confidences in 0..1; the cut-off "Overd," line uncertain; attribution; same input → same digest |
| `clipped_and_blank_images_stay_uncertain_or_empty` | a real crop: rows cut at the right edge flagged; a blank image yields nothing |
| `authority_formats_and_scope_are_enforced` | formats from the installed engine; no network; `ocr:local` required; non-images refused |
| `a_screenshot_becomes_an_artifact_read_once_and_reused` | capture → artifact → OCR → derived artifact; verifies with reproduction; view = captured bytes; three bases (observed / source / inferred); AX frames map to pixels across a negative-x second display at scale 2; same capture reused with no second OCR; different context = different artifact |
| `a_different_reader_reads_the_reused_original_without_re_ingesting` | reader identity governs reuse |
| `the_helper_protocol_maps_normalized_boxes_to_pixels` | the Vision path's protocol adapter (scripted helper) |
| `without_ocr_or_its_permission_the_content_stays_unknown` | missing `ocr:local`; no OCR provider |
| `accessibility_permission_denied_is_recorded_not_hidden` | omissions and facts; a screenshot without context is refused |
| `a_capture_reaches_text_only_and_vision_models_through_the_gateway` | text-only: OCR text and geometry, no image, instruction first; vision: the original |
| `xeet_capture_makes_an_artifact_and_reads_its_text_locally` (CLI) | the report; no text in it; reuse; `--ocr off` |

Found by these tests: OCR boxes (and accessibility frames) stored as
unquantized `f32` broke artifact identity after a disk round trip;
`annotate` and the accessibility mapping now quantize bounds to 1/64 px, like
every other producer.

### Manual acceptance on macOS (not yet performed)

Environment: macOS 14 or later, Xcode 15+ or Command Line Tools, Rust.

1. `apps/macos/scripts/make-app.sh` → `apps/macos/build/Xeet Capture.app`.
   Fix any compile errors (the Swift has only been parsed, not compiled).
2. `"…/Contents/MacOS/xeet-vision-ocr" --identify` prints engine JSON with
   `apple-vision`; `xeet-vision-ocr tests/fixtures/gateway/invoices.png`
   prints lines including `INV-1041`, boxes in 0..1.
3. Launch the app. First run explains permissions; grant Screen Recording.
4. ⌥⇧⌘3: a full-screen capture. Check the menu: "Last capture: N text lines";
   a PNG on the Desktop; `xeet artifact verify` on the store entry passes.
5. ⌥⇧⌘4, drag a region; Escape on a second try cancels without a capture.
6. ⌥⇧⌘5 on a window; with "Include Accessibility Context" on and permission
   granted, the report's `accessibilityNodes` > 0; with it denied, the
   artifact lists the omission.
7. Two displays, one Retina: captures on each have pixel sizes = points ×
   scale; accessibility bounds land on the right pixels.
8. Revoke Screen Recording: a capture reports the permission error and makes
   no artifact.
9. Assign ⇧⌘4 per "Making Xeet the screenshot tool"; with the built-in still
   on, the menu shows the conflict.
10. `xeet gateway prepare --model tests/fixtures/gateway/profile-text-only.json --artifact <ref> --instruction "…"`
    delivers the OCR text; with `profile-vision.json` and `--attach` the PNG,
    the original.

## Measurements (Linux container, optimized build, Tesseract 5.3.4)

| | invoices.png 480×356 | Chromium capture 1280×600 |
|---|---:|---:|
| `xeet capture`, first (ingest, OCR, annotate, 3 validations) | 218–269 ms | 226–307 ms |
| `tesseract` alone | 229 ms | 235 ms |
| `xeet capture`, same pixels and context again (reused) | 11–15 ms | 10–12 ms |

OCR is the cost; the artifact, annotation and validation overhead is within
the run-to-run variation. The Chromium capture (a headless browser, not an OS
screen capture) read 14 lines at 0.93–0.97 confidence with no uncertain
lines; Tesseract reads that table column by column, and the projection's
geometric layout rows restore the rows.

## Not done

- Native capture has not run. The Swift has not been compiled.
- No first-run window beyond an alert; shortcuts are configured with
  `defaults write`, not a recorder UI.
- No scene interpretation provider (deliberately: separate capability).
- OCR covers English in this environment (installed Tesseract data); Apple
  Vision's languages come from the OS.
- Window capture takes the frontmost app's front window, not a picker.
