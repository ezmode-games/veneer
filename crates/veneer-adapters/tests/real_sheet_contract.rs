//! The preview-sheet contract, asserted against REAL rafters output.
//!
//! Every other sheet fixture in this suite is a small hand-written stand-in,
//! which is fine for exercising veneer's machinery and useless for validating
//! claims about what rafters emits: a fixture veneer authored cannot disagree
//! with veneer's assumptions. `real-documentation-excerpt.css` is copied
//! byte-for-byte out of a generated sheet (see
//! `scripts/extract-sheet-fixture.py`, which records the source hash in the
//! file header), so these tests fail if the real artifact's shape moves.

use veneer_adapters::preview_styles_module;

fn real_excerpt() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/real-documentation-excerpt.css"
    );
    std::fs::read_to_string(path).expect("the real-output excerpt fixture must be readable")
}

#[test]
fn the_real_sheet_carries_its_theme_on_host_and_never_on_root() {
    let sheet = real_excerpt();
    // The load-bearing mechanic for preview isolation: tokens on :host beat
    // whatever the embedding page declares on :root.
    assert!(
        sheet.contains(":host{"),
        "real output must carry :host blocks"
    );
    assert!(
        sheet.contains("container-type:inline-size"),
        "previews are container-query scoped by the sheet, not by veneer"
    );
    // Counting rule declarations, not the substring -- ":root" also appears
    // inside the provenance comment of this fixture.
    let body = sheet.split("*/").nth(1).unwrap_or(&sheet);
    assert!(
        !body.contains(":root"),
        "a :root block would couple previews to the host page's theme"
    );
    assert!(!body.contains("@theme"), "compiled output, not source form");
    assert!(
        !body.contains("@import"),
        "the sheet must be self-contained"
    );
}

#[test]
fn the_real_sheet_registers_the_custom_properties_its_composites_read() {
    let sheet = real_excerpt();
    // .shadow-sm composes box-shadow out of --tw-* operands. Unregistered and
    // unassigned, the whole declaration is invalid at computed-value time and
    // every shadow silently stops painting.
    assert!(sheet.contains(".shadow-sm{"));
    assert!(
        sheet.contains("@property --tw-shadow"),
        "composite utilities need their custom properties registered"
    );
    // An animation utility whose keyframes are absent is a named reference to
    // nothing -- it renders as no motion rather than as an error.
    assert!(sheet.contains(".animate-pulse{"));
    assert!(sheet.contains("@keyframes pulse"));
}

#[test]
fn real_escaped_selectors_survive_the_js_string_round_trip() {
    // The reason this test uses real bytes: the sheet is mostly escaped
    // selectors (`.hover\:bg-muted`), and a hand-written fixture with no
    // backslashes in it cannot see an escaping regression at all.
    let sheet = real_excerpt();
    assert!(
        sheet.contains(r"\:"),
        "the excerpt must contain real escaped selectors or this proves nothing"
    );

    let module = preview_styles_module(&sheet);
    let literal = module
        .split_once("const previewCss = '")
        .and_then(|(_, rest)| rest.split_once("';\n\nlet previewSheet"))
        .map(|(literal, _)| literal)
        .expect("the module must embed the sheet as a single quoted literal");

    // Evaluate the JS literal the way an engine would.
    let mut evaluated = String::with_capacity(sheet.len());
    let mut chars = literal.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            assert_ne!(ch, '\'', "an unescaped quote would end the literal early");
            evaluated.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => evaluated.push('\n'),
            Some('r') => evaluated.push('\r'),
            Some('t') => evaluated.push('\t'),
            Some(other) => evaluated.push(other),
            None => panic!("literal ends in a dangling escape"),
        }
    }

    assert_eq!(
        evaluated, sheet,
        "the emitted module must evaluate back to the sheet byte for byte"
    );
}
