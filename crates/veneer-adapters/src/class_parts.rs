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
#[derive(Clone)]
enum Local {
    Text(String),
    /// A record whose values may themselves be records --
    /// `variantClasses.default.border` is two levels deep.
    Record(Vec<(String, Local)>),
}

/// Read the part map a `.classes.ts` declares.
///
/// `defaults` supplies the key to use for a record lookup such as
/// `avatarSizeClasses[size]` — the value the component's own props declare as
/// its default. Without a default for a key, the first entry is used and the
/// part is still resolved, because a record's first entry is a defensible
/// preview whereas dropping the part is not.
/// Resolve the class constants a JSX `className` named, against the
/// declarations in the component's `.classes.ts`.
///
/// Names the classes file does not declare are skipped -- `className` in
/// `classy(tableWrapperClasses, className)` is the caller's prop
/// passthrough, not a constant. Returns `None` when none of the names
/// resolve, so the caller can tell "no classes declared" from "classes I
/// failed to read".
pub fn resolve_named_classes(source: &str, names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
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
            if let (Some(name), Some(init)) = (
                declarator.id.get_identifier_name(),
                declarator.init.as_ref(),
            ) {
                if let Some(local) = local_value(init, &locals) {
                    locals.insert(name.to_string(), local);
                }
            }
        }
    }

    let mut pieces = Vec::new();
    for name in names {
        if let Some(Local::Text(text)) = locals.get(name.as_str()) {
            if !text.trim().is_empty() {
                pieces.push(text.clone());
            }
        }
    }
    (!pieces.is_empty()).then(|| normalize(&pieces.join(" ")))
}

/// Read the part map a `.classes.ts` declares, given the component's file
/// stem (`table`, `aspect-ratio`) so the named-constant convention can be
/// read when the file exports no classes function.
pub fn read_class_parts_for(
    component_stem: &str,
    source: &str,
    defaults: &BTreeMap<String, String>,
) -> Option<ClassParts> {
    read_class_parts(source, defaults).or_else(|| read_named_part_constants(component_stem, source))
}

/// Parts from the `<component><Part>Classes` naming convention, for the files
/// that export their parts as named constants rather than through a classes
/// function: `tableRootClasses`, `tableRowClasses`, `tableCellClasses`.
///
/// The part name is IN the identifier -- this reads it, it does not guess.
/// Only constants carrying the component's own prefix are taken, so an
/// unrelated export cannot become a part.
fn read_named_part_constants(component_stem: &str, source: &str) -> Option<ClassParts> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::ts()).parse();
    if ret.panicked {
        return None;
    }

    let prefix = lower_camel(component_stem);
    let mut locals: BTreeMap<String, Local> = BTreeMap::new();
    let mut parts = ClassParts::default();

    for stmt in &ret.program.body {
        let Statement::ExportNamedDeclaration(export) = stmt else {
            continue;
        };
        let Some(Declaration::VariableDeclaration(decl)) = &export.declaration else {
            continue;
        };
        for declarator in &decl.declarations {
            let (Some(name), Some(init)) = (
                declarator.id.get_identifier_name(),
                declarator.init.as_ref(),
            ) else {
                continue;
            };
            if let Some(local) = local_value(init, &locals) {
                locals.insert(name.to_string(), local);
            }
            let Some(part) = part_name(&prefix, &name) else {
                continue;
            };
            if let Some(text) = text_of(init, &locals, &BTreeMap::new()) {
                parts.parts.insert(part, normalize(&text));
            }
        }
    }

    (!parts.parts.is_empty()).then_some(parts)
}

/// `tableRootClasses` with prefix `table` -> `root`. Returns `None` for a
/// name that is not this component's part constant.
fn part_name(prefix: &str, name: &str) -> Option<String> {
    let rest = name.strip_prefix(prefix)?.strip_suffix("Classes")?;
    if rest.is_empty() {
        // `tableClasses` names the component, not a part.
        return None;
    }
    let mut chars = rest.chars();
    let first = chars.next()?.to_lowercase().to_string();
    Some(format!("{first}{}", chars.as_str()))
}

