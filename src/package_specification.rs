//! Package-local exclusions and an owned, shared package-group union DAG.
//!
//! This retains the native ASCII label profile, not Bazel's broader literal
//! package grammar. No membership lookup loads packages or resolves repositories.

use allocative::Allocative;
use anyhow::bail;
use starlark::collections::StarlarkHasher;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};
use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::repository_budget::HostBudget;

pub(crate) const PROVIDER_ID: &str = "builtin:PackageSpecificationInfo";
const MAX_PATH_NODES: usize = 256;

#[derive(Clone, Debug, Allocative)]
pub(crate) enum Specification {
    Public,
    Private,
    Package {
        repository: String,
        package: String,
        subtree: bool,
    },
}

impl Specification {
    pub(crate) fn contains(&self, target: &PackageId) -> bool {
        match self {
            Self::Public => true,
            Self::Private => false,
            Self::Package {
                repository,
                package,
                subtree,
            } => {
                repository == &target.repository
                    && (package == &target.package
                        || (*subtree
                            && (package.is_empty()
                                || target
                                    .package
                                    .strip_prefix(package.as_str())
                                    .is_some_and(|tail| tail.starts_with('/')))))
            }
        }
    }
}

#[derive(Clone, Debug, Allocative)]
pub(crate) struct GroupContents {
    positives: Vec<Specification>,
    negatives: Vec<Specification>,
}

impl GroupContents {
    pub(crate) fn query_patterns(&self) -> Vec<String> {
        let render = |spec: &Specification| match spec {
            Specification::Public => "public".to_owned(),
            Specification::Private => "private".to_owned(),
            Specification::Package {
                repository,
                package,
                subtree,
            } => {
                let repo = if repository.is_empty() {
                    String::new()
                } else {
                    format!("@@{repository}")
                };
                let suffix = if *subtree {
                    if package.is_empty() { "..." } else { "/..." }
                } else {
                    ""
                };
                format!("{repo}//{package}{suffix}")
            }
        };
        self.positives
            .iter()
            .map(&render)
            .chain(
                self.negatives
                    .iter()
                    .map(|spec| format!("-{}", render(spec))),
            )
            .collect()
    }
    /// Reserve retained copies in the graph owner's cumulative allocation budget.
    pub(crate) fn charge(&self, budget: &mut HostBudget) -> Result<(), String> {
        budget.node()?;
        for spec in self.positives.iter().chain(&self.negatives) {
            budget.node()?;
            if let Specification::Package {
                repository,
                package,
                ..
            } = spec
            {
                budget.string(repository)?;
                budget.string(package)?;
            }
        }
        Ok(())
    }

    pub(crate) fn contains(&self, target: &PackageId) -> bool {
        self.positives.iter().any(|spec| spec.contains(target))
            && !self.negatives.iter().any(|spec| spec.contains(target))
    }
}

#[derive(Debug)]
pub(crate) struct GroupDefinition {
    pub own_specs: GroupContents,
    pub includes: Vec<Label>,
}

