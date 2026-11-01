//! Bazel's `print()` outside MODULE.bazel files: MethodLibrary.print
//! (net/starlark/java/eval/MethodLibrary.java:940-955) reports a DEBUG
//! event at the call's location, which the console shows as
//! `DEBUG: FILE:LINE:COL: TEXT` (Event.makeDebugPrintHandler) — in BUILD
//! files, `.bzl` top levels and macros, rule, aspect and transition
//! implementations, repository rules and module extensions alike (MODULE
//! files: crate::bzlmod's own `print`).
//!
//! The interpreter's `print` formats the text and hands it to the
//! evaluation's print handler, which sees no location. So the `print`
//! bound here records the caller's location in a thread-local slot and
//! delegates to the interpreter's (same parameters, same binding errors);
//! the handlers of the evaluations above turn the text into the DEBUG line
//! with [`line`], which takes the slot. A handler that does not (the
//! interpreter's default one) prints the bare text as before.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{FrozenValue, Value};

thread_local! {
    /// The location of the `print` call whose text the print handler is
    /// about to receive.
    static LOCATION: RefCell<Option<String>> = const { RefCell::new(None) };
    /// `print` prints nothing ([`silenced`]).
    static SILENCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with `print` printing nothing: an `Args.map_each` callback
/// (StarlarkCustomCommandLine.applyMapEach, :1140, gives its thread a print
/// handler that drops the text) and a TemplateDict `map_each` (its thread's
/// default handler writes to the server's own stderr, never the client's).
pub(crate) fn silenced<T>(f: impl FnOnce() -> T) -> T {
    let previous = SILENCED.with(|silenced| silenced.replace(true));
    let result = f();
    SILENCED.with(|silenced| silenced.set(previous));
    result
}

/// The main repository's directory: its `.bzl` files (whose evaluations
/// are named by label) print under their path, like Bazel's locations.
static WORKSPACE: OnceLock<PathBuf> = OnceLock::new();

/// Record the main repository's directory (once, by the CLI).
pub fn set_workspace(root: &Path) {
    let _ = WORKSPACE.set(root.to_path_buf());
}

/// This command's output base (`bazel-rs info output_base`): another
/// repository's files print below its `external/` directory.
static OUTPUT_BASE: OnceLock<PathBuf> = OnceLock::new();

/// Record this command's output base (once, by the CLI).
pub fn set_output_base(path: &Path) {
    let _ = OUTPUT_BASE.set(path.to_path_buf());
}

/// Where Bazel's locations name file `relative` of the external repository
/// `repository` (a canonical name): `<output_base>/external/<repository>/
/// <relative>` — the repository's root is that directory in Bazel
/// (RepositoryDirectoryValue; Location.file is the Root-relative path's
/// absolute form), whatever directory bazel-rs read it from (its
/// repository store, a `local_path_override`'s path). `None` before the CLI
/// set the output base (library use and unit tests keep the read path).
pub(crate) fn external_path(repository: &str, relative: &Path) -> Option<PathBuf> {
    let base = OUTPUT_BASE.get()?;
    let mut path = base.join("external").join(repository);
    if !relative.as_os_str().is_empty() {
        path.push(relative);
    }
    Some(path)
}

/// The path a location names for the file `path`, read from the root
/// `root` of `repository` (empty: the main repository, whose files keep
/// their workspace path).
pub(crate) fn displayed_path(repository: &str, root: &Path, path: &Path) -> PathBuf {
    if repository.is_empty() {
        return path.to_path_buf();
    }
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| external_path(repository, relative))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Another repository's loaded `.bzl` files: label -> the file's path
/// (where it was read from; what locations name before the output base is
/// known).
static SOURCES: LazyLock<std::sync::Mutex<std::collections::HashMap<String, PathBuf>>> =
    LazyLock::new(Default::default);

/// Record where another repository's `.bzl` file `label` (its module's
/// name) was read from.
pub(crate) fn record_source(label: &str, path: &Path) {
    if ["@@//", "@//", "//"]
        .iter()
        .any(|prefix| label.starts_with(prefix))
    {
        return;
    }
    SOURCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(label.to_owned(), path.to_path_buf());
}

/// `pkg/name` of a label's `pkg:name` part.
fn label_path(relative: &str) -> PathBuf {
    let (package, file) = relative.split_once(':').unwrap_or((relative, ""));
    let mut path = PathBuf::new();
    if !package.is_empty() {
        path.push(package);
    }
    if !file.is_empty() {
        path.push(file);
    }
    path
}

/// A source file as a location names it: a main-repository label
/// (`//pkg:defs.bzl`, the name of a loaded `.bzl` module) as its path once
/// the workspace is known, another repository's `.bzl` file
/// (`@@repo//pkg:defs.bzl`) as `<output_base>/external/repo/pkg/defs.bzl`
/// ([`external_path`]; before the output base is known, the path it was
/// read from, [`record_source`]); anything else (a BUILD file's path) as
/// it is.
pub(crate) fn location_file(name: &str) -> String {
    if let Some((repository, relative)) = name
        .strip_prefix("@@")
        .and_then(|rest| rest.split_once("//"))
        && !repository.is_empty()
        && let Some(path) = external_path(repository, &label_path(relative))
    {
        return path.display().to_string();
    }
    if let Some(path) = SOURCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
    {
        return path.display().to_string();
    }
    let relative = ["@@//", "@//", "//"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix));
    match (relative, WORKSPACE.get()) {
        (Some(relative), Some(root)) => root.join(label_path(relative)).display().to_string(),
        _ => name.to_owned(),
    }
}

