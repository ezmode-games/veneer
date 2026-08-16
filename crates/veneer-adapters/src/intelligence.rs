//! Per-component framework-less preview plus compiled intelligence
//! (FR-VEN-003). For every discovered component and composite,
//! [`render_component`] produces the Web Component preview (the
//! `preview_web_component_block` pipeline: no framework runtime referenced,
//! shadow-root CSS the project's compiled documentation sheet adopted whole
//! per FR-VEN-018) together with the intelligence fields present in its
//! source.
//!
//! Grounding (verified against the real rafters repo):
//!
//! - Component intelligence lives in JSDoc blocks using the canonical tags
//!   from `packages/shared/src/component-intelligence.ts`:
//!   `@cognitive-load N/10 - description`, `@usage-patterns` with
//!   `DO:`/`NEVER:` lines (plus the legacy `@do`/`@never` tags), and
//!   `@dependencies`. Old-constitution components
//!   (`packages/ui/src/old/ui/*.tsx`) carry these tags; new-constitution
//!   components (`x.behavior.ts` + framework wiring) carry none, so their
//!   cognitive load and do/never render as absent -- never synthesized.
//! - Composite manifests (`*.composite.json`) declare `cognitiveLoad`
//!   (a bare number) and `usagePatterns.do` / `usagePatterns.never`.
//! - Props are the properties a `*Props` TypeScript interface declares.
//! - Dependencies are the external packages the source imports, plus any
//!   packages a `@dependencies` JSDoc tag declares.
//! - The namespace source declares no component-to-token link; the only
//!   token references present in component source are the utility classes
//!   themselves (for example `bg-primary` references the semantic token
//!   `primary`). A [`TokenRef`] is therefore an exact-name match between a
//!   class the component uses and a token the `.rafters/` namespace source
//!   declares -- both sides real, nothing invented. With
//!   [`IntelligenceSource::NoSource`] there are no declared tokens to
//!   reference, so `tokens` is empty.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, PropertyKey, Statement, TSSignature};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use serde::Deserialize;

use crate::config_interface::{resolve_config_interface, ResolvedConfig};
use crate::element::{component_source_for, resolve_root_element, ResolvedElement};
use crate::generator::{
    ensure_sheet_present, generate_passthrough_web_component, preview_web_component_block,
};
use crate::rafters_source::{IntelligenceSource, UsagePatterns};
use crate::registry::{extract_component_candidate, is_composite_manifest, DiscoveredItem};
use crate::traits::{TransformError, TransformedBlock};
use crate::ts_helpers::{kebab_case, normalize_whitespace};

/// One property a `*Props` TypeScript interface declares. Every field is
/// read from the declaration itself: nothing is inferred from usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PropDoc {
    /// Declared property name.
    pub name: String,
    /// The declared type annotation, verbatim from source (whitespace
    /// normalized). `None` when the declaration has no annotation.
    pub type_text: Option<String>,
    /// Whether the property is declared optional (`name?:`).
    pub optional: bool,
}

/// One variant the component source declares, with the classes it maps to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VariantDoc {
    /// Variant key as declared in the variant record.
    pub name: String,
    /// The class string that variant maps to, verbatim from source.
    pub classes: String,
}

/// Cognitive load as declared in source: the `@cognitive-load N/10 - desc`
/// JSDoc tag on components, or the bare `cognitiveLoad` number a composite
/// manifest declares (which carries no description).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CognitiveLoad {
    /// Declared score on the 0-10 scale.
    pub score: u8,
    /// The prose after the score, when the source declares one.
    pub description: Option<String>,
}

/// Whether a constraint is a DO or a NEVER.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintKind {
    Do,
    Never,
}

/// One do/never usage constraint, from `@usage-patterns` `DO:`/`NEVER:`
/// lines, legacy `@do`/`@never` tags, or a composite manifest's
/// `usagePatterns` block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub text: String,
}

/// A namespace token the component's classes reference by exact name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TokenRef {
    /// Token name as declared in the namespace source.
    pub token: String,
    /// Namespace that declares the token (for example "semantic").
    pub namespace: String,
    /// The component classes that reference the token, sorted.
    pub referenced_by: Vec<String>,
}

/// Where a dependency declaration comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyOrigin {
    /// An `import ... from '<package>'` statement in the source file.
    Import,
    /// A `@dependencies` JSDoc tag.
    JsDocTag,
}

/// One dependency the component source declares.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DependencyRef {
    /// Package name as declared.
    pub name: String,
    pub origin: DependencyOrigin,
}

/// The compiled intelligence of one component or composite. Every field
/// holds exactly what the source declares: an empty `Vec` or `None` means
/// the source declares nothing for that field -- absent, never synthesized.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompiledIntelligence {
    pub props: Vec<PropDoc>,
    /// The `Config` interfaces the prop surface extends, by name and
    /// unresolved (each lives in another file). Empty on the `*Props` path
    /// and for composites -- the honest remainder of the attribute surface.
    pub config_extends: Vec<String>,
    pub variants: Vec<VariantDoc>,
    pub cognitive_load: Option<CognitiveLoad>,
    pub do_never: Vec<Constraint>,
    pub tokens: Vec<TokenRef>,
    pub dependencies: Vec<DependencyRef>,
}

/// A rendered component: the framework-less Web Component preview plus its
/// compiled intelligence.
#[derive(Debug, Clone)]
pub struct RenderedComponent {
    /// Web Component preview from the existing generation pipeline.
    pub preview: TransformedBlock,
    pub intelligence: CompiledIntelligence,
}

