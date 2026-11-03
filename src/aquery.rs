//! `aquery` (docs/AQUERY.md): the action graph of the configured targets an
//! expression selects, formatted like Bazel 9.3.1's
//! query2/aquery/ActionGraph{Text,Summary,Proto}OutputFormatterCallback and
//! skyframe/actiongraph/v2 (analysis_v2.proto ActionGraphContainer).
//!
//! The engine (crate::engine::aquery) analyzes the universe with action
//! planning but no execution and describes each planned action as an
//! [`ActionRecord`]; this module evaluates the aquery filters and writes the
//! records. Action keys are Bazel's (crate::aquery_key) where the native
//! action is modelled exactly.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::aquery_key::{ArgsRecipe, CommandLinePart};
use crate::query_language::{Argument, Expr, Function};

static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Whether analysis records aquery metadata ([`Meta`]) for its actions.
pub fn capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// Turn aquery metadata capture on (the aquery command) or off.
pub fn set_capturing(on: bool) {
    CAPTURING.store(on, Ordering::Relaxed);
}

/// What an action's registration knows that its native [`crate::action::Action`]
/// does not (crate::action::ActionExtra::aquery): recorded only while
/// [`capturing`], never keyed or persisted.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    /// The raw progress message (`%{label}`, `%{output}`, `%{input}`
    /// placeholders); `None`: the class default.
    pub progress: Option<String>,
    /// The fixed environment in Bazel's insertion order.
    pub environment: Option<Vec<(String, String)>>,
    /// The execution info in Bazel's insertion order.
    pub execution_info: Option<Vec<(String, String)>>,
    /// The spawn's CommandLines as SpawnAction.computeKey sees them.
    pub command_lines: Option<Vec<CommandLinePart>>,
    /// CommandAction.getArguments (CommandLines.allArguments: parameter
    /// file contents inline).
    pub arguments: Option<Vec<String>>,
    /// A ParameterFileWriteAction (`ctx.actions.write` of an Args).
    pub param_write: Option<ParamWrite>,
    /// SymlinkAction.toExecutable (`is_executable = True`).
    pub executable_symlink: bool,
}

/// A ParameterFileWriteAction's file type, fingerprint and arguments.
#[derive(Clone, Debug)]
pub struct ParamWrite {
    pub file_type: &'static str,
    pub recipe: ArgsRecipe,
    pub arguments: Vec<String>,
}

/// AqueryOptions plus the common post-analysis query options.
#[derive(Clone, Debug)]
pub struct Options {
    pub output: String,
    pub include_commandline: bool,
    pub include_artifacts: bool,
    pub include_pruned_inputs: bool,
    pub include_param_files: bool,
    pub include_file_write_contents: bool,
    pub skyframe_state: bool,
    /// The shared configured-query options (universe_scope, implicit_deps,
    /// tool_deps, include_aspects, query_file, output_file).
    pub query: crate::engine::cquery::Options,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            output: "text".into(),
            include_commandline: true,
            include_artifacts: true,
            include_pruned_inputs: true,
            include_param_files: false,
            include_file_write_contents: false,
            skyframe_state: false,
            query: Default::default(),
        }
    }
}

/// The formatter names, in Bazel's order (PostAnalysisQueryProcessor's
/// "Valid values are" list).
const OUTPUTS: [&str; 7] = [
    "proto",
    "streamed_proto",
    "textproto",
    "jsonproto",
    "text",
    "commands",
    "summary",
];

impl Options {
    /// One aquery option: `Some(used_next)` when recognized.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        if let Some(value) = arg.strip_prefix("--output=") {
            self.output = value.to_owned();
            return Ok(Some(false));
        }
        if arg == "--output" {
            self.output = next.ok_or("Expected value after --output")?.to_owned();
            return Ok(Some(true));
        }
        for (name, destination) in [
            ("include_commandline", &mut self.include_commandline),
            ("include_artifacts", &mut self.include_artifacts),
            ("include_pruned_inputs", &mut self.include_pruned_inputs),
            ("include_param_files", &mut self.include_param_files),
            (
                "include_file_write_contents",
                &mut self.include_file_write_contents,
            ),
            ("skyframe_state", &mut self.skyframe_state),
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                *destination = value?;
                return Ok(Some(false));
            }
        }
        if arg.starts_with("--transitions")
            || arg.starts_with("--proto:")
            || arg.starts_with("--starlark:")
        {
            return Ok(None);
        }
        self.query.parse(arg, next)
    }

    /// Validate the output format and parse the expression with the aquery
    /// functions.
    pub fn expression(&self, text: &str) -> Result<Expr, String> {
        if self.skyframe_state {
            return Err(
                "aquery --skyframe_state is not implemented in the native profile: there is no persistent analysis cache to dump (run aquery with a query expression)"
                    .into(),
            );
        }
        if !OUTPUTS.contains(&self.output.as_str()) {
            return Err(format!(
                "Invalid output format '{}'. Valid values are: {}",
                self.output,
                OUTPUTS.join(", ")
            ));
        }
        crate::query_language::parse_aquery(text).map_err(|error| {
            format!(
                "Error while parsing '{}': {error}",
                crate::query_language::truncate(text)
            )
        })
    }

    /// The analyzed universe: --universe_scope, else the expression's
    /// target patterns (QueryCommandUtils.getTopLevelTargets).
    pub fn universe(&self, expression: &Expr) -> Vec<String> {
        self.query.universe(expression)
    }
}

/// AqueryActionFilter: per function, the patterns that must all match.
#[derive(Default)]
pub struct Filters {
    inputs: Vec<fancy_regex::Regex>,
    outputs: Vec<fancy_regex::Regex>,
    mnemonic: Vec<fancy_regex::Regex>,
}

