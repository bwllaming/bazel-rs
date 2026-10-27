//! `bazel coverage` and `--collect_code_coverage` (docs/COVERAGE.md).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (Release 9.3.0); Java
//! paths are relative to `src/main/java/com/google/devtools/build/lib/`, the
//! coverage output generator's to
//! `tools/test/CoverageOutputGenerator/java/com/google/devtools/coverageoutputgenerator/`.
//!
//! This module holds what is not analysis:
//!
//! * [`RegexFilter`] (`--instrumentation_filter`, common/RegexFilter.java)
//!   and InstrumentationFilterSupport's default heuristic
//!   ([`default_instrumentation_filter`]);
//! * the command-level coverage options ([`Settings`]: `--combined_report`,
//!   `--coverage_output_generator`, `--coverage_report_generator`,
//!   `--coverage_support`, `--experimental_split_coverage_postprocessing`,
//!   `--experimental_fetch_all_coverage_outputs`), set once per process;
//! * the native coverage output generator ([`output_generator_main`], the
//!   hidden `__coverage-output-generator` subcommand): a port of
//!   CoverageOutputGenerator's LCOV merger (`Main`, `LcovParser`,
//!   `LcovPrinter`, `Coverage`, `SourceFileCoverage`), used where genuine
//!   Bazel runs `@remote_coverage_tools//:Main`, a rules_java `java_binary`
//!   the native profile cannot build (declared divergence).
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// CoreOptions.instrumentationFilter's default (CoreOptions.java:358-372).
pub const DEFAULT_INSTRUMENTATION_FILTER: &str = "-/javatests[/:],-/test/java[/:]";

/// The hidden subcommand of the native coverage output generator (the
/// `LCOV_MERGER` of a coverage test action and the coverage report
/// generator, both CoverageOutputGenerator's `Main` in Bazel).
pub const OUTPUT_GENERATOR_ARG: &str = "__coverage-output-generator";

/// The default `--coverage_output_generator`
/// (CoverageConfiguration.java:40-56) and `--coverage_report_generator`
/// (:57-74): aliases to `@remote_coverage_tools//:Main`, which the native
/// coverage output generator replaces.
pub const DEFAULT_OUTPUT_GENERATOR: &str = "@@bazel_tools//tools/test:lcov_merger";
pub const DEFAULT_REPORT_GENERATOR: &str = "@@bazel_tools//tools/test:coverage_report_generator";

/// RegexFilter (common/RegexFilter.java): comma-separated regexes (`\,`
/// escapes a comma), `-` excludes, `+` (optional) includes; a value is
/// included when no exclusion matches and some inclusion (if any) does
/// (`Matcher.find`).
#[derive(Clone)]
pub struct RegexFilter {
    inclusion: Option<(String, fancy_regex::Regex)>,
    exclusion: Option<(String, fancy_regex::Regex)>,
}

impl fmt::Debug for RegexFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RegexFilter({self})")
    }
}

impl RegexFilter {
    /// RegexFilterConverter.convert (:43-74).
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.starts_with("--") {
            return Err(format!(
                "Failed to build filter: value looks like another flag ({input}). Either escape the value with \"\\-\\-\", or pass an explicit value to the flag."
            ));
        }
        let mut inclusions = Vec::new();
        let mut exclusions = Vec::new();
        // input.split("(?<!\\\\),"): Java's String.split drops trailing
        // empty strings (they are skipped below anyway).
        let mut pieces = Vec::new();
        let mut current = String::new();
        let mut previous = None;
        for character in input.chars() {
            if character == ',' && previous != Some('\\') {
                pieces.push(std::mem::take(&mut current));
            } else {
                current.push(character);
            }
            previous = Some(character);
        }
        pieces.push(current);
        for piece in pieces {
            let piece = piece.replace("\\,", ",");
            let excluded = piece.starts_with('-');
            let piece = if excluded || piece.starts_with('+') {
                piece[1..].to_owned()
            } else {
                piece
            };
            if !piece.is_empty() {
                if excluded {
                    exclusions.push(piece);
                } else {
                    inclusions.push(piece);
                }
            }
        }
        Ok(Self {
            inclusion: union(inclusions)?,
            exclusion: union(exclusions)?,
        })
    }

    /// RegexFilter.isIncluded (:118-126).
    pub fn is_included(&self, value: &str) -> bool {
        if let Some((_, exclusion)) = &self.exclusion
            && exclusion.is_match(value).unwrap_or(false)
        {
            return false;
        }
        match &self.inclusion {
            None => true,
            Some((_, inclusion)) => inclusion.is_match(value).unwrap_or(false),
        }
    }
}

/// takeUnionOfRegexes (:104-114): sorted, deduplicated, each an atomic group.
fn union(regexes: Vec<String>) -> Result<Option<(String, fancy_regex::Regex)>, String> {
    if regexes.is_empty() {
        return Ok(None);
    }
    let deduped: BTreeSet<String> = regexes.into_iter().collect();
    let pattern = format!(
        "(?:(?>{}))",
        deduped.into_iter().collect::<Vec<_>>().join(")|(?>")
    );
    let regex = fancy_regex::Regex::new(&pattern)
        .map_err(|error| format!("Failed to build valid regular expression: {error}"))?;
    Ok(Some((pattern, regex)))
}

