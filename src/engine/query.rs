//! `bazel query`: evaluation of the Bazel 9.3 query language over the
//! unconfigured target graph, and its output formatters.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (Release 9.3.0); paths
//! are relative to `src/main/java/com/google/devtools/build/lib/`. The
//! command is runtime/commands/QueryCommand.java and
//! QueryEnvironmentBasedCommand.java; evaluation follows
//! query2/query/GraphlessBlazeQueryEnvironment.java (streamed, unordered
//! formatters) and query2/query/BlazeQueryEnvironment.java (ordered
//! formatters: a Digraph of every visited target), with the visitation of
//! query2/query/LabelVisitor.java, query2/query/PathLabelVisitor.java and
//! packages/LabelVisitationUtils.java. Output formatters are in
//! query2/query/output/. See docs/QUERY.md for the supported surface.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

mod formats;

pub use formats::FormatOptions;

use super::query_attrs::{self, QueryAttribute, Shape};
use super::{Engine, Target};
use crate::custom_rule::{AttrValue, ConfigurablePart};
use crate::label::{Label, PackageId};
use crate::package::{Package, Rule, RuleKind};
use crate::query_language::{self as language, Argument, Expr, Function, Operator};
use crate::visibility::Visibility;

/// QueryOptions.OrderOutput (query2/query/output/QueryOptions.java:79-92).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderOutput {
    No,
    Deps,
    Auto,
    Full,
}

/// The `query` command options this profile models (QueryOptions.java,
/// CommonQueryOptions.java, KeepGoingOption). Everything else is rejected.
#[derive(Clone, Debug)]
pub struct QueryOptions {
    pub output: String,
    pub order_output: OrderOutput,
    pub keep_going: bool,
    pub implicit_deps: bool,
    pub tool_deps: bool,
    pub nodep_deps: bool,
    pub strict_test_suite: bool,
    pub line_terminator_null: bool,
    pub relative_locations: bool,
    pub consistent_labels: bool,
    pub lexicographical_output: bool,
    pub graph_factored: bool,
    pub graph_node_limit: i32,
    pub graph_conditional_edges_limit: i32,
    pub query_file: String,
    pub output_file: String,
    pub graphless: Option<bool>,
    /// `--loading_phase_threads` (LoadingPhaseThreadsOption.java): "auto"
    /// (the default) or a positive integer. Both loaders' pattern
    /// expansion parallelizes package loading with it (P2,
    /// docs/PARALLEL_ANALYSIS.md). `None` = "auto" (unset or spelled
    /// `auto`): main expands it through the memory-cap bound
    /// (crate::memory_limit::bound_auto_loading_threads); an explicit
    /// integer is kept exactly as given.
    pub loading_phase_threads: Option<usize>,
    /// The xml/proto formatter options.
    pub format: FormatOptions,
    /// `--universe_scope` (a comma-separated pattern list).
    pub universe_scope: Vec<String>,
    /// `--infer_universe_scope`.
    pub infer_universe_scope: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            output: "label".to_owned(),
            order_output: OrderOutput::Auto,
            keep_going: false,
            implicit_deps: true,
            tool_deps: true,
            nodep_deps: true,
            strict_test_suite: false,
            line_terminator_null: false,
            relative_locations: false,
            consistent_labels: false,
            lexicographical_output: true,
            graph_factored: true,
            graph_node_limit: 512,
            graph_conditional_edges_limit: 4,
            query_file: String::new(),
            output_file: String::new(),
            graphless: None,
            loading_phase_threads: None,
            format: FormatOptions::default(),
            universe_scope: Vec::new(),
            infer_universe_scope: false,
        }
    }
}

/// OutputFormatters.getDefaultFormatters order (OutputFormatters.java:31-45).
const FORMATTERS: &[&str] = &[
    "label",
    "label_kind",
    "build",
    "minrank",
    "maxrank",
    "package",
    "location",
    "graph",
    "xml",
    "proto",
    "streamed_jsonproto",
    "streamed_proto",
];

/// StreamedFormatter implementations (AbstractUnorderedFormatter subclasses
/// and the streamed proto formatters).
fn is_streamed(output: &str) -> bool {
    matches!(
        output,
        "label"
            | "label_kind"
            | "build"
            | "package"
            | "location"
            | "xml"
            | "proto"
            | "streamed_jsonproto"
            | "streamed_proto"
    )
}

fn boolean(arg: &str, name: &str) -> Option<Result<bool, String>> {
    crate::configuration::boolean_option(arg, name)
}

fn int_value(name: &str, value: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("While parsing option --{name}={value}: '{value}' is not an int"))
}

impl QueryOptions {
    /// Parse one query option; `Ok(Some(true))` when `next` was consumed,
    /// `Ok(None)` when `arg` is not a query option.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        let valued = |name: &str| -> Option<(Option<String>, bool)> {
            if let Some(value) = arg.strip_prefix(&format!("--{name}=")) {
                Some((Some(value.to_owned()), false))
            } else if arg == format!("--{name}") {
                Some((next.map(str::to_owned), true))
            } else {
                None
            }
        };
        for name in [
            "output",
            "order_output",
            "graph:node_limit",
            "graph:conditional_edges_limit",
            "query_file",
            "output_file",
            "aspect_deps",
            "universe_scope",
            "experimental_graphless_query",
            "loading_phase_threads",
            "proto:output_rule_attrs",
        ] {
            let Some((value, consumed)) = valued(name) else {
                continue;
            };
            let value = value.ok_or_else(|| format!("Expected value after --{name}"))?;
            match name {
                "output" => self.output = value,
                "order_output" => {
                    self.order_output = match value.to_ascii_lowercase().as_str() {
                        "no" => OrderOutput::No,
                        "deps" => OrderOutput::Deps,
                        "auto" => OrderOutput::Auto,
                        "full" => OrderOutput::Full,
                        _ => {
                            return Err(format!(
                                "While parsing option --order_output={value}: Not a valid order output setting: '{value}' (should be no, deps, auto or full)"
                            ));
                        }
                    }
                }
                "graph:node_limit" => self.graph_node_limit = int_value(name, &value)?,
                "graph:conditional_edges_limit" => {
                    self.graph_conditional_edges_limit = int_value(name, &value)?
                }
                "query_file" => self.query_file = value,
                "output_file" => self.output_file = value,
                // Read by the xml/proto formatters (CommonQueryOptions.java aspect_deps).
                "aspect_deps" => {
                    if !matches!(value.as_str(), "off" | "conservative" | "precise") {
                        return Err(format!(
                            "While parsing option --aspect_deps={value}: Not a valid aspect resolution mode: '{value}' (should be off, conservative or precise)"
                        ));
                    }
                    self.format.aspect_deps = value;
                }
                // CommaSeparatedOptionListConverter.
                "proto:output_rule_attrs" => {
                    self.format.proto_output_rule_attrs = if value.is_empty() {
                        Vec::new()
                    } else {
                        value.split(',').map(str::to_owned).collect()
                    };
                }
                "universe_scope" => {
                    self.universe_scope = if value.is_empty() {
                        Vec::new()
                    } else {
                        value.split(',').map(str::to_owned).collect()
                    };
                }
                "experimental_graphless_query" => {
                    self.graphless = match value.as_str() {
                        "auto" => None,
                        "yes" | "true" | "1" => Some(true),
                        "no" | "false" | "0" => Some(false),
                        _ => {
                            return Err(format!(
                                "While parsing option --experimental_graphless_query={value}: '{value}' is not one of {{auto, yes, no}}"
                            ));
                        }
                    }
                }
                "loading_phase_threads" => {
                    self.loading_phase_threads = if value == "auto" {
                        None
                    } else {
                        Some(
                            value
                                .parse::<usize>()
                                .ok()
                                .filter(|n| *n > 0)
                                .ok_or_else(|| {
                                    format!("While parsing option --loading_phase_threads={value}: not a positive integer or 'auto'")
                                })?,
                        )
                    }
                }
                _ => unreachable!(),
            }
            return Ok(Some(consumed));
        }
        if arg == "-k" {
            self.keep_going = true;
            return Ok(Some(false));
        }
        if arg == "--null" {
            self.line_terminator_null = true;
            return Ok(Some(false));
        }
        if arg == "--order_results" {
            self.order_output = OrderOutput::Auto;
            return Ok(Some(false));
        }
        if arg == "--noorder_results" {
            self.order_output = OrderOutput::No;
            return Ok(Some(false));
        }
        // `host_deps` is the old name of `tool_deps` (CommonQueryOptions.java).
        if let Some(value) = boolean(arg, "host_deps") {
            self.tool_deps = value?;
            return Ok(Some(false));
        }
        let flags: [(&str, &mut bool); 10] = [
            ("keep_going", &mut self.keep_going),
            ("implicit_deps", &mut self.implicit_deps),
            ("tool_deps", &mut self.tool_deps),
            ("nodep_deps", &mut self.nodep_deps),
            ("strict_test_suite", &mut self.strict_test_suite),
            ("line_terminator_null", &mut self.line_terminator_null),
            ("relative_locations", &mut self.relative_locations),
            ("consistent_labels", &mut self.consistent_labels),
            (
                "incompatible_lexicographical_output",
                &mut self.lexicographical_output,
            ),
            ("graph:factored", &mut self.graph_factored),
        ];
        for (name, slot) in flags {
            if let Some(value) = boolean(arg, name) {
                *slot = value?;
                return Ok(Some(false));
            }
        }
        if let Some(result) = self.format.parse_flag(arg) {
            result?;
            return Ok(Some(false));
        }
        // Accepted no-op options for `query` (CommonQueryOptions.java:
        // "query: no-op (aspects are always followed)").
        if let Some(value) = boolean(arg, "infer_universe_scope") {
            self.infer_universe_scope = value?;
            return Ok(Some(false));
        }
        for name in [
            "include_aspects",
            "experimental_explicit_aspects",
            "incompatible_package_group_includes_double_slash",
        ] {
            if let Some(value) = boolean(arg, name) {
                value?;
                return Ok(Some(false));
            }
        }
        Ok(None)
    }

    fn line_terminator(&self) -> &'static str {
        if self.line_terminator_null {
            "\0"
        } else {
            "\n"
        }
    }

    /// DependencyFilter (AbstractBlazeQueryEnvironment.constructDependencyFilter,
    /// query2/common/AbstractBlazeQueryEnvironment.java:118-131).
    fn admits(&self, attribute: &QueryAttribute) -> bool {
        (self.tool_deps || !attribute.tool)
            && (self.implicit_deps || attribute.explicit)
            && (self.nodep_deps || attribute.shape != Shape::Nodep)
    }
}

/// The result of one `query` command.
pub struct QueryOutcome {
    pub stdout: Vec<u8>,
    /// Bazel events in order (`ERROR: ...`, `WARNING: ...`, `INFO: ...`).
    pub events: Vec<String>,
    pub exit_code: i32,
    /// A native fatal loading failure (printed as `bazel-rs: ...`, exit 1)
    /// after the events reported before it.
    pub fatal: Option<String>,
}

/// A query target (packages/Target.java subclasses), by label.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Rule,
    PackageGroup,
    Input,
    Output(Label),
    /// A FakeLoadTarget (query2/compat/FakeLoadTarget.java): a BUILD or
    /// `.bzl` file of `buildfiles`/`loadfiles`.
    Load,
}

/// A failed QueryExpression evaluation (QueryException).
struct QueryError {
    /// The failed subexpression (identity, as Java compares references).
    expression: Option<*const Expr>,
    expression_text: String,
    message: String,
}

enum Failure {
    Query(QueryError),
    /// Native loading failures outside Bazel's NoSuchThing domain.
    Fatal(String),
}

type Eval<T> = Result<T, Failure>;

fn query_error(expression: &Expr, message: impl Into<String>) -> Failure {
    Failure::Query(QueryError {
        expression: Some(expression as *const Expr),
        expression_text: expression.truncated(),
        message: message.into(),
    })
}

/// A target lookup failure.
enum Lookup {
    /// NoSuchPackageException / NoSuchTargetException.
    Missing(String),
    Fatal(String),
}

fn classify(error: String) -> Lookup {
    if error.starts_with("no such package '") || error.starts_with("no such target '") {
        Lookup::Missing(error)
    } else {
        Lookup::Fatal(error)
    }
}

/// An insertion-ordered set of labels (Java's LinkedHashSet results).
#[derive(Clone, Debug, Default)]
struct Set {
    items: Vec<Label>,
    seen: HashSet<Label>,
}

impl Set {
    fn insert(&mut self, label: Label) -> bool {
        if self.seen.insert(label.clone()) {
            self.items.push(label);
            true
        } else {
            false
        }
    }

    fn extend(&mut self, labels: impl IntoIterator<Item = Label>) {
        for label in labels {
            self.insert(label);
        }
    }

    fn contains(&self, label: &Label) -> bool {
        self.seen.contains(label)
    }

    fn from_labels(labels: impl IntoIterator<Item = Label>) -> Self {
        let mut set = Self::default();
        set.extend(labels);
        set
    }

