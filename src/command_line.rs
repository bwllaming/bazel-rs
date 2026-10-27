//! Starlark action command lines (docs/ACTIONS_PARAMS.md): Args groups,
//! execution-time expansion of tree artifacts, `flag_per_line` grouping and
//! parameter files with Bazel's naming and threshold semantics.
//!
//! References (pinned Bazel 9.3 source):
//! - CommandLines.expand (actions/CommandLines.java:89-153): the threshold,
//!   the per-part 512-byte estimate and `<primary output>-<n>.params` names.
//! - ParameterFile.derivePath/writeParameterFile (actions/ParameterFile.java:79-101).
//! - StarlarkCustomCommandLine.VectorArg.preprocess
//!   (analysis/starlark/StarlarkCustomCommandLine.java:309-418) and the
//!   flag_per_line grouping (:995-1049, :1468-1477, :1696-1735).
//! - StarlarkActionFactory.buildCommandLine
//!   (analysis/starlark/StarlarkActionFactory.java:690-717).

use crate::starlark_types::File;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::OwnedFrozenValue;
use starlark::values::list::ListRef;
use std::path::PathBuf;
use std::sync::Arc;

/// `--min_param_file_size` default (analysis/config/CoreOptions.java:294-304).
pub(crate) const MIN_PARAM_FILE_SIZE: usize = 32768;
/// CommandLines.PARAM_FILE_ARG_LENGTH_ESTIMATE (actions/CommandLines.java:45).
const PARAM_FILE_ARG_LENGTH_ESTIMATE: usize = 512;

/// ParameterFile.ParameterFileType subset reachable from Starlark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ParamFileType {
    /// SHELL_QUOTED (`shell`, the default).
    Shell,
    /// UNQUOTED (`multiline`, `flag_per_line`).
    Unquoted,
}

/// ParamFileInfo (actions/ParamFileInfo.java:30-67).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParamFileInfo {
    pub flag_format: String,
    pub always: bool,
    pub file_type: ParamFileType,
    pub flags_only: bool,
}

