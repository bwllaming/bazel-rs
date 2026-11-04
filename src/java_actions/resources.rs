//! JavaCompilationHelper.separateResourceJar and ResourceJarActionBuilder.
use super::*;

/// BazelJavaSemantics.getDefaultJavaResourcePath / JavaUtil.javaSegmentIndex.
fn default_path(path: &str) -> String {
    let segments: Vec<_> = path.split('/').collect();
    for i in 0..segments.len().saturating_sub(2) {
        if segments[i] == "src" && segments[i + 2] == "resources" {
            return segments[i + 3..].join("/");
        }
    }
    let Some(mut root) = segments
        .iter()
        .position(|s| matches!(*s, "java" | "javatests" | "src" | "testsrc"))
    else {
        return path.to_owned();
    };
    let is_src = segments[root] == "src";
    let mut maven = is_src.then_some(root);
    if root == 0 || is_src {
        for i in root + 1..segments.len().saturating_sub(1) {
            if segments[i] == "src" || (is_src && matches!(segments[i], "java" | "javatests")) {
                if matches!(segments[i + 1], "com" | "org" | "net") {
                    root = i;
                } else if segments[i] == "src" {
                    maven = Some(i);
                }
                break;
            }
        }
    }
    if let Some(i) = maven
        && i + 2 < segments.len()
        && matches!(segments[i + 1], "main" | "test")
        && matches!(segments[i + 2], "java" | "resources")
    {
        root = i + 2;
    }
    segments[root + 1..].join("/")
}

fn resource_path(
    resource: &File,
    repository: &str,
    prefix: Option<&str>,
) -> starlark::Result<String> {
    let mut path = crate::runfiles::root_relative(resource);
    let repo_prefix = format!("external/{repository}/");
    if !repository.is_empty()
        && let Some(relative) = path.strip_prefix(&repo_prefix)
    {
        path = relative;
    }
    match prefix {
        None => Ok(default_path(path)),
        Some(prefix) => {
            let prefix = crate::action::unresolved_link_text(prefix);
            if prefix.is_empty() {
                return Ok(path.to_owned());
            }
            if path == prefix {
                return Ok(String::new());
            }
            path.strip_prefix(&format!("{prefix}/")).map(str::to_owned).ok_or_else(|| failure(format!("in resource_strip_prefix attribute: Resource file '{path}' is not under the specified prefix to strip")))
        }
    }
}

