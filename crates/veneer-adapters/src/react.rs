//! React/JSX adapter for transforming components to Web Components.
//!
//! Uses oxc AST parsing instead of regex for reliable component extraction.

use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPatternKind, Declaration, Expression, ObjectPropertyKind, PropertyKey, Statement,
    TSSignature,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::conventions::ComponentConventions;
use crate::generator::preview_web_component_block;
use crate::traits::{FrameworkAdapter, TransformContext, TransformError, TransformedBlock};
use crate::ts_helpers::{
    extract_class_value, extract_nested_object_classes, normalize_whitespace,
    unwrap_type_expressions,
};

/// Extracted component structure from source code.
#[derive(Debug, Clone, Default)]
pub struct ComponentStructure {
    /// Component name (e.g., "Button")
    pub name: String,

    /// Variant classes mapping
    pub variant_lookup: Vec<(String, String)>,

    /// Size classes mapping
    pub size_lookup: Vec<(String, String)>,

    /// Base classes applied to all variants
    pub base_classes: String,

    /// Classes applied when disabled
    pub disabled_classes: String,

    /// Default variant value
    pub default_variant: String,

    /// Default size value
    pub default_size: String,

    /// Observed attributes from props
    pub observed_attributes: Vec<String>,

    /// `prefix-*` patterns for classes the component composes dynamically
    /// at render (for example `text-quality-${tint}`). Tailwind tree-shakes
    /// those names from compiled output (bullpen 019f1f4d), so scoping
    /// matches them by prefix instead of by literal (FR-VEN-018). Sorted
    /// and deduplicated.
    pub dynamic_class_patterns: Vec<String>,
}

impl ComponentStructure {
    /// Collect all unique CSS classes referenced by this component
    /// structure, including the `prefix-*` patterns for
    /// dynamically-composed classes.
    pub fn collect_all_classes(&self) -> Vec<String> {
        let lookup_classes = self
            .variant_lookup
            .iter()
            .chain(&self.size_lookup)
            .flat_map(|(_, classes)| classes.split_whitespace());

        let all_classes = self
            .base_classes
            .split_whitespace()
            .chain(lookup_classes)
            .chain(self.disabled_classes.split_whitespace())
            .chain(self.dynamic_class_patterns.iter().map(String::as_str));

        let set: HashSet<&str> = all_classes.collect();
        let mut result: Vec<String> = set.into_iter().map(String::from).collect();
        result.sort();
        result
    }
}

/// Accumulates extraction results while walking the AST.
#[derive(Debug)]
struct ExtractionState<'c> {
    conventions: &'c ComponentConventions,
    variant_lookup: Vec<(String, String)>,
    size_lookup: Vec<(String, String)>,
    base_classes: Option<String>,
    disabled_classes: Option<String>,
    component_name: Option<String>,
    observed_attributes: Vec<String>,
    dynamic_class_patterns: Vec<String>,
}

impl<'c> ExtractionState<'c> {
    fn new(conventions: &'c ComponentConventions) -> Self {
        Self {
            conventions,
            variant_lookup: Vec::new(),
            size_lookup: Vec::new(),
            base_classes: None,
            disabled_classes: None,
            component_name: None,
            observed_attributes: Vec::new(),
            dynamic_class_patterns: Vec::new(),
        }
    }
}

/// React/JSX to Web Component adapter.
#[derive(Debug, Clone, Default)]
pub struct ReactAdapter {
    conventions: ComponentConventions,
}

impl ReactAdapter {
    /// Create a new React adapter with default conventions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new React adapter with custom conventions.
    pub fn with_conventions(conventions: ComponentConventions) -> Self {
        Self { conventions }
    }

    /// Extract component structure from source code using oxc AST parsing.
    pub fn extract_structure(&self, source: &str) -> Result<ComponentStructure, TransformError> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();

        if ret.panicked {
            return Err(TransformError::ParseError(
                "oxc parser panicked while parsing source".to_string(),
            ));
        }

