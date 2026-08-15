//! Reader for the `.rafters/` namespace source -- the design-intelligence
//! source of truth. Veneer reads this directly and never derives a rendered
//! field from the lossy DTCG export when the namespace source is present
//! (FR-VEN-002).
//!
//! Format grounding (verified against a real `.rafters/` directory, schema
//! `https://rafters.studio/schemas/namespace-tokens.json`): each file under
//! `.rafters/tokens/<namespace>.rafters.json` is an object with `$schema`,
//! `namespace`, `version`, `generatedAt`, and a `tokens` array. The fields
//! FR-VEN-002 names as dropped by the DTCG export exist in this source under
//! these paths:
//!
//! - override reasons: `token.userOverride.reason` (with `previousValue`)
//! - full OKLCH scale: `token.value.scale`, an array of `{l, c, h, alpha}`
//! - accessibility matrices: `token.value.accessibility.wcagAA` /
//!   `.wcagAAA`, each holding `normal` and `large` arrays of
//!   `[foreground, background]` scale-index pairs
//!
//! A DTCG-style `$extensions` field does NOT exist in the namespace files
//! and is therefore not modeled; nothing here is invented. Namespace-specific
//! fields (for example `motionDuration`, `easingCurve`, `generationRule`)
//! are preserved verbatim in [`NamespaceToken::extra`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Errors while reading the `.rafters/` namespace source. Each variant names
/// the file and, where available, the location of the failure. A parse
/// failure is surfaced as an error -- never degraded to the DTCG export.
#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    /// A namespace file or directory could not be read from disk.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A namespace file is not valid JSON or does not match the
    /// namespace-tokens schema shape.
    #[error("malformed namespace file {path} at line {line}, column {column}: {message}")]
    Malformed {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    /// A token inside an otherwise well-formed file has an invalid shape.
    #[error("invalid token \"{token}\" in {path}: {message}")]
    InvalidToken {
        path: PathBuf,
        token: String,
        message: String,
    },
}

/// Where design intelligence comes from. Either the `.rafters/` namespace
/// source was found and parsed, or there is declared no source at all.
/// There is deliberately no DTCG variant: the lossy export is never a
/// fallback (the silent-fallback path is the defect FR-VEN-002 removes).
#[derive(Debug, Clone, PartialEq)]
pub enum IntelligenceSource {
    /// `.rafters/` present and parsed.
    Namespace(RaftersNamespace),
    /// `.rafters/` absent -- declared, never silently substituted.
    NoSource,
}

/// Parsed representation of the `.rafters/` namespace files, preserving the
/// fields the DTCG export drops: override reasons (`userOverride.reason`),
/// the full OKLCH scale (`value.scale`), and accessibility matrices
/// (`value.accessibility`).
#[derive(Debug, Clone, PartialEq)]
pub struct RaftersNamespace {
    /// Parsed namespace files keyed by namespace name (for example "color",
    /// "semantic", "motion"). A namespace whose file is absent has no entry:
    /// absence is explicit, never guessed.
    pub namespaces: BTreeMap<String, NamespaceFile>,
}

/// One parsed `<namespace>.rafters.json` file.
#[derive(Debug, Clone, PartialEq)]
pub struct NamespaceFile {
    /// The `$schema` URL, if declared.
    pub schema: Option<String>,
    /// Namespace name as declared inside the file.
    pub namespace: String,
    /// File format version, if declared.
    pub version: Option<String>,
    /// Generation timestamp of the file, if declared.
    pub generated_at: Option<String>,
    /// All tokens in the file, in file order.
    pub tokens: Vec<NamespaceToken>,
}

