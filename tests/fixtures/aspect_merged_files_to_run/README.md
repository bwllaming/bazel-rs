# aspect_merged_files_to_run

An aspect-applied dependency (MergedConfiguredTarget) keeps its base target's
FilesToRunProvider and RunfilesProvider: an aspect cannot return them, so
`getProvider` falls through to the base (MergedConfiguredTarget.java:131-140)
and `target[DefaultInfo]` / `target.files_to_run` delegate to them. `plain`,
`with_asp` (an aspect returning a provider) and `with_empty` (an aspect
returning nothing) print the same executable, default runfiles and files of an
executable Starlark rule, a non-executable rule naming an executable in its
DefaultInfo (like ~/code's `copyfiles`), an executable rule symlinking that
rule's output (like `sh_binary` over it) and a source file. (Aspects over
native rules such as genrule/filegroup: tests/fixtures/native_aspects.)
(Found on ~/code: `rules_multirun`'s `commands` attribute carries an aspect,
and an `sh_binary` there lost its executable.) `genuine.txt` holds the
genuine 9.3.0 capture; tests/aspects.rs
`aspect_applied_dependencies_keep_files_to_run` compares the native prints
with it.
