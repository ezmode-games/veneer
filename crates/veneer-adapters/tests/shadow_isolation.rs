//! Shadow-boundary isolation integration (FR-VEN-018, issue #105).
//!
//! Drives the full pipeline over real-shaped fixtures: a `.classes.ts`
//! modeled verbatim on rafters `packages/ui/src/old/ui/badge.classes.ts`,
//! and a preview sheet in the shape rafters' `registryToDocumentation`
//! emits -- COMPILED rules with the theme on `:host`, not the `@theme`/
//! `@utility` source form. Asserts the isolation contract structurally on
//! the generated JavaScript: an open shadow root, CSS delivered only via
//! `shadowRoot.adoptedStyleSheets` from the ONE shared sheet module, zero
//! page-global style injection, and no framework runtime. The pixel-level
//! hostile-CSS comparison needs a browser harness, which this repository
//! does not have; these tests assert the structural contract that
//! guarantees it.

use std::fs;
use std::path::PathBuf;

use veneer_adapters::{
    extract_classes_from_ts, preview_styles_module, preview_web_component_block,
    ComponentConventions, ComponentRegistry, ReactAdapter, DOCUMENTATION_SHEET_PATH,
};

/// A preview sheet in the compiled shape veneer now adopts: resolved tokens
/// on `:host` (never `:root`), plain compiled class rules, and no Tailwind
/// source at-rules. `.unreferenced-by-any-component` is deliberate -- the
/// sheet is adopted WHOLE, so a rule nothing references must still arrive.
const PREVIEW_SHEET: &str = ":host{container-type:inline-size}\
:host{--color-primary:oklch(.645 .12 180);--font-size-label-small:.75rem}\
.text-label-small{font-size:var(--font-size-label-small)}\
.bg-primary{background-color:var(--color-primary)}\
.unreferenced-by-any-component{outline:1px solid red}";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shadow")
}

fn fixture(name: &str) -> String {
    let path = fixture_dir().join(name);
    match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => panic!("cannot read fixture {}: {error}", path.display()),
    }
}

/// The structural isolation contract on a generated preview module.
fn assert_isolation_contract(js: &str) {
    // Open shadow root, CSS via adoptedStyleSheets from the shared module.
    assert!(js.contains("this.attachShadow({ mode: 'open' })"));
    assert!(js.contains("this.shadowRoot.adoptedStyleSheets = [previewStyles()]"));
    assert!(js.contains("import { previewStyles } from './preview-styles.js';"));

    // Zero page-global style interaction: the module neither reads the host
    // page's stylesheets nor injects a <style>/<link> into it.
    assert!(!js.contains("document.styleSheets"));
    assert!(!js.contains("document.head"));
    assert!(!js.contains("document.adoptedStyleSheets"));
    assert!(!js.contains("<style"));
    assert!(!js.contains("<link"));
    assert!(!js.contains("createElement('style')"));
    assert!(!js.contains("createElement('link')"));

    // No framework runtime on the host page. The shared sheet module is the
    // only import a preview is allowed to carry.
    assert_eq!(js.matches("import ").count(), 1);
    assert!(!js.contains("require("));
    assert!(!js.to_lowercase().contains("react"));
}

#[test]
fn badge_preview_is_style_isolated_and_imports_the_shared_sheet() {
    let ts = fixture("badge.classes.ts");

    let adapter = ReactAdapter::with_conventions(ComponentConventions::for_classes_file("badge"));
    let structure = match adapter.extract_structure(&ts) {
        Ok(structure) => structure,
        Err(error) => panic!("badge.classes.ts must extract: {error}"),
    };

    let block = match preview_web_component_block("badge-preview", &structure, PREVIEW_SHEET) {
        Ok(block) => block,
        Err(error) => panic!("badge preview must render against a real sheet: {error}"),
    };

    assert_isolation_contract(&block.web_component);

    // No CSS is carried in the preview itself: the sheet lives in one shared
    // module, and per-component subsetting is what this change retired.
    assert!(!block
        .web_component
        .contains("font-size:var(--font-size-label-small)"));
    assert!(!block.web_component.contains(":host{"));
}

#[test]
fn the_shared_sheet_is_adopted_whole_including_rules_no_component_references() {
    // The adopt-whole thesis, asserted where it can fail: a rule that no
    // component's class list mentions must still reach the shadow root. A
    // subsetting regression would drop exactly this rule and nothing else,
    // and every preview would still look right in a screenshot.
    let module = preview_styles_module(PREVIEW_SHEET);

    assert!(module.contains(".unreferenced-by-any-component{outline:1px solid red}"));
    assert!(module.contains(".text-label-small{"));
    // Tokens ride on :host, so an adopted copy beats the host page's :root.
    assert!(module.contains(":host{--color-primary:"));
    assert!(!module.contains(":root{"));
    // One construction, shared by every preview on the page.
    assert_eq!(module.matches("new CSSStyleSheet()").count(), 1);
}

#[test]
fn dynamically_composed_quality_classes_still_surface_as_docs_data() {
    // Tree-shake caveat (bullpen 019f1f4d): `text-quality-${tint}` never
    // appears as a source literal. It no longer selects any CSS -- nothing
    // does -- but the class list is what a page reports the component
    // resolves to, so the pattern must still surface.
    let ts = fixture("quality-indicator.classes.ts");

    let classes = extract_classes_from_ts(&ts);
    assert!(
        classes.contains(&"text-quality-*".to_string()),
        "extraction must surface the dynamic composition as a pattern: {classes:?}"
    );
}

#[test]
fn dynamic_quality_classes_reach_the_generated_module_through_the_registry_pipeline() {
    // The wired pipeline end to end: registry scan (export discovery over
    // the .classes.ts source) -> component structure -> preview block.
    let mut registry = ComponentRegistry::new();
    let count = match registry.scan(&fixture_dir()) {
        Ok(count) => count,
        Err(error) => panic!("shadow fixtures must scan: {error}"),
    };
    assert!(count >= 2, "badge and quality-indicator must register");

    let block = match registry.generate_web_component(
        "QualityIndicator",
        "quality-indicator-preview",
        PREVIEW_SHEET,
    ) {
        Ok(block) => block,
        Err(error) => panic!("quality indicator preview must render: {error}"),
    };

    assert_isolation_contract(&block.web_component);
    assert!(
        block.classes_used.contains(&"text-quality-*".to_string()),
        "the dynamic composition must surface as a pattern: {:?}",
        block.classes_used
    );
}

#[test]
fn a_missing_sheet_names_the_component_and_the_path_instead_of_rendering_unstyled() {
    let ts = fixture("badge.classes.ts");

    let adapter = ReactAdapter::with_conventions(ComponentConventions::for_classes_file("badge"));
    let structure = match adapter.extract_structure(&ts) {
        Ok(structure) => structure,
        Err(error) => panic!("badge.classes.ts must extract: {error}"),
    };

    let error = match preview_web_component_block("badge-preview", &structure, "") {
        Ok(_) => panic!("must not emit a preview with no sheet to adopt"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains(&structure.name),
        "error must name the component: {message}"
    );
    assert!(
        message.contains(DOCUMENTATION_SHEET_PATH),
        "error must name the sheet it looked for: {message}"
    );
}