        if !ret.errors.is_empty() {
            return Err(TransformError::ParseError(format!(
                "syntax errors in component source: {}",
                ret.errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        let mut state = ExtractionState::new(&self.conventions);

        for stmt in &ret.program.body {
            visit_statement(stmt, &mut state);
        }

        if state.variant_lookup.is_empty()
            && state.base_classes.is_none()
            && state.size_lookup.is_empty()
        {
            return Err(TransformError::MissingVariants);
        }

        // Fallback: if no attributes were found from interface or destructuring,
        // check for common attribute names used anywhere in the source. This
        // catches components that access props without destructuring (e.g., props.variant).
        if state.observed_attributes.is_empty() {
            for attr in &self.conventions.fallback_attributes {
                if source.contains(attr.as_str()) {
                    state.observed_attributes.push(attr.clone());
                }
            }
        }

        let default_variant = state
            .variant_lookup
            .first()
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "default".to_string());

        let default_size = state
            .size_lookup
            .first()
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| "default".to_string());

        let mut dynamic_class_patterns = state.dynamic_class_patterns;
        dynamic_class_patterns.sort();
        dynamic_class_patterns.dedup();

        Ok(ComponentStructure {
            name: state
                .component_name
                .unwrap_or_else(|| "Component".to_string()),
            base_classes: state.base_classes.unwrap_or_default(),
            disabled_classes: state
                .disabled_classes
                .unwrap_or_else(|| "opacity-50 pointer-events-none cursor-not-allowed".to_string()),
            variant_lookup: state.variant_lookup,
            size_lookup: state.size_lookup,
            default_variant,
            default_size,
            observed_attributes: state.observed_attributes,
            dynamic_class_patterns,
        })
    }
}

impl FrameworkAdapter for ReactAdapter {
    fn name(&self) -> &'static str {
        "react"
    }

    fn extensions(&self) -> &[&'static str] {
        &["tsx", "jsx"]
    }

    fn transform(
        &self,
        source: &str,
        tag_name: &str,
        ctx: &TransformContext,
    ) -> Result<TransformedBlock, TransformError> {
        let structure = self.extract_structure(source)?;
        // The preview adopts the sheet the context carries, whole. A
        // missing or empty sheet is an error naming the component and the
        // path -- a preview never renders silently unstyled (FR-VEN-018).
        preview_web_component_block(tag_name, &structure, &ctx.stylesheet)
    }
}

// --- AST walking ---