/// The formatting of one add_all/add_joined call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct VectorShape {
    pub arg_name: Option<String>,
    pub format_each: Option<String>,
    pub before_each: Option<String>,
    pub join_with: Option<String>,
    pub format_joined: Option<String>,
    pub omit_if_empty: bool,
    pub uniquify: bool,
    pub terminate_with: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Item {
    /// Already converted (and map_each-mapped) values of a non-directory item.
    Strings(Vec<String>),
    /// A tree artifact expanded at execution time.
    Directory(#[serde(with = "crate::snapshot::codec::file")] File),
    /// A tree artifact passed as it is to a map_each that takes a
    /// DirectoryExpander (`expand_directories = False`), at execution time.
    Unexpanded(#[serde(with = "crate::snapshot::codec::file")] File),
}

/// The map_each function of a deferred vector, called at execution time: a
/// frozen top-level function, or any other callable (a closure with
/// `allow_closure = True`, a built-in such as `str`) retained by the
/// analysis module and set when that module freezes
/// (crate::analysis_runtime::Context::retain_mapper) — or, decoded from an
/// analysis snapshot, the function its portable description names,
/// resolved by the hit before anything executes (crate::snapshot::mapper).
#[derive(Clone, Debug)]
pub(crate) struct Mapper {
    function: MapperFunction,
    /// The function's portable description (crate::snapshot::mapper::
    /// MapperRef): set by planning once the analysis snapshot checked that
    /// it names this very function (crate::snapshot::mapper::settle), or
    /// decoded. A mapper without one keeps its request from being encoded.
    portable: Arc<std::sync::OnceLock<Arc<crate::snapshot::mapper::MapperRef>>>,
}

#[derive(Clone, Debug)]
enum MapperFunction {
    Frozen(OwnedFrozenValue),
    /// Filled when the analysis module freezes, or (a decoded mapper) when
    /// the snapshot hit resolves the description.
    Retained(Arc<std::sync::OnceLock<OwnedFrozenValue>>),
}

impl Mapper {
    /// A frozen top-level function.
    pub(crate) fn frozen(function: OwnedFrozenValue) -> Self {
        Self {
            function: MapperFunction::Frozen(function),
            portable: Arc::default(),
        }
    }

    /// A callable its analysis module retains (filled at its freeze).
    pub(crate) fn retained(cell: Arc<std::sync::OnceLock<OwnedFrozenValue>>) -> Self {
        Self {
            function: MapperFunction::Retained(cell),
            portable: Arc::default(),
        }
    }

    /// A mapper decoded from its description: [`Self::resolve`] sets its
    /// function.
    pub(crate) fn decoded(reference: crate::snapshot::mapper::MapperRef) -> Self {
        let portable = std::sync::OnceLock::new();
        let _ = portable.set(Arc::new(reference));
        Self {
            function: MapperFunction::Retained(Arc::default()),
            portable: Arc::new(portable),
        }
    }

    pub(crate) fn function(&self) -> Result<&OwnedFrozenValue, String> {
        match &self.function {
            MapperFunction::Frozen(function) => Ok(function),
            MapperFunction::Retained(cell) => cell.get().ok_or_else(|| {
                if self.portable.get().is_some() {
                    "internal error: a deferred map_each decoded from the analysis snapshot was not resolved"
                        .to_owned()
                } else {
                    "internal error: a deferred map_each was not retained by its analysis module"
                        .to_owned()
                }
            }),
        }
    }

    /// The portable description, once checked or decoded.
    pub(crate) fn portable(&self) -> Option<&Arc<crate::snapshot::mapper::MapperRef>> {
        self.portable.get()
    }

    /// Record the checked portable description (the first one wins).
    pub(crate) fn set_portable(&self, reference: Arc<crate::snapshot::mapper::MapperRef>) {
        let _ = self.portable.set(reference);
    }

    /// Set the function a decoded mapper's description names (once).
    pub(crate) fn resolve(&self, function: OwnedFrozenValue) -> Result<(), String> {
        match &self.function {
            MapperFunction::Retained(cell) if self.portable.get().is_some() => {
                let _ = cell.set(function);
                Ok(())
            }
            _ => Err("internal error: only a decoded map_each is resolved".to_owned()),
        }
    }
}

/// An add_all/add_joined call whose values contain tree artifacts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeferredVector {
    pub shape: VectorShape,
    pub items: Vec<Item>,
    /// The Starlark mapper of the tree children: an in-memory value, so a
    /// planned request that still needs one cannot be encoded
    /// (crate::snapshot::codec::map_each).
    #[serde(with = "crate::snapshot::codec::map_each")]
    pub map_each: Option<Mapper>,
    /// map_each takes a DirectoryExpander (wantsDirectoryExpander: a
    /// StarlarkFunction with two or more parameters).
    pub expander: bool,
}

/// The file listings a DirectoryExpander expands at execution time: each
/// tree artifact of the vector (by exec path) to its files.
pub(crate) type Listings = std::collections::BTreeMap<String, Vec<File>>;

// Only derive-generated marker implementations need the exception. The value
// owns its listings (Files), no heap references.
#[allow(unsafe_code)]
mod expander {
    use super::*;
    use allocative::Allocative;
    use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic};

    /// DirectoryExpander (starlarkbuildapi/DirectoryExpander.java,
    /// StarlarkCustomCommandLine.FullExpander :1099-1123): `expand(file)` is
    /// the files of a tree artifact input, or `[file]`. `listings` is `None`
    /// for the native's eager mapping of non-directory values at analysis
    /// time, where expanding a directory is an explicit gap.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct DirectoryExpander {
        #[allocative(skip)]
        pub listings: Option<Arc<Listings>>,
    }
    starlark::starlark_simple_value!(DirectoryExpander);
}
pub(crate) use expander::DirectoryExpander;

impl std::fmt::Display for DirectoryExpander {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<DirectoryExpander>")
    }
}

starlark::methods_static!(DIRECTORY_EXPANDER_METHODS = directory_expander_methods);

#[starlark::values::starlark_value(type = "DirectoryExpander")]
impl<'v> starlark::values::StarlarkValue<'v> for DirectoryExpander {
    fn get_methods() -> Option<&'static starlark::environment::Methods> {
        Some(DIRECTORY_EXPANDER_METHODS.methods())
    }
}

