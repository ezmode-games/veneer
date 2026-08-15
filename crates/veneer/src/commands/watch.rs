//! Watch mode (issue #94 / FR-VEN-026): a thin foreground loop that
//! re-derives veneer's OWN outputs when a watched input changes.
//!
//! This is a scheduler around the existing pipeline, not new rendering
//! logic. On a change to a watched input it calls
//! `crate::commands::extract::run_substrate_phase` -- the SAME derivation
//! entry point the batch `extract` command uses -- so watch re-derives
//! whatever extract emits today (`docs.jsonl` + `index.jsonl`) and whatever
//! it emits tomorrow, without watch encoding an assumption about the output
//! set.
//!
//! Explicitly out of scope (see the issue): no server, no HMR channel, no
//! network listener; no rafters/tailwind toolchain invocation (veneer never
//! regenerates the project's rafters state, only re-reads and re-derives
//! its own outputs); no daemon management (pidfiles, restarts) -- this is a
//! foreground process the operator supervises; no incremental/partial
//! re-derivation -- a whole re-derive on change is correct and cheap at
//! this scale.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use veneer_adapters::is_excluded_dir_name;

use super::extract::run_substrate_phase;

/// Default quiet period after the last relevant change before a
/// re-derivation fires. Chosen to collapse an editor's rapid save-on-type
/// or a formatter's multi-file rewrite into one derivation, without making
/// an operator wait noticeably after a single deliberate save.
const DEFAULT_DEBOUNCE_MS: u64 = 300;

#[derive(Args)]
pub struct WatchArgs {
    /// Path to the target rafters project. Re-derives the component
    /// substrate into its .rafters/veneer/ directory on every watched
    /// change, the same as `extract --project`.
    #[arg(short, long)]
    project: PathBuf,

    /// Debounce window in milliseconds: how long a burst of changes must go
    /// quiet before one re-derivation fires.
    #[arg(long, default_value_t = DEFAULT_DEBOUNCE_MS)]
    debounce_ms: u64,
}

pub async fn run(args: WatchArgs) -> Result<()> {
    if !args.project.exists() {
        anyhow::bail!("Project path does not exist: {}", args.project.display());
    }
    let project = args.project.clone();
    let debounce = Duration::from_millis(args.debounce_ms.max(1));

    // Watch mode never spawns and never blocks the tokio runtime it shares
    // with the CLI's other commands; the loop itself is synchronous I/O
    // (channel recv + filesystem), so it runs on a blocking thread.
    tokio::task::spawn_blocking(move || watch_loop(&project, debounce, None))
        .await
        .context("watch loop task panicked")??;

    Ok(())
}

/// One filesystem-driven derivation loop. `shutdown`, when given, is
/// polled non-blockingly each iteration so tests can stop the loop instead
/// of racing it forever; production callers pass `None` and rely on the
/// operator killing the process (no daemon management -- FR-VEN-026).
fn watch_loop(
    project: &Path,
    debounce: Duration,
    shutdown: Option<mpsc::Receiver<()>>,
) -> Result<()> {
    // Resolve the project root to its canonical form up front. FSEvents (and
    // some other backends) report canonical paths -- on macOS a `/var/...`
    // temp root surfaces as `/private/var/...` -- so a raw root would make
    // every `strip_prefix` in `is_watched_input` silently fail and no change
    // would ever register. Canonicalizing here keeps the watched roots and
    // the event-path comparison on the same footing; `is_watched_input`
    // stays a pure prefix check (unit-tested with synthetic paths).
    let project = project
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", project.display()))?;
    let project = project.as_path();

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        // The channel receiver outlives every send: it is dropped only when
        // `watch_loop` returns, which also drops `watcher`. A send failing
        // here would mean the loop already exited; nothing to report.
        let _ = tx.send(event);
    })
    .context("failed to start the filesystem watcher")?;

    let roots = watch_roots(project).context("failed to enumerate watch roots")?;
    if roots.is_empty() {
        anyhow::bail!(
            "no watchable input under {} (component/composite source or .rafters/ state)",
            project.display()
        );
    }
    for root in &roots {
        let mode = if root.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(root, mode)
            .with_context(|| format!("failed to watch {}", root.display()))?;
    }

    tracing::info!(
        "watch: deriving the initial substrate for {}",
        project.display()
    );
    run_derivation(project)?;

    tracing::info!(
        "watch: watching {} root path(s) under {} (debounce {}ms); ctrl-c to stop",
        roots.len(),
        project.display(),
        debounce.as_millis()
    );

    let mut debouncer = Debouncer::new(debounce);
    loop {
        if let Some(shutdown) = &shutdown {
            match shutdown.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        // Bound the wait even with nothing pending so a `shutdown` signal
        // sent between iterations is noticed promptly; production callers
        // (no `shutdown`) simply loop again and re-block.
        let timeout = debouncer
            .wait_hint(Instant::now())
            .unwrap_or(Duration::from_millis(200));
        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event
                    .paths
                    .iter()
                    .any(|path| is_watched_input(project, path))
                {
                    debouncer.record_event(Instant::now());
                }
            }
            Ok(Err(error)) => {
                tracing::warn!("watch: filesystem watcher error: {error}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The watcher (and its callback closure holding `tx`) was
                // dropped -- nothing left to watch.
                return Ok(());
            }
        }

        if debouncer.try_fire(Instant::now()) {
            run_derivation(project)?;
        }
    }
}

