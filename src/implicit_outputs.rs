//! `rule(outputs = ...)` implicit outputs (docs/IMPLICIT_OUTPUTS.md): Bazel
//! 9.3's ImplicitOutputsFunction template substitution
//! (packages/ImplicitOutputsFunction.java), shared by the dict form
//! (StarlarkImplicitOutputsFunctionWithMap, :133-161) and the function form
//! (StarlarkImplicitOutputsFunctionWithCallback, :77-130). The attribute
//! lookup (`attributeValues`, :356-399) is supplied by the caller
//! (crate::custom_rule), which knows the rule's schema and values.

use anyhow::{anyhow, bail};

/// One piece of a parsed template: literal text or a `%{name}` placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Literal(String),
    Placeholder(String),
}

/// `createPlaceholderSubstitutionFormatString` (ImplicitOutputsFunction.java:416-438):
/// `%{` up to the next `}` is a placeholder; an unterminated `%{` and all
/// text after it is literal.
pub(crate) fn parse(template: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        let Some(length) = rest[start + 2..].find('}') else {
            break;
        };
        if start > 0 {
            segments.push(Segment::Literal(rest[..start].to_owned()));
        }
        segments.push(Segment::Placeholder(
            rest[start + 2..start + 2 + length].to_owned(),
        ));
        rest = &rest[start + 2 + length + 1..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest.to_owned()));
    }
    segments
}

/// The placeholder names of a template, in order (duplicates kept).
pub(crate) fn placeholders(template: &str) -> Vec<String> {
    parse(template)
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Placeholder(name) => Some(name),
            Segment::Literal(_) => None,
        })
        .collect()
}

/// `attributeValues` result for one placeholder: `Ok(values)` (an
/// ImmutableSet in insertion order; empty for an unknown attribute or an
/// empty list) or an error (unsupported attribute type, or a value genuine
/// Bazel 9.3 crashes on).
pub(crate) type Lookup<'a> = dyn Fn(&str) -> anyhow::Result<Vec<String>> + 'a;

/// `ParsedTemplate.substituteAttributes` (ImplicitOutputsFunction.java:487-506):
/// every combination of the placeholders' values (Sets.cartesianProduct,
/// the last placeholder varying fastest), de-duplicated in order (the
/// callers collect into an ImmutableSet, :238-247, :268-279). Empty if any
/// placeholder has no values.
pub(crate) fn substitute(template: &str, lookup: &Lookup<'_>) -> anyhow::Result<Vec<String>> {
    let segments = parse(template);
    let mut values = Vec::new();
    for segment in &segments {
        if let Segment::Placeholder(name) = segment {
            let mut found = Vec::new();
            for value in lookup(name)? {
                if !found.contains(&value) {
                    found.push(value);
                }
            }
            if found.is_empty() {
                return Ok(Vec::new());
            }
            values.push(found);
        }
    }
    let mut results: Vec<String> = Vec::new();
    let mut indices = vec![0usize; values.len()];
    loop {
        let mut output = String::new();
        let mut next = 0;
        for segment in &segments {
            match segment {
                Segment::Literal(text) => output.push_str(text),
                Segment::Placeholder(_) => {
                    output.push_str(&values[next][indices[next]]);
                    next += 1;
                }
            }
        }
        if !results.contains(&output) {
            results.push(output);
        }
        // Advance the odometer, last position fastest.
        let mut position = values.len();
        loop {
            if position == 0 {
                return Ok(results);
            }
            position -= 1;
            indices[position] += 1;
            if indices[position] < values[position].len() {
                break;
            }
            indices[position] = 0;
        }
    }
}

/// One implicit output template of the dict form, or one value returned by
/// the function form, expanded to the single output it must name.
///
/// `configurable` is `Some` for the dict form, whose unsafe templates reject
/// configurable placeholders first (substitutePlaceholderIntoUnsafeTemplate,
/// ImplicitOutputsFunction.java:509-525); the function form's templates are
/// safe (fromTemplates, :114-123) and skip that check.
pub(crate) fn expand_one(
    key: &str,
    template: &str,
    configurable: Option<&dyn Fn(&str) -> bool>,
    lookup: &Lookup<'_>,
) -> anyhow::Result<String> {
    if let Some(configurable) = configurable {
        for placeholder in placeholders(template) {
            if configurable(&placeholder) {
                bail!("Attribute {placeholder} is configurable and cannot be used in outputs");
            }
        }
    }
    let mut outputs = substitute(template, lookup)?;
    match outputs.len() {
        0 => bail!("For attribute '{key}' in outputs: Invalid placeholder(s) in template"),
        1 => Ok(outputs.remove(0)),
        _ if configurable.is_none() => {
            // Iterables.getOnlyElement's IllegalArgumentException, caught and
            // rethrown as an EvalException by the callback form (:126-128).
            bail!("expected one element but was: <{}>", outputs.join(", "))
        }
        _ => Err(anyhow!(
            "implicit output '{key}' (template '{template}') expands to more than one file ({}): genuine Bazel 9.3 crashes on this (IllegalArgumentException: expected one element but was: <{}>, ImplicitOutputsFunction.java:157), which the native profile reports instead",
            outputs.join(", "),
            outputs.join(", ")
        )),
    }
}

