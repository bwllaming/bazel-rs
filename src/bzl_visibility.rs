//! Bazel 9.3 `.bzl` load visibility (`visibility()` and load-time checks).
//!
//! Sources (pinned reference e89755...):
//! - `BazelBuildApiGlobals.visibility` (BazelBuildApiGlobals.java:43-94)
//! - `BzlInitThreadContext.fromOrFail` (BzlInitThreadContext.java:84-92)
//! - `BzlVisibility.of` / `allowsPackage` (BzlVisibility.java)
//! - `BzlLoadFunction.checkLoadVisibilities` / `visibilityViolation`
//!   (BzlLoadFunction.java:1282-1318, 1669-1673)
//!
//! `--experimental_bzl_visibility` and `--check_bzl_visibility` both default to
//! true (BuildLanguageOptions.java:202, 233); only those defaults are emulated.

use crate::label::{Label, PackageId};
use crate::loading::ExtensionContext;
use crate::package_specification::{Specification, from_string_for_bzl_visibility};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::AstModule;
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use std::cell::RefCell;
use std::sync::Arc;

/// The load visibility of one initialized `.bzl` module.
#[derive(Clone, Debug)]
pub(crate) enum BzlVisibility {
    Public,
    Private,
    List(Arc<[Specification]>),
}

impl BzlVisibility {
    /// `BzlVisibility.of`: an empty list or a lone "private" is PRIVATE; any
    /// "public" makes the whole list PUBLIC; otherwise the union of the specs.
    pub(crate) fn of(specs: Vec<Specification>) -> Self {
        if specs.is_empty() || (specs.len() == 1 && matches!(specs[0], Specification::Private)) {
            return Self::Private;
        }
        if specs
            .iter()
            .any(|spec| matches!(spec, Specification::Public))
        {
            return Self::Public;
        }
        Self::List(specs.into())
    }

    /// Does NOT include the always-allowed same-package case.
    pub(crate) fn allows_package(&self, package: &PackageId) -> bool {
        match self {
            Self::Public => true,
            Self::Private => false,
            Self::List(specs) => specs.iter().any(|spec| spec.contains(package)),
        }
    }
}

/// `PackageIdentifier.getCanonicalForm`: "" for main, "@@name" otherwise.
pub(crate) fn package_canonical_form(package: &PackageId) -> String {
    if package.repository.is_empty() {
        format!("//{}", package.package)
    } else {
        format!("@@{}//{}", package.repository, package.package)
    }
}

/// Analogue of Bazel's per-thread `BzlInitThreadContext`: present only while
/// the native loader executes one `.bzl` file's top level. It is keyed by the
/// identity of that evaluation's `ExtensionContext`, so other evaluators that
/// borrow the same context type (analysis, callbacks, tests) never qualify.
struct InitFrame {
    context: usize,
    visibility: Option<BzlVisibility>,
}

thread_local! {
    static INIT: RefCell<Vec<InitFrame>> = const { RefCell::new(Vec::new()) };
}

fn context_identity(context: &ExtensionContext) -> usize {
    std::ptr::from_ref(context) as usize
}

/// Scoped `.bzl` initialization state; dropping it (including on error)
/// always ends initialization.
pub(crate) struct InitGuard {
    context: usize,
}

impl InitGuard {
    pub(crate) fn begin(context: &ExtensionContext) -> Self {
        let context = context_identity(context);
        INIT.with(|stack| {
            stack.borrow_mut().push(InitFrame {
                context,
                visibility: None,
            })
        });
        Self { context }
    }

    /// The declared visibility, or PUBLIC when `visibility()` was not called
    /// (BzlLoadFunction.java:923-926).
    pub(crate) fn finish(self) -> BzlVisibility {
        INIT.with(|stack| match stack.borrow().last() {
            // No visibility() call: Bazel's default is public.
            Some(frame) if frame.context == self.context => {
                frame.visibility.clone().unwrap_or(BzlVisibility::Public)
            }
            // Unreachable by construction; fail closed (deny) rather than grant.
            _ => {
                debug_assert!(false, "bzl visibility init frame mismatch");
                BzlVisibility::Private
            }
        })
    }
}

