//! Native Java action factories (Bazel 9.3 JavaStarlarkCommon).
//!
//! The direct-classpath branch is an ordinary spawn: Turbine produces the
//! header jars and dependency proto. JavaBuilder's factory constructs the
//! typed execution-time reduced-classpath/fallback policy in `java_compile`.
use std::collections::{BTreeMap, HashSet};

use starlark::eval::{Arguments, Evaluator};
use starlark::values::{Heap, Value, ValueLike};

use crate::analysis_runtime::{Context, Registration};
use crate::command_line::{CommandLineSpec, Group, ParamFileInfo, ParamFileType, Part};
use crate::language_modules::{bind, failure, param, sequence, type_error};
use crate::starlark_types::{Depset, DepsetNode, File, Item, file_list, unpack_file};
use std::sync::Arc;

const HEADER: &str = "create_header_compilation_action";
const TOOLCHAIN_TYPE: &str = "@@bazel_tools//tools/jdk:toolchain_type";

fn unsupported(branch: &str) -> starlark::Error {
    unsupported_for(HEADER, branch)
}

fn unsupported_for(function: &str, branch: &str) -> starlark::Error {
    failure(format!(
        "java_common.{function}: {branch} is not implemented in the native profile (docs/JAVA_ACTIONS.md)"
    ))
}

fn field<'v>(value: Value<'v>, name: &str, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
    value.get_attr_error(name, heap)
}

/// Consume the rules_java provider, including its defining extension identity.
/// Both identities are accepted by the genuine Java provider wrappers.
fn provider(value: Value<'_>, path: &str, name: &str) -> starlark::Result<()> {
    let id = crate::providers::instance_identity(value);
    if id.as_deref() == Some(&format!("@@rules_java+//{path}%{name}"))
        || id.as_deref() == Some(&format!("//{path}%{name}"))
    {
        Ok(())
    } else {
        Err(failure(format!(
            "got value of type '{}', want '{name}' (rules_java provider identity)",
            value.get_type()
        )))
    }
}

fn files(value: Value<'_>, name: &str, depset: bool) -> starlark::Result<Vec<File>> {
    files_for(HEADER, value, name, depset)
}

fn files_for(
    function: &str,
    value: Value<'_>,
    name: &str,
    depset: bool,
) -> starlark::Result<Vec<File>> {
    if depset {
        if value.downcast_ref::<Depset>().is_none() {
            return Err(type_error(function, name, value, "depset"));
        }
    } else {
        sequence(function, name, value)?;
    }
    let files = file_list(value).map_err(starlark::Error::new_other)?;
    if files
        .iter()
        .any(|file| file.is_directory || file.is_symlink)
    {
        return Err(unsupported_for(
            function,
            &format!("tree or unresolved symlink artifacts in {name}"),
        ));
    }
    Ok(files)
}

fn strings(value: Value<'_>, name: &str) -> starlark::Result<Vec<String>> {
    strings_for(HEADER, value, name)
}

fn strings_for(function: &str, value: Value<'_>, name: &str) -> starlark::Result<Vec<String>> {
    let set = value
        .downcast_ref::<Depset>()
        .ok_or_else(|| type_error(function, name, value, "depset"))?;
    set.flatten()
        .into_iter()
        .map(|item| match item {
            Item::String(text) => Ok(text.clone()),
            _ => Err(failure(format!("{name} must be a depset of strings"))),
        })
        .collect()
}

fn file(value: Value<'_>, name: &str) -> starlark::Result<File> {
    file_for(HEADER, value, name)
}

fn file_for(function: &str, value: Value<'_>, name: &str) -> starlark::Result<File> {
    let file = unpack_file(value).map_err(starlark::Error::new_other)?;
    if file.is_directory || file.is_symlink {
        return Err(unsupported_for(
            function,
            &format!("tree or unresolved symlink artifact in {name}"),
        ));
    }
    Ok(file)
}

fn unique(files: impl IntoIterator<Item = File>) -> Vec<File> {
    let mut seen = HashSet::new();
    files
        .into_iter()
        .filter(|f| seen.insert(f.clone()))
        .collect()
}

/// JavaHelper.tokenizeJavaOptions: reverse the flattened depset, tokenize
/// each entry, and retain an un-tokenizable entry as the tool's argument.
fn javac_options(options: Vec<String>) -> Vec<String> {
    options
        .into_iter()
        .rev()
        .flat_map(|option| crate::ctx_surface::tokenize(&option).unwrap_or_else(|_| vec![option]))
        .collect()
}

