//! Resolve which DOM element a component actually renders, from its React
//! source (issue #109).
//!
//! Before this, every generated preview emitted a `<button>` — the generator
//! was written for Button and every component inherited its element. A
//! `<button>` where a `<nav>` or `<label>` belongs is an accessibility defect
//! that renders plausibly, which is the failure mode worth refusing over.
//!
//! Two sources, both read from the source rather than inferred from the name:
//!
//! 1. The **JSX root** the component returns, when it resolves to an intrinsic
//!    tag. This is ground truth — it is what the browser gets.
//! 2. The **`forwardRef` generic**, when the JSX root is a context provider or
//!    a `createElement` call. `React.forwardRef<HTMLSpanElement, AvatarProps>`
//!    is the author declaring the element type, and it answers cases the JSX
//!    root cannot.
//!
//! When neither resolves, this refuses. It never falls back to `div`:
//! substituting one wrong element for another would close #109 without fixing
//! it.
//!
//! Declaration shapes handled, all four found by enumerating the 55 installed
//! components rather than by guessing:
//! `export function X`, `export const X = () => …`,
//! `export const X = React.forwardRef<E, P>(render)`,
//! `export const X = Object.assign(Root, { … })` (compound components — `Table`
//! is one, and it was invisible to the first two passes), and bare-identifier
//! initializers that have to be followed to a local declaration.

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Expression, JSXElementName, Statement, VariableDeclarator};
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Which source answered. Carried so a page can say how it knows, and so a
/// disagreement between the two is observable rather than silently resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementSource {
    /// The intrinsic tag the component's JSX returns.
    JsxRoot,
    /// The element type declared in a `forwardRef` generic.
    ForwardRefGeneric,
}

/// The element a component renders at its root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedElement {
    /// The HTML tag name, for example `nav`.
    pub tag: String,
    pub source: ElementSource,
}

/// Failure to resolve a component's root element. Always names the component
/// and what was looked for, so a refusal is actionable rather than a shrug.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElementError {
    #[error(
        "cannot resolve the root element for '{component}': no exported \
         declaration of that name in the component source"
    )]
    NoDeclaration { component: String },

    #[error(
        "cannot resolve the root element for '{component}': its JSX root is \
         {root}, and it declares no forwardRef element generic to fall back on"
    )]
    Unresolved { component: String, root: String },

    #[error(
        "cannot resolve the root element for '{component}': its JSX root is \
         {root} and its declared generic '{generic}' names no single tag"
    )]
    AmbiguousGeneric {
        component: String,
        root: String,
        generic: String,
    },

    #[error("cannot parse the component source for '{component}'")]
    ParseFailed { component: String },
}

/// Element-type generics that name exactly one tag. Deliberately partial:
/// `HTMLElement` (polymorphic `as`), `HTMLHeadingElement` (h1–h6),
/// `HTMLTableSectionElement` (thead/tbody/tfoot) and `HTMLTableCellElement`
/// (td/th) name a family, not a tag, and are refused rather than guessed.
const GENERIC_TO_TAG: &[(&str, &str)] = &[
    ("HTMLAnchorElement", "a"),
    ("HTMLButtonElement", "button"),
    ("HTMLDivElement", "div"),
    ("HTMLFormElement", "form"),
    ("HTMLHRElement", "hr"),
    ("HTMLImageElement", "img"),
    ("HTMLInputElement", "input"),
    ("HTMLLIElement", "li"),
    ("HTMLLabelElement", "label"),
    ("HTMLOListElement", "ol"),
    ("HTMLParagraphElement", "p"),
    ("HTMLPreElement", "pre"),
    ("HTMLProgressElement", "progress"),
    ("HTMLSelectElement", "select"),
    ("HTMLSpanElement", "span"),
    ("HTMLTableElement", "table"),
    ("HTMLTextAreaElement", "textarea"),
    ("HTMLUListElement", "ul"),
];

