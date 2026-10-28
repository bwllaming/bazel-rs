//! JavaStarlarkCommon.getRuntimeClasspathForArchive (Bazel 9.3:336-350).
//! Pure, private API: an empty exclusion set returns the original depset;
//! otherwise flatten once, filter by Artifact identity, and wrap in stable order.
use std::collections::HashSet;

use starlark::eval::{Arguments, Evaluator};
use starlark::values::{Value, ValueLike};

use crate::language_modules::{bind, failure, param, type_error};
use crate::starlark_types::{Depset, Item, ItemType};

pub(crate) fn for_archive<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    const FUNCTION: &str = "get_runtime_classpath_for_archive";
    let bound = bind(
        FUNCTION,
        &[
            param("jars", true, false, true),
            param("excluded_jars", true, false, true),
        ],
        args,
        eval.heap(),
    )?;
    let jars_value = bound[0].expect("required");
    let excluded_value = bound[1].expect("required");
    let jars = jars_value
        .downcast_ref::<Depset>()
        .ok_or_else(|| type_error(FUNCTION, "jars", jars_value, "depset"))?;
    let excluded = excluded_value
        .downcast_ref::<Depset>()
        .ok_or_else(|| type_error(FUNCTION, "excluded_jars", excluded_value, "depset"))?;
    crate::subrule_fragments::private_api(eval)?;
    if excluded.node.empty {
        // This bypasses even the runtime element-type check in genuine Bazel.
        // Preserve identity/order/frozen values without traversing either DAG.
        return Ok(jars_value);
    }
    if !jars.node.empty
        && let Some(kind) = jars.node.element_type
        && kind != ItemType::File
    {
        return Err(failure(format!(
            "got a depset of '{}', expected a depset of 'File'",
            kind.name()
        )));
    }
    // Genuine deliberately uses excludedArtifacts.getSet(), NOT getSet(Artifact).
    // Non-File exclusions cannot equal an Artifact, but still cause a fresh
    // stable-order result. No path/name/extension based filtering is involved.
    let excluded_files: HashSet<_> = if excluded.node.element_type == Some(ItemType::File) {
        excluded
            .flatten()
            .into_iter()
            .filter_map(|item| match item {
                Item::File(file) => Some(file),
                _ => None,
            })
            .collect()
    } else {
        HashSet::new()
    };
    let mut files = Vec::new();
    for item in jars.flatten() {
        let Item::File(file) = item else {
            return Err(failure(format!(
                "got a depset of '{}', expected a depset of 'File'",
                item.kind().name()
            )));
        };
        if !excluded_files.contains(file) {
            files.push(file.clone());
        }
    }
    Ok(eval.heap().alloc(Depset::from_files(files)))
}

#[cfg(test)]
mod tests {
    use crate::language_modules::tests::{fails, pass};

    #[test]
    fn empty_exclusions_are_a_context_free_identity_operation_even_for_non_files() {
        pass(
            "@@rules_java+//java:classpath.bzl",
            r#"
f = java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive
def check():
    for order in ["default", "preorder", "postorder", "topological"]:
        jars = depset(["not a File"], order=order)
        result = f(jars, depset(transitive=[depset()]))
        (result == jars and result.to_list() == ["not a File"]) or fail("identity")
check()
"#,
        );
    }

    #[test]
    fn nonempty_exclusions_check_runtime_elements_but_do_not_require_context() {
        fails(
            "@@rules_java+//java:classpath.bzl",
            r#"
java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive(depset(["x"]), depset(["x"]))
"#,
            "got a depset of 'string', expected a depset of 'File'",
        );
        pass(
            "@@rules_java+//java:classpath.bzl",
            r#"
f = java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive
f(depset(order="preorder"), depset([1])) == depset() or fail("empty stable set")
"#,
        );
        fails(
            "@@rules_java+//other:classpath.bzl",
            r#"
java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive(depset(), depset())
"#,
            "cannot use private API",
        );
    }
}
