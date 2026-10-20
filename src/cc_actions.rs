//! `cc_internal` analysis-time natives of the C++ compile/link surface
//! (CcStarlarkInternal.java): `actions2ctx_cheat`, the compile action factory
//! (`create_cc_compile_action`), the artifact-name helpers, the `_solib_`
//! symlink actions, `get_link_args` and the small Starlark-semantics helpers
//! that rules_cc 0.2.17's Starlarkified `cc_common.compile`/`link` call (N60).
//!
//! Every native registers plain native actions (crate::analysis_runtime)
//! whose argv comes from the feature configuration
//! (crate::cc_features::FeatureConfigurationData), so the executed compilers
//! and the produced bytes match genuine Bazel. Modules, libc_top, FDO,
//! propeller, stamped outputs and the LTO backends are unsupported and fail
//! explicitly (docs/CC_ACTIONS.md).

use crate::analysis_runtime::Context;
use crate::cc_features::{CcToolchainFeatures, FeatureConfiguration};
use crate::label::Label;
use crate::language_modules::{bind, failure, param, sequence, type_error};
use crate::starlark_types::{File, alloc_file, file_list, unpack_file};
use starlark::eval::{Arguments, Evaluator};
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, Value, ValueLike};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The cc_internal methods `crate::cc_actions` implements (including the
/// explicit unsupported-failure arms of `call`).
pub(crate) fn is_cc_actions_method(name: &str) -> bool {
    matches!(
        name,
        "absolute_symlink"
            | "actions2ctx_cheat"
            | "aspect_class"
            | "check_toplevel"
            | "collect_per_file_lto_backend_opts"
            | "compute_output_name_prefix_dir"
            | "create_cc_compile_action"
            | "create_cc_compile_action_template"
            | "create_lto_backend_action"
            | "create_lto_backend_action_template"
            | "declare_compile_output_file"
            | "declare_other_output_file"
            | "dynamic_library_soname"
            | "dynamic_library_symlink"
            | "dynamic_library_symlink2"
            | "expand_and_tokenize"
            | "get_artifact_name_extension_for_category"
            | "get_artifact_name_for_category"
            | "get_link_args"
            | "intern_seq"
            | "is_tree_artifact"
            | "per_file_copts"
            | "rule_class"
            | "solib_symlink_action"
            | "wrap_link_actions"
    )
}

/// `Actions.escapedPath` (actions/Actions.java:43-52).
fn escaped_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '_' => escaped.push_str("_U"),
            '/' => escaped.push_str("_S"),
            '\\' => escaped.push_str("_B"),
            ':' => escaped.push_str("_C"),
            '@' => escaped.push_str("_A"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// `SolibSymlinkAction.maybeHashPreserveExtension` (:249-261).
fn maybe_hash_preserve_extension(filename: &str) -> String {
    const MAX_FILENAME_LENGTH: usize = 255;
    if filename.len() <= MAX_FILENAME_LENGTH {
        return filename.to_owned();
    }
    use sha2::{Digest, Sha256};
    let hashed: String = Sha256::digest(filename.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    match filename.rsplit_once('.') {
        Some((_, extension)) if !extension.is_empty() => format!("{hashed}.{extension}"),
        _ => hashed,
    }
}

/// `SolibSymlinkAction.getDynamicLibrarySoname` (:306-319).
fn dynamic_library_soname(library_path: &str, preserve_name: bool, mnemonic: &str) -> String {
    if preserve_name {
        return library_path
            .rsplit_once('/')
            .map(|(_, base)| base.to_owned())
            .unwrap_or_else(|| library_path.to_owned());
    }
    let mnemonic_mangling = match mnemonic.find("ST-") {
        Some(at) => format!("{}_", &mnemonic[at..]),
        None => String::new(),
    };
    format!("lib{mnemonic_mangling}{}", escaped_path(library_path))
}

/// `SolibSymlinkAction.getMangledName` (:273-295).
fn mangled_name(
    label: &Label,
    solib_dir: &str,
    mnemonic: &str,
    library_path: &str,
    preserve_name: bool,
    prefix_consumer: bool,
) -> String {
    let escaped_rule_path = escaped_path(&format!("_{label}"));
    let soname = dynamic_library_soname(library_path, preserve_name, mnemonic);
    let join = |dir: &str, name: &str| {
        if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        }
    };
    if preserve_name {
        let parent = library_path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let escaped_library_path = escaped_path(&format!("_{parent}"));
        let escaped_full_path = if prefix_consumer {
            format!("{escaped_rule_path}__{escaped_library_path}")
        } else {
            escaped_library_path
        };
        let mangled_dir = join(
            solib_dir,
            &maybe_hash_preserve_extension(&escaped_full_path),
        );
        join(&mangled_dir, &soname)
    } else {
        let filename = if prefix_consumer {
            format!("{escaped_rule_path}__{soname}")
        } else {
            soname
        };
        join(solib_dir, &maybe_hash_preserve_extension(&filename))
    }
}

/// The CcToolchainInfo provider struct's `_toolchain_features` field
/// (CcToolchainProvider.getFeatures, CcToolchainProvider.java:389-391).
fn toolchain_features<'v>(
    function: &str,
    info: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<&'v CcToolchainFeatures> {
    let features = info
        .get_attr("_toolchain_features", heap)
        .ok()
        .flatten()
        .filter(|value| !value.is_none())
        .and_then(|value| CcToolchainFeatures::from_value(value));
    features.ok_or_else(|| type_error(function, "cc_toolchain", info, "CcToolchainInfo"))
}

/// A field of a Starlark provider struct, `None` when absent or None.
fn struct_field<'v>(info: Value<'v>, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    info.get_attr(name, heap)
        .ok()
        .flatten()
        .filter(|value| !value.is_none())
}

fn files_field<'v>(
    function: &str,
    info: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<File>> {
    struct_field(info, name, heap)
        .map(|value| {
            file_list(value).map_err(|error| failure(format!("{function}: {name}: {error}")))
        })
        .transpose()
        .map(|files| files.unwrap_or_default())
}