impl Drop for InitGuard {
    fn drop(&mut self) {
        INIT.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack
                .last()
                .is_some_and(|frame| frame.context == self.context)
            {
                stack.pop();
            }
        });
    }
}

#[starlark_module]
pub(crate) fn globals(builder: &mut GlobalsBuilder) {
    /// Sets the load visibility of the `.bzl` module currently being initialized.
    fn visibility<'v>(
        #[starlark(require = pos)] value: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // BzlInitThreadContext.fromOrFail(thread, "visibility()").
        let context = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
            .filter(|context| {
                let identity = context_identity(context);
                INIT.with(|stack| {
                    stack
                        .borrow()
                        .last()
                        .is_some_and(|frame| frame.context == identity)
                })
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "visibility() can only be used during .bzl initialization (top-level evaluation)"
                )
            })?;
        // Exactly [<toplevel>, visibility]: no helper function, lambda or
        // native higher-order function may stand between the module and us.
        if eval.call_stack_count() != 2 {
            anyhow::bail!(
                "load visibility may only be set at the top level, not inside a function"
            );
        }
        let identity = context_identity(context);
        let already_set = INIT.with(|stack| {
            stack
                .borrow()
                .last()
                .is_some_and(|frame| frame.visibility.is_some())
        });
        if already_set {
            anyhow::bail!("load visibility may not be set more than once");
        }
        let repositories = context.repositories.clone();
        let repository = context.label.repository.clone();
        let parse = |text: &str| {
            from_string_for_bzl_visibility(&repositories, &repository, text)
                .map_err(anyhow::Error::msg)
        };
        let specs = if let Some(text) = value.unpack_str() {
            vec![parse(text)?]
        } else if let Some(list) = ListRef::from_value(value) {
            // Sequence.cast(value, String.class, "visibility list") checks
            // every element before any specification is parsed.
            let mut texts = Vec::with_capacity(list.len());
            for (index, element) in list.iter().enumerate() {
                let text = element.unpack_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "at index {index} of visibility list, got element of type {}, want string",
                        element.get_type()
                    )
                })?;
                texts.push(text);
            }
            texts
                .into_iter()
                .map(parse)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            // Only StarlarkList is accepted; tuples and other sequences fail.
            anyhow::bail!(
                "Invalid visibility: got '{}', want string or list of strings",
                value.get_type()
            );
        };
        INIT.with(|stack| {
            if let Some(frame) = stack
                .borrow_mut()
                .last_mut()
                .filter(|frame| frame.context == identity)
            {
                frame.visibility = Some(BzlVisibility::of(specs));
            }
        });
        Ok(NoneType)
    }
}

/// Bazel's event `Location` of a load: `<requesting file path>:<line>:<col>`,
/// 1-based, at the start of the module string. The column counts bytes, as
/// Bazel parses files as Latin-1 (`StarlarkUtil.createParserInput`).
fn load_location(path: &str, span: &starlark::codemap::FileSpan) -> String {
    let begin = span.span.begin();
    let line = span.file.find_line(begin);
    let column = begin.get() - span.file.line_span(line).begin().get();
    format!("{path}:{}:{}", line + 1, column + 1)
}

