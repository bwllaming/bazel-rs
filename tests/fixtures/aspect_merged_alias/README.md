# aspect_merged_alias

A dependency with attribute aspects applied is a MergedConfiguredTarget:
`<merged target ACTUAL>` (MergedConfiguredTarget.java:371-373, its base's
label: an alias's actual, AliasConfiguredTarget.getLabel), whether it was
requested through an alias or directly, and whichever request comes first.
`ctx.expand_location` keeps the label as written (the alias's AliasProvider
is the merged target's). The same target `:p` (and `:q` below the aspect's
`ctx.rule.attr.deps`) is requested through `:p_alias`/`:q_alias` and
directly. bazel-rs used to share one aspect-configured `Target` value per
(actual, configuration, aspects) whose contents were the first requester's in
plan order (`<alias target //:q_alias of //:q>` for every consumer).
`genuine.txt` holds the genuine 9.3.1 capture; tests/aspects.rs
`aspect_applied_dependencies_are_merged_targets_whatever_the_request` compares
the native prints with it.