fn class_jar(context: &Context, output: &File) -> starlark::Result<File> {
    let marker = context
        .label
        .with_name("__java_resource_marker__")
        .map_err(failure)?
        .generated_exec_path(&context.output_root);
    let prefix = format!(
        "{}/",
        marker.parent().expect("generated package root").display()
    );
    let name = output.path.strip_prefix(&prefix).ok_or_else(|| {
        unsupported_for(
            "create_compilation_action",
            "resource output outside the declaring package",
        )
    })?;
    let (dir, base) = name
        .rsplit_once('/')
        .map_or(("", name), |(dir, base)| (dir, base));
    let name = match base.rsplit_once('.') {
        Some((base, ext)) => format!("{base}-class.{ext}"),
        None => format!("{base}-class"),
    };
    context
        .declare(&if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        })
        .map_err(starlark::Error::new_other)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn register<'v>(
    context: &Context,
    ctx: Value<'v>,
    toolchain: Value<'v>,
    output: &File,
    resources: &[File],
    (resource_jars, resource_jars_node): (&[File], Option<Arc<DepsetNode>>),
    classpath_resources: &[File],
    additional_inputs: &[File],
    group: &str,
    heap: Heap<'v>,
) -> starlark::Result<File> {
    let class_jar = class_jar(context, output)?;
    let surface = crate::analysis_ctx::current_surface(ctx).map_err(starlark::Error::new_other)?;
    let prefix = if surface.explicit_resource_strip_prefix {
        let attrs = field(ctx, "attr", heap)?;
        let value = field(attrs, "resource_strip_prefix", heap)?;
        Some(value.unpack_str().ok_or_else(|| {
            type_error(
                "create_compilation_action",
                "resource_strip_prefix",
                value,
                "string",
            )
        })?)
    } else {
        None
    };
    // LinkedHashMap: duplicate destinations keep their position, last file wins.
    let mut entries: Vec<(String, File)> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for resource in resources {
        let path = resource_path(resource, &context.label.repository, prefix)?;
        if let Some(&i) = index.get(&path) {
            entries[i] = (path, resource.clone());
        } else {
            index.insert(path.clone(), entries.len());
            entries.push((path, resource.clone()));
        }
    }
    let jars = unique(resource_jars.iter().cloned().chain([class_jar.clone()]));
    let runner = field(toolchain, "single_jar", heap)?;
    let runner = runner
        .downcast_ref::<crate::runfiles::FilesToRun>()
        .ok_or_else(|| {
            type_error(
                "create_compilation_action",
                "single_jar",
                runner,
                "FilesToRunProvider",
            )
        })?;
    let executable = runner
        .executable()
        .ok_or_else(|| failure("Java single_jar tool has no executable".into()))?;
    // ResourceJarActionBuilder: the resource jars (a stable-order set of
    // the attribute's) plus the class jar.
    let attribute = command::build(
        Vec::new(),
        resource_jars_node.into_iter().collect(),
        crate::starlark_types::Order::Default,
        true,
    );
    let jars_node = command::build(
        vec![class_jar.clone()],
        vec![Arc::clone(&attribute)],
        attribute.order,
        true,
    );
    let mut line = command::Line::default();
    line.add("--normalize")
        .add("--dont_change_compression")
        .add("--exclude_build_data")
        .add("--no_strip_module_info")
        .add_opt("--output", Some(&output.path))
        .add_set("--sources", &jars_node);
    if !entries.is_empty() {
        line.add("--resources");
        for (path, resource) in &entries {
            line.add(if path == &resource.path {
                resource.path.clone()
            } else {
                format!("{}:{path}", resource.path)
            });
        }
    }
    line.add_paths("--classpath_resources", classpath_resources);
    let flags = line.expand();
    // SpawnAction.ensureCallable: PathFragment.getCallablePathString.
    let callable = if executable.path.contains('/') {
        executable.path.clone()
    } else {
        format!("./{}", executable.path)
    };
    let meta = crate::aquery::capturing().then(|| {
        let mut key_info: BTreeMap<String, String> = context
            .propagated_tags
            .iter()
            .map(|tag| (tag.clone(), String::new()))
            .collect();
        key_info.insert("supports-path-mapping".into(), "1".into());
        let key_info: Vec<(String, String)> = key_info.into_iter().collect();
        let mut executable_line = command::Line::default();
        executable_line.add(callable.clone());
        let mut arguments = vec![callable.clone()];
        arguments.extend(flags.iter().cloned());
        crate::aquery::Meta {
            progress: Some("Building Java resource jar".into()),
            arguments: Some(arguments),
            compute_key: Some(command::spawn_key(
                &[
                    (&executable_line, None),
                    (
                        &line,
                        Some(command::ParamKey {
                            file_type: "SHELL_QUOTED",
                        }),
                    ),
                ],
                "JavaResourceJar",
                &command::KeyEnvironment::of(context, &[], true),
                &key_info,
            )),
            ..Default::default()
        }
    });
    let spec = CommandLineSpec {
        executable: vec![callable.clone()],
        parts: vec![Part {
            groups: vec![Group::Strings(flags)],
            flag_per_line: false,
            param: Some(ParamFileInfo {
                flag_format: "@%s".into(),
                always: false,
                file_type: ParamFileType::Shell,
                flags_only: false,
            }),
        }],
        primary_output: output.path.clone(),
        max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
    };
    let (arguments, param_files) = spec.expand(None).map_err(failure)?;
    let inputs = unique(
        entries
            .into_iter()
            .map(|(_, f)| f)
            .chain(jars)
            .chain(additional_inputs.iter().cloned())
            .chain(classpath_resources.iter().cloned())
            .chain(runner.files().iter().cloned()),
    );
    let environment = context.action_env.for_spawn(true, BTreeMap::new());
    context
        .register_action(Registration {
            outputs: vec![output.clone()],
            inputs,
            command: String::new(),
            write: None,
            program: Some(callable.clone()),
            arguments,
            environment: environment.fixed,
            extra: crate::action::ActionExtra {
                mnemonic: "JavaResourceJar".into(),
                param_files,
                inherited_environment: environment.inherited,
                execution_info: BTreeMap::from([("supports-path-mapping".into(), "1".into())]),
                ..Default::default()
            },
            shell_pad: false,
            executable_file: true,
            deferred: None,
            exec_group: group.to_owned(),
            runfiles_trees: crate::analysis_runtime::tool_runfiles_tree(runner)
                .into_iter()
                .collect(),
            build_info: None,
            shareable: true,
        })
        .map_err(starlark::Error::new_other)?;
    if let Some(meta) = meta {
        super::attach_meta(context, output, meta);
    }
    Ok(class_jar)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_roots_follow_java_util_and_maven_heuristics() {
        for (input, expected) in [
            ("pkg/data.txt", "pkg/data.txt"),
            ("pkg/java/com/a.txt", "com/a.txt"),
            ("src/main/resources/a.txt", "a.txt"),
            ("pkg/src/other/resources/a.txt", "a.txt"),
            ("src/some/java/org/a.txt", "org/a.txt"),
            ("java/some/src/main/java/org/a.txt", "org/a.txt"),
            ("testsrc/a.txt", "a.txt"),
            ("java/some/src/no/a.txt", "some/src/no/a.txt"),
        ] {
            assert_eq!(default_path(input), expected, "{input}");
        }
        let file = File::new(crate::starlark_types::FileData {
            path: "external/repo/pkg/java/x.txt".into(),
            short_path: "../repo/pkg/java/x.txt".into(),
            owner: "@@repo//pkg:java/x.txt".into(),
            is_source: true,
            is_directory: false,
            is_symlink: false,
        });
        assert_eq!(resource_path(&file, "repo", None).unwrap(), "x.txt");
        assert_eq!(
            resource_path(&file, "repo", Some("")).unwrap(),
            "pkg/java/x.txt"
        );
        assert_eq!(
            resource_path(&file, "repo", Some("pkg/./java/")).unwrap(),
            "x.txt"
        );
        assert!(resource_path(&file, "repo", Some("other")).is_err());
    }
}
