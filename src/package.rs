//! Native BUILD evaluation, local macros, and frozen custom-rule declarations.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use starlark::any::ProvidesStaticType;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::starlark_module;
use starlark::syntax::ast::{Argument, Expr};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;

use crate::custom_rule::{AttrSpec, AttrValue, CustomInvocation};
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::visibility::Visibility;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Filegroup,
    Genrule,
    Custom,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub label: Label,
    pub kind: RuleKind,
    pub srcs: Vec<String>,
    pub outs: Vec<String>,
    pub cmd: Option<String>,
    pub visibility: Visibility,
    pub custom: Option<CustomInvocation>,
}

#[derive(Clone, Debug, Default)]
pub struct Package {
    pub rules: BTreeMap<Label, Rule>,
    pub exports: BTreeMap<Label, Visibility>,
    pub default_visibility: Visibility,
}

impl std::ops::Deref for Package {
    type Target = BTreeMap<Label, Rule>;
    fn deref(&self) -> &Self::Target {
        &self.rules
    }
}

// The upstream derive generates an unsafe marker-trait implementation. This
// collector contains only owned 'static Rust data; no handwritten unsafe code.
#[allow(unsafe_code)]
mod collector {
    use super::*;
    #[derive(ProvidesStaticType)]
    pub(super) struct Declarations {
        pub package: String,
        pub repository: String,
        pub repositories: Arc<RepositorySet>,
        pub root: PathBuf,
        pub rules: RefCell<Vec<Rule>>,
        pub exports: RefCell<BTreeMap<Label, Visibility>>,
        pub default_visibility: RefCell<Visibility>,
        pub defaulted_rules: RefCell<BTreeSet<Label>>,
        pub package_called: Cell<bool>,
        pub custom: RefCell<Vec<PendingCustom>>,
    }
}
use collector::Declarations;

#[derive(Debug)]
struct PendingCustom {
    index: usize,
    slot: String,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .map(|context| context.repositories.clone())
}

pub(crate) fn custom_label_context(
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<(Arc<RepositorySet>, PackageId)> {
    let context = declarations(eval)?;
    Ok((context.repositories.clone(), context.package_id()))
}

pub(crate) fn register_custom<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    name: &str,
    visibility: Option<Value<'v>>,
    implementation: Value<'v>,
    attrs: BTreeMap<String, AttrValue>,
    schema: BTreeMap<String, AttrSpec>,
) -> anyhow::Result<()> {
    let context = declarations(eval)?;
    let mut srcs = Vec::new();
    let mut outs = Vec::new();
    for value in attrs.values() {
        match value {
            AttrValue::Label(Some(value)) => srcs.push(value.clone()),
            AttrValue::Labels(values) => srcs.extend(values.iter().cloned()),
            AttrValue::Output(Some(value)) => outs.push(value.clone()),
            AttrValue::Outputs(values) => outs.extend(values.iter().cloned()),
            _ => {}
        }
    }
    let index = context.rules.borrow().len();
    context.declare(
        name,
        RuleKind::Custom,
        srcs,
        outs,
        None,
        parse_visibility(visibility, context)?,
    )?;
    // Not a valid Starlark identifier: BUILD assignments cannot overwrite roots.
    let slot = format!("__custom_implementation${index}");
    eval.module().set(&slot, implementation);
    context.custom.borrow_mut().push(PendingCustom {
        index,
        slot,
        attrs,
        schema,
    });
    Ok(())
}

impl Declarations {
    fn package_id(&self) -> PackageId {
        PackageId {
            repository: self.repository.clone(),
            package: self.package.clone(),
        }
    }

    fn declare(
        &self,
        name: &str,
        kind: RuleKind,
        srcs: Vec<String>,
        outs: Vec<String>,
        cmd: Option<String>,
        visibility: Option<Visibility>,
    ) -> anyhow::Result<NoneType> {
        let label = local_label(name, &self.package_id(), "rule name")?;
        for src in &srcs {
            (if kind == RuleKind::Custom {
                Label::parse_canonical(src).map(|_| ())
            } else {
                Label::validate_syntax(src, &self.package)
            })
            .map_err(|e| anyhow!("invalid srcs entry {src:?} in {label}: {e}"))?;
        }
        if kind == RuleKind::Genrule && outs.is_empty() {
            bail!("genrule {label}: outs must not be empty");
        }
        for out in &outs {
            label
                .with_name(out)
                .map_err(|e| anyhow!("invalid output path {out:?}: {e}"))?;
        }
        if visibility.is_none() {
            self.defaulted_rules.borrow_mut().insert(label.clone());
        }
        self.rules.borrow_mut().push(Rule {
            label,
            kind,
            srcs,
            outs,
            cmd,
            visibility: visibility.unwrap_or_else(|| self.default_visibility.borrow().clone()),
            custom: None,
        });
        Ok(NoneType)
    }
}

