//! Component registry for looking up component definitions.
//!
//! Scans a components directory, parses source files, and provides
//! lookup by component name for generating Web Components.
//!
//! Uses export discovery instead of hardcoded naming conventions:
//! reads every `export const` in a `.classes.ts` file, categorizes
//! each export by its shape and name suffix, and builds the component
//! record from what actually exists.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPatternKind, Declaration, Expression, ObjectPropertyKind, PropertyKey, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::generator::preview_web_component_block;
use crate::rafters_source::IntelligenceSource;
use crate::react::{ComponentStructure, ReactAdapter};
use crate::scope::collect_classes_from_expr;
use crate::traits::{TransformError, TransformedBlock};
use crate::ts_helpers::{
    extract_class_value, extract_nested_object_classes, extract_string_value, normalize_whitespace,
    unwrap_type_expressions,
};

/// A registry of component definitions.
#[derive(Debug, Default)]
pub struct ComponentRegistry {
    /// Cached component structures by name (lowercase)
    components: HashMap<String, CachedComponent>,
}

/// A cached component with its source and structure.
#[derive(Debug, Clone)]
pub struct CachedComponent {
    /// Original component name
    pub name: String,

    /// Source file path
    pub source_path: PathBuf,

    /// Extracted structure
    pub structure: ComponentStructure,

    /// Full source code
    pub source: String,
}

/// Whether a discovered item is a component or a composite (FR-VEN-017).
///
/// Kind comes from what the source declares -- never from naming patterns.
/// The rafters source tree declares compositeness in exactly three places
/// (verified against the real repo):
///
/// - a `*.composite.json` manifest file (for example
///   `packages/ui/src/composites/hero-banner.composite.json`)
/// - residence under the `compositesPath` directory declared in
///   `.rafters/config.rafters.json`
/// - a name listed in `installed.composites` of that same config
///
/// Everything else the component source walk yields is a component: the
/// walk itself only visits component source files, and `componentsPath` /
/// `installed.components` declare components explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveredKind {
    Component,
    Composite,
}

impl DiscoveredKind {
    /// The lowercase kind label used across every veneer output surface
    /// (docs/index line `kind` fields, page frontmatter). One mapping, owned
    /// by the type, so surfaces cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveredKind::Component => "component",
            DiscoveredKind::Composite => "composite",
        }
    }
}

/// One component or composite found in a project by [`ComponentRegistry::discover`].
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredItem {
    /// Declared name: the extracted component name, the composite manifest
    /// `manifest.id`, or the name listed in the rafters config.
    pub name: String,
    /// Component or composite, from what the source declares.
    pub kind: DiscoveredKind,
    /// Where the item is declared: the source file, the composite manifest,
    /// or `.rafters/config.rafters.json` for installed-only declarations.
    pub source_path: PathBuf,
    /// present in source but not yet generated (feeds FR-VEN-009)
    ///
    /// True when the extract pass produced a component structure -- the
    /// input generation renders from. False when the item exists in source
    /// but nothing is generatable from it yet (unparseable file, manifest
    /// without a generation pipeline, installed name with no source file),
    /// so it is reportable as "not yet documented" instead of silently
    /// absent.
    pub generated: bool,
    /// Set exactly when this item's source is a `.tsx`/`.jsx` file under a
    /// project whose declared `componentTarget` names a framework veneer has
    /// no adapter for (FR-VEN-033). The declared value, never a guess: the
    /// item is discovered (it exists in source) but its source is never
    /// parsed for props -- an unsupported framework is an observation, not
    /// an inference. `None` for every other item, including `.classes.ts`
    /// sources, which read identically regardless of the declared target.
    pub unsupported_framework: Option<String>,
}

/// The subset of `.rafters/config.rafters.json` that declares components
/// and composites. Grounded against the real file: it also declares
/// `primitivesPath` / `installed.primitives`, which FR-VEN-017's
/// [`DiscoveredKind`] cannot represent, so primitives are not read here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RaftersConfig {
    composites_path: Option<String>,
    #[serde(default)]
    installed: InstalledDeclarations,
}

/// The `installed` block of the rafters config: names the project has
/// installed, declared per kind.
#[derive(Debug, Default, Deserialize)]
struct InstalledDeclarations {
    #[serde(default)]
    components: Vec<String>,
    #[serde(default)]
    composites: Vec<String>,
}

/// A `*.composite.json` manifest, read only for the declared identifier.
#[derive(Debug, Deserialize)]
struct CompositeManifestFile {
    manifest: CompositeManifest,
}

/// The `manifest` block of a composite manifest file.
#[derive(Debug, Deserialize)]
struct CompositeManifest {
    id: String,
}

/// How a candidate component source file is extracted.
#[derive(Debug, Clone, Copy)]
enum SourceFileKind {
    /// `.classes.ts` -- export-discovery extraction.
    ClassesTs,
    /// `.tsx` / `.jsx` -- conventions-based extraction.
    ComponentModule,
}

/// A single discovered export from a .classes.ts file.
#[derive(Debug)]
struct DiscoveredExport {
    name: String,
    shape: ExportShape,
    /// `prefix-*` patterns for class tokens the export composes
    /// dynamically at render (FR-VEN-018 tree-shake caveat).
    patterns: Vec<String>,
}

/// The value shape of a discovered export.
#[derive(Debug)]
enum ExportShape {
    /// Key-value pairs (from flat or nested object expressions)
    Record { entries: Vec<(String, String)> },
    /// A single resolved class string (from string literal, template, array, or join)
    Scalar { value: String },
}

/// Categorize an export name based on what role it plays in the component.
///
/// Priority order: Variant > Size > Base > Disabled > Other.
/// First match wins, so `variantSizeClasses` would classify as Variant.
#[derive(Debug, PartialEq)]
enum ExportRole {
    Variant,
    Size,
    Base,
    Disabled,
    Other,
}

impl ExportRole {
    fn classify(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.contains("variant") {
            Self::Variant
        } else if lower.contains("size") {
            Self::Size
        } else if lower.contains("base") {
            Self::Base
        } else if lower.contains("disabled") {
            Self::Disabled
        } else {
            Self::Other
        }
    }
}

/// Discover all exported constants from a TypeScript source file.
///
/// This is the core of the discovery approach: instead of looking for
/// specific variable names, we read every `export const` and determine
/// its shape (flat record, nested record, string, array, object).
fn discover_exports(source: &str, file_hint: &str) -> Vec<DiscoveredExport> {
    let allocator = Allocator::default();
    let source_type = SourceType::ts();
    let ret = Parser::new(&allocator, source, source_type).parse();

    if ret.panicked {
        eprintln!("warning: parser panicked on {file_hint}");
        return Vec::new();
    }
    if !ret.errors.is_empty() {
        eprintln!(
            "warning: parse errors in {file_hint}: {}",
            ret.errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        );
        return Vec::new();
    }

    let mut exports = Vec::new();

    for stmt in &ret.program.body {
        let Statement::ExportNamedDeclaration(export) = stmt else {
            continue;
        };
        let Some(ref decl) = export.declaration else {
            continue;
        };
        let Declaration::VariableDeclaration(var_decl) = decl else {
            continue;
        };

        for declarator in &var_decl.declarations {
            let name = match &declarator.id.kind {
                BindingPatternKind::BindingIdentifier(id) => id.name.as_str().to_string(),
                _ => continue,
            };

            let Some(ref init) = declarator.init else {
                continue;
            };

            let init = unwrap_type_expressions(init);

            if let Some(export) = try_extract_export(&name, init) {
                exports.push(export);
            }
        }
    }

    exports
}