/// `FILE:LINE:COL` of the current call's left parenthesis (Bazel's call
/// location), empty without a call frame.
fn caller_location(eval: &Evaluator<'_, '_, '_>) -> String {
    let Some(call) = eval.call_stack_top_location() else {
        return String::new();
    };
    // The span covers the whole call expression; the callee is the name
    // `print`, so its first '(' is the call's own.
    let offset = call.source_span().find('(').unwrap_or(0);
    let begin = call.span.begin().get() + offset as u32;
    let position = call
        .file
        .resolve_span(starlark::codemap::Span::new(
            starlark::codemap::Pos::new(begin),
            starlark::codemap::Pos::new(begin),
        ))
        .begin;
    format!(
        "{}:{}:{}",
        location_file(call.filename()),
        position.line + 1,
        position.column + 1
    )
}

/// Bind `print` (over the interpreter's, when `builder` has it).
pub(crate) fn install(builder: &mut GlobalsBuilder) {
    located_print(builder);
}

/// The console line of a printed `text`: `DEBUG: FILE:LINE:COL: TEXT` when
/// the `print` of this module made the call, `text` otherwise.
pub(crate) fn line(text: &str) -> String {
    match LOCATION.with(|slot| slot.borrow_mut().take()) {
        Some(location) => format!("DEBUG: {location}: {text}"),
        None => text.to_owned(),
    }
}

#[starlark_module]
fn located_print(builder: &mut GlobalsBuilder) {
    fn print<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(require = named, default = " ")] sep: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        if SILENCED.with(std::cell::Cell::get) {
            return Ok(NoneType);
        }
        let location = caller_location(eval);
        LOCATION.with(|slot| *slot.borrow_mut() = (!location.is_empty()).then_some(location));
        let sep = eval.heap().alloc_str(sep).to_value();
        let printed = eval.eval_function(standard_print().to_value(), &args.items, &[("sep", sep)]);
        // A handler that took no location leaves none for the next print.
        LOCATION.with(|slot| slot.borrow_mut().take());
        printed?;
        Ok(NoneType)
    }
}

/// The interpreter's own `print` ([`LibraryExtension::Print`]).
fn standard_print() -> FrozenValue {
    static GLOBALS: LazyLock<Globals> =
        LazyLock::new(|| GlobalsBuilder::extended_by(&[LibraryExtension::Print]).build());
    GLOBALS
        .iter()
        .find_map(|(name, value)| (name == "print").then_some(value))
        .unwrap_or_else(|| unreachable!("LibraryExtension::Print defines print"))
}