/// `BzlLoadFunction.checkLoadVisibilities` with `--check_bzl_visibility`'s
/// default (errors, not warnings). `loads` corresponds to the requesting
/// file's load statements in source order. `requesting_path` is the
/// requesting file's filesystem path. Every violation is reported, followed by
/// `visibilityViolation(requesting_description)`.
pub(crate) fn check_load_visibilities<'a>(
    requesting_package: &PackageId,
    requesting_description: &str,
    requesting_path: &str,
    ast: &AstModule,
    loads: impl IntoIterator<Item = (Label, &'a BzlVisibility)>,
) -> Result<(), String> {
    let mut violations = Vec::new();
    for (load, (label, visibility)) in ast.loads().into_iter().zip(loads) {
        if label.package_id() != *requesting_package
            && !visibility.allows_package(requesting_package)
        {
            violations.push(format!(
                "{}: Starlark file {label} is not visible for loading from package {}. Check the file's `visibility()` declaration.",
                load_location(requesting_path, &load.span),
                package_canonical_form(requesting_package)
            ));
        }
    }
    if violations.is_empty() {
        return Ok(());
    }
    violations.push(format!(
        "{requesting_description} contains .bzl load visibility violations"
    ));
    Err(violations.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::RepositorySet;
    use starlark::environment::Module;
    use starlark::syntax::Dialect;
    use std::fs;
    use std::path::Path;

    fn package(repository: &str, package: &str) -> PackageId {
        PackageId {
            repository: repository.into(),
            package: package.into(),
        }
    }

    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (name, text) in [("MODULE.bazel", "module(name='main')\n"), ("BUILD", "")]
            .iter()
            .chain(files)
        {
            let path = root.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        root
    }

    /// Evaluate `source` as the BUILD file of `package` through the real loader.
    fn load(root: &Path, package: &str, source: &str) -> Result<(), String> {
        let ast = AstModule::parse(
            &format!("{package}/BUILD"),
            source.to_owned(),
            &Dialect::Standard,
        )
        .unwrap();
        crate::loading::load_modules(root, package, &ast, &crate::package::bzl_globals())
            .map(|_| ())
    }

    fn spec(repository: &str, text: &str) -> Result<Specification, String> {
        let root = workspace(&[]);
        from_string_for_bzl_visibility(&RepositorySet::new(root.path().into()), repository, text)
    }

    fn visibility_error(source: &str) -> String {
        let root = workspace(&[("a/BUILD", ""), ("a/defs.bzl", source)]);
        load(root.path(), "b", "load('//a:defs.bzl', 'X')").unwrap_err()
    }

    #[test]
    fn visibility_call_errors_match_bazel_text() {
        for (source, message) in [
            (
                "def f():\n    visibility('public')\nf()\nX = 1\n",
                "load visibility may only be set at the top level, not inside a function",
            ),
            (
                "[visibility(v) for v in ['public']]\nX = sorted([1], key = visibility)\n",
                "load visibility may only be set at the top level, not inside a function",
            ),
            (
                "visibility('private')\nvisibility('public')\nX = 1\n",
                "load visibility may not be set more than once",
            ),
            (
                "visibility(('//a',))\nX = 1\n",
                "Invalid visibility: got 'tuple', want string or list of strings",
            ),
            (
                "visibility(None)\nX = 1\n",
                "Invalid visibility: got 'NoneType', want string or list of strings",
            ),
            (
                "visibility(['//a', 3, 'bad'])\nX = 1\n",
                "at index 1 of visibility list, got element of type int, want string",
            ),
            (
                "visibility(['bad', '-//x'])\nX = 1\n",
                "invalid package name 'bad': must start with '//', '@', or be 'public' or 'private'",
            ),
            ("visibility(value = 'public')\nX = 1\n", "value"),
        ] {
            let error = visibility_error(source);
            assert!(error.contains(message), "{source}: {error}");
            assert!(error.starts_with("cannot evaluate //a:defs.bzl"), "{error}");
        }
        // A list comprehension at top level is still the top level (no frame).
        let root = workspace(&[
            ("a/BUILD", ""),
            ("a/defs.bzl", "[visibility(v) for v in ['public']]\nX = 1\n"),
        ]);
        load(root.path(), "b", "load('//a:defs.bzl', 'X')").unwrap();
    }

    #[test]
    fn visibility_outside_bzl_initialization_is_rejected() {
        let message =
            "visibility() can only be used during .bzl initialization (top-level evaluation)";
        // No initialization context at all (e.g. a plain evaluator).
        let error = Module::with_temp_heap(|module| {
            let ast = AstModule::parse("x.bzl", "visibility('public')".into(), &Dialect::Standard)
                .unwrap();
            starlark::eval::Evaluator::new(&module)
                .eval_module(ast, &crate::package::bzl_globals())
                .map(|_| ())
                .unwrap_err()
                .to_string()
        });
        assert!(error.contains(message), "{error}");
        // An ExtensionContext that is not the active initialization frame.
        let error = Module::with_temp_heap(|module| {
            let context = ExtensionContext {
                label: Label::parse_canonical("//a:x.bzl").unwrap(),
                repositories: Arc::new(RepositorySet::new(".".into())),
                byte_strings: false,
            };
            let ast = AstModule::parse("x.bzl", "visibility('public')".into(), &Dialect::Standard)
                .unwrap();
            let mut eval = starlark::eval::Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(ast, &crate::package::bzl_globals())
                .map(|_| ())
                .unwrap_err()
                .to_string()
        });
        assert!(error.contains(message), "{error}");
        // A frozen .bzl function invoked after initialization finished.
        let root = workspace(&[
            ("a/BUILD", ""),
            ("a/defs.bzl", "def f():\n    visibility('public')\n"),
        ]);
        let ast = AstModule::parse(
            "a/BUILD",
            "load(':defs.bzl', 'f')".into(),
            &Dialect::Standard,
        )
        .unwrap();
        let modules =
            crate::loading::load_modules(root.path(), "a", &ast, &crate::package::bzl_globals())
                .unwrap();
        let function = modules[":defs.bzl"].get("f").unwrap();
        let error = Module::with_temp_heap(|module| {
            starlark::eval::Evaluator::new(&module)
                .eval_function(module.heap().access_owned_frozen_value(&function), &[], &[])
                .map(|_| ())
                .unwrap_err()
                .to_string()
        });
        assert!(error.contains(message), "{error}");
        // The thread-local initialization stack is empty again.
        INIT.with(|stack| assert!(stack.borrow().is_empty()));
    }

    #[test]
    fn specification_errors_match_bazel_text() {
        for (text, message) in [
            (
                "foo",
                "invalid package name 'foo': must start with '//', '@', or be 'public' or 'private'",
            ),
            (
                "",
                "invalid package name '': must start with '//', '@', or be 'public' or 'private'",
            ),
            ("-public", "Cannot negate \"public\" package specification"),
            (
                "-private",
                "Cannot negate \"private\" package specification",
            ),
            ("-//foo", "Cannot use negative package patterns here"),
            ("-//foo/...", "Cannot use negative package patterns here"),
            (
                "-foo",
                "invalid package name 'foo': must start with '//', '@', or be 'public' or 'private'",
            ),
            (
                ":__pkg__",
                "invalid package name ':__pkg__': must start with '//', '@', or be 'public' or 'private'",
            ),
        ] {
            assert_eq!(spec("", text).unwrap_err(), message, "{text}");
        }
        // Genuine Bazel 9.3 texts (tests/fixtures/bzl_visibility/reference_receipt.json).
        for (text, message) in [
            (
                "//b:foo",
                "invalid package name '//b:foo': invalid target name 'foo:__pkg__': target names may not contain ':'",
            ),
            (
                "//b/../c",
                "invalid package name '//b/../c': invalid package name 'b/../c': package name component contains only '.' characters",
            ),
            (
                "@package_metadata",
                "invalid package name '@package_metadata': invalid repository name 'package_metadata:__pkg__': repo names may contain only A-Z, a-z, 0-9, '-', '_', '.' and '+'",
            ),
            // Ported (not captured) LabelParser/LabelValidator texts.
            (
                "//foo/",
                "invalid package name '//foo/': invalid package name 'foo/': package names may not end with '/'",
            ),
            (
                "//foo//bar",
                "invalid package name '//foo//bar': invalid package name 'foo//bar': package names may not contain '//' path separators",
            ),
            (
                "//a\\b",
                "invalid package name '//a\\b': invalid package name 'a\\b': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')",
            ),
            (
                "@.//a",
                "invalid package name '@.//a': invalid repository name '.': repo names are not allowed to be '.'",
            ),
        ] {
            assert_eq!(spec("", text).unwrap_err(), message, "{text}");
        }
        // Bazel's broader grammar: valid there, so valid here (matches nothing
        // native can name, but is not an error).
        for text in ["//foo bar", "//a/b:__pkg__", "//x/...", "@@weird+name//p"] {
            spec("", text).unwrap();
        }
    }

    #[test]
    fn specification_forms_and_visibility_of() {
        let contains = |repository: &str, text: &str, target: PackageId| {
            BzlVisibility::of(vec![spec(repository, text).unwrap()]).allows_package(&target)
        };
        assert!(contains("", "public", package("other+", "x")));
        assert!(!contains("", "private", package("", "")));
        assert!(contains("", "//pkg", package("", "pkg")));
        assert!(!contains("", "//pkg", package("", "pkg/sub")));
        assert!(!contains("", "//pkg", package("ext+", "pkg")));
        assert!(contains("", "//pkg/...", package("", "pkg/sub/deep")));
        assert!(contains("", "//pkg/...", package("", "pkg")));
        assert!(!contains("", "//pkg/...", package("", "pkgx")));
        assert!(contains("", "//pkg:__pkg__", package("", "pkg")));
        assert!(!contains("", "//pkg:__pkg__", package("", "pkg/a")));
        assert!(contains("", "//pkg:__subpackages__", package("", "pkg/a")));
        assert!(contains("", "//", package("", "")));
        assert!(!contains("", "//", package("", "a")));
        // "//..." means the declaring repository only (repoRootMeansCurrentRepo).
        assert!(contains("ext+", "//...", package("ext+", "any/path")));
        assert!(!contains("ext+", "//...", package("", "any/path")));
        assert!(contains("ext+", "//pkg", package("ext+", "pkg")));
        assert!(!contains("ext+", "//pkg", package("", "pkg")));
        assert!(contains("", "@@ext+//pkg", package("ext+", "pkg")));
        assert!(contains("", "@@ext+//...", package("ext+", "a/b")));
        assert!(contains("", "@@ext+//pkg/...", package("ext+", "pkg/b")));
        assert!(contains("", "@main//pkg", package("", "pkg")));
        // An unknown apparent repository is not an error; it matches nothing.
        assert!(!contains("", "@unknown//pkg", package("", "pkg")));
        assert!(!contains(
            "",
            "@unknown//pkg:__pkg__",
            package("unknown", "pkg")
        ));

        // BzlVisibility.of: empty list / lone private -> PRIVATE, any public -> PUBLIC.
        assert!(matches!(BzlVisibility::of(vec![]), BzlVisibility::Private));
        assert!(matches!(
            BzlVisibility::of(vec![Specification::Private]),
            BzlVisibility::Private
        ));
        let mixed = vec![spec("", "//a").unwrap(), Specification::Public];
        assert!(matches!(BzlVisibility::of(mixed), BzlVisibility::Public));
        let union = BzlVisibility::of(vec![
            spec("", "//a").unwrap(),
            spec("", "//b/...").unwrap(),
            Specification::Private,
        ]);
        assert!(matches!(union, BzlVisibility::List(_)));
        assert!(union.allows_package(&package("", "a")));
        assert!(union.allows_package(&package("", "b/c")));
        assert!(!union.allows_package(&package("", "c")));
        assert_eq!(package_canonical_form(&package("", "")), "//");
        assert_eq!(
            package_canonical_form(&package("ext+", "a/b")),
            "@@ext+//a/b"
        );
    }

    #[test]
    fn load_checks_per_requesting_package() {
        let root = workspace(&[
            ("a/BUILD", ""),
            ("a/private.bzl", "visibility('private')\nP = 1\n"),
            ("a/empty.bzl", "visibility([])\nE = 1\n"),
            ("a/listed.bzl", "visibility(['//b', '//c/...'])\nL = 1\n"),
            ("a/default.bzl", "D = 1\n"),
            (
                "a/public.bzl",
                "load(':private.bzl', 'P')\nvisibility('public')\nQ = P\n",
            ),
            ("b/BUILD", ""),
            ("b/user.bzl", "load('//a:private.bzl', 'P')\nU = P\n"),
            ("c/d/BUILD", ""),
        ]);
        let root = root.path();
        // Same package always loads; private file reached through a public one.
        load(
            root,
            "a",
            "load(':private.bzl', 'P')\nload(':empty.bzl', 'E')",
        )
        .unwrap();
        load(
            root,
            "x",
            "load('//a:public.bzl', 'Q')\nload('//a:default.bzl', 'D')",
        )
        .unwrap();
        for package in ["b", "c", "c/d"] {
            load(root, package, "load('//a:listed.bzl', 'L')").unwrap();
        }
        let error = load(
            root,
            "x",
            "load('//a:listed.bzl', 'L')\nload('//a:empty.bzl', 'E')",
        )
        .unwrap_err();
        assert_eq!(
            error,
            "x/BUILD:1:6: Starlark file //a:listed.bzl is not visible for loading from package //x. Check the file's `visibility()` declaration.\n\
             x/BUILD:2:6: Starlark file //a:empty.bzl is not visible for loading from package //x. Check the file's `visibility()` declaration.\n\
             file //x:BUILD contains .bzl load visibility violations"
        );
        // A .bzl requester is checked against its own package, before it runs.
        let error = load(root, "a", "load('//b:user.bzl', 'U')").unwrap_err();
        assert_eq!(
            error,
            format!(
                "{}/b/user.bzl:1:6: Starlark file //a:private.bzl is not visible for loading from package //b. Check the file's `visibility()` declaration.\n\
                 module //b:user.bzl contains .bzl load visibility violations",
                root.display()
            )
        );
        // Columns count bytes (Latin-1 parsing; chars would give 15), 1-based.
        let error = load(root, "x", "X = '\u{e9}'; load('//a:empty.bzl', 'E')").unwrap_err();
        assert!(
            error.starts_with("x/BUILD:1:16: Starlark file //a:empty.bzl"),
            "{error}"
        );
    }

    #[test]
    fn module_extension_entry_file_is_not_checked_but_its_loads_are() {
        let root = workspace(&[
            ("ext/BUILD", ""),
            ("ext/private.bzl", "visibility('private')\nfiles = 1\n"),
            ("lib/BUILD", ""),
            ("lib/private.bzl", "visibility('private')\nP = 1\n"),
            ("ext/bad.bzl", "load('//lib:private.bzl', 'P')\nfiles = P\n"),
        ]);
        let repositories = Arc::new(RepositorySet::new(root.path().into()));
        let label = Label::parse_canonical("//ext:private.bzl").unwrap();
        crate::loading::load_module_extension_in(
            repositories.clone(),
            &label,
            "files",
            &crate::package::module_bzl_globals(),
        )
        .unwrap();
        let label = Label::parse_canonical("//ext:bad.bzl").unwrap();
        let error = crate::loading::load_module_extension_in(
            repositories,
            &label,
            "files",
            &crate::package::module_bzl_globals(),
        )
        .unwrap_err();
        assert!(
            error.ends_with("module //ext:bad.bzl contains .bzl load visibility violations"),
            "{error}"
        );
    }
}