/// Try to extract a DiscoveredExport from an expression.
fn try_extract_export(name: &str, expr: &Expression<'_>) -> Option<DiscoveredExport> {
    let mut patterns: Vec<String> = Vec::new();

    // Try as object expression first (Records and plain objects)
    if let Expression::ObjectExpression(obj) = expr {
        let mut flat_entries: Vec<(String, String)> = Vec::new();
        let mut nested_entries: Vec<(String, String)> = Vec::new();
        let mut has_nested = false;

        for prop in &obj.properties {
            let ObjectPropertyKind::ObjectProperty(prop) = prop else {
                continue;
            };

            let key = match &prop.key {
                PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                PropertyKey::NumericLiteral(n) => {
                    // Use integer formatting if it's a whole number, otherwise float
                    let v = n.value;
                    if v == (v as i64) as f64 {
                        format!("{}", v as i64)
                    } else {
                        format!("{}", v)
                    }
                }
                _ => continue,
            };

            let value_expr = unwrap_type_expressions(&prop.value);

            // Try string-shaped value first; dynamically-composed tokens
            // land in `patterns` while the entry keeps its static classes.
            if let Some(value) = extract_class_value(value_expr) {
                flat_entries.push((key.clone(), value.static_classes.clone()));
                nested_entries.push((key, value.static_classes));
                patterns.extend(value.patterns);
            }
            // Try nested object (flatten all string values)
            else if let Some(value) = extract_nested_object_classes(value_expr, &mut patterns) {
                nested_entries.push((key, value));
                has_nested = true;
            }
        }

        let entries = if has_nested {
            nested_entries
        } else {
            flat_entries
        };
        if !entries.is_empty() {
            return Some(DiscoveredExport {
                name: name.to_string(),
                shape: ExportShape::Record { entries },
                patterns,
            });
        }

        // Object with no extractable values -- still record it if it has properties
        // (might be entirely nested objects we could not parse)
        return None;
    }

    // Try as string literal / template literal / concatenation. A token
    // composed dynamically at render becomes a `prefix-*` pattern
    // (FR-VEN-018): the resolved names never appear as source literals.
    if let Some(value) = extract_class_value(expr) {
        if value.static_classes.is_empty() && value.patterns.is_empty() {
            return None;
        }
        return Some(DiscoveredExport {
            name: name.to_string(),
            shape: ExportShape::Scalar {
                value: value.static_classes,
            },
            patterns: value.patterns,
        });
    }

    // Try as array expression (join elements)
    if let Expression::ArrayExpression(arr) = expr {
        let parts = collect_array_string_values(arr);
        if !parts.is_empty() {
            return Some(DiscoveredExport {
                name: name.to_string(),
                shape: ExportShape::Scalar {
                    value: parts.join(" "),
                },
                patterns,
            });
        }
    }

    // Try as a class-builder arrow (for example
    // `(tint) => \`text-quality-${tint}\``): everything the builder can
    // return is a class the component resolves to at render.
    if let Expression::ArrowFunctionExpression(_) = expr {
        let mut seen: HashSet<String> = HashSet::new();
        let mut tokens: Vec<String> = Vec::new();
        collect_classes_from_expr(expr, &mut seen, &mut tokens);
        let (dynamic, statics): (Vec<String>, Vec<String>) =
            tokens.into_iter().partition(|token| token.ends_with('*'));
        if statics.is_empty() && dynamic.is_empty() {
            return None;
        }
        return Some(DiscoveredExport {
            name: name.to_string(),
            shape: ExportShape::Scalar {
                value: statics.join(" "),
            },
            patterns: dynamic,
        });
    }

    // Try as method call -- e.g., [...].join(' ')
    if let Expression::CallExpression(call) = expr {
        if let Expression::StaticMemberExpression(member) = &call.callee {
            if member.property.name.as_str() == "join" {
                if let Expression::ArrayExpression(arr) = &member.object {
                    let parts = collect_array_string_values(arr);
                    if !parts.is_empty() {
                        let sep = call
                            .arguments
                            .first()
                            .and_then(|arg| {
                                if let oxc_ast::ast::Argument::StringLiteral(s) = arg {
                                    Some(s.value.as_str().to_string())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_else(|| " ".to_string());
                        return Some(DiscoveredExport {
                            name: name.to_string(),
                            shape: ExportShape::Scalar {
                                value: parts.join(&sep),
                            },
                            patterns,
                        });
                    }
                }
            }
        }
    }

    None
}

/// Extract non-empty string values from array elements.
fn collect_array_string_values(arr: &oxc_ast::ast::ArrayExpression<'_>) -> Vec<String> {
    let mut parts = Vec::new();
    for element in &arr.elements {
        let expr_ref = match element {
            oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => continue,
            oxc_ast::ast::ArrayExpressionElement::Elision(_) => continue,
            _ => element.to_expression(),
        };
        if let Some(value) = extract_string_value(expr_ref) {
            if !value.is_empty() {
                parts.push(value);
            }
        }
    }
    parts
}

/// Build a ComponentStructure from discovered exports.
///
/// Uses the export name to classify each export's role (variant, size,
/// base, disabled, other) and then assembles the structure from whatever
/// was found. Never rejects a file for missing a specific export name.
fn build_structure_from_exports(
    component_name: &str,
    exports: Vec<DiscoveredExport>,
) -> Option<ComponentStructure> {
    if exports.is_empty() {
        return None;
    }

    let mut variant_lookup: Vec<(String, String)> = Vec::new();
    let mut size_lookup: Vec<(String, String)> = Vec::new();
    let mut base_classes_parts: Vec<String> = Vec::new();
    let mut disabled_classes: Option<String> = None;
    let mut extra_classes: Vec<String> = Vec::new();
    let mut dynamic_class_patterns: Vec<String> = Vec::new();

    for export in exports {
        let role = ExportRole::classify(&export.name);
        dynamic_class_patterns.extend(export.patterns);

        match (role, export.shape) {
            (ExportRole::Variant, ExportShape::Record { entries }) => {
                if variant_lookup.is_empty() {
                    variant_lookup = entries;
                } else {
                    for (_, v) in entries {
                        extra_classes.push(v);
                    }
                }
            }

            (ExportRole::Size, ExportShape::Record { entries }) => {
                if size_lookup.is_empty() {
                    size_lookup = entries;
                } else {
                    for (_, v) in entries {
                        extra_classes.push(v);
                    }
                }
            }

            (ExportRole::Base, ExportShape::Scalar { value }) => {
                base_classes_parts.push(value);
            }

            (ExportRole::Disabled, ExportShape::Scalar { value }) => {
                disabled_classes = Some(value);
            }

            (ExportRole::Other, ExportShape::Record { entries }) => {
                for (_, v) in entries {
                    if !v.is_empty() {
                        extra_classes.push(v);
                    }
                }
            }

            (ExportRole::Other, ExportShape::Scalar { value }) => {
                if !value.is_empty() {
                    base_classes_parts.push(value);
                }
            }

            (ExportRole::Base, ExportShape::Record { entries }) => {
                for (_, v) in entries {
                    if !v.is_empty() {
                        base_classes_parts.push(v);
                    }
                }
            }

            (ExportRole::Disabled, ExportShape::Record { entries }) => {
                let combined: String = entries
                    .into_iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(_, v)| v)
                    .collect::<Vec<_>>()
                    .join(" ");
                if !combined.is_empty() {
                    disabled_classes = Some(combined);
                }
            }

            // Variant/Size role but scalar value -- treat as base classes
            (ExportRole::Variant | ExportRole::Size, ExportShape::Scalar { value }) => {
                if !value.is_empty() {
                    base_classes_parts.push(value);
                }
            }
        }
    }

    // Combine base classes -- always include extra_classes (from "Other" role records)
    // alongside any explicit base classes found.
    let mut all_base_parts: Vec<String> = base_classes_parts;
    all_base_parts.extend(extra_classes);
    let base_classes = all_base_parts.join(" ");

    // We found something -- build the structure. Dynamic patterns count:
    // a component whose only classes are composed at render still has
    // CSS to scope (FR-VEN-018).
    let has_content = !variant_lookup.is_empty()
        || !size_lookup.is_empty()
        || !base_classes.is_empty()
        || !dynamic_class_patterns.is_empty();

    if !has_content {
        return None;
    }

    dynamic_class_patterns.sort();
    dynamic_class_patterns.dedup();

    let default_variant = variant_lookup
        .first()
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "default".to_string());

    let default_size = size_lookup
        .first()
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| "default".to_string());

    // Infer observed attributes from what we found
    let mut observed_attributes = Vec::new();
    if !variant_lookup.is_empty() {
        observed_attributes.push("variant".to_string());
    }
    if !size_lookup.is_empty() {
        observed_attributes.push("size".to_string());
    }

    Some(ComponentStructure {
        name: component_name.to_string(),
        variant_lookup,
        size_lookup,
        base_classes: normalize_whitespace(&base_classes),
        disabled_classes: disabled_classes
            .unwrap_or_else(|| "opacity-50 pointer-events-none cursor-not-allowed".to_string()),
        default_variant,
        default_size,
        observed_attributes,
        dynamic_class_patterns,
    })
}