/// Parse borrowed specifications, charging one cumulative host budget before
/// copying raw/resolved payloads. The conservative charge includes temporary
/// labels. Callers retaining multiple definitions must also charge cumulatively.
pub(crate) fn parse_specs(
    specs: &[String],
    package: &PackageId,
    repos: &RepositorySet,
) -> Result<GroupContents, String> {
    let mut budget = HostBudget::default();
    let mut contents = GroupContents {
        positives: Vec::new(),
        negatives: Vec::new(),
    };
    for raw in specs {
        budget.string(raw)?;
        let (negative, text) = raw
            .strip_prefix('-')
            .map_or((false, raw.as_str()), |s| (true, s));
        let spec = match text {
            "public" | "private" if negative => {
                return Err("negative public/private package specifications are invalid".to_owned());
            }
            "public" => Specification::Public,
            "private" => Specification::Private,
            _ => {
                if !text.starts_with("//") && !text.starts_with('@') {
                    return Err(format!(
                        "invalid package specification {raw:?}: expected an absolute package, public or private"
                    ));
                }
                // A colon is legal only for the two visibility sentinels. The
                // fallback parser must not accidentally accept ordinary targets.
                let (path, subtree) = if let Some((path, sentinel)) = text.split_once(':') {
                    match sentinel {
                        "__pkg__" => (path, false),
                        "__subpackages__" => (path, true),
                        _ => {
                            return Err(format!(
                                "invalid package specification {raw:?}: expected __pkg__ or __subpackages__ sentinel"
                            ));
                        }
                    }
                } else if text.ends_with("//...") {
                    (&text[..text.len() - 3], true)
                } else if let Some(path) = text.strip_suffix("/...") {
                    (path, true)
                } else {
                    // Upstream tries Label parsing first, so the implicit
                    // target of //p/__subpackages__ is also a sentinel.
                    (text, text.rsplit('/').next() == Some("__subpackages__"))
                };
                // Charge mapping expansion BEFORE constructing the dummy label.
                budget.bytes(
                    repos
                        .resolution_size_bound(path, &package.repository, &package.package)?
                        .saturating_add(2),
                )?;
                let dummy = format!("{path}:x");
                let label = repos.parse(&dummy, &package.repository, &package.package)?;
                Specification::Package {
                    repository: label.repository,
                    package: label.package,
                    subtree,
                }
            }
        };
        if negative {
            contents.negatives.push(spec);
        } else {
            contents.positives.push(spec);
        }
    }
    Ok(contents)
}

/// `PackageSpecification.fromStringForBzlVisibility` (PackageSpecification.java
/// 233-252): `fromString` with `allowPublicPrivate` and
/// `repoRootMeansCurrentRepo` both true, then negative patterns rejected.
/// `repository` is the canonical repository of the `.bzl` being initialized;
/// apparent names use that repository's mapping. Label syntax and its error
/// text follow Bazel's own grammar (`crate::bazel_label`), not the stricter
/// native label profile.
pub(crate) fn from_string_for_bzl_visibility(
    repos: &RepositorySet,
    repository: &str,
    raw: &str,
) -> Result<Specification, String> {
    // PackageSpecification.java:142-150: a "-" prefix is stripped before the
    // positive parse; negated public/private fail first with their own text.
    let (negative, spec) = raw.strip_prefix('-').map_or((false, raw), |s| (true, s));
    if negative && matches!(spec, "public" | "private") {
        return Err(format!("Cannot negate \"{spec}\" package specification"));
    }
    let positive = bzl_spec_positive(repos, repository, spec)?;
    if negative {
        // PackageSpecification.java:248-250.
        return Err("Cannot use negative package patterns here".to_owned());
    }
    Ok(positive)
}

/// Bazel's `LabelSyntaxException` (caught or wrapped) versus a native
/// repository-resolution failure, which Bazel's infallible mapping lookup
/// cannot produce and which is therefore always propagated unchanged.
enum SpecLabelError {
    Syntax(String),
    Resolution(String),
}

/// `fromStringPositive` (PackageSpecification.java:155-214).
fn bzl_spec_positive(
    repos: &RepositorySet,
    repository: &str,
    spec: &str,
) -> Result<Specification, String> {
    match spec {
        "public" => return Ok(Specification::Public),
        "private" => return Ok(Specification::Private),
        _ => {}
    }
    if !spec.starts_with("//") && !spec.starts_with('@') {
        return Err(format!(
            "invalid package name '{spec}': must start with '//', '@', or be 'public' or 'private'"
        ));
    }
    // A label naming a sentinel target wins; syntax errors fall through.
    match bzl_spec_label(repos, repository, spec) {
        Ok((resolved, name)) if name == "__pkg__" => return Ok(bzl_spec_package(resolved, false)),
        Ok((resolved, name)) if name == "__subpackages__" => {
            return Ok(bzl_spec_package(resolved, true));
        }
        Ok(_) | Err(SpecLabelError::Syntax(_)) => {}
        Err(SpecLabelError::Resolution(error)) => return Err(error),
    }
    let (path, subtree) = match spec.strip_suffix("/...") {
        // "//..." means the current repository's root and everything beneath.
        Some("/") => ("//".to_owned(), true),
        // "@repo//..." keeps the "//" separator.
        Some(prefix) if spec.ends_with("//...") => (format!("{prefix}/"), true),
        Some(prefix) => (prefix.to_owned(), true),
        None => (spec.to_owned(), false),
    };
    match bzl_spec_label(repos, repository, &format!("{path}:__pkg__")) {
        Ok((resolved, _)) => Ok(bzl_spec_package(resolved, subtree)),
        Err(SpecLabelError::Syntax(error)) => {
            Err(format!("invalid package name '{spec}': {error}"))
        }
        Err(SpecLabelError::Resolution(error)) => Err(error),
    }
}