fn optional_file<'v>(
    function: &str,
    name: &str,
    value: Option<Value<'v>>,
) -> starlark::Result<Option<File>> {
    value
        .filter(|value| !value.is_none())
        .map(|value| {
            unpack_file(value).map_err(|error| failure(format!("{function}: {name}: {error}")))
        })
        .transpose()
}

/// `create_cc_compile_action`'s CcCompilationContext payload: the declared
/// headers and module-map inputs (CcCompilationContext.java:155-170, :477-483).
struct CompilationContextInputs {
    headers: Vec<File>,
    non_code_inputs: Vec<File>,
    direct_module_maps: Vec<File>,
    module_map: Option<File>,
}

fn compilation_context_inputs<'v>(
    function: &str,
    info: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<CompilationContextInputs> {
    let module_map = struct_field(info, "_module_map", heap)
        .map(|value| -> starlark::Result<File> {
            // CppModuleMap.getArtifact: the Starlark struct's `file` field.
            value
                .get_attr("file", heap)
                .ok()
                .flatten()
                .map(unpack_file)
                .transpose()?
                .ok_or_else(|| failure(format!("{function}: _module_map has no file")))
        })
        .transpose()?;
    Ok(CompilationContextInputs {
        headers: files_field(function, info, "headers", heap)?,
        non_code_inputs: files_field(function, info, "_non_code_inputs", heap)?,
        direct_module_maps: files_field(function, info, "_direct_module_maps", heap)?,
        module_map,
    })
}

/// `CppCompileActionBuilder.getActionName` (CppCompileActionBuilder.java:
/// 2028-2056) over CppFileTypes (case-sensitive suffix matching).
fn compile_action_name(action_name: Option<&str>, source: &File) -> String {
    if let Some(name) = action_name {
        return name.to_owned();
    }
    let path = &source.path;
    let has = |extension: &str| path.ends_with(extension);
    if has(".cppmap") {
        "c++-module-map".to_owned()
    } else if [
        ".h", ".hh", ".hpp", ".ipp", ".hxx", ".h++", ".inc", ".inl", ".tlh", ".tli", ".H", ".tcc",
    ]
    .iter()
    .any(|extension| has(extension))
    {
        "c++-header-parsing".to_owned()
    } else if has(".c") {
        "c-compile".to_owned()
    } else if [".cc", ".cpp", ".cxx", ".c++", ".C", ".cu", ".cl"]
        .iter()
        .any(|extension| has(extension))
    {
        "c++-compile".to_owned()
    } else if has(".m") {
        "objc-compile".to_owned()
    } else if has(".mm") {
        "objc++-compile".to_owned()
    } else if (has(".s") && !has(".pic.s")) || has(".asm") {
        "assemble".to_owned()
    } else if has(".S") {
        "preprocess-assemble".to_owned()
    } else if has(".ipb") {
        "clif-match".to_owned()
    } else if has(".pcm") || has(".gcm") || has(".ifc") {
        "c++-module-codegen".to_owned()
    } else {
        "c++-compile".to_owned()
    }
}

/// `CppCompileAction.actionNameToMnemonic` (CppCompileAction.java:1993-2018)
/// over the actions the native profile creates (the header mnemonic flag
/// `--experimental_use_cpp_compile_header_mnemonic` is off by default).
fn compile_mnemonic(action_name: &str) -> &'static str {
    match action_name {
        "objc-compile" | "objc++-compile" => "ObjcCompile",
        "linkstamp-compile" => "CppLinkstampCompile",
        "c++-header-analysis" => "CppHeaderAnalysis",
        "c++-module-deps-scanning" => "CppDepsScanning",
        _ => "CppCompile",
    }
}