/// BootClassPathInfo.bootclasspath() and whether the info is empty.
fn bootclasspath<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<(Vec<File>, Arc<DepsetNode>, bool)> {
    provider(
        value,
        "java/private:boot_class_path_info.bzl",
        "BootClassPathInfo",
    )?;
    let boot_value = field(value, "bootclasspath", heap)?;
    let boot = files(boot_value, "bootclasspath", true)?;
    let auxiliary = files(field(value, "_auxiliary", heap)?, "_auxiliary", true)?;
    let system = files(
        field(value, "_system_inputs", heap)?,
        "_system_inputs",
        true,
    )?;
    let path = field(value, "_system_path", heap)?;
    if !path.is_none() && path.unpack_str().is_none() {
        return Err(type_error(
            HEADER,
            "_system_path",
            path,
            "string or NoneType",
        ));
    }
    let empty = boot.is_empty() && auxiliary.is_empty() && system.is_empty() && path.is_none();
    // Headers use only bootclasspath(), including when a system image is
    // present. The other fields decide whether the override is empty.
    Ok((
        boot,
        command::node(boot_value).expect("checked depset"),
        empty,
    ))
}

/// One JavaPluginData (`plugins` or `api_generating_plugins`): the
/// processor classes, jars and data with their NestedSets.
#[derive(Clone)]
struct PluginData {
    classes: Vec<String>,
    classes_node: Arc<DepsetNode>,
    jars: Vec<File>,
    jars_node: Arc<DepsetNode>,
    data: Vec<File>,
}

impl PluginData {
    fn read<'v>(
        function: &str,
        info: Value<'v>,
        name: &str,
        heap: Heap<'v>,
    ) -> starlark::Result<Self> {
        let data = field(info, name, heap)?;
        let classes = field(data, "processor_classes", heap)?;
        let jars = field(data, "processor_jars", heap)?;
        let inputs = field(data, "processor_data", heap)?;
        Ok(Self {
            classes: strings_for(function, classes, "processor_classes")?,
            classes_node: command::node(classes).expect("checked depset"),
            jars: files_for(function, jars, "processor_jars", true)?,
            jars_node: command::node(jars).expect("checked depset"),
            data: files_for(function, inputs, "processor_data", true)?,
        })
    }
}

/// The rules_java JavaPluginInfo: its `plugins` and `api_generating_plugins`.
fn plugin_info<'v>(
    function: &str,
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<(PluginData, PluginData)> {
    provider(value, "java/private:java_info.bzl", "JavaPluginInfo")?;
    crate::java_plugins::validate_outputs(value, heap).map_err(failure)?;
    Ok((
        PluginData::read(function, value, "plugins", heap)?,
        PluginData::read(function, value, "api_generating_plugins", heap)?,
    ))
}

/// JavaToolchainTool.getCommandLine and addInputs: only executable + data
/// (+ runtime files for a jar). Deliberately do not add the tool's runfiles
/// (genuine :124-130). `extra_jvm_flags`: withAdditionalJvmFlags.
fn tool_command<'v>(
    function: &str,
    tool: Value<'v>,
    toolchain: Value<'v>,
    extra_jvm_flags: Option<&std::sync::Arc<crate::starlark_types::DepsetNode>>,
    heap: Heap<'v>,
) -> starlark::Result<(command::Line, Vec<File>)> {
    let runner = field(tool, "tool", heap)?;
    let runner = runner
        .downcast_ref::<crate::runfiles::FilesToRun>()
        .ok_or_else(|| type_error(function, "tool", runner, "FilesToRunProvider"))?;
    let executable = runner
        .executable()
        .ok_or_else(|| failure("Java compiler tool has no executable".into()))?;
    let mut inputs = files_for(function, field(tool, "data", heap)?, "data", true)?;
    inputs.push(executable.clone());
    let jvm_opts = field(tool, "jvm_opts", heap)?;
    strings_for(function, jvm_opts, "jvm_opts")?;
    let mut options = command::node(jvm_opts).expect("checked depset");
    if let Some(extra) = extra_jvm_flags.filter(|extra| !extra.empty) {
        options = command::build(
            Vec::new(),
            vec![options, std::sync::Arc::clone(extra)],
            crate::starlark_types::Order::Default,
            false,
        );
    }
    let mut line = command::Line::default();
    if executable.path.ends_with(".jar") {
        let runtime = field(toolchain, "java_runtime", heap)?;
        provider(
            runtime,
            "java/common/rules:java_runtime.bzl",
            "JavaRuntimeInfo",
        )?;
        inputs.extend(files_for(
            function,
            field(runtime, "files", heap)?,
            "JavaRuntimeInfo.files",
            true,
        )?);
        let java = field(runtime, "java_executable_exec_path", heap)?;
        let java = java
            .unpack_str()
            .ok_or_else(|| type_error(function, "java_executable_exec_path", java, "string"))?;
        let jvm_opt = field(toolchain, "jvm_opt", heap)?;
        strings_for(function, jvm_opt, "jvm_opt")?;
        line.add(java)
            .add_bare_set(&command::node(jvm_opt).expect("checked depset"))
            .add_bare_set(&options)
            .add("-jar")
            .add(executable.path.clone());
    } else {
        line.add(executable.path.clone()).add_bare_set(&options);
    }
    Ok((line, inputs))
}

