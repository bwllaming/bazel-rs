//! Analysis-only, eagerly expanded Args builders. Unlike Bazel, transformations
//! run at mutation time, not execution time; only owned strings are retained.
//! Builders cannot be frozen, iterated, or converted to lists from Starlark.
//! Parameter files and two-argument DirectoryExpander callbacks are unsupported.
// Match Bazel's keyword-rich Args API, including generated call trampolines.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::list::ListRef;
use starlark::values::none::{NoneOr, NoneType};
use starlark::values::starlark_value;
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLike,
};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// Only upstream derives generate unsafe marker implementations. No heap values
// are retained: both fields are ordinary owned Rust data.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct Args {
        pub(super) values: RefCell<Vec<String>>,
        #[allocative(skip)]
        #[trace(static)]
        pub(super) active: Arc<AtomicBool>,
    }
}
use owned::Args;

impl Args {
    fn from_value(value: Value<'_>) -> anyhow::Result<&Self> {
        value
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("expected Args"))
    }

    fn check_active(&self) -> anyhow::Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "Args is no longer active: rule analysis has ended"
        );
        Ok(())
    }

    fn append(&self, values: Vec<String>) -> anyhow::Result<()> {
        self.check_active()?;
        self.values.borrow_mut().extend(values);
        Ok(())
    }
}

impl fmt::Display for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<Args>")
    }
}

starlark::methods_static!(ARGS_METHODS = args_methods);

#[starlark_value(type = "Args")]
impl<'v> StarlarkValue<'v> for Args {
    fn get_methods() -> Option<&'static Methods> {
        Some(ARGS_METHODS.methods())
    }
}

/// Allocate a mutable builder tied to one rule analysis. The owner must set
/// `active` to false when analysis ends, including error/unwind paths.
pub fn alloc_args<'v>(heap: Heap<'v>, active: Arc<AtomicBool>) -> Value<'v> {
    heap.alloc_complex_no_freeze(Args {
        values: RefCell::new(Vec::new()),
        active,
    })
}

/// Serialize only genuine, active Args for actions.write's default SHELL_QUOTED
/// format: shell-escape each argument and append LF. Borrow the already-expanded
/// strings directly; do not allocate an intermediate list or clone their payloads.
/// Native map_each remains eager at builder mutation, unlike Bazel's deferred
/// command-line expansion. This adds neither deferred callbacks nor param files;
/// use_param_file/set_param_file_format deliberately remain unsupported.
pub(crate) fn serialize_write_content(value: Value<'_>) -> anyhow::Result<Vec<u8>> {
    let args = value
        .downcast_ref::<Args>()
        .ok_or_else(|| anyhow!("write content must be a string or Args"))?;
    args.check_active()?;
    let values = args.values.borrow();
    // Compute the exact output allocation with checked arithmetic, without an
    // invented content-size cap. Each embedded apostrophe expands from 1 to 4
    // bytes; quotes contribute 2 and every argument contributes a trailing LF.
    let mut size = 0usize;
    for text in values.iter() {
        size = size
            .checked_add(text.len())
            .and_then(|size| size.checked_add(1))
            .ok_or_else(|| anyhow!("Args write content size overflow"))?;
        if shell_quote_required(text) {
            size = size
                .checked_add(2)
                .ok_or_else(|| anyhow!("Args write content size overflow"))?;
            for byte in text.bytes() {
                if byte == b'\'' {
                    size = size
                        .checked_add(3)
                        .ok_or_else(|| anyhow!("Args write content size overflow"))?;
                }
            }
        }
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|error| anyhow!("cannot allocate Args write content: {error}"))?;
    for text in values.iter() {
        if shell_quote_required(text) {
            output.push(b'\'');
            for byte in text.bytes() {
                if byte == b'\'' {
                    output.extend_from_slice(b"'\\''");
                } else {
                    output.push(byte);
                }
            }
            output.push(b'\'');
        } else {
            output.extend_from_slice(text.as_bytes());
        }
        output.push(b'\n');
    }
    debug_assert_eq!(output.len(), size);
    Ok(output)
}