impl Filters {
    /// AqueryProcessor.buildActionFilters: the filter functions along the
    /// chain of function expressions from the top (each function's first
    /// expression argument); a filter below another function is an error.
    pub fn of(expression: &Expr) -> Result<Self, String> {
        let mut filters = Self::default();
        let mut current = match expression {
            Expr::Function(..) => Some(expression),
            _ => None,
        };
        let mut other: Option<Function> = None;
        while let Some(Expr::Function(function, args)) = current {
            if matches!(
                function,
                Function::Inputs | Function::Outputs | Function::Mnemonic
            ) {
                if let Some(other) = other {
                    return Err(format!(
                        "aquery filter functions (inputs, outputs, mnemonic) produce actions, and therefore can't be the input of other function types: {}\n{expression}",
                        other.name()
                    ));
                }
                let Some(Argument::Word(pattern)) = args.first() else {
                    unreachable!("the parser requires the pattern word")
                };
                let regex = fancy_regex::Regex::new(&format!("^(?:{pattern})$"))
                    .map_err(|error| format!("Wrong query syntax: {error}\n{expression}"))?;
                match function {
                    Function::Inputs => filters.inputs.push(regex),
                    Function::Outputs => filters.outputs.push(regex),
                    _ => filters.mnemonic.push(regex),
                }
            } else {
                other = Some(*function);
            }
            current = args.iter().find_map(|arg| match arg {
                Argument::Expression(expr @ Expr::Function(..)) => Some(expr),
                _ => None,
            });
        }
        Ok(filters)
    }

    fn all(patterns: &[fancy_regex::Regex], text: &str) -> bool {
        patterns
            .iter()
            .all(|pattern| pattern.is_match(text).unwrap_or(false))
    }

    /// AqueryUtils.matchesAqueryFilters.
    pub fn matches(&self, action: &ActionRecord) -> bool {
        if !self.mnemonic.is_empty() && !Self::all(&self.mnemonic, &action.mnemonic) {
            return false;
        }
        if !self.inputs.is_empty()
            && !action
                .inputs
                .iter()
                .any(|input| Self::all(&self.inputs, input))
        {
            return false;
        }
        if !self.outputs.is_empty() {
            return action
                .outputs
                .iter()
                .any(|(output, _)| Self::all(&self.outputs, output));
        }
        true
    }
}

/// ActionFilterFunction.eval: a filter function passes its expression
/// along (after checking its arity and pattern); everything else is the
/// configured query's.
pub fn strip_filters(expression: &Expr) -> Result<Expr, String> {
    Ok(match expression {
        Expr::Function(function, args)
            if matches!(
                function,
                Function::Inputs | Function::Outputs | Function::Mnemonic
            ) =>
        {
            let Some(Argument::Word(pattern)) = args.first() else {
                unreachable!("the parser requires the pattern word")
            };
            if let Err(error) = fancy_regex::Regex::new(pattern) {
                return Err(format!(
                    "Illegal '{}' pattern regexp '{pattern}': {error}",
                    function.name()
                ));
            }
            match args.get(1) {
                Some(Argument::Expression(inner)) if args.len() == 2 => strip_filters(inner)?,
                _ => {
                    return Err(
                        "aquery filter functions (inputs, outputs, mnemonics) must have exactly 2 arguments,except when --skyframe_state is used."
                            .into(),
                    );
                }
            }
        }
        Expr::Function(function, args) => Expr::Function(
            *function,
            args.iter()
                .map(|arg| {
                    Ok(match arg {
                        Argument::Expression(expr) => Argument::Expression(strip_filters(expr)?),
                        other => other.clone(),
                    })
                })
                .collect::<Result<_, String>>()?,
        ),
        Expr::Binary(operator, operands) => Expr::Binary(
            *operator,
            operands
                .iter()
                .map(strip_filters)
                .collect::<Result<_, _>>()?,
        ),
        Expr::Let(name, value, body) => Expr::Let(
            name.clone(),
            Box::new(strip_filters(value)?),
            Box::new(strip_filters(body)?),
        ),
        other => other.clone(),
    })
}

/// One configuration of the dump (KnownConfigurations: the BEP
/// Configuration's mnemonic, platform name, checksum and is_tool).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub mnemonic: String,
    pub platform_name: String,
    pub checksum: String,
    pub is_tool: bool,
}

/// An AspectDescriptor: the aspect class name and its parameters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Aspect {
    pub name: String,
    pub parameters: Vec<(String, String)>,
}

/// One action as aquery describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionRecord {
    /// The owner's label (Label.toString) and rule class.
    pub target: String,
    pub rule_class: Option<String>,
    /// Index into [`Dump::configurations`].
    pub configuration: usize,
    /// The owner's aspect path, outermost first (getAspectDescriptors().reverse()).
    pub aspects: Vec<Aspect>,
    pub mnemonic: String,
    /// AbstractAction.describe: the progress message.
    pub description: String,
    pub action_key: String,
    pub discovers_inputs: bool,
    /// CommandAction.getArguments (`None`: not a CommandAction).
    pub arguments: Option<Vec<String>>,
    /// The fixed environment (empty for non-spawns).
    pub environment: Vec<(String, String)>,
    /// Input exec paths (deduplicated, in order).
    pub inputs: Vec<String>,
    /// Output exec paths and whether each is a tree artifact.
    pub outputs: Vec<(String, bool)>,
    pub execution_info: Vec<(String, String)>,
    pub execution_platform: Option<String>,
    /// FileContentsProvider: executable bit and contents.
    pub file_write: Option<(bool, String)>,
    /// TemplateExpansionAction: the template text and substitutions.
    pub template: Option<(String, Vec<(String, String)>)>,
    pub unresolved_symlink: Option<String>,
    /// A ParameterFileWriteAction's arguments.
    pub param_file: Option<Vec<String>>,
}

/// The selected actions with their configurations.
#[derive(Clone, Debug, Default)]
pub struct Dump {
    pub configurations: Vec<Configuration>,
    pub actions: Vec<ActionRecord>,
}

/// Bazel's progress message (AbstractAction.getProgressMessageChecked):
/// placeholders replaced, `[for tool]` appended in a tool configuration.
pub fn progress_message(
    raw: &str,
    label: &str,
    primary_output: Option<&str>,
    primary_input: Option<&str>,
    tool: bool,
) -> String {
    let mut message = raw.to_owned();
    if message.contains("%{label}") {
        message = message.replace("%{label}", label);
    }
    if let Some(output) = primary_output
        && message.contains("%{output}")
    {
        message = message.replace("%{output}", root_relative(output));
    }
    if let Some(input) = primary_input
        && message.contains("%{input}")
    {
        message = message.replace("%{input}", root_relative(input));
    }
    if tool {
        message.push_str(" [for tool]");
    }
    message
}