#[starlark::starlark_module]
fn directory_expander_methods(builder: &mut starlark::environment::MethodsBuilder) {
    /// DirectoryExpander.expand (DirectoryExpander.java): a tree artifact
    /// input's files, else the file alone.
    fn expand<'v>(
        this: &DirectoryExpander,
        #[starlark(require = pos)] file: starlark::values::Value<'v>,
        heap: starlark::values::Heap<'v>,
    ) -> anyhow::Result<starlark::values::Value<'v>> {
        let Some(found) = File::from_value(file) else {
            anyhow::bail!(
                "in call to expand(), parameter 'file' got value of type '{}', want 'File'",
                file.get_type()
            );
        };
        if !found.is_directory {
            return Ok(heap.alloc(vec![file]));
        }
        let Some(listings) = &this.listings else {
            anyhow::bail!(
                "DirectoryExpander.expand({}) while map_each maps a non-directory value: the native profile maps those values at analysis time, where directories cannot be expanded (docs/ACTIONS_PARAMS.md)",
                file.to_repr()
            );
        };
        let Some(children) = listings.get(&found.path) else {
            // FullExpander.list (:1106-1117).
            anyhow::bail!(
                "Failed to expand directory {}. Only directories that are action inputs can be expanded.",
                file.to_repr()
            );
        };
        Ok(heap.alloc(
            children
                .iter()
                .map(|child| crate::starlark_types::alloc_file(heap, child.clone()))
                .collect::<Vec<_>>(),
        ))
    }
}

/// One Args mutation (`recordArgStart`): add, add_all or add_joined.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Group {
    Strings(Vec<String>),
    Deferred(Arc<DeferredVector>),
}

/// One CommandLines part: a run of literal strings, or one Args object.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Part {
    pub groups: Vec<Group>,
    pub flag_per_line: bool,
    pub param: Option<ParamFileInfo>,
}

/// Tree file listing of a tree artifact (relative paths, byte order).
pub(crate) type TreeLookup<'a> = &'a dyn Fn(&File) -> Result<Vec<PathBuf>, String>;

/// SingleStringArgFormatter.format (exactly one `%s`, `%%` a literal `%`).
pub(crate) fn format(template: Option<&str>, value: &str) -> Result<String, String> {
    let Some(template) = template else {
        return Ok(value.to_owned());
    };
    let mut result = String::new();
    let mut placeholders = 0;
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            result.push(ch);
        } else {
            match chars.next() {
                Some('%') => result.push('%'),
                Some('s') => {
                    placeholders += 1;
                    result.push_str(value);
                }
                _ => {
                    return Err(
                        "Args format requires exactly one %s placeholder; only %s and %% are supported"
                            .into(),
                    );
                }
            }
        }
    }
    if placeholders != 1 {
        return Err("Args format requires exactly one %s placeholder".into());
    }
    Ok(result)
}

/// The arguments of one add_all/add_joined call from its (mapped) values.
pub(crate) fn assemble(shape: &VectorShape, values: Vec<String>) -> Result<Vec<String>, String> {
    let values = if shape.uniquify {
        let mut seen = std::collections::BTreeSet::new();
        values
            .into_iter()
            .filter(|value| seen.insert(value.clone()))
            .collect()
    } else {
        values
    };
    let omitted = shape.omit_if_empty && values.is_empty();
    let mut result = Vec::new();
    if omitted {
        return Ok(result);
    }
    result.extend(shape.arg_name.clone());
    if let Some(join_with) = &shape.join_with {
        let formatted = values
            .iter()
            .map(|value| format(shape.format_each.as_deref(), value))
            .collect::<Result<Vec<_>, _>>()?;
        result.push(format(
            shape.format_joined.as_deref(),
            &formatted.join(join_with),
        )?);
    } else {
        for value in &values {
            result.extend(shape.before_each.clone());
            result.push(format(shape.format_each.as_deref(), value)?);
        }
    }
    result.extend(shape.terminate_with.clone());
    Ok(result)
}

/// The File of one tree child (TreeFileArtifact): below the tree's paths.
pub(crate) fn tree_child(tree: &File, relative: &std::path::Path) -> File {
    let relative = relative.to_string_lossy();
    File::new(crate::starlark_types::FileData {
        is_directory: false,
        is_symlink: false,
        path: format!("{}/{relative}", tree.path),
        short_path: format!("{}/{relative}", tree.short_path),
        owner: tree.owner.clone(),
        is_source: false,
    })
}