/// `Label.parseWithRepoContext` with the declaring repository's mapping
/// (Label.java:177-208). An unknown apparent repository never fails in Bazel:
/// `RepositoryMapping.get` returns an invisible RepositoryName equal to no real
/// package's repository. `None` models that.
fn bzl_spec_label(
    repos: &RepositorySet,
    repository: &str,
    text: &str,
) -> Result<(Option<PackageId>, String), SpecLabelError> {
    let parts =
        crate::bazel_label::Parts::parse_with_repo_context(text).map_err(SpecLabelError::Syntax)?;
    let resolved = match parts.repo {
        // ABSOLUTE_PACKAGE_NAMES stay in the main repository.
        None if matches!(parts.pkg, "conditions" | "visibility") => Some(String::new()),
        None => Some(repository.to_owned()),
        Some(canonical) if parts.repo_is_canonical => Some(canonical.to_owned()),
        Some(apparent) => match repos.parse(&format!("@{apparent}//:x"), repository, "") {
            Ok(label) => Some(label.repository),
            Err(error) if error.starts_with("unknown apparent repository") => None,
            Err(error) => return Err(SpecLabelError::Resolution(error)),
        },
    };
    Ok((
        resolved.map(|repository| PackageId {
            repository,
            package: parts.pkg.to_owned(),
        }),
        parts.target.to_owned(),
    ))
}

fn bzl_spec_package(package: Option<PackageId>, subtree: bool) -> Specification {
    match package {
        Some(PackageId {
            repository,
            package,
        }) => Specification::Package {
            repository,
            package,
            subtree,
        },
        // An invisible repository contains no package: identical membership.
        None => Specification::Private,
    }
}

/// Shared nodes preserve group boundaries: an included positive can re-add a
/// package excluded by the parent's own specifications. Fields are private so
/// every node must pass the path-height invariant before publication.
#[derive(Clone, Allocative)]
pub(crate) struct GroupNode {
    own: Arc<GroupContents>,
    includes: Vec<Arc<GroupNode>>,
    height: usize,
}

impl fmt::Debug for GroupNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never recursively format edges: a compact diamond DAG can have an
        // exponential number of paths despite its bounded height/node count.
        f.debug_struct("GroupNode")
            .field("height", &self.height)
            .field("positive_specs", &self.own.positives.len())
            .field("negative_specs", &self.own.negatives.len())
            .field("direct_includes", &self.includes.len())
            .finish()
    }
}

impl GroupNode {
    /// The analysis graph owner MUST precharge cumulative node/edge/payload
    /// budgets before allocating/cloning these inputs (100k nodes, HostBudget
    /// 64 MiB profile), and publish only successful nodes under canonical labels.
    /// This constructor independently enforces 256 path nodes, including cached
    /// child heights; it neither flattens nor clones transitive contents.
    pub(crate) fn new(
        own: Arc<GroupContents>,
        includes: Vec<Arc<GroupNode>>,
    ) -> Result<Self, String> {
        let height = includes
            .iter()
            .map(|node| node.height)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .filter(|height| *height <= MAX_PATH_NODES)
            .ok_or_else(|| {
                format!("package group DAG exceeds depth limit {MAX_PATH_NODES} path nodes")
            })?;
        Ok(Self {
            own,
            includes,
            height,
        })
    }

    pub(crate) fn contains(&self, target: &PackageId) -> bool {
        let mut scheduled = HashSet::new();
        scheduled.insert(std::ptr::from_ref(self));
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            if node.own.contains(target) {
                return true;
            }
            for child in &node.includes {
                // Mark on scheduling, not visitation: repeated edges and
                // diamonds cannot amplify the worklist with duplicate nodes.
                if scheduled.insert(Arc::as_ptr(child)) {
                    pending.push(child.as_ref());
                }
            }
        }
        false
    }
}

