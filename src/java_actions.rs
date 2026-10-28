//! Source-based Java header compilation (Bazel 9.3 JavaHeaderCompileAction).
//!
//! The direct-classpath branch is an ordinary spawn: Turbine produces the
//! header jars and dependency proto. JavaBuilder's execution-time reduced
//! classpath/fallback protocol is a separate, deliberately unsupported branch.
use std::collections::{BTreeMap, HashSet};

use starlark::eval::{Arguments, Evaluator};
use starlark::values::{Heap, Value, ValueLike};

use crate::analysis_runtime::{Context, Registration};
use crate::command_line::{CommandLineSpec, Group, ParamFileInfo, ParamFileType, Part};
use crate::language_modules::{bind, failure, param, sequence, type_error};
use crate::starlark_types::{Depset, File, Item, file_list, unpack_file};

const HEADER: &str = "create_header_compilation_action";
const TOOLCHAIN_TYPE: &str = "@@bazel_tools//tools/jdk:toolchain_type";

fn unsupported(branch: &str) -> starlark::Error {
    failure(format!(
        "java_common.{HEADER}: {branch} is not implemented in the native profile (docs/JAVA_ACTIONS.md)"
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
    if depset {
        if value.downcast_ref::<Depset>().is_none() {
            return Err(type_error(HEADER, name, value, "depset"));
        }
    } else {
        sequence(HEADER, name, value)?;
    }
    let files = file_list(value).map_err(starlark::Error::new_other)?;
    if files
        .iter()
        .any(|file| file.is_directory || file.is_symlink)
    {
        return Err(unsupported(&format!(
            "tree or unresolved symlink artifacts in {name}"
        )));
    }
    Ok(files)
}

fn strings(value: Value<'_>, name: &str) -> starlark::Result<Vec<String>> {
    let set = value
        .downcast_ref::<Depset>()
        .ok_or_else(|| type_error(HEADER, name, value, "depset"))?;
    set.flatten()
        .into_iter()
        .map(|item| match item {
            Item::String(text) => Ok(text.clone()),
            _ => Err(failure(format!("{name} must be a depset of strings"))),
        })
        .collect()
}

fn file(value: Value<'_>, name: &str) -> starlark::Result<File> {
    let file = unpack_file(value).map_err(starlark::Error::new_other)?;
    if file.is_directory || file.is_symlink {
        return Err(unsupported(&format!(
            "tree or unresolved symlink artifact in {name}"
        )));
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

fn paths(args: &mut Vec<String>, flag: &str, files: &[File]) {
    if !files.is_empty() {
        args.push(flag.to_owned());
        args.extend(files.iter().map(|f| f.path.clone()));
    }
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

fn bootclasspath<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<(Vec<File>, bool)> {
    provider(
        value,
        "java/private:boot_class_path_info.bzl",
        "BootClassPathInfo",
    )?;
    let boot = files(field(value, "bootclasspath", heap)?, "bootclasspath", true)?;
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
    Ok((boot, empty))
}

fn validate_plugins<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<()> {
    provider(value, "java/private:java_info.bzl", "JavaPluginInfo")?;
    crate::java_plugins::validate_outputs(value, heap).map_err(failure)?;
    for name in ["plugins", "api_generating_plugins"] {
        let data = field(value, name, heap)?;
        let classes = strings(field(data, "processor_classes", heap)?, "processor_classes")?;
        files(field(data, "processor_jars", heap)?, "processor_jars", true)?;
        files(field(data, "processor_data", heap)?, "processor_data", true)?;
        if name == "api_generating_plugins" && !classes.is_empty() {
            return Err(unsupported("API-generating annotation processors"));
        }
    }
    Ok(())
}

/// JavaToolchainTool: only executable + data (+ runtime files for a jar).
/// Deliberately do not add the tool's runfiles (genuine :124-130).
fn launcher<'v>(
    tool: Value<'v>,
    toolchain: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<(Vec<String>, Vec<File>)> {
    let runner = field(tool, "tool", heap)?;
    let runner = runner
        .downcast_ref::<crate::runfiles::FilesToRun>()
        .ok_or_else(|| type_error(HEADER, "tool", runner, "FilesToRunProvider"))?;
    let executable = runner
        .executable()
        .ok_or_else(|| failure("Java compiler tool has no executable".into()))?;
    let mut inputs = files(field(tool, "data", heap)?, "data", true)?;
    inputs.push(executable.clone());
    let options = strings(field(tool, "jvm_opts", heap)?, "jvm_opts")?;
    let mut command;
    if executable.path.ends_with(".jar") {
        let runtime = field(toolchain, "java_runtime", heap)?;
        provider(
            runtime,
            "java/common/rules:java_runtime.bzl",
            "JavaRuntimeInfo",
        )?;
        inputs.extend(files(
            field(runtime, "files", heap)?,
            "JavaRuntimeInfo.files",
            true,
        )?);
        let java = field(runtime, "java_executable_exec_path", heap)?;
        let java = java
            .unpack_str()
            .ok_or_else(|| type_error(HEADER, "java_executable_exec_path", java, "string"))?;
        command = vec![java.to_owned()];
        command.extend(strings(field(toolchain, "jvm_opt", heap)?, "jvm_opt")?);
        command.extend(options);
        command.push("-jar".to_owned());
        command.push(executable.path.clone());
    } else {
        command = vec![executable.path.clone()];
        command.extend(options);
    }
    Ok((command, inputs))
}

/// Resolve the declared Java toolchain and its execution group, checking the
/// actual current ctx capability before touching the action registration.
fn execution_group<'v>(
    ctx: Value<'v>,
    context: &Context,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    let surface = crate::analysis_ctx::current_surface(ctx).map_err(starlark::Error::new_other)?;
    // The native executor currently selects only the host platform. Its
    // user-defined replacement can have a different OS constraint, which
    // this scope does not carry yet (BazelJavaSemantics.utf8Environment).
    if surface.config.host_platform != crate::configuration::default_host_platform() {
        return Err(unsupported(
            "custom host execution-platform UTF-8 environment",
        ));
    }
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
    validate_plugins(v(4), heap)?;
    // JavaTargetAttributes.addSourceFiles keeps only .java artifacts.
    let sources: Vec<_> = files(v(5), "source_files", true)?
        .into_iter()
        .filter(|f| f.path.ends_with(".java"))
        .collect();
    let source_jars = files(v(6), "source_jars", false)?;
    let classpath = files(v(7), "compilation_classpath", true)?;
    files(v(8), "direct_jars", true)?;
    files(v(10), "compile_time_java_deps", true)?;
    let options = javac_options(strings(v(11), "javac_opts")?);
    let strict = v(12)
        .unpack_str()
        .ok_or_else(|| type_error(HEADER, "strict_deps_mode", v(12), "string"))?
        .to_ascii_uppercase();
    match strict.as_str() {
        "OFF" | "ERROR" | "DEFAULT" | "WARN" => {}
        _ => {
            return Err(failure(format!(
                "StrictDepsMode {strict} not allowed. Only OFF and ERROR values are accepted."
            )));
        }
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
    let header_deps = files(v(18), "header_compilation_direct_deps", true)?;
    if !enable_direct {
        return Err(unsupported(
            "non-direct classpath header compilation (JavaCompileAction jdeps reduction/fallback)",
        ));
    }
    let tool = field(toolchain, "_header_compiler_direct", heap)?;
    if tool.is_none() {
        return Err(unsupported("non-direct header compiler (JavacTurbine)"));
    }
    strings(
        field(toolchain, "_header_compiler_builtin_processors", heap)?,
        "_header_compiler_builtin_processors",
    )?;
    strings(
        field(
            toolchain,
            "_reduced_classpath_incompatible_processors",
            heap,
        )?,
        "_reduced_classpath_incompatible_processors",
    )?;
    let mut boot = if v(9).is_none() {
        (Vec::new(), true)
    } else {
        bootclasspath(v(9), heap)?
    };
    if boot.1 {
        boot = bootclasspath(field(toolchain, "_bootclasspath_info", heap)?, heap)?;
    }
    // The API explicitly takes a classpath with direct jars already prepended
    // (setCompileTimeClassPathEntriesWithPrependedDirectJars).
    let classpath = if !header_deps.is_empty() || classpath.is_empty() {
        header_deps
    } else {
        classpath
    };
    let tools = files(field(toolchain, "tools", heap)?, "tools", true)?;
    let (launcher, tool_inputs) = launcher(tool, toolchain, heap)?;
    let inputs = unique(
        additional
            .into_iter()
            .chain(boot.0.iter().cloned())
            .chain(source_jars.iter().cloned())
            .chain(sources.iter().cloned())
            .chain(tools)
            .chain(tool_inputs)
            .chain(classpath.iter().cloned()),
    );
    let mut flags = vec![
        "--output".to_owned(),
        output.path.clone(),
        "--header_compilation_output".to_owned(),
        tjar.path.clone(),
        "--output_deps".to_owned(),
        jdeps.path.clone(),
    ];
    paths(&mut flags, "--bootclasspath", &boot.0);
    paths(&mut flags, "--sources", &sources);
    paths(&mut flags, "--source_jars", &source_jars);
    if let Some(kind) = injecting {
        flags.extend(["--injecting_rule_kind".to_owned(), kind.to_owned()]);
    }
    if !options.is_empty() {
        flags.push("--javacopts".to_owned());
        flags.extend(options);
        flags.push("--".to_owned());
    }
    flags.extend([
        "--target_label".to_owned(),
        if target.starts_with('@') {
            format!("@{target}")
        } else {
            target
        },
    ]);
    paths(&mut flags, "--classpath", &classpath);
    flags.extend(["--reduce_classpath_mode".to_owned(), "NONE".to_owned()]);
    let spec = CommandLineSpec {
        executable: launcher.clone(),
        parts: vec![Part {
            groups: vec![Group::Strings(flags)],
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
    let environment = context.action_env.for_spawn(
        true,
        BTreeMap::from([(
            "LC_CTYPE".to_owned(),
            if cfg!(target_os = "macos") {
                "en_US.UTF-8"
            } else {
                "C.UTF-8"
            }
            .to_owned(),
        )]),
    );
    context
        .register_action(Registration {
            outputs: vec![output, jdeps.clone(), tjar],
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
                execution_info: BTreeMap::from([
                    ("supports-path-mapping".to_owned(), "1".to_owned()),
                    ("internal-inline-outputs".to_owned(), jdeps.path.clone()),
                ]),
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
    Ok(Value::new_none())
}

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
