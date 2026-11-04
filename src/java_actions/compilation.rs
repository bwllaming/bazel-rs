//! JavaStarlarkCommon.createCompilationAction and JavaCompileActionBuilder.
//! The selected JavaBuilder executes the compiler; this layer only constructs
//! the genuine flags, artifacts and reduced/fallback execution policy.
use super::*;
use crate::java_compile::{ClasspathMode, JavaCompileSpec};

const COMPILE: &str = "create_compilation_action";

fn unsupported(branch: &str) -> starlark::Error {
    unsupported_for(COMPILE, branch)
}
fn files(value: Value<'_>, name: &str, depset: bool) -> starlark::Result<Vec<File>> {
    files_for(COMPILE, value, name, depset)
}
fn strings(value: Value<'_>, name: &str) -> starlark::Result<Vec<String>> {
    strings_for(COMPILE, value, name)
}
fn file(value: Value<'_>, name: &str) -> starlark::Result<File> {
    file_for(COMPILE, value, name)
}

struct Boot {
    jars: Vec<File>,
    jars_node: Arc<DepsetNode>,
    auxiliary: Vec<File>,
    auxiliary_node: Arc<DepsetNode>,
    system_inputs: Vec<File>,
    system_path: Option<String>,
}
impl Boot {
    fn read<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<Self> {
        provider(
            value,
            "java/private:boot_class_path_info.bzl",
            "BootClassPathInfo",
        )?;
        let path = field(value, "_system_path", heap)?;
        let jars = field(value, "bootclasspath", heap)?;
        let auxiliary = field(value, "_auxiliary", heap)?;
        Ok(Self {
            jars: files(jars, "bootclasspath", true)?,
            jars_node: command::node(jars).expect("checked depset"),
            auxiliary: files(auxiliary, "_auxiliary", true)?,
            auxiliary_node: command::node(auxiliary).expect("checked depset"),
            system_inputs: files(
                field(value, "_system_inputs", heap)?,
                "_system_inputs",
                true,
            )?,
            system_path: if path.is_none() {
                None
            } else {
                Some(
                    path.unpack_str()
                        .ok_or_else(|| {
                            type_error(COMPILE, "_system_path", path, "string or NoneType")
                        })?
                        .to_owned(),
                )
            },
        })
    }
    fn empty(&self) -> bool {
        self.jars.is_empty()
            && self.auxiliary.is_empty()
            && self.system_inputs.is_empty()
            && self.system_path.is_none()
    }
}