/// RegexFilter.toString (:133-147).
impl fmt::Display for RegexFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((pattern, _)) = &self.inclusion {
            f.write_str(&pattern.replace(',', "\\,"))?;
            if self.exclusion.is_some() {
                f.write_str(",")?;
            }
        }
        if let Some((pattern, _)) = &self.exclusion {
            write!(f, "-{}", pattern.replace(',', "\\,"))?;
        }
        Ok(())
    }
}

/// What a coverage configuration's analysis reads of its instrumentation
/// options (BuildConfigurationValue.getInstrumentationFilter,
/// shouldInstrumentTestTargets, shouldCollectCodeCoverageForGeneratedFiles).
#[derive(Clone, Debug)]
pub struct Instrumenting {
    pub filter: RegexFilter,
    pub test_targets: bool,
    pub generated_files: bool,
}

impl Instrumenting {
    pub fn of(options: &crate::configuration::Instrumentation) -> Self {
        Self {
            filter: options.filter(),
            test_targets: options.test_targets,
            generated_files: options.generated_files,
        }
    }

    /// InstrumentedFilesCollector.shouldIncludeLocalSources
    /// (analysis/test/InstrumentedFilesCollector.java:176-186): `label` is
    /// Label.toString (the canonical form).
    pub fn includes(&self, label: &str, is_test: bool) -> bool {
        (self.test_targets || !is_test) && self.filter.is_included(label)
    }
}

/// InstrumentationFilterSupport.getInstrumentedPrefix
/// (buildtool/InstrumentationFilterSupport.java:126-140).
pub fn instrumented_prefix(package: &str) -> String {
    let package = package
        .strip_suffix("/internal")
        .or_else(|| package.strip_suffix("/public"))
        .or_else(|| package.strip_suffix("/tests"))
        .unwrap_or(package);
    let replace_first = |text: &str, from: &str, to: &str| -> String {
        // `(?<=^|/)from`: at the start or after a '/'.
        let mut search = 0;
        while let Some(found) = text[search..].find(from) {
            let at = search + found;
            if at == 0 || text.as_bytes()[at - 1] == b'/' {
                return format!("{}{to}{}", &text[..at], &text[at + from.len()..]);
            }
            search = at + 1;
        }
        text.to_owned()
    };
    let package = replace_first(package, "javatests/", "java/");
    replace_first(&package, "test/java/", "main/java/")
}

/// InstrumentationFilterSupport.computeInstrumentationFilter (:55-80) over
/// the packages of the tests to run (test suites expanded): the filter and
/// whether its INFO lines are printed (some package).
pub fn default_instrumentation_filter<'a>(
    packages: impl IntoIterator<Item = &'a str>,
) -> (String, bool) {
    let mut filters: BTreeSet<String> = packages.into_iter().map(instrumented_prefix).collect();
    // optimizeFilterSet (:142-158): drop filters nested in the previous kept
    // one (PathFragment.startsWith: whole segments; the empty fragment is a
    // prefix of every relative fragment).
    let mut kept: Vec<String> = Vec::new();
    for filter in std::mem::take(&mut filters) {
        let nested = kept.last().is_some_and(|previous| {
            previous.is_empty()
                || filter == *previous
                || filter
                    .strip_prefix(previous.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if !nested {
            kept.push(filter);
        }
    }
    let mut filter = format!("^//{}[/:]", kept.join("[/:],^//"));
    if filter == "^//[/:]" {
        filter = "^//".to_owned();
    }
    (filter, !kept.is_empty())
}

/// The INFO lines of the computed default (InstrumentationFilterSupport.java:72-78).
pub fn default_filter_notes(filter: &str) -> [String; 2] {
    [
        format!("INFO: Using default value for --instrumentation_filter: \"{filter}\"."),
        "INFO: Override the above default with --instrumentation_filter".to_owned(),
    ]
}

/// The coverage options of the command that are not configuration options
/// natively (BazelCoverageReportModule.Options, CoverageOptions'
/// generators and TestOptions' coverage flags).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `--combined_report=lcov|none` (BazelCoverageReportModule.java:46-55,
    /// default `lcov`).
    pub combined_report: bool,
    /// `--coverage_output_generator` (CoverageConfiguration.java:40-56), as
    /// given; `None` is the default, the native output generator.
    pub output_generator: Option<String>,
    /// `--coverage_report_generator` (:57-74); `None` is the default.
    pub report_generator: Option<String>,
    /// `--coverage_support` (TestConfiguration.java:325-340); `None` is the
    /// default `@bazel_tools//tools/test:coverage_support`.
    pub coverage_support: Option<String>,
    /// `--experimental_split_coverage_postprocessing` (TestConfiguration.java:366-376).
    pub split_postprocessing: bool,
    /// `--experimental_fetch_all_coverage_outputs` (TestConfiguration.java:342-352).
    pub fetch_all_outputs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            combined_report: true,
            output_generator: None,
            report_generator: None,
            coverage_support: None,
            split_postprocessing: false,
            fetch_all_outputs: false,
        }
    }
}

