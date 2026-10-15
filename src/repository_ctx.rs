//! Bounded native repository callbacks: file/template and tracked getenv.
//! Paths use the native label component profile (ASCII alphanumeric, `._+-`).
//! The caller owns the private stage and publication/cleanup; this is not an OS
//! sandbox and assumes no concurrent hostile filesystem mutation of that stage.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{Context as _, anyhow, bail, ensure};
use serde::Serialize;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, OwnedFrozenValue, Value, ValueLike};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::custom_rule::AttrValue;
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::repository_environment::{Captured, EnvError, Watch, WatchedEnvironment};
use crate::repository_rule::RuleDefinition;
use crate::starlark_types::{alloc_label, unpack_label};

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPERATIONS: usize = 10_000;

#[derive(Debug, Default, Serialize)]
pub struct ExecutionInputs {
    /// Ordered reads, including repeated reads of a subsequently changed file.
    pub templates: Vec<TemplateInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watched_env: Option<WatchedEnvironment>,
}

#[derive(Debug, Serialize)]
pub struct TemplateInput {
    pub source: String,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct State {
    inputs: ExecutionInputs,
    written: usize,
    read: usize,
    writes: usize,
    environment: Watch,
}

// Marker/trace/freeze derives generate unsafe implementations. Runtime host
// state owns all its data; the Starlark attrs field is always traced/frozen.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

    #[derive(ProvidesStaticType)]
    pub(super) struct Context {
        pub repositories: Arc<RepositorySet>,
        pub stage: PathBuf,
        pub label: Label,
        pub state: RefCell<State>,
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct RepositoryCtxGen<V> {
        pub attrs: V,
        pub os: V,
        pub name: String,
        pub original_name: String,
    }

    impl<V> fmt::Display for RepositoryCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<repository_ctx {}>", self.name)
        }
    }

    starlark_complex_value!(pub(super) RepositoryCtx);

    #[starlark_value(type = "repository_ctx")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepositoryCtxGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_methods() -> Option<&'static Methods> {
            Some(REPOSITORY_METHODS.methods())
        }

        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "attr" => Some(self.attrs.to_value()),
                "os" => Some(self.os.to_value()),
                "name" => Some(heap.alloc(self.name.as_str())),
                "original_name" => Some(heap.alloc(self.original_name.as_str())),
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            ["attr", "name", "original_name", "os"]
                .map(str::to_owned)
                .to_vec()
        }
    }
}
use values::{Context, RepositoryCtxGen};

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow!("repository_ctx is only available during repository execution"))
}

/// True while evaluating a repository-rule callback (byte-string profile).
pub(crate) fn evaluating_callback(eval: &Evaluator<'_, '_, '_>) -> bool {
    context(eval).is_ok()
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    context(eval)
        .ok()
        .map(|context| context.repositories.clone())
}

pub(crate) fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<PackageId> {
    let context = context(eval).ok()?;
    Some(
        eval.call_stack_top_location()
            .and_then(|location| Label::parse_canonical(location.filename()).ok())
            .unwrap_or_else(|| context.label.clone())
            .package_id(),
    )
}

fn relative_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "repository path must contain 1..4096 bytes"
    );
    ensure!(
        path.split('/').count() <= 64,
        "repository path exceeds depth 64"
    );
    for component in path.split('/') {
        ensure!(
            !matches!(component, "" | "." | "..")
                && component
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+')),
            "unsupported repository path {path:?}: expected clean relative ASCII components"
        );
    }
    Ok(())
}

fn real_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "directory {} must be a real directory, not a symlink",
        path.display()
    );
    Ok(())
}

fn checked_root(root: &Path) -> anyhow::Result<PathBuf> {
    // Check ancestors as well as the final root before canonicalization.
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        real_dir(&current)?;
    }
    Ok(fs::canonicalize(root)?)
}

fn parents(root: &Path, relative: &str, create: bool) -> anyhow::Result<PathBuf> {
    relative_path(relative)?;
    real_dir(root)?;
    let mut path = root.to_path_buf();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        path.push(part);
        if parts.peek().is_some() {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    fs::create_dir(&path)?
                }
                Err(error) => return Err(error.into()),
                Ok(metadata) => ensure!(
                    metadata.file_type().is_dir(),
                    "parent {} must be a real directory, not a symlink",
                    path.display()
                ),
            }
        }
    }
    Ok(path)
}

fn regular_file(path: &Path) -> anyhow::Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "{} must be a regular file, not a symlink",
        path.display()
    );
    Ok(())
}