pub(crate) fn create_compilation<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let names = [
        "ctx",
        "java_toolchain",
        "output",
        "manifest_proto",
        "plugin_info",
        "compilation_classpath",
        "direct_jars",
        "bootclasspath",
        "javabuilder_jvm_flags",
        "compile_time_java_deps",
        "javac_opts",
        "strict_deps_mode",
        "target_label",
        "deps_proto",
        "gen_class",
        "gen_source",
        "native_header_jar",
        "sources",
        "source_jars",
        "resources",
        "resource_jars",
        "classpath_resources",
        "sourcepath",
        "injecting_rule_kind",
        "enable_jspecify",
        "enable_direct_classpath",
        "additional_inputs",
        "additional_outputs",
        "extra_args",
    ];
    let params: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| param(name, i != 28, i >= 13, i < 13))
        .collect();
    let bound = bind(COMPILE, &params, args, heap)?;
    let value = |i: usize| bound[i].unwrap_or_else(Value::new_none);
    let context = crate::analysis_runtime::context(eval).map_err(starlark::Error::new_other)?;
    let group = execution_group(value(0), context, heap)?;
    let surface =
        crate::analysis_ctx::current_surface(value(0)).map_err(starlark::Error::new_other)?;
    let toolchain = value(1);
    provider(
        toolchain,
        "java/common/rules:java_toolchain.bzl",
        "JavaToolchainInfo",
    )?;
    let mut output = file(value(2), "output")?;
    let manifest = file(value(3), "manifest_proto")?;
    let (plugins, _) = plugin_info(COMPILE, value(4), heap)?;
    let naive = crate::starlark_types::Order::Preorder;
    let attribute = |value: Value<'v>, order| {
        command::build(
            Vec::new(),
            command::node(value).into_iter().collect(),
            order,
            true,
        )
    };
    files(value(5), "compilation_classpath", true)?;
    files(value(6), "direct_jars", true)?;
    // JavaTargetAttributes.build: naive-link builders over the given sets.
    let mut classpath_node = attribute(value(5), naive);
    let mut direct_node = attribute(value(6), naive);
    let mut boot = if value(7).is_none() {
        None
    } else {
        Some(Boot::read(value(7), heap)?)
    };
    if boot.as_ref().is_none_or(Boot::empty) {
        boot = Some(Boot::read(
            field(toolchain, "_bootclasspath_info", heap)?,
            heap,
        )?);
    }
    let boot = boot.expect("toolchain bootclasspath");
    if !boot.auxiliary.is_empty() {
        classpath_node = command::build(
            Vec::new(),
            vec![Arc::clone(&boot.auxiliary_node), classpath_node],
            naive,
            true,
        );
        direct_node = command::build(
            Vec::new(),
            vec![Arc::clone(&boot.auxiliary_node), direct_node],
            naive,
            true,
        );
    }
    let jvm_flags_value = value(8);
    strings(jvm_flags_value, "javabuilder_jvm_flags")?;
    let jvm_flags = command::node(jvm_flags_value).expect("checked depset");
    files(value(9), "compile_time_java_deps", true)?;
    let mut dependency_node = attribute(value(9), crate::starlark_types::Order::Default);
    let options = javac_options(strings(value(10), "javac_opts")?);
    let strict = value(11)
        .unpack_str()
        .ok_or_else(|| type_error(COMPILE, "strict_deps_mode", value(11), "string"))?
        .to_ascii_uppercase();
    let strict = match strict.as_str() {
        "OFF" => "OFF",
        "DEFAULT" | "ERROR" => "ERROR",
        "WARN" => "WARN",
        _ => {
            return Err(failure(format!(
                "StrictDepsMode {strict} not allowed. Only OFF and ERROR values are accepted."
            )));
        }
    };
    if strict == "OFF" {
        direct_node = command::empty(naive);
        dependency_node = command::empty(crate::starlark_types::Order::Default);
    }
    let target = crate::starlark_types::label_command_line_form(value(12), eval)
        .ok_or_else(|| type_error(COMPILE, "target_label", value(12), "Label"))?;
    let optional_file = |i, name| {
        if value(i).is_none() {
            Ok(None)
        } else {
            file(value(i), name).map(Some)
        }
    };
    let jdeps = optional_file(13, "deps_proto")?;
    let gen_class = optional_file(14, "gen_class")?;
    let gen_source = optional_file(15, "gen_source")?;
    let native_header = optional_file(16, "native_header_jar")?;
    let sources = if value(17).is_none() {
        Vec::new()
    } else {
        unique(
            files(value(17), "sources", true)?
                .into_iter()
                .filter(|f| f.path.ends_with(".java")),
        )
    };
    let sequence_files = |i, name| match bound[i] {
        Some(value) => files(value, name, false),
        None => Ok(Vec::new()),
    };
    let source_jars = sequence_files(18, "source_jars")?;
    let resources = sequence_files(19, "resources")?;
    let (resource_jars, resource_jars_node) = if value(20).is_none() {
        (Vec::new(), None)
    } else {
        (
            files(value(20), "resource_jars", true)?,
            command::node(value(20)),
        )
    };
    let classpath_resources = sequence_files(21, "classpath_resources")?;
    let sourcepath = sequence_files(22, "sourcepath")?;
    let injecting = if value(23).is_none() {
        None
    } else {
        Some(value(23).unpack_str().ok_or_else(|| {
            type_error(
                COMPILE,
                "injecting_rule_kind",
                value(23),
                "string or NoneType",
            )
        })?)
    };
    let boolean = |i: usize, name| {
        bound[i]
            .unwrap_or_else(|| Value::new_bool(true))
            .unpack_bool()
            .ok_or_else(|| type_error(COMPILE, name, value(i), "bool"))
    };
    let jspecify = boolean(24, "enable_jspecify")?;
    boolean(25, "enable_direct_classpath")?; // Only affects turbine annotation processing.
    let additional = sequence_files(26, "additional_inputs")?;
    let additional_outputs = unique(sequence_files(27, "additional_outputs")?);
    if let Some(extra) = bound[28] {
        let extra = sequence(COMPILE, "extra_args", extra)?;
        for arg in &extra {
            if crate::args::snapshot(*arg)
                .map_err(starlark::Error::new_other)?
                .is_none()
            {
                return Err(type_error(COMPILE, "extra_args element", *arg, "Args"));
            }
        }
        if !extra.is_empty() {
            return Err(unsupported("extra_args"));
        }
    }
    if jspecify && !field(toolchain, "_jspecify_info", heap)?.is_none() {
        return Err(unsupported("JSpecify toolchain processors"));
    }
    if !sequence(
        COMPILE,
        "_package_configuration",
        field(toolchain, "_package_configuration", heap)?,
    )?
    .is_empty()
    {
        return Err(unsupported("per-package toolchain data"));
    }
    if surface.config.coverage_enabled && !sources.is_empty() {
        return Err(unsupported("Java coverage instrumentation"));
    }
    let incompatible = strings(
        field(
            toolchain,
            "_reduced_classpath_incompatible_processors",
            heap,
        )?,
        "_reduced_classpath_incompatible_processors",
    )?;
    let runtime = field(toolchain, "java_runtime", heap)?;
    provider(
        runtime,
        "java/common/rules:java_runtime.bzl",
        "JavaRuntimeInfo",
    )?;
    let runtime_inputs = files(
        field(runtime, "files", heap)?,
        "JavaRuntimeInfo.files",
        true,
    )?;
    let tools = files(field(toolchain, "tools", heap)?, "tools", true)?;
    let tool = field(toolchain, "_javabuilder", heap)?;
    if tool.is_none() {
        return Err(failure("JavaToolchainInfo has no JavaBuilder tool".into()));
    }
    let (executable, tool_inputs) = tool_command(COMPILE, tool, toolchain, Some(&jvm_flags), heap)?;
    if let Some(gen_class) = &gen_class {
        // JavaCompilationHelper.createGenJarAction, over the requested
        // (not the resource-split class) output.
        gen_jar_action(
            context, toolchain, &output, &manifest, gen_class, &group, heap,
        )?;
    }
    if !resources.is_empty() || !resource_jars.is_empty() || !classpath_resources.is_empty() {
        output = super::resources::register(
            context,
            value(0),
            toolchain,
            &output,
            &resources,
            (&resource_jars, resource_jars_node),
            &classpath_resources,
            &additional,
            &group,
            heap,
        )?;
    }
    let mandatory = unique(
        plugins
            .jars
            .iter()
            .cloned()
            .chain(plugins.data.iter().cloned())
            .chain(source_jars.iter().cloned())
            .chain(sources.iter().cloned())
            .chain(runtime_inputs)
            .chain(boot.jars.iter().cloned())
            .chain(sourcepath.iter().cloned())
            .chain(additional)
            .chain(boot.system_inputs)
            .chain(tool_inputs)
            .chain(tools),
    );
    let flat = |node: &Arc<DepsetNode>| -> Vec<File> {
        Depset {
            node: Arc::clone(node),
        }
        .flatten()
        .into_iter()
        .filter_map(|item| match item {
            Item::File(file) => Some(file.clone()),
            _ => None,
        })
        .collect()
    };
    let classpath = flat(&classpath_node);
    let direct = flat(&direct_node);
    // JavaCompileActionBuilder.build: reduced-classpath-incompatible
    // processors, strict deps OFF and an absent jdeps select OFF.
    let bazel = !plugins
        .classes
        .iter()
        .any(|class| incompatible.contains(class))
        && strict != "OFF"
        && jdeps.is_some();
    if !bazel {
        dependency_node = command::empty(crate::starlark_types::Order::Default);
    }
    let dependency_jdeps = flat(&dependency_node);
    let inputs = unique(
        mandatory
            .iter()
            .cloned()
            .chain(classpath.iter().cloned())
            .chain(dependency_jdeps.iter().cloned()),
    );
    let mut outputs = vec![output.clone()];
    outputs.extend(additional_outputs);
    outputs.extend(jdeps.iter().cloned());
    outputs.extend(native_header.iter().cloned());
    outputs.extend(gen_source.iter().cloned());
    outputs.push(manifest.clone());
    let outputs = unique(outputs);
    let mut flags = command::Line::default();
    flags
        .add_opt("--output", Some(&output.path))
        .add_opt(
            "--native_header_output",
            native_header.as_ref().map(|f| f.path.as_str()),
        )
        .add_opt(
            "--generated_sources_output",
            gen_source.as_ref().map(|f| f.path.as_str()),
        )
        .add_opt("--output_manifest_proto", Some(&manifest.path))
        .add("--compress_jar")
        .add_opt(
            "--output_deps_proto",
            jdeps.as_ref().map(|f| f.path.as_str()),
        )
        .add_set("--bootclasspath", &boot.jars_node)
        .add_opt("--system", boot.system_path.as_deref())
        .add_paths("--sourcepath", &sourcepath)
        .add_set("--processorpath", &plugins.jars_node)
        .add_set("--processors", &plugins.classes_node)
        .add_paths("--source_jars", &source_jars)
        .add_paths("--sources", &sources);
    if !options.is_empty() {
        flags.add("--javacopts");
        for option in &options {
            flags.add(option.clone());
        }
        flags.add("--");
    }
    flags
        .add_target_label(&target)
        .add_opt("--injecting_rule_kind", injecting);
    if strict != "OFF" {
        flags
            .add("--strict_java_deps")
            .add(strict)
            .add_set("--direct_dependencies", &direct_node);
    }
    flags.add("--experimental_fix_deps_tool").add("add_dep");
    // JavaCompilationHelper.getExecutionInfo, in its order, then the rule's
    // propagated tags.
    let mut key_info = vec![("supports-path-mapping".to_owned(), "1".to_owned())];
    for (field_name, requirement) in [
        ("_javac_supports_workers", "supports-workers"),
        (
            "_javac_supports_multiplex_workers",
            "supports-multiplex-workers",
        ),
        (
            "_javac_supports_worker_cancellation",
            "supports-worker-cancellation",
        ),
        (
            "_javac_supports_worker_multiplex_sandboxing",
            "supports-multiplex-sandboxing",
        ),
    ] {
        let enabled = field(toolchain, field_name, heap)?;
        if enabled
            .unpack_bool()
            .ok_or_else(|| type_error(COMPILE, field_name, enabled, "bool"))?
        {
            key_info.push((requirement.into(), "1".into()));
        }
    }
    for tag in &context.propagated_tags {
        if !key_info.iter().any(|(key, _)| key == tag) {
            key_info.push((tag.clone(), String::new()));
        }
    }
    let mut execution_info: BTreeMap<String, String> = key_info
        .iter()
        .filter(|(key, _)| !context.propagated_tags.contains(key))
        .cloned()
        .collect();
    if let Some(jdeps) = &jdeps {
        execution_info.insert("internal-inline-outputs".into(), jdeps.path.clone());
    }
    let locale = utf8_locale(context, &group)?;
    let environment = context
        .action_env
        .for_spawn(true, BTreeMap::from([("LC_CTYPE".into(), locale.into())]));
    let meta = crate::aquery::capturing().then(|| {
        let key_environment = command::KeyEnvironment::of(context, &[("LC_CTYPE", locale)], true);
        let mut arguments = executable.expand();
        arguments.extend(flags.expand());
        let mut classpath_line = command::Line::default();
        classpath_line.add_set("--classpath", &classpath_node);
        arguments.extend(classpath_line.expand());
        let mut display_info = key_info.clone();
        if let Some(jdeps) = &jdeps {
            display_info.push(("internal-inline-outputs".to_owned(), jdeps.path.clone()));
        }
        crate::aquery::Meta {
            progress: Some(command::progress(
                "Building",
                sources.len(),
                source_jars.len(),
                &plugins.classes,
            )),
            arguments: Some(arguments),
            compute_key: Some(command::java_compile_key(
                if bazel { 2 } else { 0 },
                &executable,
                &flags,
                &classpath_node,
                &key_environment,
                &key_info,
                jdeps.is_some(),
            )),
            execution_info: Some(display_info),
            ..Default::default()
        }
    });
    let paths = |files: &[File]| files.iter().map(|f| f.path.clone().into()).collect();
    let mode = if bazel {
        ClasspathMode::Bazel
    } else {
        ClasspathMode::Off
    };
    let launcher = executable.expand();
    context
        .register_action(Registration {
            outputs,
            inputs,
            command: String::new(),
            write: None,
            program: launcher.first().cloned(),
            arguments: launcher.into_iter().skip(1).collect(),
            environment: environment.fixed,
            extra: crate::action::ActionExtra {
                mnemonic: "Javac".into(),
                inherited_environment: environment.inherited,
                execution_info,
                java_compile: Some(Box::new(JavaCompileSpec {
                    mode,
                    flags: flags.expand(),
                    classpath: paths(&classpath),
                    direct_jars: paths(&direct),
                    dependency_jdeps: paths(&dependency_jdeps),
                    mandatory_inputs: paths(&mandatory),
                    output_jdeps: jdeps.map(|f| f.path.clone().into()),
                })),
                ..Default::default()
            },
            shell_pad: false,
            executable_file: false,
            deferred: None,
            exec_group: group,
            runfiles_trees: vec![],
            build_info: None,
            shareable: true,
        })
        .map_err(starlark::Error::new_other)?;
    if let Some(meta) = meta {
        attach_meta(context, &output, meta);
    }
    Ok(Value::new_none())
}