/// Render one discovered component or composite: framework-less preview
/// plus compiled intelligence. Composites go through this same path --
/// a composite source file renders exactly like a component, and a
/// composite manifest renders a passthrough preview with the intelligence
/// the manifest declares.
///
/// `full_css` is the project's compiled preview sheet
/// (`.rafters/output/rafters.documentation.css` via
/// `read_rafters_stylesheet`); every preview adopts it whole (FR-VEN-018).
/// It is not read here for content -- only for presence, because a preview
/// that would render unstyled is refused rather than emitted.
///
/// Any failure -- including a missing or empty sheet, so a preview never
/// renders silently missing its styles -- is a
/// [`TransformError::RenderFailed`] naming the item, so a failing
/// component surfaces in coverage instead of vanishing.
pub fn render_component(
    item: &DiscoveredItem,
    source: &IntelligenceSource,
    full_css: &str,
) -> Result<RenderedComponent, TransformError> {
    render_item(item, source, full_css).map_err(|reason| TransformError::RenderFailed {
        component: item.name.clone(),
        reason,
    })
}

fn render_item(
    item: &DiscoveredItem,
    source: &IntelligenceSource,
    full_css: &str,
) -> Result<RenderedComponent, String> {
    if is_composite_manifest(&item.source_path) {
        return render_manifest_composite(item, full_css);
    }
    render_source_item(item, source, full_css)
}

/// Render an item declared by a component source file (`.tsx`, `.jsx`, or
/// `.classes.ts`). Composite source files take exactly this path too.
fn render_source_item(
    item: &DiscoveredItem,
    source: &IntelligenceSource,
    full_css: &str,
) -> Result<RenderedComponent, String> {
    let source_text = read_source_file(&item.source_path)?;

    let Some((_, structure)) = extract_component_candidate(&item.source_path, &source_text) else {
        return Err(format!(
            "{} is not a renderable component source file",
            item.source_path.display()
        ));
    };
    let structure = structure.map_err(|error| {
        format!(
            "failed to extract a component structure from {}: {error}",
            item.source_path.display()
        )
    })?;

    // Which element the component actually renders, read from the sibling
    // `.tsx` (issue #109). Unresolvable is a refusal, never a default: every
    // preview used to emit `<button>`, and a `<button>` standing in for a
    // `<nav>` or a `<label>` is an accessibility defect that renders
    // plausibly enough that nobody files it.
    let element = resolve_element(item)?;

    // The root element's own classes, bound by the JSX rather than by a
    // constant's name: `<div data-part="root" className={classy(
    // tableWrapperClasses, className)}>` says the root wears
    // `tableWrapperClasses`. `tableRootClasses` belongs to the inner
    // `<table>`, and breadcrumb's root names no constant at all -- its root
    // legitimately carries nothing.
    // Only override when the JSX binding actually RESOLVES. A root whose
    // className is a call (`classy(kbdClasses({size}), className)`) names no
    // constant this can read, and clobbering the extracted classes with
    // nothing would trade a too-broad preview for a blank one.
    let mut structure = structure;
    if let Some(root_classes) =
        crate::class_parts::resolve_named_classes(&source_text, &element.class_idents)
    {
        structure.base_classes = root_classes;
    }

    // The preview adopts the project's documentation sheet whole; nothing is
    // scoped out of it. A missing or empty sheet refuses the preview with
    // that reason -- never a preview silently missing its styles
    // (FR-VEN-018).
    let preview = preview_web_component_block(
        &preview_tag_name(&item.name),
        &structure,
        full_css,
        &element.tag,
    )
    .map_err(|error| match error {
        TransformError::RenderFailed { reason, .. } => reason,
        other => other.to_string(),
    })?;

    let module_facts = parse_module_facts(&item.source_path, &source_text)?;
    let jsdoc = read_family_jsdoc(&item.source_path, &source_text)?;

    // The prop/API surface is the behavior's Config interface when the
    // component has one (new constitution); otherwise the `*Props` interface
    // (old constitution). The Config path also names the bases it extends --
    // the unresolved remainder of the surface.
    let (props, config_extends) = match read_component_config(&item.source_path)? {
        Some(config) => (config.props, config.unresolved_extends),
        None => (module_facts.props, Vec::new()),
    };

    let mut dependencies: Vec<DependencyRef> = Vec::new();
    for import in module_facts.external_imports {
        push_unique_dependency(&mut dependencies, import, DependencyOrigin::Import);
    }
    for declared in jsdoc.dependencies {
        push_unique_dependency(&mut dependencies, declared, DependencyOrigin::JsDocTag);
    }

    let variants = structure
        .variant_lookup
        .iter()
        .map(|(name, classes)| VariantDoc {
            name: name.clone(),
            classes: classes.clone(),
        })
        .collect();

    let tokens = token_references(&preview.classes_used, source);

    Ok(RenderedComponent {
        preview,
        intelligence: CompiledIntelligence {
            props,
            config_extends,
            variants,
            cognitive_load: jsdoc.cognitive_load,
            do_never: jsdoc.do_never,
            tokens,
            dependencies,
        },
    })
}

/// The component's fully-resolved `Config` prop surface, read from its
/// `.behavior.ts` -- the item's own file when it is the behavior file, else
/// the same-stem `.behavior.ts` sibling. The extends chain is followed across
/// files. `None` when the component has no behavior file (the old
/// constitution) or its behavior declares no `Config` interface.
fn read_component_config(path: &Path) -> Result<Option<ResolvedConfig>, String> {
    let behavior_path = if is_behavior_file(path) {
        Some(path.to_path_buf())
    } else {
        family_files(path)
            .into_iter()
            .find(|sibling| is_behavior_file(sibling))
    };

    match behavior_path {
        Some(behavior_path) => resolve_config_interface(&behavior_path),
        None => Ok(None),
    }
}