/// Classify a filename as a component source candidate, applying the shared
/// skip rules (tests, specs, stories, index files).
fn candidate_file_kind(filename: &str, path: &Path) -> Option<SourceFileKind> {
    if filename.contains(".test.")
        || filename.contains(".spec.")
        || filename.contains(".stories.")
        || filename == "index.tsx"
        || filename == "index.jsx"
        || filename == "index.ts"
    {
        return None;
    }

    if filename.ends_with(".classes.ts") {
        return Some(SourceFileKind::ClassesTs);
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") | Some("jsx") => Some(SourceFileKind::ComponentModule),
        _ => None,
    }
}

/// Extract a component name and, when extraction succeeds, its structure.
///
/// The name comes from the source when the source declares one, falling
/// back to the file stem. An `Err` structure means the file is present in
/// source but nothing is generatable from it, with the reason -- callers
/// decide whether that is a skip ([`ComponentRegistry::scan`]), an
/// explicit not-yet-generated item ([`ComponentRegistry::discover`]), or
/// a named render failure (`render_component`).
fn extract_from_source(
    adapter: &ReactAdapter,
    file_kind: SourceFileKind,
    filename: &str,
    path: &Path,
    source: &str,
) -> (String, Result<ComponentStructure, TransformError>) {
    match file_kind {
        SourceFileKind::ClassesTs => {
            let stem = filename.strip_suffix(".classes.ts").unwrap_or("unknown");
            let component_name = kebab_to_pascal(stem);
            let exports = discover_exports(source, filename);
            let structure = build_structure_from_exports(&component_name, exports)
                .ok_or(TransformError::MissingVariants);
            (component_name, structure)
        }
        SourceFileKind::ComponentModule => match adapter.extract_structure(source) {
            Ok(structure) => {
                let name = if structure.name.is_empty() || structure.name == "Component" {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string()
                } else {
                    structure.name.clone()
                };
                (name, Ok(structure))
            }
            Err(error) => {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                (kebab_to_pascal(stem), Err(error))
            }
        },
    }
}

/// Classify and extract one source file as a component candidate, for
/// callers outside the walk (`render_component`). `None` when the file is
/// not a component source candidate at all (wrong extension, test/story
/// file); otherwise the declared name plus the structure or the named
/// reason it is not generatable.
pub(crate) fn extract_component_candidate(
    path: &Path,
    source: &str,
) -> Option<(String, Result<ComponentStructure, TransformError>)> {
    let filename = path.file_name().and_then(|name| name.to_str())?;
    let file_kind = candidate_file_kind(filename, path)?;
    Some(extract_from_source(
        &ReactAdapter::new(),
        file_kind,
        filename,
        path,
        source,
    ))
}

/// True when a directory name is excluded from the discovery/mode-detection
/// walk: hidden directories, dependency and build output. The single source
/// of truth for that exclusion list -- [`is_walkable_entry`] (the `WalkDir`
/// predicate) and veneer's watch mode (which mirrors this same input
/// contract when deciding whether a filesystem change should trigger a
/// re-derivation) both call this rather than each keeping their own copy of
/// the name list.
pub fn is_excluded_dir_name(name: &str) -> bool {
    name.starts_with('.')
        || name == "node_modules"
        || name == "target"
        || name == "dist"
        || name == "build"
}

/// Directory filter for the discovery walk: descend everywhere except
/// dependency, build-output, and hidden directories (`.rafters/` is read
/// separately via its config, not walked for component source). Shared with
/// [`crate::mode::detect_mode`]'s behavior-file walk so the two walks agree
/// on what counts as project source.
pub(crate) fn is_walkable_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_str().unwrap_or("");
    !is_excluded_dir_name(name)
}

/// Read the component/composite declarations from
/// `.rafters/config.rafters.json`, if the file exists. Returns the config
/// path alongside the parsed config so installed-only items can point at
/// the file that declares them.
fn read_rafters_config(
    project_root: &Path,
) -> Result<Option<(PathBuf, RaftersConfig)>, RegistryError> {
    let path = project_root.join(".rafters").join("config.rafters.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|source| RegistryError::UnreadableSource {
        path: path.clone(),
        source,
    })?;
    let config: RaftersConfig =
        serde_json::from_str(&text).map_err(|error| RegistryError::MalformedDeclaration {
            path: path.clone(),
            message: error.to_string(),
        })?;
    Ok(Some((path, config)))
}

