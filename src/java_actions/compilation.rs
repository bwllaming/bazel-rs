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
    auxiliary: Vec<File>,
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
        Ok(Self {
            jars: files(field(value, "bootclasspath", heap)?, "bootclasspath", true)?,
            auxiliary: files(field(value, "_auxiliary", heap)?, "_auxiliary", true)?,
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
    let output = file(value(2), "output")?;
    let manifest = file(value(3), "manifest_proto")?;
    let plugin_info = value(4);
    provider(plugin_info, "java/private:java_info.bzl", "JavaPluginInfo")?;
    crate::java_plugins::validate_outputs(plugin_info, heap).map_err(failure)?;
    let mut plugin_jars = Vec::new();
    let mut plugin_data = Vec::new();
    for name in ["plugins", "api_generating_plugins"] {
        let data = field(plugin_info, name, heap)?;
        let classes = strings(field(data, "processor_classes", heap)?, "processor_classes")?;
        let jars = files(field(data, "processor_jars", heap)?, "processor_jars", true)?;
        let inputs = files(field(data, "processor_data", heap)?, "processor_data", true)?;
        if !classes.is_empty() {
            return Err(unsupported("annotation processors"));
        }
        if name == "plugins" {
            plugin_jars = jars;
            plugin_data = inputs;
        }
    }
    let mut classpath = files(value(5), "compilation_classpath", true)?;
    let mut direct = files(value(6), "direct_jars", true)?;
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
        classpath = unique(boot.auxiliary.iter().cloned().chain(classpath));
        direct = unique(boot.auxiliary.iter().cloned().chain(direct));
    }
    let jvm_flags = strings(value(8), "javabuilder_jvm_flags")?;
    let mut dependency_jdeps = files(value(9), "compile_time_java_deps", true)?;
    let options = javac_options(strings(value(10), "javac_opts")?);
    if options.iter().any(|option| option == "-processor") {
        return Err(unsupported("annotation processors in javac_opts"));
    }
    let strict = value(11)
        .unpack_str()
        .ok_or_else(|| type_error(COMPILE, "strict_deps_mode", value(11), "string"))?
        .to_ascii_uppercase();
    let strict = match strict.as_str() {
        "OFF" => {
            direct.clear();
            dependency_jdeps.clear();
            "OFF"
        }
        "DEFAULT" | "ERROR" => "ERROR",
        "WARN" => "WARN",
        _ => {
            return Err(failure(format!(
                "StrictDepsMode {strict} not allowed. Only OFF and ERROR values are accepted."
            )));
        }
    };
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
    if gen_class.is_some() || gen_source.is_some() {
        return Err(unsupported("generated processor class/source jars"));
    }
    let native_header = optional_file(16, "native_header_jar")?;
    let sources = if value(17).is_none() {
        Vec::new()
    } else {
        files(value(17), "sources", true)?
            .into_iter()
            .filter(|f| f.path.ends_with(".java"))
            .collect()
    };
    let sequence_files = |i, name| match bound[i] {
        Some(value) => files(value, name, false),
        None => Ok(Vec::new()),
    };
    let source_jars = sequence_files(18, "source_jars")?;
    let resources = sequence_files(19, "resources")?;
    let resource_jars = if value(20).is_none() {
        Vec::new()
    } else {
        files(value(20), "resource_jars", true)?
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
    boolean(25, "enable_direct_classpath")?; // Only affects turbine annotation processing, rejected above.
    let additional = sequence_files(26, "additional_inputs")?;
    let additional_outputs = sequence_files(27, "additional_outputs")?;
    if !additional_outputs.is_empty() {
        return Err(unsupported("additional processor outputs"));
    }
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
    if !resources.is_empty() || !resource_jars.is_empty() || !classpath_resources.is_empty() {
        return Err(unsupported("resource jar packaging"));
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
    strings(
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
    let (launcher, tool_inputs) = launcher_with_flags(COMPILE, tool, toolchain, &jvm_flags, heap)?;
    let mandatory = unique(
        plugin_jars
            .iter()
            .cloned()
            .chain(plugin_data)
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
    let inputs = unique(
        mandatory
            .iter()
            .cloned()
            .chain(classpath.iter().cloned())
            .chain(dependency_jdeps.iter().cloned()),
    );
    let mut outputs = vec![output.clone()];
    outputs.extend(jdeps.iter().cloned());
    outputs.extend(native_header.iter().cloned());
    outputs.push(manifest.clone());
    let mut flags = vec!["--output".into(), output.path.clone()];
    if let Some(header) = &native_header {
        paths(
            &mut flags,
            "--native_header_output",
            std::slice::from_ref(header),
        );
    }
    paths(
        &mut flags,
        "--output_manifest_proto",
        std::slice::from_ref(&manifest),
    );
    flags.push("--compress_jar".into());
    if let Some(deps) = &jdeps {
        paths(
            &mut flags,
            "--output_deps_proto",
            std::slice::from_ref(deps),
        );
    }
    paths(&mut flags, "--bootclasspath", &boot.jars);
    if let Some(system) = boot.system_path {
        flags.extend(["--system".into(), system]);
    }
    paths(&mut flags, "--sourcepath", &sourcepath);
    paths(&mut flags, "--processorpath", &plugin_jars);
    paths(&mut flags, "--source_jars", &source_jars);
    paths(&mut flags, "--sources", &sources);
    if !options.is_empty() {
        flags.push("--javacopts".into());
        flags.extend(options);
        flags.push("--".into());
    }
    flags.extend([
        "--target_label".into(),
        if target.starts_with('@') {
            format!("@{target}")
        } else {
            target
        },
    ]);
    if let Some(injecting) = injecting {
        flags.extend(["--injecting_rule_kind".into(), injecting.to_owned()]);
    }
    if strict != "OFF" {
        flags.extend(["--strict_java_deps".into(), strict.into()]);
        paths(&mut flags, "--direct_dependencies", &direct);
    }
    flags.extend(["--experimental_fix_deps_tool".into(), "add_dep".into()]);
    let mut execution_info = BTreeMap::from([("supports-path-mapping".into(), "1".into())]);
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
            execution_info.insert(requirement.into(), "1".into());
        }
    }
    if let Some(jdeps) = &jdeps {
        execution_info.insert("internal-inline-outputs".into(), jdeps.path.clone());
    }
    let environment = context.action_env.for_spawn(
        true,
        BTreeMap::from([("LC_CTYPE".into(), utf8_locale(context, &group)?.into())]),
    );
    let paths = |files: &[File]| files.iter().map(|f| f.path.clone().into()).collect();
    // The native fragment currently advertises BAZEL; no selectable alternative
    // mode is accepted. These two late OFF conditions are genuine builder rules.
    let mode = match crate::ctx_surface::JAVA_CLASSPATH_MODE {
        "BAZEL" if strict != "OFF" && jdeps.is_some() => ClasspathMode::Bazel,
        "BAZEL" | "OFF" => ClasspathMode::Off,
        _ => return Err(unsupported("configured Java classpath mode")),
    };
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
                java_compile: Some(JavaCompileSpec {
                    mode,
                    flags,
                    classpath: paths(&classpath),
                    direct_jars: paths(&direct),
                    dependency_jdeps: paths(&dependency_jdeps),
                    mandatory_inputs: paths(&mandatory),
                    output_jdeps: jdeps.map(|f| f.path.clone().into()),
                }),
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
    Ok(Value::new_none())
}