impl Settings {
    /// One coverage option of a building command: `Some(used_next)` when
    /// `arg` is one.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        for name in [
            "experimental_split_coverage_postprocessing",
            "experimental_fetch_all_coverage_outputs",
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                let value = value?;
                if name == "experimental_split_coverage_postprocessing" {
                    self.split_postprocessing = value;
                } else {
                    self.fetch_all_outputs = value;
                }
                return Ok(Some(false));
            }
        }
        for name in [
            "combined_report",
            "coverage_output_generator",
            "coverage_report_generator",
            "coverage_support",
        ] {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                (
                    next.ok_or_else(|| format!("Expected value after {flag}"))?,
                    true,
                )
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            match name {
                // ReportTypeConverter: an EnumConverter (case-insensitive).
                "combined_report" => {
                    self.combined_report = match value.to_ascii_lowercase().as_str() {
                        "lcov" => true,
                        "none" => false,
                        _ => {
                            return Err(format!(
                                "While parsing option {flag}={value}: Not a valid combined coverage report type: '{value}' (should be none or lcov)"
                            ));
                        }
                    }
                }
                "coverage_output_generator" => self.output_generator = Some(value.to_owned()),
                "coverage_report_generator" => self.report_generator = Some(value.to_owned()),
                _ => self.coverage_support = Some(value.to_owned()),
            }
            return Ok(Some(used));
        }
        Ok(None)
    }
}