/// Process a single top-level statement.
fn visit_statement(stmt: &Statement<'_>, state: &mut ExtractionState<'_>) {
    match stmt {
        Statement::VariableDeclaration(decl) => {
            visit_variable_declaration(decl, state);
        }
        Statement::FunctionDeclaration(func) => {
            process_function_decl(func, state);
        }
        Statement::TSInterfaceDeclaration(iface) => {
            process_interface_decl(iface, state);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(ref decl) = export.declaration {
                visit_declaration(decl, state);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            use oxc_ast::ast::ExportDefaultDeclarationKind;
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &export.declaration {
                process_function_decl(func, state);
            }
        }
        _ => {}
    }
}

/// Process a declaration (shared by top-level statements and named exports).
fn visit_declaration(decl: &Declaration<'_>, state: &mut ExtractionState<'_>) {
    match decl {
        Declaration::VariableDeclaration(var_decl) => {
            visit_variable_declaration(var_decl, state);
        }
        Declaration::FunctionDeclaration(func) => {
            process_function_decl(func, state);
        }
        Declaration::TSInterfaceDeclaration(iface) => {
            process_interface_decl(iface, state);
        }
        _ => {}
    }
}

/// Extract component name and props from a function declaration.
fn process_function_decl(func: &oxc_ast::ast::Function<'_>, state: &mut ExtractionState<'_>) {
    if let Some(ref id) = func.id {
        let name = id.name.as_str();
        if is_pascal_case(name) && state.component_name.is_none() {
            state.component_name = Some(name.to_string());
        }
        extract_params_attributes(
            &func.params,
            &state.conventions.excluded_props,
            &mut state.observed_attributes,
        );
    }
}

/// Extract attributes from a TypeScript interface declaration ending in "Props".
fn process_interface_decl(
    iface: &oxc_ast::ast::TSInterfaceDeclaration<'_>,
    state: &mut ExtractionState<'_>,
) {
    if iface.id.name.as_str().ends_with("Props") {
        extract_interface_attributes(
            iface,
            &state.conventions.excluded_props,
            &mut state.observed_attributes,
        );
    }
}

/// Process a variable declaration, looking for known identifiers.
fn visit_variable_declaration(
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    state: &mut ExtractionState<'_>,
) {
    for declarator in &decl.declarations {
        let name = match &declarator.id.kind {
            BindingPatternKind::BindingIdentifier(id) => id.name.as_str(),
            _ => continue,
        };

        let Some(ref init) = declarator.init else {
            continue;
        };

        let init = unwrap_type_expressions(init);

        if state.conventions.variant_records.iter().any(|v| v == name) {
            if let Some(entries) = extract_object_entries(init, &mut state.dynamic_class_patterns) {
                state.variant_lookup = entries;
            }
        } else if state.conventions.size_records.iter().any(|v| v == name) {
            if let Some(entries) = extract_object_entries(init, &mut state.dynamic_class_patterns) {
                state.size_lookup = entries;
            }
        } else if state.conventions.base_class_vars.iter().any(|v| v == name) {
            if let Some(value) = extract_class_value(init) {
                state.base_classes = Some(normalize_whitespace(&value.static_classes));
                state.dynamic_class_patterns.extend(value.patterns);
            }
        } else if state
            .conventions
            .disabled_class_vars
            .iter()
            .any(|v| v == name)
        {
            if let Some(value) = extract_class_value(init) {
                state.disabled_classes = Some(value.static_classes);
                state.dynamic_class_patterns.extend(value.patterns);
            }
        } else if is_pascal_case(name) && state.component_name.is_none() {
            let params = match init {
                Expression::ArrowFunctionExpression(arrow) => Some(&arrow.params),
                Expression::FunctionExpression(func) => Some(&func.params),
                _ => None,
            };
            if let Some(params) = params {
                state.component_name = Some(name.to_string());
                extract_params_attributes(
                    params,
                    &state.conventions.excluded_props,
                    &mut state.observed_attributes,
                );
            }
        }
    }
}

/// Extract key-value pairs from an ObjectExpression. Dynamically-composed
/// class tokens inside the values are appended to `patterns` as `prefix-*`
/// entries; the entry itself keeps only its static classes.
fn extract_object_entries(
    expr: &Expression<'_>,
    patterns: &mut Vec<String>,
) -> Option<Vec<(String, String)>> {
    let Expression::ObjectExpression(obj) = expr else {
        return None;
    };

    let mut entries = Vec::new();

    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(prop) = prop else {
            continue;
        };

        let key = match &prop.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
            _ => continue,
        };

        let value_expr = unwrap_type_expressions(&prop.value);

        if let Some(value) = extract_class_value(value_expr) {
            entries.push((key, value.static_classes));
            patterns.extend(value.patterns);
        } else if let Some(value) = extract_nested_object_classes(value_expr, patterns) {
            entries.push((key, value));
        }
    }

    Some(entries)
}

// --- Attribute helpers ---

/// Extract observed attributes from a TSInterfaceDeclaration whose name ends with "Props".
fn extract_interface_attributes(
    iface: &oxc_ast::ast::TSInterfaceDeclaration<'_>,
    excluded_props: &[String],
    attrs: &mut Vec<String>,
) {
    for sig in &iface.body.body {
        if let TSSignature::TSPropertySignature(prop) = sig {
            let name = match &prop.key {
                PropertyKey::StaticIdentifier(id) => id.name.as_str(),
                _ => continue,
            };

            if should_include_attribute(name, excluded_props) && !attrs.contains(&name.to_string())
            {
                attrs.push(name.to_string());
            }
        }
    }
}