/// Call a deferred map_each with one value (a tree child, or an unexpanded
/// tree), and a DirectoryExpander over `listings` when it takes one
/// (StarlarkCustomCommandLine.applyMapEach, :1125-1183).
fn call_mapper(
    mapper: &Mapper,
    file: File,
    listings: Option<&Arc<Listings>>,
) -> Result<Vec<String>, String> {
    let mapper = mapper.function()?;
    Module::with_temp_heap(|module| {
        let heap = module.heap();
        let function = heap.access_owned_frozen_value(mapper);
        let mut eval = Evaluator::new(&module);
        let mut arguments = vec![crate::starlark_types::alloc_file(heap, file)];
        if let Some(listings) = listings {
            arguments.push(heap.alloc(DirectoryExpander {
                listings: Some(Arc::clone(listings)),
            }));
        }
        // Its `print`s show nothing (crate::debug_print::silenced).
        let mapped = crate::debug_print::silenced(|| eval.eval_function(function, &arguments, &[]))
            .map_err(|e| format!("map_each failed: {e}"))?;
        mapped_strings(mapped)
    })
}

/// map_each's result: None, a string, or a list of strings.
pub(crate) fn mapped_strings(mapped: starlark::values::Value<'_>) -> Result<Vec<String>, String> {
    if mapped.is_none() {
        Ok(Vec::new())
    } else if let Some(text) = mapped.unpack_str() {
        Ok(vec![text.to_owned()])
    } else if let Some(list) = ListRef::from_value(mapped) {
        list.iter()
            .map(|value| {
                value.unpack_str().map(str::to_owned).ok_or_else(|| {
                    "map_each must return None, a string, or a list of strings".to_owned()
                })
            })
            .collect()
    } else {
        Err("map_each must return None, a string, or a list of strings".into())
    }
}

fn expand_deferred(vector: &DeferredVector, trees: TreeLookup<'_>) -> Result<Vec<String>, String> {
    // The DirectoryExpander of a map_each that takes one lists the vector's
    // tree artifacts (FullExpander over the action's input metadata).
    let listings = if vector.expander {
        let mut listings = Listings::new();
        for item in &vector.items {
            // A tree the lookup cannot list (no action input) stays unlisted:
            // expanding it is FullExpander's error, ignoring it is fine.
            if let Item::Directory(tree) | Item::Unexpanded(tree) = item
                && !listings.contains_key(&tree.path)
                && let Ok(relatives) = trees(tree)
            {
                let children = relatives
                    .iter()
                    .map(|relative| tree_child(tree, relative))
                    .collect();
                listings.insert(tree.path.clone(), children);
            }
        }
        Some(Arc::new(listings))
    } else {
        None
    };
    let mut values = Vec::new();
    for item in &vector.items {
        match item {
            Item::Strings(strings) => values.extend(strings.iter().cloned()),
            Item::Directory(tree) => {
                for relative in trees(tree)? {
                    let child = tree_child(tree, &relative);
                    match &vector.map_each {
                        Some(mapper) => {
                            values.extend(call_mapper(mapper, child, listings.as_ref())?)
                        }
                        None => values.push(child.path.clone()),
                    }
                }
            }
            Item::Unexpanded(tree) => match &vector.map_each {
                Some(mapper) => {
                    values.extend(call_mapper(mapper, tree.clone(), listings.as_ref())?)
                }
                None => values.push(tree.path.clone()),
            },
        }
    }
    assemble(&vector.shape, values)
}

/// Expand the groups of one Args object, applying flag_per_line grouping:
/// a group of two or more arguments becomes `first=rest joined by spaces`
/// (`rest` alone when `first` is empty).
pub(crate) fn expand_groups(
    groups: &[Group],
    flag_per_line: bool,
    trees: Option<TreeLookup<'_>>,
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for group in groups {
        let strings = match group {
            Group::Strings(strings) => strings.clone(),
            Group::Deferred(vector) => match trees {
                Some(trees) => expand_deferred(vector, trees)?,
                // Without input metadata Bazel does not expand directories
                // (maybeExpandDirectories); debug output shows their paths.
                None => {
                    let mut values = Vec::new();
                    for item in &vector.items {
                        match item {
                            Item::Strings(strings) => values.extend(strings.iter().cloned()),
                            Item::Directory(tree) | Item::Unexpanded(tree) => {
                                values.push(tree.path.clone())
                            }
                        }
                    }
                    assemble(&vector.shape, values)?
                }
            },
        };
        if flag_per_line && strings.len() >= 2 {
            let first = &strings[0];
            let rest = strings[1..].join(" ");
            result.push(if first.is_empty() {
                rest
            } else {
                format!("{first}={rest}")
            });
        } else {
            result.extend(strings);
        }
    }
    Ok(result)
}