/// Artifact.getRootRelativePath of an exec path.
pub fn root_relative(exec_path: &str) -> &str {
    crate::label::split_generated_root(exec_path).map_or(exec_path, |(_, relative)| relative)
}

/// `internalToEscapedUnicode` (ActionGraphTextOutputFormatterCallback):
/// characters outside printable ASCII as `\uXXXX` / `\UXXXXXXXX`.
fn escaped_unicode(text: &str) -> String {
    let unicode = crate::bazel_bytes::platform(text);
    if unicode.chars().all(|c| (' '..'\u{7f}').contains(&c)) {
        return unicode.into_owned();
    }
    let mut out = String::new();
    for c in unicode.chars() {
        let value = u32::from(c);
        if (0x20..0x7f).contains(&value) {
            out.push(c);
        } else if value <= 0xffff {
            out.push_str(&format!("\\u{value:04X}"));
        } else {
            out.push_str(&format!("\\U{value:08X}"));
        }
    }
    out
}

/// The Java string of an internal string (its bytes as Latin-1
/// characters): what Bazel stores in protos and prints unescaped.
fn java_string(text: &str) -> String {
    let bytes = crate::bazel_bytes::internal_bytes(text);
    if bytes.is_ascii() {
        return text.to_owned();
    }
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

fn shell_escape(text: &str) -> String {
    crate::run_command::shell_escape(text)
}

/// Write the dump in the requested format.
pub fn write(
    dump: &Dump,
    filters: &Filters,
    options: &Options,
    destination: &mut dyn Write,
) -> std::io::Result<()> {
    let mut options = options.clone();
    // "Enabling includeParamFiles should enable includeCommandline by default."
    options.include_commandline |= options.include_param_files;
    match options.output.as_str() {
        "text" => write_text(dump, filters, &options, destination, false),
        "commands" => write_text(dump, filters, &options, destination, true),
        "summary" => write_summary(dump, filters, destination),
        format => {
            let messages = proto_messages(dump, filters, &options);
            match format {
                "proto" => {
                    for message in &messages {
                        let mut buffer = Vec::new();
                        message.encode(&mut buffer);
                        let mut field = Vec::new();
                        length_delimited(&mut field, message.field(), &buffer);
                        destination.write_all(&field)?;
                    }
                    Ok(())
                }
                "streamed_proto" => {
                    for message in &messages {
                        let mut buffer = Vec::new();
                        message.encode(&mut buffer);
                        let mut container = Vec::new();
                        length_delimited(&mut container, message.field(), &buffer);
                        let mut framed = Vec::new();
                        put_varint(&mut framed, container.len() as u64);
                        framed.extend_from_slice(&container);
                        destination.write_all(&framed)?;
                    }
                    Ok(())
                }
                "textproto" => {
                    for message in &messages {
                        let mut text = String::new();
                        message.text(&mut text, "");
                        write!(destination, "{} {{\n{text}}}\n", message.label())?;
                    }
                    Ok(())
                }
                _ => write_json(&messages, destination),
            }
        }
    }
}

/// ActionGraphTextOutputFormatterCallback.writeText / writeCommand.
fn write_text(
    dump: &Dump,
    filters: &Filters,
    options: &Options,
    destination: &mut dyn Write,
    commands: bool,
) -> std::io::Result<()> {
    let mut param_files: HashMap<String, String> = HashMap::new();
    for action in &dump.actions {
        if options.include_param_files
            && let Some(arguments) = &action.param_file
            && let Some((output, _)) = action.outputs.first()
        {
            param_files.insert(output.clone(), arguments.join(" \\\n    "));
        }
        if !filters.matches(action) {
            continue;
        }
        let mut out = String::new();
        if commands {
            let Some(arguments) = &action.arguments else {
                continue;
            };
            let line: Vec<String> = arguments
                .iter()
                .map(|argument| shell_escape(&escaped_unicode(argument)))
                .collect();
            out.push_str(&line.join(" "));
            out.push('\n');
            destination.write_all(out.as_bytes())?;
            continue;
        }
        let configuration = &dump.configurations[action.configuration];
        out.push_str(&format!("action '{}'\n", java_string(&action.description)));
        out.push_str(&format!("  Mnemonic: {}\n", action.mnemonic));
        out.push_str(&format!("  Target: {}\n", action.target));
        out.push_str(&format!("  Configuration: {}\n", configuration.mnemonic));
        if let Some(platform) = &action.execution_platform {
            out.push_str(&format!("  Execution platform: {platform}\n"));
        }
        if !action.aspects.is_empty() {
            let aspects: Vec<String> = action
                .aspects
                .iter()
                .map(|aspect| {
                    let parameters: Vec<String> = aspect
                        .parameters
                        .iter()
                        .map(|(key, value)| format!("{key}='{value}'"))
                        .collect();
                    format!("{}({})", aspect.name, parameters.join(", "))
                })
                .collect();
            out.push_str(&format!(
                "  AspectDescriptors: [{}]\n",
                aspects.join("\n    -> ")
            ));
        }
        out.push_str(&format!("  ActionKey: {}\n", action.action_key));
        if options.include_artifacts {
            let mut inputs: Vec<String> =
                action.inputs.iter().map(|i| escaped_unicode(i)).collect();
            inputs.sort();
            out.push_str(&format!("  Inputs: [{}]\n", inputs.join(", ")));
            let mut outputs: Vec<String> = action
                .outputs
                .iter()
                .map(|(output, tree)| {
                    escaped_unicode(&if *tree {
                        format!("{output} (TreeArtifact)")
                    } else {
                        output.clone()
                    })
                })
                .collect();
            outputs.sort();
            out.push_str(&format!("  Outputs: [{}]\n", outputs.join(", ")));
        }
        if !action.environment.is_empty() {
            let mut environment: Vec<String> = action
                .environment
                .iter()
                .map(|(key, value)| escaped_unicode(&format!("{key}={value}")))
                .collect();
            environment.sort();
            out.push_str(&format!("  Environment: [{}]\n", environment.join(", ")));
        }
        if options.include_commandline
            && let Some(arguments) = &action.arguments
        {
            out.push_str("  Command Line: (exec ");
            let escaped: Vec<String> = arguments
                .iter()
                .map(|argument| shell_escape(&escaped_unicode(argument)))
                .collect();
            out.push_str(&escaped.join(" \\\n    "));
            out.push(')');
            out.push_str(&format!("\n# Configuration: {}", configuration.checksum));
            if let Some(platform) = &action.execution_platform {
                out.push_str(&format!("\n# Execution platform: {platform}"));
            }
            out.push('\n');
        }
        if options.include_param_files {
            for input in &action.inputs {
                if let Some(content) = param_files.get(input) {
                    out.push_str(&format!(
                        "  Params File Content ({input}):\n    {content}\n"
                    ));
                }
            }
        }
        if !action.execution_info.is_empty() {
            let mut info: Vec<&(String, String)> = action.execution_info.iter().collect();
            info.sort();
            let info: Vec<String> = info
                .iter()
                .map(|(key, value)| format!("{}: {}", shell_escape(key), shell_escape(value)))
                .collect();
            out.push_str(&format!("  ExecutionInfo: {{{}}}\n", info.join(", ")));
        }
        if let Some((content, substitutions)) = &action.template {
            out.push_str(&format!("  Template: {}\n", java_string(content)));
            out.push_str("  Substitutions: [\n");
            for (key, value) in substitutions {
                out.push_str(&format!(
                    "    {{{}: {}}}\n",
                    java_string(key),
                    java_string(value)
                ));
            }
            out.push_str("  ]\n");
        }
        if let Some((executable, contents)) = &action.file_write {
            out.push_str(&format!("  IsExecutable: {executable}\n"));
            if options.include_file_write_contents {
                use base64::Engine as _;
                out.push_str(&format!(
                    "  FileWriteContents: [{}]\n",
                    base64::engine::general_purpose::STANDARD
                        .encode(crate::bazel_bytes::platform(contents).as_bytes())
                ));
            }
        }
        if let Some(target) = &action.unresolved_symlink {
            out.push_str(&format!("  UnresolvedSymlinkTarget: {target}\n"));
        }
        out.push('\n');
        destination.write_all(out.as_bytes())?;
    }
    Ok(())
}

/// Java's `String.hashCode` over an internal string.
fn java_hash(text: &str) -> i32 {
    let mut hash: i32 = 0;
    for &byte in crate::bazel_bytes::internal_bytes(text).iter() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(byte));
    }
    hash
}

