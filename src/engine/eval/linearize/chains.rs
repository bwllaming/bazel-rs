//! Errors whose text names the walk's active chain, built bottom-up (S2,
//! worker C; docs/EVALUATOR.md "First-visit decisions").
//!
//! The walk words a dependency cycle (`cycle_error`: the path from the
//! requested target and the cycle) and a registration cycle report
//! (`toolchain_cycle`, `execution_platform_cycle`: the chain from the
//! registered target) from its active chain where it detects them. A node
//! body does not know its chain once bodies run in parallel (nor which
//! registration's chain first analyzes a target the graph then memoizes).
//! So the evaluator creates such an error as an **open payload** naming
//! only what the detecting body knows, and every configured target's body
//! that fails with it adds itself as the error propagates up
//! ([`on_ct1_failure`]); a node that starts a fresh chain (a platform's
//! resolution, the registered toolchains or execution platforms) or the
//! requested target's segment renders it ([`close`]). A node's error text
//! then depends on its subtree only (whatever order the nodes finish in);
//! the rendered text is the walk's.
//!
//! The payloads use private-use characters; [`close`] renders every one
//! left (a payload never reaches the user).

use super::super::super::ConfiguredTargetKey;

/// A payload begins.
const START: char = '\u{E020}';
/// A payload ends.
const END: char = '\u{E02F}';
/// Separates a payload's fields.
const FIELD: char = '\u{E021}';
/// Separates a list's items.
const ITEM: char = '\u{E022}';

/// A dependency cycle: the head (the key reached while active), the
/// cycle's other members (from the head's child down to the reacher), the
/// path above the head (outermost first), and whether the cycle is closed
/// (the head's own body added the error: what is added now is the path).
#[derive(Debug, PartialEq, Eq)]
struct Cycle {
    head: String,
    closed: bool,
    members: Vec<String>,
    path: Vec<String>,
}

/// A registration cycle report: toolchains or execution platforms, the
/// chain from the registered target (outermost first), the fixed tail.
#[derive(Debug, PartialEq, Eq)]
struct Report {
    toolchains: bool,
    elements: Vec<String>,
    tail: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Payload {
    Cycle(Cycle),
    Report(Report),
}

fn list(items: &[String]) -> String {
    items.join(&ITEM.to_string())
}

fn unlist(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split(ITEM).map(str::to_owned).collect()
    }
}

impl Payload {
    fn encode(&self) -> String {
        let fields = match self {
            Payload::Cycle(cycle) => vec![
                "c".to_owned(),
                cycle.head.clone(),
                if cycle.closed { "1" } else { "0" }.to_owned(),
                list(&cycle.members),
                list(&cycle.path),
            ],
            Payload::Report(report) => vec![
                if report.toolchains { "t" } else { "e" }.to_owned(),
                list(&report.elements),
                report.tail.clone(),
            ],
        };
        format!("{START}{}{END}", fields.join(&FIELD.to_string()))
    }

    fn decode(text: &str) -> Option<Self> {
        let fields: Vec<&str> = text.split(FIELD).collect();
        match fields.as_slice() {
            ["c", head, closed, members, path] => Some(Payload::Cycle(Cycle {
                head: (*head).to_owned(),
                closed: *closed == "1",
                members: unlist(members),
                path: unlist(path),
            })),
            [kind @ ("t" | "e"), elements, tail] => Some(Payload::Report(Report {
                toolchains: *kind == "t",
                elements: unlist(elements),
                tail: (*tail).to_owned(),
            })),
            _ => None,
        }
    }
}