// The simple native value owns its complete Arc DAG; no Starlark heap reference
// is hidden from tracing/freezing. Allocative 0.3.6 Arc::visit uses enter_shared
// with Arc::as_ptr, deduplicating backing allocations across diamond edges.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct PackageSpecificationInfo {
        pub contents: Arc<GroupNode>,
    }
}
use owned::{Constructor, PackageSpecificationInfo};
starlark_simple_value!(Constructor);
starlark_simple_value!(PackageSpecificationInfo);

pub(crate) fn install(builder: &mut GlobalsBuilder) {
    builder.set("PackageSpecificationInfo", Constructor);
}

/// The same provider key, for `py_internal.PackageSpecificationInfo`.
pub(crate) fn alloc_frozen_constructor(
    heap: &starlark::values::FrozenHeap,
) -> starlark::values::FrozenValue {
    heap.alloc(Constructor)
}

pub(crate) fn is_constructor(value: Value<'_>) -> bool {
    Constructor::from_value(value).is_some()
}
pub(crate) fn is_instance(value: Value<'_>) -> bool {
    PackageSpecificationInfo::from_value(value).is_some()
}

pub(crate) fn alloc_info<'v>(heap: &Heap<'v>, contents: Arc<GroupNode>) -> Value<'v> {
    heap.alloc(PackageSpecificationInfo { contents })
}

impl fmt::Display for Constructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function PackageSpecificationInfo>")
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for Constructor {
    // Deliberately no invoke: the key is not a public factory.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_constructor(other))
    }
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

impl fmt::Display for PackageSpecificationInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("struct()")
    }
}

starlark::methods_static!(INFO_METHODS = info_methods);

#[starlark_module]
fn info_methods(builder: &mut MethodsBuilder) {
    fn contains(
        this: &PackageSpecificationInfo,
        #[starlark(require = pos)] target: Value<'_>,
    ) -> anyhow::Result<bool> {
        let mut budget = HostBudget::default();
        let package =
            if let Some((repository, package, _)) = crate::starlark_types::label_parts(target) {
                budget.string(repository).map_err(anyhow::Error::msg)?;
                budget.string(package).map_err(anyhow::Error::msg)?;
                PackageId {
                    repository: repository.to_owned(),
                    package: package.to_owned(),
                }
            } else if let Some(text) = target.unpack_str() {
                budget
                    .bytes(text.len().saturating_mul(2).saturating_add(1))
                    .map_err(anyhow::Error::msg)?;
                // Single @ here names a CANONICAL repository, never an apparent
                // alias. This also maps @// to the canonical main-repository form.
                let normalized;
                let text = if text.starts_with('@') && !text.starts_with("@@") {
                    normalized = format!("@{text}");
                    normalized.as_str()
                } else {
                    text
                };
                // Label.parseCanonical (PackageSpecificationProvider.java:103):
                // `@repo` alone is `@@repo//:repo`.
                let label = Label::parse_canonical(text).map_err(anyhow::Error::msg)?;
                PackageId {
                    repository: label.repository,
                    package: label.package,
                }
            } else {
                bail!(
                    "PackageSpecificationInfo.contains expects a Label or string, got {}",
                    target.get_type()
                );
            };
        Ok(this.contents.contains(&package))
    }
}