impl Settings {
    /// The coverage options a native coverage build cannot honour fail the
    /// command (docs/COVERAGE.md): other output/report generators and
    /// coverage support (they would be analyzed and run as tools) and the
    /// split post-processing spawn.
    pub fn check_native(&self) -> Result<(), String> {
        for (name, value) in [
            ("coverage_output_generator", &self.output_generator),
            ("coverage_report_generator", &self.report_generator),
            ("coverage_support", &self.coverage_support),
        ] {
            if let Some(value) = value {
                return Err(format!(
                    "--{name}={value} is not implemented in the native profile: coverage uses the native coverage output generator in place of @remote_coverage_tools (docs/COVERAGE.md)"
                ));
            }
        }
        if self.split_postprocessing {
            return Err(
                "--experimental_split_coverage_postprocessing is not implemented in the native profile (docs/COVERAGE.md)"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Record the command's coverage settings (once, before analysis).
pub fn install(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

/// The command's coverage settings (the defaults when none were installed).
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(Settings::default)
}

/// The configured value of the late-bound `:lcov_merger`
/// (`configuration_field("coverage", "output_generator")`) in a coverage
/// configuration: `None` is the default generator, which the native
/// output generator replaces (no dependency edge).
pub fn explicit_output_generator() -> Option<&'static str> {
    settings().output_generator.as_deref()
}

// ---------------------------------------------------------------------------
// The native coverage output generator (CoverageOutputGenerator's Main).

/// One source file's coverage (SourceFileCoverage.java).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SourceFile {
    name: String,
    /// Function name to its start line (FN).
    function_lines: BTreeMap<String, i32>,
    /// Function name to its execution count (FNDA).
    function_executions: BTreeMap<String, i64>,
    /// Instrumented line to its execution count (DA).
    lines: BTreeMap<i32, i64>,
    /// (line, block, branch) to its execution count (BRDA/BA).
    branches: BTreeMap<(i32, String, String), i64>,
    /// The lines with an evaluated branch (BranchCoverage.evaluatedData).
    evaluated: BTreeSet<i32>,
}

impl SourceFile {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// SourceFileCoverage.merge (:84-97).
    fn add(&mut self, other: &SourceFile) {
        for (name, line) in &other.function_lines {
            self.function_lines.insert(name.clone(), *line);
        }
        for (name, count) in &other.function_executions {
            *self.function_executions.entry(name.clone()).or_default() += count;
        }
        for (key, count) in &other.branches {
            *self.branches.entry(key.clone()).or_default() += count;
        }
        self.evaluated.extend(other.evaluated.iter().copied());
        for (line, count) in &other.lines {
            *self.lines.entry(*line).or_default() += count;
        }
    }

    fn add_branch(&mut self, line: i32, block: &str, branch: &str, evaluated: bool, count: i64) {
        if evaluated {
            self.evaluated.insert(line);
        }
        *self
            .branches
            .entry((line, block.to_owned(), branch.to_owned()))
            .or_default() += count;
    }
}

/// Coverage.java: the source files by name (a TreeMap; a renamed source
/// keeps its key, Coverage.maybeReplaceSourceFileNames).
#[derive(Debug, Default)]
struct Coverage {
    files: BTreeMap<String, SourceFile>,
}

impl Coverage {
    fn add(&mut self, file: SourceFile) {
        match self.files.get_mut(&file.name) {
            Some(existing) => existing.add(&file),
            None => {
                self.files.insert(file.name.clone(), file);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// A logged warning of the generator (java.util.logging's WARNING level on
/// stderr; the timestamp header line is not reproduced).
fn warning(message: &str) {
    eprintln!("WARNING: {message}");
}

fn severe(message: &str) {
    eprintln!("SEVERE: {message}");
}

/// Java's Integer.parseInt / Long.parseLong (an optional sign, ASCII digits).
fn java_int<T: std::str::FromStr>(text: &str) -> Option<T> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// BufferedReader.readLine: lines end at `\n`, `\r` or `\r\n`.
fn read_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                start = index + 1;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// LcovParser.parse (LcovParser.java:63-430): the completed records of one
/// tracefile.
fn parse_lcov(text: &str) -> Result<Vec<SourceFile>, String> {
    let mut all = Vec::new();
    let mut current: Option<SourceFile> = None;
    let mut ba_branches_at_line = 0;
    let mut last_ba_line = -1;
    for line in read_lines(text) {
        if let Some(name) = line.strip_prefix("SF:") {
            last_ba_line = -1;
            if current.is_some() {
                warning(&format!(
                    "Tracefile doesn't have SF:<source file> line before{line}"
                ));
            } else if name.is_empty() {
                warning(&format!(
                    "Tracefile doesn't contain source file name on line: {line}"
                ));
            } else {
                current = Some(SourceFile::new(name));
            }
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        let split = |content: &str| content.split(',').map(str::to_owned).collect::<Vec<_>>();
        if let Some(content) = line.strip_prefix("FN:") {
            let data = split(content);
            if data.len() < 2
                || data.len() > 3
                || (data.len() == 3 && data[2].is_empty())
                || data[0].is_empty()
                || data[1].is_empty()
            {
                warning(&format!("Tracefile contains invalid FN line {line}"));
                continue;
            }
            match java_int::<i32>(&data[0]) {
                Some(start) => {
                    file.function_lines
                        .insert(data[data.len() - 1].clone(), start);
                }
                None => warning(&format!(
                    "Tracefile contains invalid line number on FN line {line}"
                )),
            }
        } else if let Some(content) = line.strip_prefix("FNDA:") {
            let data = split(content);
            if data.len() != 2 || data[0].is_empty() || data[1].is_empty() {
                warning(&format!("Tracefile contains invalid FNDA line {line}"));
                continue;
            }
            match java_int::<i64>(&data[0]) {
                Some(count) => {
                    *file.function_executions.entry(data[1].clone()).or_default() += count;
                }
                None => warning(&format!(
                    "Tracefile contains invalid execution count on FN line {line}"
                )),
            }
        } else if let Some((marker, content)) = ["FNF:", "FNH:", "BRF:", "BRH:", "LH:", "LF:"]
            .iter()
            .find_map(|marker| line.strip_prefix(marker).map(|rest| (*marker, rest)))
        {
            // Totals are recomputed when printing (the asserts are off).
            let what = match marker {
                "FNF:" => "invalid FNF line",
                "FNH:" => "invalid FNH line",
                "BRF:" => "invalid BRF line",
                "BRH:" => "invalid BRH line",
                "LH:" => "invalid LHL line",
                _ => "invalid LF line",
            };
            if content.is_empty() {
                warning(&format!("Tracefile contains {what} {line}"));
            } else if java_int::<i32>(content).is_none() {
                warning(&format!("Tracefile contains an invalid number on {line}"));
            }
        } else if let Some(content) = line.strip_prefix("BRDA:") {
            let data = split(content);
            if data.len() != 4 || data.iter().any(String::is_empty) {
                warning(&format!("Tracefile contains invalid BRDA line {line}"));
                continue;
            }
            let Some(number) = java_int::<i32>(&data[0]) else {
                warning(&format!(
                    "Tracefile contains an invalid number BRDA line {line}"
                ));
                continue;
            };
            let (count, evaluated) = if data[3] == "-" {
                (0, false)
            } else {
                match java_int::<i64>(&data[3]) {
                    Some(count) => (count, true),
                    None => {
                        warning(&format!(
                            "Tracefile contains an invalid number BRDA line {line}"
                        ));
                        continue;
                    }
                }
            };
            file.add_branch(number, &data[1], &data[2], evaluated, count);
        } else if let Some(content) = line.strip_prefix("BA:") {
            let data = split(content);
            if data.len() != 2 || data.iter().any(String::is_empty) {
                warning(&format!("Tracefile contains invalid BA line {line}"));
                continue;
            }
            let (Some(number), Some(value)) =
                (java_int::<i32>(&data[0]), java_int::<i32>(&data[1]))
            else {
                warning(&format!(
                    "Tracefile contains an invalid number BA line {line}"
                ));
                continue;
            };
            let (evaluated, count) = match value {
                0 => (false, 0),
                1 => (true, 0),
                2 => (true, 1),
                _ => {
                    warning(&format!(
                        "Tracefile contains invalid BA {line} - value not one of {{0, 1, 2}}"
                    ));
                    continue;
                }
            };
            if last_ba_line == number {
                ba_branches_at_line += 1;
            } else {
                ba_branches_at_line = 0;
                last_ba_line = number;
            }
            file.add_branch(
                number,
                "0",
                &ba_branches_at_line.to_string(),
                evaluated,
                count,
            );
        } else if let Some(content) = line.strip_prefix("DA:") {
            let data = split(content);
            if (data.len() != 2 && data.len() != 3) || data.iter().any(String::is_empty) {
                warning(&format!("Tracefile contains invalid DA line {line}"));
                continue;
            }
            match (java_int::<i32>(&data[0]), java_int::<i64>(&data[1])) {
                (Some(number), Some(count)) => {
                    // LineCoverage.addLine (:66-77).
                    if number < 0 {
                        return Err("Line number must be non-negative.".to_owned());
                    }
                    *file.lines.entry(number).or_default() += count;
                }
                _ => warning(&format!(
                    "Tracefile contains an invalid number on DA line {line}"
                )),
            }
        } else if line == "end_of_record" {
            all.push(current.take().expect("a current record"));
        } else {
            warning(&format!("Tracefile includes invalid line: {line}"));
        }
    }
    Ok(all)
}

/// LcovPrinter.print (LcovPrinter.java:52-207).
fn print_lcov(coverage: &Coverage, legacy_branches: bool) -> String {
    let mut out = String::new();
    for file in coverage.files.values() {
        out.push_str(&format!("SF:{}\n", file.name));
        for (name, line) in &file.function_lines {
            out.push_str(&format!("FN:{line},{name}\n"));
        }
        for (name, count) in &file.function_executions {
            out.push_str(&format!("FNDA:{count},{name}\n"));
        }
        out.push_str(&format!("FNF:{}\n", file.function_executions.len()));
        out.push_str(&format!(
            "FNH:{}\n",
            file.function_executions
                .values()
                .filter(|count| **count > 0)
                .count()
        ));
        for ((line, block, branch), count) in &file.branches {
            let evaluated = file.evaluated.contains(line);
            if legacy_branches {
                let value = match (evaluated, *count > 0) {
                    (false, _) => "0",
                    (true, true) => "2",
                    (true, false) => "1",
                };
                out.push_str(&format!("BA:{line},{value}\n"));
            } else if evaluated {
                out.push_str(&format!("BRDA:{line},{block},{branch},{count}\n"));
            } else {
                out.push_str(&format!("BRDA:{line},{block},{branch},-\n"));
            }
        }
        if !file.branches.is_empty() {
            out.push_str(&format!("BRF:{}\n", file.branches.len()));
            out.push_str(&format!(
                "BRH:{}\n",
                file.branches.values().filter(|count| **count > 0).count()
            ));
        }
        for (line, count) in &file.lines {
            out.push_str(&format!("DA:{line},{count}\n"));
        }
        out.push_str(&format!(
            "LH:{}\n",
            file.lines.values().filter(|count| **count > 0).count()
        ));
        out.push_str(&format!("LF:{}\n", file.lines.len()));
        out.push_str("end_of_record\n");
    }
    out
}

/// LcovMergerFlags (LcovMergerFlags.java): JCommander with `=` or a space
/// as separators, unknown options accepted, later values overwrite.
#[derive(Debug, Default)]
pub struct Flags {
    pub coverage_dir: Option<String>,
    pub reports_file: Option<String>,
    pub output_file: Option<String>,
    pub filter_sources: Vec<String>,
    pub source_file_manifest: Option<String>,
    pub sources_to_replace_file: Option<String>,
    pub legacy_branches: bool,
}

impl Flags {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut flags = Flags::default();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            index += 1;
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            if name == "--legacy_branches" {
                flags.legacy_branches = true;
                continue;
            }
            let known = [
                "--coverage_dir",
                "--reports_file",
                "--lcovfile_path",
                "--output_file",
                "--filter_sources",
                "--source_file_manifest",
                "--sources_to_replace_file",
                "--parse_parallelism",
            ];
            if !known.contains(&name) {
                continue;
            }
            let value = match inline {
                Some(value) => value,
                None => {
                    let value = args
                        .get(index)
                        .cloned()
                        .ok_or_else(|| "Error parsing args".to_owned())?;
                    index += 1;
                    value
                }
            };
            match name {
                "--coverage_dir" => flags.coverage_dir = Some(value),
                "--reports_file" | "--lcovfile_path" => flags.reports_file = Some(value),
                "--output_file" => flags.output_file = Some(value),
                "--filter_sources" => flags.filter_sources.push(value),
                "--source_file_manifest" => flags.source_file_manifest = Some(value),
                "--sources_to_replace_file" => flags.sources_to_replace_file = Some(value),
                _ => {}
            }
        }
        if flags.coverage_dir.is_none() && flags.reports_file.is_none() {
            return Err("At least one of coverage_dir or reports_file should be specified.".into());
        }
        if flags.coverage_dir.is_some() && flags.reports_file.is_some() {
            warning("Overriding --coverage_dir value in favor of --reports_file");
        }
        if flags.output_file.is_none() {
            return Err("output_file was not specified.".into());
        }
        Ok(flags)
    }
}

/// Main.getCoverageFilesInDir (Main.java:318-336): every path below `dir`
/// ending in a coverage extension.
fn coverage_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    fn walk(path: &Path, files: &mut Vec<PathBuf>) {
        let text = path.to_string_lossy();
        if [".dat", ".gcov", ".gcov.json.gz", ".profdata"]
            .iter()
            .any(|extension| text.ends_with(extension))
        {
            files.push(path.to_path_buf());
        }
        if path.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            let mut entries: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
            entries.sort();
            for entry in entries {
                walk(&entry, files);
            }
        }
    }
    if dir.exists() {
        walk(dir, &mut files);
    } else {
        severe(&format!(
            "Error reading folder {}: {}",
            dir.display(),
            dir.display()
        ));
    }
    files
}

/// Main.runWithArgs (Main.java:61-178), relative paths against `cwd`: the
/// exit code.
pub fn run(flags: &Flags, cwd: &Path) -> i32 {
    match run_inner(flags, cwd) {
        Ok(code) => code,
        Err(error) => {
            severe(&format!("Unhandled exception on lcov tool: {error}"));
            1
        }
    }
}

fn run_inner(flags: &Flags, cwd: &Path) -> Result<i32, String> {
    run_resolved(flags, cwd, &|path: &str| cwd.join(path))
}

fn run_resolved(
    flags: &Flags,
    cwd: &Path,
    resolve_tracefile: &dyn Fn(&str) -> PathBuf,
) -> Result<i32, String> {
    let resolve = |path: &str| cwd.join(path);
    let output = resolve(flags.output_file.as_deref().expect("validated"));
    let in_dir = match &flags.coverage_dir {
        Some(dir) => coverage_files_in_dir(&resolve(dir)),
        None => Vec::new(),
    };
    let with_extension = |extension: &str| -> Vec<&PathBuf> {
        in_dir
            .iter()
            .filter(|file| file.to_string_lossy().ends_with(extension))
            .collect()
    };
    // getTracefiles (:268-284).
    let tracefiles: Vec<PathBuf> = match &flags.reports_file {
        Some(reports) => match std::fs::read_to_string(resolve(reports)) {
            Ok(text) => read_lines(&text)
                .into_iter()
                .map(resolve_tracefile)
                .collect(),
            Err(error) => {
                severe(&format!("Error reading file {reports}: {error}"));
                Vec::new()
            }
        },
        None => with_extension(".dat").into_iter().cloned().collect(),
    };
    if !with_extension(".gcov").is_empty() || !with_extension(".gcov.json.gz").is_empty() {
        return Err(
            "gcov coverage files are not supported by the native coverage output generator (C/C++ coverage: docs/COVERAGE.md)"
                .to_owned(),
        );
    }
    let mut coverage = Coverage::default();
    for file in &tracefiles {
        let text = match std::fs::read(file) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                severe(&format!(
                    "File {} could not be parsed due to: {error}",
                    file.display()
                ));
                return Ok(1);
            }
        };
        for source in parse_lcov(&text)? {
            coverage.add(source);
        }
    }
    // maybeReplaceSourceFileNames (Coverage.java:82-96; getMapFromFile, Main.java:286-305).
    if let Some(file) = &flags.sources_to_replace_file {
        let mut map = BTreeMap::new();
        match std::fs::read_to_string(resolve(file)) {
            Ok(text) => {
                for line in read_lines(&text) {
                    let mut parts: Vec<&str> = line.split(':').collect();
                    while parts.last() == Some(&"") {
                        parts.pop();
                    }
                    if parts.len() == 2 {
                        if map.contains_key(parts[0]) {
                            return Err(format!("Multiple entries with same key: {}", parts[0]));
                        }
                        map.insert(parts[0].to_owned(), parts[1].to_owned());
                    }
                }
            }
            Err(error) => severe(&format!("Error reading file {file}: {error}")),
        }
        for source in coverage.files.values_mut() {
            if let Some(name) = map.get(&source.name) {
                source.name = name.clone();
            }
        }
    }
    // getProfdataFileOrNull (:236-258).
    let profdata = {
        let files = with_extension(".profdata");
        match files.len() {
            0 => None,
            1 => Some(files[0].clone()),
            count => {
                severe(&format!(
                    "Bazel currently supports only one profdata file per test. {count} .profadata files were found instead."
                ));
                None
            }
        }
    };
    let empty_output = |output: &Path| -> i32 {
        warning("There was no coverage found.");
        if !output.exists()
            && let Err(error) = std::fs::write(output, b"")
        {
            severe(&format!(
                "Could not create empty output file {} due to: {error}",
                output
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            return 1;
        }
        0
    };
    if coverage.is_empty() {
        return Ok(match profdata {
            None => empty_output(&output),
            Some(profdata) => match std::fs::copy(&profdata, &output) {
                Ok(_) => 0,
                Err(error) => {
                    severe(&format!(
                        "Could not copy file {} to output file {} due to: {error}",
                        profdata.display(),
                        output.display()
                    ));
                    1
                }
            },
        });
    }
    if profdata.is_some() {
        warning("Bazel doesn't support LLVM profdata coverage amongst other coverage formats.");
        return Ok(0);
    }
    // filterOutMatchingSources (Coverage.java:98-124): String.matches is a
    // whole-string match.
    if !flags.filter_sources.is_empty() {
        let regexes = flags
            .filter_sources
            .iter()
            .map(|regex| {
                fancy_regex::Regex::new(&format!("^(?:{regex})$"))
                    .map_err(|error| format!("invalid --filter_sources {regex}: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut filtered = Coverage::default();
        for source in std::mem::take(&mut coverage.files).into_values() {
            if !regexes
                .iter()
                .any(|regex| regex.is_match(&source.name).unwrap_or(false))
            {
                filtered.add(source);
            }
        }
        coverage = filtered;
    }
    // getOnlyTheseSources (Coverage.java:53-80; getSourcesFromSourceFileManifest,
    // Main.java:189-210: metadata files .gcno/.em are skipped).
    if let Some(manifest) = &flags.source_file_manifest {
        let mut keep = BTreeSet::new();
        match std::fs::read_to_string(resolve(manifest)) {
            Ok(text) => {
                for line in read_lines(&text) {
                    if !line.ends_with(".gcno") && !line.ends_with(".em") {
                        keep.insert(line.to_owned());
                    }
                }
            }
            Err(error) => severe(&format!("Error reading file {manifest}: {error}")),
        }
        let mut kept = Coverage::default();
        if !keep.is_empty() {
            for source in std::mem::take(&mut coverage.files).into_values() {
                if keep.contains(&source.name) {
                    kept.add(source);
                }
            }
        }
        coverage = kept;
    }
    if coverage.is_empty() {
        return Ok(empty_output(&output));
    }
    if let Err(error) = std::fs::write(&output, print_lcov(&coverage, flags.legacy_branches)) {
        severe(&format!(
            "Could not write to output file {} due to {error}",
            output.display()
        ));
        return Ok(1);
    }
    Ok(0)
}

/// The combined LCOV report of a coverage build (`--combined_report=lcov`;
/// CoverageReportActionBuilder.createCoverageActionsWrapper,
/// bazel/coverage/CoverageReportActionBuilder.java:166-262, with
/// BazelCoverageReportModule's helper): `bazel-out/_coverage/` below the
/// execution root gets `baseline_lcov_files.tmp` and `coverage_lcov_files.tmp`
/// (the exec paths, one per line: the baselines, then the tests'
/// coverage.dat) and the reports the native generator merges from them,
/// `_baseline_report.dat` and `_coverage_report.dat`. Returns the INFO
/// lines (CoverageReportAction.execute's location message) or the
/// generator's failure.
pub fn combined_report(
    execution_root: &Path,
    coverage: &[String],
    baselines: &[(String, PathBuf)],
) -> Result<Vec<String>, String> {
    if coverage.is_empty() && baselines.is_empty() {
        return Err(
            "Cannot generate coverage report - no coverage information was collected".into(),
        );
    }
    let directory = execution_root.join("bazel-out/_coverage");
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let published: BTreeMap<&str, &PathBuf> = baselines
        .iter()
        .map(|(exec, path)| (exec.as_str(), path))
        .collect();
    let resolve = |exec: &str| -> PathBuf {
        published
            .get(exec)
            .map(|path| (*path).clone())
            .unwrap_or_else(|| execution_root.join(exec))
    };
    let baseline_paths: Vec<String> = baselines.iter().map(|(exec, _)| exec.clone()).collect();
    let mut all = baseline_paths.clone();
    all.extend(coverage.iter().cloned());
    let mut notes = Vec::new();
    for (list, name, paths) in [
        (
            "baseline_lcov_files.tmp",
            "_baseline_report.dat",
            &baseline_paths,
        ),
        ("coverage_lcov_files.tmp", "_coverage_report.dat", &all),
    ] {
        let list_path = directory.join(list);
        let output = directory.join(name);
        for path in [&list_path, &output] {
            if let Ok(metadata) = std::fs::metadata(path) {
                let mut permissions = metadata.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(path, permissions);
                let _ = std::fs::remove_file(path);
            }
        }
        let mut content = String::new();
        for path in paths.iter() {
            content.push_str(path);
            content.push('\n');
        }
        std::fs::write(&list_path, content)
            .map_err(|error| format!("cannot write {}: {error}", list_path.display()))?;
        let flags = Flags {
            reports_file: Some(format!("bazel-out/_coverage/{list}")),
            output_file: Some(format!("bazel-out/_coverage/{name}")),
            ..Flags::default()
        };
        let code = run_resolved(&flags, execution_root, &resolve).unwrap_or_else(|error| {
            severe(&format!("Unhandled exception on lcov tool: {error}"));
            1
        });
        if code != 0 {
            return Err(format!(
                "Coverage report generation failed: the native coverage report generator exited with {code}"
            ));
        }
        notes.push(format!(
            "INFO: LCOV coverage report is located at {}\n and execpath is bazel-out/_coverage/{name}",
            output.display()
        ));
    }
    Ok(notes)
}

/// The `__coverage-output-generator` subcommand: Main.main (Main.java:48-58).
pub fn output_generator_main(args: &[String]) -> i32 {
    let flags = match Flags::parse(args) {
        Ok(flags) => flags,
        Err(error) => {
            severe(&error);
            return 1;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    run(&flags, &cwd)
}

/// The launcher script staged as a coverage test action's `LCOV_MERGER`:
/// it runs this binary's native coverage output generator.
pub fn launcher_script(binary: &Path) -> String {
    let quoted = binary.to_string_lossy().replace('\'', "'\\''");
    format!("#!/bin/sh\nexec '{quoted}' {OUTPUT_GENERATOR_ARG} \"$@\"\n")
}

/// The exec path of the native `LCOV_MERGER` launcher in a coverage test
/// action's execution root.
pub const LAUNCHER_EXEC_PATH: &str =
    "external/bazel_tools/tools/test/bazel-rs-coverage-output-generator";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_filter_follows_bazel() {
        let filter = RegexFilter::parse(DEFAULT_INSTRUMENTATION_FILTER).unwrap();
        assert!(filter.is_included("//go/lib:lib"));
        assert!(!filter.is_included("//a/javatests/b:c"));
        assert!(!filter.is_included("//x/test/java:y"));
        assert_eq!(
            filter.to_string(),
            "-(?:(?>/javatests[/:])|(?>/test/java[/:]))"
        );
        let filter = RegexFilter::parse("^//go/lib[/:],^//sh[/:]").unwrap();
        assert!(filter.is_included("//go/lib:lib"));
        assert!(filter.is_included("//go/lib/sub:x"));
        assert!(!filter.is_included("//go/library:x"));
        assert!(!filter.is_included("@@rules_go+//go/lib:x"));
        let filter = RegexFilter::parse("+a\\,b,-c").unwrap();
        assert!(filter.is_included("xa,by"));
        assert!(!filter.is_included("a,bc"));
        assert!(
            RegexFilter::parse("--x")
                .unwrap_err()
                .contains("looks like another flag")
        );
        assert!(RegexFilter::parse("(").is_err());
        assert!(RegexFilter::parse("").unwrap().is_included("anything"));
    }

    #[test]
    fn default_filter_heuristic() {
        assert_eq!(
            default_instrumentation_filter(["go/lib", "sh"]),
            ("^//go/lib[/:],^//sh[/:]".to_owned(), true)
        );
        assert_eq!(
            default_instrumentation_filter(["a/b", "a", "a/bc/tests", "java/x/internal"]).0,
            "^//a[/:],^//java/x[/:]"
        );
        assert_eq!(default_instrumentation_filter([""]).0, "^//");
        assert_eq!(default_instrumentation_filter(["", "x"]).0, "^//");
        assert_eq!(
            default_instrumentation_filter(Vec::<&str>::new()),
            ("^//".to_owned(), false)
        );
        assert_eq!(instrumented_prefix("javatests/foo"), "java/foo");
        assert_eq!(
            instrumented_prefix("a/src/test/java/b"),
            "a/src/main/java/b"
        );
        assert_eq!(instrumented_prefix("xjavatests/foo"), "xjavatests/foo");
    }

    #[test]
    fn lcov_merge_and_print_follow_the_generator() {
        let a = "SF:b.go\nFN:3,f\nFNDA:1,f\nDA:3,1\nDA:4,0\nBRDA:3,0,0,1\nBRDA:3,0,1,-\nend_of_record\nSF:a.go\nDA:1,2\nend_of_record\nSF:dropped.go\nDA:1,1\n";
        let b = "SF:b.go\nDA:4,2\nDA:5,0\nFNDA:0,g\nFN:7,g\nend_of_record\n";
        let mut coverage = Coverage::default();
        for text in [a, b] {
            for file in parse_lcov(text).unwrap() {
                coverage.add(file);
            }
        }
        assert_eq!(
            print_lcov(&coverage, false),
            "SF:a.go\nFNF:0\nFNH:0\nDA:1,2\nLH:1\nLF:1\nend_of_record\nSF:b.go\nFN:3,f\nFN:7,g\nFNDA:1,f\nFNDA:0,g\nFNF:2\nFNH:1\nBRDA:3,0,0,1\nBRDA:3,0,1,0\nBRF:2\nBRH:1\nDA:3,1\nDA:4,2\nDA:5,0\nLH:2\nLF:3\nend_of_record\n"
        );
    }

    #[test]
    fn merger_filters_by_manifest_and_writes_empty_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("cov/sub")).unwrap();
        std::fs::write(
            root.join("cov/sub/go.dat"),
            "SF:go/lib/lib.go\nDA:3,1\nend_of_record\nSF:/usr/lib/x.c\nDA:1,1\nend_of_record\nSF:other.go\nDA:1,1\nend_of_record\n",
        )
        .unwrap();
        std::fs::write(root.join("manifest"), "go/lib/lib.go\nother.gcno\n").unwrap();
        let args = |rest: &[&str]| -> Vec<String> { rest.iter().map(|s| s.to_string()).collect() };
        let flags = Flags::parse(&args(&[
            "--coverage_dir=cov",
            "--output_file=out.dat",
            "--filter_sources=/usr/lib/.+",
            "--source_file_manifest=manifest",
        ]))
        .unwrap();
        assert_eq!(run(&flags, root), 0);
        assert_eq!(
            std::fs::read_to_string(root.join("out.dat")).unwrap(),
            "SF:go/lib/lib.go\nFNF:0\nFNH:0\nDA:3,1\nLH:1\nLF:1\nend_of_record\n"
        );
        std::fs::write(root.join("empty_manifest"), "").unwrap();
        let flags = Flags::parse(&args(&[
            "--coverage_dir",
            "cov",
            "--output_file",
            "empty.dat",
            "--source_file_manifest=empty_manifest",
        ]))
        .unwrap();
        assert_eq!(run(&flags, root), 0);
        assert_eq!(std::fs::read(root.join("empty.dat")).unwrap(), b"");
        assert!(Flags::parse(&args(&["--output_file=x"])).is_err());
        assert!(Flags::parse(&args(&["--coverage_dir=x"])).is_err());
    }
}
