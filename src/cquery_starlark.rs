//! `cquery --output=starlark` (StarlarkOutputFormatterCallback): a Starlark
//! `format(target)` applied to each configured result, one line per target.
//!
//! The configured graph keeps no provider values (docs/CQUERY.md), so the
//! formatter's `target` is a native view of what the graph does retain:
//! `target.label` (the actual target of an alias), the configured target's
//! repr/str, `providers(target)` with its genuine key names (values are
//! opaque) and `build_options(target)` of the modelled native options plus
//! the non-default Starlark settings. Reading anything else fails explicitly.
use crate::cquery_proto as wire;
use serde::{Deserialize, Serialize};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::AllocDict;
use starlark::values::{Heap, OwnedFrozenValue, Value};

/// A `build_options(target)` value as `Starlark.fromJava` exposes the
/// native option (or the Starlark setting).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildOption {
    None,
    Bool(bool),
    Int(i64),
    String(String),
    Strings(Vec<String>),
    /// A canonical label.
    Label(String),
    Labels(Vec<String>),
    /// A native enum StarlarkValue (CompilationMode, StripMode, TriState).
    Enum {
        type_name: String,
        value: String,
    },
}

/// `providers(target)`'s key of a native provider identity
/// (AbstractConfiguredTarget.tryAddProviderForQuery): a Starlark provider is
/// `<extension Label.toString()>%<name>`, a built-in one its name.
pub fn provider_key_name(id: &str) -> String {
    if let Some(name) = id.strip_prefix("builtin:") {
        return name.to_owned();
    }
    match id.rsplit_once('%') {
        Some((extension, name)) => {
            let extension = crate::label::Label::parse_canonical(extension)
                .map(|label| crate::rule_declarations::canonical_form(&label))
                .unwrap_or_else(|_| extension.to_owned());
            format!("{extension}%{name}")
        }
        None => id.to_owned(),
    }
}

fn to_build_option(value: Value<'_>) -> Option<BuildOption> {
    use starlark::values::list::ListRef;
    if value.is_none() {
        return Some(BuildOption::None);
    }
    if let Some(flag) = value.unpack_bool() {
        return Some(BuildOption::Bool(flag));
    }
    if let Some(number) = value.unpack_i32() {
        return Some(BuildOption::Int(number.into()));
    }
    if let Some(text) = value.unpack_str() {
        return Some(BuildOption::String(text.to_owned()));
    }
    if let Some(label) = crate::starlark_types::label_canonical_form(value) {
        return Some(BuildOption::Label(label));
    }
    if let Some((type_name, constant)) = crate::starlark_transitions::unpack_option_enum(value) {
        return Some(BuildOption::Enum {
            type_name: type_name.to_owned(),
            value: constant.to_owned(),
        });
    }
    let list = ListRef::from_value(value)?;
    let items: Vec<Value> = list.iter().collect();
    if let Some(strings) = items
        .iter()
        .map(|item| item.unpack_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    {
        return Some(BuildOption::Strings(strings));
    }
    items
        .iter()
        .map(|item| crate::starlark_types::label_canonical_form(*item))
        .collect::<Option<Vec<_>>>()
        .map(BuildOption::Labels)
}

/// `build_options(target)` of one configuration
/// (StarlarkOutputFormatterCallback.buildOptions): every modelled native
/// option as `//command_line_option:NAME`, then the Starlark settings the
/// options hold (those that differ from their default). Unmodelled native
/// options are absent, never invented.
pub fn build_options(
    configuration: &crate::configuration::Configuration,
) -> Vec<(String, BuildOption)> {
    use crate::configuration::OptionValue;
    let native = &configuration.native;
    let mut result = Vec::new();
    Module::with_temp_heap(|module| {
        let heap = module.heap();
        for (name, _, _, _) in crate::bazel_options::FRAGMENT_OPTIONS {
            // A non-empty List<Map.Entry> is not a Starlark value (skipped,
            // like Starlark.fromJava's IllegalArgumentException).
            if *name == "define" && !native.define.is_empty() {
                continue;
            }
            let value = match crate::starlark_transitions::native_input(native, name, heap) {
                Ok(value) => to_build_option(value),
                Err(_) => match native.raw_value(name) {
                    Some(OptionValue::Null) | Some(OptionValue::DottedVersion(None)) => {
                        Some(BuildOption::None)
                    }
                    Some(OptionValue::Bool(flag)) => Some(BuildOption::Bool(flag)),
                    Some(OptionValue::String(text))
                    | Some(OptionValue::DottedVersion(Some(text))) => {
                        Some(BuildOption::String(text))
                    }
                    Some(OptionValue::Label(label)) => Some(BuildOption::Label(label.to_string())),
                    Some(OptionValue::Labels(labels)) => Some(BuildOption::Labels(
                        labels.iter().map(ToString::to_string).collect(),
                    )),
                    Some(OptionValue::Assignments(entries)) if entries.is_empty() => {
                        Some(BuildOption::Strings(Vec::new()))
                    }
                    _ => None,
                },
            };
            if let Some(value) = value {
                result.push((format!("//command_line_option:{name}"), value));
            }
        }
        for (label, value) in &configuration.starlark {
            use crate::configuration::SettingValue;
            let value = match value {
                SettingValue::Bool(flag) => BuildOption::Bool(*flag),
                SettingValue::Int(number) => BuildOption::Int(*number),
                SettingValue::String(text) => BuildOption::String(text.clone()),
                SettingValue::StringList(items) => BuildOption::Strings(items.clone()),
                SettingValue::Sequence { items, .. } => BuildOption::Strings(items.clone()),
                SettingValue::Label(label) => BuildOption::Label(label.to_string()),
            };
            result.push((crate::rule_declarations::canonical_form(label), value));
        }
    });
    result
}

fn alloc_build_option<'v>(heap: Heap<'v>, value: &BuildOption) -> anyhow::Result<Value<'v>> {
    let label = |text: &str| crate::starlark_types::alloc_label(heap, text);
    Ok(match value {
        BuildOption::None => Value::new_none(),
        BuildOption::Bool(flag) => Value::new_bool(*flag),
        BuildOption::Int(number) => heap.alloc(*number),
        BuildOption::String(text) => heap.alloc(text.as_str()),
        BuildOption::Strings(items) => heap.alloc(items.clone()),
        BuildOption::Label(text) => label(text)?,
        BuildOption::Labels(items) => heap.alloc(
            items
                .iter()
                .map(|item| label(item))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        BuildOption::Enum { type_name, value } => {
            crate::starlark_transitions::alloc_option_enum(heap, type_name, value)
                .ok_or_else(|| anyhow::anyhow!("unknown {type_name} constant {value}"))?
        }
    })
}