/// True when `path` names a `.behavior.ts` file -- the per-component signal
/// that distinguishes the new constitution's `Config`-interface path from
/// the `*Props` fallback (above). [`crate::mode::detect_mode`] elevates this
/// same predicate to a project-level determination; it is defined here once
/// so the two never fork.
pub(crate) fn is_behavior_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".behavior.ts"))
}

/// The subset of a `*.composite.json` manifest that declares renderable
/// intelligence, grounded in the real manifests
/// (`packages/ui/src/composites/*.composite.json`): `cognitiveLoad` is a
/// bare number and `usagePatterns` holds `do`/`never` arrays. Fields the
/// manifest does not declare stay `None` -- absent, never synthesized.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderableManifest {
    cognitive_load: Option<u8>,
    usage_patterns: Option<UsagePatterns>,
}

/// The manifest wrapper of a composite file.
#[derive(Debug, Deserialize)]
struct RenderableManifestFile {
    manifest: RenderableManifest,
}

/// Resolve the element a component renders, from the `.tsx` beside its
/// `.classes.ts`. Every failure names the component and what was looked for,
/// so the item lands in not-yet-documented with an actionable reason instead
/// of rendering the wrong element.
fn resolve_element(item: &DiscoveredItem) -> Result<ResolvedElement, String> {
    let Some(tsx_path) = component_source_for(&item.source_path) else {
        return Err(format!(
            "cannot locate the component source for {}: expected a sibling \
             .tsx beside the .classes.ts",
            item.source_path.display()
        ));
    };
    let tsx_source = read_source_file(&tsx_path)?;
    resolve_root_element(&item.name, &tsx_source).map_err(|error| error.to_string())
}

/// Render a composite declared by a `*.composite.json` manifest: a
/// passthrough preview (the manifest declares blocks, not classes) plus
/// the intelligence the manifest declares.
fn render_manifest_composite(
    item: &DiscoveredItem,
    full_css: &str,
) -> Result<RenderedComponent, String> {
    let text = read_source_file(&item.source_path)?;
    let parsed: RenderableManifestFile = serde_json::from_str(&text).map_err(|error| {
        format!(
            "malformed composite manifest {}: {error}",
            item.source_path.display()
        )
    })?;

    // A manifest declares no classes, but its passthrough preview still
    // adopts the shared sheet, so the same missing-sheet refusal applies.
    ensure_sheet_present(&item.name, full_css).map_err(|error| match error {
        TransformError::RenderFailed { reason, .. } => reason,
        other => other.to_string(),
    })?;

    let tag_name = preview_tag_name(&item.name);
    let preview = TransformedBlock {
        web_component: generate_passthrough_web_component(&tag_name),
        tag_name,
        classes_used: Vec::new(),
        attributes: Vec::new(),
    };

    let cognitive_load = parsed.manifest.cognitive_load.map(|score| CognitiveLoad {
        score,
        // The manifest format declares a bare number -- there is no
        // description in source to carry.
        description: None,
    });
    let do_never = parsed
        .manifest
        .usage_patterns
        .map(constraints_from_patterns)
        .unwrap_or_default();

    Ok(RenderedComponent {
        preview,
        intelligence: CompiledIntelligence {
            cognitive_load,
            do_never,
            // A manifest declares no props interface, no variant records,
            // no imports, and no classes; these fields are absent.
            ..CompiledIntelligence::default()
        },
    })
}

fn constraints_from_patterns(patterns: UsagePatterns) -> Vec<Constraint> {
    let dos = patterns.do_patterns.into_iter().map(|text| Constraint {
        kind: ConstraintKind::Do,
        text,
    });
    let nevers = patterns.never.into_iter().map(|text| Constraint {
        kind: ConstraintKind::Never,
        text,
    });
    dos.chain(nevers).collect()
}

fn push_unique_dependency(
    dependencies: &mut Vec<DependencyRef>,
    name: String,
    origin: DependencyOrigin,
) {
    if !dependencies
        .iter()
        .any(|dep| dep.name == name && dep.origin == origin)
    {
        dependencies.push(DependencyRef { name, origin });
    }
}

/// Custom element tag for an item's preview: the kebab-cased item name
/// plus a `-preview` suffix (which also guarantees the dash a custom
/// element name requires).
fn preview_tag_name(name: &str) -> String {
    format!("{}-preview", kebab_case(name))
}

// ---- module facts: props and imports, from the oxc AST ----

/// Facts read directly from the source module: declared props and external
/// imports.
#[derive(Debug, Default)]
struct ModuleFacts {
    props: Vec<PropDoc>,
    external_imports: Vec<String>,
}

