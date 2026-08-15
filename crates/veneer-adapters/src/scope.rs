//! Class extraction from `.classes.ts` sources.
//!
//! [`extract_classes_from_ts`] parses a `.classes.ts` file and returns every
//! individual Tailwind class token found in string literals, plus `prefix-*`
//! patterns for classes composed dynamically at render (template literals or
//! concatenations with runtime values).
//!
//! This module no longer selects CSS. It used to hold the `@utility`-matching
//! scoper that built a per-component shadow stylesheet out of the rafters
//! SOURCE sheet (`rafters.css`); that path is retired (issue #105). Previews
//! now adopt the project's COMPILED documentation sheet whole, so there is
//! nothing to match: the compiled sheet contains no `@utility` blocks at all,
//! and per-component tree-shaking fights the adopt-whole thesis besides.
//!
//! The class list survives the retirement because it is docs data in its own
//! right -- the page reports what a component resolves to, and the registry
//! reads the same walk -- not because anything still selects CSS with it.

use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::ts_helpers::{class_template_value, scopable_class_token, unwrap_type_expressions};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract class names from a `.classes.ts` file.
///
/// Parses the TypeScript source to find all string literals used as Tailwind
/// class names in variant/size/state maps and string constants.  Returns
/// individual space-separated class tokens deduplicated and sorted.
pub fn extract_classes_from_ts(source: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::ts();
    let ret = Parser::new(&allocator, source, source_type).parse();

    if ret.panicked {
        eprintln!("veneer/extract_classes_from_ts: parser panicked");
        return Vec::new();
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut classes: Vec<String> = Vec::new();

    for stmt in &ret.program.body {
        collect_classes_from_statement(stmt, &mut seen, &mut classes);
    }

    classes.sort();
    classes
}

// ---------------------------------------------------------------------------
// TypeScript AST walking helpers
// ---------------------------------------------------------------------------

fn collect_classes_from_statement(
    stmt: &Statement<'_>,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    match stmt {
        Statement::ExportNamedDeclaration(export) => {
            if let Some(oxc_ast::ast::Declaration::VariableDeclaration(var_decl)) =
                &export.declaration
            {
                for declarator in &var_decl.declarations {
                    if let Some(ref init) = declarator.init {
                        collect_classes_from_expr(unwrap_type_expressions(init), seen, out);
                    }
                }
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(ref init) = declarator.init {
                    collect_classes_from_expr(unwrap_type_expressions(init), seen, out);
                }
            }
        }
        _ => {}
    }
}