/// The iteration order of a `java.util.HashMap<String, _>` filled with
/// `keys` in insertion order by `merge` (no treeified buckets): by bucket
/// of the final table, then the reverse of insertion order (HashMap.merge
/// links a new node at the head of its bin; a resize keeps each bin's
/// relative order).
fn java_hash_map_order(keys: &[String]) -> Vec<usize> {
    let mut capacity: usize = 16;
    while keys.len() * 4 > capacity * 3 {
        capacity *= 2;
    }
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&index| {
        let hash = java_hash(&keys[index]) as u32;
        (
            ((hash ^ (hash >> 16)) as usize) & (capacity - 1),
            std::cmp::Reverse(index),
        )
    });
    order
}

/// ActionGraphSummaryOutputFormatterCallback.
fn write_summary(
    dump: &Dump,
    filters: &Filters,
    destination: &mut dyn Write,
) -> std::io::Result<()> {
    #[derive(Default)]
    struct Counts {
        keys: Vec<String>,
        counts: HashMap<String, usize>,
    }
    impl Counts {
        fn add(&mut self, key: &str) {
            let count = self.counts.entry(key.to_owned()).or_insert(0);
            if *count == 0 {
                self.keys.push(key.to_owned());
            }
            *count += 1;
        }
    }
    let mut mnemonics = Counts::default();
    let mut configurations = Counts::default();
    let mut platforms = Counts::default();
    let mut aspects = Counts::default();
    let mut total = 0;
    for action in &dump.actions {
        if !filters.matches(action) {
            continue;
        }
        total += 1;
        mnemonics.add(&action.mnemonic);
        configurations.add(&dump.configurations[action.configuration].mnemonic);
        if let Some(platform) = &action.execution_platform {
            platforms.add(platform);
        }
        for aspect in &action.aspects {
            aspects.add(&aspect.name);
        }
    }
    let mut out = String::new();
    if total == 0 {
        out.push_str("No actions matched.\n");
    } else {
        out.push_str(&format!(
            "{total} total action{}.\n",
            if total == 1 { "" } else { "s" }
        ));
    }
    for (counts, title) in [
        (&mnemonics, "Mnemonics:"),
        (&configurations, "Configurations:"),
        (&platforms, "Execution Platforms:"),
        (&aspects, "Aspects:"),
    ] {
        if counts.keys.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        let mut order = java_hash_map_order(&counts.keys);
        order.sort_by_key(|&index| counts.counts[&counts.keys[index]]);
        for index in order {
            let key = &counts.keys[index];
            out.push_str(&format!("  {key}: {}\n", counts.counts[key]));
        }
    }
    destination.write_all(out.as_bytes())
}

// ---------------------------------------------------------------------------
// analysis_v2.proto ActionGraphContainer messages.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Message {
    Artifact {
        id: u32,
        path_fragment_id: u32,
        tree: bool,
    },
    Action(Box<ActionMessage>),
    Target {
        id: u32,
        label: String,
        rule_class_id: u32,
    },
    DepSet {
        id: u32,
        direct: Vec<u32>,
    },
    Configuration {
        id: u32,
        configuration: Configuration,
    },
    Aspect {
        id: u32,
        aspect: Aspect,
    },
    RuleClass {
        id: u32,
        name: String,
    },
    PathFragment {
        id: u32,
        label: String,
        parent_id: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ActionMessage {
    target_id: u32,
    aspect_descriptor_ids: Vec<u32>,
    action_key: String,
    mnemonic: String,
    configuration_id: u32,
    arguments: Vec<String>,
    environment_variables: Vec<(String, String)>,
    input_dep_set_ids: Vec<u32>,
    output_ids: Vec<u32>,
    discovers_inputs: bool,
    execution_info: Vec<(String, String)>,
    param_files: Vec<(String, Vec<String>)>,
    primary_output_id: u32,
    execution_platform: String,
    template_content: String,
    substitutions: Vec<(String, String)>,
    file_contents: String,
    unresolved_symlink_target: String,
    is_executable: bool,
}

/// BaseCache.dataToIdAndStreamOutputProto: ids from 1 in first-use order.
struct Known<K> {
    ids: HashMap<K, u32>,
}

impl<K: std::hash::Hash + Eq> Known<K> {
    fn new() -> Self {
        Self {
            ids: HashMap::new(),
        }
    }

    /// The id of `key`, and whether it is new.
    fn id(&mut self, key: K) -> (u32, bool) {
        let next = self.ids.len() as u32 + 1;
        match self.ids.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => (*entry.get(), false),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(next);
                (next, true)
            }
        }
    }
}

