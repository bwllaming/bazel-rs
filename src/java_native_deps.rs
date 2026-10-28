//! JavaStarlarkCommon.collectNativeLibsDirs (Bazel 9.3:295-332).
//! Select the interface Artifact first, otherwise the dynamic Artifact; skip
//! interface-file types and project shared libraries to runfiles directories.
use std::collections::HashSet;

use starlark::eval::{Arguments, Evaluator};
use starlark::values::{Value, ValueLike};

use crate::language_modules::{bind, failure, param, type_error};
use crate::starlark_types::{Depset, File, Item, ItemType};

fn field_file<'a>(library: &'a crate::depset_items::StructItem, name: &str) -> Option<&'a File> {
    let index = library
        .fields
        .binary_search_by(|(field, _)| field.as_str().cmp(name))
        .ok()?;
    match &library.fields[index].1 {
        Item::File(file) => Some(file),
        _ => None,
    }
}

/// FileType.hasExtension depends on the Bazel host, not the configured target
/// or execution platform. The current native profile runs on Unix.
fn suffix(name: &str, extension: &str) -> bool {
    if cfg!(target_os = "windows") {
        name.get(name.len().saturating_sub(extension.len())..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(extension))
    } else {
        name.ends_with(extension)
    }
}

fn interface(name: &str) -> bool {
    [".ifso", ".tbd", ".lib", ".dll.a"]
        .iter()
        .any(|ext| suffix(name, ext))
}

/// CppFileTypes.VERSIONED_SHARED_LIBRARY_PATTERN:
/// `^.+\.((so)|(dylib))(\.\d\w*)+$`, with Java's ASCII \d/\w defaults.
fn shared(name: &str) -> bool {
    if [".so", ".dylib", ".dll", ".pyd", ".wasm", ".tgt", ".vpi"]
        .iter()
        .any(|ext| suffix(name, ext))
    {
        return true;
    }
    for separator in [".so.", ".dylib."] {
        let Some((prefix, version)) = name.rsplit_once(separator) else {
            continue;
        };
        // Artifact paths are Bazel internal bytes (Latin-1 Java chars). Java's
        // dot excludes CR/LF/NEL; U+2028/U+2029 cannot be single internal chars.
        if prefix.is_empty()
            || crate::bazel_bytes::internal_bytes(prefix)
                .iter()
                .any(|b| matches!(b, b'\n' | b'\r' | 0x85))
        {
            continue;
        }
        if version.split('.').all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            return true;
        }
    }
    false
}

pub(crate) fn collect<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    const FUNCTION: &str = "collect_native_deps_dirs";
    let bound = bind(
        FUNCTION,
        &[param("libraries", true, false, true)],
        args,
        eval.heap(),
    )?;
    let value = bound[0].expect("required");
    let libraries = value
        .downcast_ref::<Depset>()
        .ok_or_else(|| type_error(FUNCTION, "libraries", value, "depset"))?;
    crate::subrule_fragments::private_api(eval)?;
    if libraries.node.empty {
        return Ok(crate::subrule_fragments::empty_list());
    }
    if !libraries.node.empty
        && let Some(kind) = libraries.node.element_type
        && kind != ItemType::Struct
    {
        return Err(failure(format!(
            "got a depset of '{}', expected a depset of 'struct'",
            kind.name()
        )));
    }
    let mut seen = HashSet::new();
    let mut directories = Vec::new();
    for item in libraries.flatten() {
        let Item::Struct(library) = item else {
            return Err(failure(format!(
                "got a depset of '{}', expected a depset of 'struct'",
                item.kind().name()
            )));
        };
        // Genuine asks for StarlarkInfo, not a particular provider identity.
        // Missing/wrongly typed fields are ignored by instanceof Artifact.
        let Some(file) = field_file(library, "interface_library")
            .or_else(|| field_file(library, "dynamic_library"))
        else {
            continue;
        };
        let name = file.path.rsplit('/').next().expect("File path basename");
        if interface(name) {
            continue;
        }
        if !shared(name) {
            // Genuine throws unchecked IllegalArgumentException here (exit37).
            // Native fails analysis explicitly rather than crashing the process.
            return Err(failure(format!(
                "not a shared library :{}",
                file.root_relative_path()
            )));
        }
        // File.short_path is Artifact.getRunfilesPathString, NOT exec path.
        let directory = file
            .short_path
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        if seen.insert(directory) {
            directories.push(directory.to_owned());
        }
    }
    crate::subrule_fragments::frozen_strings(eval.heap(), &directories)
        .map_err(starlark::Error::new_other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_modules::tests::{fails, pass};

    #[test]
    fn cpp_library_types_are_not_the_python_extension_subset() {
        for name in [
            "a.so",
            "a.dylib",
            "a.dll",
            "a.pyd",
            "a.wasm",
            "a.tgt",
            "a.vpi",
            "a.so.2.10",
            "a.dylib.1a_b35",
            "a\u{2028}b.so.1",
            "a\u{2029}b.so.1",
            ".so",
        ] {
            assert!(shared(name), "{name}");
        }
        for name in [
            "a.a",
            "a.ifso",
            ".so.1",
            "a.so.",
            "a.so.1..2",
            "a.so.x",
            "a.so.1-a",
            "a.so.١",
            "a\nb.so.1",
            "a\rb.dylib.1",
            "a\u{85}b.so.1",
            "a.so.1\n",
        ] {
            assert!(!shared(name), "{name:?}");
        }
        for name in ["a.ifso", "a.tbd", "a.lib", "a.dll.a"] {
            assert!(interface(name));
        }
    }

    #[test]
    fn pure_loading_calls_accept_empty_or_missing_fields_and_return_frozen_lists() {
        pass(
            "@@rules_java+//java:native_deps.bzl",
            r#"
f = java_common.internal_DO_NOT_USE().collect_native_deps_dirs
(type(f(depset())) == 'list' and f(depset()) == []) or fail('empty')
f(depset([struct(), struct(dynamic_library='not a File', resolved_symlink_dynamic_library='ignored')])) == [] or fail('ignored fields')
"#,
        );
        fails(
            "@@rules_java+//java:native_deps.bzl",
            "java_common.internal_DO_NOT_USE().collect_native_deps_dirs(depset()).append('x')",
            "trying to mutate a frozen list value",
        );
        fails(
            "@@rules_java+//java:native_deps.bzl",
            "java_common.internal_DO_NOT_USE().collect_native_deps_dirs(depset(['x']))",
            "got a depset of 'string', expected a depset of 'struct'",
        );
        fails(
            "@@rules_java+//other:native_deps.bzl",
            "java_common.internal_DO_NOT_USE().collect_native_deps_dirs(depset())",
            "cannot use private API",
        );
    }
}