fn tag_for_generic(generic: &str) -> Option<&'static str> {
    GENERIC_TO_TAG
        .iter()
        .find(|(name, _)| *name == generic)
        .map(|(_, tag)| *tag)
}

/// The `.tsx` that declares the component whose class strings live at
/// `classes_path`. rafters installs the pair side by side
/// (`button.classes.ts` / `button.tsx`), so this is a sibling lookup, not a
/// search — if the convention ever changes this returns `None` and the caller
/// refuses rather than guessing at a different file.
pub fn component_source_for(classes_path: &Path) -> Option<PathBuf> {
    let file_name = classes_path.file_name()?.to_str()?;
    // An item discovered directly from a `.tsx`/`.jsx` already IS the
    // component source; there is no sibling to look for.
    if matches!(
        classes_path.extension().and_then(|ext| ext.to_str()),
        Some("tsx") | Some("jsx")
    ) {
        return Some(classes_path.to_path_buf());
    }
    let stem = file_name.strip_suffix(".classes.ts")?;
    Some(classes_path.with_file_name(format!("{stem}.tsx")))
}

/// Resolve the root element `component` renders, from the text of the `.tsx`
/// that declares it.
pub fn resolve_root_element(
    component: &str,
    source: &str,
) -> Result<ResolvedElement, ElementError> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
    if ret.panicked {
        return Err(ElementError::ParseFailed {
            component: component.to_string(),
        });
    }

    // Every local `const x = …`, so a bare-identifier initializer and a
    // compound component's root can be followed to where they are defined.
    let mut locals: HashMap<&str, &Expression> = HashMap::new();
    for stmt in &ret.program.body {
        match stmt {
            Statement::VariableDeclaration(decl) => collect_locals(&decl.declarations, &mut locals),
            Statement::ExportNamedDeclaration(export) => {
                if let Some(Declaration::VariableDeclaration(decl)) = &export.declaration {
                    collect_locals(&decl.declarations, &mut locals);
                }
            }
            _ => {}
        }
    }

    let Some(target) = exported_binding(&ret.program.body, component, &locals) else {
        return Err(ElementError::NoDeclaration {
            component: component.to_string(),
        });
    };

    let (root, generic) = target;

    // 1. The JSX root, when it is an intrinsic tag.
    if let Some(RootKind::Intrinsic(tag)) = &root {
        return Ok(ResolvedElement {
            tag: tag.clone(),
            source: ElementSource::JsxRoot,
        });
    }

    let root_desc = match &root {
        Some(RootKind::Intrinsic(tag)) => format!("<{tag}>"),
        Some(RootKind::Component(name)) => format!("the component <{name}>"),
        Some(RootKind::Fragment) => "a fragment".to_string(),
        Some(RootKind::Other(what)) => what.clone(),
        None => "not a single JSX expression".to_string(),
    };

    // 2. The declared forwardRef generic.
    match generic {
        Some(g) => match tag_for_generic(&g) {
            Some(tag) => Ok(ResolvedElement {
                tag: tag.to_string(),
                source: ElementSource::ForwardRefGeneric,
            }),
            None => Err(ElementError::AmbiguousGeneric {
                component: component.to_string(),
                root: root_desc,
                generic: g,
            }),
        },
        None => Err(ElementError::Unresolved {
            component: component.to_string(),
            root: root_desc,
        }),
    }
}

fn collect_locals<'a>(
    declarations: &'a oxc_allocator::Vec<'a, VariableDeclarator<'a>>,
    locals: &mut HashMap<&'a str, &'a Expression<'a>>,
) {
    for declarator in declarations {
        if let (Some(name), Some(init)) = (
            declarator.id.get_identifier_name(),
            declarator.init.as_ref(),
        ) {
            locals.insert(name.as_str(), init);
        }
    }
}