struct ProtoDump<'a> {
    messages: Vec<Message>,
    targets: Known<(String, Option<String>)>,
    rule_classes: Known<String>,
    configurations: Known<usize>,
    aspects: Known<Aspect>,
    artifacts: Known<String>,
    path_fragments: Known<String>,
    dep_sets: u32,
    dump: &'a Dump,
}

impl ProtoDump<'_> {
    fn path_fragment(&mut self, path: &str) -> u32 {
        let (id, new) = self.path_fragments.id(path.to_owned());
        if new {
            let (parent, label) = match path.rsplit_once('/') {
                Some((parent, label)) => (Some(parent), label),
                None => (None, path),
            };
            let parent_id = match parent {
                Some(parent) if !parent.is_empty() => self.path_fragment(parent),
                _ => 0,
            };
            self.messages.push(Message::PathFragment {
                id,
                label: java_string(label),
                parent_id,
            });
        }
        id
    }

    fn artifact(&mut self, path: &str, tree: bool) -> u32 {
        let (id, new) = self.artifacts.id(path.to_owned());
        if new {
            let path_fragment_id = self.path_fragment(path);
            self.messages.push(Message::Artifact {
                id,
                path_fragment_id,
                tree,
            });
        }
        id
    }

    fn action(
        &mut self,
        action: &ActionRecord,
        options: &Options,
        params: &HashMap<String, Vec<String>>,
    ) {
        let mut message = ActionMessage {
            mnemonic: action.mnemonic.clone(),
            ..Default::default()
        };
        let (target_id, new) = self
            .targets
            .id((action.target.clone(), action.rule_class.clone()));
        if new {
            let rule_class_id = match &action.rule_class {
                Some(name) => {
                    let (id, new) = self.rule_classes.id(name.clone());
                    if new {
                        self.messages.push(Message::RuleClass {
                            id,
                            name: name.clone(),
                        });
                    }
                    id
                }
                None => 0,
            };
            self.messages.push(Message::Target {
                id: target_id,
                label: action.target.clone(),
                rule_class_id,
            });
        }
        message.target_id = target_id;
        message.action_key = action.action_key.clone();
        message.discovers_inputs = action.discovers_inputs;
        if action.arguments.is_some() {
            message.environment_variables = action
                .environment
                .iter()
                .map(|(k, v)| (java_string(k), java_string(v)))
                .collect();
        }
        if options.include_commandline
            && let Some(arguments) = &action.arguments
        {
            message.arguments = arguments.iter().map(|a| java_string(a)).collect();
        }
        if let Some((executable, contents)) = &action.file_write {
            message.is_executable = *executable;
            if options.include_file_write_contents {
                message.file_contents = java_string(contents);
            }
        }
        if let Some(target) = &action.unresolved_symlink {
            message.unresolved_symlink_target = java_string(target);
        }
        if options.include_param_files {
            for input in &action.inputs {
                if let Some(arguments) = params.get(input) {
                    message.param_files.push((
                        input.clone(),
                        arguments.iter().map(|a| java_string(a)).collect(),
                    ));
                }
            }
        }
        message.execution_info = action
            .execution_info
            .iter()
            .map(|(k, v)| (java_string(k), java_string(v)))
            .collect();
        let (configuration_id, new) = self.configurations.id(action.configuration);
        if new {
            self.messages.push(Message::Configuration {
                id: configuration_id,
                configuration: self.dump.configurations[action.configuration].clone(),
            });
        }
        message.configuration_id = configuration_id;
        if let Some(platform) = &action.execution_platform {
            message.execution_platform = platform.clone();
        }
        for aspect in &action.aspects {
            let (id, new) = self.aspects.id(aspect.clone());
            if new {
                self.messages.push(Message::Aspect {
                    id,
                    aspect: aspect.clone(),
                });
            }
            message.aspect_descriptor_ids.push(id);
        }
        if options.include_artifacts {
            if !action.inputs.is_empty() {
                self.dep_sets += 1;
                let id = self.dep_sets;
                let direct = action
                    .inputs
                    .iter()
                    .map(|input| self.artifact(input, false))
                    .collect();
                self.messages.push(Message::DepSet { id, direct });
                message.input_dep_set_ids.push(id);
            }
            for (output, tree) in &action.outputs {
                let id = self.artifact(output, *tree);
                message.output_ids.push(id);
            }
            if let Some((output, tree)) = action.outputs.first() {
                message.primary_output_id = self.artifact(output, *tree);
            }
        }
        if let Some((content, substitutions)) = &action.template {
            message.template_content = java_string(content);
            message.substitutions = substitutions
                .iter()
                .map(|(k, v)| (java_string(k), java_string(v)))
                .collect();
        }
        self.messages.push(Message::Action(Box::new(message)));
    }
}