fn shell_quote_required(text: &str) -> bool {
    // Pinned Bazel ShellEscaper.java:57-110. Only ASCII letters/digits and
    // @%-_+:,./ are safe; ~ is additionally safe except at the start. Preserve
    // UTF-8, backslashes and embedded newlines literally within strong quotes.
    text.is_empty()
        || text.starts_with('~')
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%-_+:,./~".contains(&byte))
}

fn sequence(value: Value<'_>) -> anyhow::Result<Vec<Value<'_>>> {
    if let Some(list) = ListRef::from_value(value) {
        Ok(list.iter().collect())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Ok(tuple.iter().collect())
    } else {
        bail!("expected list or tuple, got {}", value.get_type())
    }
}

/// Snapshot a list/tuple of strings and active Args objects for action registration.
/// Bare Args, nested lists, and other element types are rejected.
pub fn expand_arguments(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::new();
    for item in sequence(value)? {
        if let Some(text) = item.unpack_str() {
            result.push(text.to_owned());
        } else if let Some(args) = item.downcast_ref::<Args>() {
            args.check_active()?;
            result.extend(args.values.borrow().iter().cloned());
        } else {
            bail!(
                "arguments must contain only strings or Args, got {}",
                item.get_type()
            );
        }
    }
    Ok(result)
}

fn string_value(value: Value<'_>) -> anyhow::Result<String> {
    if let Some(text) = value.unpack_str() {
        Ok(text.to_owned())
    } else if let Ok(file) = crate::starlark_types::unpack_file(value) {
        Ok(file.path)
    } else if let Some(label) = crate::starlark_types::unpack_label(value) {
        Ok(label)
    } else if matches!(value.get_type(), "int" | "bool") {
        Ok(value.to_str())
    } else {
        bail!(
            "Args value must be a string, File, Label, int, or bool; got {}",
            value.get_type()
        )
    }
}

fn named_value<'v>(args: &[Value<'v>]) -> anyhow::Result<(Option<String>, Value<'v>)> {
    match args {
        [value] => Ok((None, *value)),
        [name, value] => Ok((
            Some(
                name.unpack_str()
                    .ok_or_else(|| anyhow!("arg name must be a string"))?
                    .to_owned(),
            ),
            *value,
        )),
        _ => bail!("Args method requires one value, or an arg name followed by a value"),
    }
}

// Exactly one %s, with %% representing a literal percent. Do not recursively
// interpret inserted values: argument strings remain literal, including NULs.
fn formatted(template: Option<&str>, value: &str) -> anyhow::Result<String> {
    let Some(template) = template else {
        return Ok(value.to_owned());
    };
    let mut result = String::new();
    let mut placeholders = 0;
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            result.push(ch);
        } else {
            match chars.next() {
                Some('%') => result.push('%'),
                Some('s') => {
                    placeholders += 1;
                    result.push_str(value);
                }
                _ => bail!(
                    "Args format requires exactly one %s placeholder; only %s and %% are supported"
                ),
            }
        }
    }
    ensure!(
        placeholders == 1,
        "Args format requires exactly one %s placeholder"
    );
    Ok(result)
}

