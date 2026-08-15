//! Extract documentation from a target project.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use veneer_adapters::{
    assess_coverage, build_substrate, component_page_file_name, default_matrix_path, detect_mode,
    generate_component_page, preview_styles_module, read_matrix, read_rafters_namespace,
    read_rafters_stylesheet, read_veneer_config, to_jsonl, ComponentLine, ComponentRegistry,
    CoverageReport, CoverageState, VeneerConfig, PREVIEW_STYLES_MODULE,
};

#[derive(Args)]
pub struct ExtractArgs {
    /// Path to the target rafters project. Writes the component substrate to
    /// its .rafters/veneer/ directory.
    #[arg(short, long)]
    project: PathBuf,
}

pub async fn run(args: ExtractArgs) -> Result<()> {
    if !args.project.exists() {
        anyhow::bail!("Project path does not exist: {}", args.project.display());
    }

    tracing::info!("Extracting docs from {}", args.project.display());

    // Name which mode veneer runs in (FR-VEN-033): detected from what the
    // project root contains -- a component matrix + `.behavior.ts` files
    // (default, the rafters monorepo) versus their absence (sidecar, an
    // installed consumer project) -- never a flag, never guessed.
    let mode = detect_mode(&args.project);
    tracing::info!("veneer mode: {mode:?}");

    // The project's optional input declarations (FR-VEN-021): absence is
    // defaults; a malformed file is a typed refusal naming file and field,
    // never a silent fallback.
    let veneer_config: VeneerConfig =
        read_veneer_config(&args.project).map_err(|error| anyhow::anyhow!(error))?;
    if let Some(reporters) = &veneer_config.reporters {
        tracing::info!(
            "veneer.json declares {} test and {} accessibility reporter path(s) (read as input facts; consumed when FR-VEN-028/029 land)",
            reporters.tests.len(),
            reporters.accessibility.len()
        );
    }

    // The .rafters/veneer/ substrate -- the canonical docs.jsonl
    // (FR-VEN-022) and the veneer index.jsonl (FR-VEN-031), both derived from
    // one assessment pass against the discovered set (FR-VEN-009 folds in as
    // the index coverage dimension).
    let substrate = run_substrate_phase(&args.project)?;
    tracing::info!(
        "Wrote {} docs lines and {} index lines to {}",
        substrate.docs_lines,
        substrate.index_lines,
        substrate.dir.display()
    );

    // Pages, from the same model the substrate serialized (FR-VEN-022):
    // a component page with the DO/NEVER constraints beside the preview
    // (FR-VEN-004), and an explicit not-yet-documented page for every item
    // that did not render (FR-VEN-009) -- never a blank where a component
    // should be.
    let pages_dir = args
        .project
        .join(veneer_config.output_dir())
        .join("components");
    let pages = write_pages(&substrate.assessed, &pages_dir, &substrate.preview_sheet)?;
    tracing::info!("Wrote {} pages to {}", pages, pages_dir.display());

    print_coverage_summary(&substrate.report);

    Ok(())
}

/// Write one page per discovered item: the full MDX page (+ its preview
/// sidecar) for a documented item, an explicit not-yet-documented page for
/// the rest. Deterministic content; atomic writes.
///
/// The preview sheet is written ONCE, as the shared module every preview
/// sidecar imports (issue #105) -- never inlined per component. It is written
/// only when at least one item documented, so a run that produced no preview
/// leaves no orphan sheet behind claiming otherwise.
fn write_pages(
    assessed: &[veneer_adapters::AssessedItem],
    dir: &Path,
    preview_sheet: &str,
) -> Result<usize> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let mut written = 0;

    let any_documented = assessed
        .iter()
        .any(|entry| matches!(entry.state, CoverageState::Documented));
    if any_documented {
        write_atomic(
            &dir.join(PREVIEW_STYLES_MODULE),
            &preview_styles_module(preview_sheet),
        )?;
        written += 1;
    }

    for entry in assessed {
        let file = dir.join(component_page_file_name(&entry.item.name));
        match (&entry.state, &entry.rendered) {
            (CoverageState::Documented, Some(rendered)) => {
                let page = generate_component_page(&entry.item, rendered)
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                write_atomic(&file, &page.page)?;
                write_atomic(&dir.join(&page.sidecar_name), &page.sidecar)?;
                written += 2;
            }
            (CoverageState::NotYetDocumented { reason }, _) => {
                // FR-VEN-009: an explicit state, never a blank page. Facts
                // only -- the reason veneer observed, nothing synthesized.
                let page = format!(
                    "---
title: {name}
status: not-yet-documented
---

Not yet documented.

veneer could not render this item: {reason}
",
                    name = entry.item.name,
                );
                write_atomic(&file, &page)?;
                written += 1;
            }
            _ => {}
        }
    }
    Ok(written)
}