#[allow(unsafe_code)]
mod values {
    use allocative::Allocative;
    use starlark::starlark_simple_value;
    use starlark::values::{
        Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
        starlark_value,
    };
    use std::fmt;

    /// A configured result as the formatter's `target` (ConfiguredTarget):
    /// its repr and the label `target.label` reads.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct CqueryTarget {
        pub repr: String,
        pub label: String,
    }
    starlark_simple_value!(CqueryTarget);

    impl fmt::Display for CqueryTarget {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.repr)
        }
    }

    #[starlark_value(type = "Target")]
    impl<'v> StarlarkValue<'v> for CqueryTarget {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "label" => crate::starlark_types::alloc_label(heap, &self.label).ok(),
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            vec!["label".to_owned()]
        }

        fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
            Some(format!(
                "native cquery --output=starlark retains only `label` of a configured target; \
                 {attribute:?} needs provider values, which the native configured graph does not keep"
            ))
        }
    }

    /// A `providers(target)` value: the graph keeps provider names only.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct CqueryProvider(pub String);
    starlark_simple_value!(CqueryProvider);

    impl fmt::Display for CqueryProvider {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "<provider {} (value not retained by native cquery)>",
                self.0
            )
        }
    }

    #[starlark_value(type = "Provider")]
    impl<'v> StarlarkValue<'v> for CqueryProvider {
        fn get_attr(&self, _name: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
            None
        }

        fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
            Some(format!(
                "native cquery --output=starlark does not retain provider values: \
                 cannot read {attribute:?} of {}",
                self.0
            ))
        }
    }
}
use values::{CqueryProvider, CqueryTarget};

/// One configured result as the formatter sees it.
pub struct TargetView<'a> {
    /// The original (requested) label, for error messages.
    pub original: &'a str,
    /// `target.label`: the actual target of an alias.
    pub label: &'a str,
    pub repr: String,
    /// `providers(target)` keys; `None` is Starlark None.
    pub providers: Option<&'a [String]>,
    /// Why the providers of this target were not recorded, if they were not.
    pub providers_error: Option<&'a str>,
    /// `build_options(target)`; `None` for null-configuration targets.
    pub build_options: Option<&'a [(String, BuildOption)]>,
}