/// The messages ActionGraphDump emits, in emission order.
fn proto_messages(dump: &Dump, filters: &Filters, options: &Options) -> Vec<Message> {
    let mut proto = ProtoDump {
        messages: Vec::new(),
        targets: Known::new(),
        rule_classes: Known::new(),
        configurations: Known::new(),
        aspects: Known::new(),
        artifacts: Known::new(),
        path_fragments: Known::new(),
        dep_sets: 0,
        dump,
    };
    let mut params: HashMap<String, Vec<String>> = HashMap::new();
    for action in &dump.actions {
        if options.include_param_files
            && let Some(arguments) = &action.param_file
            && let Some((output, _)) = action.outputs.first()
        {
            params.insert(output.clone(), arguments.clone());
        }
        if filters.matches(action) {
            proto.action(action, options, &params);
        }
    }
    proto.messages
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn length_delimited(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_varint(out, u64::from(field << 3 | 2));
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_uint(out: &mut Vec<u8>, field: u32, value: u32) {
    if value != 0 {
        put_varint(out, u64::from(field << 3));
        put_varint(out, u64::from(value));
    }
}

fn put_bool(out: &mut Vec<u8>, field: u32, value: bool) {
    if value {
        put_varint(out, u64::from(field << 3));
        out.push(1);
    }
}

fn put_string(out: &mut Vec<u8>, field: u32, value: &str) {
    if !value.is_empty() {
        length_delimited(out, field, value.as_bytes());
    }
}

fn put_packed(out: &mut Vec<u8>, field: u32, values: &[u32]) {
    if values.is_empty() {
        return;
    }
    let mut packed = Vec::new();
    for value in values {
        put_varint(&mut packed, u64::from(*value));
    }
    length_delimited(out, field, &packed);
}

fn pair_bytes(key: &str, value: &str) -> Vec<u8> {
    let mut pair = Vec::new();
    put_string(&mut pair, 1, key);
    put_string(&mut pair, 2, value);
    pair
}

/// protobuf TextFormat string escaping (TextFormat.escapeBytes over UTF-8).
fn text_escape(text: &str) -> String {
    let mut out = String::new();
    for &byte in text.as_bytes() {
        match byte {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out
}

/// protobuf-java JsonFormat string escaping (Gson's HTML-safe escaping).
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)))
            }
            c if u32::from(c) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON / text field value.
