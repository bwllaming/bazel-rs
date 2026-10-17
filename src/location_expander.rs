//! LocationExpander's location functions (analysis/LocationExpander.java)
//! shared by `ctx.expand_location` (LocationExpander.expand, :172-224) and
//! the genrule command, whose TemplateExpander resolves `$(name arg)` through
//! LocationTemplateContext.lookupFunction (analysis/LocationTemplateContext.java:100-120).

use crate::label::Label;
use crate::starlark_types::File;
use std::collections::{BTreeMap, BTreeSet};

/// LocationExpander.allLocationFunctions (:368-391), in Bazel's order.
pub(crate) const FUNCTIONS: [&str; 8] = [
    "location",
    "locations",
    "rootpath",
    "rootpaths",
    "execpath",
    "execpaths",
    "rlocationpath",
    "rlocationpaths",
];

/// The runfiles directory of the main repository under Bzlmod
/// (RuleContext.getWorkspaceName).
pub(crate) const MAIN_RUNFILES_DIRECTORY: &str = "_main";

/// LocationFunction.PathType (:241-246).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathType {
    Location,
    Exec,
    Rlocation,
}

/// The location map (buildLocationMap, :393-485): the files of each label
/// (Label.toString), a set per label (mapGet).
#[derive(Clone, Debug, Default)]
pub(crate) struct LocationMap(BTreeMap<String, Vec<File>>);

impl LocationMap {
    /// mapGet(map, label).addAll(files): an entry exists even when empty.
    pub(crate) fn add(&mut self, label: &str, files: impl IntoIterator<Item = File>) {
        let entry = self.0.entry(label.to_owned()).or_default();
        for file in files {
            if !entry.iter().any(|seen| seen.path == file.path) {
                entry.push(file);
            }
        }
    }
}

/// The function `name` with `execPaths` (`$(location)` is an exec path for
/// genrules and `ctx.expand_location`): its path type and whether it allows
/// several files.
fn function(name: &str, exec_paths: bool) -> Option<(PathType, bool)> {
    let location = if exec_paths {
        PathType::Exec
    } else {
        PathType::Location
    };
    Some(match name {
        "location" => (location, false),
        "locations" => (location, true),
        "rootpath" => (PathType::Location, false),
        "rootpaths" => (PathType::Location, true),
        "execpath" => (PathType::Exec, false),
        "execpaths" => (PathType::Exec, true),
        "rlocationpath" => (PathType::Rlocation, false),
        "rlocationpaths" => (PathType::Rlocation, true),
        _ => return None,
    })
}

pub(crate) fn is_function(name: &str) -> bool {
    FUNCTIONS.contains(&name)
}

/// PathFragment.getCallablePathString (vfs/PathFragment.java:735-745).
fn callable(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else if path.is_empty() {
        ".".to_owned()
    } else if !path.contains('/') {
        format!("./{path}")
    } else {
        path.to_owned()
    }
}

/// LocationFunction.getPath (:338-351): Artifact.getRunfilesPath is the short
/// path (`../repo/...` for external repositories).
fn path(file: &File, path_type: PathType) -> String {
    match path_type {
        PathType::Exec => file.path.clone(),
        PathType::Location => file.short_path.clone(),
        PathType::Rlocation => match file.short_path.strip_prefix("../") {
            Some(external) => external.to_owned(),
            None => format!("{MAIN_RUNFILES_DIRECTORY}/{}", file.short_path),
        },
    }
}