fn local_label(name: &str, package: &PackageId, what: &str) -> anyhow::Result<Label> {
    // Prefixing ':' ensures names cannot change the package or repository.
    Label {
        repository: package.repository.clone(),
        package: package.package.clone(),
        name: "__package__".into(),
    }
    .with_name(name)
    .map_err(|e| anyhow!("invalid {what} {name:?}: {e}"))
}

fn strings(value: Value<'_>, attribute: &str) -> anyhow::Result<Vec<String>> {
    let list = ListRef::from_value(value)
        .ok_or_else(|| anyhow!("{attribute} must be a list of strings"))?;
    list.iter()
        .map(|item| {
            item.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("{attribute} must be a list of strings"))
        })
        .collect()
}

fn parse_visibility(
    value: Option<Value<'_>>,
    context: &Declarations,
) -> anyhow::Result<Option<Visibility>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    Visibility::parse_in(
        &strings(value, "visibility")?,
        &context.package_id(),
        &context.repositories,
    )
    .map(Some)
    .map_err(anyhow::Error::msg)
}

fn declarations<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Declarations> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Declarations>())
        .ok_or_else(|| anyhow!("rule declaration outside package evaluation"))
}

#[starlark_module]
fn rule_globals(builder: &mut GlobalsBuilder) {
    fn filegroup<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.declare(
            name,
            RuleKind::Filegroup,
            srcs.map(|v| strings(v, "srcs"))
                .transpose()?
                .unwrap_or_default(),
            Vec::new(),
            None,
            parse_visibility(visibility, context)?,
        )
    }

    fn genrule<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] outs: Value<'v>,
        #[starlark(require = named)] cmd: &str,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        context.declare(
            name,
            RuleKind::Genrule,
            srcs.map(|v| strings(v, "srcs"))
                .transpose()?
                .unwrap_or_default(),
            strings(outs, "outs")?,
            Some(cmd.to_owned()),
            parse_visibility(visibility, context)?,
        )
    }
}

#[starlark_module]
fn package_globals(builder: &mut GlobalsBuilder) {
    fn package<'v>(
        #[starlark(require = named)] default_visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        if context.package_called.replace(true) {
            bail!("package() may only be called once");
        }
        if default_visibility.is_none() {
            bail!("package() requires at least one argument");
        }
        if default_visibility.is_some_and(|v| v.is_none()) {
            bail!("package default_visibility must be a list");
        }
        *context.default_visibility.borrow_mut() =
            parse_visibility(default_visibility, context)?.unwrap_or_default();
        Ok(NoneType)
    }

    fn exports_files<'v>(
        srcs: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        #[starlark(require = named)] licenses: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = declarations(eval)?;
        if licenses.is_some_and(|value| !value.is_none()) {
            bail!("exports_files licenses metadata is not implemented");
        }
        // exports_files is PUBLIC by default, independent of package defaults.
        let visibility = parse_visibility(visibility, context)?.unwrap_or(Visibility::Public);
        for name in strings(srcs, "exports_files srcs")? {
            let label = local_label(&name, &context.package_id(), "exported file")?;
            if let Some(previous) = context
                .exports
                .borrow_mut()
                .insert(label.clone(), visibility.clone())
                && previous != visibility
            {
                bail!("inconsistent visibility for repeated exported file: {label}");
            }
        }
        Ok(NoneType)
    }

    fn glob<'v>(
        include: Option<Value<'v>>,
        exclude: Option<Value<'v>>,
        #[starlark(default = 1)] exclude_directories: i32,
        #[starlark(default = false)] allow_empty: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<String>> {
        let context = declarations(eval)?;
        let includes = include
            .map(|v| strings(v, "glob include"))
            .transpose()?
            .unwrap_or_default();
        let excludes = exclude
            .map(|v| strings(v, "glob exclude"))
            .transpose()?
            .unwrap_or_default();
        crate::glob::glob(
            &context.root,
            &context.package,
            &includes,
            &excludes,
            exclude_directories != 0,
            allow_empty,
        )
        .map_err(anyhow::Error::msg)
    }

    fn package_name(eval: &mut Evaluator<'_, '_, '_>) -> anyhow::Result<String> {
        Ok(declarations(eval)?.package.clone())
    }
}

/// Complete extension-file globals, shared by package and definition loading.
/// Native declarations still require a BUILD evaluation context when invoked.
pub fn bzl_globals() -> Globals {
    let native = GlobalsBuilder::new()
        .with(rule_globals)
        .with(package_globals)
        .build();
    let mut bzl = GlobalsBuilder::extended_by(&[LibraryExtension::StructType]);
    bzl.frozen_heap().add_reference(native.heap());
    bzl.set("native", AllocStruct(native.iter()));
    crate::custom_rule::rule_globals(&mut bzl);
    crate::providers::globals(&mut bzl);
    crate::repository_rule::globals(&mut bzl);
    crate::module_extension::globals(&mut bzl);
    bzl.build()
}