/// One token from a namespace file. Every optional field is `None` (or
/// empty) exactly when the source file omits it.
#[derive(Debug, Clone, PartialEq)]
pub struct NamespaceToken {
    pub name: String,
    pub value: TokenValue,
    pub category: Option<String>,
    pub namespace: Option<String>,
    pub semantic_meaning: Option<String>,
    pub usage_context: Vec<String>,
    pub usage_patterns: Option<UsagePatterns>,
    pub depends_on: Vec<String>,
    pub progression_system: Option<String>,
    pub scale_position: Option<i64>,
    pub container_query_aware: Option<bool>,
    pub locale_aware: Option<bool>,
    pub requires_confirmation: Option<bool>,
    /// A user override of a generated value, including the reason -- a field
    /// the DTCG export drops entirely.
    pub user_override: Option<UserOverride>,
    pub description: Option<String>,
    pub generated_at: Option<String>,
    /// Namespace-specific fields preserved verbatim (for example
    /// `motionDuration`, `easingCurve`, `trustLevel`, `generationRule`).
    pub extra: serde_json::Map<String, Value>,
}

/// Do/never usage guidance attached to a token.
///
/// This shape is closed in the source schema, so unknown fields are a named
/// parse error rather than silent data loss (open, namespace-specific fields
/// live in [`NamespaceToken::extra`] instead).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsagePatterns {
    #[serde(rename = "do", default)]
    pub do_patterns: Vec<String>,
    #[serde(default)]
    pub never: Vec<String>,
}

/// A user override of a generated token value. `reason` is the override
/// reason FR-VEN-002 requires preserved.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserOverride {
    pub reason: Option<String>,
    pub previous_value: Option<Value>,
}

/// A token value as it appears in the namespace source.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    /// A literal CSS-ish string, for example "oklch(0.985 0 0)" or "0ms".
    Literal(String),
    /// A reference to a position within a color family, for example
    /// `{"family": "neutral", "position": "50"}`.
    FamilyReference { family: String, position: String },
    /// A structured color value carrying the intelligence the DTCG export
    /// drops: the full OKLCH scale and accessibility matrices.
    Structured(StructuredValue),
}

/// A structured (object) token value. `scale` and `accessibility` are typed
/// because FR-VEN-002 names them; every other key is preserved verbatim in
/// `extra` (for example `harmonies`, `intelligence`, `analysis`, `use`).
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredValue {
    /// The full OKLCH scale, present on color-family and brand-color values.
    pub scale: Option<Vec<OklchComponents>>,
    /// Accessibility matrices, present on brand-color values.
    pub accessibility: Option<AccessibilityMatrices>,
    /// All remaining keys of the value object, preserved verbatim.
    pub extra: serde_json::Map<String, Value>,
}

/// One step of an OKLCH scale.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OklchComponents {
    pub l: f64,
    pub c: f64,
    pub h: f64,
    pub alpha: f64,
}

/// The accessibility block of a structured color value. WCAG matrices are
/// typed; the remaining keys (`onWhite`, `onBlack`, `apca`) are preserved
/// verbatim in `extra`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessibilityMatrices {
    pub wcag_aa: Option<ContrastMatrix>,
    pub wcag_aaa: Option<ContrastMatrix>,
    pub extra: serde_json::Map<String, Value>,
}

/// Scale-index pairs that pass a WCAG level, split by text size.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContrastMatrix {
    #[serde(default)]
    pub normal: Vec<ContrastPair>,
    #[serde(default)]
    pub large: Vec<ContrastPair>,
}

/// A foreground/background pair of scale indices, serialized in the source
/// as a two-element array `[foreground, background]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "[usize; 2]")]
pub struct ContrastPair {
    pub foreground: usize,
    pub background: usize,
}

impl From<[usize; 2]> for ContrastPair {
    fn from([foreground, background]: [usize; 2]) -> Self {
        ContrastPair {
            foreground,
            background,
        }
    }
}

