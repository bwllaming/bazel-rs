//! Compile-time guarded `.bzl` predeclareds (Java `GuardedValue`).
//!
//! Bazel's resolver (Module.resolve) rejects a *free* reference to a disabled
//! guarded predeclared while compiling the file, before any of its loads run,
//! whether or not the referencing code executes. Two kinds matter here:
//!
//! * removed C++ symbols (CcRules.java via BaseRuleClasses.createEmptySymbol):
//!   always disabled, with a "symbol has been removed" message;
//! * `py_internal` (BazelRuleClassProvider, ContextGuardedValue
//!   .onlyInAllowedRepos): enabled only for allowlisted repositories.
//!
//! starlark-rust globals are file-independent, so this pass performs the
//! file-dependent scope resolution for those names only.
//!
//! The same resolution reports starlark-rust universe builtins that Bazel's
//! universe lacks (`chr`, `ord`): Bazel resolves a free reference to them at
//! compile time as `name 'chr' is not defined` (Module.java:250-256,
//! Resolver.java:941-953), whether or not the code runs. It is conservative:
//! a name bound anywhere in an enclosing function, lambda or comprehension is
//! treated as local, so it never rejects a legitimately bound reference.

use crate::label::Label;
use starlark::codemap::Span;
use starlark::syntax::AstModule;
use starlark::syntax::ast::{AssignTarget, AstExpr, AstStmt, ClauseP, Expr, ParameterP, Stmt};
use std::collections::HashSet;

/// (symbol, rules_cc load label) registered by CcRules.init.
const REMOVED: [(&str, &str); 4] = [
    ("CcInfo", "@rules_cc//cc/common:cc_info.bzl"),
    (
        "DebugPackageInfo",
        "@rules_cc//cc/common:debug_package_info.bzl",
    ),
    (
        "CcSharedLibraryInfo",
        "@rules_cc//cc/common:cc_shared_library_info.bzl",
    ),
    (
        "CcSharedLibraryHintInfo",
        "@rules_cc//cc/common:cc_shared_library_hint_info.bzl",
    ),
];

/// BazelRuleClassProvider PYTHON_RULES allowedRepositories, in ImmutableSet
/// (insertion) order: (repository, package prefix).
const PY_INTERNAL_ALLOWED: [(&str, &str); 4] = [
    ("_builtins", ""),
    ("bazel_tools", ""),
    ("rules_python", ""),
    ("", "tools/build_defs/python"),
];

fn removed_message(name: &str, label: &str) -> String {
    format!(
        "The {name} symbol has been removed, add the following to your BUILD/bzl file:\n\nload(\"{label}\", \"{name}\")\n"
    )
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Java `repo.matches(".*\\b" + quote(entry) + "\\b.*")`.
fn word_bounded_contains(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        // \b: a word/non-word transition at each end of the (word) needle.
        before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c))
    })
}

fn prefix_matches(package: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || package == prefix
        || package
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// ContextGuardedValue.onlyInAllowedRepos for `py_internal`.
pub(crate) fn py_internal_allowed(label: &Label) -> bool {
    PY_INTERNAL_ALLOWED.iter().any(|(repo, prefix)| {
        let repo_matches = if repo.is_empty() {
            label.repository.is_empty()
        } else {
            word_bounded_contains(&label.repository, repo)
        };
        repo_matches && prefix_matches(&label.package, prefix)
    })
}

fn py_internal_message() -> String {
    let entries: Vec<String> = PY_INTERNAL_ALLOWED
        .iter()
        .map(|(repo, prefix)| {
            if repo.is_empty() {
                (*prefix).to_owned()
            } else {
                format!("@@{repo}//{prefix}")
            }
        })
        .collect();
    format!(
        "py_internal may only be used from one of the following repositories or prefixes: {}",
        entries.join(", ")
    )
}

/// starlark-rust `GlobalsBuilder::standard` functions that are not in
/// Bazel 9.3's `Starlark.UNIVERSE` (net/starlark/java/eval/MethodLibrary.java
/// has neither `chr` nor `ord`; bazel_lib's strings.bzl defines both).
const NATIVE_ONLY_UNIVERSALS: [&str; 2] = ["chr", "ord"];

/// The first free reference to one of `names` that no module binding
/// shadows, with the names visible at that point (spelling candidates).
fn first_free(ast: &AstModule, names: &[&str]) -> Option<(String, Span, Vec<String>)> {
    let mut module_bound = HashSet::new();
    collect_bindings(ast.statement(), &mut module_bound, true);
    let guarded: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| !module_bound.contains(*name))
        .collect();
    if guarded.is_empty() {
        return None;
    }
    let mut finder = Finder {
        guarded: &guarded,
        scopes: vec![module_bound],
        found: None,
    };
    finder.stmt(ast.statement());
    finder.found
}