/// Evaluate a BUILD file with native Starlark; never invoke upstream Bazel.
///
/// The caller supplies a main-repository package path (empty for root). Rule and output
/// names follow the same strict relative-path grammar as `Label`. The local `.bzl`
/// loader freezes modules and resolves relative loads in their declaring package.
/// The call stack is limited to 128 frames; there is no total CPU or
/// heap budget, so BUILD files must be trusted.
pub fn evaluate(root: &Path, package: &str) -> Result<Package, String> {
    evaluate_in(
        Arc::new(RepositorySet::new(root.to_path_buf())),
        &PackageId {
            repository: String::new(),
            package: package.to_owned(),
        },
    )
}

pub fn evaluate_in(
    repositories: Arc<RepositorySet>,
    package: &PackageId,
) -> Result<Package, String> {
    local_label("__package__", package, "package path").map_err(|e| e.to_string())?;
    let root = repositories.root(&package.repository)?;
    let build = crate::workspace::package_build_file(root, &package.package)?;
    let source = std::fs::read_to_string(&build)
        .map_err(|e| format!("cannot read {}: {e}", build.display()))?;
    evaluate_at(repositories, &build.to_string_lossy(), package, source)
}

fn evaluate_at(
    repositories: Arc<RepositorySet>,
    filename: &str,
    package: &PackageId,
    source: String,
) -> Result<Package, String> {
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(filename, source, &dialect).map_err(|e| format!("{e:#}"))?;
    // Bazel BUILD files disallow call splats even though ordinary .bzl files
    // support them. Inspect the real AST, never the textual spelling in strings.
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        if let Expr::Call(_, arguments) = &expr.node
            && arguments
                .args
                .iter()
                .any(|arg| matches!(arg.node, Argument::Args(_) | Argument::KwArgs(_)))
        {
            return Err(format!(
                "{}: *args and **kwargs calls are not allowed in BUILD files",
                ast.file_span(expr.span)
            ));
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    let globals = GlobalsBuilder::standard()
        .with(rule_globals)
        .with(package_globals)
        .build();
    let bzl_globals = bzl_globals();
    let loaded = crate::loading::load_modules_with_functions_in(
        repositories.clone(),
        package,
        &ast,
        &bzl_globals,
    )?;
    let modules = loaded
        .modules
        .iter()
        .map(|(name, module)| (name.as_str(), module))
        .collect::<HashMap<_, _>>();
    let loader = ReturnFileLoader { modules: &modules };
    let declarations = Declarations {
        package: package.package.clone(),
        repository: package.repository.clone(),
        root: repositories.root(&package.repository)?.to_path_buf(),
        repositories,
        rules: RefCell::new(Vec::new()),
        exports: RefCell::new(BTreeMap::new()),
        default_visibility: RefCell::new(Visibility::Private),
        defaulted_rules: RefCell::new(BTreeSet::new()),
        package_called: Cell::new(false),
        custom: RefCell::new(Vec::new()),
    };
    let frozen = Module::with_temp_heap(|module| {
        let mut evaluator = Evaluator::new(&module);
        evaluator.extra = Some(&declarations);
        evaluator.set_loader(&loader);
        evaluator
            .set_max_callstack_size(128)
            .map_err(|e| e.to_string())?;
        evaluator.eval_module(ast, &globals).map_err(|e| {
            format!(
                "{e:#}\nSupported rules: filegroup, genrule, and local custom rules. select and non-local repositories remain unsupported."
            )
        })?;
        drop(evaluator);
        module
            .freeze_named(starlark::values::FrozenHeapName::User(Box::new(
                filename.to_owned(),
            )))
            .map_err(|e| format!("cannot freeze BUILD module: {e:?}"))
    })?;
    let mut declared_rules = declarations.rules.into_inner();
    for pending in declarations.custom.into_inner() {
        let (implementation, _) = frozen
            .get_any_visibility(&pending.slot)
            .map_err(|e| e.to_string())?;
        declared_rules[pending.index].custom = Some(CustomInvocation {
            implementation,
            top_level_functions: loaded.functions.clone(),
            attrs: pending.attrs,
            schema: pending.schema,
        });
    }
    let mut rules = validate_rules(declared_rules).map_err(|e| format!("{filename}: {e}"))?;
    let default_visibility = declarations.default_visibility.into_inner();
    for label in declarations.defaulted_rules.into_inner() {
        if let Some(rule) = rules.get_mut(&label) {
            rule.visibility = default_visibility.clone();
        }
    }
    let exports = declarations.exports.into_inner();
    for label in exports.keys() {
        if rules.contains_key(label) || rules.values().any(|rule| rule.outs.contains(&label.name)) {
            return Err(format!(
                "exported file {label} collides with a rule or generated output"
            ));
        }
    }
    Ok(Package {
        rules,
        exports,
        default_visibility,
    })
}

#[cfg(test)]
fn evaluate_source(
    filename: &str,
    package: &str,
    source: String,
) -> Result<BTreeMap<Label, Rule>, String> {
    evaluate_at(
        Arc::new(RepositorySet::new(PathBuf::from("."))),
        filename,
        &PackageId {
            repository: String::new(),
            package: package.to_owned(),
        },
        source,
    )
    .map(|package| package.rules)
}

fn validate_rules(declarations: Vec<Rule>) -> Result<BTreeMap<Label, Rule>, String> {
    let mut rules = BTreeMap::new();
    for rule in declarations {
        if rules.contains_key(&rule.label) {
            return Err(format!("duplicate rule name: {}", rule.label));
        }
        rules.insert(rule.label.clone(), rule);
    }
    let mut outputs = BTreeMap::new();
    for rule in rules.values() {
        for out in &rule.outs {
            let output = local_label(out, &rule.label.package_id(), "output path")
                .map_err(|e| e.to_string())?;
            if rules.contains_key(&output) {
                return Err(format!("output {output} collides with a rule name"));
            }
            if let Some(previous) = outputs.insert(out.clone(), &rule.label) {
                return Err(format!(
                    "duplicate output {output}, declared by {previous} and {}",
                    rule.label
                ));
            }
        }
    }
    for (out, owner) in &outputs {
        for (index, _) in out.match_indices('/') {
            if let Some(other) = outputs.get(&out[..index]) {
                return Err(format!(
                    "output path conflict: {out:?} from {owner} is nested under output {:?} from {other}",
                    &out[..index]
                ));
            }
        }
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn eval(source: &str) -> Result<BTreeMap<Label, Rule>, String> {
        evaluate_source("pkg/BUILD", "pkg", source.to_owned())
    }

    fn rejected(source: &str, expected: &str) {
        let error = eval(source).unwrap_err();
        assert!(error.contains(expected), "expected {expected:?} in {error}");
    }

    #[test]
    fn evaluates_literal_rules_and_defaults() {
        let rules = eval(
            r#"
filegroup(name = "empty")
filegroup(name = "inputs", srcs = ["input.txt", "//other:files"])
genrule(name = "make", srcs = [":inputs"], outs = ["dir/out"], cmd = "cat $(SRCS) > $@")
genrule(name = "constant", outs = ["constant.txt"], cmd = "echo hi > $@")
"#,
        )
        .unwrap();
        assert_eq!(rules.len(), 4);
        let empty = &rules[&Label::parse(":empty", "pkg").unwrap()];
        assert_eq!(empty.kind, RuleKind::Filegroup);
        assert!(empty.srcs.is_empty() && empty.outs.is_empty() && empty.cmd.is_none());
        let generated = &rules[&Label::parse(":make", "pkg").unwrap()];
        assert_eq!(generated.kind, RuleKind::Genrule);
        assert_eq!(generated.srcs, [":inputs"]);
        assert_eq!(generated.outs, ["dir/out"]);
        assert_eq!(generated.cmd.as_deref(), Some("cat $(SRCS) > $@"));
        assert!(
            rules[&Label::parse(":constant", "pkg").unwrap()]
                .srcs
                .is_empty()
        );
    }

    #[test]
    fn preserves_declarations_from_comprehensions() {
        let rules = eval(
            r#"
unused = [filegroup(name = "input_" + n, srcs = [n + ".txt"]) for n in ["a", "b", "c"]]
unused = [genrule(name = "make_" + n, outs = [n + ".out"], cmd = "echo " + n + " > $@") for n in ["a", "b", "c"]]
"#,
        ).unwrap();
        assert_eq!(rules.len(), 6);
        assert_eq!(
            rules[&Label::parse(":input_b", "pkg").unwrap()].srcs,
            ["b.txt"]
        );
    }

    #[test]
    fn attributes_are_strictly_checked() {
        for source in [
            "filegroup(name='x', testonly=True)",
            "filegroup(name='x', **{'tags': []})",
            "genrule(name='x', outs=['y'], cmd='true', tools=[])",
            "genrule(name='x', outs=['y'], cmd='true', executable=True)",
            "genrule(name='x', cmd='true')",
            "genrule(name='x', outs=['y'])",
            "filegroup('x')",
            "filegroup(name=42)",
            "genrule(name='x', outs=['y'], cmd=42)",
        ] {
            assert!(eval(source).is_err(), "accepted {source}");
        }
        for value in ["None", "'x'", "('x',)", "[1]", "{'x': 1}"] {
            rejected(
                &format!("filegroup(name='x', srcs={value})"),
                "list of strings",
            );
            rejected(
                &format!("genrule(name='x', outs={value}, cmd='true')"),
                "list of strings",
            );
        }
        rejected(
            "genrule(name='x', outs=[], cmd='true')",
            "outs must not be empty",
        );
    }

    #[test]
    fn rejects_unsafe_names_and_paths() {
        for name in [
            "", "..", "../x", "/x", "x/", "x//y", "x/./y", "x/../y", "//else:x", ":x", "a\\b",
        ] {
            rejected(&format!("filegroup(name={name:?})"), "rule name");
            rejected(
                &format!("genrule(name='ok', outs=[{name:?}], cmd='true')"),
                "output path",
            );
        }
        rejected(
            "filegroup(name='x', srcs=['../escape'])",
            "invalid srcs entry",
        );
    }

    #[test]
    fn duplicate_and_conflicting_declarations_fail() {
        rejected(
            "filegroup(name='x')\nfilegroup(name='x')",
            "duplicate rule name",
        );
        rejected(
            "genrule(name='x', outs=['y', 'y'], cmd='true')",
            "duplicate output",
        );
        rejected(
            "genrule(name='x', outs=['z'], cmd='true')\ngenrule(name='y', outs=['z'], cmd='true')",
            "duplicate output",
        );
        rejected(
            "genrule(name='x', outs=['x'], cmd='true')",
            "collides with a rule name",
        );
        for source in [
            "filegroup(name='out')\ngenrule(name='x', outs=['out'], cmd='true')",
            "genrule(name='x', outs=['out'], cmd='true')\nfilegroup(name='out')",
        ] {
            rejected(source, "collides with a rule name");
        }
        rejected(
            "genrule(name='x', outs=['a', 'a/b'], cmd='true')",
            "output path conflict",
        );
    }

    #[test]
    fn loading_and_unsupported_bazel_features_fail() {
        rejected(
            "load('@repo//:defs.bzl', 'my_rule')\nmy_rule(name='x')",
            "external repository",
        );
        for source in [
            "filegroup(name='x', srcs=select({'//conditions:default': []}))",
            "native.filegroup(name='x')",
            "cc_binary(name='x')",
        ] {
            rejected(source, "unsupported");
        }
        assert!(eval("filegroup(name=").is_err());
    }

    #[test]
    fn uses_build_not_bzl_syntax() {
        for source in [
            "def declare():\n    filegroup(name='x')\ndeclare()",
            "f = lambda: 1",
            "for n in ['x']:\n    filegroup(name=n)",
            "if True:\n    filegroup(name='x')",
        ] {
            assert!(eval(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn supports_public_private_and_package_visibility() {
        for (attribute, expected) in [
            ("", false),
            (", visibility=['//visibility:private']", false),
            (", visibility=['//visibility:public']", true),
        ] {
            for declaration in [
                format!("filegroup(name='x'{attribute})"),
                format!("genrule(name='x', outs=['y'], cmd='true'{attribute})"),
            ] {
                let rules = eval(&declaration).unwrap();
                assert_eq!(
                    rules[&Label::parse(":x", "pkg").unwrap()].visibility == Visibility::Public,
                    expected
                );
            }
        }
        for value in [
            "['//some:group']",
            "['//visibility:public', '//visibility:private']",
            "['//visibility:public', '//visibility:public']",
            "'//visibility:public'",
            "[1]",
        ] {
            rejected(
                &format!("filegroup(name='x', visibility={value})"),
                "visibility",
            );
            rejected(
                &format!("genrule(name='x', outs=['y'], cmd='true', visibility={value})"),
                "visibility",
            );
        }
    }

    #[test]
    fn evaluates_docker_config_smoke_target() {
        let rules = evaluate_source(
            "src/docker/BUILD.bazel",
            "src/docker",
            "filegroup(name='config', srcs=['config.json'], visibility=['//visibility:public'])"
                .to_owned(),
        )
        .unwrap();
        let rule = &rules[&Label::parse("//src/docker:config", "").unwrap()];
        assert_eq!(rule.visibility, Visibility::Public);
        assert_eq!(rule.srcs, ["config.json"]);
    }

    #[test]
    fn records_owned_attribute_snapshots() {
        let rules = eval("s = ['a']\nfilegroup(name='x', srcs=s)\ns.append('b')").unwrap();
        assert_eq!(rules[&Label::parse(":x", "pkg").unwrap()].srcs, ["a"]);
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bazel-rs-package-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("pkg")).unwrap();
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_build_files_and_prefers_build_bazel() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/BUILD"), "filegroup(name='fallback')").unwrap();
        assert!(
            evaluate(&root.0, "pkg")
                .unwrap()
                .contains_key(&Label::parse(":fallback", "pkg").unwrap())
        );
        std::fs::write(
            root.0.join("pkg/BUILD.bazel"),
            "filegroup(name='preferred')",
        )
        .unwrap();
        let rules = evaluate(&root.0, "pkg").unwrap();
        assert_eq!(rules.len(), 1);
        assert!(rules.contains_key(&Label::parse(":preferred", "pkg").unwrap()));
    }

    #[test]
    fn local_bzl_loading_does_not_resolve_unused_invalid_module() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("MODULE.bazel"), "invalid MODULE !!!").unwrap();
        std::fs::write(
            root.0.join("pkg/defs.bzl"),
            "def macro(name):\n    native.filegroup(name=name)\n",
        )
        .unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "load(':defs.bzl', 'macro')\nmacro(name='local')\n",
        )
        .unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        assert!(package.contains_key(&Label::parse("//pkg:local", "").unwrap()));
        assert!(!root.0.join("bazel-rs-out").exists());
    }

    #[test]
    fn rejects_invalid_packages_and_missing_build_files() {
        let root = TempRoot::new();
        for package in [
            "../escape",
            "/absolute",
            "pkg/../other",
            "pkg//sub",
            "pkg/.",
        ] {
            assert!(evaluate(&root.0, package).unwrap_err().contains("package"));
        }
        assert!(evaluate(&root.0, "missing").is_err());
    }

    #[test]
    fn root_package_is_supported() {
        let root = TempRoot::new();
        std::fs::write(root.0.join("BUILD.bazel"), "filegroup(name='root')").unwrap();
        assert!(
            evaluate(&root.0, "")
                .unwrap()
                .contains_key(&Label::parse("//:root", "").unwrap())
        );
    }

    fn custom_fixture(defs: &str, build: &str) -> Result<Package, String> {
        let root = TempRoot::new();
        std::fs::write(root.0.join("pkg/defs.bzl"), defs).unwrap();
        std::fs::write(root.0.join("pkg/BUILD.bazel"), build).unwrap();
        evaluate(&root.0, "pkg")
    }

    #[test]
    fn custom_declarations_snapshot_defaults_labels_and_outputs_without_running_implementation() {
        let package = custom_fixture(
            r#"
def impl(ctx):
    fail("implementation must not run during package evaluation/query")
defaults = ["initial"]
r = rule(implementation=impl, attrs={
    "s": attr.string(default="hello"),
    "ss": attr.string_list(default=defaults),
    "i": attr.int(default=12),
    "b": attr.bool(default=True),
    "dep": attr.label(default="//other:x"),
    "deps": attr.label_list(allow_files=True),
    "out": attr.output(),
    "outputs": attr.output_list(),
})
defaults.append("late")
"#,
            r#"
load(":defs.bzl", "r")
deps = [":source", "plain.txt"]
r(name="x", deps=deps, out="result", outputs=["dir/second"])
deps.append("later")
"#,
        )
        .unwrap();
        let rule = &package[&Label::parse(":x", "pkg").unwrap()];
        assert_eq!(rule.kind, RuleKind::Custom);
        assert_eq!(rule.srcs, ["//other:x", "//pkg:source", "//pkg:plain.txt"]);
        assert_eq!(rule.outs, ["result", "dir/second"]);
        let custom = rule.custom.as_ref().unwrap();
        assert_eq!(custom.attrs["s"], AttrValue::String("hello".into()));
        assert_eq!(
            custom.attrs["ss"],
            AttrValue::Strings(vec!["initial".into()])
        );
        assert_eq!(custom.attrs["i"], AttrValue::Int(12));
        assert_eq!(custom.attrs["b"], AttrValue::Bool(true));
        assert!(custom.schema["deps"].allow_files);
    }

    #[test]
    fn custom_attributes_enforce_types_mandatory_and_named_arguments() {
        let defs = r#"
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "s": attr.string(mandatory=True),
    "ss": attr.string_list(),
    "i": attr.int(),
    "b": attr.bool(),
    "dep": attr.label(),
    "deps": attr.label_list(),
    "out": attr.output(),
    "outputs": attr.output_list(),
})
"#;
        for (call, expected) in [
            ("r(name='x')", "mandatory"),
            ("r(name='x', s=1)", "string"),
            ("r(name='x', s='', ss=('a',))", "list of strings"),
            ("r(name='x', s='', ss=[1])", "list of strings"),
            ("r(name='x', s='', i=True)", "int"),
            ("r(name='x', s='', b=1)", "bool"),
            ("r(name='x', s='', dep=1)", "string"),
            ("r(name='x', s='', deps=[1])", "list of strings"),
            ("r(name='x', s='', out='../escape')", "output path"),
            ("r(name='x', s='', outputs=[1])", "list of strings"),
            ("r(name='x', s='', unknown=1)", "unknown attribute"),
            ("r('x', s='')", "positional"),
            ("r(s='')", "name"),
            ("r(name=1, s='')", "name"),
        ] {
            let error =
                custom_fixture(defs, &format!("load(':defs.bzl', 'r')\n{call}")).unwrap_err();
            assert!(
                error.contains(expected),
                "{call}: expected {expected:?}: {error}"
            );
        }
    }

    #[test]
    fn custom_common_visibility_defaults_duplicates_and_output_collisions() {
        let defs = "def impl(ctx):\n    return []\nr = rule(implementation=impl, attrs={'out': attr.output()})";
        let package = custom_fixture(defs, "load(':defs.bzl', 'r')\nr(name='x')\nr(name='y', visibility=['//visibility:private'])\npackage(default_visibility=['//visibility:public'])").unwrap();
        assert_eq!(
            package[&Label::parse(":x", "pkg").unwrap()].visibility,
            Visibility::Public
        );
        assert_eq!(
            package[&Label::parse(":y", "pkg").unwrap()].visibility,
            Visibility::Private
        );
        for (calls, expected) in [
            ("r(name='x')\nr(name='x')", "duplicate rule"),
            ("r(name='x', out='x')", "collides"),
            (
                "r(name='x', out='z')\nr(name='y', out='z')",
                "duplicate output",
            ),
            ("r(name='x', visibility=42)", "visibility"),
        ] {
            let error =
                custom_fixture(defs, &format!("load(':defs.bzl', 'r')\n{calls}")).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
    }

    #[test]
    fn custom_owned_implementation_survives_modules_and_preserves_nested_helpers() {
        let package = custom_fixture(
            r#"
def helper(value):
    return "prefix:" + value
def factory():
    captured = ["alive"]
    def nested_helper():
        return helper(captured[0])
    def implementation(ctx):
        return nested_helper()
    return rule(implementation=implementation)
r = factory()
def macro(name):
    return r(name=name)
"#,
            "load(':defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let implementation = package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .implementation
            .clone();
        drop(package);
        Module::with_temp_heap(|module| {
            let function = module.heap().access_owned_frozen_value(&implementation);
            let mut eval = Evaluator::new(&module);
            let value = eval
                .eval_function(function, &[Value::new_none()], &[])
                .unwrap();
            assert_eq!(value.unpack_str(), Some("prefix:alive"));
        });
    }

    #[test]
    fn custom_definition_globals_are_not_build_globals() {
        rejected("x = attr.string()", "attr");
        rejected("x = rule(implementation=print)", "rule");
    }

    #[test]
    fn custom_invocations_retain_top_level_function_identities_for_analysis() {
        let package = custom_fixture(
            r#"
def _mapper(value):
    return value
def _impl(ctx):
    return _mapper
r = rule(implementation=_impl)
"#,
            "load(':defs.bzl', 'r')\nr(name='x')\nr(name='y')",
        )
        .unwrap();
        let first = package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        let second = package[&Label::parse(":y", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        assert_eq!(first.top_level_functions.len(), 2);
        assert!(
            first
                .top_level_functions
                .iter()
                .any(|f| f.value().ptr_eq(first.implementation.value()))
        );
        for (a, b) in first
            .top_level_functions
            .iter()
            .zip(&second.top_level_functions)
        {
            assert!(a.value().ptr_eq(b.value()));
        }
        let invocation = first.clone();
        drop(package);
        Module::with_temp_heap(|module| {
            let function = module
                .heap()
                .access_owned_frozen_value(&invocation.implementation);
            let mapper = Evaluator::new(&module)
                .eval_function(function, &[Value::new_none()], &[])
                .unwrap();
            assert!(
                invocation
                    .top_level_functions
                    .iter()
                    .any(|f| f.value().ptr_eq(mapper))
            );
        });
    }

    #[test]
    fn custom_relative_defaults_use_lexical_extension_but_arguments_use_build_package() {
        let root = TempRoot::new();
        for package in ["defs", "helpers"] {
            std::fs::create_dir_all(root.0.join(package)).unwrap();
            std::fs::write(root.0.join(package).join("BUILD"), "").unwrap();
        }
        std::fs::write(
            root.0.join("helpers/attrs.bzl"),
            r#"
from_top = attr.label(default=":top-helper")
label_alias = attr.label
def leaf():
    return attr.label(default=":helper")
def nested():
    return leaf()
def label_factory():
    return Label(":helper-object")
"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("defs/rules.bzl"),
            r#"
load("//helpers:attrs.bzl", "from_top", "label_alias", "nested", "label_factory")
def impl(ctx):
    return []
object_label = Label(":object")
r = rule(implementation=impl, attrs={
    "local": attr.label(default=":local"),
    "helper": nested(),
    "top": from_top,
    "alias": label_alias(default=":alias-local"),
    "object": attr.label(default=object_label),
    "helper_object": attr.label(default=label_factory()),
    "defaults": attr.label_list(default=[":local-list", object_label, "@//other:absolute"]),
    "direct": attr.label(),
    "explicit": attr.label(),
    "mixed": attr.label_list(),
})
def macro(name):
    r(name=name, direct=":caller", explicit=object_label, mixed=[":caller-list", object_label])
"#,
        )
        .unwrap();
        std::fs::write(
            root.0.join("pkg/BUILD"),
            "load('//defs:rules.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let package = evaluate(&root.0, "pkg").unwrap();
        let attrs = &package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .attrs;
        for (name, label) in [
            ("local", "//defs:local"),
            ("helper", "//helpers:helper"),
            ("top", "//helpers:top-helper"),
            ("alias", "//defs:alias-local"),
            ("object", "//defs:object"),
            ("helper_object", "//helpers:helper-object"),
            ("direct", "//pkg:caller"),
            ("explicit", "//defs:object"),
        ] {
            assert_eq!(attrs[name], AttrValue::Label(Some(label.into())), "{name}");
        }
        assert_eq!(
            attrs["defaults"],
            AttrValue::Labels(vec![
                "//defs:local-list".into(),
                "//defs:object".into(),
                "//other:absolute".into()
            ])
        );
        assert_eq!(
            attrs["mixed"],
            AttrValue::Labels(vec!["//pkg:caller-list".into(), "//defs:object".into()])
        );
    }

    #[test]
    fn external_defaults_and_macro_labels_are_lexical_but_strings_use_build_context() {
        let main = TempRoot::new();
        let external = TempRoot::new();
        std::fs::write(main.0.join("MODULE.bazel"), format!("module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path={:?})\n", external.0.to_string_lossy())).unwrap();
        std::fs::write(
            external.0.join("MODULE.bazel"),
            "module(name='dep',repo_name='self_dep')",
        )
        .unwrap();
        std::fs::write(
            external.0.join("pkg/BUILD"),
            "exports_files(['input'])\nfilegroup(name='native',srcs=[':input'])",
        )
        .unwrap();
        std::fs::create_dir_all(external.0.join("helpers")).unwrap();
        std::fs::write(external.0.join("helpers/BUILD"), "").unwrap();
        std::fs::write(external.0.join("helpers/attrs.bzl"), "def defaults():\n    return attr.label(default='//helpers:input')\ndef object_label():\n    return Label('@self_dep//helpers:input')\n").unwrap();
        std::fs::write(
            external.0.join("pkg/defs.bzl"),
            r#"
load("//helpers:attrs.bzl", "defaults", "object_label")
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "helper": defaults(),
    "local": attr.label(default=":input"),
    "main": attr.label(default=Label("@@//pkg:input")),
    "explicit": attr.label(),
    "object": attr.label(),
    "absolute": attr.label(),
})
def macro(name):
    r(name=name, explicit=":input", object=object_label(), absolute=Label("//pkg:input"))
"#,
        )
        .unwrap();
        std::fs::write(
            main.0.join("pkg/BUILD"),
            "load('@alias//pkg:defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap();
        let package = evaluate(&main.0, "pkg").unwrap();
        let custom = package[&Label::parse("//pkg:x", "").unwrap()]
            .custom
            .as_ref()
            .unwrap();
        for (name, label) in [
            ("helper", "@@dep+//helpers:input"),
            ("local", "@@dep+//pkg:input"),
            ("main", "//pkg:input"),
            ("explicit", "//pkg:input"),
            ("object", "@@dep+//helpers:input"),
            ("absolute", "@@dep+//pkg:input"),
        ] {
            assert_eq!(
                custom.attrs[name],
                AttrValue::Label(Some(label.into())),
                "{name}"
            );
        }
        let repositories = Arc::new(RepositorySet::new(main.0.clone()));
        let package = evaluate_in(
            repositories,
            &PackageId {
                repository: "dep+".into(),
                package: "pkg".into(),
            },
        )
        .unwrap();
        let native = &package[&Label::parse_canonical("@@dep+//pkg:native").unwrap()];
        assert_eq!(native.srcs, [":input"]);
        assert!(
            package
                .exports
                .contains_key(&Label::parse_canonical("@@dep+//pkg:input").unwrap())
        );
    }

    #[test]
    fn custom_private_defaults_cannot_be_overridden() {
        let defs = r#"
def impl(ctx):
    return []
r = rule(implementation=impl, attrs={
    "_tool": attr.label(default=":tool"),
    "_flags": attr.string_list(default=["private"]),
})
"#;
        let package = custom_fixture(defs, "load(':defs.bzl', 'r')\nr(name='x')").unwrap();
        let attrs = &package[&Label::parse(":x", "pkg").unwrap()]
            .custom
            .as_ref()
            .unwrap()
            .attrs;
        assert_eq!(attrs["_tool"], AttrValue::Label(Some("//pkg:tool".into())));
        assert_eq!(attrs["_flags"], AttrValue::Strings(vec!["private".into()]));
        for argument in ["_tool=':other'", "_flags=['override']"] {
            let error = custom_fixture(
                defs,
                &format!("load(':defs.bzl', 'r')\nr(name='x', {argument})"),
            )
            .unwrap_err();
            assert!(error.contains("private attribute"), "{error}");
        }
    }

    #[test]
    fn custom_rule_definition_is_rejected_inside_build_macros() {
        let error = custom_fixture(
            r#"
def impl(ctx):
    return []
def macro(name):
    r = rule(implementation=impl)
    r(name=name)
"#,
            "load(':defs.bzl', 'macro')\nmacro(name='x')",
        )
        .unwrap_err();
        assert!(error.contains("extension initialization"), "{error}");
    }
}
