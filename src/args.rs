//! Analysis-only Args builders. Unlike Bazel, transformations of known values
//! run at mutation time, not execution time; only owned data is retained.
//! Values that are tree artifacts (with expand_directories) are kept and
//! expanded when the consuming action executes, calling a frozen top-level
//! map_each (crate::command_line). Builders cannot be frozen, iterated, or
//! converted to lists from Starlark. Two-argument DirectoryExpander callbacks
//! are unsupported.
// Match Bazel's keyword-rich Args API, including generated call trampolines.
#![allow(clippy::too_many_arguments)]

use crate::command_line::{
    DeferredVector, Group, Item, ParamFileInfo, ParamFileType, Part, VectorShape,
};
use crate::starlark_types::File;
use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::list::ListRef;
use starlark::values::none::{NoneOr, NoneType};
use starlark::values::starlark_value;
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLike,
};
use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// use_param_file / set_param_file_format state (Args.MutableArgs,
/// analysis/starlark/Args.java:262-292, 548-595).
#[derive(Debug, Default)]
pub(crate) struct ParamState {
    flag_format: Option<String>,
    always: bool,
    /// The file type and flag_per_line; None until set_param_file_format.
    format: Option<(ParamFileType, bool)>,
}

// Only upstream derives generate unsafe marker implementations. No heap values
// are retained: all fields are ordinary owned Rust data.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct Args {
        #[allocative(skip)]
        #[trace(static)]
        pub(super) groups: RefCell<Vec<Group>>,
        #[allocative(skip)]
        #[trace(static)]
        pub(super) param: RefCell<ParamState>,
        #[allocative(skip)]
        #[trace(static)]
        pub(super) active: Arc<AtomicBool>,
    }
}
use owned::Args;

impl Args {
    fn from_value(value: Value<'_>) -> anyhow::Result<&Self> {
        value
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("expected Args"))
    }

    fn check_active(&self) -> anyhow::Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "Args is no longer active: rule analysis has ended"
        );
        Ok(())
    }

    fn push(&self, group: Group) -> anyhow::Result<()> {
        self.check_active()?;
        self.groups.borrow_mut().push(group);
        Ok(())
    }

    #[cfg(test)]
    fn append(&self, values: Vec<String>) -> anyhow::Result<()> {
        self.push(Group::Strings(values))
    }

    fn flag_per_line(&self) -> bool {
        self.param.borrow().format.is_some_and(|(_, line)| line)
    }

    /// getParameterFileType: SHELL_QUOTED unless set.
    fn file_type(&self) -> ParamFileType {
        self.param
            .borrow()
            .format
            .map_or(ParamFileType::Shell, |(file_type, _)| file_type)
    }

    /// getParamFileInfo: only after use_param_file; flagsOnly is flag_per_line.
    fn param_file_info(&self) -> Option<ParamFileInfo> {
        let param = self.param.borrow();
        let flag_format = param.flag_format.clone()?;
        Some(ParamFileInfo {
            flag_format,
            always: param.always,
            file_type: self.file_type(),
            flags_only: self.flag_per_line(),
        })
    }

    fn snapshot(&self) -> anyhow::Result<ArgsSnapshot> {
        self.check_active()?;
        Ok(ArgsSnapshot {
            groups: self.groups.borrow().clone(),
            flag_per_line: self.flag_per_line(),
            file_type: self.file_type(),
            param: self.param_file_info(),
        })
    }
}

/// An immutable copy of an Args object at action registration.
#[derive(Clone, Debug)]
pub(crate) struct ArgsSnapshot {
    pub groups: Vec<Group>,
    pub flag_per_line: bool,
    pub file_type: ParamFileType,
    pub param: Option<ParamFileInfo>,
}

impl ArgsSnapshot {
    /// getDirectoryArtifacts: tree artifacts expanded by this command line.
    pub(crate) fn directories(&self) -> Vec<File> {
        directories(&self.groups)
    }
}