/// The function body and declared element generic for `component`, following
/// `forwardRef`/`memo` wrappers, `Object.assign` compounds, and identifier
/// aliases to whatever actually renders.
fn exported_binding<'a>(
    body: &'a oxc_allocator::Vec<'a, Statement<'a>>,
    component: &str,
    locals: &HashMap<&'a str, &'a Expression<'a>>,
) -> Option<(Option<RootKind>, Option<String>)> {
    for stmt in body {
        let Statement::ExportNamedDeclaration(export) = stmt else {
            continue;
        };
        match &export.declaration {
            // `export function X() { … }` — the JSX is in the declaration's
            // own body, with no wrapper and so no generic to fall back on.
            Some(Declaration::FunctionDeclaration(func))
                if func.id.as_ref().is_some_and(|id| id.name == component) =>
            {
                let root = func
                    .body
                    .as_ref()
                    .and_then(|body| first_return(&body.statements))
                    .map(classify_root);
                return Some((root, None));
            }
            Some(Declaration::VariableDeclaration(decl)) => {
                for declarator in &decl.declarations {
                    if declarator.id.get_identifier_name().as_deref() != Some(component) {
                        continue;
                    }
                    let init = declarator.init.as_ref()?;
                    let (body_expr, generic) = unwrap_component(init, locals, 0);
                    return Some((body_expr.and_then(returned_jsx_root), generic));
                }
            }
            _ => {}
        }
    }
    None
}

/// Peel wrappers until an actual render function is reached, capturing the
/// first type argument of a `forwardRef`-shaped call on the way through.
fn unwrap_component<'a>(
    expr: &'a Expression<'a>,
    locals: &HashMap<&'a str, &'a Expression<'a>>,
    depth: u8,
) -> (Option<&'a Expression<'a>>, Option<String>) {
    if depth > 6 {
        return (None, None);
    }
    match expr {
        Expression::CallExpression(call) => {
            let callee = call.callee.get_member_expr().map_or_else(
                || {
                    call.callee
                        .get_identifier_reference()
                        .map(|id| id.name.to_string())
                },
                |member| member.static_property_name().map(|name| name.to_string()),
            );
            let callee = callee.unwrap_or_default();

            let generic = call
                .type_arguments
                .as_ref()
                .and_then(|args| args.params.first())
                .map(|param| type_name(param));

            // `Object.assign(Root, { … })` — the compound's root is arg 0.
            if callee == "assign" {
                if let Some(first) = call.arguments.first().and_then(|a| a.as_expression()) {
                    let (body, inner) = unwrap_component(first, locals, depth + 1);
                    return (body, generic.or(inner));
                }
            }
            if callee == "forwardRef" || callee == "memo" {
                if let Some(first) = call.arguments.first().and_then(|a| a.as_expression()) {
                    let (body, inner) = unwrap_component(first, locals, depth + 1);
                    return (body, generic.or(inner));
                }
            }
            (None, generic)
        }
        Expression::Identifier(ident) => match locals.get(ident.name.as_str()) {
            Some(target) => unwrap_component(target, locals, depth + 1),
            None => (None, None),
        },
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
            (Some(expr), None)
        }
        _ => (None, None),
    }
}

fn type_name(param: &oxc_ast::ast::TSType<'_>) -> String {
    match param {
        oxc_ast::ast::TSType::TSTypeReference(reference) => reference
            .type_name
            .get_identifier_reference()
            .name
            .to_string(),
        _ => String::new(),
    }
}

/// What a component's returned JSX resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootKind {
    Intrinsic(String),
    Component(String),
    Fragment,
    Other(String),
}

fn returned_jsx_root(expr: &Expression<'_>) -> Option<RootKind> {
    let returned: Option<&Expression<'_>> = match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            if arrow.expression {
                arrow.body.statements.first().and_then(|stmt| match stmt {
                    Statement::ExpressionStatement(inner) => Some(&inner.expression),
                    _ => None,
                })
            } else {
                first_return(&arrow.body.statements)
            }
        }
        Expression::FunctionExpression(func) => func
            .body
            .as_ref()
            .and_then(|body| first_return(&body.statements)),
        _ => None,
    };

    returned.map(classify_root)
}