pub(crate) fn call<'v>(
    method: &str,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    // The analysis Context borrows the evaluator; arms that expand variables
    // need the evaluator mutably, so they re-derive it after their last
    // mutable use (context() is a plain downcast of eval.extra).
    let context = crate::analysis_runtime::context(eval)
        .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
    match method {
        "actions2ctx_cheat" => {
            // CcStarlarkInternal.getStarlarkRuleContext (:486-494).
            let bound = bind(method, &[param("actions", true, false, true)], args, heap)?;
            let actions = bound[0].expect("required");
            crate::analysis_runtime::actions_rule_ctx(actions).ok_or_else(|| {
                failure(
                    "cc_internal.actions2ctx_cheat: the action factory has no rule context in the native profile"
                        .to_owned(),
                )
            })
        }
        "rule_class" => {
            // CcStarlarkInternal.getRuleClass (:504-510).
            bind(method, &[param("ctx", true, false, true)], args, heap)?;
            Ok(heap.alloc(context.rule_class.clone()))
        }
        "aspect_class" => {
            // CcStarlarkInternal.getAspectClass (:512-528): None outside an
            // aspect application; the applying aspect's name without its file.
            let bound = bind(method, &[param("ctx", true, false, true)], args, heap)?;
            let ctx = bound[0].expect("required");
            let name = ctx
                .downcast_ref::<crate::analysis_ctx::AspectCtx>()
                .and_then(|aspect| {
                    aspect
                        .ctx
                        .backing
                        .get_attr("$aspect_ids", heap)
                        .ok()
                        .flatten()
                        .and_then(|ids| {
                            ids.iterate(heap)
                                .ok()
                                .and_then(|values| {
                                    values
                                        .map(|value| value.unpack_str().map(str::to_owned))
                                        .collect::<Option<Vec<_>>>()
                                })
                        })
                })
                .and_then(|ids| ids.last().cloned())
                .and_then(|id| id.rsplit_once('%').map(|(_, name)| name.to_owned()));
            Ok(name.map_or_else(Value::new_none, |name| heap.alloc(name)))
        }
        "check_toplevel" => {
            // CcStarlarkInternal.checkToplevel (:962-970).
            let bound = bind(method, &[param("fn", true, false, true)], args, heap)?;
            let function = bound[0].expect("required");
            if crate::analysis_runtime::owned_top_level_function(eval, function).is_none() {
                return Err(failure(
                    "Passed function must be top-level functions.".to_owned(),
                ));
            }
            Ok(Value::new_none())
        }
        "intern_seq" => {
            // CcStarlarkInternal.internList (:399-405): the weak interner's
            // observable effect is value equality, which native values have.
            let bound = bind(method, &[param("seq", true, false, true)], args, heap)?;
            let value = bound[0].expect("required");
            let items: Vec<Value<'v>> = match TupleRef::from_value(value) {
                Some(tuple) => tuple.iter().collect(),
                None => sequence(method, "seq", value)?,
            };
            Ok(heap.alloc(AllocTuple(items)))
        }
        "is_tree_artifact" => {
            // CcStarlarkInternal.isTreeArtifact (:544-554).
            let bound = bind(method, &[param("artifact", true, false, true)], args, heap)?;
            let artifact = unpack_file(bound[0].expect("required"))
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            Ok(Value::new_bool(artifact.is_directory))
        }
        "compute_output_name_prefix_dir" => {
            // CcStarlarkInternal.computeOutputNamePrefixDir (:556-576): objc
            // purposes only; everything else is the empty prefix.
            let bound = bind(
                method,
                &[
                    param("configuration", false, true, true),
                    param("purpose", false, true, true),
                ],
                args,
                heap,
            )?;
            let purpose_value = bound[1].expect("required");
            let purpose = purpose_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "purpose", purpose_value, "string"))?;
            Ok(heap.alloc(if purpose.ends_with("_objc_arc") {
                if purpose.ends_with("_non_objc_arc") {
                    "non_arc"
                } else {
                    "arc"
                }
            } else {
                ""
            }))
        }
        "per_file_copts" => {
            // CcStarlarkInternal.perFileCopts (:972-991): --per_file_copt is
            // unmodelled, so no options match.
            bind(
                method,
                &[
                    param("cpp_configuration", true, false, true),
                    param("source_file", true, false, true),
                    param("label", true, false, true),
                ],
                args,
                heap,
            )?;
            Ok(heap.alloc(Vec::<String>::new()))
        }
        "wrap_link_actions" => {
            // CcStarlarkInternal.wrapLinkActions (:466-484): the wrapped
            // factory differs from the plain one only for shareable
            // artifacts (WrappedStarlarkActionFactory.createShareableArtifact).
            let bound = bind(
                method,
                &[
                    param("actions", true, false, true),
                    param("build_configuration", true, false, false),
                    param("sharable_artifacts", true, false, false),
                ],
                args,
                heap,
            )?;
            let actions = bound[0].expect("required");
            if let Some(config) = bound[1].filter(|value| !value.is_none())
                && config.downcast_ref::<crate::ctx_surface::Configuration>().is_none()
            {
                return Err(type_error(
                    method,
                    "build_configuration",
                    config,
                    "configuration or NoneType",
                ));
            }
            if bound[2].is_some_and(|shareable| shareable.unpack_bool() == Some(true)) {
                return Err(failure(
                    "cc_internal.wrap_link_actions with sharable_artifacts is not implemented in the native profile (shareable link artifacts, docs/CC_ACTIONS.md)".to_owned(),
                ));
            }
            Ok(actions)
        }
        "get_artifact_name_for_category" | "get_artifact_name_extension_for_category" => {
            // CcStarlarkInternal (:345-374) over CcToolchainFeatures.
            // getArtifactNameForCategory (:1338-1352).
            let extension_only = method.ends_with("extension_for_category");
            let mut params = vec![
                param("cc_toolchain", false, true, true),
                param("category", false, true, true),
            ];
            if !extension_only {
                params.push(param("output_name", false, true, true));
            }
            let bound = bind(method, &params, args, heap)?;
            let toolchain_value = bound[0].expect("required");
            let category_value = bound[1].expect("required");
            let category = category_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "category", category_value, "string"))?
                .to_owned();
            let features = toolchain_features(method, toolchain_value, heap)?;
            let data = features.data();
            let name = if extension_only {
                data.artifact_name_extension_for_category(&category)
            } else {
                let output_value = bound[2].expect("required");
                let output = output_value
                    .unpack_str()
                    .ok_or_else(|| type_error(method, "output_name", output_value, "string"))?;
                data.artifact_name_for_category(&category, output)
            }
            .map_err(failure)?;
            Ok(heap.alloc(name))
        }
        "declare_compile_output_file" => {
            // CcStarlarkInternal.declareCompileOutputFile (:993-1009) over
            // CppHelper.getCompileOutputArtifact (CppHelper.java:82-91):
            // <package exec path>/_objs/<name>/<output_name> below bin.
            let bound = bind(
                method,
                &[
                    param("ctx", false, true, true),
                    param("label", false, true, true),
                    param("output_name", false, true, true),
                    param("configuration", false, true, true),
                ],
                args,
                heap,
            )?;
            let label_value = bound[1].expect("required");
            let label = label_of(method, label_value)?;
            let output_value = bound[2].expect("required");
            let output_name = output_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "output_name", output_value, "string"))?;
            // Context::declare places the name below the analysis label's
            // package, so the package appears exactly once
            // (getUniqueDirectory, AnalysisUtils.java:147-153).
            let _ = label.package_exec_path();
            let path = format!("_objs/{}/{}", label.name, output_name);
            let file = context
                .declare(&path)
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            Ok(alloc_file(heap, file))
        }
        "declare_other_output_file" => {
            // CcStarlarkInternal.declareOtherOutputFile (:1011-1025): a
            // sibling of the object file's directory.
            let bound = bind(
                method,
                &[
                    param("ctx", false, true, true),
                    param("output_name", false, true, true),
                    param("object_file", false, true, true),
                ],
                args,
                heap,
            )?;
            let output_value = bound[1].expect("required");
            let output_name = output_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "output_name", output_value, "string"))?;
            let object = unpack_file(bound[2].expect("required"))
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            let parent = object
                .short_path
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            let path = if parent.is_empty() {
                output_name.to_owned()
            } else {
                format!("{parent}/{output_name}")
            };
            let file = context
                .declare(&path)
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            Ok(alloc_file(heap, file))
        }
        "get_link_args" => {
            // CcStarlarkInternal.getArgs (:407-440): an Args value expanding
            // to the LinkCommandLine's parameter command line (the flags, not
            // the tool path; the spawn's `executable` is the tool).
            let bound = bind(
                method,
                &[
                    param("action_name", false, true, true),
                    param("feature_configuration", false, true, true),
                    param("build_variables", false, true, true),
                    param("parameter_file_type", false, true, true),
                ],
                args,
                heap,
            )?;
            let action_value = bound[0].expect("required");
            let action = action_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "action_name", action_value, "string"))?
                .to_owned();
            let configuration = feature_configuration(method, bound[1].expect("required"))?;
            let variables = bound[2].expect("required");
            if !crate::cc_variables::is_variables(variables) {
                return Err(type_error(method, "build_variables", variables, "Variables"));
            }
            let parameter_file_type_value = bound[3].expect("required");
            let parameter_file_type = parameter_file_type_value
                .unpack_str()
                .ok_or_else(|| {
                    type_error(method, "parameter_file_type", parameter_file_type_value, "string")
                })?
                .to_owned();
            let data = Arc::clone(&configuration.data);
            let active = context.active.clone();
            let _ = context;
            data.require_action_config(&action).map_err(failure)?;
            let expanded = data.command_line(&action, variables, eval)?;
            // getParamFileInfo (LinkCommandLine.java:132-150): a flag
            // containing the placeholder turns the whole command line into an
            // always-on parameter file whose reference argument is the flag
            // with the placeholder replaced by %s (% doubled first).
            let param_info = expanded
                .iter()
                .find(|flag| flag.contains("LINKER_PARAM_FILE_PLACEHOLDER"))
                .map(|flag| {
                    let flag_format = flag
                        .replace('%', "%%")
                        .replace("LINKER_PARAM_FILE_PLACEHOLDER", "%s");
                    // Bazel's ParameterFileType of finalize_link_action's
                    // _can_split_command_line: GCC_QUOTED
                    // (gcc_quoting_for_param_files), WINDOWS, else UNQUOTED.
                    // Windows quoting is a Windows-toolchain path (unsupported
                    // platform); GCC quoting is the native Shell writer.
                    let file_type = match parameter_file_type.as_str() {
                        "GCC_QUOTED" => crate::command_line::ParamFileType::Shell,
                        "UNQUOTED" => crate::command_line::ParamFileType::Unquoted,
                        other => {
                            return Err(failure(format!(
                                "cc_internal.get_link_args with parameter file type '{other}' is not implemented in the native profile (Windows toolchains)"
                            )));
                        }
                    };
                    Ok::<_, starlark::Error>(crate::command_line::ParamFileInfo {
                        flag_format,
                        always: true,
                        file_type,
                        flags_only: false,
                    })
                })
                .transpose()?;
            // getParamCommandLine (:92-130): filter the flags containing the
            // linker_param_file variable's value (the placeholder) out of the
            // parameter command line.
            let mut flags = expanded;
            if let Some(param_file) = crate::cc_variables::linker_param_file(variables, eval)? {
                flags.retain(|flag| !flag.contains(&param_file));
            }
            let args_value = crate::args::alloc_args(heap, active);
            let args = args_value
                .downcast_ref::<crate::args::Args>()
                .expect("fresh Args");
            if let Some(info) = &param_info {
                crate::args::set_param_file_info(args, info.clone())?;
            }
            crate::args::add_strings(args, flags).map_err(|error| {
                failure(format!("cc_internal.{method}: {error}"))
            })?;
            Ok(args_value)
        }
        "dynamic_library_soname" => {
            // CcStarlarkInternal.dynamicLibrarySoname (:239-254).
            let bound = bind(
                method,
                &[
                    param("actions", true, false, true),
                    param("path", true, false, true),
                    param("preserve_name", true, false, true),
                ],
                args,
                heap,
            )?;
            let path_value = bound[1].expect("required");
            let path = path_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "path", path_value, "string"))?;
            let preserve_value = bound[2].expect("required");
            let preserve = preserve_value
                .unpack_bool()
                .ok_or_else(|| type_error(method, "preserve_name", preserve_value, "bool"))?;
            Ok(heap.alloc(dynamic_library_soname(
                path,
                preserve,
                &context.output_root,
            )))
        }
        "dynamic_library_symlink" | "dynamic_library_symlink2" => {
            // CcStarlarkInternal (:204-237) over
            // SolibSymlinkAction.getDynamicLibrarySymlink (:158-202).
            let second = method.ends_with('2');
            let mut params = vec![
                param("actions", true, false, true),
                param("library", true, false, true),
                param("solib_directory", true, false, true),
            ];
            if second {
                params.push(param("path", true, false, true));
            } else {
                params.push(param("preserve_name", true, false, true));
                params.push(param("prefix_consumer", true, false, true));
            }
            let bound = bind(method, &params, args, heap)?;
            let library = unpack_file(bound[1].expect("required"))
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            let solib_value = bound[2].expect("required");
            let solib = solib_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "solib_directory", solib_value, "string"))?
                .to_owned();
            let link_path = if second {
                let path_value = bound[3].expect("required");
                let path = path_value
                    .unpack_str()
                    .ok_or_else(|| type_error(method, "path", path_value, "string"))?;
                if solib.is_empty() {
                    path.to_owned()
                } else {
                    format!("{solib}/{path}")
                }
            } else {
                let preserve_value = bound[3].expect("required");
                let preserve = preserve_value
                    .unpack_bool()
                    .ok_or_else(|| type_error(method, "preserve_name", preserve_value, "bool"))?;
                let prefix_value = bound[4].expect("required");
                let prefix_consumer = prefix_value
                    .unpack_bool()
                    .ok_or_else(|| type_error(method, "prefix_consumer", prefix_value, "bool"))?;
                mangled_name(
                    &context.label,
                    &solib,
                    &context.output_root,
                    &library.short_path,
                    preserve,
                    prefix_consumer,
                )
            };
            Ok(alloc_file(heap, solib_symlink(context, &library, &link_path)?))
        }
        "solib_symlink_action" => {
            // CcStarlarkInternal.solibSymlinkAction (:186-202) over
            // SolibSymlinkAction.getCppRuntimeSymlink (:209-222): the
            // runtime library keeps its basename below the override dir.
            let bound = bind(
                method,
                &[
                    param("ctx", false, true, true),
                    param("artifact", false, true, true),
                    param("solib_directory", false, true, true),
                    param("runtime_solib_dir_base", false, true, true),
                ],
                args,
                heap,
            )?;
            let library = unpack_file(bound[1].expect("required"))
                .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
            let solib_value = bound[2].expect("required");
            let solib = solib_value
                .unpack_str()
                .ok_or_else(|| type_error(method, "solib_directory", solib_value, "string"))?
                .to_owned();
            let base = library
                .short_path
                .rsplit_once('/')
                .map(|(_, base)| base)
                .unwrap_or(&library.short_path)
                .to_owned();
            let link_path = if solib.is_empty() {
                base
            } else {
                format!("{solib}/{base}")
            };
            Ok(alloc_file(heap, solib_symlink(context, &library, &link_path)?))
        }
        "absolute_symlink" => {
            // CcStarlarkInternal.absoluteSymlink (:376-395): an FDO-profile
            // absolute symlink; FDO is unsupported, so nothing can use it.
            bind(
                method,
                &[
                    param("ctx", false, true, true),
                    param("output", false, true, true),
                    param("target_path", false, true, true),
                    param("progress_message", false, true, true),
                ],
                args,
                heap,
            )?;
            Err(failure(
                "cc_internal.absolute_symlink is not implemented in the native profile (FDO profiles with absolute paths)".to_owned(),
            ))
        }
        "create_cc_compile_action" => {
            let _ = context;
            create_cc_compile_action(args, eval, method, heap)?;
            Ok(Value::new_none())
        }
        "create_cc_compile_action_template" | "create_lto_backend_action"
        | "create_lto_backend_action_template" | "collect_per_file_lto_backend_opts" => {
            let what = match method {
                "create_cc_compile_action_template" => "tree-artifact compile action templates",
                "collect_per_file_lto_backend_opts" => "LTO backend options",
                _ => "LTO backend actions",
            };
            Err(failure(format!(
                "cc_internal.{method} is not implemented in the native profile ({what})"
            )))
        }
        "expand_and_tokenize" => Err(failure(
            "cc_internal.expand_and_tokenize is not implemented in the native profile (objc flag expansion)".to_owned(),
        )),
        other => Err(failure(format!(
            "cc_internal.{other} is not implemented in the native profile (analysis-time C++ API)"
        ))),
    }
}