/// Read the `.rafters/` namespace source under `project_root`.
///
/// - `.rafters/` absent: returns [`IntelligenceSource::NoSource`]. Veneer
///   never falls back to the DTCG export; `parse_dtcg_tokens` is not
///   consulted anywhere on this path.
/// - `.rafters/` present: parses every `tokens/<namespace>.rafters.json`.
///   A partial namespace (some files missing) parses what exists; the
///   absent namespaces simply have no entry.
/// - Malformed input: returns a [`NamespaceError`] naming the file and
///   location -- never degrades to the DTCG export.
pub fn read_rafters_namespace(project_root: &Path) -> Result<IntelligenceSource, NamespaceError> {
    let rafters_dir = project_root.join(".rafters");
    if !rafters_dir.is_dir() {
        return Ok(IntelligenceSource::NoSource);
    }

    let mut namespaces: BTreeMap<String, NamespaceFile> = BTreeMap::new();
    let tokens_dir = rafters_dir.join("tokens");
    if tokens_dir.is_dir() {
        let entries = std::fs::read_dir(&tokens_dir).map_err(|source| NamespaceError::Io {
            path: tokens_dir.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| NamespaceError::Io {
                path: tokens_dir.clone(),
                source,
            })?;
            let path = entry.path();
            let is_namespace_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".rafters.json"));
            if !is_namespace_file || !path.is_file() {
                continue;
            }
            let file = parse_namespace_file(&path)?;
            namespaces.insert(file.namespace.clone(), file);
        }
    }

    Ok(IntelligenceSource::Namespace(RaftersNamespace {
        namespaces,
    }))
}

/// The project-relative path of the compiled preview sheet, named as a
/// constant so the reader and the refusal that fires when it is absent
/// cite the same path rather than two hand-written strings.
pub const DOCUMENTATION_SHEET_PATH: &str = ".rafters/output/rafters.documentation.css";

/// Read the compiled preview sheet the rafters exporter writes to
/// `.rafters/output/rafters.documentation.css`. Previews adopt it WHOLE
/// into their shadow root (FR-VEN-018, issue #105); nothing scopes or
/// tree-shakes it.
///
/// This is the documentation sheet, not `rafters.css`. `rafters.css` is the
/// `@theme`/`@utility` SOURCE sheet a consumer build JITs from -- it carries
/// no compiled rules a browser can adopt. The documentation sheet is
/// compiled, carries its tokens on `:host` rather than `:root`, and is
/// content-scanned over the project's installed components (rafters
/// `outputs.ts` passes `contentSources`), so it moves when components move.
///
/// `Ok(None)` when the project declares no preview sheet -- absence is
/// explicit, never guessed, and the caller refuses rather than emitting an
/// unstyled preview. An existing file that cannot be read is an error naming
/// the path, so a preview never renders silently missing its styles because
/// the sheet vanished mid-read.
pub fn read_rafters_stylesheet(project_root: &Path) -> Result<Option<String>, NamespaceError> {
    let path = project_root.join(DOCUMENTATION_SHEET_PATH);
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::read_to_string(&path)
        .map(Some)
        .map_err(|source| NamespaceError::Io { path, source })
}

/// The `framework`/`componentTarget` facts declared in
/// `.rafters/config.rafters.json` (FR-VEN-033), read as input facts and
/// nothing more: `component_target` is the only field veneer acts on, and
/// only to select a framework adapter
/// (`crate::mode::dispatch_framework`) -- never to spawn or manage a
/// toolchain. `framework` is carried for observability; veneer never
/// dispatches on it. Absence of the file, or of either field, yields `None`
/// for that field -- never guessed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameworkDeclaration {
    /// The project's declared delivery framework, for example `"wc"`.
    pub framework: Option<String>,
    /// The declared source framework of the components, for example
    /// `"react"`. Drives adapter selection; an unsupported value is an
    /// observation, never an inference (see
    /// `crate::mode::FrameworkDispatch`).
    pub component_target: Option<String>,
}

/// Raw shape of the subset of `.rafters/config.rafters.json` this reader
/// captures. The real file declares many more fields (`componentsPath`,
/// `installed`, `exports`, ...); those are read elsewhere
/// (`crate::registry`) and are intentionally ignored here rather than
/// duplicating that parse.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFrameworkDeclaration {
    #[serde(default)]
    framework: Option<String>,
    #[serde(default)]
    component_target: Option<String>,
}