/// SpellChecker.didYouMean/suggest (the vendored interpreter's port):
/// case-insensitive edit distance below min(5, (len + 1) / 2); the first
/// strictly better candidate wins. Bazel iterates a HashSet of every visible
/// name; ties are broken here by sorted order. Predeclared and universe names
/// are not candidates here: none is within the bound of `chr` or `ord`.
fn did_you_mean(input: &str, mut candidates: Vec<String>) -> String {
    candidates.sort();
    candidates.dedup();
    starlark::errors::bazel_did_you_mean(input, candidates.iter().map(String::as_str))
}

/// Bazel's resolver error for a free `chr`/`ord` reference in a `.bzl`,
/// BUILD or MODULE file: `name 'chr' is not defined`.
pub(crate) fn check_universe(ast: &AstModule) -> Result<(), String> {
    let Some((name, span, candidates)) = first_free(ast, &NATIVE_ONLY_UNIVERSALS) else {
        return Ok(());
    };
    Err(format!(
        "{}: name '{name}' is not defined{}",
        ast.file_span(span),
        did_you_mean(&name, candidates)
    ))
}

/// Reject the first free reference to a disabled guarded predeclared.
pub(crate) fn check(label: &Label, ast: &AstModule) -> Result<(), String> {
    let mut guarded: Vec<&str> = REMOVED.iter().map(|(name, _)| *name).collect();
    if !py_internal_allowed(label) {
        guarded.push("py_internal");
    }
    let Some((name, span, _)) = first_free(ast, &guarded) else {
        return Ok(());
    };
    let message = REMOVED
        .iter()
        .find(|(removed, _)| *removed == name)
        .map(|(name, label)| removed_message(name, label))
        .unwrap_or_else(py_internal_message);
    Err(format!("{}: {message}", ast.file_span(span)))
}

fn target_names(target: &AssignTarget, out: &mut HashSet<String>) {
    target.visit_lvalue(|ident| {
        out.insert(ident.node.ident.clone());
    });
}

/// Names bound by statements of one scope; nested `def` bodies are separate
/// scopes. At module scope, `load` bindings count too.
fn collect_bindings(stmt: &AstStmt, out: &mut HashSet<String>, module: bool) {
    match &stmt.node {
        Stmt::Statements(children) => children
            .iter()
            .for_each(|child| collect_bindings(child, out, module)),
        Stmt::Def(def) => {
            out.insert(def.name.node.ident.clone());
        }
        Stmt::Assign(assign) => target_names(&assign.lhs, out),
        Stmt::AssignModify(target, _, _) => target_names(target, out),
        Stmt::For(for_) => {
            target_names(&for_.var, out);
            collect_bindings(&for_.body, out, module);
        }
        Stmt::If(_, then) => collect_bindings(then, out, module),
        Stmt::IfElse(_, branches) => {
            collect_bindings(&branches.0, out, module);
            collect_bindings(&branches.1, out, module);
        }
        Stmt::Load(load) if module => {
            for argument in &load.args {
                out.insert(argument.local.node.ident.clone());
            }
        }
        _ => {}
    }
}

struct Finder<'a> {
    guarded: &'a [&'a str],
    /// Visible bindings, outermost (module) first.
    scopes: Vec<HashSet<String>>,
    found: Option<(String, Span, Vec<String>)>,
}