impl Context {
    fn write(&self, path: &str, bytes: &[u8], executable: bool) -> anyhow::Result<()> {
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "repository file exceeds 16 MiB limit"
        );
        let mut state = self.state.borrow_mut();
        ensure!(
            state.written + bytes.len() <= MAX_TOTAL_BYTES,
            "repository cumulative writes exceed 64 MiB limit"
        );
        ensure!(
            state.writes < MAX_OPERATIONS,
            "repository exceeds 10000 writes"
        );
        let path = parents(&self.stage, path, true)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "output {} must be a regular file, not a symlink",
                    path.display()
                );
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(if executable {
                0o755
            } else {
                0o644
            }))?;
        }
        #[cfg(not(unix))]
        let _ = executable;
        state.written += bytes.len();
        state.writes += 1;
        Ok(())
    }

    fn read_template(&self, template: Value<'_>) -> anyhow::Result<Vec<u8>> {
        let (source, path) = if let Some(canonical) = unpack_label(template) {
            let label = Label::parse_canonical(&canonical).map_err(anyhow::Error::msg)?;
            let root = checked_root(
                self.repositories
                    .root(&label.repository)
                    .map_err(anyhow::Error::msg)?,
            )?;
            let package = if label.package.is_empty() {
                root.clone()
            } else {
                let path = parents(&root, &label.package, false)?;
                real_dir(&path)?;
                path
            };
            let mut marker = false;
            for name in ["BUILD.bazel", "BUILD"] {
                match fs::symlink_metadata(package.join(name)) {
                    Ok(metadata) => {
                        ensure!(
                            metadata.file_type().is_file(),
                            "package BUILD marker must be a regular file, not a symlink"
                        );
                        marker = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            ensure!(
                marker,
                "template label {label} requires a package BUILD file"
            );
            (label.to_string(), parents(&package, &label.name, false)?)
        } else {
            let relative = template.unpack_str().ok_or_else(|| anyhow!("template must be a Label or a stage-relative string; path objects are unsupported"))?;
            (
                format!("stage:{relative}"),
                parents(&self.stage, relative, false)?,
            )
        };
        regular_file(&path)?;
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "template input exceeds 16 MiB limit"
        );
        let mut state = self.state.borrow_mut();
        ensure!(
            state.read + bytes.len() <= MAX_TOTAL_BYTES,
            "template cumulative reads exceed 64 MiB limit"
        );
        ensure!(
            state.inputs.templates.len() < MAX_OPERATIONS,
            "repository exceeds 10000 template reads"
        );
        state.read += bytes.len();
        state.inputs.templates.push(TemplateInput {
            source,
            bytes: bytes.clone(),
        });
        Ok(bytes)
    }
}

fn replace_bytes(input: &[u8], key: &[u8], replacement: &[u8]) -> anyhow::Result<Vec<u8>> {
    if key.is_empty() {
        return Ok(input.to_vec());
    }
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let bytes = if input[offset..].starts_with(key) {
            offset += key.len();
            replacement
        } else {
            offset += 1;
            &input[offset - 1..offset]
        };
        ensure!(
            output.len() + bytes.len() <= MAX_FILE_BYTES,
            "template output exceeds 16 MiB limit"
        );
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

starlark::methods_static!(REPOSITORY_METHODS = repository_methods);

/// A private, value-free marker carried intact by ErrorKind::Native. Only this
/// method's failures suppress source diagnostics; user messages cannot opt in.
#[derive(Debug)]
enum GetenvError {
    Receiver,
    Phase,
    Arguments,
    Name,
    Default,
    Environment(EnvError),
}

impl fmt::Display for GetenvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Receiver => f.write_str("getenv requires a repository_ctx receiver"),
            Self::Phase => f.write_str("repository_ctx is only available during repository execution"),
            Self::Arguments => f.write_str("repository getenv requires one or two positional arguments and no keyword arguments"),
            Self::Name => f.write_str("repository getenv name must be a string"),
            Self::Default => f.write_str("repository getenv default must be a string or None"),
            Self::Environment(error) => fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for GetenvError {}
impl From<EnvError> for GetenvError {
    fn from(error: EnvError) -> Self {
        Self::Environment(error)
    }
}

fn getenv_impl<'v>(
    this: Value<'v>,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<Value<'v>, GetenvError> {
    // Validate the genuine receiver and phase before inspecting user arguments.
    if values::RepositoryCtx::from_value(this).is_none() {
        return Err(GetenvError::Receiver);
    }
    let context = context(eval).map_err(|_| GetenvError::Phase)?;
    // Discard binder errors without formatting or retaining their payloads.
    args.no_named_args().map_err(|_| GetenvError::Arguments)?;
    let mut positions = args
        .positions(eval.heap())
        .map_err(|_| GetenvError::Arguments)?;
    let name = positions.next().ok_or(GetenvError::Arguments)?;
    let default = positions.next().unwrap_or_else(Value::new_none);
    if positions.next().is_some() {
        return Err(GetenvError::Arguments);
    }
    let name = name.unpack_str().ok_or(GetenvError::Name)?;
    if !default.is_none() && default.unpack_str().is_none() {
        return Err(GetenvError::Default);
    }
    let entry = context
        .state
        .borrow_mut()
        .environment
        .observe(context.repositories.environment(), name)?;
    match entry.captured()? {
        Captured::Absent => Ok(default),
        Captured::Value(value) => {
            context
                .state
                .borrow_mut()
                .environment
                .charge_return(value)?;
            Ok(eval.heap().alloc(value.as_ref()))
        }
    }
}

fn callback_error(error: starlark::Error) -> anyhow::Error {
    if let starlark::ErrorKind::Native(native) = error.kind()
        && let Some(private) = native.downcast_ref::<GetenvError>()
    {
        // Render the bounded marker itself, not arbitrary attached context or
        // the diagnostic's source/stack, which may contain rejected arguments.
        return anyhow!(private.to_string());
    }
    anyhow!(error.to_string())
}

#[starlark_module]
fn repository_methods(builder: &mut MethodsBuilder) {
    fn getenv<'v>(
        this: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        // Raw Arguments bypass generated binding: keyword/arity failures must
        // enter the same typed privacy boundary as name/default/read failures.
        getenv_impl(this, args, eval).map_err(anyhow::Error::new)
    }

    fn file<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(default = false)] legacy_utf8: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, legacy_utf8);
        // Bazel writes its internal Latin-1 string bytes; characters above
        // U+00FF are not representable there and are rejected explicitly.
        let bytes = crate::repository_rule::latin1_bytes(content).ok_or_else(|| {
            anyhow!("repository file content above U+00FF is unsupported (Latin-1 byte strings)")
        })?;
        context(eval)?.write(path, &bytes, executable)?;
        Ok(NoneType)
    }

    fn template<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] template: Value<'v>,
        substitutions: Option<Value<'v>>,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = "auto")] watch_template: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        relative_path(path)?;
        ensure!(
            watch_template == "auto",
            "watch_template modes other than 'auto' are unsupported"
        );
        let mut replacements = Vec::new();
        if let Some(substitutions) = substitutions {
            let dict = DictRef::from_value(substitutions)
                .ok_or_else(|| anyhow!("substitutions must be a string-to-string dict"))?;
            for (key, value) in dict.iter() {
                let key = key
                    .unpack_str()
                    .ok_or_else(|| anyhow!("substitution keys must be ASCII strings"))?;
                let value = value
                    .unpack_str()
                    .ok_or_else(|| anyhow!("substitution values must be ASCII strings"))?;
                ensure!(
                    key.is_ascii() && value.is_ascii(),
                    "template substitutions must be ASCII strings"
                );
                replacements.push((key, value));
            }
        }
        let context = context(eval)?;
        let mut bytes = context.read_template(template)?;
        for (key, value) in replacements {
            bytes = replace_bytes(&bytes, key.as_bytes(), value.as_bytes())?;
        }
        context.write(path, &bytes, executable)?;
        Ok(NoneType)
    }
}