/// True when the path names a `*.composite.json` manifest -- the one file
/// format that declares a composite by itself (see [`DiscoveredKind`]).
/// The single suffix predicate every consumer of the manifest format
/// shares: discovery, rendering, and artifact emission.
pub(crate) fn is_composite_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".composite.json"))
}

/// Read the declared identifier from a `*.composite.json` manifest.
/// A manifest that does not declare `manifest.id` is a named error, never
/// a silently dropped composite.
fn read_composite_manifest_name(path: &Path) -> Result<String, RegistryError> {
    let text = fs::read_to_string(path).map_err(|source| RegistryError::UnreadableSource {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: CompositeManifestFile =
        serde_json::from_str(&text).map_err(|error| RegistryError::MalformedDeclaration {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    Ok(parsed.manifest.id)
}

/// Canonical fold for discovered-item identity. The same physical item is
/// named in different casing conventions by the three declaration
/// mechanisms: kebab-case by composite manifest ids and the `installed`
/// lists (`hero-banner`), PascalCase by source extraction (`HeroBanner`).
/// Folding lowercases and drops the word separators those conventions
/// disagree on, so both spellings key the same dedup slot.
fn identity_fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Insert an item into the discovered set, merging duplicates of the same
/// (folded name, kind) -- for example `button.tsx` alongside
/// `button.classes.ts`, or a `hero-banner` manifest alongside a
/// `HeroBanner` source file (see [`identity_fold`]). A generatable sighting
/// wins over a non-generatable one; otherwise the first sighting
/// (deterministic, the walk is sorted) is kept.
fn record_discovered(
    set: &mut BTreeMap<(String, DiscoveredKind), DiscoveredItem>,
    item: DiscoveredItem,
) {
    match set.entry((identity_fold(&item.name), item.kind)) {
        std::collections::btree_map::Entry::Vacant(slot) => {
            slot.insert(item);
        }
        std::collections::btree_map::Entry::Occupied(mut slot) => {
            let existing = slot.get_mut();
            if !existing.generated && item.generated {
                *existing = item;
            }
        }
    }
}

impl ComponentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan a directory for component files and populate the registry.
    ///
    /// For `.classes.ts` files, uses export discovery: reads every `export const`,
    /// categorizes by name pattern and value shape, builds component records from
    /// whatever is actually exported. Never rejects a file for not matching a
    /// specific naming convention.
    ///
    /// For `.tsx`/`.jsx` files, uses the ReactAdapter with conventions-based
    /// extraction (unchanged -- these are component source files, not class defs).
    pub fn scan(&mut self, components_dir: &Path) -> Result<usize, RegistryError> {
        if !components_dir.exists() {
            return Err(RegistryError::DirectoryNotFound(
                components_dir.display().to_string(),
            ));
        }

        let default_adapter = ReactAdapter::new();
        let mut count = 0;

        for entry in WalkDir::new(components_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            let Some(file_kind) = candidate_file_kind(filename, path) else {
                continue;
            };

            // Read source
            let source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let (name, structure) =
                extract_from_source(&default_adapter, file_kind, filename, path, &source);
            let Ok(structure) = structure else {
                continue;
            };

            let cached = CachedComponent {
                name: name.clone(),
                source_path: path.to_path_buf(),
                structure,
                source,
            };

            // Store by lowercase name for case-insensitive lookup.
            // Only insert if not already present (component files take priority over classes files).
            let key = name.to_lowercase();
            if let std::collections::hash_map::Entry::Vacant(e) = self.components.entry(key) {
                e.insert(cached);
                count += 1;
            }
        }

        Ok(count)
    }

    /// Look up a component by name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&CachedComponent> {
        self.components.get(&name.to_lowercase())
    }

    /// Check if a component exists.
    pub fn contains(&self, name: &str) -> bool {
        self.components.contains_key(&name.to_lowercase())
    }

    /// Get all registered component names.
    pub fn names(&self) -> Vec<&str> {
        self.components.values().map(|c| c.name.as_str()).collect()
    }

    /// Generate a Web Component for a registered component, scoping its
    /// CSS out of `full_css` (the project stylesheet text) for the shadow
    /// root. Extraction failure is an error naming the component -- a
    /// preview never renders silently unstyled (FR-VEN-018).
    pub fn generate_web_component(
        &self,
        component_name: &str,
        tag_name: &str,
        full_css: &str,
    ) -> Result<TransformedBlock, RegistryError> {
        let cached = self
            .get(component_name)
            .ok_or_else(|| RegistryError::ComponentNotFound(component_name.to_string()))?;

        preview_web_component_block(tag_name, &cached.structure, full_css).map_err(|error| {
            RegistryError::PreviewCss {
                component: cached.name.clone(),
                message: error.to_string(),
            }
        })
    }

    /// Enumerate every component and composite the project source declares
    /// (FR-VEN-017): the union of the component source walk (the existing
    /// oxc extract pass) and the `.rafters/` namespace declarations
    /// (`compositesPath` and `installed` in `config.rafters.json`).
    ///
    /// - Kind comes from declarations only (see [`DiscoveredKind`]), never
    ///   from naming patterns.
    /// - An item whose source exists but yields nothing generatable appears
    ///   with `generated: false` instead of being silently dropped.
    /// - An unreadable source file is a named error
    ///   ([`RegistryError::UnreadableSource`]); a declaration file that
    ///   does not parse is [`RegistryError::MalformedDeclaration`].
    /// - Ordering is deterministic: a stable sort by name.
    ///
    /// With [`IntelligenceSource::NoSource`] there is no `.rafters/`
    /// directory, so only the component source walk contributes.
    pub fn discover(
        project_root: &Path,
        source: &IntelligenceSource,
    ) -> Result<Vec<DiscoveredItem>, RegistryError> {
        if !project_root.is_dir() {
            return Err(RegistryError::DirectoryNotFound(
                project_root.display().to_string(),
            ));
        }

        let config = match source {
            IntelligenceSource::Namespace(_) => read_rafters_config(project_root)?,
            IntelligenceSource::NoSource => None,
        };
        let composites_root: Option<PathBuf> = config
            .as_ref()
            .and_then(|(_, config)| config.composites_path.as_deref())
            .map(|declared| project_root.join(declared));

        // Framework dispatch (FR-VEN-033): `componentTarget` is an input
        // fact read from `.rafters/config.rafters.json`, never guessed. A
        // declared, unsupported target is recorded as an observation on the
        // discovered items below -- their `.tsx`/`.jsx` source is never
        // parsed for props, since veneer has no adapter for the shape.
        // `.classes.ts` extraction does not depend on this dispatch at all
        // (see `extract_from_source`'s `ClassesTs` arm), so it reads
        // identically regardless of the declared target.
        let declaration =
            crate::rafters_source::read_framework_declaration(project_root).map_err(|error| {
                RegistryError::MalformedDeclaration {
                    path: project_root.join(".rafters").join("config.rafters.json"),
                    message: error.to_string(),
                }
            })?;
        let unsupported_target: Option<String> =
            match crate::mode::dispatch_framework(declaration.component_target.as_deref()) {
                crate::mode::FrameworkDispatch::Supported(_) => None,
                crate::mode::FrameworkDispatch::Unsupported { declared } => Some(declared),
            };

        let adapter = ReactAdapter::new();
        let mut discovered: BTreeMap<(String, DiscoveredKind), DiscoveredItem> = BTreeMap::new();

        for entry in WalkDir::new(project_root)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(is_walkable_entry)
        {
            let entry = entry.map_err(|error| RegistryError::WalkFailed {
                path: error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| project_root.to_path_buf()),
                message: error.to_string(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // A composite manifest declares a composite by its format.
            if is_composite_manifest(path) {
                let name = read_composite_manifest_name(path)?;
                record_discovered(
                    &mut discovered,
                    DiscoveredItem {
                        name,
                        kind: DiscoveredKind::Composite,
                        source_path: path.to_path_buf(),
                        generated: false,
                        unsupported_framework: None,
                    },
                );
                continue;
            }

            let Some(file_kind) = candidate_file_kind(filename, path) else {
                continue;
            };
            let source_text =
                fs::read_to_string(path).map_err(|source| RegistryError::UnreadableSource {
                    path: path.to_path_buf(),
                    source,
                })?;

            // A `.tsx`/`.jsx` source under a declared, unsupported
            // `componentTarget` is never handed to `ReactAdapter`: that
            // would infer props from a shape veneer has no adapter for.
            // `.classes.ts` is unaffected -- extract_from_source's
            // `ClassesTs` arm never reads the adapter argument.
            let is_unsupported_module = matches!(file_kind, SourceFileKind::ComponentModule)
                && unsupported_target.is_some();
            let (name, structure) = if is_unsupported_module {
                let declared = unsupported_target.clone().unwrap_or_default();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                let name = kebab_to_pascal(stem);
                (
                    name.clone(),
                    Err(TransformError::RenderFailed {
                        component: name,
                        reason: format!(
                            "componentTarget \"{declared}\" has no adapter; props are not read from a source shape veneer cannot parse"
                        ),
                    }),
                )
            } else {
                extract_from_source(&adapter, file_kind, filename, path, &source_text)
            };
            let kind = match &composites_root {
                Some(root) if path.starts_with(root) => DiscoveredKind::Composite,
                _ => DiscoveredKind::Component,
            };
            record_discovered(
                &mut discovered,
                DiscoveredItem {
                    name,
                    kind,
                    source_path: path.to_path_buf(),
                    generated: structure.is_ok(),
                    unsupported_framework: if is_unsupported_module {
                        unsupported_target.clone()
                    } else {
                        None
                    },
                },
            );
        }

        // Union with the names the rafters config declares as installed.
        // An installed name without a matching source file is still part of
        // the set (generated: false), pointing at the config that declares it.
        if let Some((config_path, config)) = &config {
            for (names, kind) in [
                (&config.installed.components, DiscoveredKind::Component),
                (&config.installed.composites, DiscoveredKind::Composite),
            ] {
                for name in names {
                    record_discovered(
                        &mut discovered,
                        DiscoveredItem {
                            name: name.clone(),
                            kind,
                            source_path: config_path.clone(),
                            generated: false,
                            unsupported_framework: None,
                        },
                    );
                }
            }
        }

        let mut items: Vec<DiscoveredItem> = discovered.into_values().collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }
}

/// Convert kebab-case to PascalCase (e.g., "context-menu" -> "ContextMenu").
fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let mut s = c.to_uppercase().collect::<String>();
                    s.push_str(chars.as_str());
                    s
                }
                None => String::new(),
            }
        })
        .collect()
}