    fn retain(&mut self, keep: impl Fn(&Label) -> bool) {
        self.items.retain(|label| keep(label));
        self.seen = self.items.iter().cloned().collect();
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A dependency that could not be resolved (TargetEdgeObserver.missingEdge).
struct MissingEdge {
    from: Label,
    /// `None` for a label text that does not resolve to a label.
    to: Option<Label>,
    error: String,
    /// Approximate Skyframe completion order of the failure.
    cost: u8,
}

/// One dependency edge of a target (LabelVisitationUtils.LabelProcessor).
#[derive(Clone, Debug)]
struct Edge {
    label: Label,
    /// The attribute index in the source rule's attribute list.
    attribute: Option<usize>,
}

/// The query environment of one command.
struct Env<'a> {
    engine: &'a mut Engine,
    options: &'a QueryOptions,
    /// TargetPattern.Parser's relativeDirectory: the working directory below
    /// the workspace root.
    relative_directory: String,
    kinds: HashMap<Label, Result<Kind, String>>,
    attributes: HashMap<Label, Arc<Vec<QueryAttribute>>>,
    edges: HashMap<Label, Arc<Vec<Edge>>>,
    load_targets: BTreeMap<Label, PackageId>,
    patterns: HashMap<String, Vec<Label>>,
    /// BlazeQueryEnvironment's Digraph: visited targets and observed edges.
    graph_nodes: Set,
    graph_edges: HashMap<Label, Set>,
    events: Vec<String>,
    /// ErrorSensingEventHandler.hasErrors.
    has_errors: bool,
    package_errors: HashMap<PackageId, String>,
    /// Dependency label texts that do not resolve, per target.
    unresolved: HashMap<Label, Vec<String>>,
    /// RepositorySet::main_mapping_order, for display-form labels.
    main_mapping_order: Vec<String>,
    /// `.bzl` transitive digests ($rule_implementation_hash).
    digests: HashMap<Label, [u8; 32]>,
    /// SkyQueryEnvironment (QueryEnvironmentFactory.canUseSkyQuery: a
    /// universe scope and streamed, unordered results).
    sky: bool,
    /// Edges ignore the dependency filter (building the SkyQuery universe:
    /// TransitiveTraversal loads every dependency).
    unfiltered: bool,
    /// The SkyQuery universe: the transitive closure of the universe scope.
    universe: Set,
    universe_packages: HashSet<PackageId>,
    /// Reverse edges within the universe (allrdeps).
    universe_parents: Option<HashMap<Label, Vec<Label>>>,
    /// SkyQuery pattern failures, raised when the literal is evaluated.
    pattern_failures: HashMap<String, String>,
}

/// QueryOptionHelper.readQuery (runtime/commands/QueryOptionHelper.java:
/// 41-70): the residue joined by spaces, else the `--query_file` contents
/// (ISO_8859_1). `None` is a readQuery failure, which QueryCommand returns as
/// the command's failure detail without an event
/// (QueryEnvironmentBasedCommand.java:
/// `return BlazeCommandResult.failureDetail(e.getFailureDetail())`): exit 2,
/// nothing on stderr (paired capture error.no_expression). The failures are
/// "missing query expression. Type 'bazel help query' for syntax and help",
/// "Command-line query and --query_file cannot both be specified" and
/// "I/O error reading from PATH".
pub fn read_query(residue: &[String], options: &QueryOptions) -> Option<String> {
    if !residue.is_empty() {
        if !options.query_file.is_empty() {
            return None;
        }
        return Some(residue.join(" "));
    }
    if options.query_file.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let bytes = crate::observed_fs::read(cwd.join(&options.query_file)).ok()?;
    Some(bytes.into_iter().map(char::from).collect())
}

impl Engine {
    /// Evaluate one `bazel query` command's expression (read by
    /// [`read_query`] before any loading). `Err` is a native fatal loading
    /// failure (printed as `bazel-rs: ...`, exit 1).
    pub fn query(
        &mut self,
        query: &str,
        options: &QueryOptions,
        relative_directory: &str,
    ) -> Result<QueryOutcome, String> {
        let mut outcome = QueryOutcome {
            stdout: Vec::new(),
            events: Vec::new(),
            exit_code: 0,
            fatal: None,
        };
        let fail = |mut outcome: QueryOutcome, message: String, code: i32| {
            outcome.events.push(format!("ERROR: {message}"));
            outcome.exit_code = code;
            Ok(outcome)
        };
        let query = query.to_owned();
        if !FORMATTERS.contains(&options.output.as_str()) {
            return fail(
                outcome,
                format!(
                    "Invalid output format '{}'. Valid values are: {}",
                    options.output,
                    FORMATTERS.join(", ")
                ),
                2,
            );
        }
        if options.format.proto_rule_classes
            && matches!(
                options.output.as_str(),
                "proto" | "streamed_jsonproto" | "streamed_proto"
            )
        {
            return Err(
                "unsupported --proto:rule_classes: Stardoc rule_class_info is not implemented in the native profile"
                    .to_owned(),
            );
        }
        let streamed = is_streamed(&options.output)
            && (options.order_output == OrderOutput::No
                || (options.order_output == OrderOutput::Auto && options.lexicographical_output));
        if options.graphless == Some(true) && !streamed {
            return fail(
                outcome,
                format!(
                    "--experimental_graphless_query requires --order_output=no or --order_output=auto and an --output option that supports streaming; valid values are: {}",
                    FORMATTERS
                        .iter()
                        .filter(|name| is_streamed(name))
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                2,
            );
        }
        let sky = streamed && (!options.universe_scope.is_empty() || options.infer_universe_scope);
        let parsed = if sky {
            language::parse_sky(&query)
        } else {
            language::parse(&query)
        };
        let expr = match parsed {
            Ok(expr) => expr,
            Err(error) => {
                return fail(
                    outcome,
                    format!(
                        "Error while parsing '{}': {error}",
                        language::truncate(&query)
                    ),
                    2,
                );
            }
        };
        // LocationOutputFormatter.verifyCompatible (LocationOutputFormatter.java:51-66).
        if options.output == "location"
            && expr.contains_function(&[Function::LoadFiles, Function::BuildFiles])
        {
            return fail(
                outcome,
                "Query expressions involving 'buildfiles' or 'loadfiles' cannot be used with --output=location".to_owned(),
                2,
            );
        }
        let mut env = Env {
            engine: self,
            options,
            relative_directory: relative_directory.to_owned(),
            kinds: HashMap::new(),
            attributes: HashMap::new(),
            edges: HashMap::new(),
            load_targets: BTreeMap::new(),
            patterns: HashMap::new(),
            graph_nodes: Set::default(),
            graph_edges: HashMap::new(),
            events: Vec::new(),
            has_errors: false,
            package_errors: HashMap::new(),
            unresolved: HashMap::new(),
            main_mapping_order: Vec::new(),
            digests: HashMap::new(),
            sky,
            unfiltered: false,
            universe: Set::default(),
            universe_packages: HashSet::new(),
            universe_parents: None,
            pattern_failures: HashMap::new(),
        };
        let result = env.evaluate_query(&expr);
        let mut events = std::mem::take(&mut env.events);
        let result = match result {
            Ok(result) => result,
            Err(Failure::Fatal(error)) => {
                // Events reported before the fatal failure still print.
                outcome.events = events;
                outcome.fatal = Some(error);
                outcome.exit_code = 1;
                return Ok(outcome);
            }
            Err(Failure::Query(error)) => {
                events.push(format!("ERROR: {}", error.message));
                outcome.events = events;
                outcome.exit_code = 7;
                return Ok(outcome);
            }
        };
        if !options.consistent_labels
            && result
                .items
                .iter()
                .any(|label| !label.repository.is_empty())
        {
            env.main_mapping_order = env.engine.repositories.main_mapping_order();
        }
        let success = if env.has_errors {
            events.push(
                "WARNING: --keep_going specified, ignoring errors. Results may be inaccurate"
                    .to_owned(),
            );
            false
        } else {
            true
        };
        let sort = streamed
            && options.order_output == OrderOutput::Auto
            && !matches!(&expr, Expr::Function(Function::SomePath, _));
        let stdout = match env.output(&result, streamed, sort) {
            Ok(stdout) => stdout,
            Err(Failure::Fatal(error)) => {
                outcome.events = events;
                outcome.fatal = Some(error);
                outcome.exit_code = 1;
                return Ok(outcome);
            }
            Err(Failure::Query(error)) => {
                events.push(format!("ERROR: {}", error.message));
                outcome.events = events;
                outcome.exit_code = 7;
                return Ok(outcome);
            }
        };
        if !options.output_file.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            std::fs::write(cwd.join(&options.output_file), &stdout)
                .map_err(|e| format!("I/O error: {e}"))?;
        } else {
            outcome.stdout = stdout;
        }
        events.extend(std::mem::take(&mut env.events));
        if result.is_empty() {
            events.push("INFO: Empty results".to_owned());
        }
        outcome.events = events;
        outcome.exit_code = if success { 0 } else { 3 };
        Ok(outcome)
    }
}

/// QueryEnvironment.shouldVisit.
fn should_visit(depth: Option<i32>, current: i32) -> bool {
    depth.is_none_or(|max| current <= max)
}

impl Env<'_> {
    fn error_event(&mut self, message: String) {
        self.has_errors = true;
        self.events.push(format!("ERROR: {message}"));
    }

    /// AbstractBlazeQueryEnvironment.evaluateQueryInternal
    /// (query2/common/AbstractBlazeQueryEnvironment.java:150-227).
    fn evaluate_query(&mut self, expr: &Expr) -> Eval<Set> {
        let mut patterns = Vec::new();
        expr.collect_target_patterns(&mut patterns);
        if self.sky {
            self.build_universe(&patterns)?;
        }
        for pattern in &patterns {
            let resolved = self.resolve_pattern(pattern)?;
            let resolved = if self.sky {
                self.within_universe(pattern, resolved)
            } else {
                resolved
            };
            match resolved {
                Ok(targets) => {
                    self.patterns.insert(pattern.clone(), targets);
                }
                Err(message) if self.sky => {
                    // SkyQuery resolves a literal when it is evaluated
                    // (TargetLiteral.eval): the literal is the failed
                    // expression.
                    self.pattern_failures.insert(pattern.clone(), message);
                    self.patterns.insert(pattern.clone(), Vec::new());
                }
                Err(message) => {
                    if !self.options.keep_going {
                        // The TargetParsingException message, unwrapped.
                        return Err(Failure::Query(QueryError {
                            expression: None,
                            expression_text: String::new(),
                            message,
                        }));
                    }
                    self.error_event(format!("Skipping '{pattern}': {message}"));
                    self.patterns.insert(pattern.clone(), Vec::new());
                }
            }
        }
        let top = expr as *const Expr;
        match self.eval(expr, &mut Vec::new()) {
            Ok(result) => {
                if self.has_errors && !self.options.keep_going {
                    return Err(Failure::Query(QueryError {
                        expression: None,
                        expression_text: String::new(),
                        message: format!("Evaluation of query \"{}\" failed", expr.truncated()),
                    }));
                }
                Ok(result)
            }
            Err(Failure::Query(error)) => {
                // QueryException.describeFailedQuery (QueryException.java:26-37).
                let message = match error.expression {
                    None => format!("Evaluation failed: {}", error.message),
                    Some(failed) if failed == top => format!(
                        "Evaluation of query \"{}\" failed: {}",
                        expr.truncated(),
                        error.message
                    ),
                    Some(_) => format!(
                        "Evaluation of subquery \"{}\" failed (did you want to use --keep_going?): {}",
                        error.expression_text, error.message
                    ),
                };
                Err(Failure::Query(QueryError {
                    expression: None,
                    expression_text: String::new(),
                    message,
                }))
            }
            Err(fatal) => Err(fatal),
        }
    }