/// The configured target's repr (AbstractConfiguredTarget.repr and
/// overrides). `alias` is the requested label when it names an alias.
pub fn target_repr(target: &wire::Target, label: &str, alias: Option<&str>) -> String {
    let shown = |text: &str| {
        text.strip_prefix("@@//")
            .map_or(text.to_owned(), |rest| format!("//{rest}"))
    };
    if let Some(alias) = alias {
        return format!("<alias target {} of {}>", shown(alias), shown(label));
    }
    match target {
        wire::Target::Rule(_) => format!("<target {}>", shown(label)),
        wire::Target::SourceFile(_) => format!("<input file target {}>", shown(label)),
        wire::Target::GeneratedFile(_) => format!("<output file target {}>", shown(label)),
        wire::Target::PackageGroup(_) => format!("<unknown target {}>", shown(label)),
    }
}

/// The compiled `format` function (StarlarkOutputFormatterCallback's
/// constructor checks, in its order and wording).
pub struct Formatter {
    module: FrozenModule,
}

const EXPR_FILE: &str = "--starlark:expr";

fn dialect() -> Dialect {
    Dialect {
        enable_keyword_only_arguments: true,
        enable_load_reexport: false,
        ..Dialect::Standard
    }
}

fn first_line(error: impl std::fmt::Display) -> String {
    let text = error.to_string();
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// The cquery dialect globals: the Starlark universe, `depset`, `json`,
/// `struct` (StarlarkGlobalsImpl.getUtilToplevelsForCquery) and the two
/// CqueryDialectGlobals methods.
fn globals() -> Globals {
    let analysis = GlobalsBuilder::new()
        .with(crate::starlark_types::analysis_globals)
        .build();
    let mut builder = GlobalsBuilder::extended_by(&[LibraryExtension::Print]);
    builder.frozen_heap().add_reference(analysis.heap());
    for (name, value) in analysis.iter() {
        if name == "depset" {
            builder.set(name, value);
        }
    }
    crate::starlark_types::install_struct_provider(&mut builder);
    crate::json::install(&mut builder);
    builder.with(dialect_globals).build()
}

#[starlark::starlark_module]
fn dialect_globals(builder: &mut GlobalsBuilder) {
    fn providers<'v>(
        #[starlark(require = pos)] target: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = target;
        let context = context(eval)?;
        if let Some(error) = context.providers_error {
            return Err(anyhow::anyhow!("{error}"));
        }
        let heap = eval.heap();
        Ok(match context.providers {
            None => Value::new_none(),
            Some(names) => heap.alloc(AllocDict(
                names
                    .iter()
                    .map(|name| (name.as_str(), heap.alloc(CqueryProvider(name.clone()))))
                    .collect::<Vec<_>>(),
            )),
        })
    }

    fn build_options<'v>(
        #[starlark(require = pos)] target: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = target;
        let context = context(eval)?;
        let heap = eval.heap();
        Ok(match context.build_options {
            None => Value::new_none(),
            Some(options) => {
                let mut entries = Vec::with_capacity(options.len());
                for (name, value) in options {
                    entries.push((name.as_str(), alloc_build_option(heap, value)?));
                }
                heap.alloc(AllocDict(entries))
            }
        })
    }
}

/// The current target's data for `providers()`/`build_options()`: the one
/// argument a `format` call can pass is the current target (CqueryNode).
#[allow(unsafe_code)]
mod context {
    use super::BuildOption;
    #[derive(starlark::any::ProvidesStaticType)]
    pub(super) struct Context<'a> {
        pub providers: Option<&'a [String]>,
        pub providers_error: Option<&'a str>,
        pub build_options: Option<&'a [(String, BuildOption)]>,
    }
}
use context::Context;

fn context<'a, 'e>(eval: &Evaluator<'_, 'a, 'e>) -> anyhow::Result<&'a Context<'e>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow::anyhow!("providers()/build_options() outside cquery formatting"))
}