/// Outcome of the substrate phase: the coverage report plus what was written
/// to `.rafters/veneer/`. `pub(crate)` so `commands::watch` -- which re-runs
/// this same phase on a filesystem change instead of forking its own copy of
/// the derivation -- can read the counts it logs.
pub(crate) struct SubstrateOutcome {
    pub(crate) report: CoverageReport,
    /// The assessed set, carried so page generation reuses the same render
    /// pass (never a second render).
    pub(crate) assessed: Vec<veneer_adapters::AssessedItem>,
    /// The `.rafters/veneer/` directory the jsonl were written to.
    pub(crate) dir: PathBuf,
    pub(crate) docs_lines: usize,
    pub(crate) index_lines: usize,
    /// The preview sheet text this pass assessed against, carried so page
    /// writing emits the same bytes the assessment was made against rather
    /// than re-reading a file that could have moved underneath it.
    pub(crate) preview_sheet: String,
}

/// Assess the discovered set and write the `.rafters/veneer/` substrate:
/// `docs.jsonl` (FR-VEN-022) and `index.jsonl` (FR-VEN-031), both derived
/// from the same assessment pass. The substrate is veneer's only write under
/// `.rafters/`; everything else there is read-only input (FR-VEN-021).
///
/// `pub(crate)`: this is the ONE derivation entry point. Both the batch
/// `extract` command (`run`, above) and `commands::watch`'s re-derive-on-change
/// loop call this same function -- watch never reimplements or hardcodes
/// what it emits (issue #94 / FR-VEN-026).
pub(crate) fn run_substrate_phase(project: &Path) -> Result<SubstrateOutcome> {
    let source = read_rafters_namespace(project)
        .with_context(|| format!("failed to read the rafters source in {}", project.display()))?;
    let items = ComponentRegistry::discover(project, &source)
        .with_context(|| format!("failed to discover components in {}", project.display()))?;
    // A project without a preview sheet assesses against empty CSS: every
    // preview is refused (FR-VEN-018) and lands in not-yet-documented with
    // that refusal as its reason, naming the path it looked for.
    let full_css = read_rafters_stylesheet(project)
        .with_context(|| {
            format!(
                "failed to read the compiled stylesheet in {}",
                project.display()
            )
        })?
        .unwrap_or_default();
    let assessed = assess_coverage(items, &source, &full_css);

    let matrix = load_component_matrix(project)?;
    let substrate = build_substrate(&assessed, &matrix, project, &source);
    let docs = substrate
        .docs_jsonl()
        .context("failed to serialize docs.jsonl")?;
    let index = to_jsonl(&substrate.index).context("failed to serialize index.jsonl")?;

    let dir = project.join(".rafters").join("veneer");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    write_atomic(&dir.join("docs.jsonl"), &docs)?;
    write_atomic(&dir.join("index.jsonl"), &index)?;

    Ok(SubstrateOutcome {
        report: CoverageReport::from_assessed(&assessed),
        assessed,
        dir,
        docs_lines: substrate.docs_line_count(),
        index_lines: substrate.index.len(),
        preview_sheet: full_css,
    })
}

/// Write a file by writing a sibling temp file and renaming it into place, so
/// a reader never observes a torn or partial file (FR-VEN-031).
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("substrate path has no file name")?;
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    fs::write(&tmp, content).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Load the rafters component matrix (`components.jsonl`) for the project,
/// keyed by component name. The matrix is the canonical intelligence source
/// (interface contract): it supplies the doc line's principle-first lead and
/// intelligence dimensions. A project with no matrix -- a consumer sidecar,
/// for now -- yields an empty map, and the doc lines carry honestly-absent
/// intelligence with the compiled source as the fallback. A malformed matrix
/// fails loudly rather than silently emitting thin docs.
///
/// TODO(phase-1/S1): the matrix path is a provisional convention. Confirm it
/// (or a path declared in the rafters `veneer` config block) with rafters' S1
/// config work before consumer sidecar mode ships.
fn load_component_matrix(project: &Path) -> Result<BTreeMap<String, ComponentLine>> {
    let path = default_matrix_path(project);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let lines = read_matrix(&path)
        .with_context(|| format!("failed to read the component matrix {}", path.display()))?;
    Ok(lines
        .into_iter()
        .map(|line| (line.name.clone(), line))
        .collect())
}