/// The first `return` in a body, not descending into nested functions — a
/// callback's return is not the component's.
fn first_return<'a>(statements: &'a [Statement<'a>]) -> Option<&'a Expression<'a>> {
    for stmt in statements {
        match stmt {
            Statement::ReturnStatement(ret) => return ret.argument.as_ref(),
            Statement::IfStatement(if_stmt) => {
                if let Statement::BlockStatement(block) = &if_stmt.consequent {
                    if let Some(found) = first_return(&block.body) {
                        return Some(found);
                    }
                }
            }
            Statement::BlockStatement(block) => {
                if let Some(found) = first_return(&block.body) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn classify_root(expr: &Expression<'_>) -> RootKind {
    match expr {
        Expression::ParenthesizedExpression(inner) => classify_root(&inner.expression),
        Expression::TSAsExpression(inner) => classify_root(&inner.expression),
        Expression::JSXFragment(_) => RootKind::Fragment,
        Expression::JSXElement(element) => match &element.opening_element.name {
            JSXElementName::Identifier(ident) => {
                let name = ident.name.to_string();
                // JSX's own rule: a lowercase tag is an intrinsic element, a
                // capitalized one is a component reference.
                if name.starts_with(|c: char| c.is_lowercase()) {
                    RootKind::Intrinsic(name)
                } else {
                    RootKind::Component(name)
                }
            }
            JSXElementName::IdentifierReference(ident) => {
                RootKind::Component(ident.name.to_string())
            }
            // Name the whole path (`AccordionContext.Provider`), not just the
            // trailing segment — a refusal that says `<Provider>` makes the
            // reader go find which provider.
            JSXElementName::MemberExpression(member) => RootKind::Component(member.to_string()),
            other => RootKind::Other(format!("{other:?}")),
        },
        Expression::ConditionalExpression(_) => {
            RootKind::Other("a conditional expression".to_string())
        }
        Expression::CallExpression(call) => RootKind::Other(format!(
            "a call to {}",
            call.callee
                .get_member_expr()
                .and_then(|m| m.static_property_name().map(|s| s.to_string()))
                .or_else(|| call
                    .callee
                    .get_identifier_reference()
                    .map(|i| i.name.to_string()))
                .unwrap_or_else(|| "an expression".to_string())
        )),
        _ => RootKind::Other("not a JSX element".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(component: &str, source: &str) -> ResolvedElement {
        resolve_root_element(component, source)
            .unwrap_or_else(|error| panic!("{component} must resolve: {error}"))
    }

    // --- the four declaration shapes, all found by enumerating the real
    // installed components rather than by guessing ---

    #[test]
    fn function_declaration_resolves_from_its_jsx_root() {
        let resolved = tag("Calendar", "export function Calendar() { return <div />; }");
        assert_eq!(resolved.tag, "div");
        assert_eq!(resolved.source, ElementSource::JsxRoot);
    }

    #[test]
    fn forward_ref_resolves_from_its_jsx_root_not_its_generic() {
        // Both sources are available and they agree; the JSX root wins
        // because it is what the browser actually gets.
        let resolved = tag(
            "Label",
            "export const Label = React.forwardRef<HTMLLabelElement, LabelProps>((p, ref) => {
                 return <label ref={ref} />;
             });",
        );
        assert_eq!(resolved.tag, "label");
        assert_eq!(resolved.source, ElementSource::JsxRoot);
    }

    #[test]
    fn forward_ref_generic_answers_when_the_jsx_root_is_a_provider() {
        // Avatar's shape: the root is a context provider, so the JSX gives no
        // element and the declared generic is the only source.
        let resolved = tag(
            "Avatar",
            "export const Avatar = React.forwardRef<HTMLSpanElement, AvatarProps>((p, ref) => {
                 return <AvatarContext.Provider value={v}><span ref={ref} /></AvatarContext.Provider>;
             });",
        );
        assert_eq!(resolved.tag, "span");
        assert_eq!(resolved.source, ElementSource::ForwardRefGeneric);
    }

    #[test]
    fn compound_component_follows_object_assign_to_its_root() {
        // Table's shape. This one was invisible to two earlier passes of the
        // enumeration, which is why it has a test of its own.
        let resolved = tag(
            "Table",
            "const TableRoot = React.forwardRef<HTMLTableElement, TableProps>((p, ref) => {
                 return <div className='wrap'><table ref={ref} /></div>;
             });
             export const Table = Object.assign(TableRoot, { Row: TableRow });",
        );
        assert_eq!(resolved.tag, "div", "the scroll wrapper is the real root");
    }

    #[test]
    fn arrow_component_with_expression_body_resolves() {
        let resolved = tag("Kbd", "export const Kbd = () => <kbd />;");
        assert_eq!(resolved.tag, "kbd");
    }

    // --- refusals: every one names the component, and none invents a tag ---

    #[test]
    fn refuses_when_the_component_is_not_exported() {
        let error = resolve_root_element("Ghost", "const Ghost = () => <div />;")
            .expect_err("an unexported binding is not the component's public shape");
        assert!(matches!(error, ElementError::NoDeclaration { .. }));
        assert!(error.to_string().contains("Ghost"));
    }

    #[test]
    fn refuses_a_provider_root_with_no_generic_rather_than_defaulting() {
        // The failure that matters: this is the shape 17 real components
        // have, and the old generator would have emitted <button> for it.
        let error = resolve_root_element(
            "Accordion",
            "export const Accordion = (props) => {
                 return <AccordionContext.Provider value={v}>{props.children}</AccordionContext.Provider>;
             };",
        )
        .expect_err("a provider root names no element");
        assert!(matches!(error, ElementError::Unresolved { .. }));
        let message = error.to_string();
        assert!(message.contains("Accordion"), "{message}");
        assert!(message.contains("AccordionContext.Provider"), "{message}");
    }

    #[test]
    fn refuses_a_generic_that_names_a_family_rather_than_a_tag() {
        // HTMLElement is what a polymorphic `as` component declares. Mapping
        // it to any single tag would be a guess.
        let error = resolve_root_element(
            "Typography",
            "export const Typography = React.forwardRef<HTMLElement, TypographyProps>((p, ref) => {
                 return React.createElement(p.as ?? 'p', { ref });
             });",
        )
        .expect_err("HTMLElement names a family");
        assert!(matches!(error, ElementError::AmbiguousGeneric { .. }));
        assert!(error.to_string().contains("HTMLElement"));
    }

    #[test]
    fn never_falls_back_to_a_plausible_tag() {
        // The whole point of #109: a wrong element that renders is worse than
        // a refusal, because it looks fine. Nothing here may return Ok.
        for source in [
            "export const X = (p) => <Wrapped />;",
            "export const X = (p) => { return null; };",
            "export const X = 42;",
        ] {
            assert!(
                resolve_root_element("X", source).is_err(),
                "must refuse rather than guess for: {source}"
            );
        }
    }

    // --- locating the component source ---

    #[test]
    fn component_source_is_the_tsx_beside_the_classes_file() {
        let found = component_source_for(Path::new("/p/src/components/ui/table.classes.ts"))
            .expect("a sibling .tsx is the convention");
        assert_eq!(found, Path::new("/p/src/components/ui/table.tsx"));
    }

    #[test]
    fn an_item_discovered_from_a_tsx_is_its_own_component_source() {
        let found = component_source_for(Path::new("/p/components/button.tsx"))
            .expect("a .tsx is already the component source");
        assert_eq!(found, Path::new("/p/components/button.tsx"));
    }

    #[test]
    fn an_unrecognized_source_name_yields_no_guess() {
        assert!(component_source_for(Path::new("/p/components/button.styles.css")).is_none());
    }
}