/// Errors that can occur with the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Components directory not found: {0}")]
    DirectoryNotFound(String),

    #[error("Component not found: {0}")]
    ComponentNotFound(String),

    #[error("Failed to parse component: {0}")]
    ParseError(String),

    /// A source file exists but could not be read; discovery names it
    /// instead of silently dropping the item (FR-VEN-017).
    #[error("failed to read source file {path}: {source}")]
    UnreadableSource {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A declaration file (composite manifest or rafters config) exists
    /// but does not parse; the items it declares would otherwise be lost.
    #[error("malformed declaration file {path}: {message}")]
    MalformedDeclaration { path: PathBuf, message: String },

    /// The discovery walk could not traverse part of the project tree.
    #[error("failed to walk {path} during discovery: {message}")]
    WalkFailed { path: PathBuf, message: String },

    /// A component's preview CSS could not be scoped from the project
    /// stylesheet; the preview is refused rather than emitted silently
    /// missing its styles (FR-VEN-018).
    #[error("cannot generate preview for {component}: {message}")]
    PreviewCss { component: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn scans_components_directory() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        // Create a valid component file
        fs::write(
            comp_dir.join("button.tsx"),
            r#"
const variantClasses = {
  default: 'bg-primary text-white',
  secondary: 'bg-secondary text-black',
};

export function Button() {
  return <button />;
}
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Button"));
    }

    const REGISTRY_TEST_CSS: &str = r#"
@utility bg-blue-500 {
  background-color: oklch(0.62 0.19 260);
}
"#;

    #[test]
    fn generates_web_component_from_registry() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("button.tsx"),
            r#"
const variantClasses = {
  primary: 'bg-blue-500',
};

export function Button() {}
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        registry.scan(&comp_dir).unwrap();

        let result = registry
            .generate_web_component("Button", "button-preview", REGISTRY_TEST_CSS)
            .unwrap();

        assert_eq!(result.tag_name, "button-preview");
        assert!(result.web_component.contains("bg-blue-500"));
        // The class name rides in the module's lookup tables; the RULE lives
        // in the shared sheet the module adopts, never copied per component.
        assert!(!result.web_component.contains(".bg-blue-500 {"));
        assert!(result
            .web_component
            .contains("this.shadowRoot.adoptedStyleSheets = [previewStyles()]"));
    }

    // FR-VEN-018: extraction failure refuses the preview with an error
    // naming the component -- never a preview silently missing its styles.
    #[test]
    fn generate_web_component_errors_when_no_css_matches() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("button.tsx"),
            r#"
const variantClasses = {
  primary: 'bg-blue-500',
};

export function Button() {}
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        registry.scan(&comp_dir).unwrap();

        let error = registry
            .generate_web_component("Button", "button-preview", "")
            .expect_err("an empty stylesheet cannot style a classed component");
        match &error {
            RegistryError::PreviewCss { component, .. } => {
                assert_eq!(component, "Button");
            }
            other => panic!("expected PreviewCss, got {other:?}"),
        }
        assert!(error.to_string().contains("Button"));
    }

    // FR-VEN-018 (bullpen 019f1f4d): a class-builder arrow in a
    // .classes.ts file surfaces its dynamically-composed classes as
    // prefix patterns, and the generated module carries every utility
    // the pattern matches -- the real registry pipeline end to end.
    #[test]
    fn dynamic_class_builder_reaches_the_generated_module_css() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("quality-indicator.classes.ts"),
            r#"
