//! Read the part-to-classes map a component's `.classes.ts` declares.
//!
//! rafters components export a classes function that names each part and the
//! classes it carries:
//!
//! ```text
//! export function avatarClasses(config: AvatarConfig): AvatarClassSet {
//!   const { size } = resolveAvatar(config);
//!   return {
//!     root:     `${avatarBaseClasses} ${avatarSizeClasses[size]}`,
//!     image:    avatarImageClasses,
//!     fallback: avatarFallbackClasses,
//!   };
//! }
//! ```
//!
//! 48 of the 55 installed components declare their parts this way, and the
//! JSX marks the matching nodes with `data-part="root"`. The mapping is
//! stated in source; nothing here infers it.
//!
//! veneer used to ignore this function entirely, scrape the loose constants
//! it composes, and union them onto one element. That is how an avatar's
//! `image` and `fallback` classes — `h-full w-full`, written for a child
//! inside a sized parent — landed on the root and rendered a circle as wide
//! as the page. The constants were never unattributed: they are labelled
//! `image` and `fallback` right here.

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Expression, ObjectPropertyKind, PropertyKey, Statement, TSType};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// The parts a component declares, each with the classes it carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassParts {
    /// Part name (`root`, `image`, `fallback`, `trigger`, …) to class string.
    pub parts: BTreeMap<String, String>,
    /// Parts whose class expression this reader could not resolve, with the
    /// source text that defeated it. Recorded rather than dropped: a part
    /// that silently vanishes is indistinguishable from one that does not
    /// exist.
    pub unresolved: Vec<(String, String)>,
}

impl ClassParts {
    /// The classes for the component's own root element, if it declares one.
    pub fn root(&self) -> Option<&str> {
        self.parts.get("root").map(String::as_str)
    }
}

/// A local `const` in the classes file: either a class string or a record of
/// them (`{ sm: 'h-8', md: 'h-10' }`).
enum Local {
    Text(String),
    Record(Vec<(String, String)>),
}

/// Read the part map a `.classes.ts` declares.
///
/// `defaults` supplies the key to use for a record lookup such as
/// `avatarSizeClasses[size]` — the value the component's own props declare as
/// its default. Without a default for a key, the first entry is used and the
/// part is still resolved, because a record's first entry is a defensible
/// preview whereas dropping the part is not.
pub fn read_class_parts(source: &str, defaults: &BTreeMap<String, String>) -> Option<ClassParts> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::ts()).parse();
    if ret.panicked {
        return None;
    }

    let mut locals: BTreeMap<String, Local> = BTreeMap::new();
    for stmt in &ret.program.body {
        let declarations = match stmt {
            Statement::VariableDeclaration(decl) => Some(&decl.declarations),
            Statement::ExportNamedDeclaration(export) => match &export.declaration {
                Some(Declaration::VariableDeclaration(decl)) => Some(&decl.declarations),
                _ => None,
            },
            _ => None,
        };
        let Some(declarations) = declarations else {
            continue;
        };
        for declarator in declarations {
            let (Some(name), Some(init)) = (
                declarator.id.get_identifier_name(),
                declarator.init.as_ref(),
            ) else {
                continue;
            };
            if let Some(local) = local_value(init, &locals) {
                locals.insert(name.to_string(), local);
            }
        }
    }

    let (returned, body_locals) = classes_function_return(&ret.program.body)?;
    // Consts declared INSIDE the classes function are where several
    // components assemble their parts (`const root = [...].join(' ')`).
    for (name, local) in body_locals {
        locals.entry(name).or_insert(local);
    }

    let mut parts = ClassParts::default();
    for property in &returned.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        let name = match &property.key {
            PropertyKey::StaticIdentifier(ident) => ident.name.to_string(),
            PropertyKey::StringLiteral(literal) => literal.value.to_string(),
            _ => continue,
        };
        match resolve(&property.value, &locals, defaults) {
            Some(classes) => {
                parts.parts.insert(name, normalize(&classes));
            }
            None => parts
                .unresolved
                .push((name, "unsupported class expression".to_string())),
        }
    }

    (!parts.parts.is_empty() || !parts.unresolved.is_empty()).then_some(parts)
}