/// `aspect-ratio` -> `aspectRatio`, matching how the constants are named.
fn lower_camel(stem: &str) -> String {
    let mut out = String::new();
    for (index, segment) in stem.split('-').enumerate() {
        if index == 0 {
            out.push_str(segment);
            continue;
        }
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.push_str(&first.to_uppercase().to_string());
            out.push_str(chars.as_str());
        }
    }
    out
}

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

    let (returned, body_locals) = classes_function_return(&ret.program.body, &locals)?;
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
    module_locals: &BTreeMap<String, Local>,
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
        // can build on an earlier one -- and against the module's constants,
        // which is where the pieces they compose actually live
        // (`const root = [progressContainerClasses, ...].join(' ')`).
        let mut body_locals: BTreeMap<String, Local> = BTreeMap::new();
        for (name, local) in module_locals {
            body_locals.insert(name.clone(), local.clone());
        }
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
        | Expression::LogicalExpression(_)
        | Expression::ConditionalExpression(_)
        | Expression::CallExpression(_) => text_of(expr, locals, &BTreeMap::new()).map(Local::Text),

        // `const variant = variantClasses[config.variant ?? 'default']` binds
        // a RECORD, not a string -- `variant.border` reads a key out of it a
        // line later. Resolving these to text here would lose the nesting.
        Expression::ComputedMemberExpression(_) | Expression::StaticMemberExpression(_) => {
            local_of(expr, locals, &BTreeMap::new())
        }
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
                if let Some(value) = local_value(&property.value, locals) {
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

        // `variant.border` — a named key of a local record, where `variant`
        // may itself have been picked out of another record.
        Expression::StaticMemberExpression(_) => match local_of(expr, locals, defaults)? {
            Local::Text(text) => Some(text),
            Local::Record(_) => None,
        },

        // `a || b` / `a ?? b` — the left side when it carries classes, else
        // the right. This is how a component spells "the configured override,
        // otherwise the default", and a preview supplies no config.
        Expression::LogicalExpression(logical) => match text_of(&logical.left, locals, defaults) {
            Some(text) if !text.trim().is_empty() => Some(text),
            _ => text_of(&logical.right, locals, defaults),
        },

        // `vertical ? trackVertical : trackHorizontal` — take the ALTERNATE.
        // The test reads a config value a preview does not set, so the
        // alternate is the branch that actually runs.
        Expression::ConditionalExpression(conditional) => {
            text_of(&conditional.alternate, locals, defaults)
                .or_else(|| text_of(&conditional.consequent, locals, defaults))
        }

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
            match local_of(&member.object, locals, defaults)? {
                Local::Record(entries) => {
                    let key = key_text(&member.expression, defaults);
                    let picked = match key {
                        Some(key) => entries
                            .iter()
                            .find(|(k, _)| *k == key)
                            .or_else(|| entries.first()),
                        None => entries.first(),
                    };
                    match picked.map(|(_, v)| v) {
                        Some(Local::Text(text)) => Some(text.clone()),
                        _ => None,
                    }
                }
                Local::Text(_) => None,
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

/// Resolve an expression to a local value, so a record can be indexed twice
/// (`variantClasses[config.variant ?? 'default'].border`).
fn local_of(
    expr: &Expression<'_>,
    locals: &BTreeMap<String, Local>,
    defaults: &BTreeMap<String, String>,
) -> Option<Local> {
    match expr {
        Expression::Identifier(ident) => locals.get(ident.name.as_str()).cloned(),
        Expression::ComputedMemberExpression(member) => {
            let Local::Record(entries) = local_of(&member.object, locals, defaults)? else {
                return None;
            };
            let key = key_text(&member.expression, defaults);
            match key {
                Some(key) => entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .or_else(|| entries.first()),
                None => entries.first(),
            }
            .map(|(_, v)| v.clone())
        }
        Expression::StaticMemberExpression(member) => {
            let Local::Record(entries) = local_of(&member.object, locals, defaults)? else {
                return None;
            };
            entries
                .iter()
                .find(|(k, _)| k == member.property.name.as_str())
                .map(|(_, v)| v.clone())
        }
        _ => text_of(expr, locals, defaults).map(Local::Text),
    }
}

/// The key an index expression selects: a literal, a declared default, or
/// the right-hand side of `config.x ?? 'default'`.
fn key_text(expr: &Expression<'_>, defaults: &BTreeMap<String, String>) -> Option<String> {
    match expr {
        Expression::StringLiteral(literal) => Some(literal.value.to_string()),
        Expression::Identifier(ident) => defaults.get(ident.name.as_str()).cloned(),
        Expression::LogicalExpression(logical) => {
            key_text(&logical.right, defaults).or_else(|| key_text(&logical.left, defaults))
        }
        Expression::StaticMemberExpression(member) => {
            defaults.get(member.property.name.as_str()).cloned()
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