fn feature_configuration<'v>(
    function: &str,
    value: Value<'v>,
) -> starlark::Result<&'v FeatureConfiguration> {
    FeatureConfiguration::from_value(value).ok_or_else(|| {
        type_error(
            function,
            "feature_configuration",
            value,
            "FeatureConfiguration",
        )
    })
}

/// A Label parameter (declare_compile_output_file's `label`).
fn label_of<'v>(function: &str, value: Value<'v>) -> starlark::Result<Label> {
    let text = crate::starlark_types::unpack_label(value)
        .ok_or_else(|| type_error(function, "label", value, "Label"))?;
    Label::parse_canonical(&text).map_err(|_| type_error(function, "label", value, "Label"))
}

/// SolibSymlinkAction.getDynamicLibrarySymlinkInternal (:228-245): declare
/// the symlink below the bin directory and register the SolibSymlink action
/// (a real symlink to the input's absolute execroot path).
fn solib_symlink(context: &Context, library: &File, link_path: &str) -> anyhow::Result<File> {
    use anyhow::{bail, ensure};
    let name = library
        .short_path
        .rsplit_once('/')
        .map(|(_, base)| base)
        .unwrap_or(&library.short_path);
    // Link.SHARED_LIBRARY_FILETYPES: versioned and plain .so and no extension.
    let shared_library = name.ends_with(".so")
        || name.split_once(".so.").is_some_and(|(prefix, rest)| {
            !prefix.is_empty()
                && !rest.is_empty()
                && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
        || !name.contains('.');
    ensure!(
        shared_library,
        "Library '{name}' does not match expected filetype"
    );
    if library.short_path.starts_with("_solib_") {
        bail!("cc_internal dynamic library symlink of '{name}': already a solib symlink");
    }
    let symlink = context.declare(link_path)?;
    context.register_action(crate::analysis_runtime::Registration {
        outputs: vec![symlink.clone()],
        inputs: vec![library.clone()],
        command: String::new(),
        write: None,
        program: None,
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        extra: crate::action::ActionExtra {
            transform: Some(crate::action::Transform::Symlink {
                target: library.path.clone().into(),
                require_executable: false,
                progress: format!("SolibSymlink {}", crate::runfiles::root_relative(&symlink)),
            }),
            mnemonic: "SolibSymlink".to_owned(),
            ..Default::default()
        },
        shell_pad: false,
        executable_file: false,
        deferred: None,
        exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
        runfiles_trees: Vec::new(),
        build_info: None,
    })?;
    Ok(symlink)
}

/// `cc_internal.create_cc_compile_action`
/// (CcStarlarkInternal.java:578-757 over CppCompileActionBuilder): a native
/// CppCompile spawn whose argv is the feature configuration's tool and
/// per-feature expansions (CompileCommandLine.java:66-155), with the
/// mandatory inputs of buildMandatoryInputs (CppCompileActionBuilder.java:
/// 348-396) plus the prunable headers of discoverInputs.
#[allow(clippy::too_many_lines)]
fn create_cc_compile_action<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
    method: &str,
    heap: Heap<'v>,
) -> starlark::Result<()> {
    const F: &str = "create_cc_compile_action";
    let params: Vec<_> = [
        ("action_construction_context", false),
        ("cc_compilation_context", false),
        ("cc_toolchain", false),
        ("configuration", false),
        ("copts_filter", true),
        ("feature_configuration", false),
        ("source", false),
        ("additional_compilation_inputs", true),
        ("additional_compilation_inputs_set", true),
        ("additional_include_scanning_roots", true),
        ("output_file", true),
        ("dotd_file", true),
        ("diagnostics_file", true),
        ("gcno_file", true),
        ("dwo_file", true),
        ("lto_indexing_file", true),
        ("use_pic", true),
        ("compile_build_variables", false),
        ("cache_key_inputs", true),
        ("build_info_header_files", true),
        ("action_name", true),
        ("should_scan_includes", true),
        ("shareable", true),
        ("module_files", true),
        ("modmap_file", true),
        ("modmap_input_file", true),
        ("additional_outputs", true),
        ("needs_include_validation", true),
        ("toolchain_type", false),
    ]
    .into_iter()
    .map(|(name, optional)| param(name, false, true, !optional))
    .collect();
    let bound = bind(F, &params, args, heap)?;
    let context = crate::analysis_runtime::context(eval)
        .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;

    let compilation_info = bound[1].expect("required");
    let toolchain_info = bound[2].expect("required");
    let configuration_value = bound[3].expect("required");
    if configuration_value
        .downcast_ref::<crate::ctx_surface::Configuration>()
        .is_none()
    {
        return Err(type_error(
            F,
            "configuration",
            configuration_value,
            "configuration",
        ));
    }
    let configuration = feature_configuration(F, bound[5].expect("required"))?;
    let source = unpack_file(bound[6].expect("required"))
        .map_err(|error| failure(format!("cc_internal.{F}: {error}")))?;
    let optional_file = |index: usize, name: &str| {
        optional_file(F, name, bound[index].filter(|value| !value.is_none()))
    };
    let files_arg = |index: usize, name: &str| -> starlark::Result<Vec<File>> {
        bound[index]
            .filter(|value| !value.is_none())
            .map(|value| {
                file_list(value)
                    .map_err(|error| failure(format!("cc_internal.{F}: {name}: {error}")))
            })
            .transpose()
            .map(|files| files.unwrap_or_default())
    };

    // CoptsFilter (CcStarlarkInternal.createCoptsFilter, :759-770).
    let copts_filter = match bound[4].filter(|value| !value.is_none()) {
        Some(value) => {
            let pattern = value
                .unpack_str()
                .ok_or_else(|| type_error(F, "copts_filter", value, "string or NoneType"))?;
            Some(fancy_regex::Regex::new(pattern).map_err(|error| {
                failure(format!(
                    "invalid regular expression '{pattern}': {}",
                    crate::engine::query::java_pattern_error(pattern, &error)
                ))
            })?)
        }
        None => None,
    };

    // Unsupported semantics fail explicitly before anything is registered.
    if bound[22].is_some_and(|shareable| shareable.unpack_bool() == Some(true)) {
        return Err(failure(
            "cc_internal.create_cc_compile_action with shareable artifacts is not implemented in the native profile".to_owned(),
        ));
    }
    if bound[21].is_some_and(|scan| scan.unpack_bool() == Some(true)) {
        return Err(failure(
            "cc_internal.create_cc_compile_action with should_scan_includes is not implemented in the native profile (include scanning)".to_owned(),
        ));
    }
    if !files_arg(23, "module_files")?.is_empty() {
        return Err(failure(
            "cc_internal.create_cc_compile_action with module_files is not implemented in the native profile (C++20 modules)".to_owned(),
        ));
    }
    let modmap = optional_file(24, "modmap_file")?;
    let modmap_input = optional_file(25, "modmap_input_file")?;
    if modmap.is_some() || modmap_input.is_some() {
        return Err(failure(
            "cc_internal.create_cc_compile_action with a module map is not implemented in the native profile".to_owned(),
        ));
    }

    // buildAndVerify (:256-291): the action config and its tool requirements.
    let action_name = compile_action_name(
        bound[20]
            .filter(|value| !value.is_none())
            .and_then(|name| name.unpack_str()),
        &source,
    );
    let data = Arc::clone(&configuration.data);
    data.require_action_config(&action_name).map_err(failure)?;

    // The compilation context payload (declared headers and module maps).
    let compilation = compilation_context_inputs(F, compilation_info, heap)?;

    // The toolchain file groups: with --use_specific_tool_files off (the
    // default) every action stages all_files; include scanning (which would
    // use compiler_files_without_includes) just failed explicitly above.
    let mut inputs = files_arg(7, "additional_compilation_inputs")?;
    let mut push = |file: File| {
        if !inputs.contains(&file) {
            inputs.push(file);
        }
    };
    for file in files_arg(8, "additional_compilation_inputs_set")? {
        push(file);
    }
    // getBuiltinIncludeFiles over build_info_header_files (:348-370).
    let mut builtin_includes = files_field(F, toolchain_info, "_builtin_include_files", heap)?;
    for file in files_arg(19, "build_info_header_files")? {
        if !builtin_includes.contains(&file) {
            builtin_includes.push(file);
        }
    }
    for file in builtin_includes {
        push(file);
    }
    for file in compilation
        .headers
        .iter()
        .chain(&compilation.non_code_inputs)
        .chain(&compilation.direct_module_maps)
    {
        push(file.clone());
    }
    if let Some(map) = &compilation.module_map {
        push(map.clone());
    }
    push(source.clone());
    if let Some(grep) = struct_field(toolchain_info, "_grep_includes", heap)
        .map(|value| {
            unpack_file(value).map_err(|error| failure(format!("{F}: _grep_includes: {error}")))
        })
        .transpose()?
    {
        push(grep);
    }
    for file in files_field(F, toolchain_info, "all_files", heap)? {
        push(file);
    }
    for file in files_arg(18, "cache_key_inputs")? {
        push(file);
    }

    // Outputs (setOutputs, :940-943).
    let mut outputs = Vec::new();
    outputs.extend(
        [
            optional_file(10, "output_file")?,
            optional_file(11, "dotd_file")?,
            optional_file(12, "diagnostics_file")?,
            optional_file(13, "gcno_file")?,
            optional_file(14, "dwo_file")?,
            optional_file(15, "lto_indexing_file")?,
        ]
        .into_iter()
        .flatten(),
    );
    outputs.extend(files_arg(26, "additional_outputs")?);
    if outputs.is_empty() {
        return Err(failure(format!(
            "cc_internal.{F}: the action must have at least one output"
        )));
    }

    // CompileCommandLine: the tool plus the per-feature expansions, the
    // copts filter applied to everything but unfiltered_compile_flags
    // (CompileCommandLine.java:66-155).
    let variables = bound[17].expect("required");
    if !crate::cc_variables::is_variables(variables) {
        return Err(type_error(
            F,
            "compile_build_variables",
            variables,
            "Variables",
        ));
    }
    let tool = data.tool_path(&action_name).map_err(failure)?;
    let action_env = context.action_env.clone();
    let label_owner = context.label.clone();
    let active_owner = context.active.clone();
    let _ = context;
    let mut arguments = vec![tool];
    for (owner, flags) in data.per_feature_expansions(&action_name, variables, eval)? {
        for flag in flags {
            // CcCommon.CoptsFilter.passesFilter (CcCommon.java:122-130): a
            // flag survives unless the nocopts regex FULLY matches it; the
            // unfiltered_compile_flags feature is exempt.
            if owner == "unfiltered_compile_flags"
                || copts_filter
                    .as_ref()
                    .is_none_or(|filter| passes_filter(filter, &flag))
            {
                arguments.push(flag);
            }
        }
    }
    let environment_feature = data.environment(&action_name, variables, eval)?;
    let context = crate::analysis_runtime::context(eval)
        .map_err(|error| failure(format!("cc_internal.{method}: {error}")))?;
    let _ = (&label_owner, &active_owner);

    // CppCompileAction.getEffectiveEnvironment (:1010-1027): the
    // configuration's action environment (fixed and inherited names), then
    // PWD put (overriding, unless requires-darwin), then the feature
    // variables putAll (which may override PWD again).
    let action_environment = action_env.for_spawn(true, BTreeMap::new());
    let mut environment = action_environment.fixed;
    environment.insert("PWD".to_owned(), "/proc/self/cwd".to_owned());
    for (key, value) in environment_feature {
        environment.insert(key, value);
    }

    // The action config's execution requirements (buildAndVerify :262-268);
    // the rule tags apply like every native action.
    let mut execution_info = BTreeMap::new();
    for requirement in data.tool_requirements(&action_name).map_err(failure)? {
        execution_info.insert(requirement, String::new());
    }
    let mnemonic = compile_mnemonic(&action_name);
    context
        .register_action(crate::analysis_runtime::Registration {
            outputs,
            inputs,
            command: String::new(),
            write: None,
            program: arguments.first().cloned(),
            arguments: arguments[1..].to_vec(),
            environment,
            extra: crate::action::ActionExtra {
                execution_info,
                mnemonic: mnemonic.to_owned(),
                // The configuration's inherited --action_env names
                // (ActionEnvironment.getInheritedEnv).
                inherited_environment: action_environment.inherited,
                ..Default::default()
            },
            shell_pad: false,
            executable_file: false,
            deferred: None,
            exec_group: crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
            runfiles_trees: Vec::new(),
            build_info: None,
        })
        .map_err(|error| failure(format!("cc_internal.{F}: {error}")))?;
    Ok(())
}

/// CcCommon.CoptsFilter.passesFilter (CcCommon.java:122-130): the flag passes
/// unless the nocopts regex fully matches it (Pattern.matcher.matches, not
/// find).
fn passes_filter(filter: &fancy_regex::Regex, flag: &str) -> bool {
    let anchored = format!("^(?:{})$", filter.as_str());
    match fancy_regex::Regex::new(&anchored) {
        Ok(full) => !full.is_match(flag).unwrap_or(false),
        // Unanchorable patterns fall back to a find (Bazel compiles Java
        // regexes; the native profile cannot represent a full match of a
        // pattern that fancy-regex cannot re-anchor, so it filters less).
        Err(_) => !filter.is_match(flag).unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copts_filter_passes_only_non_full_matches() {
        // CcCommon.CoptsFilter.passesFilter: full-match, inverted
        // (Pattern.matcher.matches, not find).
        let filter = fancy_regex::Regex::new("-Wno-unused").unwrap();
        assert!(passes_filter(&filter, "-Wall"));
        assert!(!passes_filter(&filter, "-Wno-unused"));
        // A substring hit that is not a full match passes.
        assert!(passes_filter(&filter, "-Wno-unused=extra"));
        assert!(passes_filter(&filter, "x-Wno-unused"));
        // A prefix pattern covers every flag it starts with.
        let prefix = fancy_regex::Regex::new("-Wno.*").unwrap();
        assert!(!passes_filter(&prefix, "-Wno-unused=extra"));
        // The empty regex fully matches only the empty flag.
        let empty = fancy_regex::Regex::new("").unwrap();
        assert!(!passes_filter(&empty, ""));
        assert!(passes_filter(&empty, "-Wall"));
    }

    #[test]
    fn path_escaping_matches_actions_escaped_path() {
        // Actions.java:43-52.
        assert_eq!(
            escaped_path("//cdep:prebuilt_shared"),
            "_S_Scdep_Cprebuilt_Ushared"
        );
        assert_eq!(escaped_path("_cdep"), "_Ucdep");
        assert_eq!(escaped_path("cdep/libbuilt.so"), "cdep_Slibbuilt.so");
        assert_eq!(escaped_path("a@b\\c"), "a_Ab_Bc");
    }

    #[test]
    fn solib_sonames_follow_solib_symlink_action() {
        // getDynamicLibrarySoname (:306-319): preserved basename, else the
        // mangled lib name with the ST- configuration suffix.
        assert_eq!(
            dynamic_library_soname("cdep/libbuilt.so", false, "k8-fastbuild"),
            "libcdep_Slibbuilt.so"
        );
        assert_eq!(
            dynamic_library_soname("cdep/libbuilt.so", false, "k8-fastbuild-ST-deadbeef"),
            "libST-deadbeef_cdep_Slibbuilt.so"
        );
        assert_eq!(
            dynamic_library_soname("cdep/libprebuilt.so", true, "k8-fastbuild"),
            "libprebuilt.so"
        );
    }

    #[test]
    fn solib_mangled_names_match_the_genuine_capture() {
        // The genuine aquery of the N60 fixture:
        // _solib_k8/libcdep_Slibbuilt.so (cc_library's own .so, NODEPS,
        // not preserved, no consumer) and
        // _solib_k8/_U_S_Scdep_Cprebuilt_Ushared___Ucdep/libprebuilt.so
        // (cc_import's shared library, preserved with the consumer prefix).
        let label = Label::parse_canonical("//cdep:built").unwrap();
        assert_eq!(
            mangled_name(
                &label,
                "_solib_k8",
                "k8-fastbuild",
                "cdep/libbuilt.so",
                false,
                false
            ),
            "_solib_k8/libcdep_Slibbuilt.so"
        );
        let label = Label::parse_canonical("//cdep:prebuilt_shared").unwrap();
        assert_eq!(
            mangled_name(
                &label,
                "_solib_k8",
                "k8-fastbuild",
                "cdep/libprebuilt.so",
                true,
                true
            ),
            "_solib_k8/_U_S_Scdep_Cprebuilt_Ushared___Ucdep/libprebuilt.so"
        );
    }

    #[test]
    fn long_mangled_names_are_hashed_with_extension_kept() {
        // maybeHashPreserveExtension (:249-261) over a 255+ character name.
        let label = Label::parse_canonical("//pkg:t").unwrap();
        let library_path = format!("cdep/{}.so", "x".repeat(300));
        let mangled = mangled_name(
            &label,
            "_solib_k8",
            "k8-fastbuild",
            &library_path,
            false,
            false,
        );
        let name = mangled.rsplit('/').next().unwrap();
        // The hash replaces the whole over-long name, "lib" prefix included
        // (:249-261).
        assert!(!name.starts_with("lib"), "{name}");
        assert!(name.len() < 100, "{}", name.len());
        assert!(name.ends_with(".so"));
    }

    #[test]
    fn compile_action_names_follow_cpp_file_types() {
        let action = |path: &str| {
            compile_action_name(
                None,
                &File {
                    path: path.to_owned(),
                    short_path: path.to_owned(),
                    owner: String::new(),
                    is_source: true,
                    is_directory: false,
                },
            )
        };
        // CppFileTypes (case-sensitive suffixes) and getActionName's order
        // (CppCompileActionBuilder.java:2028-2056).
        assert_eq!(action("a.c"), "c-compile");
        assert_eq!(action("a.cc"), "c++-compile");
        assert_eq!(action("a.C"), "c++-compile");
        assert_eq!(action("a.c++"), "c++-compile");
        assert_eq!(action("a.cppmap"), "c++-module-map");
        assert_eq!(action("a.h"), "c++-header-parsing");
        assert_eq!(action("a.H"), "c++-header-parsing");
        assert_eq!(action("a.m"), "objc-compile");
        assert_eq!(action("a.mm"), "objc++-compile");
        assert_eq!(action("a.s"), "assemble");
        // A .pic.s source reaches Bazel's fallthrough IllegalStateException
        // (CppCompileActionBuilder.java:2054-2055); the native profile keeps
        // the c++-compile default of the routing above.
        assert_eq!(action("a.pic.s"), "c++-compile");
        assert_eq!(action("a.S"), "preprocess-assemble");
        assert_eq!(action("a.ipb"), "clif-match");
        assert_eq!(action("a.pcm"), "c++-module-codegen");
        // An explicit action name wins.
        assert_eq!(
            compile_action_name(
                Some("c++-module-deps-scanning"),
                &File {
                    path: "a.cc".into(),
                    short_path: "a.cc".into(),
                    owner: String::new(),
                    is_source: true,
                    is_directory: false,
                },
            ),
            "c++-module-deps-scanning"
        );
    }

    #[test]
    fn compile_mnemonics_follow_action_name_to_mnemonic() {
        // CppCompileAction.actionNameToMnemonic (:1993-2018).
        assert_eq!(compile_mnemonic("c-compile"), "CppCompile");
        assert_eq!(compile_mnemonic("c++-compile"), "CppCompile");
        assert_eq!(compile_mnemonic("objc-compile"), "ObjcCompile");
        assert_eq!(compile_mnemonic("cc-compile-dotd"), "CppCompile");
        assert_eq!(compile_mnemonic("linkstamp-compile"), "CppLinkstampCompile");
        assert_eq!(compile_mnemonic("c++-header-analysis"), "CppHeaderAnalysis");
        assert_eq!(
            compile_mnemonic("c++-module-deps-scanning"),
            "CppDepsScanning"
        );
    }

    #[test]
    fn artifact_names_cover_value_of_and_the_toolchain_overrides() {
        // A ToolchainData without overrides uses the category defaults
        // (ArtifactCategory.java:29-65); the query goes through
        // ArtifactCategory.valueOf (the uppercase constant names).
        let data = crate::cc_features::tests::graph(&[], &[]);
        assert_eq!(
            data.artifact_name_for_category("STATIC_LIBRARY", "built")
                .unwrap(),
            "libbuilt.a"
        );
        assert_eq!(
            data.artifact_name_for_category("PIC_OBJECT_FILE", "prebuilt")
                .unwrap(),
            "prebuilt.pic.o"
        );
        assert_eq!(
            data.artifact_name_for_category("OBJECT_FILE", "dir/x")
                .unwrap(),
            "dir/x.o"
        );
        assert_eq!(
            data.artifact_name_extension_for_category("STATIC_LIBRARY")
                .unwrap(),
            ".a"
        );
        assert!(
            data.artifact_name_for_category("NOT_A_CATEGORY", "x")
                .unwrap_err()
                .contains("No enum constant")
        );
    }
}