/// Resolve the declared Java toolchain and its execution group, checking the
/// actual current ctx capability before touching the action registration.
fn execution_group<'v>(
    ctx: Value<'v>,
    context: &Context,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    crate::analysis_ctx::current_surface(ctx).map_err(starlark::Error::new_other)?;
    let toolchains = field(ctx, "toolchains", heap)?;
    let label = crate::label::Label::parse_canonical(TOOLCHAIN_TYPE).map_err(failure)?;
    let key = crate::starlark_types::alloc_label(heap, TOOLCHAIN_TYPE)
        .map_err(starlark::Error::new_other)?;
    if !toolchains.is_in(key)? || toolchains.at(key, heap)?.is_none() {
        return Err(failure(format!(
            "Rule '{}' must declare '@bazel_tools//tools/jdk:toolchain_type' toolchain in order to use java_common.",
            context.rule_class
        )));
    }
    context
        .exec_groups
        .determine(None, &crate::exec_groups::ToolchainArgument::Label(label))
        .map_err(starlark::Error::new_other)
}

/// BazelJavaSemantics.utf8Environment / ConstraintConstants: the selected
/// execution platform's OS constraint wins, including inherited constraints.
/// Only a missing OS constraint falls back to the local OS.
fn utf8_locale(context: &Context, group: &str) -> starlark::Result<&'static str> {
    let setting = context
        .repositories
        .parse("@platforms//os:os", "bazel_tools", "")
        .map_err(failure)?;
    let os = context
        .exec_groups
        .executions
        .iter()
        .find(|execution| execution.exec_group == group)
        .and_then(|execution| execution.constraints.as_ref())
        .and_then(|constraints| {
            constraints.get(&crate::platforms::ConstraintSettingInfo {
                label: setting.clone(),
                default_value: None,
                refined: None,
            })
        });
    let macos = os.map_or(cfg!(target_os = "macos"), |os| {
        os.label.repository == setting.repository
            && os.label.package == "os"
            && matches!(os.label.name.as_str(), "osx" | "macos")
    });
    Ok(if macos { "en_US.UTF-8" } else { "C.UTF-8" })
}