#[starlark_value(type = "PackageSpecificationInfo")]
impl<'v> StarlarkValue<'v> for PackageSpecificationInfo {
    fn get_methods() -> Option<&'static Methods> {
        Some(INFO_METHODS.methods())
    }
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_instance(other))
    }
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        // Upstream NativeInfo excludes methods (and membership data) from its
        // equality/hash fields. Never use this identity to canonicalize nodes.
        (PROVIDER_ID, "instance").hash(hasher);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starlark_types::{alloc_label, analysis_globals};
    use anyhow::anyhow;
    use starlark::environment::{FrozenModule, Globals, LibraryExtension, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    fn package(repository: &str, package: &str) -> PackageId {
        PackageId {
            repository: repository.into(),
            package: package.into(),
        }
    }

    #[test]
    fn group_contents_reserve_retained_copies_cumulatively() {
        let contents = GroupContents {
            positives: vec![Specification::Package {
                repository: String::new(),
                package: "p".repeat(1024 * 1024),
                subtree: false,
            }],
            negatives: vec![Specification::Private],
        };
        let mut budget = HostBudget::default();
        for _ in 0..7 {
            contents.charge(&mut budget).unwrap();
        }
        assert!(
            contents
                .charge(&mut budget)
                .unwrap_err()
                .contains("weighted byte limit")
        );
    }

    fn specs(values: &[&str], repository: &str) -> GroupContents {
        parse_specs(
            &values.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            &package(repository, "declaring"),
            &RepositorySet::new(".".into()),
        )
        .unwrap()
    }

    fn node(values: &[&str], children: Vec<Arc<GroupNode>>) -> Arc<GroupNode> {
        Arc::new(GroupNode::new(Arc::new(specs(values, "")), children).unwrap())
    }

    #[test]
    fn local_grammar_boundaries_negatives_and_repository_universes() {
        let group = specs(
            &[
                "//foo/...",
                "-//foo/private",
                "//root:__pkg__",
                "//tree:__subpackages__",
                "//",
            ],
            "ext+",
        );
        for p in [
            "",
            "foo",
            "foo/a",
            "foo/private/child",
            "root",
            "tree",
            "tree/a",
        ] {
            assert!(group.contains(&package("ext+", p)), "{p}");
            assert!(!group.contains(&package("", p)), "{p}");
        }
        for p in ["foobar", "foo/private", "root/child", "trees"] {
            assert!(!group.contains(&package("ext+", p)), "{p}");
        }
        let all = specs(&["//..."], "ext+");
        assert!(all.contains(&package("ext+", "any/path")));
        assert!(!all.contains(&package("", "any/path")));
        for repo in ["", "other+"] {
            assert!(specs(&["public"], "ext+").contains(&package(repo, "anything")));
            assert!(!specs(&["private"], "ext+").contains(&package(repo, "anything")));
            assert!(!specs(&["-//foo"], repo).contains(&package(repo, "foo")));
            assert!(!specs(&[], repo).contains(&package(repo, "foo")));
        }
        for values in [["//foo", "-//foo"], ["-//foo", "//foo"]] {
            assert!(!specs(&values, "").contains(&package("", "foo")));
        }
        // PackageSpecification's parser itself calls Label.parseWithRepoContext;
        // these two reserved package names therefore retain Label's main-repo
        // override even for specs declared in an external repository.
        for reserved in ["visibility", "conditions"] {
            for text in [format!("//{reserved}"), format!("//{reserved}:__pkg__")] {
                let group = specs(&[&text], "ext+");
                assert!(group.contains(&package("", reserved)));
                assert!(!group.contains(&package("ext+", reserved)));
            }
            let group = specs(&[&format!("@@ext+//{reserved}:__pkg__")], "ext+");
            assert!(group.contains(&package("ext+", reserved)));
            assert!(!group.contains(&package("", reserved)));
        }
    }

    #[test]
    fn invalid_grammar_and_explicit_native_literal_profile() {
        let repos = RepositorySet::new(".".into());
        for raw in [
            "",
            "foo",
            ":__pkg__",
            "-public",
            "-private",
            "--//foo",
            "//foo:target",
            "//visibility:public",
            "//foo/",
            "//foo//bar",
            "//foo/../bar",
            "//foo:__pkg__:x",
            "@bare",
        ] {
            assert!(
                parse_specs(&[raw.into()], &package("", "p"), &repos).is_err(),
                "{raw}"
            );
        }
        // LabelValidator.java:50-61: any printable ASCII except ':' and '\\'.
        for raw in ["//foo bar", "//foo/<", "//foo/**", "//(app)/tt@x/..."] {
            parse_specs(&[raw.into()], &package("", "p"), &repos).unwrap();
        }
        assert_eq!(
            parse_specs(&["//café".into()], &package("", "p"), &repos).unwrap_err(),
            "invalid package name 'café': package names may contain A-Z, a-z, 0-9, or any of ' !\"#$%&'()*+,-./;<=>?[]^_`{|}~' (any ASCII character except 0-31, 127, ':', or '\\')"
        );
        // All inputs are borrowed/precharged; neither a huge copy nor a huge
        // diagnostic is constructed before this limit rejects the payload.
        let huge = "a".repeat(8 * 1024 * 1024 + 1);
        assert!(
            parse_specs(&[huge], &package("", "p"), &repos)
                .unwrap_err()
                .contains("byte limit")
        );
        assert!(
            parse_specs(&vec!["private".into(); 100_001], &package("", "p"), &repos)
                .unwrap_err()
                .contains("node limit")
        );
    }

    #[test]
    fn declaring_repository_mapping_and_canonical_specs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dep")).unwrap();
        std::fs::write(root.path().join("MODULE.bazel"), "module(name='main')\nbazel_dep(name='dep',version='1.0',repo_name='alias')\nlocal_path_override(module_name='dep',path='dep')\n").unwrap();
        std::fs::write(
            root.path().join("dep/MODULE.bazel"),
            "module(name='dep',version='1.0')\n",
        )
        .unwrap();
        let repos = RepositorySet::new(root.path().into());
        let values = [
            "@alias//foo",
            "@@other+//tree/...",
            "@//",
            "@@//root:__subpackages__",
        ];
        let group = parse_specs(&values.map(String::from), &package("", "p"), &repos).unwrap();
        assert!(group.contains(&package("dep+", "foo")));
        assert!(!group.contains(&package("alias", "foo")));
        assert!(group.contains(&package("other+", "tree/a")));
        assert!(group.contains(&package("", "")));
        assert!(group.contains(&package("", "root/a")));
        // Within dep+, its own apparent name is dep, not main's alias.
        let own = parse_specs(
            &["@dep//foo".into(), "//bar".into()],
            &package("dep+", "p"),
            &repos,
        )
        .unwrap();
        assert!(own.contains(&package("dep+", "foo")));
        assert!(own.contains(&package("dep+", "bar")));
        assert!(parse_specs(&["@alias//foo".into()], &package("dep+", "p"), &repos).is_err());
    }

    #[test]
    fn independent_exclusions_union_and_diamond_depth_limit() {
        let leaf = node(&["//foo/private"], vec![]);
        let left = node(&["//foo/...", "-//foo/private"], vec![leaf.clone()]);
        let right = node(&["-//foo/private"], vec![leaf.clone()]);
        let root = node(&[], vec![left.clone(), right.clone(), left]);
        assert!(root.contains(&package("", "foo/private")));
        assert!(root.contains(&package("", "foo/other")));
        assert!(!right.own.contains(&package("", "foo/private")));
        assert!(!root.contains(&package("", "foobar")));
        let empty = Arc::new(specs(&[], ""));
        let mut cached = node(&[], vec![]);
        for _ in 1..MAX_PATH_NODES {
            cached = Arc::new(GroupNode::new(empty.clone(), vec![cached.clone(), cached]).unwrap());
        }
        assert_eq!(cached.height, MAX_PATH_NODES);
        assert!(format!("{cached:?}").len() < 150);
        assert!(!cached.contains(&package("", "absent")));
        assert!(
            GroupNode::new(empty.clone(), vec![cached.clone()])
                .unwrap_err()
                .contains("256")
        );
        assert!(GroupNode::new(empty, vec![leaf, cached]).is_err());
    }

    fn globals() -> Globals {
        GlobalsBuilder::extended_by(&[LibraryExtension::StructType])
            .with(analysis_globals)
            .with(install)
            .build()
    }

    fn evaluate(source: &str) -> anyhow::Result<FrozenModule> {
        Module::with_temp_heap(|module| {
            module.set(
                "x",
                alloc_info(&module.heap(), node(&["//foo", "@@ext+//allowed"], vec![])),
            );
            module.set("y", alloc_info(&module.heap(), node(&["//other"], vec![])));
            module.set(
                "typed",
                alloc_label(module.heap(), "@@ext+//allowed:target")?,
            );
            let ast = AstModule::parse("spec.bzl", source.to_owned(), &Dialect::Standard)
                .map_err(|e| anyhow!("{e}"))?;
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .map_err(|e| anyhow!("{e}"))?;
            module.freeze().map_err(|e| anyhow!("{e:?}"))
        })
    }

    #[test]
    fn provider_shape_equality_does_not_merge_membership_and_canonical_strings() {
        evaluate(r#"
(type(x) == "PackageSpecificationInfo" and repr(x) == "struct()" and dir(x) == ["contains"]) or fail("shape")
values = {x: "first"}
values[y] = "second"
(x == y and values[x] == "second" and len(values) == 1) or fail("upstream equality")
(x != PackageSpecificationInfo and PackageSpecificationInfo != x and x != struct()) or fail("distinct types")
(x.contains("//foo:x") and x.contains("//foo:other") and not y.contains("//foo:x")) or fail("independent data")
(y.contains("//other:x") and not x.contains("//other:x")) or fail("independent data")
(x.contains(typed) and x.contains("@ext+//allowed:x") and x.contains("@@ext+//allowed:x")) or fail("canonical repository")
(not x.contains("@alias//allowed:x") and not x.contains("//allowed:x")) or fail("no apparent or provider repository context")
(x.contains("@//foo:x") and x.contains("@@//foo:x") and x.contains(*["//foo:x"])) or fail("main and expanded positional")
(not x.contains("@bare") and not x.contains("@@bare") and x.contains("//foo:x y")) or fail("repository shorthand and full label alphabet")
"#).unwrap();
    }

    #[test]
    fn contains_binding_validation_and_immutable_native_values() {
        for source in [
            "x.contains()",
            "x.contains('//foo:x', '//foo:y')",
            "x.contains(target='//foo:x')",
            "x.contains('//foo:x', target='//foo:x')",
            "x.contains(unknown='//foo:x')",
            "x.contains(**{'target': '//foo:x'})",
            "x.contains(1)",
            "x.contains(None)",
            "x.contains([])",
            "x.contains(':x')",
            "x.contains('foo:x')",
            "x.contains('//foo:x\\\\y')",
            "x.contains = 1",
            "x.extra = 1",
            "x.missing",
            "PackageSpecificationInfo()",
        ] {
            assert!(evaluate(source).is_err(), "{source}");
        }
    }

    #[test]
    fn frozen_bound_method_owns_receiver_and_target_transport_is_native_only() {
        let original = evaluate("method = x.contains\nKey = PackageSpecificationInfo").unwrap();
        let method = original.get("method").unwrap();
        let instance = original.get("x").unwrap();
        let constructor = original.get("Key").unwrap();
        assert_eq!(
            crate::providers::instance_identity(instance.value()).as_deref(),
            Some(PROVIDER_ID)
        );
        assert_eq!(
            crate::providers::provider_identity(constructor.value()).as_deref(),
            Some(PROVIDER_ID)
        );
        assert!(crate::providers::provider_identity(instance.value()).is_none());
        assert!(crate::providers::instance_identity(constructor.value()).is_none());
        drop(original);
        let reowned = Module::with_temp_heap(|module| {
            module.set("method", module.heap().access_owned_frozen_value(&method));
            module.set(
                "target",
                crate::starlark_types::alloc_target_with_providers(
                    module.heap(),
                    "//groups:g",
                    vec![],
                    std::collections::BTreeMap::from([(PROVIDER_ID.into(), instance)]),
                ),
            );
            let ast = AstModule::parse("consumer.bzl", "(method('//foo:x') and not method('//other:x')) or fail('method owner')\n(PackageSpecificationInfo in target and DefaultInfo in target and target[DefaultInfo].files.to_list() == []) or fail('target')\nagain = target[PackageSpecificationInfo].contains\nagain('//foo:x') or fail('native transport')".into(), &Dialect::Standard).unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            module.freeze().unwrap()
        });
        drop(method);
        let again = reowned.get("again").unwrap();
        drop(reowned);
        Module::with_temp_heap(|module| {
            module.set("again", module.heap().access_owned_frozen_value(&again));
            let ast = AstModule::parse(
                "last.bzl",
                "again('//foo:x') or fail('reowned method')".into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
        });
    }
}