pub(crate) fn has_deferred(groups: &[Group]) -> bool {
    groups
        .iter()
        .any(|group| matches!(group, Group::Deferred(_)))
}

/// Whether a tree-artifact expansion of `groups` calls a Starlark `map_each`
/// at execution time (an analysis-snapshot taint, PLAN-L2 §1b).
pub(crate) fn has_deferred_map_each(groups: &[Group]) -> bool {
    groups
        .iter()
        .any(|group| matches!(group, Group::Deferred(vector) if vector.map_each.is_some()))
}

/// Every deferred `map_each` of `groups`, in order.
pub(crate) fn deferred_mappers<'a>(groups: &'a [Group]) -> impl Iterator<Item = &'a Mapper> + 'a {
    groups.iter().filter_map(|group| match group {
        Group::Deferred(vector) => vector.map_each.as_ref(),
        Group::Strings(_) => None,
    })
}

/// ShellEscaper (ShellEscaper.java:57-110): only ASCII letters/digits and
/// `@%-_+:,./` are safe; `~` is safe except at the start.
pub(crate) fn shell_quote_required(text: &str) -> bool {
    text.is_empty()
        || text.starts_with('~')
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%-_+:,./~".contains(&byte))
}

/// Java String.length() of an internal string: its internal byte count.
fn java_length(text: &str) -> usize {
    crate::bazel_bytes::internal_bytes(text).len()
}

/// ParameterFile.writeParameterFile: each (escaped) argument's internal
/// bytes followed by LF.
pub(crate) fn param_file_bytes(arguments: &[String], file_type: ParamFileType) -> Vec<u8> {
    let mut output = Vec::new();
    for text in arguments {
        let bytes = crate::bazel_bytes::internal_bytes(text);
        if file_type == ParamFileType::Shell && shell_quote_required(text) {
            output.push(b'\'');
            for &byte in bytes.iter() {
                if byte == b'\'' {
                    output.extend_from_slice(b"'\\''");
                } else {
                    output.push(byte);
                }
            }
            output.push(b'\'');
        } else {
            output.extend_from_slice(&bytes);
        }
        output.push(b'\n');
    }
    // Kept with the planned action for the rest of the build (MEM4).
    output.shrink_to_fit();
    output
}

/// The command line of a Starlark spawn action.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommandLineSpec {
    /// Bazel's executable part (`[exec path]` or `[sh, -c, command, ""?]`),
    /// used only for the length threshold.
    pub executable: Vec<String>,
    pub parts: Vec<Part>,
    /// The primary (first) output's execution path.
    pub primary_output: String,
    pub max_length: usize,
}

/// Arguments after the executable part, and the parameter files.
pub(crate) type Expanded = (Vec<String>, Vec<(PathBuf, Vec<u8>)>);

impl CommandLineSpec {
    pub(crate) fn deferred(&self) -> bool {
        self.parts.iter().any(|part| has_deferred(&part.groups))
    }

    /// [`has_deferred_map_each`] over every part.
    pub(crate) fn deferred_map_each(&self) -> bool {
        self.parts
            .iter()
            .any(|part| has_deferred_map_each(&part.groups))
    }