pub(crate) fn directories(groups: &[Group]) -> Vec<File> {
    let mut result = Vec::new();
    for group in groups {
        if let Group::Deferred(vector) = group {
            for item in &vector.items {
                if let Item::Directory(file) = item
                    && !result.contains(file)
                {
                    result.push(file.clone());
                }
            }
        }
    }
    result
}

/// Args.repr (analysis/starlark/Args.java:67-70); str() is repr().
impl fmt::Display for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("context.args() object")
    }
}

starlark::methods_static!(ARGS_METHODS = args_methods);

#[starlark_value(type = "Args")]
impl<'v> StarlarkValue<'v> for Args {
    fn get_methods() -> Option<&'static Methods> {
        Some(ARGS_METHODS.methods())
    }

    /// Args.debugPrint (analysis/starlark/Args.java:72-83): the expanded
    /// arguments joined by spaces. (Bazel expands Labels there without a
    /// main repository mapping; native arguments are already expanded.)
    fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
        let expanded =
            crate::command_line::expand_groups(&self.groups.borrow(), self.flag_per_line(), None)
                .unwrap_or_default();
        collector.push_str(&expanded.join(" "));
    }
}

/// Allocate a mutable builder tied to one rule analysis. The owner must set
/// `active` to false when analysis ends, including error/unwind paths.
pub fn alloc_args<'v>(heap: Heap<'v>, active: Arc<AtomicBool>) -> Value<'v> {
    heap.alloc_complex_no_freeze(Args {
        groups: RefCell::new(Vec::new()),
        param: RefCell::new(ParamState::default()),
        active,
    })
}

/// The snapshot of a genuine, active Args value; None for other values.
pub(crate) fn snapshot(value: Value<'_>) -> anyhow::Result<Option<ArgsSnapshot>> {
    match value.downcast_ref::<Args>() {
        Some(args) => Ok(Some(args.snapshot()?)),
        None => Ok(None),
    }
}

/// actions.write content of an Args value without tree artifacts:
/// ParameterFileWriteAction writes the command line (flag_per_line grouping
/// applied) in the Args' parameter file format (shell-quoted by default).
#[cfg(test)]
pub(crate) fn serialize_write_content(value: Value<'_>) -> anyhow::Result<Vec<u8>> {
    let args = value
        .downcast_ref::<Args>()
        .ok_or_else(|| anyhow!("write content must be a string or Args"))?;
    serialize_write_content_of(&args.snapshot()?).map_err(anyhow::Error::msg)
}

pub(crate) fn serialize_write_content_of(snapshot: &ArgsSnapshot) -> Result<Vec<u8>, String> {
    let arguments =
        crate::command_line::expand_groups(&snapshot.groups, snapshot.flag_per_line, None)?;
    Ok(crate::command_line::param_file_bytes(
        &arguments,
        snapshot.file_type,
    ))
}

