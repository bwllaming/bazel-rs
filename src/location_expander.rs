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
/// (Label.toString), a set per label (mapGet). The set is kept as the
/// files in insertion order; a later file with an exec path already in
/// the set is a duplicate, dropped when the entry is read ([`apply`]) —
/// most entries are never read, and a dependency's file list can hold
/// thousands of files (a language SDK).
#[derive(Clone, Debug, Default)]
pub(crate) struct LocationMap(BTreeMap<String, Vec<File>>);

impl LocationMap {
    /// mapGet(map, label).addAll(files): an entry exists even when empty.
    pub(crate) fn add(&mut self, label: &str, files: impl IntoIterator<Item = File>) {
        match self.0.get_mut(label) {
            Some(entry) => entry.extend(files),
            None => {
                self.0.insert(label.to_owned(), files.into_iter().collect());
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
    apply_with(name, arg, exec_paths, parse, map, &[])
}

/// [`apply`] on `map` with `extra` added in order ([`LocationMap::add`] of
/// each `(label, files)`), without copying `map`: an entry is the map's
/// files followed by those of every `extra` entry of the same label.
pub(crate) fn apply_with(
    name: &str,
    arg: &str,
    exec_paths: bool,
    parse: &dyn Fn(&str) -> Result<Label, String>,
    map: &LocationMap,
    extra: &[(String, Vec<File>)],
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
    let key = label.to_string();
    let base = map.0.get(&key);
    let mut added = extra.iter().filter(|(label, _)| *label == key).peekable();
    if base.is_none() && added.peek().is_none() {
        return Some(Err(format!(
            "label '{label}' in {function_name} expression is not a declared prerequisite of this rule"
        )));
    }
    let files = base
        .into_iter()
        .flatten()
        .chain(added.flat_map(|(_, files)| files));
    // The set of the label's files: the first file per exec path.
    let mut seen = std::collections::HashSet::new();
    let paths = files
        .filter(|file| seen.insert(file.path.as_str()))
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

/// LabelExpander.expand (analysis/LabelExpander.java:63-150), genrule's
/// `heuristic_label_expansion`: every maximal run of label characters
/// (`[a-zA-Z0-9:/_.+-]`) that parses as a label in the rule's package and
/// names an entry of `map` (CommandHelper.getLabelMap: the `srcs` and tool
/// dependencies, never the outputs) is replaced by that entry's one exec
/// path, unquoted; anything else is kept. An entry with another number of
/// files is NotUniqueExpansionException's message.
pub(crate) fn expand_labels_heuristically(
    expression: &str,
    parse: &dyn Fn(&str) -> Result<Label, String>,
    map: &LocationMap,
) -> Result<String, String> {
    let is_label_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '_' | '.' | '-' | '+');
    let mut result = String::with_capacity(expression.len());
    let mut rest = expression;
    while let Some(c) = rest.chars().next() {
        let end = rest.find(|c| !is_label_char(c)).unwrap_or(rest.len());
        if end == 0 {
            result.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let text = &rest[..end];
        rest = &rest[end..];
        let Some(files) = parse(text)
            .ok()
            .and_then(|label| map.0.get(&label.to_string()))
        else {
            result.push_str(text);
            continue;
        };
        let paths = files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();
        match paths.first() {
            Some(path) if paths.len() == 1 => result.push_str(path),
            _ => {
                return Err(format!(
                    "heuristic label expansion found '{text}', which expands to {} files{}",
                    paths.len(),
                    if paths.len() > 1 {
                        format!(", please use $(locations {text}) instead")
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_label_expansion_follows_label_expander() {
        let root = Label::parse_canonical("//pkg:g").unwrap();
        let mut map = LocationMap::default();
        map.add("//pkg:a.txt", [file("pkg/a.txt", "pkg/a.txt")]);
        map.add(
            "//pkg:tool",
            [file("bazel-out/k8-opt-exec/bin/pkg/tool", "pkg/tool")],
        );
        map.add(
            "//pkg:two",
            [file("pkg/x", "pkg/x"), file("pkg/y", "pkg/y")],
        );
        map.add("//pkg:none", []);
        let mapping = BTreeMap::new();
        let parse = |arg: &str| Label::parse_in(arg, &root.repository, &root.package, &mapping);
        let run = |text: &str| expand_labels_heuristically(text, &parse, &map);
        assert_eq!(
            run(":tool a.txt >$@; cat //pkg:a.txt b.txt 'a.txt' /abs ${a.txt}"),
            Ok("bazel-out/k8-opt-exec/bin/pkg/tool pkg/a.txt >$@; cat pkg/a.txt b.txt 'pkg/a.txt' /abs ${pkg/a.txt}".to_owned())
        );
        assert_eq!(
            run("cat :two"),
            Err("heuristic label expansion found ':two', which expands to 2 files, please use $(locations :two) instead".to_owned())
        );
        assert_eq!(
            run("none"),
            Err("heuristic label expansion found 'none', which expands to 0 files".to_owned())
        );
        assert_eq!(run(""), Ok(String::new()));
    }

    fn file(path: &str, short_path: &str) -> File {
        File::new(crate::starlark_types::FileData {
            path: path.to_owned(),
            short_path: short_path.to_owned(),
            owner: String::new(),
            is_source: false,
            is_directory: false,
            is_symlink: false,
        })
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
    fn a_label_entry_keeps_the_first_file_of_each_exec_path() {
        let root = Label::parse_canonical("//pkg:g").unwrap();
        let mut map = LocationMap::default();
        // The same exec path added again (another attribute, the written
        // and the actual label of one dependency) with another short path:
        // the first file stands, as with the eager set it replaces.
        map.add("//pkg:a", [file("bazel-out/bin/pkg/a", "pkg/a")]);
        map.add(
            "//pkg:a",
            [
                file("bazel-out/bin/pkg/a", "other/a"),
                file("bazel-out/bin/pkg/b", "pkg/b"),
            ],
        );
        let mapping = BTreeMap::new();
        let parse = |arg: &str| Label::parse_in(arg, &root.repository, &root.package, &mapping);
        assert_eq!(
            apply("rootpaths", ":a", true, &parse, &map),
            Some(Ok("pkg/a pkg/b".to_owned()))
        );
        assert_eq!(
            apply("execpaths", ":a", true, &parse, &map),
            Some(Ok("bazel-out/bin/pkg/a bazel-out/bin/pkg/b".to_owned()))
        );
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