pub(crate) fn create_header<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let names = [
        "ctx",
        "java_toolchain",
        "compile_jar",
        "compile_deps_proto",
        "plugin_info",
        "source_files",
        "source_jars",
        "compilation_classpath",
        "direct_jars",
        "bootclasspath",
        "compile_time_java_deps",
        "javac_opts",
        "strict_deps_mode",
        "target_label",
        "injecting_rule_kind",
        "enable_direct_classpath",
        "additional_inputs",
        "header_compilation_jar",
        "header_compilation_direct_deps",
    ];
    let params: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| param(name, true, i >= 17, true))
        .collect();
    let bound = bind(HEADER, &params, args, heap)?;
    let v = |i: usize| bound[i].expect("required parameter");
    let context = crate::analysis_runtime::context(eval).map_err(starlark::Error::new_other)?;
    let exec_group = execution_group(v(0), context, heap)?;
    let toolchain = v(1);
    provider(
        toolchain,
        "java/common/rules:java_toolchain.bzl",
        "JavaToolchainInfo",
    )?;
    let output = file(v(2), "compile_jar")?;
    let jdeps = file(v(3), "compile_deps_proto")?;
    // JavaCompilationHelper.createHeaderCompilationAction: the
    // API-generating plugins only.
    let (_, plugins) = plugin_info(HEADER, v(4), heap)?;
    // JavaTargetAttributes.addSourceFiles keeps only .java artifacts.
    let sources: Vec<_> = unique(
        files(v(5), "source_files", true)?
            .into_iter()
            .filter(|f| f.path.ends_with(".java")),
    );
    let source_jars = files(v(6), "source_jars", false)?;
    files(v(7), "compilation_classpath", true)?;
    files(v(8), "direct_jars", true)?;
    files(v(10), "compile_time_java_deps", true)?;
    let naive = crate::starlark_types::Order::Preorder;
    let attribute = |value: Value<'v>, order| {
        command::build(
            Vec::new(),
            command::node(value).into_iter().collect(),
            order,
            true,
        )
    };
    // JavaTargetAttributes.build: naive-link builders over the given sets.
    let classpath_node = attribute(v(7), naive);
    let mut direct_node = attribute(v(8), naive);
    let mut dependency_node = attribute(v(10), crate::starlark_types::Order::Default);
    let options = javac_options(strings(v(11), "javac_opts")?);
    let strict = v(12)
        .unpack_str()
        .ok_or_else(|| type_error(HEADER, "strict_deps_mode", v(12), "string"))?
        .to_ascii_uppercase();
    let strict = match strict.as_str() {
        "OFF" => "OFF",
        "ERROR" | "DEFAULT" => "ERROR",
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
    let target = crate::starlark_types::label_command_line_form(v(13), eval)
        .ok_or_else(|| type_error(HEADER, "target_label", v(13), "Label"))?;
    let injecting = if v(14).is_none() {
        None
    } else {
        Some(v(14).unpack_str().ok_or_else(|| {
            type_error(HEADER, "injecting_rule_kind", v(14), "string or NoneType")
        })?)
    };
    let enable_direct = v(15)
        .unpack_bool()
        .ok_or_else(|| type_error(HEADER, "enable_direct_classpath", v(15), "bool"))?;
    let additional = files(v(16), "additional_inputs", false)?;
    let tjar = file(v(17), "header_compilation_jar")?;
    files(v(18), "header_compilation_direct_deps", true)?;
    let header_deps_node = attribute(v(18), naive);
    let builtin = strings(
        field(toolchain, "_header_compiler_builtin_processors", heap)?,
        "_header_compiler_builtin_processors",
    )?;
    let incompatible = strings(
        field(
            toolchain,
            "_reduced_classpath_incompatible_processors",
            heap,
        )?,
        "_reduced_classpath_incompatible_processors",
    )?;
    let use_direct_classpath = enable_direct && plugins.classes.is_empty();
    let direct_tool = field(toolchain, "_header_compiler_direct", heap)?;
    let use_direct_compiler =
        !direct_tool.is_none() && plugins.classes.iter().all(|class| builtin.contains(class));
    let tool = if use_direct_compiler {
        direct_tool
    } else {
        field(toolchain, "_header_compiler", heap)?
    };
    if tool.is_none() {
        return Err(unsupported("a Java toolchain without a header compiler"));
    }
    let mut boot = match bound[9].filter(|value| !value.is_none()) {
        Some(value) => bootclasspath(value, heap)?,
        None => (Vec::new(), command::empty(naive), true),
    };
    if boot.2 {
        boot = bootclasspath(field(toolchain, "_bootclasspath_info", heap)?, heap)?;
    }
    let tools_value = field(toolchain, "tools", heap)?;
    let tools = files(tools_value, "tools", true)?;
    let (executable, tool_inputs) = tool_command(HEADER, tool, toolchain, None, heap)?;
    let mut flags = command::Line::default();
    flags
        .add_opt("--output", Some(&output.path))
        .add_opt("--header_compilation_output", Some(&tjar.path))
        .add_opt("--output_deps", Some(&jdeps.path))
        .add_set("--bootclasspath", &boot.1)
        .add_paths("--sources", &sources)
        .add_paths("--source_jars", &source_jars)
        .add_opt("--injecting_rule_kind", injecting);
    if !options.is_empty() {
        flags.add("--javacopts");
        for option in &options {
            flags.add(option.clone());
        }
        flags.add("--");
    }
    flags.add_target_label(&target);
    let mut mandatory = unique(
        additional
            .into_iter()
            .chain(boot.0.iter().cloned())
            .chain(source_jars.iter().cloned())
            .chain(sources.iter().cloned())
            .chain(tools)
            .chain(tool_inputs),
    );
    let tag_info: Vec<(String, String)> = context
        .propagated_tags
        .iter()
        .map(|tag| (tag.clone(), String::new()))
        .collect();
    let environment = context.action_env.for_spawn(
        true,
        BTreeMap::from([(
            "LC_CTYPE".to_owned(),
            utf8_locale(context, &exec_group)?.to_owned(),
        )]),
    );
    let locale = utf8_locale(context, &exec_group)?;
    let key_environment = command::KeyEnvironment::of(context, &[("LC_CTYPE", locale)], true);
    let progress = command::progress(
        "Compiling Java headers",
        sources.len(),
        source_jars.len(),
        &plugins.classes,
    );
    let capturing = crate::aquery::capturing();
    let mut execution_info = BTreeMap::from([
        ("supports-path-mapping".to_owned(), "1".to_owned()),
        ("internal-inline-outputs".to_owned(), jdeps.path.clone()),
    ]);
    if !use_direct_classpath {
        // JavaCompileAction(TURBINE): javac-based header compilation with
        // annotation processors, reduced classpath and fallback like Javac.
        if !use_direct_compiler {
            mandatory = unique(
                mandatory
                    .into_iter()
                    .chain(plugins.jars.iter().cloned())
                    .chain(plugins.data.iter().cloned()),
            );
        }
        let builtin_used: Vec<String> = unique_strings(
            plugins
                .classes
                .iter()
                .filter(|class| builtin.contains(class))
                .cloned(),
        );
        flags.add_all("--builtin_processors", &builtin_used);
        flags.add_set("--processors", &plugins.classes_node);
        if !use_direct_compiler {
            flags.add_set("--processorpath", &plugins.jars_node);
        }
        if strict != "OFF" {
            flags.add_set("--direct_dependencies", &direct_node);
        }
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
        let dependency_jdeps = flat(&dependency_node);
        let bazel = !plugins
            .classes
            .iter()
            .any(|class| incompatible.contains(class));
        let inputs = unique(
            mandatory
                .iter()
                .cloned()
                .chain(classpath.iter().cloned())
                .chain(dependency_jdeps.iter().cloned()),
        );
        let key_info: Vec<(String, String)> =
            std::iter::once(("supports-path-mapping".to_owned(), "1".to_owned()))
                .chain(tag_info.iter().cloned())
                .collect();
        let mut meta = None;
        if capturing {
            let mut arguments = executable.expand();
            arguments.extend(flags.expand());
            let mut classpath_line = command::Line::default();
            classpath_line.add_set("--classpath", &classpath_node);
            arguments.extend(classpath_line.expand());
            let mut display_info = key_info.clone();
            display_info.push(("internal-inline-outputs".to_owned(), jdeps.path.clone()));
            meta = Some(crate::aquery::Meta {
                progress: Some(progress.clone()),
                arguments: Some(arguments),
                compute_key: Some(command::java_compile_key(
                    if bazel { 2 } else { 0 },
                    &executable,
                    &flags,
                    &classpath_node,
                    &key_environment,
                    &key_info,
                    true,
                )),
                execution_info: Some(display_info),
                ..Default::default()
            });
        }
        let launcher = executable.expand();
        let paths = |files: &[File]| files.iter().map(|f| f.path.clone().into()).collect();
        let outputs = vec![output.clone(), jdeps.clone(), tjar];
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
                    mnemonic: "JavacTurbine".to_owned(),
                    inherited_environment: environment.inherited,
                    execution_info,
                    java_compile: Some(Box::new(crate::java_compile::JavaCompileSpec {
                        mode: if bazel {
                            crate::java_compile::ClasspathMode::Bazel
                        } else {
                            crate::java_compile::ClasspathMode::Off
                        },
                        flags: flags.expand(),
                        classpath: paths(&classpath),
                        direct_jars: paths(&direct),
                        dependency_jdeps: paths(&dependency_jdeps),
                        mandatory_inputs: paths(&mandatory),
                        output_jdeps: Some(jdeps.path.clone().into()),
                    })),
                    ..Default::default()
                },
                shell_pad: false,
                executable_file: false,
                deferred: None,
                exec_group,
                runfiles_trees: Vec::new(),
                build_info: None,
                shareable: true,
            })
            .map_err(starlark::Error::new_other)?;
        if let Some(meta) = meta {
            attach_meta(context, &output, meta);
        }
        return Ok(Value::new_none());
    }
    // The direct classpath: the header direct dependencies when present (or
    // when there is no classpath), else the classpath.
    let classpath_node = if !header_deps_node.empty || classpath_node.empty {
        header_deps_node
    } else {
        classpath_node
    };
    flags
        .add_set("--classpath", &classpath_node)
        .add("--reduce_classpath_mode")
        .add("NONE");
    let classpath: Vec<File> = Depset {
        node: Arc::clone(&classpath_node),
    }
    .flatten()
    .into_iter()
    .filter_map(|item| match item {
        Item::File(file) => Some(file.clone()),
        _ => None,
    })
    .collect();
    let inputs = unique(mandatory.into_iter().chain(classpath.iter().cloned()));
    let launcher = executable.expand();
    let spec = CommandLineSpec {
        executable: launcher.clone(),
        parts: vec![Part {
            groups: vec![Group::Strings(flags.expand())],
            flag_per_line: false,
            param: Some(ParamFileInfo {
                flag_format: "@%s".to_owned(),
                always: false,
                file_type: ParamFileType::Unquoted,
                flags_only: false,
            }),
        }],
        primary_output: output.path.clone(),
        max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
    };
    let (arguments, param_files) = spec.expand(None).map_err(failure)?;
    let mut meta = None;
    if capturing {
        // SpawnAction.getExecutionInfo: the sorted execution info, then
        // JavaHeaderCompileAction's in-memory jdeps directive.
        let mut key_info: BTreeMap<String, String> = tag_info.iter().cloned().collect();
        key_info.insert("supports-path-mapping".to_owned(), "1".to_owned());
        let mut key_info: Vec<(String, String)> = key_info.into_iter().collect();
        key_info.push(("internal-inline-outputs".to_owned(), jdeps.path.clone()));
        let mut all = launcher.clone();
        all.extend(flags.expand());
        meta = Some(crate::aquery::Meta {
            progress: Some(progress),
            arguments: Some(all),
            compute_key: Some(command::spawn_key(
                &[
                    (&executable, None),
                    (
                        &flags,
                        Some(command::ParamKey {
                            file_type: "UNQUOTED",
                        }),
                    ),
                ],
                "Turbine",
                &key_environment,
                &key_info,
            )),
            execution_info: Some(key_info),
            ..Default::default()
        });
    }
    context
        .register_action(Registration {
            outputs: vec![output.clone(), jdeps.clone(), tjar],
            inputs,
            command: String::new(),
            write: None,
            program: launcher.first().cloned(),
            arguments: launcher.into_iter().skip(1).chain(arguments).collect(),
            environment: environment.fixed,
            extra: crate::action::ActionExtra {
                mnemonic: "Turbine".to_owned(),
                param_files,
                inherited_environment: environment.inherited,
                execution_info: std::mem::take(&mut execution_info),
                ..Default::default()
            },
            shell_pad: false,
            executable_file: false,
            deferred: None,
            exec_group,
            runfiles_trees: Vec::new(),
            build_info: None,
            shareable: true,
        })
        .map_err(starlark::Error::new_other)?;
    if let Some(meta) = meta {
        attach_meta(context, &output, meta);
    }
    Ok(Value::new_none())
}

fn unique_strings(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

/// The aquery description of the action just registered for `output`
/// (crate::aquery::Meta; only while aquery captures).
fn attach_meta(context: &Context, output: &File, meta: crate::aquery::Meta) {
    let output: std::path::PathBuf = output.path.clone().into();
    let mut actions = context.actions.borrow_mut();
    if let Some(registered) = actions
        .iter_mut()
        .rev()
        .find(|action| action.action.outputs.contains(&output))
    {
        registered.action.extra.aquery = Some(Arc::new(meta));
    }
}

mod command;
mod compilation;
mod resources;
pub(crate) use compilation::create_compilation;

#[cfg(test)]
mod tests {
    #[test]
    fn options_reverse_depset_and_preserve_unparseable_tokens() {
        assert_eq!(
            super::javac_options(vec![
                "'bad".into(),
                "-source 11".into(),
                "'-Akey=a b'".into()
            ]),
            ["-Akey=a b", "-source", "11", "'bad"]
        );
    }
}
