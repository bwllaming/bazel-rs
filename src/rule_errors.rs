//! The rule-error events of a configured target's analysis, formatted like
//! RuleContext's EventHandlingErrorReporter
//! (analysis/EventHandlingErrorReporter.java:80-101) and reported like
//! StarlarkRuleConfiguredTargetUtil.buildRule (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:97-115):
//! each event at the rule's location, the target failing once
//! RuleContext.hasErrors. One formatter for every increment that reports
//! `ruleError` / `attributeError` events (Starlark implementations, native
//! factories, prerequisite validation).

/// The rule an event belongs to: RuleContext.getRuleClassNameForLogging and
/// the label (Label.toString / canonical form).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RuleEvents<'a> {
    pub rule_class: &'a str,
    pub label: &'a str,
}

impl RuleEvents<'_> {
    /// prefixRuleMessage (:80-82): `in KIND rule L: MESSAGE`.
    pub(crate) fn rule_message(&self, message: &str) -> String {
        format!("in {} rule {}: {message}", self.rule_class, self.label)
    }

    /// completeAttributeMessage (:91-101) with maskInternalAttributeNames
    /// (:84-86, Attribute.isImplicit: a `$` name): `in A attribute of KIND
    /// rule L: MESSAGE`. Native targets are never macro-created here, so the
    /// macro appendix is empty.
    pub(crate) fn attribute_message(&self, attribute: &str, message: &str) -> String {
        let shown = if attribute.starts_with('$') {
            "(an implicit dependency)"
        } else {
            attribute
        };
        format!(
            "in {shown} attribute of {} rule {}: {message}",
            self.rule_class, self.label
        )
    }
}

/// The reported events of one target as printed: each at the rule's
/// location (`LOC: MESSAGE`), identical events once (the event handler
/// de-duplicates), in report order.
pub(crate) fn located_events(
    location: Option<&str>,
    events: impl IntoIterator<Item = String>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in events {
        let line = match location {
            Some(location) => format!("{location}: {event}"),
            None => event,
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Events already formatted and located by [`located_events`]: callers
/// print them as they are (no `analysis of L:` wrapper).
#[derive(Debug)]
pub(crate) struct ReportedErrors(pub String);

impl std::fmt::Display for ReportedErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReportedErrors {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_follow_event_handling_error_reporter() {
        let rule = RuleEvents {
            rule_class: "genrule",
            label: "//p:g",
        };
        assert_eq!(rule.rule_message("boom"), "in genrule rule //p:g: boom");
        assert_eq!(
            rule.attribute_message("cmd", "$(X) not defined"),
            "in cmd attribute of genrule rule //p:g: $(X) not defined"
        );
        assert_eq!(
            rule.attribute_message("$genrule_setup", "x"),
            "in (an implicit dependency) attribute of genrule rule //p:g: x"
        );
        assert_eq!(
            located_events(
                Some("/w/p/BUILD:1:8"),
                ["a".to_owned(), "b".to_owned(), "a".to_owned()]
            ),
            "/w/p/BUILD:1:8: a\n/w/p/BUILD:1:8: b"
        );
        assert_eq!(located_events(None, ["a".to_owned()]), "a");
    }
}