/// Re-derive veneer's own outputs by calling the SAME substrate phase the
/// batch `extract` command runs. Watch never writes an output byte itself;
/// every write goes through `run_substrate_phase`, which is the only write
/// path under `.rafters/` (FR-VEN-021/022/031) -- so watch inherits that
/// guarantee structurally rather than re-asserting it.
fn run_derivation(project: &Path) -> Result<()> {
    let outcome = run_substrate_phase(project)?;
    tracing::info!(
        "watch: re-derived {} docs line(s) and {} index line(s) into {}",
        outcome.docs_lines,
        outcome.index_lines,
        outcome.dir.display()
    );
    Ok(())
}

/// The top-level paths to register with the filesystem watcher: every entry
/// directly under `project_root` except dependency/build-output and hidden
/// directories -- with `.rafters/` explicitly re-included, since it is read
/// separately from the general source walk but is still a real input (the
/// namespace tokens, the compiled stylesheet, `config.rafters.json`).
///
/// This is a structural enforcement of "the watched set comes from the
/// project input contract, never a guessed glob beyond it": `node_modules/`,
/// `target/`, `dist/`, `build/`, and other dot-directories are never handed
/// to the watcher at all, so no event can originate from them.
fn watch_roots(project_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let entries = std::fs::read_dir(project_root)
        .with_context(|| format!("failed to read {}", project_root.display()))?;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read entry under {}", project_root.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir && name != ".rafters" && is_excluded_dir_name(name) {
            continue;
        }
        roots.push(path);
    }
    roots.sort();
    Ok(roots)
}

/// True when a changed path is a genuine input to the derivation
/// `run_substrate_phase` reads: component/composite source under
/// `project_root` (excluding dependency/build-output/hidden directories),
/// or the project's `.rafters/` state (tokens, compiled stylesheet,
/// `config.rafters.json`, and anything else declared there).
///
/// `.rafters/veneer/` is excluded even though it is nested under
/// `.rafters/`: it is where `run_substrate_phase` WRITES, never an input.
/// Treating it as watched would make every derivation fire an event that
/// triggers another derivation -- an infinite self-triggered loop.
pub(crate) fn is_watched_input(project_root: &Path, changed: &Path) -> bool {
    if changed.starts_with(project_root.join(".rafters").join("veneer")) {
        return false;
    }
    let Ok(relative) = changed.strip_prefix(project_root) else {
        return false;
    };
    for component in relative.components() {
        let std::path::Component::Normal(os_name) = component else {
            continue;
        };
        let Some(name) = os_name.to_str() else {
            return false;
        };
        if name == ".rafters" {
            continue;
        }
        if is_excluded_dir_name(name) {
            return false;
        }
    }
    true
}