/// Extract observed attributes from function parameters (destructured object pattern).
fn extract_params_attributes(
    params: &oxc_ast::ast::FormalParameters<'_>,
    excluded_props: &[String],
    attrs: &mut Vec<String>,
) {
    for param in &params.items {
        if let BindingPatternKind::ObjectPattern(obj_pat) = &param.pattern.kind {
            for prop in &obj_pat.properties {
                let name = match &prop.key {
                    PropertyKey::StaticIdentifier(id) => id.name.as_str(),
                    _ => continue,
                };

                if should_include_attribute(name, excluded_props)
                    && !attrs.contains(&name.to_string())
                {
                    attrs.push(name.to_string());
                }
            }
        }
    }
}

/// Determine if an attribute name should be included in observed attributes.
/// Excludes props listed in the conventions' excluded_props list.
fn should_include_attribute(name: &str, excluded_props: &[String]) -> bool {
    !name.is_empty() && !excluded_props.iter().any(|p| p == name)
}

// --- String helpers ---

/// Check if a string is PascalCase (starts with uppercase letter).
fn is_pascal_case(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal project stylesheet covering the classes the test
    /// components declare, in the rafters exporter output shape.
    const TEST_CSS: &str = r#"
@theme {
  --color-primary: oklch(0.645 0.12 180);
}

@utility bg-primary {
  background-color: var(--color-primary);
}

@utility bg-blue-500 {
  background-color: oklch(0.62 0.19 260);
}

@utility opacity-50 {
  opacity: 0.5;
}
"#;

    fn test_ctx() -> TransformContext {
        TransformContext {
            stylesheet: TEST_CSS.to_string(),
            ..TransformContext::default()
        }
    }

    #[test]
    fn extracts_variant_classes() {
        let source = r#"
const variantClasses: Record<string, string> = {
  default: 'bg-primary text-primary-foreground',
  secondary: 'bg-secondary text-secondary-foreground',
};

export function Button() {
  return <button />;
}
        "#;

        let adapter = ReactAdapter::new();
        let result = adapter
            .transform(source, "button-preview", &test_ctx())
            .unwrap();

        assert!(result.web_component.contains("variantClasses"));
        assert!(result.web_component.contains("bg-primary"));
        assert!(result.classes_used.contains(&"bg-primary".to_string()));
        // The module adopts the shared sheet; it carries no rule of its own.
        assert!(result
            .web_component
            .contains("import { previewStyles } from './preview-styles.js';"));
        assert!(!result.web_component.contains(".bg-primary {"));
    }

    // FR-VEN-018: with no stylesheet in the context, a classed component
    // refuses to transform -- never a preview silently missing its styles.
    #[test]
    fn transform_errors_without_a_stylesheet() {
        let source = r#"
const variantClasses = { default: 'bg-primary' };

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let error = adapter
            .transform(source, "button-preview", &TransformContext::default())
            .expect_err("an empty stylesheet cannot style a classed component");
        assert!(matches!(
            error,
            TransformError::RenderFailed { ref component, .. } if component == "Button"
        ));
    }

    // FR-VEN-018 (bullpen 019f1f4d): a dynamically-composed class in a
    // convention-named variable surfaces as a prefix pattern on the
    // structure, keeping its static siblings.
    #[test]
    fn extracts_dynamic_class_patterns_from_base_classes() {
        let source = r#"
const variantClasses = { default: 'bg-primary' };
const baseClasses = `text-quality-${tint} font-bold`;

export function QualityBadge() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.base_classes, "font-bold");
        assert_eq!(
            structure.dynamic_class_patterns,
            vec!["text-quality-*".to_string()]
        );
        assert!(structure
            .collect_all_classes()
            .contains(&"text-quality-*".to_string()));
    }

    // Dynamic composition inside a variant record value: the entry keeps
    // its static classes and the pattern reaches the structure.
    #[test]
    fn extracts_dynamic_class_patterns_from_variant_records() {
        let source = r#"
const variantClasses = {
  default: `bg-quality-${tint} rounded`,
};

export function QualityBadge() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(
            structure.variant_lookup,
            vec![("default".to_string(), "rounded".to_string())]
        );
        assert_eq!(
            structure.dynamic_class_patterns,
            vec!["bg-quality-*".to_string()]
        );
    }

    #[test]
    fn extracts_concatenated_base_classes() {
        let source = r#"
const variantClasses = { default: '' };
const baseClasses =
  'inline-flex items-center ' +
  'justify-center gap-2';

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert!(structure.base_classes.contains("inline-flex"));
        assert!(structure.base_classes.contains("items-center"));
        assert!(structure.base_classes.contains("justify-center"));
    }

    #[test]
    fn extracts_simple_base_classes() {
        let source = r#"
const variantClasses = { default: '' };
const baseClasses = 'inline-flex items-center';

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.base_classes, "inline-flex items-center");
    }

    #[test]
    fn errors_on_missing_variants() {
        let source = "export function Button() { return <button />; }";

        let adapter = ReactAdapter::new();
        let result = adapter.transform(source, "button-preview", &TransformContext::default());

        assert!(matches!(result, Err(TransformError::MissingVariants)));
    }

    #[test]
    fn extracts_observed_attributes() {
        let source = r#"
const variantClasses = { default: '' };

interface ButtonProps {
  variant?: string;
  size?: string;
  disabled?: boolean;
  loading?: boolean;
}

export function Button({ variant, size, disabled, loading }: ButtonProps) {}
        "#;

        let adapter = ReactAdapter::new();
        let result = adapter
            .transform(source, "button-preview", &test_ctx())
            .unwrap();

        assert!(result.attributes.contains(&"variant".to_string()));
        assert!(result.attributes.contains(&"size".to_string()));
        assert!(result.attributes.contains(&"disabled".to_string()));
        assert!(result.attributes.contains(&"loading".to_string()));
    }

    #[test]
    fn generates_valid_tag_name() {
        let source = r#"
const variantClasses = { primary: 'bg-blue-500' };
export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let result = adapter.transform(source, "my-button", &test_ctx()).unwrap();

        assert_eq!(result.tag_name, "my-button");
        assert!(result.web_component.contains("my-button"));
    }

    #[test]
    fn extracts_attributes_from_arrow_function_component() {
        let source = r#"
const variantClasses = { default: '' };

interface ButtonProps {
  variant?: string;
  size?: string;
}

const Button = ({ variant, size }: ButtonProps) => {
  return <button />;
};
        "#;

        let adapter = ReactAdapter::new();
        let result = adapter
            .transform(source, "button-preview", &test_ctx())
            .unwrap();

        assert!(result.attributes.contains(&"variant".to_string()));
        assert!(result.attributes.contains(&"size".to_string()));
    }

    #[test]
    fn common_attributes_fallback_when_no_interface_or_destructuring() {
        let source = r#"
const variantClasses = { default: 'bg-primary' };

export function Button(props: any) {
  const cls = props.variant === 'primary' ? 'bg-blue' : 'bg-gray';
  const isDisabled = props.disabled;
  return <button className={cls} disabled={isDisabled} />;
}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert!(structure
            .observed_attributes
            .contains(&"variant".to_string()));
        assert!(structure
            .observed_attributes
            .contains(&"disabled".to_string()));
    }

    #[test]
    fn handles_as_const_satisfies_pattern() {
        let source = r#"
const variantClasses = {
  default: 'bg-primary text-primary-foreground',
  secondary: 'bg-secondary text-secondary-foreground',
} as const satisfies Record<string, string>;

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.variant_lookup.len(), 2);
        assert_eq!(
            structure.variant_lookup[0],
            (
                "default".to_string(),
                "bg-primary text-primary-foreground".to_string()
            )
        );
        assert_eq!(
            structure.variant_lookup[1],
            (
                "secondary".to_string(),
                "bg-secondary text-secondary-foreground".to_string()
            )
        );
    }

    #[test]
    fn handles_comments_inside_objects() {
        let source = r#"
const variantClasses = {
  // Primary variant for main actions
  default: 'bg-primary text-primary-foreground',
  /* Secondary variant for less important actions */
  secondary: 'bg-secondary text-secondary-foreground',
};

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.variant_lookup.len(), 2);
        assert_eq!(
            structure.variant_lookup[0],
            (
                "default".to_string(),
                "bg-primary text-primary-foreground".to_string()
            )
        );
    }

    #[test]
    fn handles_template_literals_in_class_values() {
        let source = r#"
const variantClasses = {
  default: `bg-primary text-primary-foreground`,
  secondary: `bg-secondary text-secondary-foreground`,
};

const baseClasses = `inline-flex items-center`;

export function Button() {}
        "#;

        let adapter = ReactAdapter::new();
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.variant_lookup.len(), 2);
        assert_eq!(
            structure.variant_lookup[0],
            (
                "default".to_string(),
                "bg-primary text-primary-foreground".to_string()
            )
        );
        assert_eq!(structure.base_classes, "inline-flex items-center");
    }

    #[test]
    fn custom_variant_record_name() {
        let source = r#"
const styles = {
  primary: 'bg-blue-500',
  danger: 'bg-red-500',
};

export function Alert() {}
        "#;

        let conventions = ComponentConventions {
            variant_records: vec!["styles".to_string()],
            ..Default::default()
        };
        let adapter = ReactAdapter::with_conventions(conventions);
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.variant_lookup.len(), 2);
        assert_eq!(structure.variant_lookup[0].0, "primary");
        assert_eq!(structure.variant_lookup[1].0, "danger");
    }

    #[test]
    fn custom_size_record_name() {
        let source = r#"
const variantClasses = { default: 'bg-primary' };
const dimensions = {
  small: 'h-8 px-2',
  large: 'h-12 px-4',
};

export function Button() {}
        "#;

        let conventions = ComponentConventions {
            size_records: vec!["dimensions".to_string()],
            ..Default::default()
        };
        let adapter = ReactAdapter::with_conventions(conventions);
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.size_lookup.len(), 2);
        assert_eq!(structure.size_lookup[0].0, "small");
        assert_eq!(structure.size_lookup[1].0, "large");
    }

    #[test]
    fn multiple_variant_record_names() {
        let source = r#"
const colorStyles = {
  red: 'text-red-500',
  blue: 'text-blue-500',
};

export function Badge() {}
        "#;

        let conventions = ComponentConventions {
            variant_records: vec!["variantClasses".to_string(), "colorStyles".to_string()],
            ..Default::default()
        };
        let adapter = ReactAdapter::with_conventions(conventions);
        let structure = adapter.extract_structure(source).unwrap();

        assert_eq!(structure.variant_lookup.len(), 2);
        assert_eq!(structure.variant_lookup[0].0, "red");
        assert_eq!(structure.variant_lookup[1].0, "blue");
    }

    #[test]
    fn custom_excluded_props() {
        let source = r#"
const variantClasses = { default: '' };

interface WidgetProps {
  variant?: string;
  ref?: any;
  key?: string;
  label?: string;
}

export function Widget({ variant, ref, key, label }: WidgetProps) {}
        "#;

        let conventions = ComponentConventions {
            excluded_props: vec!["ref".to_string(), "key".to_string()],
            ..Default::default()
        };
        let adapter = ReactAdapter::with_conventions(conventions);
        let structure = adapter.extract_structure(source).unwrap();

        assert!(structure
            .observed_attributes
            .contains(&"variant".to_string()));
        assert!(structure.observed_attributes.contains(&"label".to_string()));
        assert!(!structure.observed_attributes.contains(&"ref".to_string()));
        assert!(!structure.observed_attributes.contains(&"key".to_string()));
    }

    #[test]
    fn errors_when_no_matching_conventions() {
        let source = r#"
const myStyles = {
  primary: 'bg-blue-500',
};

export function Button() {}
        "#;

        // Default conventions won't match "myStyles"
        let adapter = ReactAdapter::new();
        let result = adapter.extract_structure(source);

        assert!(matches!(result, Err(TransformError::MissingVariants)));
    }
}