enum Value<'a> {
    Uint(u32),
    Bool(bool),
    Str(&'a str),
    Uints(&'a [u32]),
    Strs(&'a [String]),
    Pairs(&'a [(String, String)]),
    ParamFiles(&'a [(String, Vec<String>)]),
}

impl Message {
    fn field(&self) -> u32 {
        match self {
            Self::Artifact { .. } => 1,
            Self::Action(_) => 2,
            Self::Target { .. } => 3,
            Self::DepSet { .. } => 4,
            Self::Configuration { .. } => 5,
            Self::Aspect { .. } => 6,
            Self::RuleClass { .. } => 7,
            Self::PathFragment { .. } => 8,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Artifact { .. } => "artifacts",
            Self::Action(_) => "actions",
            Self::Target { .. } => "targets",
            Self::DepSet { .. } => "dep_set_of_files",
            Self::Configuration { .. } => "configuration",
            Self::Aspect { .. } => "aspect_descriptors",
            Self::RuleClass { .. } => "rule_classes",
            Self::PathFragment { .. } => "path_fragments",
        }
    }

    fn json_label(&self) -> &'static str {
        match self {
            Self::Artifact { .. } => "artifacts",
            Self::Action(_) => "actions",
            Self::Target { .. } => "targets",
            Self::DepSet { .. } => "depSetOfFiles",
            Self::Configuration { .. } => "configuration",
            Self::Aspect { .. } => "aspectDescriptors",
            Self::RuleClass { .. } => "ruleClasses",
            Self::PathFragment { .. } => "pathFragments",
        }
    }

    /// The non-default fields in field-number order: (snake name, JSON name, value).
    fn fields(&self) -> Vec<(&'static str, &'static str, Value<'_>)> {
        let mut fields = Vec::new();
        let mut uint = |snake, json, value: u32| {
            if value != 0 {
                fields.push((snake, json, Value::Uint(value)));
            }
        };
        match self {
            Self::Artifact {
                id,
                path_fragment_id,
                tree,
            } => {
                uint("id", "id", *id);
                uint("path_fragment_id", "pathFragmentId", *path_fragment_id);
                if *tree {
                    fields.push(("is_tree_artifact", "isTreeArtifact", Value::Bool(true)));
                }
            }
            Self::Target {
                id,
                label,
                rule_class_id,
            } => {
                uint("id", "id", *id);
                fields.push(("label", "label", Value::Str(label)));
                if *rule_class_id != 0 {
                    fields.push(("rule_class_id", "ruleClassId", Value::Uint(*rule_class_id)));
                }
            }
            Self::DepSet { id, direct } => {
                uint("id", "id", *id);
                if !direct.is_empty() {
                    fields.push((
                        "direct_artifact_ids",
                        "directArtifactIds",
                        Value::Uints(direct),
                    ));
                }
            }
            Self::Configuration { id, configuration } => {
                uint("id", "id", *id);
                for (snake, json, value) in [
                    ("mnemonic", "mnemonic", &configuration.mnemonic),
                    (
                        "platform_name",
                        "platformName",
                        &configuration.platform_name,
                    ),
                    ("checksum", "checksum", &configuration.checksum),
                ] {
                    if !value.is_empty() {
                        fields.push((snake, json, Value::Str(value)));
                    }
                }
                if configuration.is_tool {
                    fields.push(("is_tool", "isTool", Value::Bool(true)));
                }
            }
            Self::Aspect { id, aspect } => {
                uint("id", "id", *id);
                fields.push(("name", "name", Value::Str(&aspect.name)));
                if !aspect.parameters.is_empty() {
                    fields.push(("parameters", "parameters", Value::Pairs(&aspect.parameters)));
                }
            }
            Self::RuleClass { id, name } => {
                uint("id", "id", *id);
                fields.push(("name", "name", Value::Str(name)));
            }
            Self::PathFragment {
                id,
                label,
                parent_id,
            } => {
                uint("id", "id", *id);
                fields.push(("label", "label", Value::Str(label)));
                if *parent_id != 0 {
                    fields.push(("parent_id", "parentId", Value::Uint(*parent_id)));
                }
            }
            Self::Action(action) => {
                if action.target_id != 0 {
                    fields.push(("target_id", "targetId", Value::Uint(action.target_id)));
                }
                if !action.aspect_descriptor_ids.is_empty() {
                    fields.push((
                        "aspect_descriptor_ids",
                        "aspectDescriptorIds",
                        Value::Uints(&action.aspect_descriptor_ids),
                    ));
                }
                if !action.action_key.is_empty() {
                    fields.push(("action_key", "actionKey", Value::Str(&action.action_key)));
                }
                if !action.mnemonic.is_empty() {
                    fields.push(("mnemonic", "mnemonic", Value::Str(&action.mnemonic)));
                }
                if action.configuration_id != 0 {
                    fields.push((
                        "configuration_id",
                        "configurationId",
                        Value::Uint(action.configuration_id),
                    ));
                }
                if !action.arguments.is_empty() {
                    fields.push(("arguments", "arguments", Value::Strs(&action.arguments)));
                }
                if !action.environment_variables.is_empty() {
                    fields.push((
                        "environment_variables",
                        "environmentVariables",
                        Value::Pairs(&action.environment_variables),
                    ));
                }
                if !action.input_dep_set_ids.is_empty() {
                    fields.push((
                        "input_dep_set_ids",
                        "inputDepSetIds",
                        Value::Uints(&action.input_dep_set_ids),
                    ));
                }
                if !action.output_ids.is_empty() {
                    fields.push(("output_ids", "outputIds", Value::Uints(&action.output_ids)));
                }
                if action.discovers_inputs {
                    fields.push(("discovers_inputs", "discoversInputs", Value::Bool(true)));
                }
                if !action.execution_info.is_empty() {
                    fields.push((
                        "execution_info",
                        "executionInfo",
                        Value::Pairs(&action.execution_info),
                    ));
                }
                if !action.param_files.is_empty() {
                    fields.push((
                        "param_files",
                        "paramFiles",
                        Value::ParamFiles(&action.param_files),
                    ));
                }
                if action.primary_output_id != 0 {
                    fields.push((
                        "primary_output_id",
                        "primaryOutputId",
                        Value::Uint(action.primary_output_id),
                    ));
                }
                for (snake, json, value) in [
                    (
                        "execution_platform",
                        "executionPlatform",
                        &action.execution_platform,
                    ),
                    (
                        "template_content",
                        "templateContent",
                        &action.template_content,
                    ),
                ] {
                    if !value.is_empty() {
                        fields.push((snake, json, Value::Str(value)));
                    }
                }
                if !action.substitutions.is_empty() {
                    fields.push((
                        "substitutions",
                        "substitutions",
                        Value::Pairs(&action.substitutions),
                    ));
                }
                for (snake, json, value) in [
                    ("file_contents", "fileContents", &action.file_contents),
                    (
                        "unresolved_symlink_target",
                        "unresolvedSymlinkTarget",
                        &action.unresolved_symlink_target,
                    ),
                ] {
                    if !value.is_empty() {
                        fields.push((snake, json, Value::Str(value)));
                    }
                }
                if action.is_executable {
                    fields.push(("is_executable", "isExecutable", Value::Bool(true)));
                }
            }
        }
        fields
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Artifact {
                id,
                path_fragment_id,
                tree,
            } => {
                put_uint(out, 1, *id);
                put_uint(out, 2, *path_fragment_id);
                put_bool(out, 3, *tree);
            }
            Self::Target {
                id,
                label,
                rule_class_id,
            } => {
                put_uint(out, 1, *id);
                put_string(out, 2, label);
                put_uint(out, 3, *rule_class_id);
            }
            Self::DepSet { id, direct } => {
                put_uint(out, 1, *id);
                put_packed(out, 3, direct);
            }
            Self::Configuration { id, configuration } => {
                put_uint(out, 1, *id);
                put_string(out, 2, &configuration.mnemonic);
                put_string(out, 3, &configuration.platform_name);
                put_string(out, 4, &configuration.checksum);
                put_bool(out, 5, configuration.is_tool);
            }
            Self::Aspect { id, aspect } => {
                put_uint(out, 1, *id);
                put_string(out, 2, &aspect.name);
                for (key, value) in &aspect.parameters {
                    length_delimited(out, 3, &pair_bytes(key, value));
                }
            }
            Self::RuleClass { id, name } => {
                put_uint(out, 1, *id);
                put_string(out, 2, name);
            }
            Self::PathFragment {
                id,
                label,
                parent_id,
            } => {
                put_uint(out, 1, *id);
                put_string(out, 2, label);
                put_uint(out, 3, *parent_id);
            }
            Self::Action(action) => {
                put_uint(out, 1, action.target_id);
                put_packed(out, 2, &action.aspect_descriptor_ids);
                put_string(out, 3, &action.action_key);
                put_string(out, 4, &action.mnemonic);
                put_uint(out, 5, action.configuration_id);
                for argument in &action.arguments {
                    length_delimited(out, 6, argument.as_bytes());
                }
                for (key, value) in &action.environment_variables {
                    length_delimited(out, 7, &pair_bytes(key, value));
                }
                put_packed(out, 8, &action.input_dep_set_ids);
                put_packed(out, 9, &action.output_ids);
                put_bool(out, 10, action.discovers_inputs);
                for (key, value) in &action.execution_info {
                    length_delimited(out, 11, &pair_bytes(key, value));
                }
                for (path, arguments) in &action.param_files {
                    let mut param = Vec::new();
                    put_string(&mut param, 1, path);
                    for argument in arguments {
                        length_delimited(&mut param, 2, argument.as_bytes());
                    }
                    length_delimited(out, 12, &param);
                }
                put_uint(out, 13, action.primary_output_id);
                put_string(out, 14, &action.execution_platform);
                put_string(out, 15, &action.template_content);
                for (key, value) in &action.substitutions {
                    length_delimited(out, 16, &pair_bytes(key, value));
                }
                put_string(out, 17, &action.file_contents);
                put_string(out, 18, &action.unresolved_symlink_target);
                put_bool(out, 19, action.is_executable);
            }
        }
    }

    /// TextFormat of the message's fields at `indent`.
    fn text(&self, out: &mut String, indent: &str) {
        for (name, _, value) in self.fields() {
            text_field(out, indent, name, &value);
        }
    }
}

fn text_field(out: &mut String, indent: &str, name: &str, value: &Value<'_>) {
    match value {
        Value::Uint(value) => out.push_str(&format!("{indent}{name}: {value}\n")),
        Value::Bool(value) => out.push_str(&format!("{indent}{name}: {value}\n")),
        Value::Str(value) => out.push_str(&format!("{indent}{name}: \"{}\"\n", text_escape(value))),
        Value::Uints(values) => {
            for value in *values {
                out.push_str(&format!("{indent}{name}: {value}\n"));
            }
        }
        Value::Strs(values) => {
            for value in *values {
                out.push_str(&format!("{indent}{name}: \"{}\"\n", text_escape(value)));
            }
        }
        Value::Pairs(pairs) => {
            for (key, value) in *pairs {
                out.push_str(&format!("{indent}{name} {{\n"));
                let inner = format!("{indent}  ");
                if !key.is_empty() {
                    out.push_str(&format!("{inner}key: \"{}\"\n", text_escape(key)));
                }
                if !value.is_empty() {
                    out.push_str(&format!("{inner}value: \"{}\"\n", text_escape(value)));
                }
                out.push_str(&format!("{indent}}}\n"));
            }
        }
        Value::ParamFiles(files) => {
            for (path, arguments) in *files {
                out.push_str(&format!("{indent}{name} {{\n"));
                let inner = format!("{indent}  ");
                if !path.is_empty() {
                    out.push_str(&format!("{inner}exec_path: \"{}\"\n", text_escape(path)));
                }
                for argument in arguments {
                    out.push_str(&format!(
                        "{inner}arguments: \"{}\"\n",
                        text_escape(argument)
                    ));
                }
                out.push_str(&format!("{indent}}}\n"));
            }
        }
    }
}

/// One JSON object (protobuf-java JsonFormat.printer layout) from its
/// fields at `indent` (the indentation of the object's closing brace).
fn json_object(fields: &[(&str, String)], indent: &str) -> String {
    if fields.is_empty() {
        return format!("{{\n{indent}}}");
    }
    let inner = format!("{indent}  ");
    let body: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{inner}\"{name}\": {value}"))
        .collect();
    format!("{{\n{}\n{indent}}}", body.join(",\n"))
}