/// Rewrite every payload of `text` (`f` returns its replacement text).
fn rewrite(text: &str, mut f: impl FnMut(Payload) -> String) -> String {
    if !text.contains(START) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(START) {
        out.push_str(&rest[..start]);
        let after = &rest[start + START.len_utf8()..];
        let Some(end) = after.find(END) else {
            out.push_str(&rest[start..]);
            return out;
        };
        match Payload::decode(&after[..end]) {
            Some(payload) => out.push_str(&f(payload)),
            None => out.push_str(&rest[start..start + START.len_utf8() + end + END.len_utf8()]),
        }
        rest = &after[end + END.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// Whether `text` holds an open payload.
pub(crate) fn is_open(text: &str) -> bool {
    text.contains(START)
}

/// The message of a dependency cycle detected at a reach of `head` (an
/// active key; `self_edge`: the reacher is the head itself), to be wrapped
/// as `cycle_error` wraps its message (the head rule's location and
/// class). The reacher's own body adds itself when it fails.
pub(crate) fn open_cycle(head: &ConfiguredTargetKey, self_edge: bool) -> String {
    Payload::Cycle(Cycle {
        head: head.to_string(),
        closed: self_edge,
        members: Vec::new(),
        path: Vec::new(),
    })
    .encode()
}

/// A registration cycle report (`toolchain_cycle`: `toolchains`; else
/// `execution_platform_cycle`) with the walk's fixed tail (the toolchain
/// types lines and the closing line); the detecting body adds itself.
pub(crate) fn open_report(toolchains: bool, tail: String) -> String {
    Payload::Report(Report {
        toolchains,
        elements: Vec::new(),
        tail,
    })
    .encode()
}

/// The configured target `key`'s body fails with `error`: it joins the
/// chain of every open payload (the walk's active chain had it there).
pub(crate) fn on_ct1_failure(error: String, key: &ConfiguredTargetKey) -> String {
    if !is_open(&error) {
        return error;
    }
    let shown_key = key.to_string();
    let shown_label = crate::rule_declarations::canonical_form(&key.label);
    rewrite(&error, |payload| {
        match payload {
            Payload::Cycle(mut cycle) => {
                if !cycle.closed {
                    if cycle.head == shown_key {
                        cycle.closed = true;
                    } else {
                        cycle.members.insert(0, shown_key.clone());
                    }
                } else if cycle.head != shown_key || !cycle.path.is_empty() {
                    cycle.path.insert(0, shown_key.clone());
                }
                Payload::Cycle(cycle)
            }
            Payload::Report(mut report) => {
                report.elements.insert(0, shown_label.clone());
                Payload::Report(report)
            }
        }
        .encode()
    })
}

/// A requested target's (test-trimmed) top-level key: a cycle whose path
/// starts there prints its untrimmed key first (`cycle_error`).
pub(crate) struct TrimmedRoot {
    pub(crate) key: ConfiguredTargetKey,
    /// `LABEL (UNTRIMMED CONFIG)`.
    pub(crate) untrimmed: String,
}

/// Render every payload of `error` (a fresh chain starts here, or the
/// requested target's segment ends): the walk's texts; the registration
/// reports rendered, in order (`take_*_cycle_report`).
pub(crate) fn close(error: &str, root: Option<&TrimmedRoot>) -> (String, Vec<(bool, String)>) {
    let mut reports = Vec::new();
    let text = rewrite(error, |payload| match payload {
        Payload::Cycle(cycle) => {
            let mut message = String::from("cycle in dependency graph:");
            if let Some(root) = root
                && cycle.path.first().unwrap_or(&cycle.head) == &root.key.to_string()
            {
                message.push_str("\n    ");
                message.push_str(&root.untrimmed);
            }
            for element in &cycle.path {
                message.push_str("\n    ");
                message.push_str(element);
            }
            message.push_str("\n.-> ");
            message.push_str(&cycle.head);
            if cycle.members.is_empty() {
                message.push_str(" [self-edge]\n`--");
            } else {
                for element in &cycle.members {
                    message.push_str("\n|   ");
                    message.push_str(element);
                }
                message.push_str("\n`-- ");
                message.push_str(&cycle.head);
            }
            message
        }
        Payload::Report(report) => {
            let head = report.elements.first().cloned().unwrap_or_default();
            let mut text = if report.toolchains {
                format!(
                    "Misconfigured toolchains: {head} is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains"
                )
            } else {
                format!(
                    "Misconfigured execution platforms: {head} is declared as a platform but has inappropriate dependencies. Execution platforms should not have dependencies that themselves require toolchain resolution.\n.-> RegisteredExecutionPlatforms"
                )
            };
            for element in &report.elements {
                text.push_str("\n|   ");
                text.push_str(element);
            }
            text.push_str(&report.tail);
            reports.push((report.toolchains, text.clone()));
            text
        }
    });
    (text, reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::eval::linearize::pass1::tests::ctk;

    /// root → a → b → c → a: the walk's text, built from c up.
    #[test]
    fn a_cycle_is_built_bottom_up() {
        let (root, a, b, c) = (ctk("root"), ctk("a"), ctk("b"), ctk("c"));
        // c reaches a (active).
        let error = format!("X: in r rule //p:a: {}", open_cycle(&a, false));
        let error = on_ct1_failure(error, &c);
        let error = on_ct1_failure(error, &b);
        let error = on_ct1_failure(error, &a);
        let error = on_ct1_failure(error, &root);
        let (text, reports) = close(&error, None);
        assert!(reports.is_empty());
        assert_eq!(
            text,
            format!(
                "X: in r rule //p:a: cycle in dependency graph:\n    {root}\n.-> {a}\n|   {b}\n|   {c}\n`-- {a}"
            )
        );
        // The requested target's untrimmed key first.
        let trimmed = TrimmedRoot {
            key: root.clone(),
            untrimmed: "//p:root (abc)".into(),
        };
        let (text, _) = close(&error, Some(&trimmed));
        assert!(text.contains("graph:\n    //p:root (abc)\n    "), "{text}");
    }

    #[test]
    fn a_self_edge_and_its_path() {
        let (root, a) = (ctk("root"), ctk("a"));
        let error = on_ct1_failure(open_cycle(&a, true), &a);
        let error = on_ct1_failure(error, &root);
        let (text, _) = close(&error, None);
        assert_eq!(
            text,
            format!("cycle in dependency graph:\n    {root}\n.-> {a} [self-edge]\n`--")
        );
    }

    #[test]
    fn a_report_names_the_chain_from_the_registered_target() {
        let (registered, middle, rule) = (ctk("reg"), ctk("mid"), ctk("rule"));
        let tail =
            "\n|   toolchain types //t:t\n|   toolchain type //t:t\n`-- RegisteredToolchains";
        let error = format!("wrapped: {}", open_report(true, tail.into()));
        let error = on_ct1_failure(error, &rule);
        let error = on_ct1_failure(error, &middle);
        let error = on_ct1_failure(error, &registered);
        let (text, reports) = close(&error, None);
        let report = format!(
            "Misconfigured toolchains: //p:reg is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains\n|   //p:reg\n|   //p:mid\n|   //p:rule{tail}"
        );
        assert_eq!(text, format!("wrapped: {report}"));
        assert_eq!(reports, [(true, report)]);
        assert!(!is_open(&text));
    }
}