/// Read the `framework`/`componentTarget` facts from
/// `.rafters/config.rafters.json`. `.rafters/config.rafters.json` absent
/// yields [`FrameworkDeclaration::default`] (both fields `None`) -- the
/// common case for a project that declares nothing. Present-but-malformed
/// JSON is a named error, never a silent default.
pub fn read_framework_declaration(
    project_root: &Path,
) -> Result<FrameworkDeclaration, NamespaceError> {
    let path = project_root.join(".rafters").join("config.rafters.json");
    if !path.is_file() {
        return Ok(FrameworkDeclaration::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|source| NamespaceError::Io {
        path: path.clone(),
        source,
    })?;
    let raw: RawFrameworkDeclaration =
        serde_json::from_str(&text).map_err(|error| NamespaceError::Malformed {
            path: path.clone(),
            line: error.line(),
            column: error.column(),
            message: error.to_string(),
        })?;
    Ok(FrameworkDeclaration {
        framework: raw.framework,
        component_target: raw.component_target,
    })
}

/// Raw serde shape of a `<namespace>.rafters.json` file.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNamespaceFile {
    #[serde(rename = "$schema")]
    schema: Option<String>,
    namespace: String,
    version: Option<String>,
    generated_at: Option<String>,
    tokens: Vec<RawToken>,
}

/// Raw serde shape of one token. `value` stays untyped here and is converted
/// to [`TokenValue`] in a second pass so shape failures can name the token.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawToken {
    name: String,
    value: Value,
    category: Option<String>,
    namespace: Option<String>,
    semantic_meaning: Option<String>,
    #[serde(default)]
    usage_context: Vec<String>,
    usage_patterns: Option<UsagePatterns>,
    #[serde(default)]
    depends_on: Vec<String>,
    progression_system: Option<String>,
    scale_position: Option<i64>,
    container_query_aware: Option<bool>,
    locale_aware: Option<bool>,
    requires_confirmation: Option<bool>,
    user_override: Option<UserOverride>,
    description: Option<String>,
    generated_at: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