    /// AbstractBlazeQueryEnvironment.handleError (:330-345).
    fn handle_error(&mut self, expression: &Expr, message: String) -> Eval<()> {
        if !self.options.keep_going {
            return Err(query_error(expression, message));
        }
        self.error_event(format!(
            "Evaluation of query \"{}\" failed: {message}",
            expression.truncated()
        ));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Targets.

    fn load_package(&mut self, id: &PackageId) -> Result<(), String> {
        if let Some(error) = self.package_errors.get(id) {
            return Err(error.clone());
        }
        let result = self.engine.load_package(id);
        if let Err(error) = &result {
            self.package_errors.insert(id.clone(), error.clone());
        }
        result
    }

    /// TargetProvider.getTarget.
    fn lookup(&mut self, label: &Label) -> Result<Result<Kind, String>, Failure> {
        if let Some(kind) = self.kinds.get(label) {
            return Ok(kind.clone());
        }
        let kind = match self.compute_kind(label) {
            Ok(kind) => Ok(kind),
            Err(Lookup::Missing(error)) => Err(error),
            Err(Lookup::Fatal(error)) => return Err(Failure::Fatal(error)),
        };
        self.kinds.insert(label.clone(), kind.clone());
        Ok(kind)
    }

    fn compute_kind(&mut self, label: &Label) -> Result<Kind, Lookup> {
        let id = label.package_id();
        if crate::label::unknown_repository_name(&id.repository).is_some() {
            return Err(classify(
                self.engine
                    .load_package(&id)
                    .err()
                    .unwrap_or_else(|| "no such package".to_owned()),
            ));
        }
        self.load_package(&id).map_err(classify)?;
        if let Some(rule) = self.engine.packages[&id].rules.get(label) {
            return Ok(if query_attrs::is_package_group(rule) {
                Kind::PackageGroup
            } else {
                Kind::Rule
            });
        }
        if let Some((owner, _)) = self.engine.generated.get(label) {
            return Ok(Kind::Output(owner.clone()));
        }
        // The BUILD file is an input file target of its package
        // (Package.Builder.createBuildFile).
        if self.build_file_name(&id).as_deref() == Some(label.name.as_str()) {
            return Ok(Kind::Input);
        }
        match self.engine.target(label).map_err(classify)? {
            Target::Source(_) => Ok(Kind::Input),
            Target::Output(rule, _) => Ok(Kind::Output(rule.label.clone())),
            Target::Rule(_) => Ok(Kind::Rule),
        }
    }

    fn build_file_name(&self, id: &PackageId) -> Option<String> {
        self.engine.packages.get(id).and_then(|package| {
            package
                .build_file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
    }

    /// Every target of a loaded package in name order (Package.getTargets):
    /// rules, outputs, input files and the BUILD file.
    fn all_target_names(&self, id: &PackageId) -> BTreeSet<String> {
        let mut names = self.engine.package_target_names(id);
        names.extend(self.build_file_name(id));
        names
    }

    fn kind(&self, label: &Label) -> Kind {
        if self.load_targets.contains_key(label) && !self.kinds.contains_key(label) {
            return Kind::Load;
        }
        match self.kinds.get(label) {
            Some(Ok(kind)) => kind.clone(),
            _ => Kind::Load,
        }
    }

    fn rule(&self, label: &Label) -> &Rule {
        &self.engine.packages[&label.package_id()].rules[label]
    }

    fn package(&self, label: &Label) -> &Package {
        &self.engine.packages[&label.package_id()]
    }

    fn rule_attributes(&mut self, label: &Label) -> Arc<Vec<QueryAttribute>> {
        if let Some(attributes) = self.attributes.get(label) {
            return attributes.clone();
        }
        let attributes = self.compute_attributes(label);
        self.attributes.insert(label.clone(), attributes.clone());
        attributes
    }

    fn compute_attributes(&self, label: &Label) -> Arc<Vec<QueryAttribute>> {
        let package = self.package(label);
        let rule = &package.rules[label];
        let mut attributes = vec![QueryAttribute {
            name: "name".to_owned(),
            public_name: "name".to_owned(),
            shape: Shape::Other,
            ty: query_attrs::AttrType::String,
            explicit: true,
            tool: false,
            value: Some(AttrValue::String(label.name.clone())),
            select: None,
            default: None,
            aspects: Vec::new(),
        }];
        attributes.extend(query_attrs::rule_attributes(rule, package));
        Arc::new(attributes)
    }

    fn parse_label(&self, from: &Label, text: &str) -> Result<Label, String> {
        Label::parse_canonical(text).or_else(|_| {
            self.engine
                .repositories
                .parse(text, &from.repository, &from.package)
        })
    }

    /// The `getVisibilityDependencyLabels` of a target.
    fn visibility_of(&self, label: &Label, kind: &Kind) -> Visibility {
        match kind {
            Kind::Rule | Kind::PackageGroup => self.rule(label).visibility.clone(),
            Kind::Output(rule) => self.rule(rule).visibility.clone(),
            Kind::Input => self.engine.source_visibility(label),
            Kind::Load => Visibility::Private,
        }
    }

    /// LabelVisitationUtils.visitTarget (packages/LabelVisitationUtils.java:58-80),
    /// filtered by the dependency filter.
    fn edges(&mut self, label: &Label) -> Eval<Arc<Vec<Edge>>> {
        if let Some(edges) = self.edges.get(label) {
            return Ok(edges.clone());
        }
        let kind = self.kind(label);
        let mut edges = Vec::new();
        let push = |edges: &mut Vec<Edge>, label: Label, attribute: Option<usize>| {
            edges.push(Edge { label, attribute });
        };
        match &kind {
            Kind::Output(rule) => {
                push(&mut edges, rule.clone(), None);
                for group in self.visibility_of(label, &kind).group_labels() {
                    push(&mut edges, group.clone(), None);
                }
            }
            Kind::Input => {
                for group in self.visibility_of(label, &kind).group_labels() {
                    push(&mut edges, group.clone(), None);
                }
            }
            Kind::PackageGroup => {
                if let RuleKind::PackageGroup(definition) = &self.rule(label).kind {
                    for include in &definition.includes {
                        push(&mut edges, include.clone(), None);
                    }
                }
            }
            Kind::Load => {}
            Kind::Rule => {
                let attributes = self.rule_attributes(label);
                let rule = self.rule(label);
                // visitRuleVisibility (:94-108): the NODEP_LABEL_LIST visibility.
                let visibility = QueryAttribute {
                    name: "visibility".to_owned(),
                    public_name: "visibility".to_owned(),
                    shape: Shape::Nodep,
                    ty: query_attrs::AttrType::NodepLabelList,
                    explicit: rule.explicit_visibility,
                    tool: false,
                    value: None,
                    select: None,
                    default: None,
                    aspects: Vec::new(),
                };
                if self.unfiltered || self.options.admits(&visibility) {
                    let index = attributes.iter().position(|a| a.name == "visibility");
                    for group in rule.visibility.group_labels() {
                        push(&mut edges, group.clone(), index);
                    }
                }
                let mut texts = Vec::new();
                for (index, attribute) in attributes.iter().enumerate() {
                    if !attribute.shape.is_dependency()
                        || !(self.unfiltered || self.options.admits(attribute))
                    {
                        continue;
                    }
                    for text in attribute.labels() {
                        texts.push((text, Some(index)));
                    }
                }
                // visitRuleToolchains (:110-118): the hidden implicit attribute.
                if self.unfiltered || self.options.implicit_deps {
                    for text in query_attrs::toolchain_types(rule) {
                        texts.push((text, None));
                    }
                }
                for (text, index) in texts {
                    match self.parse_label(label, &text) {
                        Ok(target) => push(&mut edges, target, index),
                        // A label of a repository the rule's repository does
                        // not see: Bazel's `@@[unknown repo ...]` label, whose
                        // package lookup fails (a missing edge).
                        Err(error) => self
                            .unresolved
                            .entry(label.clone())
                            .or_default()
                            .push(error),
                    }
                }
            }
        }
        let edges = Arc::new(edges);
        self.edges.insert(label.clone(), edges.clone());
        Ok(edges)
    }

    /// TargetUtils.isExplicitDependency (packages/TargetUtils.java:338-356).
    fn is_explicit_dependency(&mut self, rule: &Label, dependency: &Label) -> bool {
        if self
            .rule(rule)
            .visibility
            .group_labels()
            .any(|group| group == dependency)
        {
            return true;
        }
        let attributes = self.rule_attributes(rule);
        attributes
            .iter()
            .filter(|attribute| attribute.shape.is_dependency() && attribute.explicit)
            .flat_map(|attribute| attribute.labels())
            .any(|text| self.parse_label(rule, &text).as_ref() == Ok(dependency))
    }

    /// ErrorPrintingTargetEdgeErrorObserver.missingEdge with
    /// TargetUtils.formatMissingEdge (TargetUtils.java:398-430).
    fn missing_edge(&mut self, from: Option<&Label>, to: &Label, error: &str) {
        let Some(from) = from else {
            self.error_event(error.to_owned());
            return;
        };
        let kind = self.kind(from);
        let (location, message) = match kind {
            Kind::Rule => {
                let message = if self.is_explicit_dependency(from, to) {
                    format!(
                        "{error} and referenced by '{}'",
                        crate::rule_declarations::canonical_form(from)
                    )
                } else {
                    format!(
                        "every rule of type {} implicitly depends upon the target '{}', but this target could not be found because of: {error}",
                        self.rule(from).rule_class_name(),
                        crate::rule_declarations::canonical_form(to)
                    )
                };
                (self.rule(from).location.clone(), message)
            }
            Kind::Input => (
                Some(self.input_location(from)),
                format!(
                    "{error} (this is usually caused by a missing package group in the package-level visibility declaration)"
                ),
            ),
            _ => (
                None,
                format!(
                    "in target '{}', no such label '{}': {error}",
                    crate::rule_declarations::canonical_form(from),
                    crate::rule_declarations::canonical_form(to)
                ),
            ),
        };
        match location {
            Some(location) => self.error_event(format!("{location}: {message}")),
            None => self.error_event(message),
        }
    }

    /// InputFile.getLocation: the BUILD file location (line 1, column 1).
    fn input_location(&self, label: &Label) -> String {
        format!("{}:1:1", self.package(label).build_file.display())
    }

    /// LabelVisitor.syncWithVisitor (query2/query/LabelVisitor.java): a
    /// breadth-first visitation from `roots`, bounded by `depth`, observing
    /// nodes and edges into the Digraph and reporting missing edges. Returns
    /// the visited targets in visitation order and whether errors occurred.
    fn visit(&mut self, roots: &Set, depth: Option<i32>) -> Eval<(Set, Vec<MissingEdge>)> {
        let loaded_before = self
            .engine
            .packages
            .keys()
            .chain(self.package_errors.keys())
            .cloned()
            .collect::<HashSet<_>>();
        let mut visited = Set::default();
        let mut missing = Vec::new();
        let mut frontier = Vec::new();
        for root in &roots.items {
            if visited.insert(root.clone()) {
                self.graph_nodes.insert(root.clone());
                frontier.push(root.clone());
            }
        }
        // Aspect dependencies are enqueued from the edge's source at the
        // depth of the edge's target (LabelVisitor.visitAspectsIfRequired).
        let mut deferred: Vec<(Label, Label, Option<usize>)> = Vec::new();
        let mut level = 0;
        while !frontier.is_empty() || !deferred.is_empty() {
            // enqueueTarget: no lookups at the maximum depth.
            if !should_visit(depth, level + 1) {
                break;
            }
            let mut pending = std::mem::take(&mut deferred);
            for from in &frontier {
                for edge in self.edges(from)?.iter() {
                    pending.push((from.clone(), edge.label.clone(), edge.attribute));
                }
                for error in self.unresolved.get(from).cloned().unwrap_or_default() {
                    missing.push(MissingEdge {
                        from: from.clone(),
                        to: None,
                        error,
                        cost: 1,
                    });
                }
            }
            let mut next = Vec::new();
            for (from, to, attribute) in pending {
                match self.lookup(&to)? {
                    Ok(_) => {
                        self.graph_nodes.insert(to.clone());
                        self.graph_edges
                            .entry(from.clone())
                            .or_default()
                            .insert(to.clone());
                        deferred.extend(
                            self.aspect_dependencies(&from, &to, attribute)?
                                .into_iter()
                                .map(|label| (from.clone(), label, None)),
                        );
                        if visited.insert(to.clone()) {
                            next.push(to);
                        }
                    }
                    Err(error) => {
                        let cost = if loaded_before.contains(&to.package_id()) {
                            0
                        } else if error.contains("BUILD file not found") {
                            1
                        } else {
                            2
                        };
                        missing.push(MissingEdge {
                            from,
                            to: Some(to),
                            error,
                            cost,
                        });
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        Ok((visited, missing))
    }

    /// buildTransitiveClosure / the graphless deps, rdeps, somepath and
    /// allpaths visitations: QueryTransitivePackagePreloader first
    /// (when the depth is unbounded or at least MAX_DEPTH_FULL_SCAN_LIMIT,
    /// GraphlessBlazeQueryEnvironment.java:470-492), then LabelVisitor with
    /// the error-printing observer, then handleError.
    fn closure(&mut self, caller: &Expr, roots: &Set, depth: Option<i32>) -> Eval<Set> {
        let (visited, missing) = self.visit(roots, depth)?;
        if missing.is_empty() {
            return Ok(visited);
        }
        const MAX_DEPTH_FULL_SCAN_LIMIT: i32 = 20;
        if should_visit(depth, MAX_DEPTH_FULL_SCAN_LIMIT) {
            if !self.options.keep_going {
                // QueryTransitivePackagePreloader.maybeThrowQueryExceptionForResultWithError:
                // the first Skyframe failure, which native approximates by
                // completion order: packages already loaded, then missing
                // BUILD files, then packages that must be loaded; ties in
                // visitation order.
                let first = missing
                    .iter()
                    .enumerate()
                    .min_by_key(|(index, edge)| (edge.cost, *index))
                    .map(|(_, edge)| edge.error.clone())
                    .unwrap_or_default();
                return Err(query_error(
                    caller,
                    format!("preloading transitive closure failed: {first}"),
                ));
            }
            // TransitiveTargetFunction reports every missing edge too.
            for edge in &missing {
                self.report_missing(edge);
            }
        }
        for edge in &missing {
            self.report_missing(edge);
        }
        self.handle_error(
            caller,
            "errors were encountered while computing transitive closure".to_owned(),
        )?;
        Ok(visited)
    }

    fn report_missing(&mut self, edge: &MissingEdge) {
        match &edge.to {
            Some(to) => self.missing_edge(Some(&edge.from), to, &edge.error),
            None => {
                let location = self.rule(&edge.from).location.clone();
                let message = format!(
                    "{} and referenced by '{}'",
                    edge.error,
                    crate::rule_declarations::canonical_form(&edge.from)
                );
                self.error_event(match location {
                    Some(location) => format!("{location}: {message}"),
                    None => message,
                });
            }
        }
    }

    /// LabelVisitor.visitAspectsIfRequired (LabelVisitor.java:255-276): the
    /// attribute aspects of `from` that `to`'s rule class satisfies
    /// contribute their attributes' dependencies as edges of `from`.
    fn aspect_dependencies(
        &mut self,
        from: &Label,
        to: &Label,
        attribute: Option<usize>,
    ) -> Eval<Vec<Label>> {
        let Some(index) = attribute else {
            return Ok(Vec::new());
        };
        if self.kind(from) != Kind::Rule || self.kind(to) != Kind::Rule {
            return Ok(Vec::new());
        }
        let attributes = self.rule_attributes(from);
        let Some(attribute) = attributes.get(index) else {
            return Ok(Vec::new());
        };
        if attribute.aspects.is_empty() || !(self.unfiltered || self.options.implicit_deps) {
            return Ok(Vec::new());
        }
        let target = self.rule(to);
        let mut labels = Vec::new();
        for aspect in &attribute.aspects {
            if !query_attrs::aspect_applies(aspect, target) {
                continue;
            }
            for (text, tool) in query_attrs::aspect_labels(aspect) {
                if tool && !(self.unfiltered || self.options.tool_deps) {
                    continue;
                }
                labels.push(self.parse_label(from, &text).map_err(Failure::Fatal)?);
            }
        }
        Ok(labels)
    }

    /// The successors of a target including aspect dependencies
    /// (PathLabelVisitor.visitEdgesOfTarget and visitAspectsIfRequired).
    fn successors(&mut self, from: &Label) -> Eval<Vec<Label>> {
        let mut result = Vec::new();
        for edge in self.edges(from)?.iter() {
            result.push(edge.label.clone());
            if self.lookup(&edge.label)?.is_ok() {
                result.extend(self.aspect_dependencies(from, &edge.label, edge.attribute)?);
            }
        }
        Ok(result)
    }

    // ------------------------------------------------------------------
    // Target patterns (TargetPattern.java, TargetPatternPreloader).

    /// `Ok(Err(message))` is a TargetParsingException: the word does not
    /// parse as a target pattern (crate::target_pattern), its evaluation
    /// raised one (`PatternError::Parsing`), or loading a package or target
    /// it names failed with NoSuchPackage/NoSuchTarget (`Lookup::Missing`,
    /// skipped under --keep_going); other loading failures are fatal.
    /// Patterns are parsed and expanded like the build's
    /// (TargetPattern.Parser with the working directory, then
    /// Engine::expand_pattern with FilteringPolicies.NO_FILTER). A leading
    /// `-` is the query's `except` operator, never a negative pattern.
    fn resolve_pattern(&mut self, pattern: &str) -> Eval<Result<Vec<Label>, String>> {
        let parsed = match crate::target_pattern::parse(pattern, &self.relative_directory) {
            Ok(parsed) => parsed,
            Err(message) => return Ok(Err(message)),
        };
        debug_assert!(!parsed.negative, "query words never start with '-'");
        // --keep_going: a wildcard keeps the packages that load; each one
        // that does not is an error event (docs/KEEP_GOING.md).
        if self.options.keep_going {
            self.engine.pattern_load_errors = Some(Vec::new());
        }
        let expanded = self.engine.expand_pattern(&parsed, false, false);
        for error in self.engine.pattern_load_errors.take().unwrap_or_default() {
            self.error_event(error);
        }
        match expanded {
            Ok(labels) => {
                let mut targets = Vec::new();
                for label in labels {
                    match self.lookup(&label)? {
                        Ok(_) => targets.push(label),
                        Err(error) => return Ok(Err(error)),
                    }
                }
                self.pattern_edges(&targets)?;
                Ok(Ok(targets))
            }
            Err(crate::engine::PatternError::Parsing(message)) => Ok(Err(message)),
            Err(crate::engine::PatternError::Loading(message)) => match classify(message) {
                // The BUILD file is a target of its package
                // (Package.Builder.createBuildFile), which the build's
                // pattern expansion does not offer.
                Lookup::Missing(message) => {
                    if let crate::target_pattern::Kind::SingleTarget { target } = &parsed.kind
                        && parsed.repo.is_none()
                    {
                        let label = Label {
                            repository: String::new(),
                            package: parsed.package.clone(),
                            name: target.clone(),
                        };
                        if self.build_file_name(&label.package_id()).as_deref()
                            == Some(label.name.as_str())
                            && self.lookup(&label)?.is_ok()
                        {
                            self.pattern_edges(std::slice::from_ref(&label))?;
                            return Ok(Ok(vec![label]));
                        }
                    }
                    Ok(Err(message))
                }
                Lookup::Fatal(message) => Err(Failure::Fatal(message)),
            },
        }
    }

    /// BlazeQueryEnvironment.getTargetsMatchingPatternImpl
    /// (BlazeQueryEnvironment.java:162-195): Digraph nodes for the pattern's
    /// targets and the edges among them within their packages.
    fn pattern_edges(&mut self, targets: &[Label]) -> Eval<()> {
        let set = targets.iter().cloned().collect::<HashSet<_>>();
        let packages = targets
            .iter()
            .map(Label::package_id)
            .collect::<HashSet<_>>();
        for target in targets {
            self.graph_nodes.insert(target.clone());
            match self.kind(target) {
                Kind::Output(rule) => {
                    if set.contains(&rule) {
                        self.graph_edges
                            .entry(target.clone())
                            .or_default()
                            .insert(rule);
                    }
                }
                Kind::Rule => {
                    let mut labels = self
                        .edges(target)?
                        .iter()
                        .map(|edge| edge.label.clone())
                        .collect::<Vec<_>>();
                    labels.sort();
                    labels.dedup();
                    for label in labels {
                        if packages.contains(&label.package_id())
                            && set.contains(&label)
                            && self.lookup(&label)?.is_ok()
                        {
                            self.graph_edges
                                .entry(target.clone())
                                .or_default()
                                .insert(label);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Evaluation.

    fn eval(&mut self, expr: &Expr, context: &mut Vec<(String, Set)>) -> Eval<Set> {
        match expr {
            Expr::Target(pattern) => self.eval_literal(Some(expr), pattern, context),
            Expr::Set(words) => {
                let mut result = Set::default();
                for word in words {
                    let value = self.eval_literal(None, word, context)?;
                    result.extend(value.items);
                }
                Ok(result)
            }
            Expr::Binary(operator, operands) => match operator {
                Operator::Plus | Operator::Union => {
                    let mut result = Set::default();
                    for operand in operands {
                        result.extend(self.eval(operand, context)?.items);
                    }
                    Ok(result)
                }
                Operator::Minus | Operator::Except => {
                    let mut result = self.eval(&operands[0], context)?;
                    let mut removed = HashSet::new();
                    for operand in &operands[1..] {
                        removed.extend(self.eval(operand, context)?.items);
                    }
                    result.retain(|label| !removed.contains(label));
                    Ok(result)
                }
                Operator::Intersect | Operator::Caret => {
                    let mut result = self.eval(&operands[0], context)?;
                    for operand in &operands[1..] {
                        let other = self.eval(operand, context)?;
                        result.retain(|label| other.contains(label));
                    }
                    Ok(result)
                }
            },
            Expr::Let(name, value, body) => {
                if !language::is_valid_variable_name(name) {
                    return Err(query_error(
                        expr,
                        format!("invalid variable name '{name}' in let expression"),
                    ));
                }
                let value = self.eval(value, context)?;
                context.push((name.clone(), value));
                let result = self.eval(body, context);
                context.pop();
                result
            }
            Expr::Function(function, args) => self.eval_function(expr, *function, args, context),
        }
    }

    fn eval_literal(
        &mut self,
        owner: Option<&Expr>,
        pattern: &str,
        context: &[(String, Set)],
    ) -> Eval<Set> {
        if language::is_variable_reference(pattern) {
            let name = &pattern[1..];
            return match context.iter().rev().find(|(bound, _)| bound == name) {
                Some((_, value)) => Ok(value.clone()),
                // TargetLiteral.evalVarReference (TargetLiteral.java:43-57):
                // the literal is the failed expression; a word of set() is
                // always a subexpression.
                None => Err(Failure::Query(QueryError {
                    expression: Some(owner.map_or(std::ptr::null(), |e| e as *const Expr)),
                    expression_text: Expr::Target(pattern.to_owned()).truncated(),
                    message: format!("undefined variable '{name}'"),
                })),
            };
        }
        if let Some(message) = self.pattern_failures.get(pattern).cloned() {
            if self.options.keep_going {
                if let Some(owner) = owner {
                    self.handle_error(owner, message)?;
                } else {
                    self.error_event(message);
                }
                return Ok(Set::default());
            }
            return Err(Failure::Query(QueryError {
                expression: Some(owner.map_or(std::ptr::null(), |e| e as *const Expr)),
                expression_text: Expr::Target(pattern.to_owned()).truncated(),
                message,
            }));
        }
        Ok(Set::from_labels(
            self.patterns.get(pattern).cloned().unwrap_or_default(),
        ))
    }

    fn expression_arg(args: &[Argument], index: usize) -> &Expr {
        match &args[index] {
            Argument::Expression(expr) => expr,
            _ => unreachable!("the parser types every argument"),
        }
    }

    fn word_arg(args: &[Argument], index: usize) -> &str {
        match &args[index] {
            Argument::Word(word) => word,
            _ => unreachable!("the parser types every argument"),
        }
    }

    fn int_arg(args: &[Argument], index: usize) -> Option<i32> {
        match args.get(index) {
            Some(Argument::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    fn eval_function(
        &mut self,
        expr: &Expr,
        function: Function,
        args: &[Argument],
        context: &mut Vec<(String, Set)>,
    ) -> Eval<Set> {
        match function {
            Function::Config => unreachable!("config is only parsed for cquery"),
            // AllRdepsFunction over the SkyQuery universe graph.
            Function::AllRdeps => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let depth = Self::int_arg(args, 1);
                let parents = self.universe_parents()?;
                let mut result = Set::default();
                let mut at = from.items.clone();
                let mut level = 0;
                while should_visit(depth, level) && !at.is_empty() {
                    let mut next = Vec::new();
                    for target in at {
                        if result.insert(target.clone())
                            && let Some(parents) = parents.get(&target)
                        {
                            next.extend(parents.iter().cloned());
                        }
                    }
                    at = next;
                    level += 1;
                }
                Ok(result)
            }
            Function::RBuildFiles => {
                let words = args
                    .iter()
                    .filter_map(|arg| match arg {
                        Argument::Word(word) => Some(word.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                self.rbuildfiles(&words)
            }
            // DepsFunction / GraphlessBlazeQueryEnvironment.deps (:250-300).
            Function::Deps => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let depth = Self::int_arg(args, 1);
                self.closure(expr, &from, depth)
            }
            // RdepsFunction / GraphlessBlazeQueryEnvironment.rdeps (:360-395)
            // with PathLabelVisitor.rdeps.
            Function::Rdeps => {
                let universe = self.eval(Self::expression_arg(args, 0), context)?;
                let from = self.eval(Self::expression_arg(args, 1), context)?;
                let depth = Self::int_arg(args, 2);
                let closure = self.closure(expr, &universe, None)?;
                let parents = self.parents_within(&closure)?;
                let mut result = Set::default();
                let mut at = from
                    .items
                    .iter()
                    .filter(|label| closure.contains(label))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut level = 0;
                while should_visit(depth, level) && !at.is_empty() {
                    let mut next = Vec::new();
                    for target in at {
                        if result.insert(target.clone())
                            && let Some(parents) = parents.get(&target)
                        {
                            next.extend(parents.iter().cloned());
                        }
                    }
                    at = next;
                    level += 1;
                }
                Ok(result)
            }
            Function::AllPaths => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let to = self.eval(Self::expression_arg(args, 1), context)?;
                let closure = self.closure(expr, &from, None)?;
                let parents = self.parents_within(&closure)?;
                let mut result = Set::default();
                let mut queue = to
                    .items
                    .iter()
                    .filter(|label| closure.contains(label))
                    .cloned()
                    .collect::<VecDeque<_>>();
                while let Some(at) = queue.pop_front() {
                    if result.insert(at.clone())
                        && let Some(parents) = parents.get(&at)
                    {
                        queue.extend(parents.iter().cloned());
                    }
                }
                Ok(result)
            }
            // PathLabelVisitor.somePath (PathLabelVisitor.java:60-82): the
            // breadth-first first-discovered parent chain.
            Function::SomePath => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let to = self.eval(Self::expression_arg(args, 1), context)?;
                let closure = self.closure(expr, &from, None)?;
                let mut parent: HashMap<Label, Option<Label>> = HashMap::new();
                let mut queue = VecDeque::new();
                for root in &from.items {
                    parent.entry(root.clone()).or_insert(None);
                    queue.push_back(root.clone());
                }
                let mut seen = HashSet::new();
                while let Some(at) = queue.pop_front() {
                    if !seen.insert(at.clone()) {
                        continue;
                    }
                    for to in self.successors(&at)? {
                        if !closure.contains(&to) {
                            continue;
                        }
                        parent.entry(to.clone()).or_insert_with(|| Some(at.clone()));
                        queue.push_back(to);
                    }
                }
                for target in &to.items {
                    if closure.contains(target) {
                        let mut path = VecDeque::new();
                        let mut at = target.clone();
                        loop {
                            path.push_front(at.clone());
                            match parent.get(&at).cloned().flatten() {
                                Some(next) => at = next,
                                None => break,
                            }
                        }
                        return Ok(Set::from_labels(path));
                    }
                }
                Ok(Set::default())
            }
            Function::SamePkgDirectRdeps => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let mut result = Set::default();
                for target in &from.items {
                    let siblings = self.siblings(target)?;
                    for sibling in siblings {
                        let edges = self.edges(&sibling)?;
                        if edges.iter().any(|edge| &edge.label == target) {
                            self.graph_nodes.insert(sibling.clone());
                            result.insert(sibling);
                        }
                    }
                }
                Ok(result)
            }
            Function::Siblings => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let mut packages = HashSet::new();
                let mut result = Set::default();
                for target in &from.items {
                    if packages.insert(target.package_id()) {
                        let siblings = self.siblings(target)?;
                        for sibling in &siblings {
                            self.graph_nodes.insert(sibling.clone());
                        }
                        result.extend(siblings);
                    }
                }
                Ok(result)
            }
            Function::Some => {
                let from = self.eval(Self::expression_arg(args, 0), context)?;
                let count = Self::int_arg(args, 1).unwrap_or(1);
                let mut result = Set::default();
                for target in from.items {
                    if result.items.len() as i64 >= i64::from(count) {
                        break;
                    }
                    result.insert(target);
                }
                if result.is_empty() {
                    return Err(query_error(expr, "argument set is empty"));
                }
                Ok(result)
            }
            Function::Kind | Function::Filter | Function::Attr => {
                let (pattern, index) = match function {
                    Function::Attr => (Self::word_arg(args, 1), 2),
                    _ => (Self::word_arg(args, 0), 1),
                };
                let regex = match fancy_regex::Regex::new(pattern) {
                    Ok(regex) => regex,
                    Err(error) => {
                        return Err(query_error(
                            expr,
                            format!(
                                "illegal '{}' pattern regexp '{pattern}': {}",
                                function.name(),
                                java_pattern_error(pattern, &error)
                            ),
                        ));
                    }
                };
                let input = self.eval(Self::expression_arg(args, index), context)?;
                let mut result = Set::default();
                for target in input.items {
                    let strings = match function {
                        Function::Kind => vec![Some(self.target_kind(&target))],
                        Function::Filter => {
                            vec![Some(crate::rule_declarations::canonical_form(&target))]
                        }
                        _ => self.attribute_strings(&target, Self::word_arg(args, 0)),
                    };
                    let mut matched = false;
                    for text in strings.iter().flatten() {
                        match regex.is_match(text) {
                            Ok(true) => {
                                matched = true;
                                break;
                            }
                            Ok(false) => {}
                            // fancy-regex's backtracking limit (or another
                            // runtime failure) is not a non-match; Java's
                            // Matcher.find has no such limit.
                            Err(error) => {
                                return Err(query_error(
                                    expr,
                                    format!(
                                        "'{}' pattern regexp '{pattern}' could not be evaluated against '{text}': {error} (native fancy-regex runtime limit)",
                                        function.name()
                                    ),
                                ));
                            }
                        }
                    }
                    if matched {
                        result.insert(target);
                    }
                }
                Ok(result)
            }
            Function::Labels => {
                let name = Self::word_arg(args, 0).to_owned();
                let input = self.eval(Self::expression_arg(args, 1), context)?;
                let mut result = Set::default();
                for target in input.items {
                    if self.kind(&target) != Kind::Rule {
                        continue;
                    }
                    let prefix = format!(
                        "in '{name}' of rule {}: ",
                        crate::rule_declarations::canonical_form(&target)
                    );
                    for label in self.prerequisites(expr, &target, &name, &prefix)? {
                        self.graph_nodes.insert(label.clone());
                        result.insert(label);
                    }
                }
                Ok(result)
            }
            Function::Tests => {
                let input = self.eval(Self::expression_arg(args, 0), context)?;
                self.tests(expr, input)
            }
            Function::Executables => {
                let input = self.eval(Self::expression_arg(args, 0), context)?;
                let mut result = Set::default();
                for target in input.items {
                    if self.kind(&target) == Kind::Rule {
                        let rule = self.rule(&target);
                        if query_attrs::is_executable(rule)
                            && !rule.rule_class_name().ends_with("_test")
                        {
                            result.insert(target);
                        }
                    }
                }
                Ok(result)
            }
            Function::Visible => {
                let to = self.eval(Self::expression_arg(args, 0), context)?;
                let input = self.eval(Self::expression_arg(args, 1), context)?;
                let mut result = Set::default();
                for target in input.items {
                    let mut all = true;
                    for consumer in &to.items {
                        if !self.is_visible(expr, consumer, &target)? {
                            all = false;
                            break;
                        }
                    }
                    if all {
                        result.insert(target);
                    }
                }
                Ok(result)
            }
            Function::BuildFiles | Function::LoadFiles => {
                let input = self.eval(Self::expression_arg(args, 0), context)?;
                self.load_files(input, function == Function::BuildFiles)
            }
        }
    }

    // ------------------------------------------------------------------
    // SkyQuery (query2/SkyQueryEnvironment.java).

    /// SkyQueryEnvironment.beforeEvaluateQuery: the universe is the
    /// transitive closure (every dependency, unfiltered) of the
    /// `--universe_scope` patterns, or of the expression's own patterns
    /// with `--infer_universe_scope` (UniverseScope.InferredUniverseScope).
    /// Universe patterns that fail to resolve contribute nothing
    /// (PrepareDepsOfPatterns keeps going).
    fn build_universe(&mut self, expression_patterns: &[String]) -> Eval<()> {
        let patterns = if self.options.universe_scope.is_empty() {
            expression_patterns.to_vec()
        } else {
            self.options.universe_scope.clone()
        };
        let mut roots = Set::default();
        for pattern in &patterns {
            if let Ok(targets) = self.resolve_pattern(pattern)? {
                roots.extend(targets);
            }
        }
        self.unfiltered = true;
        let visited = self.visit(&roots, None);
        self.unfiltered = false;
        self.edges.clear();
        let (visited, _) = visited?;
        self.universe_packages = visited.items.iter().map(Label::package_id).collect();
        self.universe = visited;
        Ok(())
    }

    /// GraphBackedRecursivePackageProvider: only universe packages exist.
    fn within_universe(
        &self,
        pattern: &str,
        resolved: Result<Vec<Label>, String>,
    ) -> Result<Vec<Label>, String> {
        let targets = resolved?;
        let recursive =
            crate::target_pattern::parse(pattern, &self.relative_directory).is_ok_and(|parsed| {
                matches!(
                    parsed.kind,
                    crate::target_pattern::Kind::TargetsBelowDirectory { .. }
                )
            });
        if recursive {
            return Ok(targets
                .into_iter()
                .filter(|target| self.universe_packages.contains(&target.package_id()))
                .collect());
        }
        if let Some(outside) = targets
            .iter()
            .find(|target| !self.universe_packages.contains(&target.package_id()))
        {
            let package = if outside.repository.is_empty() {
                outside.package.clone()
            } else {
                format!("@@{}//{}", outside.repository, outside.package)
            };
            return Err(format!(
                "no such package '{package}': BUILD file not found on package path"
            ));
        }
        Ok(targets)
    }

    fn universe_parents(&mut self) -> Eval<HashMap<Label, Vec<Label>>> {
        if let Some(parents) = &self.universe_parents {
            return Ok(parents.clone());
        }
        let universe = self.universe.clone();
        let parents = self.parents_within(&universe)?;
        self.universe_parents = Some(parents.clone());
        Ok(parents)
    }

    /// RBuildFilesFunction / ParallelSkyQueryUtils.getRBuildFilesParallel:
    /// the BUILD files of universe packages whose BUILD file or transitively
    /// loaded `.bzl` files are one of `paths` (package-path relative, so main
    /// repository files only).
    fn rbuildfiles(&mut self, paths: &[String]) -> Eval<Set> {
        let mut packages = Vec::new();
        let mut seen = HashSet::new();
        for label in &self.universe.items {
            let id = label.package_id();
            if seen.insert(id.clone()) {
                packages.push(id);
            }
        }
        let relative = |package: &str, name: &str| {
            if package.is_empty() {
                name.to_owned()
            } else {
                format!("{package}/{name}")
            }
        };
        let mut result = Set::default();
        for id in packages {
            let Some(package) = self.engine.packages.get(&id) else {
                continue;
            };
            let Some(build) = self.build_file_name(&id) else {
                continue;
            };
            let mut matched =
                id.repository.is_empty() && paths.contains(&relative(&id.package, &build));
            let mut stack = package.direct_loads.clone();
            let mut visited = HashSet::new();
            while !matched && let Some(load) = stack.pop() {
                if !visited.insert(load.clone()) {
                    continue;
                }
                if load.repository.is_empty()
                    && paths.contains(&relative(&load.package, &load.name))
                {
                    matched = true;
                }
                if let Some(loads) = package.load_graph.get(&load) {
                    stack.extend(loads.iter().cloned());
                }
            }
            if matched {
                let label = Label {
                    repository: id.repository.clone(),
                    package: id.package.clone(),
                    name: build,
                };
                if self.lookup(&label)?.is_ok() {
                    result.insert(label);
                }
            }
        }
        Ok(result)
    }

    /// Reverse edges among `closure` (PathLabelVisitor ALLPATHS parent map).
    fn parents_within(&mut self, closure: &Set) -> Eval<HashMap<Label, Vec<Label>>> {
        let mut parents: HashMap<Label, Vec<Label>> = HashMap::new();
        for from in &closure.items {
            for to in self.successors(from)? {
                if closure.contains(&to) {
                    let entry = parents.entry(to).or_default();
                    if !entry.contains(from) {
                        entry.push(from.clone());
                    }
                }
            }
        }
        Ok(parents)
    }

    /// TargetProvider.getSiblingTargetsInPackage: every target of the
    /// package, in name order.
    fn siblings(&mut self, target: &Label) -> Eval<Vec<Label>> {
        let id = target.package_id();
        if self.kind(target) == Kind::Load {
            return Ok(Vec::new());
        }
        self.load_package(&id).map_err(Failure::Fatal)?;
        let names = self.all_target_names(&id);
        let mut labels = Vec::new();
        for name in names {
            let label = Label {
                repository: id.repository.clone(),
                package: id.package.clone(),
                name,
            };
            if self.lookup(&label)?.is_ok() {
                labels.push(label);
            }
        }
        Ok(labels)
    }

    /// Target.getTargetKind.
    fn target_kind(&self, label: &Label) -> String {
        match self.kind(label) {
            Kind::Rule => format!("{} rule", self.rule(label).rule_class_name()),
            Kind::PackageGroup => "package group".to_owned(),
            Kind::Input | Kind::Load => "source file".to_owned(),
            Kind::Output(_) => "generated file".to_owned(),
        }
    }

    /// BlazeTargetAccessor.getPrerequisites (BlazeTargetAccessor.java:57-82):
    /// the reachable labels of one attribute, select keys excluded.
    fn prerequisites(
        &mut self,
        caller: &Expr,
        rule: &Label,
        name: &str,
        prefix: &str,
    ) -> Eval<Vec<Label>> {
        let attributes = self.rule_attributes(rule);
        let Some(attribute) = attributes.iter().find(|attribute| attribute.name == name) else {
            return Ok(Vec::new());
        };
        let mut texts = Vec::new();
        if let Some(parts) = &attribute.select {
            for part in parts {
                match part {
                    ConfigurablePart::Plain(value) => {
                        texts.extend(query_attrs::value_labels(value, attribute.shape))
                    }
                    ConfigurablePart::Select { branches, .. } => {
                        for (_, value) in branches {
                            if let Some(value) = value.as_ref().or(attribute.default.as_ref()) {
                                texts.extend(query_attrs::value_labels(value, attribute.shape));
                            }
                        }
                    }
                }
            }
        } else if let Some(value) = &attribute.value {
            texts.extend(query_attrs::value_labels(value, attribute.shape));
        }
        let mut labels = Vec::new();
        for text in texts {
            let label = self.parse_label(rule, &text).map_err(Failure::Fatal)?;
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        let mut result = Vec::new();
        for label in labels {
            match self.lookup(&label)? {
                Ok(_) => result.push(label),
                Err(error) => self.handle_error(caller, format!("{prefix}{error}"))?,
            }
        }
        Ok(result)
    }

    /// TargetUtils.getAttrAsString (TargetUtils.java:176-210): every possible
    /// value of an attribute in Java `toString` form.
    fn attribute_strings(&mut self, label: &Label, name: &str) -> Vec<Option<String>> {
        if self.kind(label) != Kind::Rule {
            return Vec::new();
        }
        let attributes = self.rule_attributes(label);
        let Some(attribute) = attributes.iter().find(|attribute| attribute.name == name) else {
            return Vec::new();
        };
        possible_values(attribute)
            .iter()
            .map(|value| java_string(value, attribute.shape, label))
            .collect()
    }

    /// TestsFunction (query2/engine/TestsFunction.java).
    fn tests(&mut self, expr: &Expr, input: Set) -> Eval<Set> {
        let mut tests = Set::default();
        let mut suites_seen = HashSet::new();
        let mut queue = VecDeque::new();
        for target in input.items {
            if self.is_test(&target) {
                tests.insert(target);
            } else if self.is_test_suite(&target) && suites_seen.insert(target.clone()) {
                queue.push_back(target);
            }
        }
        while let Some(suite) = queue.pop_front() {
            let tags = query_attrs::rule_tags(self.rule(&suite));
            let (required, excluded) = query_attrs::sort_tags_by_sense(&tags);
            let prefix = |attribute: &str| {
                format!(
                    "couldn't expand '{attribute}' attribute of test_suite {}: ",
                    crate::rule_declarations::canonical_form(&suite)
                )
            };
            let listed = self.prerequisites(expr, &suite, "tests", &prefix("tests"))?;
            for target in listed {
                if self.is_test(&target) {
                    if self.include_test(&required, &excluded, &target) {
                        tests.insert(target);
                    }
                } else if self.is_test_suite(&target) {
                    if suites_seen.insert(target.clone()) {
                        queue.push_back(target);
                    }
                } else if self.options.strict_test_suite {
                    let message = format!(
                        "The label '{}' in the test_suite '{}' does not refer to a test or test_suite rule!",
                        crate::rule_declarations::canonical_form(&target),
                        crate::rule_declarations::canonical_form(&suite)
                    );
                    self.handle_error(expr, message)?;
                }
            }
            let implicit =
                self.prerequisites(expr, &suite, "$implicit_tests", &prefix("$implicit_tests"))?;
            for target in implicit {
                if self.is_test(&target) && self.include_test(&required, &excluded, &target) {
                    tests.insert(target);
                }
            }
        }
        Ok(tests)
    }

    fn is_test(&self, label: &Label) -> bool {
        self.kind(label) == Kind::Rule && self.rule(label).rule_class_name().ends_with("_test")
    }

    fn is_test_suite(&self, label: &Label) -> bool {
        self.kind(label) == Kind::Rule && self.rule(label).rule_class_name() == "test_suite"
    }

    fn include_test(&self, required: &[String], excluded: &[String], test: &Label) -> bool {
        let rule = self.rule(test);
        let mut tags = query_attrs::rule_tags(rule);
        tags.push(query_attrs::rule_size(rule));
        excluded.iter().all(|tag| !tags.contains(tag))
            && required.iter().all(|tag| tags.contains(tag))
    }

    /// VisibleFunction.visible (VisibleFunction.java:86-96) with
    /// BlazeTargetAccessor.getVisibility (:116-170).
    fn is_visible(&mut self, caller: &Expr, consumer: &Label, target: &Label) -> Eval<bool> {
        if consumer.package_id() == target.package_id() {
            return Ok(true);
        }
        let kind = self.kind(target);
        let visibility = self.visibility_of(target, &kind);
        let declaring = target.package_id();
        let requesting = consumer.package_id();
        match self
            .engine
            .visible(&visibility, &declaring, Some(&requesting))
        {
            Ok(visible) => Ok(visible),
            Err(error) => {
                let group = visibility
                    .group_labels()
                    .next()
                    .map(crate::rule_declarations::canonical_form)
                    .unwrap_or_default();
                self.handle_error(
                    caller,
                    format!("Invalid visibility label '{group}': {error}"),
                )?;
                Ok(false)
            }
        }
    }

    /// AbstractBlazeQueryEnvironment.transitiveLoadFiles (:400-450).
    fn load_files(&mut self, input: Set, build_files: bool) -> Eval<Set> {
        let mut seen_packages = HashSet::new();
        let mut seen_bzl = HashSet::new();
        let mut result = Set::default();
        for target in input.items {
            let id = target.package_id();
            if self.kind(&target) == Kind::Load || !seen_packages.insert(id.clone()) {
                continue;
            }
            self.load_package(&id).map_err(Failure::Fatal)?;
            let package = &self.engine.packages[&id];
            let build = package.build_file.clone();
            let mut stack = package
                .direct_loads
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>();
            let graph = package.load_graph.clone();
            if build_files {
                let name = build
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "BUILD".to_owned());
                let label = Label {
                    repository: id.repository.clone(),
                    package: id.package.clone(),
                    name,
                };
                self.add_load_target(&mut result, label, &id);
            }
            // Package.Declarations.visitLoadGraph: preorder over direct loads.
            while let Some(bzl) = stack.pop() {
                if !seen_bzl.insert(bzl.clone()) {
                    continue;
                }
                self.add_load_target(&mut result, bzl.clone(), &id);
                if build_files {
                    let bzl_package = bzl.package_id();
                    if let Ok(root) = self.engine.repositories.root(&bzl_package.repository)
                        && let Ok(file) =
                            crate::workspace::package_build_file(root, &bzl_package.package)
                    {
                        let name = file
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let label = Label {
                            repository: bzl_package.repository.clone(),
                            package: bzl_package.package.clone(),
                            name,
                        };
                        self.add_load_target(&mut result, label, &id);
                    }
                }
                if let Some(children) = graph.get(&bzl) {
                    stack.extend(children.iter().rev().cloned());
                }
            }
        }
        Ok(result)
    }

    fn add_load_target(&mut self, result: &mut Set, label: Label, owner: &PackageId) {
        if !self.kinds.contains_key(&label) {
            self.load_targets.insert(label.clone(), owner.clone());
        }
        self.graph_nodes.insert(label.clone());
        result.insert(label);
    }

    // ------------------------------------------------------------------
    // Output.

    /// LabelPrinter.displayForm / LabelPrinter.starlark
    /// (CommonQueryOptions.getLabelPrinter).
    fn print_label(&self, label: &Label) -> String {
        format!(
            "{}//{}:{}",
            self.repository_form(&label.repository),
            label.package,
            label.name
        )
    }

    fn repository_form(&self, repository: &str) -> String {
        if self.options.consistent_labels {
            return format!("@@{repository}");
        }
        if repository.is_empty() {
            return String::new();
        }
        // RepositoryName.getDisplayForm: the first apparent name of the main
        // repository mapping, in its entry order, that maps to it, else the
        // canonical name.
        if let Ok(mapping) = self.engine.repositories.mapping("")
            && let Some(apparent) = first_apparent(&self.main_mapping_order, mapping, repository)
        {
            return format!("@{apparent}");
        }
        format!("@@{repository}")
    }

    fn output(&mut self, result: &Set, streamed: bool, sort: bool) -> Eval<Vec<u8>> {
        let terminator = self.options.line_terminator();
        let output = self.options.output.clone();
        let ordered = if streamed {
            let mut items = result.items.clone();
            if sort {
                items.sort();
            }
            items
        } else {
            // BlazeQueryEnvironment: the subgraph of the result.
            let graph = self.subgraph(result);
            match output.as_str() {
                "minrank" => return Ok(self.minrank(&graph).into_bytes()),
                "maxrank" => return Ok(self.maxrank(&graph).into_bytes()),
                "graph" => return Ok(self.graph_output(&graph).into_bytes()),
                // AbstractUnorderedFormatter.getOrderedTargets: the full
                // (label-ordered) topological order; DEPS order is an
                // arbitrary topological order in Bazel and uses the same
                // deterministic order here.
                _ => topological_order(&graph),
            }
        };
        if matches!(
            output.as_str(),
            "xml" | "proto" | "streamed_proto" | "streamed_jsonproto"
        ) {
            return self.structured_output(&ordered);
        }
        let mut out = String::new();
        match output.as_str() {
            "label" => {
                for label in &ordered {
                    out.push_str(&self.print_label(label));
                    out.push_str(terminator);
                }
            }
            "label_kind" => {
                for label in &ordered {
                    out.push_str(&format!(
                        "{} {}{terminator}",
                        self.target_kind(label),
                        self.print_label(label)
                    ));
                }
            }
            "package" => {
                let packages = ordered
                    .iter()
                    .map(|label| {
                        let form = format!(
                            "{}//{}",
                            self.repository_form(&label.repository),
                            label.package
                        );
                        form.strip_prefix("//").map(str::to_owned).unwrap_or(form)
                    })
                    .collect::<BTreeSet<_>>();
                for package in packages {
                    out.push_str(&package);
                    out.push_str(terminator);
                }
            }
            "location" => {
                for label in &ordered {
                    out.push_str(&format!(
                        "{}: {} {}{terminator}",
                        self.location(label),
                        self.target_kind(label),
                        self.print_label(label)
                    ));
                }
            }
            "build" => {
                let mut printed = HashSet::new();
                for label in &ordered {
                    let rule = match self.kind(label) {
                        Kind::Rule => label.clone(),
                        Kind::Output(rule) => rule,
                        _ => continue,
                    };
                    if printed.insert(rule.clone()) {
                        out.push_str(&self.build_rule(&rule, terminator));
                    }
                }
            }
            _ => unreachable!("formatters are validated before evaluation"),
        }
        Ok(out.into_bytes())
    }

    /// FormatUtils.getLocation (query2/query/output/FormatUtils.java:47-58).
    fn location(&self, label: &Label) -> String {
        let location = match self.kind(label) {
            Kind::Rule | Kind::PackageGroup => self
                .rule(label)
                .location
                .clone()
                .unwrap_or_else(|| self.input_location(label)),
            Kind::Output(rule) => self
                .rule(&rule)
                .location
                .clone()
                .unwrap_or_else(|| self.input_location(&rule)),
            Kind::Input => {
                let root = self
                    .engine
                    .repositories
                    .root(&label.repository)
                    .map(|root| root.to_path_buf())
                    .unwrap_or_default();
                format!(
                    "{}:1:1",
                    root.join(&label.package).join(&label.name).display()
                )
            }
            Kind::Load => String::new(),
        };
        if !self.options.relative_locations {
            return location;
        }
        // FormatUtils.getRootRelativeLocation.
        match self.engine.repositories.root(&label.repository) {
            Ok(root) => {
                let prefix = format!("{}/", root.display());
                location
                    .strip_prefix(&prefix)
                    .map(str::to_owned)
                    .unwrap_or(location)
            }
            Err(_) => location,
        }
    }

    /// Digraph.extractSubgraph (graph/Digraph.java:624-700): the result's
    /// nodes, with edges preserving the transitive closure through removed
    /// nodes.
    fn subgraph(&self, result: &Set) -> BTreeMap<Label, BTreeSet<Label>> {
        let mut graph = BTreeMap::new();
        for node in &result.items {
            let mut successors = BTreeSet::new();
            let mut stack = self
                .graph_edges
                .get(node)
                .map(|edges| edges.items.clone())
                .unwrap_or_default();
            let mut seen = HashSet::new();
            while let Some(next) = stack.pop() {
                if !seen.insert(next.clone()) {
                    continue;
                }
                if result.contains(&next) {
                    successors.insert(next);
                } else if let Some(edges) = self.graph_edges.get(&next) {
                    stack.extend(edges.items.iter().cloned());
                }
            }
            graph.insert(node.clone(), successors);
        }
        graph
    }

    /// MinrankOutputFormatter (query2/query/output/MinrankOutputFormatter.java):
    /// ranks over the strongly connected component graph, sorted by
    /// (rank, label) (OrderOutput.FULL; other orders iterate hash sets in
    /// Bazel and are rendered in this deterministic order).
    fn minrank(&self, graph: &BTreeMap<Label, BTreeSet<Label>>) -> String {
        let terminator = self.options.line_terminator();
        let (components, component_of) = strongly_connected(graph);
        let edges = component_edges(graph, &components, &component_of);
        let mut predecessors = vec![0usize; components.len()];
        for successors in &edges {
            for &next in successors {
                predecessors[next] += 1;
            }
        }
        let mut rank_nodes = (0..components.len())
            .filter(|&index| predecessors[index] == 0)
            .collect::<Vec<_>>();
        let mut seen = rank_nodes.iter().copied().collect::<HashSet<_>>();
        let mut output = Vec::new();
        let mut rank = 0;
        while !rank_nodes.is_empty() {
            let mut next_nodes = Vec::new();
            for &component in &rank_nodes {
                for label in &components[component] {
                    output.push((rank, label.clone()));
                }
                for &next in &edges[component] {
                    if seen.insert(next) {
                        next_nodes.push(next);
                    }
                }
            }
            rank_nodes = next_nodes;
            rank += 1;
        }
        output.sort();
        output
            .into_iter()
            .map(|(rank, label)| format!("{rank} {}{terminator}", self.print_label(&label)))
            .collect()
    }

    /// MaxrankOutputFormatter: the longest path from a root of the strongly
    /// connected component graph.
    fn maxrank(&self, graph: &BTreeMap<Label, BTreeSet<Label>>) -> String {
        let terminator = self.options.line_terminator();
        let (components, component_of) = strongly_connected(graph);
        let edges = component_edges(graph, &components, &component_of);
        let mut predecessors = vec![Vec::new(); components.len()];
        for (from, successors) in edges.iter().enumerate() {
            for &to in successors {
                predecessors[to].push(from);
            }
        }
        fn rank(node: usize, predecessors: &[Vec<usize>], ranks: &mut [Option<i64>]) -> i64 {
            if let Some(rank) = ranks[node] {
                return rank;
            }
            let mut max = -1;
            for &p in &predecessors[node] {
                max = max.max(rank(p, predecessors, ranks));
            }
            ranks[node] = Some(max + 1);
            max + 1
        }
        let mut ranks = vec![None; components.len()];
        let mut output = Vec::new();
        for (index, component) in components.iter().enumerate() {
            let rank = rank(index, &predecessors, &mut ranks);
            for label in component {
                output.push((rank, label.clone()));
            }
        }
        output.sort();
        output
            .into_iter()
            .map(|(rank, label)| format!("{rank} {}{terminator}", self.print_label(&label)))
            .collect()
    }

    /// GraphOutputFormatter / GraphOutputWriter
    /// (query2/query/output/GraphOutputWriter.java), in the sorted (FULL)
    /// order; Bazel's other orders iterate hash sets.
    fn graph_output(&mut self, graph: &BTreeMap<Label, BTreeSet<Label>>) -> String {
        let terminator = self.options.line_terminator();
        let conditions = self.conditional_edges(graph);
        let mut out = format!("digraph mygraph {{{terminator}  node [shape=box];{terminator}");
        let condition_label = |from: &[Label], to: &[Label]| -> String {
            let limit = self.options.graph_conditional_edges_limit;
            if limit == 0 {
                return String::new();
            }
            let mut buffer = String::new();
            let mut annotated = HashSet::new();
            for source in from {
                for destination in to {
                    let Some(labels) = conditions.get(&(source.clone(), destination.clone()))
                    else {
                        continue;
                    };
                    let take = if limit == -1 {
                        labels.len()
                    } else {
                        (limit - 1).max(0) as usize
                    };
                    let mut first = true;
                    for condition in labels.iter().take(take) {
                        if !annotated.insert(condition.clone()) {
                            continue;
                        }
                        if !first {
                            buffer.push_str("\\n");
                        }
                        buffer.push_str(&crate::rule_declarations::canonical_form(condition));
                        first = false;
                    }
                    if labels.len() > take {
                        buffer.push_str("...");
                    }
                }
            }
            buffer
        };
        if !self.options.graph_factored {
            for node in topological_order(graph) {
                let from = self.print_label(&node);
                out.push_str(&format!("  \"{from}\"{terminator}"));
                for successor in &graph[&node] {
                    out.push_str(&format!(
                        "  \"{from}\" -> \"{}\"{terminator}",
                        self.print_label(successor)
                    ));
                    let label = condition_label(
                        std::slice::from_ref(&node),
                        std::slice::from_ref(successor),
                    );
                    if !label.is_empty() {
                        out.push_str(&format!("  [label=\"{label}\"];{terminator}"));
                    }
                }
            }
            out.push_str(&format!("}}{terminator}"));
            return out;
        }
        // partitionFactored: nodes with equal predecessor and successor sets.
        let mut predecessors: BTreeMap<Label, BTreeSet<Label>> = graph
            .keys()
            .map(|node| (node.clone(), BTreeSet::new()))
            .collect();
        for (from, successors) in graph {
            for to in successors {
                predecessors.get_mut(to).map(|set| set.insert(from.clone()));
            }
        }
        let mut classes: BTreeMap<(Vec<Label>, Vec<Label>), Vec<Label>> = BTreeMap::new();
        for node in graph.keys() {
            let key = (
                predecessors[node].iter().cloned().collect::<Vec<_>>(),
                graph[node].iter().cloned().collect::<Vec<_>>(),
            );
            classes.entry(key).or_default().push(node.clone());
        }
        let mut partition = classes.into_values().collect::<Vec<_>>();
        partition.sort();
        let class_of = partition
            .iter()
            .enumerate()
            .flat_map(|(index, class)| class.iter().map(move |label| (label.clone(), index)))
            .collect::<HashMap<_, _>>();
        let mut class_graph: BTreeMap<Vec<Label>, BTreeSet<Vec<Label>>> = BTreeMap::new();
        for class in &partition {
            let successors = class
                .iter()
                .flat_map(|node| graph[node].iter())
                .map(|successor| partition[class_of[successor]].clone())
                .collect::<BTreeSet<_>>();
            class_graph.insert(class.clone(), successors);
        }
        let node_limit = self.options.graph_node_limit;
        let serialize = |class: &Vec<Label>| -> String {
            let reserved = "\\n...and 9999999 more items".len() as i64;
            let actual = i64::from(node_limit) - reserved;
            let mut buffer = String::new();
            for (count, label) in class.iter().enumerate() {
                let text = self.print_label(label);
                if count > 0 {
                    buffer.push_str("\\n");
                    if node_limit != -1 && (buffer.len() + text.len()) as i64 > actual {
                        buffer.push_str(&format!("...and {} more items", class.len() - count));
                        break;
                    }
                }
                buffer.push_str(&text);
            }
            buffer
        };
        for class in topological_order(&class_graph) {
            let from = serialize(&class);
            out.push_str(&format!("  \"{from}\"{terminator}"));
            for successor in &class_graph[&class] {
                out.push_str(&format!(
                    "  \"{from}\" -> \"{}\"{terminator}",
                    serialize(successor)
                ));
                let label = condition_label(&class, successor);
                if !label.is_empty() {
                    out.push_str(&format!("  [label=\"{label}\"];{terminator}"));
                }
            }
        }
        out.push_str(&format!("}}{terminator}"));
        out
    }

    /// ConditionalEdges (query2/query/output/ConditionalEdges.java): for
    /// each rule, the select() conditions under which a successor is a
    /// dependency (list/label branch values only).
    fn conditional_edges(
        &mut self,
        graph: &BTreeMap<Label, BTreeSet<Label>>,
    ) -> HashMap<(Label, Label), Vec<Label>> {
        let mut result: HashMap<(Label, Label), Vec<Label>> = HashMap::new();
        for (node, successors) in graph {
            let rule = match self.kind(node) {
                Kind::Rule => node.clone(),
                Kind::Output(rule) => rule,
                _ => continue,
            };
            let attributes = self.rule_attributes(&rule);
            let mut conditions: HashMap<Label, Vec<Label>> = HashMap::new();
            for attribute in attributes.iter() {
                let Some(parts) = &attribute.select else {
                    continue;
                };
                for part in parts {
                    let ConfigurablePart::Select { branches, .. } = part else {
                        continue;
                    };
                    for (key, value) in branches {
                        let Some(value) = value else { continue };
                        let Ok(key) = self.parse_label(&rule, key) else {
                            continue;
                        };
                        let labels = match value {
                            AttrValue::Labels(labels) => labels.clone(),
                            AttrValue::Label(Some(label)) => vec![label.clone()],
                            _ => Vec::new(),
                        };
                        for text in labels {
                            if let Ok(dependency) = self.parse_label(&rule, &text) {
                                let entry = conditions.entry(dependency).or_default();
                                if !entry.contains(&key) {
                                    entry.push(key.clone());
                                }
                            }
                        }
                    }
                }
            }
            for successor in successors {
                if let Some(keys) = conditions.get(successor) {
                    let mut keys = keys.clone();
                    keys.sort();
                    result.insert((node.clone(), successor.clone()), keys);
                }
            }
        }
        result
    }

    /// BuildOutputFormatter.TargetOutputter.outputRule
    /// (query2/query/output/BuildOutputFormatter.java:66-110).
    fn build_rule(&mut self, label: &Label, terminator: &str) -> String {
        let attributes = self.rule_attributes(label);
        let rule = self.rule(label);
        let mut out = String::new();
        out.push_str(&format!(
            "# {}{terminator}",
            rule.location
                .clone()
                .unwrap_or_else(|| self.input_location(label))
        ));
        out.push_str(&format!("{}({terminator}", rule.rule_class_name()));
        out.push_str(&format!("  name = \"{}\",{terminator}", label.name));
        for attribute in attributes.iter().skip(1) {
            if attribute.name == "visibility" {
                if rule.explicit_visibility {
                    let labels = rule
                        .visibility
                        .declared_labels()
                        .iter()
                        .map(|label| starlark_repr_string(&self.print_label(label)))
                        .collect::<Vec<_>>();
                    out.push_str(&format!(
                        "  visibility = [{}],{terminator}",
                        labels.join(", ")
                    ));
                }
                continue;
            }
            if let Some(parts) = &attribute.select {
                let rendered = parts
                    .iter()
                    .map(|part| match part {
                        ConfigurablePart::Plain(value) => self.repr(value, attribute, label),
                        ConfigurablePart::Select { branches, .. } => {
                            let entries = branches
                                .iter()
                                .map(|(key, value)| {
                                    let key = self
                                        .parse_label(label, key)
                                        .map(|key| self.print_label(&key))
                                        .unwrap_or_else(|_| key.clone());
                                    let value = value
                                        .as_ref()
                                        .or(attribute.default.as_ref())
                                        .map(|value| self.repr(value, attribute, label))
                                        .unwrap_or_else(|| "None".to_owned());
                                    format!("{}: {value}", starlark_repr_string(&key))
                                })
                                .collect::<Vec<_>>();
                            format!("select({{{}}})", entries.join(", "))
                        }
                    })
                    .collect::<Vec<_>>();
                out.push_str(&format!(
                    "  {} = {},{terminator}",
                    attribute.public_name,
                    rendered.join(" + ")
                ));
                continue;
            }
            if !attribute.explicit {
                continue;
            }
            let Some(value) = &attribute.value else {
                continue;
            };
            out.push_str(&format!(
                "  {} = {},{terminator}",
                attribute.public_name,
                self.repr(value, attribute, label)
            ));
        }
        out.push_str(&format!("){terminator}"));
        // appendStack (BuildOutputFormatter.java:112-126).
        let mut stack = |title: String, entries: &[(String, String)]| {
            let width = entries.iter().map(|(_, location)| location.len()).max();
            let Some(width) = width.filter(|width| *width > 0) else {
                return;
            };
            out.push_str(&title);
            out.push_str(terminator);
            for (name, location) in entries {
                out.push_str(&format!("#   {location:<width$} in {name}{terminator}"));
            }
        };
        stack(
            format!(
                "# Rule {} instantiated at (most recent call last):",
                label.name
            ),
            &rule.call_stack,
        );
        if let Some(custom) = &rule.custom {
            stack(
                format!(
                    "# Rule {} defined at (most recent call last):",
                    rule.rule_class_name()
                ),
                &custom.declaration.definition_stack,
            );
        }
        out.push_str(terminator);
        out
    }

    /// Printer.repr with labels in LabelPrinter form (BuildOutputFormatter
    /// outputRawAttrValue, :128-146).
    fn repr(&self, value: &AttrValue, attribute: &QueryAttribute, owner: &Label) -> String {
        let label = |text: &String| {
            let printed = self
                .parse_label(owner, text)
                .map(|label| self.print_label(&label))
                .unwrap_or_else(|_| text.clone());
            starlark_repr_string(&printed)
        };
        let output = |name: &String| {
            starlark_repr_string(&self.print_label(&Label {
                repository: owner.repository.clone(),
                package: owner.package.clone(),
                name: name.clone(),
            }))
        };
        let list = |items: Vec<String>| format!("[{}]", items.join(", "));
        match value {
            AttrValue::String(text) => starlark_repr_string(text),
            AttrValue::Strings(items) => list(
                items
                    .iter()
                    .map(|item| starlark_repr_string(item))
                    .collect(),
            ),
            AttrValue::Int(number) => number.to_string(),
            AttrValue::Bool(value) => if *value { "True" } else { "False" }.to_owned(),
            AttrValue::Ints(items) => list(items.iter().map(ToString::to_string).collect()),
            AttrValue::Label(None) | AttrValue::Output(None) => "None".to_owned(),
            AttrValue::Label(Some(text)) => label(text),
            AttrValue::Labels(items) => list(items.iter().map(label).collect()),
            AttrValue::Output(Some(name)) => output(name),
            AttrValue::Outputs(names) => {
                if attribute.shape == Shape::Output {
                    list(names.iter().map(output).collect())
                } else {
                    list(
                        names
                            .iter()
                            .map(|name| starlark_repr_string(name))
                            .collect(),
                    )
                }
            }
            AttrValue::StringDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!(
                        "{}: {}",
                        starlark_repr_string(k),
                        starlark_repr_string(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            AttrValue::StringListDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!(
                        "{}: {}",
                        starlark_repr_string(k),
                        list(v.iter().map(|item| starlark_repr_string(item)).collect())
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            AttrValue::LabelKeyedStringDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", label(k), starlark_repr_string(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            AttrValue::StringKeyedLabelDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", starlark_repr_string(k), label(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            AttrValue::LabelListDict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!(
                        "{}: {}",
                        starlark_repr_string(k),
                        list(v.iter().map(label).collect())
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// RepositoryMapping.getInverse (cmdline/RepositoryMapping.java:142-150): the
/// first apparent name in mapping entry order (`order`, from
/// RepositorySet::main_mapping_order) that maps to `repository`; names the
/// order does not list fall back to the name-ordered native mapping.
fn first_apparent(
    order: &[String],
    mapping: &BTreeMap<String, String>,
    repository: &str,
) -> Option<String> {
    order
        .iter()
        .find(|name| mapping.get(name.as_str()).map(String::as_str) == Some(repository))
        .cloned()
        .or_else(|| {
            mapping
                .iter()
                .find(|(_, canonical)| canonical.as_str() == repository)
                .map(|(apparent, _)| apparent.clone())
        })
}

/// PatternSyntaxException.getMessage for the common syntax errors
/// (java.util.regex.Pattern): `DESC near index I`, the pattern, and a caret
/// line when the index is inside the pattern. Other errors keep the
/// fancy-regex text.
pub(crate) fn java_pattern_error(pattern: &str, error: &fancy_regex::Error) -> String {
    let length = pattern.chars().count();
    let (description, index) = match error {
        fancy_regex::Error::ParseError(_, fancy_regex::ParseError::UnclosedOpenParen) => {
            ("Unclosed group".to_owned(), length)
        }
        _ if pattern.starts_with(['*', '+', '?']) => (
            format!(
                "Dangling meta character '{}'",
                pattern.chars().next().unwrap_or('*')
            ),
            0,
        ),
        _ if pattern.contains('[') && !pattern.contains(']') => (
            "Unclosed character class".to_owned(),
            length.saturating_sub(1),
        ),
        _ => return error.to_string(),
    };
    let mut message = format!("{description} near index {index}\n{pattern}");
    if index < length {
        message.push('\n');
        message.push_str(&" ".repeat(index));
        message.push('^');
    }
    message
}

/// Starlark `repr` of a string (net/starlark/java/eval/Printer.java).
fn starlark_repr_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// AggregatingAttributeMapper.visitAttribute: every possible value of an
/// attribute; concatenated selects yield each combination.
fn possible_values(attribute: &QueryAttribute) -> Vec<AttrValue> {
    let Some(parts) = &attribute.select else {
        return attribute.value.iter().cloned().collect();
    };
    let mut combinations: Vec<Option<AttrValue>> = vec![None];
    for part in parts {
        let options = match part {
            ConfigurablePart::Plain(value) => vec![Some(value.clone())],
            ConfigurablePart::Select { branches, .. } => branches
                .iter()
                .map(|(_, value)| value.clone().or_else(|| attribute.default.clone()))
                .collect(),
        };
        let mut next = Vec::new();
        for combination in &combinations {
            for option in &options {
                next.push(concat(combination.clone(), option.clone()));
            }
        }
        combinations = next;
        if combinations.len() > 4096 {
            break;
        }
    }
    let mut unique = Vec::new();
    for value in combinations.into_iter().flatten() {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

fn concat(left: Option<AttrValue>, right: Option<AttrValue>) -> Option<AttrValue> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(AttrValue::Labels(mut a)), Some(AttrValue::Labels(b))) => {
            a.extend(b);
            Some(AttrValue::Labels(a))
        }
        (Some(AttrValue::Strings(mut a)), Some(AttrValue::Strings(b))) => {
            a.extend(b);
            Some(AttrValue::Strings(a))
        }
        (Some(AttrValue::StringDict(mut a)), Some(AttrValue::StringDict(b))) => {
            a.extend(b);
            Some(AttrValue::StringDict(a))
        }
        (_, right) => right,
    }
}

/// Java `toString` of an attribute value (TargetUtils.convertAttributeValue).
fn java_string(value: &AttrValue, shape: Shape, owner: &Label) -> Option<String> {
    let label = |text: &String| {
        Label::parse_canonical(text)
            .map(|label| crate::rule_declarations::canonical_form(&label))
            .unwrap_or_else(|_| text.clone())
    };
    let output = |name: &String| {
        crate::rule_declarations::canonical_form(&Label {
            repository: owner.repository.clone(),
            package: owner.package.clone(),
            name: name.clone(),
        })
    };
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    Some(match value {
        AttrValue::Bool(value) => if *value { "1" } else { "0" }.to_owned(),
        AttrValue::Int(number) => number.to_string(),
        AttrValue::String(text) => text.clone(),
        AttrValue::Strings(items) => list(items.clone()),
        AttrValue::Ints(items) => list(items.iter().map(ToString::to_string).collect()),
        AttrValue::Label(None) | AttrValue::Output(None) => return None,
        AttrValue::Label(Some(text)) => label(text),
        AttrValue::Labels(items) => list(items.iter().map(label).collect()),
        AttrValue::Output(Some(name)) => output(name),
        AttrValue::Outputs(names) if shape == Shape::Output => {
            list(names.iter().map(output).collect())
        }
        AttrValue::Outputs(names) => list(names.clone()),
        AttrValue::StringDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::StringListDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}={}", list(v.clone())))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::LabelKeyedStringDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{}={v}", label(k)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::StringKeyedLabelDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}={}", label(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AttrValue::LabelListDict(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, v)| format!("{k}={}", list(v.iter().map(label).collect())))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Digraph.getTopologicalOrder(comparator) (graph/Digraph.java:527-539):
/// reverse postorder of a depth-first search started from every node in
/// sorted order, successors in sorted order.
fn topological_order<T: Ord + Clone>(graph: &BTreeMap<T, BTreeSet<T>>) -> Vec<T> {
    let mut marked = BTreeSet::new();
    let mut postorder = Vec::new();
    for start in graph.keys() {
        if marked.contains(start) {
            continue;
        }
        // Iterative DFS: (node, successor iterator position).
        let mut stack: Vec<(T, Vec<T>, usize)> = Vec::new();
        marked.insert(start.clone());
        stack.push((start.clone(), graph[start].iter().cloned().collect(), 0));
        while let Some((node, successors, index)) = stack.last_mut() {
            if *index < successors.len() {
                let next = successors[*index].clone();
                *index += 1;
                if marked.insert(next.clone()) {
                    let children = graph
                        .get(&next)
                        .map(|set| set.iter().cloned().collect())
                        .unwrap_or_default();
                    stack.push((next, children, 0));
                }
            } else {
                postorder.push(node.clone());
                stack.pop();
            }
        }
    }
    postorder.reverse();
    postorder
}

/// Strongly connected components (Tarjan), each sorted by label.
fn strongly_connected(
    graph: &BTreeMap<Label, BTreeSet<Label>>,
) -> (Vec<Vec<Label>>, HashMap<Label, usize>) {
    let nodes = graph.keys().cloned().collect::<Vec<_>>();
    let index_of = nodes
        .iter()
        .enumerate()
        .map(|(i, label)| (label.clone(), i))
        .collect::<HashMap<_, _>>();
    let successors = nodes
        .iter()
        .map(|node| graph[node].iter().map(|s| index_of[s]).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let n = nodes.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut counter = 0;
    let mut components = Vec::new();
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut work = vec![(start, 0usize)];
        index[start] = counter;
        low[start] = counter;
        counter += 1;
        stack.push(start);
        on_stack[start] = true;
        while let Some(&mut (node, ref mut position)) = work.last_mut() {
            if *position < successors[node].len() {
                let next = successors[node][*position];
                *position += 1;
                if index[next] == usize::MAX {
                    index[next] = counter;
                    low[next] = counter;
                    counter += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    work.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
            } else {
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[node]);
                }
                if low[node] == index[node] {
                    let mut component = Vec::new();
                    while let Some(member) = stack.pop() {
                        on_stack[member] = false;
                        component.push(nodes[member].clone());
                        if member == node {
                            break;
                        }
                    }
                    component.sort();
                    components.push(component);
                }
            }
        }
    }
    let component_of = components
        .iter()
        .enumerate()
        .flat_map(|(i, component)| component.iter().map(move |label| (label.clone(), i)))
        .collect();
    (components, component_of)
}

/// The edges of the component graph, without self edges.
fn component_edges(
    graph: &BTreeMap<Label, BTreeSet<Label>>,
    components: &[Vec<Label>],
    component_of: &HashMap<Label, usize>,
) -> Vec<BTreeSet<usize>> {
    let mut edges = vec![BTreeSet::new(); components.len()];
    for (from, successors) in graph {
        let a = component_of[from];
        for to in successors {
            let b = component_of[to];
            if a != b {
                edges[a].insert(b);
            }
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(name: &str) -> Label {
        Label::parse_canonical(&format!("//p:{name}")).unwrap()
    }

    fn graph(edges: &[(&str, &str)], nodes: &[&str]) -> BTreeMap<Label, BTreeSet<Label>> {
        let mut graph = BTreeMap::new();
        for node in nodes {
            graph.entry(label(node)).or_insert_with(BTreeSet::new);
        }
        for (from, to) in edges {
            graph
                .entry(label(from))
                .or_insert_with(BTreeSet::new)
                .insert(label(to));
            graph.entry(label(to)).or_insert_with(BTreeSet::new);
        }
        graph
    }

    #[test]
    fn topological_order_is_reverse_sorted_postorder() {
        let g = graph(&[("a", "c"), ("b", "c"), ("c", "d")], &[]);
        let order = topological_order(&g)
            .into_iter()
            .map(|l| l.name)
            .collect::<Vec<_>>();
        // DFS from a: d, c, a; from b: b. Reversed: b a c d.
        assert_eq!(order, ["b", "a", "c", "d"]);
    }

    #[test]
    fn components_collapse_cycles() {
        let g = graph(&[("a", "b"), ("b", "a"), ("b", "c")], &[]);
        let (components, of) = strongly_connected(&g);
        assert_eq!(components.len(), 2);
        assert_eq!(of[&label("a")], of[&label("b")]);
        assert_ne!(of[&label("a")], of[&label("c")]);
    }

    #[test]
    fn select_combinations_and_java_strings() {
        let attribute = QueryAttribute {
            name: "deps".into(),
            public_name: "deps".into(),
            shape: Shape::LabelList,
            ty: query_attrs::AttrType::LabelList,
            explicit: true,
            tool: false,
            value: None,
            select: Some(vec![
                ConfigurablePart::Plain(AttrValue::Labels(vec!["//p:a".into()])),
                ConfigurablePart::Select {
                    branches: vec![
                        (
                            "//c:x".into(),
                            Some(AttrValue::Labels(vec!["//p:b".into()])),
                        ),
                        ("//conditions:default".into(), None),
                    ],
                    no_match_error: String::new(),
                },
            ]),
            default: Some(AttrValue::Labels(Vec::new())),
            aspects: Vec::new(),
        };
        let values = possible_values(&attribute)
            .iter()
            .map(|value| java_string(value, Shape::LabelList, &label("t")).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, ["[//p:a, //p:b]", "[//p:a]"]);
        assert_eq!(
            attribute.labels(),
            ["//p:a", "//c:x", "//p:b"].map(str::to_owned)
        );
    }

    #[test]
    fn option_parsing_follows_bazel_spellings() {
        let mut options = QueryOptions::default();
        assert_eq!(options.parse("--output=label_kind", None), Ok(Some(false)));
        assert_eq!(options.parse("--output", Some("graph")), Ok(Some(true)));
        assert_eq!(options.output, "graph");
        assert_eq!(options.parse("--noimplicit_deps", None), Ok(Some(false)));
        assert!(!options.implicit_deps);
        assert_eq!(options.parse("--nohost_deps", None), Ok(Some(false)));
        assert!(!options.tool_deps);
        assert_eq!(options.parse("-k", None), Ok(Some(false)));
        assert!(options.keep_going);
        assert_eq!(options.parse("--order_output=FULL", None), Ok(Some(false)));
        assert_eq!(options.order_output, OrderOutput::Full);
        assert!(options.parse("--order_output=sorted", None).is_err());
        assert_eq!(
            options.parse("--universe_scope=//a/...,//b", None),
            Ok(Some(false))
        );
        assert_eq!(options.universe_scope, ["//a/...", "//b"]);
        assert_eq!(
            options.parse("--proto:output_rule_attrs=", None),
            Ok(Some(false))
        );
        assert!(options.format.proto_output_rule_attrs.is_empty());
        assert_eq!(options.parse("--noxml:line_numbers", None), Ok(Some(false)));
        assert!(!options.format.xml_line_numbers);
        assert_eq!(options.parse("//foo", None), Ok(None));
    }

    #[test]
    fn display_form_takes_the_first_mapping_entry() {
        let mapping = BTreeMap::from([
            ("bazel_tools".to_owned(), "bazel_tools".to_owned()),
            ("zz_tools".to_owned(), "bazel_tools".to_owned()),
            ("other".to_owned(), "other+".to_owned()),
        ]);
        let order = ["zz_tools", "", "bazel_tools"].map(str::to_owned);
        assert_eq!(
            first_apparent(&order, &mapping, "bazel_tools").as_deref(),
            Some("zz_tools")
        );
        // Unlisted names fall back to the name-ordered mapping.
        assert_eq!(
            first_apparent(&order, &mapping, "other+").as_deref(),
            Some("other")
        );
        assert_eq!(first_apparent(&order, &mapping, "none+"), None);
    }

    #[test]
    fn repr_escapes_like_starlark() {
        assert_eq!(starlark_repr_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