export const qualityTintClass = (tint: string): string => `text-quality-${tint}`;
export const qualityBaseClasses = 'inline-flex items-center gap-1';
            "#,
        )
        .unwrap();

        let css = r#"
@theme {
  --color-quality-500: oklch(0.7 0.14 140);
  --color-quality-600: oklch(0.55 0.14 140);
}

@utility inline-flex {
  display: inline-flex;
}

@utility text-quality-500 {
  color: var(--color-quality-500);
}

@utility text-quality-600 {
  color: var(--color-quality-600);
}
"#;

        let mut registry = ComponentRegistry::new();
        registry.scan(&comp_dir).unwrap();

        let cached = registry.get("QualityIndicator").unwrap();
        assert_eq!(
            cached.structure.dynamic_class_patterns,
            vec!["text-quality-*".to_string()],
            "extraction must surface the dynamic composition as a pattern"
        );

        let block = registry
            .generate_web_component("QualityIndicator", "quality-indicator-preview", css)
            .unwrap();
        assert!(block.classes_used.contains(&"text-quality-*".to_string()));
        // The pattern survives as docs data. It selects no CSS any more --
        // the sheet is adopted whole, so there is nothing to select.
        assert!(!block.web_component.contains(".text-quality-500 {"));
    }

    #[test]
    fn discovers_prefixed_variant_and_size_classes() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("badge.classes.ts"),
            r#"
export const badgeVariantClasses: Record<string, string> = {
  default: 'bg-primary text-primary-foreground',
  secondary: 'bg-secondary text-secondary-foreground',
  destructive: 'bg-destructive text-destructive-foreground',
};

export const badgeSizeClasses: Record<string, string> = {
  sm: 'px-2 py-0.5 text-xs',
  default: 'px-2.5 py-0.5 text-xs',
  lg: 'px-3 py-1 text-sm',
};

export const badgeBaseClasses = 'inline-flex items-center rounded-full font-semibold';
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1, "Should register 1 component from classes file");
        assert!(
            registry.contains("Badge"),
            "Should register as PascalCase 'Badge'"
        );

        let cached = registry.get("Badge").unwrap();
        assert_eq!(cached.structure.variant_lookup.len(), 3);
        assert_eq!(cached.structure.size_lookup.len(), 3);
        assert!(cached.structure.base_classes.contains("inline-flex"));
    }

    #[test]
    fn discovers_structural_component_classes() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        // Structural components export individual class constants without variant records
        fs::write(
            comp_dir.join("accordion.classes.ts"),
            r#"
export const accordionItemClasses = 'border-b';
export const accordionTriggerClasses = 'flex items-center px-2 py-1.5';
export const accordionContentClasses = 'overflow-hidden transition-all';
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Accordion"));

        let cached = registry.get("Accordion").unwrap();
        assert!(cached.structure.base_classes.contains("border-b"));
        assert!(cached.structure.base_classes.contains("flex"));
    }

    #[test]
    fn discovers_nested_record_variant_classes() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        // Checkbox has nested variant records: Record<string, { border: string; checked: string; ring: string }>
        fs::write(
            comp_dir.join("checkbox.classes.ts"),
            r#"
export const checkboxBaseClasses = 'inline-flex items-center';
export const checkboxVariantClasses: Record<string, { border: string; checked: string; ring: string }> = {
  default: {
    border: 'border-primary',
    checked: 'data-[state=checked]:bg-primary',
    ring: 'focus-visible:ring-primary-ring',
  },
  secondary: {
    border: 'border-secondary',
    checked: 'data-[state=checked]:bg-secondary',
    ring: 'focus-visible:ring-secondary-ring',
  },
};
export const checkboxSizeClasses: Record<string, { box: string; icon: string }> = {
  sm: { box: 'h-3.5 w-3.5', icon: 'h-2.5 w-2.5' },
  default: { box: 'h-4 w-4', icon: 'h-3 w-3' },
};
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Checkbox"));

        let cached = registry.get("Checkbox").unwrap();
        assert_eq!(cached.structure.variant_lookup.len(), 2);
        assert_eq!(cached.structure.size_lookup.len(), 2);
        // Nested values should be flattened
        assert!(cached.structure.variant_lookup[0]
            .1
            .contains("border-primary"));
    }

    #[test]
    fn discovers_typography_plain_object() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("typography.classes.ts"),
            r#"
export const typographyClasses = {
  h1: 'scroll-m-20 text-4xl font-bold',
  h2: 'scroll-m-20 text-3xl font-semibold',
  p: 'leading-7 text-foreground',
} as const;
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Typography"));

        let cached = registry.get("Typography").unwrap();
        // typographyClasses is "Other" role, no variant/size in name,
        // so it goes into extra_classes -> base_classes
        assert!(cached.structure.base_classes.contains("text-4xl"));
    }

    #[test]
    fn discovers_grid_multiple_records() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("grid.classes.ts"),
            r#"
export const gridGapClasses: Record<string, string> = {
  '0': 'gap-0',
  '4': 'gap-4',
  '8': 'gap-8',
};

export const gridColumnClasses: Record<string, string> = {
  1: 'grid-cols-1',
  2: 'grid-cols-2',
  3: 'grid-cols-3',
};

export const gridGoldenClasses = 'grid-cols-3 [&>*:first-child]:col-span-2';
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Grid"));

        let cached = registry.get("Grid").unwrap();
        // All these are "Other" role, so they contribute to base_classes
        assert!(cached.structure.base_classes.contains("gap-0"));
        assert!(cached.structure.base_classes.contains("grid-cols-1"));
    }

    #[test]
    fn discovers_container_with_size_records() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("container.classes.ts"),
            r#"
export const containerSizeClasses: Record<string, string> = {
  sm: 'max-w-sm',
  md: 'max-w-md',
  lg: 'max-w-lg',
};

export const containerPaddingClasses: Record<string, string> = {
  '4': 'p-4',
  '8': 'p-8',
};
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Container"));

        let cached = registry.get("Container").unwrap();
        // containerSizeClasses -> Size role
        assert_eq!(cached.structure.size_lookup.len(), 3);
        assert_eq!(cached.structure.size_lookup[0].0, "sm");
    }

    #[test]
    fn discovers_array_join_pattern() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("tabs.classes.ts"),
            r#"