/// Parse the source module for `*Props` interface declarations and import
/// statements. The file already parsed during structure extraction, so a
/// failure here is unexpected -- but it is still a named error, never a
/// silent empty result.
fn parse_module_facts(path: &Path, source: &str) -> Result<ModuleFacts, String> {
    let source_type = if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "tsx" || extension == "jsx")
    {
        SourceType::tsx()
    } else {
        SourceType::ts()
    };

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, source_type).parse();
    if ret.panicked || !ret.errors.is_empty() {
        return Err(format!(
            "failed to parse {} while reading props and imports",
            path.display()
        ));
    }

    let mut facts = ModuleFacts::default();
    for statement in &ret.program.body {
        match statement {
            Statement::TSInterfaceDeclaration(interface) => {
                collect_props_interface(interface, source, &mut facts.props);
            }
            Statement::ExportNamedDeclaration(export) => {
                if let Some(Declaration::TSInterfaceDeclaration(interface)) = &export.declaration {
                    collect_props_interface(interface, source, &mut facts.props);
                }
            }
            Statement::ImportDeclaration(import) => {
                let specifier = import.source.value.as_str();
                let is_external = !specifier.starts_with('.') && !specifier.starts_with('/');
                if is_external && !facts.external_imports.iter().any(|s| s == specifier) {
                    facts.external_imports.push(specifier.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

/// Collect the properties of an interface whose name ends with "Props".
fn collect_props_interface(
    interface: &oxc_ast::ast::TSInterfaceDeclaration<'_>,
    source: &str,
    props: &mut Vec<PropDoc>,
) {
    if !interface.id.name.as_str().ends_with("Props") {
        return;
    }
    for signature in &interface.body.body {
        let TSSignature::TSPropertySignature(property) = signature else {
            continue;
        };
        let name = match &property.key {
            PropertyKey::StaticIdentifier(identifier) => identifier.name.as_str().to_string(),
            PropertyKey::StringLiteral(literal) => literal.value.as_str().to_string(),
            _ => continue,
        };
        let type_text = property.type_annotation.as_ref().map(|annotation| {
            normalize_whitespace(annotation.type_annotation.span().source_text(source))
        });
        if !props.iter().any(|prop| prop.name == name) {
            props.push(PropDoc {
                name,
                type_text,
                optional: property.optional,
            });
        }
    }
}

// ---- JSDoc intelligence: the canonical rafters tag format ----

/// Intelligence read from JSDoc blocks: the canonical rafters format
/// (`@cognitive-load`, `@usage-patterns` with `DO:`/`NEVER:` lines, legacy
/// `@do`/`@never`, and `@dependencies`).
#[derive(Debug, Default)]
struct JsDocIntelligence {
    cognitive_load: Option<CognitiveLoad>,
    do_never: Vec<Constraint>,
    dependencies: Vec<String>,
}

/// Read JSDoc intelligence from the item's source file and its same-stem
/// family files. The rafters constitution (bullpen 019f1c03) splits a
/// component across classes/behavior/wiring files, so intelligence declared
/// on any file of the family counts: the item's own file is read first,
/// then `<stem>.tsx` / `<stem>.jsx` / `<stem>.classes.ts` /
/// `<stem>.behavior.ts` siblings that exist on disk.
fn read_family_jsdoc(path: &Path, own_source: &str) -> Result<JsDocIntelligence, String> {
    let mut merged = JsDocIntelligence::default();
    collect_jsdoc_intelligence(own_source, &mut merged);

    for sibling in family_files(path) {
        let text = read_source_file(&sibling)?;
        collect_jsdoc_intelligence(&text, &mut merged);
    }
    Ok(merged)
}

/// Read a source file, naming the file in the failure reason.
pub(crate) fn read_source_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

/// The same-stem sibling files of a component source file, excluding the
/// file itself. Only files that exist are returned.
pub(crate) fn family_files(path: &Path) -> Vec<PathBuf> {
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let stem = filename
        .strip_suffix(".classes.ts")
        .or_else(|| filename.strip_suffix(".behavior.ts"))
        .or_else(|| filename.strip_suffix(".tsx"))
        .or_else(|| filename.strip_suffix(".jsx"))
        .unwrap_or(filename);

    [".tsx", ".jsx", ".classes.ts", ".behavior.ts"]
        .iter()
        .map(|suffix| parent.join(format!("{stem}{suffix}")))
        .filter(|candidate| candidate.is_file() && candidate != path)
        .collect()
}

/// Scan every `/** ... */` block in a source file for the canonical
/// intelligence tags, merging into `intelligence`. Matches the merge
/// semantics of the rafters parser: the first cognitive load wins;
/// do/never entries append in encounter order without deduplication.
fn collect_jsdoc_intelligence(source: &str, intelligence: &mut JsDocIntelligence) {
    for block in jsdoc_blocks(source) {
        for tag in jsdoc_tags(&block) {
            match tag.name.as_str() {
                "cognitive-load" | "cognitiveload" if intelligence.cognitive_load.is_none() => {
                    intelligence.cognitive_load = parse_cognitive_load(&tag.value);
                }
                "usage-patterns" | "usagepatterns" => {
                    parse_do_never_lines(&tag.value, &mut intelligence.do_never);
                }
                "do" => intelligence.do_never.push(Constraint {
                    kind: ConstraintKind::Do,
                    text: normalize_whitespace(&tag.value),
                }),
                "never" => intelligence.do_never.push(Constraint {
                    kind: ConstraintKind::Never,
                    text: normalize_whitespace(&tag.value),
                }),
                "dependencies" => {
                    // Whitespace-separated package list; a parenthesized
                    // remark ends the list (rafters parser behavior).
                    for token in tag.value.split_whitespace() {
                        if token.starts_with('(') {
                            break;
                        }
                        if !intelligence.dependencies.iter().any(|dep| dep == token) {
                            intelligence.dependencies.push(token.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Extract the text of every `/** ... */` block, with comment decoration
/// (leading `*`) stripped per line.
pub(crate) fn jsdoc_blocks(source: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("/**") {
        let after_open = &rest[start + 3..];
        let Some(end) = after_open.find("*/") else {
            break;
        };
        let body = &after_open[..end];
        let cleaned: Vec<String> = body
            .lines()
            .map(|line| {
                let trimmed = line.trim_start();
                let without_star = trimmed.strip_prefix('*').unwrap_or(trimmed);
                without_star.strip_prefix(' ').unwrap_or(without_star)
            })
            .map(str::to_string)
            .collect();
        blocks.push(cleaned.join("\n"));
        rest = &after_open[end + 2..];
    }
    blocks
}

/// One `@tag value` from a JSDoc block; `value` spans until the next tag.
pub(crate) struct JsDocTag {
    pub(crate) name: String,
    pub(crate) value: String,
}

pub(crate) fn jsdoc_tags(block: &str) -> Vec<JsDocTag> {
    let mut tags: Vec<JsDocTag> = Vec::new();
    for line in block.lines() {
        if let Some(tag_line) = line.trim_start().strip_prefix('@') {
            let (name, value) = match tag_line.split_once(char::is_whitespace) {
                Some((name, value)) => (name, value.trim()),
                None => (tag_line, ""),
            };
            tags.push(JsDocTag {
                name: name.to_lowercase(),
                value: value.to_string(),
            });
        } else if let Some(current) = tags.last_mut() {
            if !current.value.is_empty() {
                current.value.push('\n');
            }
            current.value.push_str(line.trim());
        }
    }
    tags
}

/// Parse a `@cognitive-load` value: `N`, `N/10`, or `N/10 - description`.
/// Scores outside 0-10 are not a valid declaration (rafters parser
/// behavior) and yield `None`.
fn parse_cognitive_load(value: &str) -> Option<CognitiveLoad> {
    let trimmed = value.trim();
    let digits_end = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let score: u8 = trimmed[..digits_end].parse().ok()?;
    if score > 10 {
        return None;
    }
    let mut remainder = trimmed[digits_end..].trim_start();
    remainder = remainder.strip_prefix("/10").unwrap_or(remainder);
    remainder = remainder.trim_start();
    remainder = remainder.strip_prefix('-').unwrap_or(remainder);
    let description = normalize_whitespace(remainder);
    Some(CognitiveLoad {
        score,
        description: (!description.is_empty()).then_some(description),
    })
}

/// Parse `DO:` / `NEVER:` lines from a `@usage-patterns` value. A line
/// that starts with neither marker continues the previous constraint.
fn parse_do_never_lines(value: &str, constraints: &mut Vec<Constraint>) {
    for line in value.lines() {
        let trimmed = line.trim();
        let (kind, text) = if let Some(text) = strip_prefix_ignore_case(trimmed, "DO:") {
            (Some(ConstraintKind::Do), text)
        } else if let Some(text) = strip_prefix_ignore_case(trimmed, "NEVER:") {
            (Some(ConstraintKind::Never), text)
        } else {
            (None, trimmed)
        };
        match kind {
            Some(kind) => constraints.push(Constraint {
                kind,
                text: text.trim().to_string(),
            }),
            None => {
                if let Some(previous) = constraints.last_mut() {
                    if !trimmed.is_empty() {
                        previous.text.push(' ');
                        previous.text.push_str(trimmed);
                    }
                }
            }
        }
    }
}

fn strip_prefix_ignore_case<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

// ---- token references: classes joined to declared namespace tokens ----

/// Join the classes a component uses to the tokens the namespace source
/// declares, by exact name: a class references token `t` when its utility
/// part (the last `:`-separated segment) equals `t` or ends with `-t`.
/// When several declared tokens match one class, the longest names win --
/// `ring-primary-ring` references `primary-ring`, not `ring`. Both sides
/// of the join are declared in source; nothing is invented.
fn token_references(classes: &[String], source: &IntelligenceSource) -> Vec<TokenRef> {
    let IntelligenceSource::Namespace(namespace) = source else {
        return Vec::new();
    };

    let declared: Vec<(&str, &str)> = namespace
        .namespaces
        .values()
        .flat_map(|file| {
            file.tokens
                .iter()
                .map(|token| (token.name.as_str(), file.namespace.as_str()))
        })
        .collect();

    let mut hits: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for class in classes {
        let utility = class.rsplit(':').next().unwrap_or(class.as_str());
        let matches: Vec<(&str, &str)> = declared
            .iter()
            .copied()
            .filter(|(token, _)| {
                utility == *token
                    || utility
                        .strip_suffix(token)
                        .is_some_and(|prefix| prefix.ends_with('-'))
            })
            .collect();
        let Some(longest) = matches.iter().map(|(token, _)| token.len()).max() else {
            continue;
        };
        for (token, namespace_name) in matches {
            if token.len() == longest {
                hits.entry((token.to_string(), namespace_name.to_string()))
                    .or_default()
                    .insert(class.clone());
            }
        }
    }

    hits.into_iter()
        .map(|((token, namespace_name), referenced_by)| TokenRef {
            token,
            namespace: namespace_name,
            referenced_by: referenced_by.into_iter().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rafters_source::{read_rafters_namespace, read_rafters_stylesheet};
    use crate::registry::ComponentRegistry;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render/project")
    }

    fn fixture_stylesheet() -> String {
        read_rafters_stylesheet(&fixture_root())
            .expect("fixture stylesheet must read")
            .expect("fixture project declares a compiled stylesheet")
    }

    fn discovered_items() -> (Vec<DiscoveredItem>, IntelligenceSource) {
        let root = fixture_root();
        let source = read_rafters_namespace(&root).expect("fixture namespace must read");
        let items =
            ComponentRegistry::discover(&root, &source).expect("fixture discovery must succeed");
        (items, source)
    }

    fn item_named(items: &[DiscoveredItem], name: &str) -> DiscoveredItem {
        items
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("fixture must discover an item named {name}"))
            .clone()
    }

    fn render_named(name: &str) -> RenderedComponent {
        let (items, source) = discovered_items();
        render_component(&item_named(&items, name), &source, &fixture_stylesheet())
            .unwrap_or_else(|error| panic!("{name} must render: {error}"))
    }

    // AC: each covered component renders a framework-less preview.
    #[test]
    fn component_preview_is_framework_less() {
        let rendered = render_named("Button");
        let preview = &rendered.preview;
        assert_eq!(preview.tag_name, "button-preview");
        assert!(preview.web_component.contains("extends HTMLElement"));
        assert!(preview
            .web_component
            .contains("customElements.define('button-preview'"));
        // No framework runtime referenced by the output. The shared preview
        // sheet module is the only import a preview may carry.
        assert_eq!(preview.web_component.matches("import ").count(), 1);
        assert!(preview
            .web_component
            .contains("import { previewStyles } from './preview-styles.js';"));
        assert!(!preview.web_component.contains("require("));
        assert!(!preview.web_component.to_lowercase().contains("react"));
    }

    // AC: every intelligence field present in source is exposed.
    #[test]
    fn exposes_every_intelligence_field_present_in_source() {
        let rendered = render_named("Button");
        let intelligence = &rendered.intelligence;

        // Props: the ButtonProps interface declares variant, size, loading.
        let prop_names: Vec<&str> = intelligence
            .props
            .iter()
            .map(|prop| prop.name.as_str())
            .collect();
        assert_eq!(prop_names, ["variant", "size", "loading"]);
        let variant_prop = &intelligence.props[0];
        assert!(variant_prop.optional);
        assert_eq!(
            variant_prop.type_text.as_deref(),
            Some("'default' | 'secondary'")
        );

        // Variants: the variantClasses record declares default + secondary.
        let variant_names: Vec<&str> = intelligence
            .variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect();
        assert_eq!(variant_names, ["default", "secondary"]);
        assert!(intelligence.variants[0].classes.contains("bg-primary"));

        // Cognitive load: from the @cognitive-load JSDoc tag.
        let load = intelligence
            .cognitive_load
            .as_ref()
            .expect("cognitive load is declared in the fixture JSDoc");
        assert_eq!(load.score, 3);
        assert_eq!(
            load.description.as_deref(),
            Some("Simple action trigger with clear visual hierarchy")
        );

        // Do/never: from the @usage-patterns DO:/NEVER: lines.
        assert_eq!(intelligence.do_never.len(), 3);
        assert_eq!(intelligence.do_never[0].kind, ConstraintKind::Do);
        assert_eq!(
            intelligence.do_never[0].text,
            "Primary: main user goal, maximum 1 per section"
        );
        assert_eq!(intelligence.do_never[2].kind, ConstraintKind::Never);
        assert_eq!(
            intelligence.do_never[2].text,
            "Multiple primary buttons competing for attention"
        );

        // Tokens: classes joined to the declared namespace tokens.
        let token_names: Vec<&str> = intelligence
            .tokens
            .iter()
            .map(|token| token.token.as_str())
            .collect();
        assert_eq!(
            token_names,
            ["primary", "primary-foreground", "primary-ring", "secondary"]
        );
        let primary = &intelligence.tokens[0];
        assert_eq!(primary.namespace, "semantic");
        assert_eq!(primary.referenced_by, ["bg-primary"]);
        let ring = &intelligence.tokens[2];
        assert_eq!(ring.referenced_by, ["focus-visible:ring-primary-ring"]);

        // Dependencies: the react import plus the @dependencies JSDoc tag.
        assert_eq!(
            intelligence.dependencies,
            [
                DependencyRef {
                    name: "react".to_string(),
                    origin: DependencyOrigin::Import,
                },
                DependencyRef {
                    name: "@radix-ui/react-slot".to_string(),
                    origin: DependencyOrigin::JsDocTag,
                },
            ]
        );
    }

    // AC: a field absent from source renders as absent, never synthesized.
    #[test]
    fn absent_fields_render_absent() {
        let rendered = render_named("Plain");
        let intelligence = &rendered.intelligence;
        assert!(intelligence.props.is_empty(), "no Props interface declared");
        assert!(
            intelligence.variants.is_empty(),
            "no variant record declared"
        );
        assert_eq!(
            intelligence.cognitive_load, None,
            "no @cognitive-load declared"
        );
        assert!(intelligence.do_never.is_empty(), "no usage patterns");
        assert!(
            intelligence.tokens.is_empty(),
            "flex/gap-2 reference no declared token"
        );
        assert!(intelligence.dependencies.is_empty(), "no imports, no tags");
        // The preview still renders.
        assert!(rendered.preview.web_component.contains("plain-preview"));
    }

    // AC: fields come from the declared source; with NoSource there are no
    // declared tokens to reference.
    #[test]
    fn no_source_means_no_token_references() {
        let (items, _) = discovered_items();
        let rendered = render_component(
            &item_named(&items, "Button"),
            &IntelligenceSource::NoSource,
            &fixture_stylesheet(),
        )
        .expect("Button must render without a namespace source");
        assert!(rendered.intelligence.tokens.is_empty());
        // Every other field still comes from the component source itself.
        assert!(!rendered.intelligence.props.is_empty());
        assert!(rendered.intelligence.cognitive_load.is_some());
    }

    // AC (FR-VEN-018, issue #105): a rendered preview adopts the project's
    // documentation sheet from the ONE shared module -- it carries no CSS of
    // its own, and nothing is subsetted per component.
    #[test]
    fn rendered_preview_adopts_the_shared_sheet_rather_than_embedding_css() {
        let rendered = render_named("Button");
        let js = &rendered.preview.web_component;
        assert!(js.contains("this.shadowRoot.adoptedStyleSheets = [previewStyles()]"));
        assert!(js.contains("import { previewStyles } from './preview-styles.js';"));
        // No CSS text in the preview at all -- not the rules it uses, not
        // the theme, not a constructed sheet.
        assert!(!js.contains("background-color: var(--color-primary);"));
        assert!(!js.contains(":host {"));
        assert!(!js.contains("new CSSStyleSheet()"));
    }

    // AC (FR-VEN-018): a project with no preview sheet refuses every render
    // by name rather than emitting previews that would look unstyled.
    #[test]
    fn a_missing_preview_sheet_refuses_the_render_by_name() {
        let (items, source) = discovered_items();
        let error = render_component(&item_named(&items, "Button"), &source, "")
            .expect_err("no sheet means no preview");
        let message = error.to_string();
        assert!(message.contains("Button"), "{message}");
        assert!(
            message.contains(crate::rafters_source::DOCUMENTATION_SHEET_PATH),
            "{message}"
        );
    }

    // AC (bullpen 019f1f4d): a dynamically-composed class
    // (`text-quality-${tint}`) reaches the rendered item through the real
    // pipeline -- discover -> extract -> generate. It no longer selects any
    // CSS (nothing does), but the class list is what the page reports the
    // component resolves to, so the pattern must still surface.
    #[test]
    fn dynamically_composed_classes_reach_the_rendered_preview_class_list() {
        let rendered = render_named("QualityIndicator");
        assert!(
            rendered
                .preview
                .classes_used
                .contains(&"text-quality-*".to_string()),
            "the dynamic composition must surface as a pattern: {:?}",
            rendered.preview.classes_used
        );
    }

    // AC: composites render through the same path as components.
    #[test]
    fn composite_source_renders_through_the_component_path() {
        let rendered = render_named("SplitPanel");
        assert_eq!(rendered.preview.tag_name, "split-panel-preview");
        assert!(rendered
            .preview
            .web_component
            .contains("customElements.define('split-panel-preview'"));
        let variant_names: Vec<&str> = rendered
            .intelligence
            .variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect();
        assert_eq!(variant_names, ["default", "stacked"]);
    }

    // AC: composites render through the same path -- a manifest composite
    // renders a passthrough preview plus the manifest's intelligence.
    #[test]
    fn composite_manifest_renders_preview_and_manifest_intelligence() {
        let rendered = render_named("hero-banner");
        assert_eq!(rendered.preview.tag_name, "hero-banner-preview");
        assert!(rendered
            .preview
            .web_component
            .contains("customElements.define('hero-banner-preview'"));
        assert_eq!(
            rendered.preview.web_component.matches("import ").count(),
            1,
            "a passthrough preview carries the shared sheet import and nothing else"
        );

        let intelligence = &rendered.intelligence;
        let load = intelligence
            .cognitive_load
            .as_ref()
            .expect("manifest declares cognitiveLoad");
        assert_eq!(load.score, 3);
        assert_eq!(load.description, None, "manifest declares a bare number");
        assert_eq!(intelligence.do_never.len(), 3);
        assert_eq!(intelligence.do_never[0].kind, ConstraintKind::Do);
        assert_eq!(intelligence.do_never[2].kind, ConstraintKind::Never);
        // The manifest declares none of these; they are absent.
        assert!(intelligence.props.is_empty());
        assert!(intelligence.variants.is_empty());
        assert!(intelligence.tokens.is_empty());
        assert!(intelligence.dependencies.is_empty());
    }

    // AC: a component that fails to transform produces a named error,
    // never a silent disappearance.
    #[test]
    fn failing_component_is_a_named_error() {
        let (items, source) = discovered_items();
        let error = render_component(
            &item_named(&items, "Broken"),
            &source,
            &fixture_stylesheet(),
        )
        .expect_err("an unparseable component must fail to render");
        match &error {
            TransformError::RenderFailed { component, .. } => {
                assert_eq!(component, "Broken");
            }
            other => panic!("expected RenderFailed, got {other:?}"),
        }
        assert!(error.to_string().contains("Broken"), "{error}");
    }

    #[test]
    fn installed_only_item_is_a_named_error_pointing_at_the_config() {
        let (items, source) = discovered_items();
        let error = render_component(
            &item_named(&items, "ghost-widget"),
            &source,
            &fixture_stylesheet(),
        )
        .expect_err("an installed name with no source file cannot render");
        let message = error.to_string();
        assert!(message.contains("ghost-widget"), "{message}");
        assert!(message.contains("config.rafters.json"), "{message}");
    }

    // Every discovered item either renders or produces a named error --
    // the full fixture project, end to end, nothing silently dropped.
    #[test]
    fn every_discovered_item_renders_or_errors_by_name() {
        let (items, source) = discovered_items();
        let full_css = fixture_stylesheet();
        assert!(
            items.len() >= 6,
            "fixture must discover its items: {items:#?}"
        );
        for item in &items {
            match render_component(item, &source, &full_css) {
                Ok(rendered) => {
                    assert!(rendered.preview.web_component.contains("HTMLElement"));
                }
                Err(TransformError::RenderFailed { component, .. }) => {
                    assert_eq!(&component, &item.name);
                }
                Err(other) => panic!("failures must be named RenderFailed, got {other:?}"),
            }
        }
    }

    // Drives the real rafters checkout when available. Run with:
    //   VENEER_REAL_RAFTERS_ROOT=/path/to/rafters \
    //     cargo test -p veneer-adapters -- --ignored real_rafters
    #[test]
    #[ignore = "requires a local rafters checkout via VENEER_REAL_RAFTERS_ROOT"]
    fn real_rafters_components_render_or_error_by_name() {
        let Ok(root) = std::env::var("VENEER_REAL_RAFTERS_ROOT") else {
            eprintln!("VENEER_REAL_RAFTERS_ROOT not set; skipping");
            return;
        };
        let root = PathBuf::from(root);
        let source = read_rafters_namespace(&root).expect("real namespace must read");
        assert!(
            matches!(source, IntelligenceSource::Namespace(_)),
            "the real checkout declares a .rafters/ namespace"
        );
        let items = ComponentRegistry::discover(&root, &source).expect("real discovery");
        let full_css = read_rafters_stylesheet(&root)
            .expect("real stylesheet must read")
            .unwrap_or_default();
        let mut rendered_count = 0usize;
        let mut with_cognitive_load = 0usize;
        let mut with_tokens = 0usize;
        for item in &items {
            match render_component(item, &source, &full_css) {
                Ok(rendered) => {
                    rendered_count += 1;
                    if rendered.intelligence.cognitive_load.is_some() {
                        with_cognitive_load += 1;
                    }
                    if !rendered.intelligence.tokens.is_empty() {
                        with_tokens += 1;
                    }
                }
                Err(TransformError::RenderFailed { component, .. }) => {
                    assert_eq!(&component, &item.name);
                }
                Err(other) => panic!("failures must be named RenderFailed, got {other:?}"),
            }
        }
        eprintln!(
            "real rafters: {} discovered, {} rendered, {} with cognitive load, {} with tokens",
            items.len(),
            rendered_count,
            with_cognitive_load,
            with_tokens
        );
        // FR-VEN-018 wires every preview to the real compiled documentation
        // sheet. A checkout that has never regenerated one correctly refuses
        // every preview with a named error instead of rendering them
        // unstyled, so the render counts are informational here; the
        // assertion is that every item either renders or errors by name
        // (checked in the loop above).
        assert!(!items.is_empty(), "the real checkout must discover items");
    }

    // ---- unit coverage for the parsers ----

    #[test]
    fn cognitive_load_parses_canonical_and_bare_forms() {
        let full = parse_cognitive_load("3/10 - Simple action trigger").expect("full form");
        assert_eq!(full.score, 3);
        assert_eq!(full.description.as_deref(), Some("Simple action trigger"));

        let bare = parse_cognitive_load("7").expect("bare form");
        assert_eq!(bare.score, 7);
        assert_eq!(bare.description, None);

        assert_eq!(parse_cognitive_load("11/10 - impossible"), None);
        assert_eq!(parse_cognitive_load("high"), None);
    }

    #[test]
    fn legacy_do_never_tags_are_collected() {
        let source = r#"
/**
 * Widget.
 *
 * @do Pair with a label
 * @never Use for primary navigation
 */
export const widgetBaseClasses = 'flex';
        "#;
        let mut intelligence = JsDocIntelligence::default();
        collect_jsdoc_intelligence(source, &mut intelligence);
        assert_eq!(
            intelligence.do_never,
            [
                Constraint {
                    kind: ConstraintKind::Do,
                    text: "Pair with a label".to_string(),
                },
                Constraint {
                    kind: ConstraintKind::Never,
                    text: "Use for primary navigation".to_string(),
                },
            ]
        );
    }

    #[test]
    fn preview_tag_names_are_kebab_cased() {
        assert_eq!(preview_tag_name("Button"), "button-preview");
        assert_eq!(preview_tag_name("SplitPanel"), "split-panel-preview");
        assert_eq!(preview_tag_name("hero-banner"), "hero-banner-preview");
    }

    #[test]
    fn token_matching_prefers_the_longest_declared_name() {
        let root = fixture_root();
        let source = read_rafters_namespace(&root).expect("fixture namespace");
        let classes = vec![
            "focus-visible:ring-primary-ring".to_string(),
            "bg-primary".to_string(),
        ];
        let tokens = token_references(&classes, &source);
        let names: Vec<&str> = tokens.iter().map(|token| token.token.as_str()).collect();
        // primary-ring wins over its prefix token primary for the ring class.
        assert_eq!(names, ["primary", "primary-ring"]);
        assert_eq!(tokens[1].referenced_by, ["focus-visible:ring-primary-ring"]);
    }

    #[test]
    fn config_props_are_read_from_the_behavior_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tsx = dir.path().join("widget.tsx");
        let own = "export function Widget() { return null; }";
        std::fs::write(&tsx, own).expect("write tsx");
        std::fs::write(
            dir.path().join("widget.behavior.ts"),
            "export interface WidgetConfig extends BaseConfig { label: string; open?: boolean; }",
        )
        .expect("write behavior");

        let config = read_component_config(&tsx)
            .expect("reads")
            .expect("the behavior sibling declares a Config");
        assert_eq!(config.name, "WidgetConfig");
        // BaseConfig has no import in the behavior file, so it stays an
        // honest unresolved base rather than silently vanishing.
        assert_eq!(config.unresolved_extends, ["BaseConfig"]);
        let names: Vec<&str> = config.props.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["label", "open"]);
    }

    #[test]
    fn a_component_with_no_behavior_file_has_no_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tsx = dir.path().join("legacy.tsx");
        let own = "export interface LegacyProps { title: string; } export function Legacy() {}";
        std::fs::write(&tsx, own).expect("write tsx");
        assert_eq!(read_component_config(&tsx).expect("reads"), None);
    }
}