fn parse_namespace_file(path: &Path) -> Result<NamespaceFile, NamespaceError> {
    let source = std::fs::read_to_string(path).map_err(|source| NamespaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let raw: RawNamespaceFile =
        serde_json::from_str(&source).map_err(|error| NamespaceError::Malformed {
            path: path.to_path_buf(),
            line: error.line(),
            column: error.column(),
            message: error.to_string(),
        })?;

    let mut tokens: Vec<NamespaceToken> = Vec::with_capacity(raw.tokens.len());
    for raw_token in raw.tokens {
        let value = parse_token_value(path, &raw_token.name, raw_token.value)?;
        tokens.push(NamespaceToken {
            name: raw_token.name,
            value,
            category: raw_token.category,
            namespace: raw_token.namespace,
            semantic_meaning: raw_token.semantic_meaning,
            usage_context: raw_token.usage_context,
            usage_patterns: raw_token.usage_patterns,
            depends_on: raw_token.depends_on,
            progression_system: raw_token.progression_system,
            scale_position: raw_token.scale_position,
            container_query_aware: raw_token.container_query_aware,
            locale_aware: raw_token.locale_aware,
            requires_confirmation: raw_token.requires_confirmation,
            user_override: raw_token.user_override,
            description: raw_token.description,
            generated_at: raw_token.generated_at,
            extra: raw_token.extra,
        });
    }

    Ok(NamespaceFile {
        schema: raw.schema,
        namespace: raw.namespace,
        version: raw.version,
        generated_at: raw.generated_at,
        tokens,
    })
}

fn parse_token_value(path: &Path, token: &str, value: Value) -> Result<TokenValue, NamespaceError> {
    match value {
        Value::String(literal) => Ok(TokenValue::Literal(literal)),
        Value::Object(mut object) => {
            if object.len() == 2 {
                if let (Some(Value::String(family)), Some(Value::String(position))) =
                    (object.get("family"), object.get("position"))
                {
                    return Ok(TokenValue::FamilyReference {
                        family: family.clone(),
                        position: position.clone(),
                    });
                }
            }

            let scale = match object.remove("scale") {
                Some(raw_scale) => Some(
                    serde_json::from_value::<Vec<OklchComponents>>(raw_scale).map_err(|error| {
                        NamespaceError::InvalidToken {
                            path: path.to_path_buf(),
                            token: token.to_string(),
                            message: format!("invalid OKLCH scale: {error}"),
                        }
                    })?,
                ),
                None => None,
            };
            let accessibility = match object.remove("accessibility") {
                Some(raw_accessibility) => {
                    Some(parse_accessibility(path, token, raw_accessibility)?)
                }
                None => None,
            };
            Ok(TokenValue::Structured(StructuredValue {
                scale,
                accessibility,
                extra: object,
            }))
        }
        other => Err(NamespaceError::InvalidToken {
            path: path.to_path_buf(),
            token: token.to_string(),
            message: format!(
                "unsupported value type {}: expected a string or an object, refusing to guess",
                json_type_name(&other)
            ),
        }),
    }
}

fn parse_accessibility(
    path: &Path,
    token: &str,
    value: Value,
) -> Result<AccessibilityMatrices, NamespaceError> {
    let Value::Object(mut object) = value else {
        return Err(NamespaceError::InvalidToken {
            path: path.to_path_buf(),
            token: token.to_string(),
            message: format!(
                "accessibility must be an object, found {}",
                json_type_name(&value)
            ),
        });
    };
    let mut parse_matrix = |key: &str| -> Result<Option<ContrastMatrix>, NamespaceError> {
        match object.remove(key) {
            Some(raw) => serde_json::from_value::<ContrastMatrix>(raw)
                .map(Some)
                .map_err(|error| NamespaceError::InvalidToken {
                    path: path.to_path_buf(),
                    token: token.to_string(),
                    message: format!("invalid {key} matrix: {error}"),
                }),
            None => Ok(None),
        }
    };
    let wcag_aa = parse_matrix("wcagAA")?;
    let wcag_aaa = parse_matrix("wcagAAA")?;
    Ok(AccessibilityMatrices {
        wcag_aa,
        wcag_aaa,
        extra: object,
    })
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rafters_source")
            .join(name)
    }

    // ---- FR-VEN-033: framework/componentTarget input facts ----

    #[test]
    fn absent_config_yields_no_declaration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let declaration =
            read_framework_declaration(temp.path()).expect("absence must not be an error");
        assert_eq!(declaration, FrameworkDeclaration::default());
        assert_eq!(declaration.framework, None);
        assert_eq!(declaration.component_target, None);
    }

    #[test]
    fn declared_framework_and_component_target_are_read_as_facts() {
        // The real shape (verified against a real `.rafters/config.rafters.json`,
        // reused from the existing discovery fixture rather than hand-modeling
        // a new one -- see tests/fixtures/README.md).
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/discovery/with_namespace");
        let declaration = read_framework_declaration(&root).expect("fixture config must read");
        assert_eq!(declaration.framework.as_deref(), Some("unknown"));
        assert_eq!(declaration.component_target.as_deref(), Some("react"));
    }

    #[test]
    fn malformed_config_is_a_named_error_never_a_silent_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join(".rafters")).expect("mkdir .rafters");
        std::fs::write(
            temp.path().join(".rafters/config.rafters.json"),
            "{not json",
        )
        .expect("write malformed config");
        let error = read_framework_declaration(temp.path())
            .expect_err("malformed config must be a named error");
        match error {
            NamespaceError::Malformed { path, .. } => {
                assert!(path.ends_with(".rafters/config.rafters.json"));
            }
            other => panic!("expected Malformed error, got {other:?}"),
        }
    }

    fn read_namespace(name: &str) -> RaftersNamespace {
        match read_rafters_namespace(&fixture(name)).expect("fixture must parse") {
            IntelligenceSource::Namespace(namespace) => namespace,
            IntelligenceSource::NoSource => panic!("fixture {name} must contain .rafters/"),
        }
    }

    fn find_token<'a>(
        namespace: &'a RaftersNamespace,
        namespace_name: &str,
        token_name: &str,
    ) -> &'a NamespaceToken {
        namespace
            .namespaces
            .get(namespace_name)
            .unwrap_or_else(|| panic!("namespace {namespace_name} must exist"))
            .tokens
            .iter()
            .find(|t| t.name == token_name)
            .unwrap_or_else(|| panic!("token {token_name} must exist in {namespace_name}"))
    }

    #[test]
    fn absent_rafters_dir_yields_declared_no_source_never_dtcg_fallback() {
        let root = fixture("dtcg_only");
        // The project HAS a DTCG export; only .rafters/ is missing.
        assert!(root.join("tokens.dtcg.json").is_file());
        let source = read_rafters_namespace(&root).expect("no-source read must succeed");
        assert_eq!(source, IntelligenceSource::NoSource);
    }

    #[test]
    fn namespace_present_reads_all_namespace_files() {
        let namespace = read_namespace("with_namespace");
        let names: Vec<&String> = namespace.namespaces.keys().collect();
        assert_eq!(names, ["color", "motion", "semantic"]);
        let color = &namespace.namespaces["color"];
        assert_eq!(color.namespace, "color");
        assert_eq!(color.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            color.schema.as_deref(),
            Some("https://rafters.studio/schemas/namespace-tokens.json")
        );
        assert_eq!(color.tokens.len(), 2);
    }

    #[test]
    fn preserves_full_oklch_scale_dropped_by_dtcg() {
        let namespace = read_namespace("with_namespace");
        let neutral = find_token(&namespace, "color", "neutral");
        let TokenValue::Structured(value) = &neutral.value else {
            panic!("neutral must have a structured value");
        };
        let scale = value.scale.as_ref().expect("scale must be preserved");
        assert_eq!(scale.len(), 11);
        assert_eq!(
            scale[0],
            OklchComponents {
                l: 0.985,
                c: 0.0,
                h: 0.0,
                alpha: 1.0
            }
        );
        assert_eq!(scale[10].l, 0.145);
        // Non-scale keys of the value object are preserved verbatim.
        assert_eq!(
            value.extra.get("use").and_then(Value::as_str),
            Some("Foundation neutral palette for backgrounds, borders, text, and UI chrome.")
        );
    }

    #[test]
    fn preserves_override_reason_dropped_by_dtcg() {
        let namespace = read_namespace("with_namespace");
        let primary = find_token(&namespace, "semantic", "primary");
        let user_override = primary
            .user_override
            .as_ref()
            .expect("userOverride must be preserved");
        assert_eq!(
            user_override.reason.as_deref(),
            Some(
                "Onboarded from --primary: Sean wants true red as primary to test onboard cascade"
            )
        );
        assert!(user_override.previous_value.is_some());
    }

    #[test]
    fn preserves_accessibility_matrices_dropped_by_dtcg() {
        let namespace = read_namespace("with_namespace");
        let primary = find_token(&namespace, "semantic", "primary");
        let TokenValue::Structured(value) = &primary.value else {
            panic!("primary must have a structured value");
        };
        let accessibility = value
            .accessibility
            .as_ref()
            .expect("accessibility must be preserved");
        let wcag_aa = accessibility.wcag_aa.as_ref().expect("wcagAA matrix");
        assert_eq!(wcag_aa.normal.len(), 5);
        assert_eq!(
            wcag_aa.normal[0],
            ContrastPair {
                foreground: 0,
                background: 7
            }
        );
        assert_eq!(wcag_aa.large.len(), 4);
        let wcag_aaa = accessibility.wcag_aaa.as_ref().expect("wcagAAA matrix");
        assert_eq!(wcag_aaa.normal.len(), 2);
        // Remaining accessibility data preserved verbatim.
        assert!(accessibility.extra.contains_key("onWhite"));
        assert!(accessibility.extra.contains_key("apca"));
    }

    #[test]
    fn parses_family_reference_values() {
        let namespace = read_namespace("with_namespace");
        let background = find_token(&namespace, "semantic", "background");
        assert_eq!(
            background.value,
            TokenValue::FamilyReference {
                family: "neutral".to_string(),
                position: "50".to_string()
            }
        );
        assert_eq!(background.depends_on, ["neutral-50", "neutral-950"]);
        assert_eq!(background.requires_confirmation, Some(false));
    }

    #[test]
    fn preserves_usage_patterns_and_namespace_specific_fields() {
        let namespace = read_namespace("with_namespace");
        let instant = find_token(&namespace, "motion", "motion-duration-instant");
        assert_eq!(instant.value, TokenValue::Literal("0ms".to_string()));
        let patterns = instant
            .usage_patterns
            .as_ref()
            .expect("usagePatterns must be preserved");
        assert_eq!(patterns.do_patterns.len(), 2);
        assert_eq!(patterns.never[0], "Ignore prefers-reduced-motion");
        assert_eq!(instant.scale_position, Some(0));
        // Motion-specific fields survive verbatim in extra.
        assert_eq!(
            instant.extra.get("motionDuration").and_then(Value::as_i64),
            Some(0)
        );
        assert_eq!(
            instant
                .extra
                .get("reducedMotionAware")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn namespace_truth_wins_over_divergent_dtcg_export() {
        let root = fixture("with_namespace");
        // The fixture project carries a divergent DTCG export claiming
        // neutral-50 is "#fafafa" -- the lossy value.
        let dtcg_source =
            std::fs::read_to_string(root.join("tokens.dtcg.json")).expect("dtcg fixture");
        let dtcg: serde_json::Value =
            serde_json::from_str(&dtcg_source).expect("dtcg fixture parses");
        let lossy = dtcg["color"]["neutral"]["50"]["$value"]
            .as_str()
            .expect("lossy value");
        assert_eq!(lossy, "#fafafa");

        // The namespace source is what read_rafters_namespace returns; the
        // DTCG export is never consulted and the divergence is invisible.
        let namespace = read_namespace("with_namespace");
        let neutral_50 = find_token(&namespace, "color", "neutral-50");
        assert_eq!(
            neutral_50.value,
            TokenValue::Literal("oklch(0.985 0 0)".to_string())
        );
        assert_ne!(neutral_50.value, TokenValue::Literal(lossy.to_string()));
    }

    #[test]
    fn partial_namespace_parses_what_exists_and_leaves_absent_areas_absent() {
        let namespace = read_namespace("partial");
        assert_eq!(namespace.namespaces.len(), 1);
        assert!(namespace.namespaces.contains_key("spacing"));
        // Absent namespaces have no entry -- not defaults, not guesses.
        assert!(!namespace.namespaces.contains_key("color"));
        assert!(!namespace.namespaces.contains_key("semantic"));
        // Fields the file omits are explicitly absent on the token too.
        let spacing_0 = find_token(&namespace, "spacing", "spacing-0");
        assert_eq!(spacing_0.semantic_meaning, None);
        assert_eq!(spacing_0.user_override, None);
        assert!(spacing_0.usage_context.is_empty());
        assert_eq!(
            spacing_0
                .extra
                .get("generationRule")
                .and_then(Value::as_str),
            Some("calc({spacing-base} * 0)")
        );
    }

    #[test]
    fn rafters_dir_without_tokens_dir_is_an_empty_namespace_not_no_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(temp.path().join(".rafters")).expect("mkdir .rafters");
        let source = read_rafters_namespace(temp.path()).expect("read succeeds");
        match source {
            IntelligenceSource::Namespace(namespace) => {
                assert!(namespace.namespaces.is_empty());
            }
            IntelligenceSource::NoSource => {
                panic!(".rafters/ exists, so the source is Namespace, not NoSource")
            }
        }
    }

    #[test]
    fn malformed_json_error_names_file_and_location() {
        let error = read_rafters_namespace(&fixture("malformed"))
            .expect_err("malformed JSON must be an error, never a DTCG fallback");
        match error {
            NamespaceError::Malformed { path, line, .. } => {
                assert!(path.ends_with(".rafters/tokens/color.rafters.json"));
                assert!(line > 0);
            }
            other => panic!("expected Malformed error, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_value_type_is_a_named_error_not_a_guess() {
        let error = read_rafters_namespace(&fixture("bad_value"))
            .expect_err("numeric token value must be a named error");
        match error {
            NamespaceError::InvalidToken {
                path,
                token,
                message,
            } => {
                assert!(path.ends_with(".rafters/tokens/color.rafters.json"));
                assert_eq!(token, "neutral-50");
                assert!(message.contains("number"));
            }
            other => panic!("expected InvalidToken error, got {other:?}"),
        }
    }
}