/// The queryable CLI coverage summary: exact counts against the
/// discovered set, then each not-yet-documented item by name.
fn print_coverage_summary(report: &CoverageReport) {
    println!(
        "Coverage: {} of {} discovered components documented, {} not yet documented",
        report.documented.len(),
        report.total,
        report.not_yet_documented.len()
    );
    for name in &report.not_yet_documented {
        println!("  not yet documented: {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed fixture with known partial coverage (shared with the
    /// veneer-adapters coverage tests, so the two layers cannot drift):
    /// Button renders, the hero-banner composite manifest renders, Broken
    /// does not parse, and ghost-widget is installed with no source file.
    fn partial_coverage_project() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../veneer-adapters/tests/fixtures/coverage/partial")
    }

    /// Copy the read-only fixture into a temp dir so the phase can write its
    /// `.rafters/veneer/` output without mutating the committed fixture.
    fn temp_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        copy_dir_all(&partial_coverage_project(), dir.path()).expect("copy fixture");
        dir
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let path = entry.path();
            let target = dst.join(entry.file_name());
            if path.is_dir() {
                copy_dir_all(&path, &target)?;
            } else {
                fs::copy(&path, &target)?;
            }
        }
        Ok(())
    }

    // AC: the CLI-facing summary reports exact numbers against the
    // discovered set of a fixture with known partial coverage.
    #[test]
    fn substrate_phase_reports_exact_numbers() {
        let project = temp_project();
        let outcome = run_substrate_phase(project.path()).expect("substrate phase must run");
        let report = &outcome.report;
        assert_eq!(report.total, 4);
        assert_eq!(report.documented, ["Button", "hero-banner"]);
        assert_eq!(report.not_yet_documented, ["Broken", "ghost-widget"]);
    }

    // AC (FR-VEN-022/031): docs.jsonl carries one line per documented item,
    // index.jsonl one line per discovered item; both live in .rafters/veneer/.
    #[test]
    fn writes_docs_and_index_jsonl_to_the_veneer_namespace() {
        let project = temp_project();
        let outcome = run_substrate_phase(project.path()).expect("substrate phase must run");

        assert_eq!(outcome.dir, project.path().join(".rafters/veneer"));
        assert_eq!(outcome.index_lines, 4, "one index line per discovered item");
        assert_eq!(
            outcome.docs_lines, 3,
            "one docs line per documented item plus the tokens system line (FR-VEN-022)"
        );

        let index = fs::read_to_string(outcome.dir.join("index.jsonl")).expect("index.jsonl");
        assert_eq!(index.lines().count(), 4);
        assert!(index.contains("\"schema\":\"veneer.index/1\""));

        let docs = fs::read_to_string(outcome.dir.join("docs.jsonl")).expect("docs.jsonl");
        assert_eq!(docs.lines().count(), 3);
        assert!(docs.contains("\"schema\":\"veneer.doc/1\""));
        assert!(
            docs.contains("\"id\":\"system:tokens\""),
            "the namespace-backed fixture yields the system page line"
        );
    }

    // AC (FR-VEN-022/031): two runs over unchanged input are byte-identical.
    #[test]
    fn substrate_is_byte_identical_across_runs() {
        let project = temp_project();

        let first = run_substrate_phase(project.path()).expect("run 1");
        let docs1 = fs::read(first.dir.join("docs.jsonl")).expect("docs 1");
        let index1 = fs::read(first.dir.join("index.jsonl")).expect("index 1");

        run_substrate_phase(project.path()).expect("run 2");
        let docs2 = fs::read(first.dir.join("docs.jsonl")).expect("docs 2");
        let index2 = fs::read(first.dir.join("index.jsonl")).expect("index 2");

        assert_eq!(docs1, docs2, "docs.jsonl must be deterministic");
        assert_eq!(index1, index2, "index.jsonl must be deterministic");
    }

    // AC (FR-VEN-021/031): veneer's only writes under .rafters/ are inside
    // .rafters/veneer/; the rafters config is read-only to veneer.
    #[test]
    fn does_not_write_the_rafters_config() {
        let project = temp_project();
        let config = project.path().join(".rafters/config.rafters.json");
        let before = fs::read(&config).expect("fixture config exists");

        run_substrate_phase(project.path()).expect("substrate phase must run");

        assert_eq!(
            before,
            fs::read(&config).expect("config still exists"),
            "veneer never writes the rafters config"
        );
        assert!(project.path().join(".rafters/veneer/docs.jsonl").exists());
        assert!(project.path().join(".rafters/veneer/index.jsonl").exists());
    }
}