/// LocationFunction.apply (:273-290) and resolveLabel (:293-325); the
/// IllegalStateException message on failure. `parse` is
/// Label.parseWithPackageContext in the rule's package and repository
/// mapping. `None` when `name` is not a location function.
pub(crate) fn apply(
    name: &str,
    arg: &str,
    exec_paths: bool,
    parse: &dyn Fn(&str) -> Result<Label, String>,
    map: &LocationMap,
) -> Option<Result<String, String>> {
    let (path_type, multiple) = function(name, exec_paths)?;
    let function_name = if multiple {
        "$(locations)"
    } else {
        "$(location)"
    };
    let label = match parse(arg) {
        Ok(label) => label,
        Err(error) => {
            return Some(Err(format!(
                "invalid label in {function_name} expression: {error}"
            )));
        }
    };
    let Some(files) = map.0.get(&label.to_string()) else {
        return Some(Err(format!(
            "label '{label}' in {function_name} expression is not a declared prerequisite of this rule"
        )));
    };
    let paths = files
        .iter()
        .map(|file| callable(&path(file, path_type)))
        .collect::<BTreeSet<_>>();
    if paths.is_empty() {
        return Some(Err(format!(
            "label '{label}' in {function_name} expression expands to no files"
        )));
    }
    if !multiple && paths.len() > 1 {
        return Some(Err(format!(
            "label '{label}' in $(location) expression expands to more than one file, please use $(locations {label}) instead.  Files (at most 5 shown) are: [{}]",
            paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    // joinPaths (:360-362): ShellEscaper.escapeString per path.
    Some(Ok(paths
        .iter()
        .map(|path| crate::args::shell_escape(path))
        .collect::<Vec<_>>()
        .join(" ")))
}

/// LocationExpander.expand (:172-224): only `$(FUNCTION ` prefixes of a
/// location function are replaced; everything else, `$$` included, is kept.
/// An error is the reporter's message (the input is then returned by the
/// caller).
pub(crate) fn expand(
    value: &str,
    resolve: &dyn Fn(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let mut restart = 0;
    let mut result = String::new();
    loop {
        let Some(start) = value[restart..].find("$(").map(|index| index + restart) else {
            result.push_str(&value[restart..]);
            break;
        };
        let Some(space) = value[start..].find(' ').map(|index| index + start) else {
            result.push_str(&value[restart..start + 2]);
            restart = start + 2;
            continue;
        };
        let name = &value[start + 2..space];
        if !is_function(name) {
            result.push_str(&value[restart..start + 2]);
            restart = start + 2;
            continue;
        }
        result.push_str(&value[restart..start]);
        let Some(end) = value[space..].find(')').map(|index| index + space) else {
            return Err(format!("unterminated $({name}) expression"));
        };
        result.push_str(&resolve(name, value[space + 1..end].trim())?);
        restart = end + 1;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, short_path: &str) -> File {
        File {
            path: path.to_owned(),
            short_path: short_path.to_owned(),
            owner: String::new(),
            is_source: false,
            is_directory: false,
        }
    }

    #[test]
    fn functions_follow_location_function() {
        let root = Label::parse_canonical("//pkg:g").unwrap();
        let mut map = LocationMap::default();
        map.add("//pkg:a", [file("pkg/a", "pkg/a")]);
        map.add("//:top", [file("top", "top")]);
        map.add(
            "@@r+//x:b",
            [
                file("bazel-out/k8-fastbuild/bin/external/r+/x/b2", "../r+/x/b2"),
                file("bazel-out/k8-fastbuild/bin/external/r+/x/b1", "../r+/x/b1"),
            ],
        );
        map.add("//pkg:none", []);
        let mapping = BTreeMap::from([("r".to_owned(), "r+".to_owned())]);
        let parse = |arg: &str| Label::parse_in(arg, &root.repository, &root.package, &mapping);
        let run = |name: &str, arg: &str| apply(name, arg, true, &parse, &map).unwrap();
        assert_eq!(run("location", ":a"), Ok("pkg/a".to_owned()));
        assert_eq!(run("execpath", "//:top"), Ok("./top".to_owned()));
        assert_eq!(run("rlocationpath", "a"), Ok("_main/pkg/a".to_owned()));
        assert_eq!(
            run("rlocationpaths", "@r//x:b"),
            Ok("r+/x/b1 r+/x/b2".to_owned())
        );
        assert_eq!(
            run("rootpaths", "@r//x:b"),
            Ok("../r+/x/b1 ../r+/x/b2".to_owned())
        );
        assert_eq!(
            run("execpath", "@r//x:b"),
            Err("label '@@r+//x:b' in $(location) expression expands to more than one file, please use $(locations @@r+//x:b) instead.  Files (at most 5 shown) are: [bazel-out/k8-fastbuild/bin/external/r+/x/b1, bazel-out/k8-fastbuild/bin/external/r+/x/b2]".to_owned())
        );
        assert_eq!(
            run("locations", ":c"),
            Err("label '//pkg:c' in $(locations) expression is not a declared prerequisite of this rule".to_owned())
        );
        assert_eq!(
            run("execpaths", ":none"),
            Err("label '//pkg:none' in $(locations) expression expands to no files".to_owned())
        );
        assert!(apply("location", ":a", false, &parse, &map) == Some(Ok("pkg/a".into())));
        assert!(apply("nope", ":a", true, &parse, &map).is_none());
    }

    #[test]
    fn expand_replaces_only_location_functions() {
        let resolve = |name: &str, arg: &str| Ok(format!("<{name}|{arg}>"));
        assert_eq!(
            expand(
                "a $$ $(FOO) $(location  :x ) $(execpath:y) $(rootpath z)",
                &resolve
            ),
            Ok("a $$ $(FOO) <location|:x> $(execpath:y) <rootpath|z>".to_owned())
        );
        assert_eq!(
            expand("$(location :x", &resolve),
            Err("unterminated $(location) expression".to_owned())
        );
    }
}