fn latin1(text: &str) -> bool {
    crate::repository_rule::is_latin1(text)
}

fn alloc_attrs<'v>(
    heap: Heap<'v>,
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
) -> anyhow::Result<Value<'v>> {
    let mut values = BTreeMap::new();
    for (name, attr) in attrs {
        let value = match attr {
            AttrValue::String(value) => {
                ensure!(
                    latin1(value),
                    "repository string attributes must be Latin-1 (U+0000..U+00FF)"
                );
                heap.alloc(value.as_str())
            }
            AttrValue::Strings(value) => {
                ensure!(
                    value.iter().all(|value| latin1(value)),
                    "repository string attributes must be Latin-1 (U+0000..U+00FF)"
                );
                heap.alloc(value.clone())
            }
            AttrValue::Int(value) => heap.alloc(*value),
            AttrValue::Ints(_) | AttrValue::LabelKeyedStringDict(_) => {
                anyhow::bail!(
                    "attribute {name:?} kind is not supported for repository rules by the native profile"
                )
            }
            AttrValue::StringKeyedLabelDict(entries) => {
                crate::attribute_values::alloc_string_keyed_label_dict(heap, entries)?
            }
            AttrValue::LabelListDict(entries) => {
                crate::attribute_values::alloc_label_dict(heap, entries, true)?
            }
            AttrValue::StringDict(entries) => {
                ensure!(
                    entries
                        .iter()
                        .all(|(key, value)| latin1(key) && latin1(value)),
                    "repository string attributes must be Latin-1 (U+0000..U+00FF)"
                );
                crate::attribute_values::alloc_string_dict(heap, entries)?
            }
            AttrValue::StringListDict(entries) => {
                ensure!(
                    entries
                        .iter()
                        .all(|(key, values)| latin1(key) && values.iter().all(|v| latin1(v))),
                    "repository string attributes must be Latin-1 (U+0000..U+00FF)"
                );
                crate::attribute_values::alloc_string_list_dict(heap, entries)?
            }
            AttrValue::Bool(value) => heap.alloc(*value),
            AttrValue::Label(None) => Value::new_none(),
            AttrValue::Label(Some(label)) => alloc_label(heap, label)?,
            AttrValue::Labels(labels) => heap.alloc(
                labels
                    .iter()
                    .map(|label| alloc_label(heap, label))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
            AttrValue::Output(_) | AttrValue::Outputs(_) => {
                bail!("output attributes are unsupported for repository rules")
            }
        };
        values.insert(name.clone(), value);
    }
    values.insert("name".to_owned(), heap.alloc(canonical_name));
    Ok(heap.alloc(AllocStruct(values)))
}