fn sequence(value: Value<'_>) -> anyhow::Result<Vec<Value<'_>>> {
    if let Some(list) = ListRef::from_value(value) {
        Ok(list.iter().collect())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Ok(tuple.iter().collect())
    } else {
        bail!("expected list or tuple, got {}", value.get_type())
    }
}

/// StarlarkActionFactory.buildCommandLine
/// (analysis/starlark/StarlarkActionFactory.java:690-717): consecutive strings
/// form one part; each Args is its own part with its ParamFileInfo.
pub(crate) fn command_line_parts(value: Value<'_>) -> anyhow::Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut strings: Option<Vec<String>> = None;
    for item in sequence(value)? {
        if let Some(text) = item.unpack_str() {
            strings.get_or_insert_with(Vec::new).push(text.to_owned());
        } else if let Some(args) = item.downcast_ref::<Args>() {
            if let Some(run) = strings.take() {
                parts.push(Part {
                    groups: vec![Group::Strings(run)],
                    flag_per_line: false,
                    param: None,
                });
            }
            let snapshot = args.snapshot()?;
            parts.push(Part {
                groups: snapshot.groups,
                flag_per_line: snapshot.flag_per_line,
                param: snapshot.param,
            });
        } else {
            bail!(
                "expected list of strings or ctx.actions.args() for arguments instead of {}",
                item.get_type()
            );
        }
    }
    if let Some(run) = strings {
        parts.push(Part {
            groups: vec![Group::Strings(run)],
            flag_per_line: false,
            param: None,
        });
    }
    Ok(parts)
}

/// Snapshot a list/tuple of strings and active Args objects without tree
/// artifacts or parameter files (CommandLines.allArguments).
pub fn expand_arguments(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::new();
    for part in command_line_parts(value)? {
        result.extend(
            crate::command_line::expand_groups(&part.groups, part.flag_per_line, None)
                .map_err(anyhow::Error::msg)?,
        );
    }
    Ok(result)
}

fn string_value(value: Value<'_>, eval: &Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
    if let Some(text) = value.unpack_str() {
        Ok(text.to_owned())
    } else if let Ok(file) = crate::starlark_types::unpack_file(value) {
        Ok(file.path)
    } else if let Some(label) = crate::starlark_types::label_command_line_form(value, eval) {
        // Label.getDisplayForm with the main repository mapping
        // (StarlarkCustomCommandLine.java:967-978): `@dep//pkg:x`, not `@@dep+//pkg:x`.
        Ok(label)
    } else if matches!(value.get_type(), "int" | "bool") {
        Ok(value.to_str())
    } else {
        bail!(
            "Args value must be a string, File, Label, int, or bool; got {}",
            value.get_type()
        )
    }
}

fn named_value<'v>(args: &[Value<'v>]) -> anyhow::Result<(Option<String>, Value<'v>)> {
    match args {
        [value] => Ok((None, *value)),
        [name, value] => Ok((
            Some(
                name.unpack_str()
                    .ok_or_else(|| anyhow!("arg name must be a string"))?
                    .to_owned(),
            ),
            *value,
        )),
        _ => bail!("Args method requires one value, or an arg name followed by a value"),
    }
}

fn formatted(template: Option<&str>, value: &str) -> anyhow::Result<String> {
    crate::command_line::format(template, value).map_err(anyhow::Error::msg)
}