    /// [`deferred_mappers`] over every part.
    pub(crate) fn deferred_mappers(&self) -> impl Iterator<Item = &Mapper> + '_ {
        self.parts
            .iter()
            .flat_map(|part| deferred_mappers(&part.groups))
    }

    /// CommandLines.expand (actions/CommandLines.java:89-153).
    pub(crate) fn expand(&self, trees: Option<TreeLookup<'_>>) -> Result<Expanded, String> {
        let parts = self.parts.len() + 1;
        let conservative = self.max_length as i64 - (parts * PARAM_FILE_ARG_LENGTH_ESTIMATE) as i64;
        let total = |arguments: &[String]| -> i64 {
            arguments
                .iter()
                .map(|argument| java_length(argument) as i64 + 1)
                .sum()
        };
        let mut length = total(&self.executable);
        let mut arguments = Vec::new();
        let mut files = Vec::new();
        for part in &self.parts {
            let chunk = expand_groups(&part.groups, part.flag_per_line, trees)?;
            let Some(param) = &part.param else {
                length += total(&chunk);
                arguments.extend(chunk);
                continue;
            };
            if !param.always {
                let tentative = length + total(&chunk);
                if tentative <= conservative {
                    arguments.extend(chunk);
                    length = tentative;
                    continue;
                }
            }
            let path = format!("{}-{}.params", self.primary_output, files.len());
            let flag = format(Some(&param.flag_format), &path)?;
            length += java_length(&flag) as i64 + 1;
            arguments.push(flag);
            let content = if param.flags_only {
                let (flags, positional): (Vec<String>, Vec<String>) = chunk
                    .into_iter()
                    .partition(|argument| argument.starts_with("--"));
                for argument in positional {
                    length += java_length(&argument) as i64 + 1;
                    arguments.push(argument);
                }
                flags
            } else {
                chunk
            };
            files.push((
                PathBuf::from(path),
                param_file_bytes(&content, param.file_type),
            ));
        }
        Ok((arguments, files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(strings: &[&str], param: Option<ParamFileInfo>, flag_per_line: bool) -> Part {
        Part {
            groups: strings
                .iter()
                .map(|s| Group::Strings(vec![(*s).to_owned()]))
                .collect(),
            flag_per_line,
            param,
        }
    }

    fn info(always: bool, file_type: ParamFileType, flags_only: bool) -> ParamFileInfo {
        ParamFileInfo {
            flag_format: "@%s".into(),
            always,
            file_type,
            flags_only,
        }
    }

    #[test]
    fn param_files_follow_threshold_naming_and_formats() {
        let spec = CommandLineSpec {
            executable: vec!["tool".into()],
            parts: vec![
                part(
                    &["a b", "c"],
                    Some(info(true, ParamFileType::Shell, false)),
                    false,
                ),
                part(&["plain"], None, false),
                part(
                    &["x y"],
                    Some(info(true, ParamFileType::Unquoted, false)),
                    false,
                ),
                part(
                    &["small"],
                    Some(info(false, ParamFileType::Shell, false)),
                    false,
                ),
            ],
            primary_output: "bazel-out/k8-fastbuild/bin/pkg/out".into(),
            max_length: MIN_PARAM_FILE_SIZE,
        };
        let (arguments, files) = spec.expand(None).unwrap();
        assert_eq!(
            arguments,
            [
                "@bazel-out/k8-fastbuild/bin/pkg/out-0.params",
                "plain",
                "@bazel-out/k8-fastbuild/bin/pkg/out-1.params",
                "small"
            ]
        );
        assert_eq!(files[0].1, b"'a b'\nc\n");
        assert_eq!(files[1].1, b"x y\n");
        // Exactly at the conservative limit stays inline; one byte more spills.
        let limit = MIN_PARAM_FILE_SIZE - 2 * PARAM_FILE_ARG_LENGTH_ESTIMATE;
        for (size, spilled) in [(limit - 5 - 1, false), (limit - 5, true)] {
            let spec = CommandLineSpec {
                executable: vec!["tool".into()],
                parts: vec![Part {
                    groups: vec![Group::Strings(vec!["x".repeat(size)])],
                    flag_per_line: false,
                    param: Some(info(false, ParamFileType::Shell, false)),
                }],
                primary_output: "o".into(),
                max_length: MIN_PARAM_FILE_SIZE,
            };
            assert_eq!(!spec.expand(None).unwrap().1.is_empty(), spilled, "{size}");
        }
    }

    #[test]
    fn flag_per_line_groups_and_keeps_positionals_on_the_command_line() {
        let groups = vec![
            Group::Strings(vec!["--a".into(), "b".into(), "c".into()]),
            Group::Strings(vec!["pos".into()]),
            Group::Strings(vec![String::new(), "x".into()]),
            Group::Strings(vec!["--flag".into()]),
        ];
        assert_eq!(
            expand_groups(&groups, true, None).unwrap(),
            ["--a=b c", "pos", "x", "--flag"]
        );
        let spec = CommandLineSpec {
            executable: vec![],
            parts: vec![Part {
                groups,
                flag_per_line: true,
                param: Some(info(true, ParamFileType::Unquoted, true)),
            }],
            primary_output: "o".into(),
            max_length: MIN_PARAM_FILE_SIZE,
        };
        let (arguments, files) = spec.expand(None).unwrap();
        assert_eq!(arguments, ["@o-0.params", "pos", "x"]);
        assert_eq!(files[0].1, b"--a=b c\n--flag\n");
    }
}
