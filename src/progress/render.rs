//! The status area's text (UiStateTracker.writeProgressBar and its
//! helpers): pure functions of a sampled [`View`], unit-tested here.

use super::{Phase, Strategy};

/// One running (or cache-checking) action, or a running test.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Active {
    pub description: String,
    pub started_ms: u64,
    pub test: bool,
    /// Set once the action's spawn started (None: checking the cache,
    /// staging inputs, a native write).
    pub strategy: Option<Strategy>,
}

impl Active {
    /// Running: a spawn (or a test) is executing, not a cache check.
    fn running(&self) -> bool {
        self.test || self.strategy.is_some()
    }
}

/// One in-flight fetch.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fetch {
    pub what: String,
    pub started_ms: u64,
    /// A download's bytes so far.
    pub bytes: Option<u64>,
}

/// Everything the status area shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct View {
    pub phase: Phase,
    pub now_ms: u64,
    pub targets: usize,
    pub packages: usize,
    /// The targets phase 2 configures, once phase 1 ended.
    pub planned: Option<usize>,
    pub configured: usize,
    pub actions_total: usize,
    pub actions_done: usize,
    pub tests_total: usize,
    pub tests_done: usize,
    pub tests_failed: usize,
    pub active: Vec<Active>,
    pub fetches: Vec<Fetch>,
}

/// How a line's leading word is colored (AnsiTerminalWriter okStatus /
/// failStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Ok,
    Fail,
}

/// One status line: an optional colored lead, then plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub lead: Option<(Tone, String)>,
    pub text: String,
}

impl Line {
    fn plain(text: String) -> Self {
        Self { lead: None, text }
    }

    fn led(tone: Tone, lead: impl Into<String>, text: String) -> Self {
        Self {
            lead: Some((tone, lead.into())),
            text,
        }
    }

    /// The line cut to `width` columns (one per character; `...` marks a
    /// cut), with its lead colored when `color`.
    pub fn render(&self, width: usize, color: bool) -> String {
        let lead = self.lead.as_ref().map_or("", |(_, lead)| lead.as_str());
        let lead_width = lead.chars().count();
        let text_width = self.text.chars().count();
        let mut out = String::new();
        let (lead, text) = if lead_width + text_width <= width {
            (lead.to_owned(), self.text.clone())
        } else if lead_width >= width {
            (cut(lead, width), String::new())
        } else {
            (lead.to_owned(), cut(&self.text, width - lead_width))
        };
        if !lead.is_empty() {
            match (color, &self.lead) {
                (true, Some((tone, _))) => {
                    out.push_str(match tone {
                        Tone::Ok => GREEN,
                        Tone::Fail => RED,
                    });
                    out.push_str(&lead);
                    out.push_str(RESET);
                }
                _ => out.push_str(&lead),
            }
        }
        out.push_str(&text);
        out
    }
}

pub(crate) const GREEN: &str = "\x1b[32m";
pub(crate) const RED: &str = "\x1b[31;1m";
pub(crate) const RESET: &str = "\x1b[0m";
const ELLIPSIS: &str = "...";

/// `text` cut to `width` characters, ending in `...` when cut.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width <= ELLIPSIS.len() {
        return text.chars().take(width).collect();
    }
    let mut out: String = text.chars().take(width - ELLIPSIS.len()).collect();
    out.push_str(ELLIPSIS);
    out
}