fn check_mapper(
    map_each: Value<'_>,
    allow_closure: bool,
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<()> {
    if map_each.is_none() {
        return Ok(());
    }
    let name = map_each
        .function_name()
        .ok_or_else(|| anyhow!("map_each must be a function or None"))?;
    if !allow_closure {
        // Analysis runs in a fresh module. The loader retains identities of
        // actual top-level def declarations, including private/imported helpers.
        ensure!(
            !name.contains("lambda")
                && (crate::analysis_runtime::is_top_level_mapper(eval, map_each)
                    || eval
                        .module()
                        .get(name)
                        .is_some_and(|global| global.ptr_eq(map_each))),
            "map_each must be a top-level function visible in this analysis module; closures and unverified imported functions require allow_closure=True"
        );
    }
    Ok(())
}

fn map_one<'v>(
    item: Value<'v>,
    map_each: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<String>> {
    if map_each.is_none() {
        return Ok(vec![string_value(item, eval)?]);
    }
    let mapped = eval
        .eval_function(map_each, &[item], &[])
        .map_err(|e| anyhow!("map_each failed: {e}"))?;
    crate::command_line::mapped_strings(mapped).map_err(anyhow::Error::msg)
}

/// One add_all/add_joined call as a group: eager unless a value is a tree
/// artifact and expand_directories is set (VectorArg.maybeExpandDirectories).
fn vector_group<'v>(
    values: Value<'v>,
    shape: VectorShape,
    map_each: Value<'v>,
    expand_directories: bool,
    allow_closure: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Option<Group>> {
    formatted(shape.format_each.as_deref(), "")?;
    formatted(shape.format_joined.as_deref(), "")?;
    check_mapper(map_each, allow_closure, eval)?;
    let values = if values.get_type() == "depset" {
        let to_list = values
            .get_attr_error("to_list", eval.heap())
            .map_err(|e| anyhow!(e.to_string()))?;
        let list = eval
            .eval_function(to_list, &[], &[])
            .map_err(|e| anyhow!(e.to_string()))?;
        sequence(list)?
    } else {
        // Args.add_all/add_joined `values`: Sequence or Depset; a range is a
        // Sequence (RangeList).
        crate::starlark_types::bazel_sequence(values, eval.heap())
            .map_or_else(|| sequence(values), Ok)?
    };
    // Args.addVectorArg (Args.java:416-475): an empty omitted vector records
    // no argument group at all.
    if values.is_empty() && shape.omit_if_empty {
        return Ok(None);
    }
    let mut items: Vec<Item> = Vec::new();
    let mut deferred = false;
    for item in values {
        if expand_directories
            && let Some(file) = File::from_value(item)
            && file.is_directory
        {
            deferred = true;
            items.push(Item::Directory(file.clone()));
            continue;
        }
        let strings = map_one(item, map_each, eval)?;
        match items.last_mut() {
            Some(Item::Strings(previous)) => previous.extend(strings),
            _ => items.push(Item::Strings(strings)),
        }
    }
    if deferred {
        let map_each = if map_each.is_none() {
            None
        } else {
            Some(
                crate::analysis_runtime::owned_top_level_function(eval, map_each).ok_or_else(
                    || {
                        anyhow!(
                            "map_each over the contents of a directory requires a top-level function of a loaded .bzl file (native execution-time expansion)"
                        )
                    },
                )?,
            )
        };
        return Ok(Some(Group::Deferred(Arc::new(DeferredVector {
            shape,
            items,
            map_each,
        }))));
    }
    let values = items
        .into_iter()
        .flat_map(|item| match item {
            Item::Strings(strings) => strings,
            Item::Directory(_) => unreachable!("no directory items"),
        })
        .collect();
    Ok(Some(Group::Strings(
        crate::command_line::assemble(&shape, values).map_err(anyhow::Error::msg)?,
    )))
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn args_methods(builder: &mut MethodsBuilder) {
    fn add<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(default = NoneOr::None)] format: NoneOr<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let (name, value) = named_value(&args.items)?;
        // Args.validateNoDirectory (analysis/starlark/Args.java:534-541).
        if File::from_value(value).is_some_and(|file| file.is_directory) {
            bail!(
                "Cannot add directories to Args#add since they may expand to multiple values. Either use Args#add_all (if you want expansion) or args.add(directory.path) (if you do not)."
            );
        }
        let mut values: Vec<_> = name.into_iter().collect();
        values.push(formatted(
            format.into_option(),
            &string_value(value, eval)?,
        )?);
        builder.push(Group::Strings(values))?;
        Ok(this)
    }

    fn add_all<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(default = NoneType)] map_each: Value<'v>,
        #[starlark(default = NoneOr::None)] format_each: NoneOr<&str>,
        #[starlark(default = NoneOr::None)] before_each: NoneOr<&str>,
        #[starlark(default = true)] omit_if_empty: bool,
        #[starlark(default = false)] uniquify: bool,
        #[starlark(default = true)] expand_directories: bool,
        #[starlark(default = NoneOr::None)] terminate_with: NoneOr<&str>,
        #[starlark(default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let (arg_name, values) = named_value(&args.items)?;
        let shape = VectorShape {
            arg_name,
            format_each: format_each.into_option().map(str::to_owned),
            before_each: before_each.into_option().map(str::to_owned),
            omit_if_empty,
            uniquify,
            terminate_with: terminate_with.into_option().map(str::to_owned),
            ..VectorShape::default()
        };
        if let Some(group) = vector_group(
            values,
            shape,
            map_each,
            expand_directories,
            allow_closure,
            eval,
        )? {
            builder.push(group)?;
        }
        Ok(this)
    }

    fn add_joined<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        join_with: &str,
        #[starlark(default = NoneType)] map_each: Value<'v>,
        #[starlark(default = NoneOr::None)] format_each: NoneOr<&str>,
        #[starlark(default = NoneOr::None)] format_joined: NoneOr<&str>,
        #[starlark(default = true)] omit_if_empty: bool,
        #[starlark(default = false)] uniquify: bool,
        #[starlark(default = true)] expand_directories: bool,
        #[starlark(default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let (arg_name, values) = named_value(&args.items)?;
        let shape = VectorShape {
            arg_name,
            format_each: format_each.into_option().map(str::to_owned),
            join_with: Some(join_with.to_owned()),
            format_joined: format_joined.into_option().map(str::to_owned),
            omit_if_empty,
            uniquify,
            ..VectorShape::default()
        };
        if let Some(group) = vector_group(
            values,
            shape,
            map_each,
            expand_directories,
            allow_closure,
            eval,
        )? {
            builder.push(group)?;
        }
        Ok(this)
    }

    /// Args.useParamsFile (analysis/starlark/Args.java:548-563).
    fn use_param_file<'v>(
        this: Value<'v>,
        param_file_arg: &str,
        #[starlark(require = named, default = false)] use_always: bool,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        if crate::command_line::format(Some(param_file_arg), "").is_err() {
            bail!(
                "Invalid value for parameter \"param_file_arg\": Expected string with a single \"%s\", got \"{param_file_arg}\""
            );
        }
        let mut param = builder.param.borrow_mut();
        param.flag_format = Some(param_file_arg.to_owned());
        param.always = use_always;
        Ok(this)
    }

    /// Args.setParamFileFormat (analysis/starlark/Args.java:565-595).
    fn set_param_file_format<'v>(this: Value<'v>, format: &str) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let mut param = builder.param.borrow_mut();
        if param.format.is_some() {
            bail!("set_param_file_format() may only be called once");
        }
        param.format = Some(match format {
            "shell" => (ParamFileType::Shell, false),
            "multiline" => (ParamFileType::Unquoted, false),
            "flag_per_line" => (ParamFileType::Unquoted, true),
            _ => bail!(
                "Invalid value for parameter \"format\": Expected one of \"shell\", \"multiline\", \"flag_per_line\""
            ),
        });
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{GlobalsBuilder, Module};
    use starlark::syntax::{AstModule, Dialect};

    #[starlark_module]
    fn test_globals(builder: &mut GlobalsBuilder) {
        fn expanded<'v>(arguments: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
            Ok(heap.alloc(expand_arguments(arguments)?))
        }

        fn serialized(arguments: Value<'_>) -> anyhow::Result<String> {
            Ok(String::from_utf8(serialize_write_content(arguments)?)?)
        }
    }

    // An intentionally misleading runtime type name must not grant Args access.
    #[allow(unsafe_code)]
    mod spoof {
        use super::*;
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct SpoofArgs;
        impl fmt::Display for SpoofArgs {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("<Args>")
            }
        }
        starlark::starlark_simple_value!(SpoofArgs);
        #[starlark_value(type = "Args")]
        impl<'v> StarlarkValue<'v> for SpoofArgs {}
    }

    fn eval(source: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            module.set(
                "a",
                alloc_args(module.heap(), Arc::new(AtomicBool::new(true))),
            );
            module.set(
                "file",
                crate::starlark_types::alloc_file(
                    module.heap(),
                    crate::starlark_types::File {
                        is_directory: false,
                        path: "pkg/file with $ spaces".into(),
                        short_path: "pkg/file with $ spaces".into(),
                        owner: "//pkg:rule".into(),
                        is_source: true,
                    },
                ),
            );
            let globals = GlobalsBuilder::standard()
                .with(crate::starlark_types::analysis_globals)
                .with(test_globals)
                .build();
            let ast = AstModule::parse("args_test.star", source.to_owned(), &Dialect::Standard)
                .map_err(|e| e.to_string())?;
            Evaluator::new(&module)
                .eval_module(ast, &globals)
                .map(|value| value.to_repr())
                .map_err(|e| e.to_string())
        })
    }

    fn pass(source: &str) {
        assert_eq!(eval(source).unwrap(), "True", "{source}");
    }

    fn fail(source: &str, message: &str) {
        let error = eval(source).unwrap_err();
        assert!(error.contains(message), "{source}: {error}");
    }

    #[test]
    fn default_write_bytes_match_pinned_shell_escaper_and_reference_fixture() {
        Module::with_temp_heap(|module| {
            let active = Arc::new(AtomicBool::new(true));
            let value = alloc_args(module.heap(), active.clone());
            let builder = Args::from_value(value).unwrap();
            assert!(serialize_write_content(value).unwrap().is_empty());
            for (text, expected) in [
                ("plain", "plain\n"),
                ("aAzZ09@%-_+:,./", "aAzZ09@%-_+:,./\n"),
                ("a~b", "a~b\n"),
                ("./~", "./~\n"),
                ("~", "'~'\n"),
                ("~user/file", "'~user/file'\n"),
                ("", "''\n"),
                ("space value", "'space value'\n"),
                ("single'quote", "'single'\\''quote'\n"),
                ("'", "''\\'''\n"),
                ("''", "''\\'''\\'''\n"),
                ("double\"quote", "'double\"quote'\n"),
                ("back\\slash", "'back\\slash'\n"),
                ("line1\nline2", "'line1\nline2'\n"),
                ("\t\r", "'\t\r'\n"),
                ("雪é🙂", "'雪é🙂'\n"),
                ("x=y", "'x=y'\n"),
                ("!#$&*?[]();<>|`", "'!#$&*?[]();<>|`'\n"),
                ("\0", "'\0'\n"),
            ] {
                builder.groups.borrow_mut().clear();
                builder.append(vec![text.to_owned()]).unwrap();
                assert_eq!(
                    serialize_write_content(value).unwrap(),
                    expected.as_bytes(),
                    "{text:?}"
                );
            }
            builder.groups.borrow_mut().clear();
            builder
                .append(
                    [
                        "plain",
                        "space value",
                        "single'quote",
                        "double\"quote",
                        "back\\slash",
                        "",
                        "line1\nline2",
                        "雪é🙂",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                )
                .unwrap();
            let bytes = serialize_write_content(value).unwrap();
            // Actual mixed.out/mixed.exec bytes from actions-write-args-contract.
            assert_eq!(bytes, "plain\n'space value'\n'single'\\''quote'\n'double\"quote'\n'back\\slash'\n''\n'line1\nline2'\n'雪é🙂'\n".as_bytes());
            active.store(false, Ordering::Release);
            assert!(
                serialize_write_content(value)
                    .unwrap_err()
                    .to_string()
                    .contains("analysis has ended")
            );
            let spoof = module.heap().alloc(spoof::SpoofArgs);
            assert_eq!(spoof.get_type(), "Args");
            assert!(
                serialize_write_content(spoof)
                    .unwrap_err()
                    .to_string()
                    .contains("string or Args")
            );
        });
    }

    #[test]
    fn write_serializes_eager_formatting_and_mapping() {
        pass(
            r#"
a.add('v w', format='pre:%s:%%')
a.add_all(['a', 'b c', ''], format_each='<%s>', before_each='--value', terminate_with='--end')
serialized(a) == "'pre:v w:%'\n--value\n'<a>'\n--value\n'<b c>'\n--value\n'<>'\n--end\n"
"#,
        );
        pass(
            r#"
def mapper(value):
    if value == 'skip':
        return None
    if value == 'two':
        return ['x y', "q'z"]
    return value
a.add_all(['skip', 'two', 'z'], map_each=mapper, format_each='[%s]')
serialized(a) == "'[x y]'\n'[q'\\''z]'\n'[z]'\n"
"#,
        );
        for source in [
            "serialized([])",
            "serialized(())",
            "serialized({})",
            "serialized('not Args')",
            "serialized(None)",
            "serialized(depset([]))",
        ] {
            fail(source, "string or Args");
        }
        // The existing native eager boundary is explicit: mapper failure occurs
        // at add_all, not at genuine Bazel's later write-action execution phase.
        fail(
            "def bad(x):\n    fail('EAGER_NATIVE_MAPPER')\na.add_all(['x'], map_each=bad)\nserialized(a)",
            "EAGER_NATIVE_MAPPER",
        );
    }

    #[test]
    fn param_file_formats_shape_written_content() {
        // ParameterFileWriteAction uses the Args' file type and flag_per_line
        // grouping (StarlarkActionFactory.write, Args.build).
        pass(
            r#"
a.set_param_file_format("multiline").use_param_file("@%s", use_always=True)
a.add("--k", "v w").add_all(["x y", "z"])
serialized(a) == "--k\nv w\nx y\nz\n"
"#,
        );
        pass(
            r#"
a.set_param_file_format("flag_per_line")
a.add("--k", "v w").add_all("--many", ["x", "y"]).add("pos").add_all(["p", "q"])
serialized(a) == "--k=v w\n--many=x y\npos\np=q\n" and expanded([a]) == ["--k=v w", "--many=x y", "pos", "p=q"]
"#,
        );
        pass("a.set_param_file_format('shell')\na.add('x y')\nserialized(a) == \"'x y'\\n\"");
        fail(
            "a.set_param_file_format('shell').set_param_file_format('shell')",
            "may only be called once",
        );
        fail(
            "a.set_param_file_format('other')",
            "Expected one of \"shell\", \"multiline\", \"flag_per_line\"",
        );
        for invalid in ["@", "@%s%s", "%d", "%"] {
            fail(
                &format!("a.use_param_file({invalid:?})"),
                "Invalid value for parameter \"param_file_arg\"",
            );
        }
    }

    #[test]
    fn add_converts_supported_scalars_formats_and_chains() {
        pass(
            r#"
a.add("--flag").add("--file", file, format="pre:%s:%%:post").add(Label("//pkg:rule")).add(42).add(True).add("雪 $HOME", format=None)
expanded(["before", a, "after"]) == ["before", "--flag", "--file", "pre:pkg/file with $ spaces:%:post", "//pkg:rule", "42", "True", "雪 $HOME", "after"]
"#,
        );
    }

    #[test]
    fn add_all_and_joined_transform_in_order() {
        pass(
            r#"
a.add_all("--files", [file, file, ""], format_each="[%s]", uniquify=True, before_each="-I", terminate_with="--end")
a.add_joined("--joined", ("b", "a", "b"), join_with=",", format_each="<%s>", format_joined="%%:%s:%%", uniquify=True)
expanded((a,)) == ["--files", "-I", "[pkg/file with $ spaces]", "-I", "[]", "--end", "--joined", "%:<b>,<a>:%"]
"#,
        );
    }

    #[test]
    fn depsets_and_empty_values_follow_omit_rules() {
        pass(
            r#"
a.add_all(depset(["b", "a", "b"])).add_all(depset([file]))
a.add_all("skip", [], terminate_with="skip-end").add_joined("skip", [], join_with=",")
a.add_all("keep", [], omit_if_empty=False, terminate_with="end")
a.add_joined("empty", [], omit_if_empty=False, join_with=",", format_joined="[%s]")
a.add_all([""], expand_directories=False)
expanded([a]) == ["b", "a", "pkg/file with $ spaces", "keep", "end", "empty", "[]", ""]
"#,
        );
    }

    #[test]
    fn map_each_supports_none_string_and_lists_before_formatting_and_dedup() {
        pass(
            r#"
def mapper(value):
    if value == "skip":
        return None
    if value == "two":
        return ["x", "y", "x"]
    return value
a.add_all("--map", ["skip", "two", "z"], map_each=mapper, format_each="<%s>", before_each="+", uniquify=True)
a.add_joined(["skip", "two"], join_with=":", map_each=mapper, uniquify=True)
a.add_all("omitted", ["skip"], map_each=mapper)
expanded([a]) == ["--map", "+", "<x>", "+", "<y>", "+", "<z>", "x:y"]
"#,
        );
    }

    #[test]
    fn map_each_can_receive_file_and_explicit_closures() {
        pass(
            r#"
def mapper(value):
    return value.path
a.add_all([file], map_each=mapper)
def build():
    prefix = "prefix:"
    def closure(value):
        return prefix + value
    a.add_joined(["x", "y"], join_with=",", map_each=closure, allow_closure=True)
build()
expanded([a]) == ["pkg/file with $ spaces", "prefix:x,prefix:y"]
"#,
        );
        fail(
            "def build():\n    def nested(x):\n        return x\n    a.add_all(['x'], map_each=nested)\nbuild()",
            "allow_closure=True",
        );
    }

    #[test]
    fn invalid_values_formats_callbacks_and_param_files_fail_explicitly() {
        for value in ["[]", "()", "{}", "depset([])", "None", "1.5"] {
            fail(&format!("a.add({value})"), "Args value must be");
        }
        for format in ["plain", "%d", "%s%s", "%", "%%"] {
            fail(&format!("a.add('x', format={format:?})"), "exactly one %s");
            fail(
                &format!("a.add_all([], format_each={format:?})"),
                "exactly one %s",
            );
            fail(
                &format!("a.add_joined([], join_with=',', format_joined={format:?})"),
                "exactly one %s",
            );
        }
        fail("a.add()", "requires one value");
        fail("a.add('a', 'b', 'c')", "requires one value");
        fail("a.add(1, 'b')", "arg name must be a string");
        fail("a.add_all('abc')", "expected list or tuple");
        // Args.add_all `values` is a Sequence; a range is one (RangeList).
        pass("a.add_all('--n', range(2))\nexpanded([a]) == ['--n', '0', '1']");
        fail("a.add_all([], map_each=42)", "map_each must be a function");
        for returned in ["42", "['ok', 42]", "('tuple',)"] {
            fail(
                &format!(
                    "def mapper(x):\n    return {returned}\na.add_all(['x'], map_each=mapper)"
                ),
                "map_each must return",
            );
        }
        fail("a.to_list()", "to_list");
        // Starlark.toIterable (Starlark.java:284).
        fail("[x for x in a]", "type 'Args' is not iterable");
        fail("expanded(a)", "expected list or tuple");
        fail(
            "expanded([[]])",
            "expected list of strings or ctx.actions.args()",
        );
    }

    #[test]
    fn expansion_snapshots_and_ended_analysis_rejects_methods_and_expansion() {
        pass(
            "a.add('first')\nsnapshot = expanded([a])\na.add('second')\nsnapshot == ['first'] and expanded([a]) == ['first', 'second']",
        );
        Module::with_temp_heap(|module| {
            let active = Arc::new(AtomicBool::new(true));
            let args = alloc_args(module.heap(), active.clone());
            module.set("a", args);
            let globals = GlobalsBuilder::standard().with(test_globals).build();
            active.store(false, Ordering::Release);
            for source in [
                "a.add('x')",
                "a.add_all([])",
                "a.add_joined([], join_with=',')",
                "a.use_param_file('@%s')",
                "a.set_param_file_format('shell')",
                "expanded([a])",
            ] {
                let ast =
                    AstModule::parse("ended.star", source.to_owned(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err();
                assert!(error.to_string().contains("analysis has ended"), "{error}");
            }
        });
    }

    #[test]
    fn args_cannot_escape_via_module_freezing() {
        Module::with_temp_heap(|module| {
            module.set(
                "a",
                alloc_args(module.heap(), Arc::new(AtomicBool::new(true))),
            );
            assert!(module.freeze().is_err());
        });
    }
}