/// A trailing-edge debounce: collapses a burst of relevant events into
/// exactly one fire, once the burst has gone quiet for `window`. A pure
/// state machine over caller-supplied `Instant`s, so it is fully testable
/// without real sleeps (construct synthetic instants with `Instant::now() +
/// Duration::from_millis(n)`) and without owning a real filesystem watcher.
struct Debouncer {
    window: Duration,
    pending_since: Option<Instant>,
}

impl Debouncer {
    fn new(window: Duration) -> Self {
        Debouncer {
            window,
            pending_since: None,
        }
    }

    /// Record a relevant change at `now`, (re)starting the quiet window.
    fn record_event(&mut self, now: Instant) {
        self.pending_since = Some(now);
    }

    /// How long the caller should wait before checking again: the
    /// remainder of the debounce window while a burst is pending, or `None`
    /// to wait indefinitely (nothing pending).
    fn wait_hint(&self, now: Instant) -> Option<Duration> {
        self.pending_since.map(|since| {
            let elapsed = now.saturating_duration_since(since);
            self.window.saturating_sub(elapsed)
        })
    }

    /// Check whether the debounce window has elapsed at `now`. If so,
    /// clears the pending state -- one fire per burst -- and returns true.
    fn try_fire(&mut self, now: Instant) -> bool {
        match self.pending_since {
            Some(since) if now.saturating_duration_since(since) >= self.window => {
                self.pending_since = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- Debouncer: collapses a burst to one derivation ----------------

    // AC (issue #94): n rapid, closely-spaced events collapse into exactly
    // one fire once the burst goes quiet for the debounce window, not n
    // racing fires.
    #[test]
    fn debounce_collapses_a_burst_of_events_to_one_fire() {
        let window = Duration::from_millis(100);
        let mut debouncer = Debouncer::new(window);
        let t0 = Instant::now();

        // A burst of five saves, each well inside the debounce window of
        // the previous one -- an editor's rapid save-on-type.
        for offset_ms in [0u64, 10, 25, 40, 55] {
            debouncer.record_event(t0 + Duration::from_millis(offset_ms));
        }

        // Before the window has elapsed since the LAST event: no fire yet.
        let still_within_window = t0 + Duration::from_millis(55 + 50);
        assert!(!debouncer.try_fire(still_within_window));

        // Once quiet for the full window since the last event: fires once.
        let after_window = t0 + Duration::from_millis(55) + window;
        assert!(debouncer.try_fire(after_window));

        // Immediately re-checking without a new event: no second fire for
        // the same burst.
        assert!(!debouncer.try_fire(after_window + Duration::from_millis(1)));
    }

    #[test]
    fn debounce_fires_again_for_a_new_burst_after_the_first() {
        let window = Duration::from_millis(100);
        let mut debouncer = Debouncer::new(window);
        let t0 = Instant::now();

        debouncer.record_event(t0);
        assert!(debouncer.try_fire(t0 + window));

        // A new event after the first burst was consumed starts a new
        // window and can fire again.
        let t1 = t0 + Duration::from_secs(1);
        debouncer.record_event(t1);
        assert!(!debouncer.try_fire(t1 + Duration::from_millis(50)));
        assert!(debouncer.try_fire(t1 + window));
    }

    #[test]
    fn debounce_wait_hint_is_none_when_nothing_pending() {
        let debouncer = Debouncer::new(Duration::from_millis(100));
        assert_eq!(debouncer.wait_hint(Instant::now()), None);
    }

    // ---- is_watched_input: the input contract, not a guessed glob ------

    #[test]
    fn watched_input_includes_component_and_composite_source() {
        let root = Path::new("/project");
        assert!(is_watched_input(
            root,
            &root.join("src/components/Button.tsx")
        ));
        assert!(is_watched_input(
            root,
            &root.join("composites/hero-banner.composite.json")
        ));
        assert!(is_watched_input(
            root,
            &root.join("docs/spec/matrix/components.jsonl")
        ));
    }

    #[test]
    fn watched_input_includes_the_rafters_state() {
        let root = Path::new("/project");
        assert!(is_watched_input(
            root,
            &root.join(".rafters/tokens/color.rafters.json")
        ));
        assert!(is_watched_input(
            root,
            &root.join(".rafters/output/rafters.documentation.css")
        ));
        assert!(is_watched_input(
            root,
            &root.join(".rafters/config.rafters.json")
        ));
    }

    // AC / loop-prevention: veneer's own output directory is never a
    // watched input. Without this exclusion every derivation would write
    // `.rafters/veneer/docs.jsonl`, generate a filesystem event, and
    // trigger another derivation forever.
    #[test]
    fn watched_input_excludes_veneers_own_output_directory() {
        let root = Path::new("/project");
        assert!(!is_watched_input(
            root,
            &root.join(".rafters/veneer/docs.jsonl")
        ));
        assert!(!is_watched_input(
            root,
            &root.join(".rafters/veneer/index.jsonl")
        ));
        assert!(!is_watched_input(
            root,
            &root.join(".rafters/veneer/docs.jsonl.tmp")
        ));
    }

    #[test]
    fn watched_input_excludes_dependency_build_and_hidden_directories() {
        let root = Path::new("/project");
        assert!(!is_watched_input(
            root,
            &root.join("node_modules/react/index.js")
        ));
        assert!(!is_watched_input(root, &root.join("target/debug/veneer")));
        assert!(!is_watched_input(root, &root.join("dist/bundle.js")));
        assert!(!is_watched_input(root, &root.join(".git/HEAD")));
    }

    #[test]
    fn watched_input_excludes_paths_outside_the_project() {
        let root = Path::new("/project");
        assert!(!is_watched_input(root, Path::new("/elsewhere/file.tsx")));
    }

    // ---- watch_roots: never registers an excluded directory -------------

    #[test]
    fn watch_roots_excludes_dependency_build_and_hidden_directories_but_keeps_rafters() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "src",
            "node_modules",
            "target",
            "dist",
            "build",
            ".git",
            ".rafters",
        ] {
            fs::create_dir(dir.path().join(name)).expect("mkdir");
        }
        fs::write(dir.path().join("veneer.json"), "{}").expect("write");

        let roots = watch_roots(dir.path()).expect("watch_roots");
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.contains(&"src".to_string()));
        assert!(names.contains(&".rafters".to_string()));
        assert!(names.contains(&"veneer.json".to_string()));
        assert!(!names.contains(&"node_modules".to_string()));
        assert!(!names.contains(&"target".to_string()));
        assert!(!names.contains(&"dist".to_string()));
        assert!(!names.contains(&"build".to_string()));
        assert!(!names.contains(&".git".to_string()));
    }