export const tabsListClasses = 'inline-flex items-center';
export const tabsTriggerBaseClasses = [
  'inline-flex items-center',
  'justify-center whitespace-nowrap',
].join(' ');
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("Tabs"));

        let cached = registry.get("Tabs").unwrap();
        assert!(cached.structure.base_classes.contains("inline-flex"));
    }

    #[test]
    fn scans_kebab_case_classes_file() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        fs::write(
            comp_dir.join("context-menu.classes.ts"),
            r#"
export const contextMenuItemClasses = 'flex items-center px-2 py-1.5';
export const contextMenuContentClasses = 'bg-popover text-popover-foreground';
            "#,
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 1);
        assert!(registry.contains("ContextMenu"));

        let cached = registry.get("ContextMenu").unwrap();
        assert!(cached.structure.base_classes.contains("flex"));
        assert!(cached.structure.base_classes.contains("bg-popover"));
    }

    #[test]
    fn kebab_to_pascal_works() {
        assert_eq!(super::kebab_to_pascal("badge"), "Badge");
        assert_eq!(super::kebab_to_pascal("context-menu"), "ContextMenu");
        assert_eq!(super::kebab_to_pascal("alert-dialog"), "AlertDialog");
    }

    #[test]
    fn skips_test_and_story_files() {
        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        // These should be skipped
        fs::write(
            comp_dir.join("button.test.tsx"),
            "const variantClasses = { test: 'x' };",
        )
        .unwrap();
        fs::write(
            comp_dir.join("button.stories.tsx"),
            "const variantClasses = { story: 'y' };",
        )
        .unwrap();

        let mut registry = ComponentRegistry::new();
        let count = registry.scan(&comp_dir).unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn classify_export_names() {
        assert_eq!(
            ExportRole::classify("badgeVariantClasses"),
            ExportRole::Variant
        );
        assert_eq!(ExportRole::classify("buttonSizeClasses"), ExportRole::Size);
        assert_eq!(ExportRole::classify("inputBaseClasses"), ExportRole::Base);
        assert_eq!(
            ExportRole::classify("switchTrackDisabledClasses"),
            ExportRole::Disabled
        );
        assert_eq!(
            ExportRole::classify("accordionItemClasses"),
            ExportRole::Other
        );
        assert_eq!(ExportRole::classify("typographyClasses"), ExportRole::Other);
        assert_eq!(ExportRole::classify("gridGoldenClasses"), ExportRole::Other);
    }

    #[test]
    fn discover_exports_finds_all_shapes() {
        let source = r#"
export const fooVariantClasses: Record<string, string> = {
  primary: 'bg-blue',
  secondary: 'bg-gray',
};

export const fooBaseClasses = 'inline-flex items-center';

export const fooDisabledClasses = 'opacity-50';
        "#;

        let exports = discover_exports(source, "test.classes.ts");
        assert_eq!(exports.len(), 3);
    }

    // ---- FR-VEN-017: component and composite discovery ----

    use crate::rafters_source::read_rafters_namespace;

    fn discovery_fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/discovery")
            .join(name)
    }

    fn discover_fixture(name: &str) -> Vec<DiscoveredItem> {
        let root = discovery_fixture(name);
        let source = read_rafters_namespace(&root).expect("fixture namespace must read");
        ComponentRegistry::discover(&root, &source).expect("discovery must succeed")
    }

    fn find_by_path_suffix<'a>(items: &'a [DiscoveredItem], suffix: &str) -> &'a DiscoveredItem {
        items
            .iter()
            .find(|item| item.source_path.to_string_lossy().ends_with(suffix))
            .unwrap_or_else(|| panic!("item with source path ending {suffix} must be discovered"))
    }

    fn find_by_name<'a>(items: &'a [DiscoveredItem], name: &str) -> &'a DiscoveredItem {
        items
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("item named {name} must be discovered"))
    }

    // AC (FR-VEN-033): the `.classes.ts` spine reads identically regardless
    // of the declared componentTarget -- extract_from_source's ClassesTs
    // arm never reads the framework adapter, so an unsupported declaration
    // never touches its extraction, unlike `.tsx`/`.jsx`.
    #[test]
    fn classes_ts_extraction_is_identical_regardless_of_declared_component_target() {
        let discover_with_target = |component_target: &str| -> Vec<DiscoveredItem> {
            let temp = tempdir().unwrap();
            fs::create_dir_all(temp.path().join(".rafters")).unwrap();
            fs::write(
                temp.path().join(".rafters/config.rafters.json"),
                format!(r#"{{"componentTarget":"{component_target}"}}"#),
            )
            .unwrap();
            let comp_dir = temp.path().join("components");
            fs::create_dir_all(&comp_dir).unwrap();
            fs::write(
                comp_dir.join("button.classes.ts"),
                "export const buttonBase = 'inline-flex items-center';\n\
                 export const buttonVariants = { primary: 'bg-primary', secondary: 'bg-secondary' };\n",
            )
            .unwrap();
            let source = read_rafters_namespace(temp.path()).expect("namespace must read");
            ComponentRegistry::discover(temp.path(), &source).expect("discovery must succeed")
        };

        let react = discover_with_target("react");
        let vue = discover_with_target("vue");

        assert_eq!(react.len(), 1);
        assert_eq!(vue.len(), 1);
        let react_item = &react[0];
        let vue_item = &vue[0];

        assert_eq!(react_item.name, vue_item.name);
        assert_eq!(react_item.kind, vue_item.kind);
        assert_eq!(react_item.generated, vue_item.generated);
        assert!(
            react_item.generated,
            "classes.ts extraction succeeds under a supported target"
        );
        assert!(
            vue_item.generated,
            "classes.ts extraction succeeds identically under an unsupported target"
        );
        assert_eq!(
            react_item.unsupported_framework, None,
            "classes.ts is never flagged unsupported"
        );
        assert_eq!(
            vue_item.unsupported_framework, None,
            "classes.ts is never flagged unsupported, even under a declared componentTarget veneer has no adapter for"
        );
    }

    // AC (FR-VEN-033): discover() wires the single dispatch site -- a
    // declared, unsupported componentTarget marks the .tsx item an
    // observation (never generated, never inferred), naming the declared
    // framework, without erroring the whole discovery pass.
    #[test]
    fn discover_marks_a_tsx_item_unsupported_when_component_target_has_no_adapter() {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".rafters")).unwrap();
        fs::write(
            temp.path().join(".rafters/config.rafters.json"),
            r#"{"componentTarget":"solid"}"#,
        )
        .unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();
        fs::write(
            comp_dir.join("button.tsx"),
            "export function Button() { return <button>ok</button>; }",
        )
        .unwrap();

        let source = read_rafters_namespace(temp.path()).expect("namespace must read");
        let items =
            ComponentRegistry::discover(temp.path(), &source).expect("discovery must succeed");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert!(
            !item.generated,
            "an unsupported target's source is never treated as generatable"
        );
        assert_eq!(
            item.unsupported_framework.as_deref(),
            Some("solid"),
            "the observation names exactly what was declared"
        );
    }

    // AC: componentTarget "react" dispatches ReactAdapter at the discover()
    // integration level -- a `.tsx` component under a project declaring
    // `"react"` extracts normally (generated: true, no observation).
    #[test]
    fn discover_extracts_normally_when_component_target_is_react() {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".rafters")).unwrap();
        fs::write(
            temp.path().join(".rafters/config.rafters.json"),
            r#"{"componentTarget":"react"}"#,
        )
        .unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();
        fs::write(
            comp_dir.join("button.tsx"),
            r#"
const variantClasses = {
  default: 'bg-primary text-white',
  secondary: 'bg-secondary text-black',
};

export function Button() {
  return <button />;
}
"#,
        )
        .unwrap();

        let source = read_rafters_namespace(temp.path()).expect("namespace must read");
        let items =
            ComponentRegistry::discover(temp.path(), &source).expect("discovery must succeed");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert!(item.generated, "a supported target extracts normally");
        assert_eq!(item.unsupported_framework, None);
    }

    #[test]
    fn discovers_union_of_source_walk_and_namespace_declarations() {
        let items = discover_fixture("with_namespace");

        // Source walk: Button (tsx + classes merged), Broken, CardComposite,
        // SplitPanel, hero-banner manifest. Namespace installed lists:
        // StatusPill, login-form.
        assert_eq!(items.len(), 7, "full union, nothing dropped: {items:#?}");
        for name in [
            "broken",
            "button",
            "hero-banner",
            "login-form",
            "statuspill",
        ] {
            assert!(
                items
                    .iter()
                    .any(|item| item.name.eq_ignore_ascii_case(name)),
                "{name} must be in the discovered set"
            );
        }
        find_by_path_suffix(&items, "card-composite.tsx");
        find_by_path_suffix(&items, "split-panel.tsx");
    }

    #[test]
    fn composite_kind_comes_from_declaration_not_name() {
        let items = discover_fixture("with_namespace");

        // Name contains "composite" but nothing declares it one: Component.
        let card = find_by_path_suffix(&items, "card-composite.tsx");
        assert_eq!(card.kind, DiscoveredKind::Component);

        // Declared composite by residence under the config compositesPath.
        let split = find_by_path_suffix(&items, "split-panel.tsx");
        assert_eq!(split.kind, DiscoveredKind::Composite);
        assert!(split.generated, "extractable composite source is generated");

        // Declared composite by manifest format.
        let hero = find_by_name(&items, "hero-banner");
        assert_eq!(hero.kind, DiscoveredKind::Composite);
        assert!(!hero.generated, "manifests have no generation pipeline yet");

        // Declared composite by the installed.composites list.
        let login = find_by_name(&items, "login-form");
        assert_eq!(login.kind, DiscoveredKind::Composite);

        // Declared component by the installed.components list.
        let pill = find_by_name(&items, "StatusPill");
        assert_eq!(pill.kind, DiscoveredKind::Component);
    }

    #[test]
    fn present_but_not_generated_is_reported_not_dropped() {
        let items = discover_fixture("with_namespace");

        // Unparseable source file: present with generated: false.
        let broken = find_by_name(&items, "Broken");
        assert_eq!(broken.kind, DiscoveredKind::Component);
        assert!(!broken.generated);
        assert!(broken.source_path.ends_with("components/broken.tsx"));

        // Installed name with no source file: present, pointing at the
        // config that declares it.
        let pill = find_by_name(&items, "StatusPill");
        assert!(!pill.generated);
        assert!(pill.source_path.ends_with(".rafters/config.rafters.json"));
    }

    #[test]
    fn duplicate_source_sightings_merge_into_one_item() {
        let items = discover_fixture("with_namespace");

        // button.tsx and button.classes.ts both declare Button.
        let buttons: Vec<&DiscoveredItem> = items
            .iter()
            .filter(|item| item.name.eq_ignore_ascii_case("button"))
            .collect();
        assert_eq!(buttons.len(), 1, "one merged item, not one per file");
        assert!(buttons[0].generated);
    }

    #[test]
    fn casing_divergent_declarations_of_one_composite_merge_into_one_item() {
        // The same physical composite declared through all three mechanisms
        // with the two casing conventions they use: manifest.id "hero-banner"
        // (kebab), installed.composites "hero-banner" (kebab), and a source
        // file under compositesPath extracting as "HeroBanner" (Pascal).
        let items = discover_fixture("casing_collision");

        let heroes: Vec<&DiscoveredItem> = items
            .iter()
            .filter(|item| identity_fold(&item.name) == "herobanner")
            .collect();
        assert_eq!(
            heroes.len(),
            1,
            "one merged item, not one per declaration mechanism: {items:#?}"
        );
        assert_eq!(heroes[0].kind, DiscoveredKind::Composite);
        assert!(
            heroes[0].generated,
            "the extractable source sighting must win the merge"
        );
        assert_eq!(items.len(), 1, "nothing else in the fixture: {items:#?}");
    }

    #[test]
    fn ordering_is_deterministic_stable_sort_by_name() {
        let first = discover_fixture("with_namespace");
        let second = discover_fixture("with_namespace");
        assert_eq!(first, second, "repeated discovery must be identical");
        assert!(
            first.windows(2).all(|pair| pair[0].name <= pair[1].name),
            "items must be sorted by name: {:?}",
            first.iter().map(|item| &item.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_source_discovery_uses_only_the_component_walk() {
        let root = discovery_fixture("no_source");
        let items = ComponentRegistry::discover(&root, &IntelligenceSource::NoSource)
            .expect("discovery without .rafters/ must succeed");
        assert_eq!(items.len(), 1);
        assert!(items[0].name.eq_ignore_ascii_case("button"));
        assert_eq!(items[0].kind, DiscoveredKind::Component);
        assert!(items[0].generated);
    }

    #[test]
    fn malformed_composite_manifest_is_a_named_error() {
        let root = discovery_fixture("malformed_composite");
        let error = ComponentRegistry::discover(&root, &IntelligenceSource::NoSource)
            .expect_err("a manifest without manifest.id must be a named error");
        match error {
            RegistryError::MalformedDeclaration { path, .. } => {
                assert!(path.ends_with("composites/bad.composite.json"));
            }
            other => panic!("expected MalformedDeclaration, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_source_file_is_a_named_error() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let comp_dir = temp.path().join("components");
        fs::create_dir_all(&comp_dir).unwrap();
        let locked = comp_dir.join("locked.tsx");
        fs::write(&locked, "export function Locked() { return <div />; }").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let error = ComponentRegistry::discover(temp.path(), &IntelligenceSource::NoSource)
            .expect_err("an unreadable file must be a named error, not a silent drop");

        // Restore permissions so the tempdir can clean up on every platform.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

        match error {
            RegistryError::UnreadableSource { path, .. } => {
                assert!(path.ends_with("components/locked.tsx"));
            }
            other => panic!("expected UnreadableSource, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn untraversable_directory_is_a_walk_failed_error_naming_the_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let sealed = temp.path().join("sealed");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("hidden.tsx"), "export function Hidden() {}").unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();

        let error = ComponentRegistry::discover(temp.path(), &IntelligenceSource::NoSource)
            .expect_err("an untraversable directory must be a named error, not a silent drop");

        // Restore permissions so the tempdir can clean up on every platform.
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();

        match error {
            RegistryError::WalkFailed { path, .. } => {
                assert!(
                    path.ends_with("sealed"),
                    "error must name the path: {path:?}"
                );
            }
            other => panic!("expected WalkFailed, got {other:?}"),
        }
    }
}