/// The object literal returned by the file's classes function, plus the
/// consts that function declares locally.
///
/// A file often exports several `*Classes` helpers (`resolveColumnsClasses`
/// beside `gridClasses`); the one that matters is the one that returns a part
/// map, so this takes the first with an object-literal return rather than the
/// first by name.
fn classes_function_return<'a>(
    body: &'a oxc_allocator::Vec<'a, Statement<'a>>,
) -> Option<(&'a oxc_ast::ast::ObjectExpression<'a>, Vec<(String, Local)>)> {
    for stmt in body {
        let Statement::ExportNamedDeclaration(export) = stmt else {
            continue;
        };
        let Some(Declaration::FunctionDeclaration(func)) = &export.declaration else {
            continue;
        };
        if !func
            .id
            .as_ref()
            .is_some_and(|id| id.name.ends_with("Classes"))
        {
            continue;
        }
        let Some(func_body) = func.body.as_ref() else {
            continue;
        };

        // Function-body consts, resolved in declaration order so a later one
        // can build on an earlier one.
        let mut body_locals: BTreeMap<String, Local> = BTreeMap::new();
        for stmt in &func_body.statements {
            if let Statement::VariableDeclaration(decl) = stmt {
                for declarator in &decl.declarations {
                    if let (Some(name), Some(init)) = (
                        declarator.id.get_identifier_name(),
                        declarator.init.as_ref(),
                    ) {
                        if let Some(local) = local_value(init, &body_locals) {
                            body_locals.insert(name.to_string(), local);
                        }
                    }
                }
            }
        }

        for stmt in &func_body.statements {
            if let Statement::ReturnStatement(ret) = stmt {
                if let Some(Expression::ObjectExpression(object)) = ret.argument.as_ref() {
                    return Some((object, body_locals.into_iter().collect()));
                }
            }
        }
    }
    None
}

fn local_value(expr: &Expression<'_>, locals: &BTreeMap<String, Local>) -> Option<Local> {
    match expr {
        Expression::StringLiteral(literal) => Some(Local::Text(literal.value.to_string())),
        Expression::TemplateLiteral(_)
        | Expression::BinaryExpression(_)
        | Expression::ArrayExpression(_)
        | Expression::CallExpression(_) => text_of(expr, locals, &BTreeMap::new()).map(Local::Text),
        Expression::TSAsExpression(inner) => local_value(&inner.expression, locals),
        Expression::ObjectExpression(object) => {
            let mut entries = Vec::new();
            for property in &object.properties {
                let ObjectPropertyKind::ObjectProperty(property) = property else {
                    continue;
                };
                let key = match &property.key {
                    PropertyKey::StaticIdentifier(ident) => ident.name.to_string(),
                    PropertyKey::StringLiteral(literal) => literal.value.to_string(),
                    _ => continue,
                };
                if let Some(value) = text_of(&property.value, locals, &BTreeMap::new()) {
                    entries.push((key, value));
                }
            }
            (!entries.is_empty()).then_some(Local::Record(entries))
        }
        _ => None,
    }
}

/// Resolve a part's class expression to a class string.
fn resolve(
    expr: &Expression<'_>,
    locals: &BTreeMap<String, Local>,
    defaults: &BTreeMap<String, String>,
) -> Option<String> {
    text_of(expr, locals, defaults)
}

fn text_of(
    expr: &Expression<'_>,
    locals: &BTreeMap<String, Local>,
    defaults: &BTreeMap<String, String>,
) -> Option<String> {
    match expr {
        Expression::StringLiteral(literal) => Some(literal.value.to_string()),
        Expression::TSAsExpression(inner) => text_of(&inner.expression, locals, defaults),
        Expression::ParenthesizedExpression(inner) => text_of(&inner.expression, locals, defaults),

        Expression::Identifier(ident) => match locals.get(ident.name.as_str()) {
            Some(Local::Text(text)) => Some(text.clone()),
            // A bare record reference contributes nothing on its own.
            Some(Local::Record(_)) | None => None,
        },

        // `${baseClasses} ${sizeClasses[size]}`
        Expression::TemplateLiteral(template) => {
            let mut out = String::new();
            for (index, quasi) in template.quasis.iter().enumerate() {
                out.push_str(quasi.value.raw.as_str());
                if let Some(expression) = template.expressions.get(index) {
                    out.push_str(&text_of(expression, locals, defaults)?);
                }
            }
            Some(out)
        }

        // `a + b`
        Expression::BinaryExpression(binary) => {
            let left = text_of(&binary.left, locals, defaults)?;
            let right = text_of(&binary.right, locals, defaults)?;
            Some(format!("{left}{right}"))
        }

        // `sizeClasses[size]` — the key is the component's declared default
        // when it has one, else the record's first entry.
        Expression::ComputedMemberExpression(member) => {
            let Expression::Identifier(record) = &member.object else {
                return None;
            };
            let Some(Local::Record(entries)) = locals.get(record.name.as_str()) else {
                return None;
            };
            let key = match &member.expression {
                Expression::Identifier(ident) => defaults.get(ident.name.as_str()).cloned(),
                Expression::StringLiteral(literal) => Some(literal.value.to_string()),
                _ => None,
            };
            match key {
                Some(key) => entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .or_else(|| entries.first())
                    .map(|(_, v)| v.clone()),
                None => entries.first().map(|(_, v)| v.clone()),
            }
        }

        // `[a, b].filter(Boolean).join(' ')` -- how several components
        // assemble a part out of optional pieces.
        Expression::CallExpression(call) => {
            let member = call.callee.get_member_expr()?;
            let method = member.static_property_name()?;
            match method {
                "join" => text_of(member.object(), locals, defaults),
                "filter" => text_of(member.object(), locals, defaults),
                _ => None,
            }
        }

        Expression::ArrayExpression(array) => {
            let mut pieces = Vec::new();
            for element in &array.elements {
                let Some(expression) = element.as_expression() else {
                    continue;
                };
                // A piece that does not resolve is skipped, not fatal: these
                // arrays are `.filter(Boolean)`-ed precisely because entries
                // are conditional.
                if let Some(text) = text_of(expression, locals, defaults) {
                    if !text.trim().is_empty() {
                        pieces.push(text);
                    }
                }
            }
            (!pieces.is_empty()).then(|| pieces.join(" "))
        }

        _ => None,
    }
}