fn json_value(value: &Value<'_>, indent: &str) -> String {
    let pair = |key: &str, value: &str, indent: &str| {
        let mut fields = Vec::new();
        if !key.is_empty() {
            fields.push(("key", json_escape(key)));
        }
        if !value.is_empty() {
            fields.push(("value", json_escape(value)));
        }
        json_object(&fields, indent)
    };
    let inner = format!("{indent}  ");
    match value {
        Value::Uint(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Str(value) => json_escape(value),
        Value::Uints(values) => format!(
            "[{}]",
            values
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Strs(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|value| json_escape(value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Pairs(pairs) => format!(
            "[{}]",
            pairs
                .iter()
                .map(|(key, value)| pair(key, value, indent))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::ParamFiles(files) => format!(
            "[{}]",
            files
                .iter()
                .map(|(path, arguments)| {
                    let mut fields = Vec::new();
                    if !path.is_empty() {
                        fields.push(("execPath", json_escape(path)));
                    }
                    if !arguments.is_empty() {
                        fields.push(("arguments", json_value(&Value::Strs(arguments), &inner)));
                    }
                    json_object(&fields, indent)
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// MonolithicOutputHandler: the whole container through JsonFormat, then
/// a newline.
fn write_json(messages: &[Message], destination: &mut dyn Write) -> std::io::Result<()> {
    let mut groups: BTreeMap<u32, (&'static str, Vec<&Message>)> = BTreeMap::new();
    for message in messages {
        groups
            .entry(message.field())
            .or_insert((message.json_label(), Vec::new()))
            .1
            .push(message);
    }
    let mut fields = Vec::new();
    for (name, messages) in groups.values() {
        let items: Vec<String> = messages
            .iter()
            .map(|message| {
                let inner: Vec<(&str, String)> = message
                    .fields()
                    .iter()
                    .map(|(_, json, value)| (*json, json_value(value, "    ")))
                    .collect();
                json_object(&inner, "  ")
            })
            .collect();
        fields.push((*name, format!("[{}]", items.join(", "))));
    }
    let text = json_object(&fields, "");
    writeln!(destination, "{text}")
}

/// The inputs of an action, deduplicated in order.
pub fn dedup(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_follows_java_hash_map_order() {
        // The genuine `aquery //pkg:all --output=summary` of the kitchen
        // fixture lists the count-1 mnemonics in this order.
        let keys: Vec<String> = [
            "Genrule",
            "FileWrite",
            "Shell",
            "Action",
            "RunTool",
            "TemplateExpand",
            "Symlink",
            "UnresolvedSymlink",
            "Params",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let counts = [3, 4, 1, 1, 1, 1, 1, 1, 1];
        let mut order = java_hash_map_order(&keys);
        order.sort_by_key(|&index| counts[index]);
        let names: Vec<&str> = order.iter().map(|&index| keys[index].as_str()).collect();
        assert_eq!(
            names,
            [
                "Action",
                "Shell",
                "Symlink",
                "RunTool",
                "Params",
                "UnresolvedSymlink",
                "TemplateExpand",
                "Genrule",
                "FileWrite"
            ]
        );
    }

    #[test]
    fn filters_follow_the_function_chain() {
        let expression =
            crate::query_language::parse_aquery("mnemonic('Go.*', inputs('.*\\.go', deps(//a)))")
                .unwrap();
        let filters = Filters::of(&expression).unwrap();
        assert_eq!(filters.mnemonic.len(), 1);
        assert_eq!(filters.inputs.len(), 1);
        let stripped = strip_filters(&expression).unwrap();
        assert_eq!(stripped.to_string(), "deps(//a)");
        let nested = crate::query_language::parse_aquery("deps(mnemonic('x', //a))").unwrap();
        assert!(
            Filters::of(&nested)
                .err()
                .unwrap()
                .starts_with("aquery filter functions (inputs, outputs, mnemonic) produce actions")
        );
    }

    #[test]
    fn json_escaping_is_html_safe() {
        assert_eq!(json_escape("a > b 'c'"), "\"a \\u003e b \\u0027c\\u0027\"");
        assert_eq!(text_escape("echo '#'"), "echo \\'#\\'");
    }
}
