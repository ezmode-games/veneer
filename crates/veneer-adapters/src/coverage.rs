//! Coverage tracking (FR-VEN-009): which discovered components are
//! documented versus not yet -- honest numbers, never a blank or a 404.
//!
//! The denominator is the discovered set (FR-VEN-017): an item veneer
//! never discovered cannot be reported missing, and an item veneer did
//! discover can never silently vanish from the numbers. An item counts as
//! documented exactly when the render pipeline ([`render_component`])
//! actually produced its preview -- a render failure counts as
//! not-yet-documented, with its failure state, never as documented. No
//! partial success is rounded up to "covered". Discovery's
//! [`DiscoveredItem::generated`] flag is deliberately not a gate here:
//! composite manifests are discovered with `generated: false` yet render
//! through the manifest path, so gating on the flag would mis-bucket
//! every composite and report a false reason.
//!
//! Every uncovered item gets an explicit "not yet documented" placeholder
//! artifact ([`not_yet_documented_placeholder`]): a real MDX page naming
//! the item and the honest reason, instead of a blank page or an error.
//!
//! Out of scope here, by requirement: closing the coverage gap itself
//! (FR-VEN-003 per component) and staleness marking (FR-VEN-014).

use std::path::Path;

use crate::intelligence::{render_component, RenderedComponent};
use crate::rafters_source::IntelligenceSource;
use crate::registry::{ComponentRegistry, DiscoveredItem, RegistryError};

/// Coverage of a discovered set: documented versus not-yet-documented,
/// with the discovered set as the denominator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Names of discovered items whose preview rendered.
    pub documented: Vec<String>,
    /// Names of discovered items not yet documented (nothing generatable,
    /// or generation/render failed).
    pub not_yet_documented: Vec<String>,
    /// Size of the discovered set: always
    /// `documented.len() + not_yet_documented.len()`.
    pub total: usize,
}

impl CoverageReport {
    /// Fold assessed items into the report. Every assessed item lands in
    /// exactly one bucket, so the totals cannot drift from the item states.
    pub fn from_assessed(assessed: &[AssessedItem]) -> Self {
        let mut documented: Vec<String> = Vec::new();
        let mut not_yet_documented: Vec<String> = Vec::new();
        for entry in assessed {
            match &entry.state {
                CoverageState::Documented => documented.push(entry.item.name.clone()),
                CoverageState::NotYetDocumented { .. } => {
                    not_yet_documented.push(entry.item.name.clone())
                }
            }
        }
        Self {
            total: documented.len() + not_yet_documented.len(),
            documented,
            not_yet_documented,
        }
    }
}

/// The coverage state of one discovered item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageState {
    /// The render pipeline produced the item's preview.
    Documented,
    /// Not documented yet, with the honest reason: what discovery or the
    /// render pipeline reported for this item.
    NotYetDocumented { reason: String },
}

/// One discovered item together with its assessed coverage state.
#[derive(Debug, Clone)]
pub struct AssessedItem {
    pub item: DiscoveredItem,
    pub state: CoverageState,
    /// The rendered preview + intelligence, present exactly when the item
    /// is [`CoverageState::Documented`]. Carried here so the render the
    /// assessment already performed can drive page generation without a
    /// second render; `None` for a not-yet-documented item.
    pub rendered: Option<RenderedComponent>,
}

/// Assess the coverage of every discovered item, in the discovered order.
///
/// An item is [`CoverageState::Documented`] exactly when
/// [`render_component`] produced its preview. Everything else is
/// [`CoverageState::NotYetDocumented`] with the failure state as the
/// reason -- a failed generation is never counted as documented. The
/// render attempt is unconditional: [`DiscoveredItem::generated`] is
/// `false` for every composite manifest even though the manifest path
/// renders it, so short-circuiting on the flag would mis-bucket
/// composites with a false reason.
///
/// `full_css` is the project's compiled preview sheet (see
/// `read_rafters_stylesheet`); a project with no sheet, or an empty one,
/// fails every render (FR-VEN-018) and therefore counts every item as not
/// yet documented, with the refusal as its honest reason.
pub fn assess_coverage(
    items: Vec<DiscoveredItem>,
    source: &IntelligenceSource,
    full_css: &str,
) -> Vec<AssessedItem> {
    items
        .into_iter()
        .map(|item| {
            let (state, rendered) = assess_item(&item, source, full_css);
            AssessedItem {
                item,
                state,
                rendered,
            }
        })
        .collect()
}

