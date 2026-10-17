"""A reproducible extension with persisted facts and a facts_version."""

load("//:repo.bzl", "gen_repo")

def _facts_impl(ctx):
    print("LOCKEXT_EVAL facts previous=%s" % ctx.facts.get("hash", "none"))
    gen_repo(name = "gen_facts", s = "facts")
    return ctx.extension_metadata(
        reproducible = True,
        facts = {
            "hash": "abc123",
            "list": [1, 2.5, True, "s"],
            "nested": {"z": 1, "a": {"b": "c"}},
        },
    )

facts_ext = module_extension(implementation = _facts_impl, facts_version = 3)