fn check_mapper(
    map_each: Value<'_>,
    allow_closure: bool,
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<()> {
    if map_each.is_none() {
        return Ok(());
    }
    let name = map_each
        .function_name()
        .ok_or_else(|| anyhow!("map_each must be a function or None"))?;
    if !allow_closure {
        // Analysis runs in a fresh module. The loader retains identities of
        // actual top-level def declarations, including private/imported helpers.
        ensure!(
            !name.contains("lambda")
                && (crate::analysis_runtime::is_top_level_mapper(eval, map_each)
                    || eval
                        .module()
                        .get(name)
                        .is_some_and(|global| global.ptr_eq(map_each))),
            "map_each must be a top-level function visible in this analysis module; closures and unverified imported functions require allow_closure=True"
        );
    }
    Ok(())
}

fn mapped_values<'v>(
    values: Value<'v>,
    map_each: Value<'v>,
    format_each: Option<&str>,
    uniquify: bool,
    allow_closure: bool,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Vec<String>> {
    formatted(format_each, "")?;
    check_mapper(map_each, allow_closure, eval)?;
    let values = if values.get_type() == "depset" {
        let to_list = values
            .get_attr_error("to_list", eval.heap())
            .map_err(|e| anyhow!(e.to_string()))?;
        let list = eval
            .eval_function(to_list, &[], &[])
            .map_err(|e| anyhow!(e.to_string()))?;
        sequence(list)?
    } else {
        sequence(values)?
    };
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for item in values {
        let strings = if map_each.is_none() {
            vec![string_value(item)?]
        } else {
            let mapped = eval
                .eval_function(map_each, &[item], &[])
                .map_err(|e| anyhow!("map_each failed: {e}"))?;
            if mapped.is_none() {
                Vec::new()
            } else if let Some(text) = mapped.unpack_str() {
                vec![text.to_owned()]
            } else if let Some(list) = ListRef::from_value(mapped) {
                list.iter()
                    .map(|value| {
                        value.unpack_str().map(str::to_owned).ok_or_else(|| {
                            anyhow!("map_each must return None, a string, or a list of strings")
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                bail!("map_each must return None, a string, or a list of strings");
            }
        };
        for text in strings {
            let text = formatted(format_each, &text)?;
            if !uniquify || seen.insert(text.clone()) {
                result.push(text);
            }
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn args_methods(builder: &mut MethodsBuilder) {
    fn add<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(default = NoneOr::None)] format: NoneOr<&str>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let (name, value) = named_value(&args.items)?;
        let mut values: Vec<_> = name.into_iter().collect();
        values.push(formatted(format.into_option(), &string_value(value)?)?);
        builder.append(values)?;
        Ok(this)
    }

    fn add_all<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(default = NoneType)] map_each: Value<'v>,
        #[starlark(default = NoneOr::None)] format_each: NoneOr<&str>,
        #[starlark(default = NoneOr::None)] before_each: NoneOr<&str>,
        #[starlark(default = true)] omit_if_empty: bool,
        #[starlark(default = false)] uniquify: bool,
        #[starlark(default = true)] expand_directories: bool,
        #[starlark(default = NoneOr::None)] terminate_with: NoneOr<&str>,
        #[starlark(default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let _ = expand_directories; // All currently supported File values are regular files.
        let (name, values) = named_value(&args.items)?;
        let values = mapped_values(
            values,
            map_each,
            format_each.into_option(),
            uniquify,
            allow_closure,
            eval,
        )?;
        if omit_if_empty && values.is_empty() {
            return Ok(this);
        }
        let mut result: Vec<_> = name.into_iter().collect();
        for value in values {
            if let NoneOr::Other(before) = before_each {
                result.push(before.to_owned());
            }
            result.push(value);
        }
        if let NoneOr::Other(end) = terminate_with {
            result.push(end.to_owned());
        }
        builder.append(result)?;
        Ok(this)
    }

    fn add_joined<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        join_with: &str,
        #[starlark(default = NoneType)] map_each: Value<'v>,
        #[starlark(default = NoneOr::None)] format_each: NoneOr<&str>,
        #[starlark(default = NoneOr::None)] format_joined: NoneOr<&str>,
        #[starlark(default = true)] omit_if_empty: bool,
        #[starlark(default = false)] uniquify: bool,
        #[starlark(default = true)] expand_directories: bool,
        #[starlark(default = false)] allow_closure: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let builder = Args::from_value(this)?;
        builder.check_active()?;
        let _ = expand_directories;
        let format_joined = format_joined.into_option();
        formatted(format_joined, "")?;
        let (name, values) = named_value(&args.items)?;
        let values = mapped_values(
            values,
            map_each,
            format_each.into_option(),
            uniquify,
            allow_closure,
            eval,
        )?;
        if omit_if_empty && values.is_empty() {
            return Ok(this);
        }
        let mut result: Vec<_> = name.into_iter().collect();
        result.push(formatted(format_joined, &values.join(join_with))?);
        builder.append(result)?;
        Ok(this)
    }

    fn use_param_file<'v>(
        this: Value<'v>,
        param_file_arg: &str,
        #[starlark(require = named, default = false)] use_always: bool,
    ) -> anyhow::Result<Value<'v>> {
        Args::from_value(this)?.check_active()?;
        let _ = (param_file_arg, use_always);
        bail!("Args parameter files are unsupported; use_param_file cannot be ignored")
    }

    fn set_param_file_format<'v>(this: Value<'v>, format: &str) -> anyhow::Result<Value<'v>> {
        Args::from_value(this)?.check_active()?;
        let _ = format;
        bail!("Args parameter files are unsupported; set_param_file_format cannot be ignored")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{GlobalsBuilder, Module};
    use starlark::syntax::{AstModule, Dialect};

    #[starlark_module]
    fn test_globals(builder: &mut GlobalsBuilder) {
        fn expanded<'v>(arguments: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
            Ok(heap.alloc(expand_arguments(arguments)?))
        }

        fn serialized(arguments: Value<'_>) -> anyhow::Result<String> {
            Ok(String::from_utf8(serialize_write_content(arguments)?)?)
        }
    }

    // An intentionally misleading runtime type name must not grant Args access.
    #[allow(unsafe_code)]
    mod spoof {
        use super::*;
        #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
        pub(super) struct SpoofArgs;
        impl fmt::Display for SpoofArgs {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("<Args>")
            }
        }
        starlark::starlark_simple_value!(SpoofArgs);
        #[starlark_value(type = "Args")]
        impl<'v> StarlarkValue<'v> for SpoofArgs {}
    }

    fn eval(source: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            module.set(
                "a",
                alloc_args(module.heap(), Arc::new(AtomicBool::new(true))),
            );
            module.set(
                "file",
                crate::starlark_types::alloc_file(
                    module.heap(),
                    crate::starlark_types::File {
                        path: "pkg/file with $ spaces".into(),
                        short_path: "pkg/file with $ spaces".into(),
                        owner: "//pkg:rule".into(),
                        is_source: true,
                    },
                ),
            );
            let globals = GlobalsBuilder::standard()
                .with(crate::starlark_types::analysis_globals)
                .with(test_globals)
                .build();
            let ast = AstModule::parse("args_test.star", source.to_owned(), &Dialect::Standard)
                .map_err(|e| e.to_string())?;
            Evaluator::new(&module)
                .eval_module(ast, &globals)
                .map(|value| value.to_repr())
                .map_err(|e| e.to_string())
        })
    }

    fn pass(source: &str) {
        assert_eq!(eval(source).unwrap(), "True", "{source}");
    }

    fn fail(source: &str, message: &str) {
        let error = eval(source).unwrap_err();
        assert!(error.contains(message), "{source}: {error}");
    }

    #[test]
    fn default_write_bytes_match_pinned_shell_escaper_and_reference_fixture() {
        Module::with_temp_heap(|module| {
            let active = Arc::new(AtomicBool::new(true));
            let value = alloc_args(module.heap(), active.clone());
            let builder = Args::from_value(value).unwrap();
            assert!(serialize_write_content(value).unwrap().is_empty());
            for (text, expected) in [
                ("plain", "plain\n"),
                ("aAzZ09@%-_+:,./", "aAzZ09@%-_+:,./\n"),
                ("a~b", "a~b\n"),
                ("./~", "./~\n"),
                ("~", "'~'\n"),
                ("~user/file", "'~user/file'\n"),
                ("", "''\n"),
                ("space value", "'space value'\n"),
                ("single'quote", "'single'\\''quote'\n"),
                ("'", "''\\'''\n"),
                ("''", "''\\'''\\'''\n"),
                ("double\"quote", "'double\"quote'\n"),
                ("back\\slash", "'back\\slash'\n"),
                ("line1\nline2", "'line1\nline2'\n"),
                ("\t\r", "'\t\r'\n"),
                ("雪é🙂", "'雪é🙂'\n"),
                ("x=y", "'x=y'\n"),
                ("!#$&*?[]();<>|`", "'!#$&*?[]();<>|`'\n"),
                ("\0", "'\0'\n"),
            ] {
                builder.values.borrow_mut().clear();
                builder.append(vec![text.to_owned()]).unwrap();
                assert_eq!(
                    serialize_write_content(value).unwrap(),
                    expected.as_bytes(),
                    "{text:?}"
                );
            }
            builder.values.borrow_mut().clear();
            builder
                .append(
                    [
                        "plain",
                        "space value",
                        "single'quote",
                        "double\"quote",
                        "back\\slash",
                        "",
                        "line1\nline2",
                        "雪é🙂",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                )
                .unwrap();
            let bytes = serialize_write_content(value).unwrap();
            // Actual mixed.out/mixed.exec bytes from actions-write-args-contract.
            assert_eq!(bytes, "plain\n'space value'\n'single'\\''quote'\n'double\"quote'\n'back\\slash'\n''\n'line1\nline2'\n'雪é🙂'\n".as_bytes());
            active.store(false, Ordering::Release);
            assert!(
                serialize_write_content(value)
                    .unwrap_err()
                    .to_string()
                    .contains("analysis has ended")
            );
            let spoof = module.heap().alloc(spoof::SpoofArgs);
            assert_eq!(spoof.get_type(), "Args");
            assert!(
                serialize_write_content(spoof)
                    .unwrap_err()
                    .to_string()
                    .contains("string or Args")
            );
        });
    }

    #[test]
    fn write_serializes_eager_formatting_and_mapping_without_new_param_file_support() {
        pass(
            r#"
a.add('v w', format='pre:%s:%%')
a.add_all(['a', 'b c', ''], format_each='<%s>', before_each='--value', terminate_with='--end')
serialized(a) == "'pre:v w:%'\n--value\n'<a>'\n--value\n'<b c>'\n--value\n'<>'\n--end\n"
"#,
        );
        pass(
            r#"
def mapper(value):
    if value == 'skip':
        return None
    if value == 'two':
        return ['x y', "q'z"]
    return value
a.add_all(['skip', 'two', 'z'], map_each=mapper, format_each='[%s]')
serialized(a) == "'[x y]'\n'[q'\\''z]'\n'[z]'\n"
"#,
        );
        for source in [
            "serialized([])",
            "serialized(())",
            "serialized({})",
            "serialized('not Args')",
            "serialized(None)",
            "serialized(depset([]))",
        ] {
            fail(source, "string or Args");
        }
        // The existing native eager boundary is explicit: mapper failure occurs
        // at add_all, not at genuine Bazel's later write-action execution phase.
        fail(
            "def bad(x):\n    fail('EAGER_NATIVE_MAPPER')\na.add_all(['x'], map_each=bad)\nserialized(a)",
            "EAGER_NATIVE_MAPPER",
        );
        for format in ["shell", "multiline", "flag_per_line"] {
            fail(
                &format!("a.set_param_file_format({format:?})"),
                "parameter files are unsupported",
            );
        }
        fail("a.use_param_file('@%s')", "parameter files are unsupported");
    }

    #[test]
    fn add_converts_supported_scalars_formats_and_chains() {
        pass(
            r#"
a.add("--flag").add("--file", file, format="pre:%s:%%:post").add(Label("//pkg:rule")).add(42).add(True).add("雪 $HOME", format=None)
expanded(["before", a, "after"]) == ["before", "--flag", "--file", "pre:pkg/file with $ spaces:%:post", "//pkg:rule", "42", "True", "雪 $HOME", "after"]
"#,
        );
    }

    #[test]
    fn add_all_and_joined_transform_in_order() {
        pass(
            r#"
a.add_all("--files", [file, file, ""], format_each="[%s]", uniquify=True, before_each="-I", terminate_with="--end")
a.add_joined("--joined", ("b", "a", "b"), join_with=",", format_each="<%s>", format_joined="%%:%s:%%", uniquify=True)
expanded((a,)) == ["--files", "-I", "[pkg/file with $ spaces]", "-I", "[]", "--end", "--joined", "%:<b>,<a>:%"]
"#,
        );
    }

    #[test]
    fn depsets_and_empty_values_follow_omit_rules() {
        pass(
            r#"
a.add_all(depset(["b", "a", "b"])).add_all(depset([file]))
a.add_all("skip", [], terminate_with="skip-end").add_joined("skip", [], join_with=",")
a.add_all("keep", [], omit_if_empty=False, terminate_with="end")
a.add_joined("empty", [], omit_if_empty=False, join_with=",", format_joined="[%s]")
a.add_all([""], expand_directories=False)
expanded([a]) == ["b", "a", "pkg/file with $ spaces", "keep", "end", "empty", "[]", ""]
"#,
        );
    }

    #[test]
    fn map_each_supports_none_string_and_lists_before_formatting_and_dedup() {
        pass(
            r#"
def mapper(value):
    if value == "skip":
        return None
    if value == "two":
        return ["x", "y", "x"]
    return value
a.add_all("--map", ["skip", "two", "z"], map_each=mapper, format_each="<%s>", before_each="+", uniquify=True)
a.add_joined(["skip", "two"], join_with=":", map_each=mapper, uniquify=True)
a.add_all("omitted", ["skip"], map_each=mapper)
expanded([a]) == ["--map", "+", "<x>", "+", "<y>", "+", "<z>", "x:y"]
"#,
        );
    }

    #[test]
    fn map_each_can_receive_file_and_explicit_closures() {
        pass(
            r#"
def mapper(value):
    return value.path
a.add_all([file], map_each=mapper)
def build():
    prefix = "prefix:"
    def closure(value):
        return prefix + value
    a.add_joined(["x", "y"], join_with=",", map_each=closure, allow_closure=True)
build()
expanded([a]) == ["pkg/file with $ spaces", "prefix:x,prefix:y"]
"#,
        );
        fail(
            "def build():\n    def nested(x):\n        return x\n    a.add_all(['x'], map_each=nested)\nbuild()",
            "allow_closure=True",
        );
    }

    #[test]
    fn invalid_values_formats_callbacks_and_param_files_fail_explicitly() {
        for value in ["[]", "()", "{}", "depset([])", "None", "1.5"] {
            fail(&format!("a.add({value})"), "Args value must be");
        }
        for format in ["plain", "%d", "%s%s", "%", "%%"] {
            fail(&format!("a.add('x', format={format:?})"), "exactly one %s");
            fail(
                &format!("a.add_all([], format_each={format:?})"),
                "exactly one %s",
            );
            fail(
                &format!("a.add_joined([], join_with=',', format_joined={format:?})"),
                "exactly one %s",
            );
        }
        fail("a.add()", "requires one value");
        fail("a.add('a', 'b', 'c')", "requires one value");
        fail("a.add(1, 'b')", "arg name must be a string");
        fail("a.add_all('abc')", "expected list or tuple");
        fail("a.add_all([], map_each=42)", "map_each must be a function");
        for returned in ["42", "['ok', 42]", "('tuple',)"] {
            fail(
                &format!(
                    "def mapper(x):\n    return {returned}\na.add_all(['x'], map_each=mapper)"
                ),
                "map_each must return",
            );
        }
        fail("a.use_param_file('@%s')", "parameter files are unsupported");
        fail(
            "a.set_param_file_format('shell')",
            "parameter files are unsupported",
        );
        fail("a.to_list()", "to_list");
        fail("list(a)", "Operation `(iter)` not supported");
        fail("expanded(a)", "expected list or tuple");
        fail("expanded([[]])", "only strings or Args");
    }

    #[test]
    fn expansion_snapshots_and_ended_analysis_rejects_methods_and_expansion() {
        pass(
            "a.add('first')\nsnapshot = expanded([a])\na.add('second')\nsnapshot == ['first'] and expanded([a]) == ['first', 'second']",
        );
        Module::with_temp_heap(|module| {
            let active = Arc::new(AtomicBool::new(true));
            let args = alloc_args(module.heap(), active.clone());
            module.set("a", args);
            let globals = GlobalsBuilder::standard().with(test_globals).build();
            active.store(false, Ordering::Release);
            for source in [
                "a.add('x')",
                "a.add_all([])",
                "a.add_joined([], join_with=',')",
                "a.use_param_file('@%s')",
                "a.set_param_file_format('shell')",
                "expanded([a])",
            ] {
                let ast =
                    AstModule::parse("ended.star", source.to_owned(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err();
                assert!(error.to_string().contains("analysis has ended"), "{error}");
            }
        });
    }

    #[test]
    fn args_cannot_escape_via_module_freezing() {
        Module::with_temp_heap(|module| {
            module.set(
                "a",
                alloc_args(module.heap(), Arc::new(AtomicBool::new(true))),
            );
            assert!(module.freeze().is_err());
        });
    }
}