/// JavaCompilationHelper.createGenJarAction: GenClass over the class jar and
/// its manifest proto (a SpawnAction, mnemonic JavaSourceJar).
#[allow(clippy::too_many_arguments)]
fn gen_jar_action<'v>(
    context: &Context,
    toolchain: Value<'v>,
    class_jar: &File,
    manifest: &File,
    gen_class: &File,
    group: &str,
    heap: Heap<'v>,
) -> starlark::Result<()> {
    let tool = field(toolchain, "_gen_class", heap)?;
    if tool.is_none() {
        return Err(unsupported("a Java toolchain without GenClass"));
    }
    let tool = file(tool, "_gen_class")?;
    let runtime = field(toolchain, "java_runtime", heap)?;
    let runtime_inputs = files(
        field(runtime, "files", heap)?,
        "JavaRuntimeInfo.files",
        true,
    )?;
    let java = field(runtime, "java_executable_exec_path", heap)?;
    let java = java
        .unpack_str()
        .ok_or_else(|| type_error(COMPILE, "java_executable_exec_path", java, "string"))?;
    let jvm_opt = field(toolchain, "jvm_opt", heap)?;
    strings(jvm_opt, "jvm_opt")?;
    let mut executable = command::Line::default();
    executable
        .add(java)
        .add_bare_set(&command::node(jvm_opt).expect("checked depset"))
        .add("-jar")
        .add(tool.path.clone());
    let mut flags = command::Line::default();
    flags
        .add_opt("--manifest_proto", Some(&manifest.path))
        .add_opt("--class_jar", Some(&class_jar.path))
        .add_opt("--output_jar", Some(&gen_class.path));
    let launcher = executable.expand();
    let meta = crate::aquery::capturing().then(|| {
        let mut arguments = launcher.clone();
        arguments.extend(flags.expand());
        let environment = command::KeyEnvironment {
            fixed: Vec::new(),
            inherited: Vec::new(),
        };
        crate::aquery::Meta {
            progress: Some("Building genclass jar %{output}".into()),
            arguments: Some(arguments),
            environment: Some(Vec::new()),
            compute_key: Some(command::spawn_key(
                &[(&executable, None), (&flags, None)],
                "JavaSourceJar",
                &environment,
                &[],
            )),
            execution_info: Some(Vec::new()),
            ..Default::default()
        }
    });
    context
        .register_action(Registration {
            outputs: vec![gen_class.clone()],
            inputs: unique(
                [manifest.clone(), class_jar.clone()]
                    .into_iter()
                    .chain(runtime_inputs)
                    .chain([tool]),
            ),
            command: String::new(),
            write: None,
            program: launcher.first().cloned(),
            arguments: launcher.into_iter().skip(1).chain(flags.expand()).collect(),
            environment: BTreeMap::new(),
            extra: crate::action::ActionExtra {
                mnemonic: "JavaSourceJar".into(),
                ..Default::default()
            },
            shell_pad: false,
            executable_file: false,
            deferred: None,
            exec_group: group.to_owned(),
            runfiles_trees: vec![],
            build_info: None,
            shareable: true,
        })
        .map_err(starlark::Error::new_other)?;
    if let Some(meta) = meta {
        attach_meta(context, gen_class, meta);
    }
    Ok(())
}