/// Render the item once and derive both its coverage state and, on success,
/// the rendered component the caller can hand to page generation.
fn assess_item(
    item: &DiscoveredItem,
    source: &IntelligenceSource,
    full_css: &str,
) -> (CoverageState, Option<RenderedComponent>) {
    // An item under a declared, unsupported `componentTarget` (FR-VEN-033)
    // is never handed to the render pipeline: `render_component` would parse
    // its source with `ReactAdapter`, inferring props from a shape veneer
    // has no adapter for. The observation itself -- naming the declared
    // framework -- is the honest not-yet-documented reason.
    if let Some(declared) = &item.unsupported_framework {
        return (
            CoverageState::NotYetDocumented {
                reason: format!(
                    "componentTarget \"{declared}\" has no adapter; props are not read from a source shape veneer cannot parse"
                ),
            },
            None,
        );
    }
    match render_component(item, source, full_css) {
        Ok(rendered) => (CoverageState::Documented, Some(rendered)),
        Err(error) => (
            CoverageState::NotYetDocumented {
                reason: error.to_string(),
            },
            None,
        ),
    }
}

impl ComponentRegistry {
    /// Query coverage for a project: discover the set, assess every item,
    /// and fold the states into a [`CoverageReport`].
    ///
    /// This is an associated function taking the project root, not
    /// `&self`: the registry's scanned cache silently drops items that
    /// fail extraction, so measuring coverage against it would violate
    /// the discovered-set denominator. Like
    /// [`ComponentRegistry::discover`], coverage works from the project
    /// source directly.
    pub fn coverage(
        project_root: &Path,
        source: &IntelligenceSource,
        full_css: &str,
    ) -> Result<CoverageReport, RegistryError> {
        let items = Self::discover(project_root, source)?;
        Ok(CoverageReport::from_assessed(&assess_coverage(
            items, source, full_css,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rafters_source::{read_rafters_namespace, read_rafters_stylesheet};
    use crate::registry::DiscoveredKind;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coverage/partial")
    }

    fn fixture_stylesheet() -> String {
        read_rafters_stylesheet(&fixture_root())
            .expect("fixture stylesheet must read")
            .expect("fixture project declares a compiled stylesheet")
    }

    fn assessed_fixture() -> Vec<AssessedItem> {
        let root = fixture_root();
        let source = read_rafters_namespace(&root).expect("fixture namespace must read");
        let items = ComponentRegistry::discover(&root, &source).expect("fixture must discover");
        assess_coverage(items, &source, &fixture_stylesheet())
    }

    // AC: coverage numbers are exact against a fixture with known partial
    // coverage. The fixture discovers exactly four items: Button
    // (renderable source), hero-banner (renderable composite manifest),
    // Broken (unparseable source), ghost-widget (installed in the rafters
    // config with no source file).
    #[test]
    fn coverage_numbers_are_exact_against_partial_fixture() {
        let report = CoverageReport::from_assessed(&assessed_fixture());
        assert_eq!(report.total, 4, "denominator is the full discovered set");
        assert_eq!(report.documented, ["Button", "hero-banner"]);
        assert_eq!(report.not_yet_documented, ["Broken", "ghost-widget"]);
    }

    // Regression: discovery marks every composite manifest `generated:
    // false`, yet the manifest path renders it. Coverage must attempt the
    // render instead of short-circuiting on the flag -- otherwise no
    // composite could ever count as documented and the placeholder would
    // carry a false reason.
    #[test]
    fn composite_manifest_counts_as_documented_despite_generated_false() {
        let assessed = assessed_fixture();
        let composite = assessed
            .iter()
            .find(|entry| entry.item.name == "hero-banner")
            .expect("the composite manifest must be assessed");
        assert_eq!(composite.item.kind, DiscoveredKind::Composite);
        assert!(
            !composite.item.generated,
            "discovery marks manifests non-generated; coverage must not trust that"
        );
        assert_eq!(composite.state, CoverageState::Documented);
    }

    // AC: coverage status is queryable, with the discovered set as the
    // denominator.
    #[test]
    fn coverage_is_queryable_from_the_registry() {
        let root = fixture_root();
        let source = read_rafters_namespace(&root).expect("fixture namespace must read");
        let report = ComponentRegistry::coverage(&root, &source, &fixture_stylesheet())
            .expect("coverage must compute");
        assert_eq!(report.total, 4);
        assert_eq!(
            report.total,
            report.documented.len() + report.not_yet_documented.len(),
            "every discovered item lands in exactly one bucket"
        );
    }

    // AC: a component that failed generation counts as not-yet-documented
    // (with its failure state), never as documented.
    #[test]
    fn failed_generation_is_not_yet_documented_with_its_failure_state() {
        let assessed = assessed_fixture();
        let broken = assessed
            .iter()
            .find(|entry| entry.item.name == "Broken")
            .expect("Broken must be assessed");
        match &broken.state {
            CoverageState::NotYetDocumented { reason } => {
                assert!(!reason.is_empty(), "the failure state must be carried");
                assert!(
                    reason.contains("broken.tsx"),
                    "reason names the source: {reason}"
                );
            }
            CoverageState::Documented => panic!("a failed generation must never be documented"),
        }
    }

    // AC: a render failure on a discovery-generatable item is still
    // not-yet-documented -- generation success at discovery time is not
    // rounded up to covered.
    #[test]
    fn render_failure_is_never_documented() {
        let item = DiscoveredItem {
            name: "Vanished".to_string(),
            kind: DiscoveredKind::Component,
            source_path: PathBuf::from("/nonexistent/vanished.tsx"),
            generated: true,
            unsupported_framework: None,
        };
        let assessed = assess_coverage(vec![item], &IntelligenceSource::NoSource, "");
        assert_eq!(assessed.len(), 1);
        match &assessed[0].state {
            CoverageState::NotYetDocumented { reason } => {
                assert!(
                    reason.contains("Vanished"),
                    "failure names the item: {reason}"
                );
            }
            CoverageState::Documented => panic!("a render failure must never be documented"),
        }
    }

    // AC (FR-VEN-033): an item under a declared, unsupported componentTarget
    // is an observation, never an inference -- it never reaches
    // render_component (which would parse it with ReactAdapter), and the
    // not-yet-documented reason names the declared framework.
    #[test]
    fn unsupported_framework_is_an_observation_never_an_inference() {
        let item = DiscoveredItem {
            name: "SolidButton".to_string(),
            kind: DiscoveredKind::Component,
            // A path that would fail if `render_component` ever touched it,
            // proving the short-circuit happens before any read.
            source_path: PathBuf::from("/nonexistent/solid-button.tsx"),
            generated: false,
            unsupported_framework: Some("solid".to_string()),
        };
        let assessed = assess_coverage(vec![item], &IntelligenceSource::NoSource, "");
        assert_eq!(assessed.len(), 1);
        assert!(assessed[0].rendered.is_none());
        match &assessed[0].state {
            CoverageState::NotYetDocumented { reason } => {
                assert!(
                    reason.contains("\"solid\""),
                    "reason names the declared framework: {reason}"
                );
                assert!(
                    reason.contains("no adapter"),
                    "reason is an observation, not a crash: {reason}"
                );
            }
            CoverageState::Documented => {
                panic!("an unsupported framework must never count as documented")
            }
        }
    }

    // AC: an undocumented component surfaces an explicit "not yet
    // documented" artifact -- a real page, not a blank.
}