    // ---- run_derivation: reuses extract's own determinism guarantee -----

    /// The committed fixture with known partial coverage, shared with
    /// `extract.rs`'s own tests (no new hand-modeled `.rafters` fixture --
    /// see `veneer-adapters/tests/fixtures/README.md`).
    fn partial_coverage_project() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../veneer-adapters/tests/fixtures/coverage/partial")
    }

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

    // AC (issue #94): a watch tick over unchanged input rewrites outputs
    // byte-identically -- no churn. Exercised through watch's own call
    // site (`run_derivation`, which calls the same `run_substrate_phase`
    // extract uses) rather than re-deriving the guarantee independently.
    #[test]
    fn watch_tick_over_unchanged_input_is_byte_identical() {
        let project = temp_project();

        run_derivation(project.path()).expect("first derivation");
        let dir = project.path().join(".rafters/veneer");
        let docs1 = fs::read(dir.join("docs.jsonl")).expect("docs.jsonl 1");
        let index1 = fs::read(dir.join("index.jsonl")).expect("index.jsonl 1");

        run_derivation(project.path()).expect("second derivation, unchanged input");
        let docs2 = fs::read(dir.join("docs.jsonl")).expect("docs.jsonl 2");
        let index2 = fs::read(dir.join("index.jsonl")).expect("index.jsonl 2");

        assert_eq!(docs1, docs2, "docs.jsonl must be byte-identical, no churn");
        assert_eq!(
            index1, index2,
            "index.jsonl must be byte-identical, no churn"
        );
    }

    // AC: killing the watcher mid-derivation then running batch extract
    // still yields correct, complete output. Watch never introduces its
    // own partial-write state (no lockfiles/pidfiles); it leans entirely
    // on `run_substrate_phase`'s existing atomic temp+rename writer. This
    // test pins that watch's derivation call site is that same function,
    // so a kill between two `run_derivation` calls leaves at worst the
    // previous complete output (never a torn file) -- the same guarantee
    // `extract.rs`'s `write_atomic` already provides and is tested there.
    #[test]
    fn watch_and_batch_extract_share_one_derivation_entry_point() {
        let project = temp_project();
        run_derivation(project.path()).expect("watch derivation");
        let dir = project.path().join(".rafters/veneer");
        assert!(dir.join("docs.jsonl").exists());
        assert!(dir.join("index.jsonl").exists());

        // Re-running the batch phase directly (what `extract` itself calls)
        // over the watch-produced output must be a no-op in content.
        let before = fs::read(dir.join("docs.jsonl")).expect("docs.jsonl before");
        run_substrate_phase(project.path()).expect("batch extract phase");
        let after = fs::read(dir.join("docs.jsonl")).expect("docs.jsonl after");
        assert_eq!(before, after);
    }

    // ---- structural: watch never writes the rafters config --------------

    // AC: watch performs zero writes to the project's rafters state.
    // `run_substrate_phase` is independently tested
    // (`does_not_write_the_rafters_config` in extract.rs) to be the only
    // writer under `.rafters/`, and watch.rs contains no `fs::write` /
    // `fs::create_dir_all` of its own -- every write goes through that one
    // function. This test pins the observable half of that guarantee from
    // watch's own call site.
    #[test]
    fn watch_derivation_does_not_write_the_rafters_config() {
        let project = temp_project();
        let config = project.path().join(".rafters/config.rafters.json");
        let before = fs::read(&config).expect("fixture config exists");

        run_derivation(project.path()).expect("watch derivation");

        assert_eq!(
            before,
            fs::read(&config).expect("config still exists"),
            "watch never writes the rafters config"
        );
    }

    // ---- end-to-end: a real filesystem edit drives a real re-derivation --

    /// Poll `probe` until it returns `Some`, or panic once `timeout`
    /// elapses. Never `sleep`-and-assume: this is how the fs-event test
    /// stays robust against real (and real-CI) filesystem-watch latency.
    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>, timeout: Duration) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = probe() {
                return value;
            }
            if Instant::now() >= deadline {
                panic!("condition did not become true within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // AC (issue #94): editing a watched component source re-derives
    // veneer's outputs with no manual action, end to end through the real
    // `notify` watcher -- not just the extracted decision logic above.
    // Robust against fs-watch latency: polls for the observable effect
    // (docs.jsonl's mtime advancing) with a timeout instead of sleeping a
    // fixed guess.
    #[test]
    fn watch_loop_rederives_on_a_real_filesystem_change() {
        let project = temp_project();
        let project_path = project.path().to_path_buf();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            watch_loop(&project_path, Duration::from_millis(50), Some(shutdown_rx))
        });

        let docs_path = project.path().join(".rafters/veneer/docs.jsonl");
        let initial_mtime = wait_for(
            || fs::metadata(&docs_path).ok()?.modified().ok(),
            Duration::from_secs(10),
        );

        // A real edit to a watched component source file.
        let button_path = project.path().join("components/button.tsx");
        let mut source = fs::read_to_string(&button_path).expect("read button.tsx");
        source.push_str("\n// watch-mode integration test edit\n");
        fs::write(&button_path, source).expect("write button.tsx");

        let rederived_mtime = wait_for(
            || {
                let mtime = fs::metadata(&docs_path).ok()?.modified().ok()?;
                (mtime > initial_mtime).then_some(mtime)
            },
            Duration::from_secs(10),
        );
        assert!(rederived_mtime > initial_mtime);

        let _ = shutdown_tx.send(());
        handle
            .join()
            .expect("watch loop thread panicked")
            .expect("watch loop returned an error");
    }
}
