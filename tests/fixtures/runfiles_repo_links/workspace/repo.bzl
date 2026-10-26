def _impl(rctx):
    rctx.file("bin/tool", rctx.read(Label("//:tool.sh")), executable = True)
    # Not a declared file of any target: reachable only next to bin/tool.
    rctx.file("bin/real/tool", "#!/bin/sh\necho real-tool-ran\n", executable = True)
    rctx.file("BUILD.bazel", 'exports_files(["bin/tool"])\n')

ext_repo = repository_rule(implementation = _impl)