/// Collect every class token an expression can resolve to at render:
/// string-shaped values (with dynamically-composed tokens as `prefix-*`
/// patterns), object/array members, conditional arms, and class-builder
/// arrow bodies. Shared with registry export discovery so the real
/// extraction pipeline surfaces the same tokens the tests prove.
pub(crate) fn collect_classes_from_expr(
    expr: &Expression<'_>,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    // String-shaped value (literal, template, concatenation) — split into
    // tokens; dynamically-composed parts become `prefix-*` patterns.
    if let Some(value) = class_template_value(expr) {
        add_classes(&value, seen, out);
        return;
    }

    match expr {
        // Object expression — recurse into each property value.
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) = prop {
                    collect_classes_from_expr(unwrap_type_expressions(&p.value), seen, out);
                }
            }
        }
        // Array expression — recurse into elements.
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                let expr_ref = match elem {
                    oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => continue,
                    oxc_ast::ast::ArrayExpressionElement::Elision(_) => continue,
                    _ => elem.to_expression(),
                };
                collect_classes_from_expr(unwrap_type_expressions(expr_ref), seen, out);
            }
        }
        // Conditional — the component resolves to either arm at render.
        Expression::ConditionalExpression(cond) => {
            collect_classes_from_expr(unwrap_type_expressions(&cond.consequent), seen, out);
            collect_classes_from_expr(unwrap_type_expressions(&cond.alternate), seen, out);
        }
        // Class-builder arrow (for example `(tint) => \`text-quality-${tint}\``)
        // — the returned expressions are what the component resolves to.
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                match stmt {
                    Statement::ExpressionStatement(expr_stmt) => {
                        collect_classes_from_expr(
                            unwrap_type_expressions(&expr_stmt.expression),
                            seen,
                            out,
                        );
                    }
                    Statement::ReturnStatement(ret) => {
                        if let Some(ref argument) = ret.argument {
                            collect_classes_from_expr(unwrap_type_expressions(argument), seen, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Split a class string and add individual tokens to the output,
/// deduplicating. A token containing a dynamic hole is a
/// dynamically-composed class: [`scopable_class_token`] turns it into a
/// `prefix-*` pattern from the static text before the hole (a token with
/// no static prefix cannot be scoped and is skipped).
fn add_classes(class_string: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
    for raw in class_string.split_whitespace() {
        let Some(token) = scopable_class_token(raw) else {
            continue;
        };
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TS: &str = r#"
export const kbdBaseClasses = 'inline-flex items-center rounded border bg-muted px-1.5 py-0.5';

export const typographyClasses = {
  h1: 'scroll-m-20 text-4xl font-bold tracking-tight',
  h2: 'scroll-m-20 text-3xl font-semibold tracking-tight',
  muted: 'text-sm text-muted-foreground',
} as const;
"#;

    #[test]
    fn extract_classes_finds_flat_string() {
        let classes = extract_classes_from_ts(SAMPLE_TS);
        assert!(classes.contains(&"inline-flex".to_string()));
        assert!(classes.contains(&"items-center".to_string()));
        assert!(classes.contains(&"rounded".to_string()));
        assert!(classes.contains(&"bg-muted".to_string()));
    }

    #[test]
    fn extract_classes_finds_object_values() {
        let classes = extract_classes_from_ts(SAMPLE_TS);
        assert!(classes.contains(&"text-4xl".to_string()));
        assert!(classes.contains(&"font-bold".to_string()));
        assert!(classes.contains(&"text-muted-foreground".to_string()));
    }

    #[test]
    fn extract_classes_deduplicates() {
        let classes = extract_classes_from_ts(SAMPLE_TS);
        let count = classes
            .iter()
            .filter(|c| c.as_str() == "scroll-m-20")
            .count();
        assert_eq!(
            count, 1,
            "scroll-m-20 appears twice in source but must be deduped"
        );
    }

    #[test]
    fn extract_classes_returns_empty_for_empty_source() {
        let classes = extract_classes_from_ts("");
        assert!(classes.is_empty());
    }

    // --- Dynamically-composed classes (Tailwind tree-shake caveat) ---
    //
    // Tailwind tree-shakes class names that never appear as source literals
    // (`color={tint}` resolving to `text-quality-*`). Extraction must
    // surface a `prefix-*` pattern for those so scoping can include every
    // utility the component resolves to at render.

    #[test]
    fn extract_classes_emits_prefix_pattern_for_template_expression() {
        let source = r#"
export const qualityClasses = {
  badge: `text-quality-${tint} font-bold`,
};
"#;
        let classes = extract_classes_from_ts(source);
        assert!(classes.contains(&"text-quality-*".to_string()));
        assert!(classes.contains(&"font-bold".to_string()));
    }

    #[test]
    fn extract_classes_emits_prefix_pattern_for_concatenation() {
        let source = "export const tintClass = 'text-quality-' + tint;";
        let classes = extract_classes_from_ts(source);
        assert_eq!(classes, vec!["text-quality-*".to_string()]);
    }

    #[test]
    fn extract_classes_emits_prefix_pattern_from_class_builder_arrow() {
        let source = "export const tintClass = (tint: string) => `bg-quality-${tint}`;";
        let classes = extract_classes_from_ts(source);
        assert_eq!(classes, vec!["bg-quality-*".to_string()]);
    }

    #[test]
    fn extract_classes_resolves_conditional_arms() {
        let source =
            "export const stateClass = (active: boolean) => active ? 'bg-primary' : 'bg-muted';";
        let classes = extract_classes_from_ts(source);
        assert!(classes.contains(&"bg-primary".to_string()));
        assert!(classes.contains(&"bg-muted".to_string()));
    }

    #[test]
    fn extract_classes_skips_token_with_no_static_prefix() {
        let source = "export const cls = `${dynamic} flex`;";
        let classes = extract_classes_from_ts(source);
        assert_eq!(classes, vec!["flex".to_string()]);
    }
}