/// Execute only into the caller's precreated private stage. Failures may leave
/// partial files there; callers must not publish them. Frozen owners remain live
/// throughout evaluation, including transitive/private helper functions.
pub fn execute(
    repositories: Arc<RepositorySet>,
    definition: &RuleDefinition,
    functions: &[OwnedFrozenValue],
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
    original_name: &str,
    definition_label: &Label,
    stage: &Path,
) -> Result<ExecutionInputs, String> {
    let run = || -> anyhow::Result<ExecutionInputs> {
        ensure!(
            canonical_name.is_ascii() && original_name.is_ascii(),
            "repository names must be ASCII"
        );
        let context = Context {
            repositories,
            stage: checked_root(stage)?,
            label: definition_label.clone(),
            state: RefCell::new(State::default()),
        };
        Module::with_temp_heap(|module| -> anyhow::Result<()> {
            let heap = module.heap();
            let ctx = heap.alloc(RepositoryCtxGen {
                attrs: alloc_attrs(heap, attrs, canonical_name)?,
                os: crate::host_os::alloc(
                    heap,
                    context.repositories.host().map_err(anyhow::Error::msg)?,
                )?,
                name: canonical_name.to_owned(),
                original_name: original_name.to_owned(),
            });
            for function in functions {
                heap.access_owned_frozen_value(function);
            }
            let implementation = heap.access_owned_frozen_value(&definition.implementation);
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)?;
            eval.set_max_tick_count(1_000_000)?;
            eval.set_max_heap_size(128 * 1024 * 1024)?;
            eval.extra = Some(&context);
            for name in definition.environ.names() {
                context
                    .state
                    .borrow_mut()
                    .environment
                    .observe(context.repositories.environment(), name)?;
            }
            let returned = eval
                .eval_function(implementation, &[ctx], &[])
                .map_err(callback_error)?;
            ensure!(
                returned.is_none(),
                "repository implementation return values other than None are unsupported"
            );
            Ok(())
        })?;
        let mut state = context.state.into_inner();
        state.inputs.watched_env = state.environment.inputs()?;
        Ok(state.inputs)
    };
    run().map_err(|error| {
        format!(
            "repository {canonical_name} ({}): {error:#}",
            definition.identity
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loading::{LoadedDefinition, load_definition_in};
    use crate::repository_rule::{bind_attributes, unpack};

    struct Fixture {
        root: tempfile::TempDir,
        stage: tempfile::TempDir,
        repositories: Arc<RepositorySet>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir_in("/tmp/opencode").unwrap();
            let stage = tempfile::tempdir_in("/tmp/opencode").unwrap();
            fs::write(root.path().join("BUILD"), "fail('never evaluate BUILD')\n").unwrap();
            fs::write(root.path().join("MODULE.bazel"), "module(name='root')\n").unwrap();
            let repositories = Arc::new(
                RepositorySet::new(root.path().to_owned())
                    .with_source_cache(root.path().join("cache")),
            );
            Self {
                root,
                stage,
                repositories,
            }
        }

        fn file(&self, name: &str, bytes: impl AsRef<[u8]>) {
            let path = self.root.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn load(&self, body: &str, schema: &str) -> LoadedDefinition {
            let body = body
                .lines()
                .map(|line| format!("    {line}\n"))
                .collect::<String>();
            self.file("defs.bzl", format!("def _impl(ctx):\n{body}\nR = repository_rule(implementation=_impl, attrs={schema})\n"));
            load_definition_in(
                self.repositories.clone(),
                &Label::parse_canonical("//:defs.bzl").unwrap(),
                "R",
                &crate::package::bzl_globals(),
            )
            .unwrap()
        }

        fn run_loaded(
            &self,
            loaded: LoadedDefinition,
            raw: BTreeMap<String, serde_json::Value>,
        ) -> Result<ExecutionInputs, String> {
            let definition = unpack(&loaded.value).unwrap();
            let attrs = bind_attributes(&definition, &raw, "canonical+repo", |text| {
                self.repositories.parse(text, "", "")
            })
            .unwrap();
            // Drop the loader export and source provenance before invoking. The
            // extracted implementation and functions own every frozen value.
            let functions = loaded.functions;
            drop(loaded.value);
            drop(loaded.sources);
            execute(
                self.repositories.clone(),
                &definition,
                &functions,
                &attrs,
                "canonical+repo",
                "original",
                &Label::parse_canonical("//:defs.bzl").unwrap(),
                self.stage.path(),
            )
        }

        fn run(&self, body: &str) -> Result<ExecutionInputs, String> {
            self.run_loaded(self.load(body, "{}"), BTreeMap::new())
        }

        fn error(&self, body: &str, expected: &str) {
            let error = self.run(body).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }

        fn bytes(&self, name: &str) -> Vec<u8> {
            fs::read(self.stage.path().join(name)).unwrap()
        }
    }

    #[test]
    fn empty_execution_inputs_keep_exact_legacy_bytes() {
        assert_eq!(
            serde_json::to_string(&ExecutionInputs::default()).unwrap(),
            r#"{"templates":[]}"#
        );
    }

    #[test]
    fn string_dict_repository_projection_drives_ordered_template_and_is_frozen() {
        let fixture = Fixture::new();
        let schema = "{'d':attr.string_dict(default={'AB':'Z', 'Z':'Q'})}";
        let loaded = fixture.load("(ctx.attr.d.keys() == ['AB','Z'] and type(ctx.attr.d['AB']) == 'string') or fail('projection')\nctx.file('in', 'AB')\nctx.template('out', 'in', ctx.attr.d)", schema);
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("out"), b"Q");
        let loaded = fixture.load("ctx.attr.d['AB']='changed'", schema);
        let error = fixture
            .run_loaded(loaded, BTreeMap::new())
            .unwrap_err()
            .to_lowercase();
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
        Module::with_temp_heap(|module| {
            for (key, value) in [("é", "ok"), ("k", "é")] {
                let attrs = BTreeMap::from([(
                    "d".into(),
                    AttrValue::StringDict(vec![(key.into(), value.into())]),
                )]);
                alloc_attrs(module.heap(), &attrs, "repo").unwrap();
            }
            for (key, value) in [("\u{100}", "ok"), ("k", "\u{100}")] {
                let attrs = BTreeMap::from([(
                    "d".into(),
                    AttrValue::StringDict(vec![(key.into(), value.into())]),
                )]);
                assert!(
                    alloc_attrs(module.heap(), &attrs, "repo")
                        .unwrap_err()
                        .to_string()
                        .contains("Latin-1")
                );
            }
        });
    }

    #[test]
    fn host_metadata_on_actual_repository_context_is_stable_without_environment_reads() {
        use crate::repository_environment::tests::Reader;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        let inputs = fixture.run("host = ctx.os\n(host == ctx.os and {host:42}[ctx.os] == 42) or fail('identity')\n(type(host) == 'repository_os' and host.name == 'linux' and host.arch == 'amd64') or fail('metadata')\n(dir(host) == ['arch', 'environ', 'name'] and hasattr(host, 'environ')) or fail('surface')\nctx.file('metadata', json.encode([host.name, host.arch]))").unwrap();
        assert_eq!(fixture.bytes("metadata"), br#"["linux","amd64"]"#);
        assert!(inputs.watched_env.is_none());
        assert_eq!(reader.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        for body in ["ctx.os.environ", "getattr(ctx.os, 'environ', {})"] {
            fixture.error(body, "repository_os.environ is unsupported");
        }
        fixture.error(
            "json.encode(ctx.os)",
            "cannot encode unsupported value as JSON",
        );
        fixture.error("ctx.os.name = 'changed'", "repository_os");
    }

    #[test]
    fn frozen_repository_context_retains_its_single_host_value_after_owner_drop() {
        let owner = Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set(
                "ctx",
                heap.alloc(RepositoryCtxGen {
                    attrs: Value::new_none(),
                    os: crate::host_os::alloc(heap, RepositorySet::new(".".into()).host().unwrap())
                        .unwrap(),
                    name: "repo".into(),
                    original_name: "original".into(),
                }),
            );
            module.freeze().unwrap()
        });
        let ctx = owner.get("ctx").unwrap();
        drop(owner);
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let value = heap.access_owned_frozen_value(&ctx);
            let first = value.get_attr("os", heap).unwrap().unwrap();
            let second = value.get_attr("os", heap).unwrap().unwrap();
            assert!(first.ptr_eq(second));
            assert_eq!(
                first.get_attr("name", heap).unwrap().unwrap().unpack_str(),
                Some("linux")
            );
            assert_eq!(
                first.get_attr("arch", heap).unwrap().unwrap().unpack_str(),
                Some("amd64")
            );
        });
    }

    #[test]
    fn getenv_present_empty_absent_defaults_and_template_flow() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        for value in [Some("SyntheticSecret"), Some(""), None] {
            let mut fixture = Fixture::new();
            let reader = Reader::new(Ok(value.map(Into::into)));
            fixture.repositories = Arc::new(
                (*fixture.repositories)
                    .clone()
                    .with_environment_reader(reader.clone()),
            );
            let inputs = fixture.run("v = ctx.getenv('SYNTHETIC', 'first')\nctx.file('value', v)\nctx.file('template', '@VALUE@')\nctx.template('expanded', 'template', {'@VALUE@': ctx.getenv('SYNTHETIC', 'second')})").unwrap();
            assert_eq!(fixture.bytes("value"), value.unwrap_or("first").as_bytes());
            assert_eq!(
                fixture.bytes("expanded"),
                value.unwrap_or("second").as_bytes()
            );
            assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
            let metadata = serde_json::to_string(&inputs).unwrap();
            assert!(metadata.contains("watched_env"));
            assert!(!metadata.contains("SyntheticSecret"));
            assert!(!format!("{inputs:?}").contains("SyntheticSecret"));
        }
    }

    #[test]
    fn getenv_validates_bindings_before_reader_and_never_echoes_rejected_values() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        for body in [
            "ctx.getenv(['RejectedSecret'])",
            "ctx.getenv('SYNTHETIC', ['RejectedSecret'])",
            "ctx.getenv('SYNTHETIC', default='RejectedSecret')",
            "ctx.getenv(name='RejectedSecret')",
            "ctx.getenv('SYNTHETIC', None, 'RejectedSecret')",
            "ctx.getenv('SYNTHETIC', RejectedSecret='RejectedSecret')",
            "ctx.getenv('SYNTHETIC', **{'RejectedSecret': 'RejectedSecret'})",
            "ctx.getenv('SYNTHETIC', **['RejectedSecret'])",
            "ctx.getenv(*['SYNTHETIC', None, 'RejectedSecret'])",
            "ctx.getenv(*struct(RejectedSecret='RejectedSecret'))",
            "ctx.getenv('SYNTHETIC', **{1: 'RejectedSecret'})",
            "ctx.getenv(name='RejectedSecret', **{'name': 'RejectedSecret'})",
            "ctx.getenv()",
        ] {
            let error = fixture.run(body).unwrap_err();
            assert!(!error.contains("RejectedSecret"), "{error}");
            assert!(!error.contains("SyntheticSecret"), "{error}");
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn non_environment_callback_errors_preserve_source_even_after_getenv_or_spoofed_messages() {
        use crate::repository_environment::tests::Reader;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(None));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader),
        );
        for body in [
            "native.package_group(name='bad')",
            "ctx.getenv('SYNTHETIC')\nnative.package_group(name='bad')",
        ] {
            let error = fixture.run(body).unwrap_err();
            assert!(
                error.contains("native.package_group(name='bad')"),
                "{error}"
            );
            assert!(
                error.contains("rule declaration outside package evaluation"),
                "{error}"
            );
            assert!(error.contains("Traceback"), "{error}");
        }
        let error = fixture
            .run("def getenv():\n    fail('repository getenv name must be a string')\ngetenv()")
            .unwrap_err();
        assert!(
            error.contains("fail('repository getenv name must be a string')"),
            "{error}"
        );
        assert!(error.contains("Traceback"), "{error}");
    }

    #[test]
    fn getenv_private_marker_is_typed_and_drops_arbitrary_attached_context() {
        let marked = starlark::Error::new_native(
            anyhow::Error::new(GetenvError::Name).context("RejectedSecret"),
        );
        let error = callback_error(marked).to_string();
        assert_eq!(error, "repository getenv name must be a string");
        let unmarked = starlark::Error::new_native(anyhow!(
            "repository getenv name must be a string: ordinary error"
        ));
        assert_eq!(
            callback_error(unmarked).to_string(),
            "repository getenv name must be a string: ordinary error"
        );
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            assert!(matches!(
                getenv_impl(
                    module.heap().alloc("RejectedSecret"),
                    &Arguments::default(),
                    &mut eval
                ),
                Err(GetenvError::Receiver)
            ));
        });
    }

    #[test]
    fn getenv_wrong_phase_rejects_before_lookup() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        let fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        Module::with_temp_heap(|module| {
            let ctx = module.heap().alloc(RepositoryCtxGen {
                attrs: Value::new_none(),
                os: crate::host_os::alloc(module.heap(), repositories.host().unwrap()).unwrap(),
                name: "repo".into(),
                original_name: "repo".into(),
            });
            let method = ctx.get_attr("getenv", module.heap()).unwrap().unwrap();
            let loading = crate::loading::ExtensionContext {
                byte_strings: false,
                repositories,
                label: Label::parse_canonical("//:defs.bzl").unwrap(),
            };
            let mut eval = Evaluator::new(&module);
            let error = eval
                .eval_function(method, &[module.heap().alloc("SYNTHETIC")], &[])
                .unwrap_err();
            assert!(
                error
                    .without_diagnostic()
                    .to_string()
                    .contains("only available during repository execution")
            );
            assert!(
                matches!(error.kind(), starlark::ErrorKind::Native(native) if matches!(native.downcast_ref::<GetenvError>(), Some(GetenvError::Phase)))
            );
            // A loading context carries repositories too, but is not an execution capability.
            eval.extra = Some(&loading);
            assert!(
                eval.eval_function(method, &[module.heap().alloc("SYNTHETIC")], &[])
                    .is_err()
            );
        });
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn getenv_error_never_defaults_and_is_cached_between_callbacks() {
        use crate::repository_environment::{ReadError, tests::Reader};
        use std::sync::atomic::Ordering;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Err(ReadError));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        for _ in 0..2 {
            let error = fixture
                .run("ctx.file('value', ctx.getenv('RejectedSecret', 'RejectedSecret'))")
                .unwrap_err();
            assert!(error.contains("repository environment read failed"));
            assert!(!error.contains("RejectedSecret"), "{error}");
            assert!(!fixture.stage.path().join("value").exists());
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn attributes_are_real_labels_and_names_are_canonical() {
        let fixture = Fixture::new();
        let loaded = fixture.load(
            "ctx.file('attrs', '|'.join([ctx.name, ctx.attr.name, ctx.original_name, type(ctx.attr.src), ctx.attr.src.name, type(ctx.attr.srcs[0]), str(ctx.attr.count), str(ctx.attr.flag), ctx.attr.words[0], str(ctx.attr.optional)]))",
            "{'src': attr.label(default='//:private.txt'), 'srcs': attr.label_list(default=['//:private.txt']), 'count': attr.int(default=7), 'flag': attr.bool(default=True), 'words': attr.string_list(default=['word']), 'optional': attr.label()}",
        );
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(
            fixture.bytes("attrs"),
            b"canonical+repo|canonical+repo|original|Label|private.txt|Label|7|True|word|None"
        );
    }

    #[test]
    fn file_defaults_overwrite_nested_exact_bytes_and_modes() {
        let fixture = Fixture::new();
        fixture.run("ctx.file('nested/empty')\nctx.file('nested/data', 'old')\nctx.file('nested/data', 'new\\000\\r\\n', False, True)\nctx.file('exec', 'x', legacy_utf8=True)").unwrap();
        assert!(fixture.bytes("nested/empty").is_empty());
        assert_eq!(fixture.bytes("nested/data"), b"new\0\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (name, mode) in [
                ("nested/empty", 0o755),
                ("nested/data", 0o644),
                ("exec", 0o755),
            ] {
                assert_eq!(
                    fs::metadata(fixture.stage.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    mode
                );
            }
        }
    }

    #[test]
    fn rejects_escapes_unclean_paths_and_non_string_output() {
        let fixture = Fixture::new();
        for path in [
            "../escape",
            "/absolute",
            "a/../escape",
            "a/./b",
            "a//b",
            "a/",
            "back\\slash",
            "a\nb",
            "",
        ] {
            let literal = serde_json::to_string(path).unwrap();
            assert!(
                fixture.run(&format!("ctx.file({literal}, 'x')")).is_err(),
                "accepted {path:?}"
            );
        }
        // Bazel supports octal escapes, not JSON's Unicode NUL escape. Reach
        // the path API with an actual NUL instead of failing source validation.
        fixture.error("ctx.file('nul\\000path', 'x')", "relative ASCII");
        fixture.error("ctx.file(Label('//:x'), 'x')", "str");
        fixture.error("ctx.file(path='x')", "path");
        assert_eq!(fs::read_dir(fixture.stage.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_output_symlinks_parents_final_and_root() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("outside", b"untouched");
        symlink(fixture.root.path(), fixture.stage.path().join("parent")).unwrap();
        symlink(
            fixture.root.path().join("outside"),
            fixture.stage.path().join("final"),
        )
        .unwrap();
        fixture.error("ctx.file('parent/outside', 'changed')", "symlink");
        fixture.error("ctx.file('final', 'changed')", "symlink");
        assert_eq!(
            fs::read(fixture.root.path().join("outside")).unwrap(),
            b"untouched"
        );
        let root_link = fixture.root.path().join("stage-link");
        symlink(fixture.stage.path(), &root_link).unwrap();
        let loaded = fixture.load("ctx.file('x')", "{}");
        let definition = unpack(&loaded.value).unwrap();
        let error = execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &BTreeMap::new(),
            "repo",
            "repo",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            &root_link,
        )
        .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!fixture.stage.path().join("x").exists());
    }

    #[test]
    fn json_unicode_escapes_use_bazel_byte_strings_in_definition_and_callback() {
        let fixture = Fixture::new();
        fixture.file(
            "defs.bzl",
            r#"TOP = json.decode('"\\u00e9\\ud83d\\ude00"')
def _impl(ctx):
    ctx.file("top", TOP, executable = False)
    ctx.file("inner", json.decode('"\\u00e9A"'), executable = False)
R = repository_rule(implementation = _impl)
"#,
        );
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        // Bazel writes UTF-8 bytes of escape runs, never U+00E9 as byte E9.
        assert_eq!(fixture.bytes("top"), b"\xc3\xa9\xf0\x9f\x98\x80");
        assert_eq!(fixture.bytes("inner"), b"\xc3\xa9A");
    }

    #[test]
    fn latin1_attributes_write_bytes_and_wider_unicode_is_rejected_at_both_boundaries() {
        let fixture = Fixture::new();
        let loaded = fixture.load("ctx.file('x', ctx.attr.text)", "{'text': attr.string()}");
        let definition = unpack(&loaded.value).unwrap();
        // Runtime byte data maps to U+0000..U+00FF; Bazel writes those bytes.
        let all: String = (0u8..=255).map(char::from).collect();
        let bound = bind_attributes(
            &definition,
            &BTreeMap::from([("text".to_owned(), serde_json::json!(all))]),
            "canonical+repo",
            |text| fixture.repositories.parse(text, "", ""),
        )
        .unwrap();
        execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &bound,
            "canonical+repo",
            "original",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            fixture.stage.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read(fixture.stage.path().join("x")).unwrap(),
            (0u8..=255).collect::<Vec<_>>()
        );
        fs::remove_file(fixture.stage.path().join("x")).unwrap();

        let error = bind_attributes(
            &definition,
            &BTreeMap::from([("text".to_owned(), serde_json::json!("\u{100}"))]),
            "canonical+repo",
            |text| fixture.repositories.parse(text, "", ""),
        )
        .unwrap_err();
        assert!(error.contains("Latin-1"), "{error}");

        // Deliberately bypass the binder with a typed host value to exercise
        // the executor's defense. The loaded Starlark source remains ASCII.
        let attrs = BTreeMap::from([("text".to_owned(), AttrValue::String("\u{100}".to_owned()))]);
        let error = execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &attrs,
            "canonical+repo",
            "original",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            fixture.stage.path(),
        )
        .unwrap_err();
        assert!(error.contains("Latin-1"), "{error}");
        assert!(!fixture.stage.path().join("x").exists());
    }

    #[test]
    fn template_preserves_binary_and_applies_insertion_order() {
        let fixture = Fixture::new();
        fixture.file("private.bin", b"AB AB\xff\r\n");
        let loaded = fixture.load(
            "ctx.template('out', ctx.attr.src, {'AB': 'Z', 'Z': 'Q', '': 'ignored'})",
            "{'src': attr.label(default='//:private.bin')}",
        );
        let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("out"), b"Q Q\xff\r\n");
        assert_eq!(inputs.templates.len(), 1);
        assert_eq!(inputs.templates[0].source, "//:private.bin");
        assert_eq!(inputs.templates[0].bytes, b"AB AB\xff\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.stage.path().join("out"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn template_same_file_ordered_repeat_reads_and_empty_key_noop() {
        let fixture = Fixture::new();
        let inputs = fixture.run("ctx.file('in', 'aaa')\nctx.template('in', 'in', {'aa': 'b', '': 'x'}, False)\nctx.template('copy', 'in')").unwrap();
        assert_eq!(fixture.bytes("in"), b"ba");
        assert_eq!(fixture.bytes("copy"), b"ba");
        assert_eq!(
            inputs
                .templates
                .iter()
                .map(|input| (input.source.as_str(), input.bytes.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                ("stage:in", b"aaa".as_slice()),
                ("stage:in", b"ba".as_slice())
            ]
        );
    }

    #[test]
    fn label_templates_require_build_not_exports_or_analysis() {
        let fixture = Fixture::new();
        fixture.file("pkg/private", b"private");
        let schema = "{'src': attr.label(default='//pkg:private')}";
        let loaded = fixture.load("ctx.template('x', ctx.attr.src)", schema);
        assert!(
            fixture
                .run_loaded(loaded, BTreeMap::new())
                .unwrap_err()
                .contains("BUILD")
        );
        fixture.file(
            "pkg/BUILD.bazel",
            b"fail('do not evaluate or require exports_files')",
        );
        fixture
            .run_loaded(
                fixture.load("ctx.template('x', ctx.attr.src)", schema),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(fixture.bytes("x"), b"private");
    }

    #[cfg(unix)]
    #[test]
    fn label_templates_reject_symlink_files_parents_and_markers() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("actual", b"private");
        fixture.file("pkg/BUILD", b"");
        symlink(
            fixture.root.path().join("actual"),
            fixture.root.path().join("pkg/link"),
        )
        .unwrap();
        symlink(
            fixture.root.path().join("pkg"),
            fixture.root.path().join("alias"),
        )
        .unwrap();
        for label in ["//pkg:link", "//alias:link"] {
            let schema = format!("{{'src': attr.label(default='{label}')}}");
            let error = fixture
                .run_loaded(
                    fixture.load("ctx.template('x', ctx.attr.src)", &schema),
                    BTreeMap::new(),
                )
                .unwrap_err();
            assert!(error.contains("symlink"), "{error}");
        }
        fs::remove_file(fixture.root.path().join("pkg/BUILD")).unwrap();
        symlink(
            fixture.root.path().join("BUILD"),
            fixture.root.path().join("pkg/BUILD"),
        )
        .unwrap();
        let error = fixture
            .run_loaded(
                fixture.load(
                    "ctx.template('x', ctx.attr.src)",
                    "{'src': attr.label(default='//pkg:link')}",
                ),
                BTreeMap::new(),
            )
            .unwrap_err();
        assert!(error.contains("marker"), "{error}");
    }

    #[test]
    fn unsupported_methods_returns_and_options_leave_only_private_stage() {
        let fixture = Fixture::new();
        for method in ["execute", "download", "symlink", "path"] {
            fixture.error(
                &format!("ctx.file('partial', 'private')\nctx.{method}('x')"),
                method,
            );
            assert_eq!(fixture.bytes("partial"), b"private");
        }
        fixture.error("ctx.file('partial')\nreturn {}", "other than None");
        fixture.error("ctx.template('out', 'partial', None)", "dict");
        for mode in ["yes", "no"] {
            fixture.error(
                &format!("ctx.template('out', 'partial', watch_template='{mode}')"),
                "unsupported",
            );
        }
        fixture.error("ctx.template('out', '//:partial')", "relative ASCII");
        fixture.error("ctx.template(path='out', template='partial')", "path");
    }

    #[test]
    fn file_and_template_output_limits_checked_before_replacement() {
        let fixture = Fixture::new();
        fixture.error(
            "ctx.file('existing', 'keep')\nctx.file('existing', 'x' * (16 * 1024 * 1024 + 1))",
            "16 MiB",
        );
        assert_eq!(fixture.bytes("existing"), b"keep");
        fixture.error("ctx.file('in', 'aa')\nctx.template('existing', 'in', {'a': 'b' * (8 * 1024 * 1024 + 1)})", "16 MiB");
        assert_eq!(fixture.bytes("existing"), b"keep");
    }

    #[test]
    fn input_and_cumulative_write_limits() {
        let fixture = Fixture::new();
        let input = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(fixture.stage.path().join("huge"))
            .unwrap();
        input.set_len((MAX_FILE_BYTES + 1) as u64).unwrap();
        fixture.error("ctx.template('out', 'huge')", "input exceeds 16 MiB");
        fixture.error(
            "for i in range(5):\n    ctx.file('repeat', 'x' * (16 * 1024 * 1024))",
            "cumulative writes exceed 64 MiB",
        );
        assert_eq!(fixture.bytes("repeat").len(), MAX_FILE_BYTES);
    }

    #[test]
    fn rejects_overdeep_long_paths_and_excessive_operations() {
        let fixture = Fixture::new();
        fixture.error("ctx.file('/'.join(['a'] * 65))", "depth 64");
        fixture.error("ctx.file('a' * 4097)", "4096");
        fixture.error(
            "for i in range(10001):\n    ctx.file('empty')",
            "10000 writes",
        );
    }

    #[test]
    fn transitive_private_functions_survive_loader_drop() {
        let fixture = Fixture::new();
        fixture.file("helper.bzl", "def _private(ctx):\n    ctx.file('alive', 'yes')\ndef public(ctx):\n    _private(ctx)\n");
        fixture.file(
            "defs.bzl",
            "load(':helper.bzl', 'public')\nR = repository_rule(implementation=public)\n",
        );
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        assert!(loaded.functions.len() >= 2);
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("alive"), b"yes");
    }

    #[test]
    fn callback_label_uses_loaded_helpers_lexical_package() {
        let fixture = Fixture::new();
        fixture.file("helper/BUILD", b"");
        fixture.file("helper/source", b"lexical");
        fixture.file("helper/impl.bzl", "def _private(ctx):\n    ctx.template('lexical', Label(':source'))\ndef implementation(ctx):\n    _private(ctx)\n");
        fixture.file("defs.bzl", "load('//helper:impl.bzl', 'implementation')\nR = repository_rule(implementation=implementation)\n");
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("lexical"), b"lexical");
        assert_eq!(inputs.templates[0].source, "//helper:source");
    }

    #[cfg(unix)]
    #[test]
    fn stage_template_rejects_symlinks_and_directory_outputs() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("outside", b"private");
        symlink(
            fixture.root.path().join("outside"),
            fixture.stage.path().join("link"),
        )
        .unwrap();
        fixture.error("ctx.template('out', 'link')", "symlink");
        fs::create_dir(fixture.stage.path().join("directory")).unwrap();
        fixture.error("ctx.file('directory', 'x')", "regular file");
        fixture.error("ctx.template('out', 'directory')", "regular file");
        assert!(!fixture.stage.path().join("out").exists());
    }
}