/// `count` with thousands separators (Bazel's PROGRESS_MESSAGE_NUMBER_FORMATTER).
pub(crate) fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// A byte count the way Bazel's download progress shows it.
fn bytes(count: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A primary output's short form: below `bazel-out/<config>/bin/` (or
/// `genfiles/`), as Bazel's default progress messages name outputs.
pub(crate) fn short_output(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("bazel-out/") else {
        return path;
    };
    let Some((_, below)) = rest.split_once('/') else {
        return path;
    };
    below
        .strip_prefix("bin/")
        .or_else(|| below.strip_prefix("genfiles/"))
        .unwrap_or(path)
}

/// `Analyzed target //x (P packages loaded, T targets configured).` or
/// `Analyzed N targets (…).` (UiStateTracker.loadingComplete and
/// analysisComplete), without the `INFO: ` prefix; `what` is `target //x`
/// or `N targets`. PackageProgressReceiver.progressState says `packages`
/// whatever the count (`1 packages loaded`).
pub(crate) fn analyzed(what: &str, packages: usize, configured: usize) -> String {
    format!(
        "Analyzed {what} ({packages} packages loaded, {} configured).",
        plural(configured, "target"),
    )
}

/// `Found N targets...` (AnalysisPhaseRunner.reportTargets) or, for `test`
/// (`tests` = the test targets among the `targets`), `Found N test
/// targets...` / `Found N targets and M test targets...`
/// (reportTargetsWithTests), without the `INFO: ` prefix.
pub(crate) fn found(targets: usize, tests: Option<usize>) -> String {
    let Some(tests) = tests else {
        return format!(
            "Found {targets} {}",
            if targets == 1 {
                "target..."
            } else {
                "targets..."
            }
        );
    };
    let others = targets.saturating_sub(tests);
    let tests = format!(
        "{tests} {}",
        if tests == 1 {
            "test target..."
        } else {
            "test targets..."
        }
    );
    if others == 0 {
        format!("Found {tests}")
    } else {
        format!(
            "Found {others} {} and {tests}",
            if others == 1 { "target" } else { "targets" }
        )
    }
}

/// An `INFO: ` line (green lead when `color`).
pub(crate) fn info(text: &str, color: bool) -> String {
    if color {
        format!("{GREEN}INFO:{RESET} {text}")
    } else {
        format!("INFO: {text}")
    }
}

/// The interactive-only summary at the end of the command
/// (BuildResultPrinter / BuildTool.stopRequest): the elapsed time, and how
/// the build completed — for a success (`exit_code` 0) or failed tests (3).
pub(crate) fn summary(
    elapsed: std::time::Duration,
    exit_code: i32,
    actions: usize,
    tests_failed: usize,
    color: bool,
) -> Vec<String> {
    let mut lines = vec![info(
        &format!("Elapsed time: {:.3}s", elapsed.as_secs_f64()),
        color,
    )];
    // UiStateTracker.buildComplete appends the int as it is (no grouping).
    let actions = format!(
        "{actions} total action{}",
        if actions == 1 { "" } else { "s" }
    );
    if exit_code == 0 {
        lines.push(info(
            &format!("Build completed successfully, {actions}"),
            color,
        ));
    } else if exit_code == 3 && tests_failed > 0 {
        lines.push(info(
            &format!(
                "Build completed, {} FAILED, {actions}",
                plural(tests_failed, "test")
            ),
            color,
        ));
    }
    lines
}

/// Seconds since `started_ms`.
fn seconds(view: &View, started_ms: u64) -> u64 {
    view.now_ms.saturating_sub(started_ms) / 1000
}

/// One action's description with its time and strategy
/// (UiStateTracker.describeAction: the time always shows with a strategy,
/// otherwise past 3 seconds).
fn describe(view: &View, active: &Active) -> String {
    let elapsed = seconds(view, active.started_ms);
    match active.strategy {
        Some(strategy) => format!("{}; {elapsed}s {}", active.description, strategy.name()),
        None if active.test || elapsed > 3 => format!("{}; {elapsed}s", active.description),
        None => active.description.clone(),
    }
}

/// The status area of `view`, at most `shown` action and fetch lines.
pub(crate) fn lines(view: &View, shown: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    match view.phase {
        Phase::Starting => lines.push(Line::led(
            Tone::Ok,
            "Computing main repo mapping:",
            String::new(),
        )),
        Phase::Loading => lines.push(Line::led(
            Tone::Ok,
            "Loading:",
            // PackageProgressReceiver.progressState: `packages` whatever the
            // count.
            format!(" {} packages loaded", view.packages),
        )),
        Phase::Analyzing => {
            // AnalysisProgressReceiver: "N targets configured"; once phase 1
            // ended, out of the targets phase 2 configures.
            let configured = match view.planned {
                Some(planned) if planned >= view.configured && planned > 0 => {
                    format!("{} / {planned} targets configured", view.configured)
                }
                _ => format!("{} configured", plural(view.configured, "target")),
            };
            lines.push(Line::led(
                Tone::Ok,
                "Analyzing:",
                format!(
                    " {} ({} packages loaded, {configured})",
                    plural(view.targets, "target"),
                    view.packages
                ),
            ));
        }
        Phase::SnapshotHit => lines.push(Line::led(
            Tone::Ok,
            "Analysis snapshot hit:",
            " preparing the saved plan".to_owned(),
        )),
        Phase::Executing => execution(view, shown, &mut lines),
    }
    fetch_lines(view, shown, &mut lines);
    lines
}

/// UiStateTracker.writeExecutionProgress.
fn execution(view: &View, shown: usize, lines: &mut Vec<Line>) {
    let lead = format!(
        "[{} / {}]",
        grouped(view.actions_done),
        grouped(view.actions_total)
    );
    let mut text = String::new();
    if view.tests_total > 0 {
        text.push_str(&format!(
            " {} / {} tests",
            view.tests_done, view.tests_total
        ));
        if view.tests_failed > 0 {
            text.push_str(&format!(", {} failed", view.tests_failed));
        }
        text.push(';');
    }
    let running: Vec<&Active> = view.active.iter().filter(|a| a.running()).collect();
    let total = view.active.len();
    let mut details = Vec::new();
    if running.is_empty() {
        text.push_str(if total > 0 {
            " checking cached actions"
        } else {
            " no actions running"
        });
    } else if total == 1 {
        text.push(' ');
        text.push_str(&describe(view, running[0]));
    } else {
        text.push_str(&if total == running.len() {
            format!(" {total} actions running")
        } else {
            format!(" {total} actions, {} running", running.len())
        });
        for active in running.iter().take(shown) {
            details.push(Line::plain(format!("    {}", describe(view, active))));
        }
        if running.len() > shown
            && let Some(last) = details.last_mut()
        {
            last.text.push_str(" ...");
        }
    }
    // A failed test turns the progress lead red (failStatus).
    let tone = if view.tests_failed > 0 {
        Tone::Fail
    } else {
        Tone::Ok
    };
    lines.push(Line::led(tone, lead, text));
    lines.extend(details);
}

/// UiStateTracker.reportOnDownloads: `    Fetching <what>; <bytes> <N>s`.
fn fetch_lines(view: &View, shown: usize, lines: &mut Vec<Line>) {
    for (index, fetch) in view.fetches.iter().take(shown).enumerate() {
        let mut postfix = String::new();
        if let Some(count) = fetch.bytes {
            postfix.push(' ');
            postfix.push_str(&bytes(count));
        }
        let elapsed = seconds(view, fetch.started_ms);
        if elapsed > 0 || fetch.bytes.is_none() {
            postfix.push_str(&format!(" {elapsed}s"));
        }
        let mut text = format!("    Fetching {};{postfix}", fetch.what);
        if index + 1 == shown && view.fetches.len() > shown {
            text.push_str(&format!(" ... ({} fetches)", view.fetches.len()));
        }
        lines.push(Line::plain(text));
    }
}

/// What the status area adds to the terminal title (`--progress_in_terminal_title`).
pub(crate) fn title(lines: &[Line]) -> String {
    lines
        .first()
        .map(|line| {
            let lead = line.lead.as_ref().map_or("", |(_, lead)| lead.as_str());
            format!("{lead}{}", line.text)
        })
        .unwrap_or_default()
}

/// Emits a refresh at most every `interval` (`--show_progress_rate_limit`).
#[derive(Debug)]
pub(crate) struct RateLimit {
    interval: std::time::Duration,
    next: std::time::Instant,
}

impl RateLimit {
    /// The first refresh is due one interval after `now`.
    pub fn new(interval: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            interval,
            next: now + interval,
        }
    }

    /// Whether a refresh is due at `now`; if so the next one is an
    /// interval later.
    pub fn due(&mut self, now: std::time::Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.interval;
        true
    }

    /// How long until the next refresh is due.
    pub fn remaining(&self, now: std::time::Instant) -> std::time::Duration {
        self.next.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn view(phase: Phase) -> View {
        View {
            phase,
            now_ms: 10_000,
            targets: 3,
            packages: 597,
            planned: None,
            configured: 4521,
            actions_total: 1234,
            actions_done: 56,
            tests_total: 0,
            tests_done: 0,
            tests_failed: 0,
            active: Vec::new(),
            fetches: Vec::new(),
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| line.render(200, false)).collect()
    }

    fn action(description: &str, started_ms: u64, strategy: Option<Strategy>) -> Active {
        Active {
            description: description.to_owned(),
            started_ms,
            test: false,
            strategy,
        }
    }

    #[test]
    fn loading_and_analysis_lines() {
        assert_eq!(
            text(&lines(&view(Phase::Starting), 3)),
            ["Computing main repo mapping:"]
        );
        assert_eq!(
            text(&lines(&view(Phase::Loading), 3)),
            ["Loading: 597 packages loaded"]
        );
        assert_eq!(
            text(&lines(&view(Phase::Analyzing), 3)),
            ["Analyzing: 3 targets (597 packages loaded, 4521 targets configured)"]
        );
        let mut planned = view(Phase::Analyzing);
        planned.planned = Some(18968);
        planned.targets = 1;
        planned.packages = 1;
        assert_eq!(
            text(&lines(&planned, 3)),
            ["Analyzing: 1 target (1 packages loaded, 4521 / 18968 targets configured)"]
        );
        assert_eq!(
            text(&lines(&view(Phase::SnapshotHit), 3)),
            ["Analysis snapshot hit: preparing the saved plan"]
        );
    }

    #[test]
    fn execution_lines() {
        let mut idle = view(Phase::Executing);
        assert_eq!(text(&lines(&idle, 3)), ["[56 / 1,234] no actions running"]);
        idle.active.push(action("GoCompilePkg a", 9_000, None));
        assert_eq!(
            text(&lines(&idle, 3)),
            ["[56 / 1,234] checking cached actions"]
        );
        let mut one = view(Phase::Executing);
        one.active
            .push(action("GoLink b", 5_000, Some(Strategy::LinuxSandbox)));
        assert_eq!(
            text(&lines(&one, 3)),
            ["[56 / 1,234] GoLink b; 5s linux-sandbox"]
        );
        let mut many = view(Phase::Executing);
        for (index, name) in ["a", "b", "c", "d"].iter().enumerate() {
            many.active.push(action(
                &format!("Genrule {name}"),
                1_000 * index as u64,
                Some(Strategy::Local),
            ));
        }
        many.active.push(action("Action e", 9_500, None));
        assert_eq!(
            text(&lines(&many, 3)),
            [
                "[56 / 1,234] 5 actions, 4 running",
                "    Genrule a; 10s local",
                "    Genrule b; 9s local",
                "    Genrule c; 8s local ...",
            ]
        );
        let mut tests = view(Phase::Executing);
        tests.tests_total = 4;
        tests.tests_done = 2;
        tests.tests_failed = 1;
        tests.active.push(Active {
            description: "Testing //a:t".to_owned(),
            started_ms: 8_000,
            test: true,
            strategy: None,
        });
        assert_eq!(
            text(&lines(&tests, 3)),
            ["[56 / 1,234] 2 / 4 tests, 1 failed; Testing //a:t; 2s"]
        );
    }

    #[test]
    fn fetch_lines_follow_the_phase() {
        let mut fetching = view(Phase::Starting);
        fetching.fetches = vec![
            Fetch {
                what: "repository @@rules_go+".to_owned(),
                started_ms: 6_000,
                bytes: None,
            },
            Fetch {
                what: "https://example.com/go.tar.gz".to_owned(),
                started_ms: 8_000,
                bytes: Some(3 * 1024 * 1024 + 300 * 1024),
            },
            Fetch {
                what: "module extension @@rules_go+//go:extensions.bzl%go_sdk".to_owned(),
                started_ms: 9_900,
                bytes: None,
            },
        ];
        assert_eq!(
            text(&lines(&fetching, 2)),
            [
                "Computing main repo mapping:",
                "    Fetching repository @@rules_go+; 4s",
                "    Fetching https://example.com/go.tar.gz; 3.3 MiB 2s ... (3 fetches)",
            ]
        );
    }

    #[test]
    fn lines_cut_to_the_width_and_color_the_lead() {
        let line = Line::led(
            Tone::Ok,
            "[1 / 2]",
            " GoCompilePkg src/very/long/path".to_owned(),
        );
        assert_eq!(line.render(20, false), "[1 / 2] GoCompile...");
        assert_eq!(line.render(5, false), "[1...");
        assert_eq!(
            line.render(80, true),
            "\x1b[32m[1 / 2]\x1b[0m GoCompilePkg src/very/long/path"
        );
        assert_eq!(Line::plain("é".repeat(10)).render(4, true), "é...");
    }

    #[test]
    fn helpers() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
        assert_eq!(bytes(10), "10 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(
            short_output("bazel-out/k8-fastbuild/bin/src/a/a.a"),
            "src/a/a.a"
        );
        assert_eq!(short_output("bazel-out/x"), "bazel-out/x");
        assert_eq!(short_output("external/x"), "external/x");
        assert_eq!(
            analyzed("target //:b", 1, 1),
            "Analyzed target //:b (1 packages loaded, 1 target configured)."
        );
        assert_eq!(
            analyzed("2 targets", 2, 3),
            "Analyzed 2 targets (2 packages loaded, 3 targets configured)."
        );
        assert_eq!(found(1, None), "Found 1 target...");
        assert_eq!(found(2, None), "Found 2 targets...");
        assert_eq!(found(1, Some(1)), "Found 1 test target...");
        assert_eq!(found(2, Some(2)), "Found 2 test targets...");
        assert_eq!(found(2, Some(1)), "Found 1 target and 1 test target...");
        assert_eq!(found(5, Some(2)), "Found 3 targets and 2 test targets...");
        assert_eq!(
            summary(Duration::from_millis(1500), 0, 1, 0, false),
            [
                "INFO: Elapsed time: 1.500s",
                "INFO: Build completed successfully, 1 total action"
            ]
        );
        assert_eq!(
            summary(Duration::from_secs(2), 3, 1234, 2, false),
            [
                "INFO: Elapsed time: 2.000s",
                "INFO: Build completed, 2 tests FAILED, 1234 total actions"
            ]
        );
        assert_eq!(
            summary(Duration::from_secs(2), 1, 689, 0, true),
            ["\x1b[32mINFO:\x1b[0m Elapsed time: 2.000s"]
        );
    }

    #[test]
    fn rate_limit() {
        let start = Instant::now();
        let mut limit = RateLimit::new(Duration::from_millis(200), start);
        assert!(!limit.due(start));
        assert!(!limit.due(start + Duration::from_millis(199)));
        assert_eq!(
            limit.remaining(start + Duration::from_millis(150)),
            Duration::from_millis(50)
        );
        assert!(limit.due(start + Duration::from_millis(200)));
        assert!(!limit.due(start + Duration::from_millis(300)));
        assert!(limit.due(start + Duration::from_millis(450)));
        assert_eq!(
            limit.remaining(start + Duration::from_millis(1000)),
            Duration::ZERO
        );
    }
}