/// `FileSystemUtils.removeExtension` (vfs/FileSystemUtils.java:128-136):
/// the text before the last `.`, unless a `/` follows it.
pub(crate) fn remove_extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if name.rfind('/').is_none_or(|slash| slash < dot) => &name[..dot],
        _ => name,
    }
}

/// `%{dirname}` and `%{basename}` of a rule name (ImplicitOutputsFunction.java:358-363):
/// the parent directory with a trailing `/` (or empty), and the last segment.
pub(crate) fn name_part(rule_name: &str, placeholder: &str) -> Option<String> {
    match placeholder {
        "dirname" => Some(match rule_name.rfind('/') {
            Some(slash) => format!("{}/", &rule_name[..slash]),
            None => String::new(),
        }),
        "basename" => Some(match rule_name.rfind('/') {
            Some(slash) => rule_name[slash + 1..].to_owned(),
            None => rule_name.to_owned(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> anyhow::Result<Vec<String>> {
        Ok(match name {
            "name" => vec!["x".into()],
            "srcs" => vec!["a".into(), "b".into(), "a".into()],
            "pair" => vec!["1".into(), "2".into()],
            "empty" => vec![],
            "n" => bail!(
                "For attribute 'n' in outputs: Attributes of type int cannot be used in an outputs substitution template"
            ),
            _ => vec![],
        })
    }

    #[test]
    fn parse_matches_bazel_format_string_semantics() {
        assert_eq!(
            parse("a-%{x}-%{y"),
            vec![
                Segment::Literal("a-".into()),
                Segment::Placeholder("x".into()),
                Segment::Literal("-%{y".into())
            ]
        );
        assert_eq!(parse("%{}"), vec![Segment::Placeholder(String::new())]);
        assert_eq!(parse("100%"), vec![Segment::Literal("100%".into())]);
        assert_eq!(
            placeholders("%{name}_%{srcs}.%{name}"),
            ["name", "srcs", "name"]
        );
    }

    #[test]
    fn substitution_is_a_deduplicated_cartesian_product() {
        assert_eq!(substitute("%{name}.a", &lookup).unwrap(), ["x.a"]);
        assert_eq!(
            substitute("%{srcs}-%{pair}", &lookup).unwrap(),
            ["a-1", "a-2", "b-1", "b-2"]
        );
        assert_eq!(substitute("%{name}%{name}", &lookup).unwrap(), ["xx"]);
        assert!(substitute("%{name}.%{empty}", &lookup).unwrap().is_empty());
        assert_eq!(substitute("plain", &lookup).unwrap(), ["plain"]);
        assert!(substitute("%{n}", &lookup).is_err());
    }

    #[test]
    fn expansion_errors_use_bazel_texts() {
        let never = |_: &str| false;
        let always = |name: &str| name == "name";
        assert_eq!(
            expand_one("k", "%{missing}", Some(&never), &lookup)
                .unwrap_err()
                .to_string(),
            "For attribute 'k' in outputs: Invalid placeholder(s) in template"
        );
        assert_eq!(
            expand_one("k", "%{name}", Some(&always), &lookup)
                .unwrap_err()
                .to_string(),
            "Attribute name is configurable and cannot be used in outputs"
        );
        assert_eq!(
            expand_one("k", "%{srcs}", None, &lookup)
                .unwrap_err()
                .to_string(),
            "expected one element but was: <a, b>"
        );
        assert!(
            expand_one("k", "%{srcs}", Some(&never), &lookup)
                .unwrap_err()
                .to_string()
                .contains("genuine Bazel 9.3 crashes")
        );
        assert_eq!(
            expand_one("k", "%{name}.k", Some(&never), &lookup).unwrap(),
            "x.k"
        );
    }

    #[test]
    fn name_parts_and_extensions() {
        assert_eq!(remove_extension("dir/in.tar.gz"), "dir/in.tar");
        assert_eq!(remove_extension("a.b/c"), "a.b/c");
        assert_eq!(remove_extension("plain"), "plain");
        assert_eq!(name_part("sub/x", "dirname").unwrap(), "sub/");
        assert_eq!(name_part("x", "dirname").unwrap(), "");
        assert_eq!(name_part("sub/x", "basename").unwrap(), "x");
        assert_eq!(name_part("x", "other"), None);
    }
}