impl Finder<'_> {
    fn is_free(&self, name: &str) -> bool {
        self.guarded.contains(&name) && !self.scopes.iter().any(|scope| scope.contains(name))
    }

    fn parameters<'p>(
        &mut self,
        params: impl IntoIterator<Item = &'p starlark::syntax::ast::AstParameter>,
    ) -> HashSet<String> {
        let mut names = HashSet::new();
        for param in params {
            // Defaults and annotations resolve in the enclosing scope.
            param.node.visit_expr(|expr| self.expr(expr));
            if let ParameterP::Normal(ident, ..)
            | ParameterP::Args(ident, _)
            | ParameterP::KwArgs(ident, _) = &param.node
            {
                names.insert(ident.node.ident.clone());
            }
        }
        names
    }

    fn stmt(&mut self, stmt: &AstStmt) {
        if self.found.is_some() {
            return;
        }
        match &stmt.node {
            Stmt::Def(def) => {
                let mut scope = self.parameters(&def.params);
                collect_bindings(&def.body, &mut scope, false);
                self.scopes.push(scope);
                self.stmt(&def.body);
                self.scopes.pop();
            }
            Stmt::Statements(children) => children.iter().for_each(|child| self.stmt(child)),
            Stmt::If(condition, then) => {
                self.expr(condition);
                self.stmt(then);
            }
            Stmt::IfElse(condition, branches) => {
                self.expr(condition);
                self.stmt(&branches.0);
                self.stmt(&branches.1);
            }
            Stmt::For(for_) => {
                self.expr(&for_.over);
                for_.var.visit_expr(|expr| self.expr(expr));
                self.stmt(&for_.body);
            }
            Stmt::Assign(assign) => {
                assign.lhs.visit_expr(|expr| self.expr(expr));
                self.expr(&assign.rhs);
            }
            Stmt::AssignModify(target, _, rhs) => {
                target.visit_expr(|expr| self.expr(expr));
                self.expr(rhs);
            }
            Stmt::Expression(expr) | Stmt::Return(Some(expr)) => self.expr(expr),
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Pass | Stmt::Load(_) => {}
        }
    }

    fn expr(&mut self, expr: &AstExpr) {
        if self.found.is_some() {
            return;
        }
        match &expr.node {
            Expr::Identifier(ident) => {
                if self.is_free(&ident.node.ident) {
                    let visible = self.scopes.iter().flatten().cloned().collect();
                    self.found = Some((ident.node.ident.clone(), ident.span, visible));
                }
            }
            Expr::Lambda(lambda) => {
                let scope = self.parameters(&lambda.params);
                self.scopes.push(scope);
                self.expr(&lambda.body);
                self.scopes.pop();
            }
            Expr::ListComprehension(_, first, clauses)
            | Expr::DictComprehension(_, first, clauses) => {
                // Conservatively scope every clause variable over the whole
                // comprehension, including the first iterable.
                let mut scope = HashSet::new();
                target_names(&first.var, &mut scope);
                for clause in clauses {
                    if let ClauseP::For(clause) = clause {
                        target_names(&clause.var, &mut scope);
                    }
                }
                self.scopes.push(scope);
                expr.node.visit_expr(|child| self.expr(child));
                self.scopes.pop();
            }
            _ => expr.node.visit_expr(|child| self.expr(child)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::syntax::Dialect;

    fn check_source(label: &str, source: &str) -> Result<(), String> {
        let dialect = Dialect {
            enable_keyword_only_arguments: true,
            ..Dialect::Standard
        };
        let ast = AstModule::parse(label, source.to_owned(), &dialect).unwrap();
        check(&Label::parse_canonical(label).unwrap(), &ast)
    }

    const FILE: &str = "@@other+//pkg:defs.bzl";

    #[test]
    fn removed_cc_symbols_fail_on_any_free_reference() {
        let error = check_source(FILE, "x = 1\ninfo = CcInfo\n").unwrap_err();
        assert_eq!(
            error,
            "@@other+//pkg:defs.bzl:2:8-14: The CcInfo symbol has been removed, add the following to your BUILD/bzl file:\n\nload(\"@rules_cc//cc/common:cc_info.bzl\", \"CcInfo\")\n"
        );
        for (source, symbol, label) in [
            (
                "def f(target):\n    return target[CcInfo]\n",
                "CcInfo",
                "cc_info.bzl",
            ),
            (
                "def f():\n    pass\nx = [DebugPackageInfo]",
                "DebugPackageInfo",
                "debug_package_info.bzl",
            ),
            (
                "x = lambda: CcSharedLibraryInfo",
                "CcSharedLibraryInfo",
                "cc_shared_library_info.bzl",
            ),
            (
                "x = {k: CcSharedLibraryHintInfo for k in []}",
                "CcSharedLibraryHintInfo",
                "cc_shared_library_hint_info.bzl",
            ),
            ("def f(x = CcInfo):\n    pass\n", "CcInfo", "cc_info.bzl"),
            (
                "def f():\n    for x in CcInfo:\n        pass\n",
                "CcInfo",
                "cc_info.bzl",
            ),
            (
                "def f():\n    if True:\n        return CcInfo\n",
                "CcInfo",
                "cc_info.bzl",
            ),
            (
                "def f():\n    x = {}\n    x[CcInfo] = 1\n",
                "CcInfo",
                "cc_info.bzl",
            ),
            ("x = '%s' % CcInfo", "CcInfo", "cc_info.bzl"),
        ] {
            let error = check_source(FILE, source).unwrap_err();
            assert!(
                error.contains(&format!("The {symbol} symbol has been removed"))
                    && error.contains(&format!(
                        "load(\"@rules_cc//cc/common:{label}\", \"{symbol}\")"
                    )),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn bound_or_non_identifier_uses_are_not_free() {
        for source in [
            "load('@rules_cc//cc/common:cc_info.bzl', 'CcInfo')\nx = CcInfo\n",
            "load(':x.bzl', CcInfo = 'Other')\nx = CcInfo\n",
            "CcInfo = provider()\nx = CcInfo\n",
            "def f():\n    return CcInfo\nCcInfo = 1\n",
            "def CcInfo():\n    pass\nx = CcInfo\n",
            "def f(CcInfo):\n    return CcInfo\n",
            "def f(*CcInfo, **DebugPackageInfo):\n    return CcInfo, DebugPackageInfo\n",
            "def f():\n    CcInfo = 1\n    return CcInfo\n",
            "def f():\n    print(CcInfo)\n    CcInfo = 1\n",
            "def f():\n    for CcInfo in []:\n        pass\n    return CcInfo\n",
            "def f():\n    def CcInfo():\n        pass\n    return CcInfo\n",
            "def f(CcInfo):\n    return lambda: CcInfo\n",
            "x = lambda CcInfo: CcInfo\n",
            "x = [CcInfo for CcInfo in []]\n",
            "x = [1 for y in [] for CcInfo in y if CcInfo]\n",
            "x = struct().CcInfo if False else 'CcInfo'\n",
            "x = {'CcInfo': 1}\n",
            "x = getattr(native, 'CcInfo', None)\n",
        ] {
            check_source(FILE, source).unwrap_or_else(|e| panic!("{source}: {e}"));
        }
    }

    #[test]
    fn py_internal_is_restricted_to_allowlisted_repositories() {
        for allowed in [
            "@@rules_python+//python/private:py_internal.bzl",
            "@@rules_python+//tools/build_defs/python/private:py_internal_renamed.bzl",
            "@@rules_python++pip+pypi//:x.bzl",
            "@@bazel_tools//tools/python:x.bzl",
            "@@_builtins//:common/python/x.bzl",
            "//tools/build_defs/python:x.bzl",
            "//tools/build_defs/python/private:x.bzl",
        ] {
            check_source(allowed, "py_internal_renamed = py_internal\n")
                .unwrap_or_else(|e| panic!("{allowed}: {e}"));
        }
        let message = "py_internal may only be used from one of the following repositories or prefixes: @@_builtins//, @@bazel_tools//, @@rules_python//, tools/build_defs/python";
        for denied in [
            "@@rules_go+//go:x.bzl",
            "@@my_rules_python+//x:x.bzl",
            "@@rules_pythonx+//x:x.bzl",
            "//tools/build_defs/pythonx:x.bzl",
            "//python:x.bzl",
        ] {
            let error = check_source(denied, "def f():\n    return py_internal\n").unwrap_err();
            assert!(error.ends_with(message), "{denied}: {error}");
        }
        check_source(
            "@@rules_go+//go:x.bzl",
            "py_internal = 1\nx = py_internal\n",
        )
        .unwrap();
        check_source(
            "@@rules_go+//go:x.bzl",
            "x = getattr(struct(), 'py_internal', None)\n",
        )
        .unwrap();
    }

    #[test]
    fn word_boundaries_follow_java_regex_semantics() {
        assert!(word_bounded_contains("rules_python+", "rules_python"));
        assert!(word_bounded_contains("x+rules_python", "rules_python"));
        assert!(word_bounded_contains("rules_python", "rules_python"));
        assert!(word_bounded_contains("a.rules_python-b", "rules_python"));
        assert!(!word_bounded_contains("my_rules_python+", "rules_python"));
        assert!(!word_bounded_contains("rules_python2", "rules_python"));
        assert!(!word_bounded_contains("", "rules_python"));
    }

    fn universe(source: &str) -> Result<(), String> {
        let dialect = Dialect {
            enable_keyword_only_arguments: true,
            ..Dialect::Standard
        };
        check_universe(&AstModule::parse(FILE, source.to_owned(), &dialect).unwrap())
    }

    #[test]
    fn free_chr_and_ord_are_bazel_resolver_errors_even_when_unreached() {
        assert_eq!(
            universe("x = 1\ndef f():\n    return chr(1)\n").unwrap_err(),
            "@@other+//pkg:defs.bzl:3:12-15: name 'chr' is not defined"
        );
        for (source, name) in [
            ("alias = chr\n", "chr"),
            ("x = [ord('a')]\n", "ord"),
            ("x = lambda: ord\n", "ord"),
            ("def f(x = chr):\n    pass\n", "chr"),
            ("x = {k: chr for k in []}\n", "chr"),
            ("def f():\n    if False:\n        return ord\n", "ord"),
        ] {
            let error = universe(source).unwrap_err();
            assert!(
                error.ends_with(&format!("name '{name}' is not defined")),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn bound_chr_and_ord_are_ordinary_identifiers() {
        for source in [
            "load(':strings.bzl', 'chr', 'ord')\nx = chr(ord('a'))\n",
            "load(':strings.bzl', chr = 'my_chr')\ndef f():\n    return chr(1)\n",
            "def chr(i):\n    return str(i)\ndef ord(c):\n    return 0\nx = [chr, ord]\n",
            "def f(chr, ord = 1):\n    return chr(ord)\n",
            "def f():\n    ord = 1\n    return ord\n",
            "x = [chr for chr in []]\n",
            "x = lambda chr: chr\n",
            "chr = 'chr'\nord = 'ord'\n",
            "x = 'chr(1)'\n# chr ord\n",
        ] {
            universe(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        }
    }

    #[test]
    fn spelling_suggestions_follow_spell_checker_over_visible_names() {
        assert_eq!(
            universe("char = 1\nx = chr\n").unwrap_err(),
            "@@other+//pkg:defs.bzl:2:5-8: name 'chr' is not defined (did you mean 'char'?)"
        );
        let error = universe("def f(word):\n    return ord\n").unwrap_err();
        assert!(error.ends_with(" (did you mean 'word'?)"), "{error}");
        // A local of another function is not visible; distance 2 is too far.
        for source in ["def g(word):\n    pass\nx = ord\n", "abc = 1\nx = ord\n"] {
            assert!(universe(source).unwrap_err().ends_with("is not defined"));
        }
        assert_eq!(
            starlark::errors::bazel_edit_distance("chr", "char", 5),
            Some(1)
        );
        assert_eq!(
            starlark::errors::bazel_edit_distance("ord", "dir", 5),
            Some(3)
        );
    }
}