impl Formatter {
    pub fn new(expr: &str, file: &str) -> Result<Self, String> {
        let (source, filename, prefix) = if !file.is_empty() {
            if !expr.is_empty() {
                return Err("You must not specify both --starlark:expr and --starlark:file".into());
            }
            let prefix = "invalid --starlark:file: ";
            let source = std::fs::read_to_string(file)
                .map_err(|_| format!("{prefix}failed to read {file}"))?;
            (source, file.to_owned(), prefix)
        } else {
            let prefix = "invalid --starlark:expr: ";
            let expr = if expr.is_empty() {
                "str(target.label)"
            } else {
                expr
            };
            // A pure expression: it must not escape its scope through
            // unbalanced parentheses (Expression.parse).
            let ast = AstModule::parse(EXPR_FILE, expr.to_owned(), &dialect())
                .map_err(|e| format!("{prefix}{}", first_line(e)))?;
            if !is_single_expression(&ast) {
                return Err(format!("{prefix}syntax error: not an expression"));
            }
            (
                format!("def format(target): return ({expr})"),
                EXPR_FILE.to_owned(),
                prefix,
            )
        };
        let ast = AstModule::parse(&filename, source, &dialect())
            .map_err(|e| format!("{prefix}{}", first_line(e)))?;
        let globals = globals();
        let module = Module::with_temp_heap(|module| -> Result<FrozenModule, String> {
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals)
                    .map_err(|e| format!("{prefix}{e}"))?;
            }
            module.freeze().map_err(|e| format!("{prefix}{e:?}"))
        })?;
        let format = module
            .get_option("format")
            .map_err(|e| format!("{prefix}{e}"))?
            .ok_or_else(|| format!("{prefix}file does not define 'format'"))?;
        let value = format.value();
        if value.get_type() != "function" {
            return Err(format!(
                "{prefix}got {} for 'format', want function",
                value.get_type()
            ));
        }
        let parameters = format
            .value()
            .unpack_frozen()
            .and_then(|frozen| starlark::eval::PortableDef::of(frozen).ok())
            .map(|def| def.params.len());
        if parameters != Some(1) {
            return Err(format!(
                "{prefix}'format' function must take exactly 1 argument"
            ));
        }
        Ok(Self { module })
    }

    fn function(&self) -> OwnedFrozenValue {
        self.module
            .get("format")
            .expect("format was checked at construction")
    }

    /// `Starlark.str(format(target))`, or the error event's text
    /// (`Starlark evaluation error for LABEL: MESSAGE`).
    pub fn format(&self, target: &TargetView<'_>) -> Result<String, String> {
        let context = Context {
            providers: target.providers,
            providers_error: target.providers_error,
            build_options: target.build_options,
        };
        let function = self.function();
        Module::with_temp_heap(|module| -> Result<String, String> {
            let function = module.heap().access_owned_frozen_value(&function);
            let value = module.heap().alloc(CqueryTarget {
                repr: target.repr.clone(),
                label: target.label.to_owned(),
            });
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            let result = eval.eval_function(function, &[value], &[]);
            match result {
                Ok(value) => Ok(value.to_str()),
                Err(error) => Err(format!(
                    "Starlark evaluation error for {}: {}",
                    target.original,
                    evaluation_message(&error)
                )),
            }
        })
    }
}

/// Bazel's `getMessageWithStack` shape: one line per Starlark frame at
/// its current position, then `Error: MSG` (or `Error in BUILTIN: MSG`).
fn evaluation_message(error: &starlark::Error) -> String {
    let frames = &error.call_stack().frames;
    let mut text = String::from("Traceback (most recent call last):\n");
    let mut builtin = None;
    for (index, frame) in frames.iter().enumerate() {
        let position = match frames.get(index + 1) {
            Some(next) => next.location.as_ref(),
            // An error raised inside a native callee carries the callee's
            // own call site as its span.
            None if index > 0
                && (error.span().is_none() || error.span() == frame.location.as_ref()) =>
            {
                builtin = Some(frame.name.as_str());
                continue;
            }
            None => error.span(),
        };
        if let Some(location) = position {
            let begin = location.resolve_span().begin;
            // Java locates a call at its `(` (CallExpression.getLocation).
            let source = location.source_span();
            let column = match source.find('(') {
                Some(paren)
                    if source.ends_with(')')
                        && source[..paren]
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '.') =>
                {
                    begin.column + paren
                }
                _ => begin.column,
            };
            text.push_str(&format!(
                "\tFile \"{}\", line {}, column {}, in {}\n",
                location.filename(),
                begin.line + 1,
                column + 1,
                frame.name
            ));
        }
    }
    let message = first_line(error.kind());
    match builtin {
        Some(name) => {
            text.push_str(&format!("Error in {name}: "));
            let own = format!("{name}: ");
            text.push_str(message.strip_prefix(&own).unwrap_or(&message));
        }
        None => {
            text.push_str("Error: ");
            text.push_str(&message);
        }
    }
    text
}

fn is_single_expression(ast: &AstModule) -> bool {
    use starlark::syntax::ast::Stmt;
    match &ast.statement().node {
        Stmt::Expression(_) => true,
        Stmt::Statements(statements) => {
            statements.len() == 1 && matches!(statements[0].node, Stmt::Expression(_))
        }
        _ => false,
    }
}