fn normalize(classes: &str) -> String {
    classes.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unused today; kept out of the public surface until a caller needs it.
#[allow(dead_code)]
fn type_text(ty: &TSType<'_>) -> String {
    format!("{ty:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_defaults() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    const AVATAR: &str = r#"
        const avatarBaseClasses = 'relative flex shrink-0 overflow-hidden rounded-full';
        const avatarSizeClasses = {
          xs: 'h-6 w-6 text-xs',
          md: 'h-10 w-10 text-base',
        };
        const avatarImageClasses = 'aspect-square h-full w-full object-cover';
        const avatarFallbackClasses = 'flex h-full w-full items-center justify-center rounded-full bg-muted';
        export function avatarClasses(config) {
          const { size } = resolveAvatar(config);
          return {
            root: `${avatarBaseClasses} ${avatarSizeClasses[size]}`,
            image: avatarImageClasses,
            fallback: avatarFallbackClasses,
          };
        }
    "#;

    #[test]
    fn reads_each_declared_part_separately() {
        let parts = read_class_parts(AVATAR, &no_defaults()).expect("avatar declares parts");
        assert_eq!(parts.parts.len(), 3);
        assert!(parts.parts.contains_key("root"));
        assert!(parts.parts.contains_key("image"));
        assert!(parts.parts.contains_key("fallback"));
    }

    #[test]
    fn a_child_parts_classes_never_reach_the_root() {
        // The avatar bug, pinned: `h-full w-full` belongs to image and
        // fallback. On the root, with nothing constraining it, it rendered a
        // circle as wide as the page.
        let parts = read_class_parts(AVATAR, &no_defaults()).expect("avatar declares parts");
        let root = parts.root().expect("a root part");
        assert!(
            !root.contains("h-full"),
            "root must not carry a part's h-full: {root}"
        );
        assert!(
            !root.contains("w-full"),
            "root must not carry a part's w-full: {root}"
        );
        assert!(root.contains("rounded-full"));
        assert!(parts.parts["image"].contains("h-full w-full"));
    }

    #[test]
    fn the_declared_default_picks_the_record_entry() {
        let mut defaults = BTreeMap::new();
        defaults.insert("size".to_string(), "md".to_string());
        let parts = read_class_parts(AVATAR, &defaults).expect("avatar declares parts");
        let root = parts.root().unwrap();
        assert!(
            root.contains("h-10 w-10"),
            "declared default size is md: {root}"
        );
        assert!(!root.contains("h-6 w-6"));
    }

    #[test]
    fn without_a_declared_default_the_first_entry_is_used() {
        let parts = read_class_parts(AVATAR, &no_defaults()).unwrap();
        assert!(parts.root().unwrap().contains("h-6 w-6"));
    }

    #[test]
    fn an_unresolvable_part_is_recorded_not_dropped() {
        // A part whose expression this reader cannot evaluate must stay
        // visible: silently missing and non-existent look identical.
        let source = r#"
            const a = 'x';
            export function thingClasses(config) {
              return { root: a, track: vertical ? trackVertical : trackHorizontal };
            }
        "#;
        let parts = read_class_parts(source, &no_defaults()).unwrap();
        assert_eq!(parts.root(), Some("x"));
        assert_eq!(parts.unresolved.len(), 1);
        assert_eq!(parts.unresolved[0].0, "track");
    }

    #[test]
    fn a_file_with_no_classes_function_yields_nothing() {
        let source = "export const looseClasses = 'p-2';";
        assert!(read_class_parts(source, &no_defaults()).is_none());
    }
}
