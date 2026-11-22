def _argv_test(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(
        output = exe,
        is_executable = True,
        # The test-setup.sh command line (the parent of the test) after the
        # script itself, and TEST_BINARY.
        content = """#!/bin/bash
args=$(tr '\\0' '\\n' < /proc/$PPID/cmdline | sed -n '/test-setup.sh$/,$p' | tail -n +2 | tr '\\n' ' ')
echo "SETUP_ARGS ${TEST_TARGET}: ${args}"
echo "TEST_BINARY ${TEST_TARGET}: ${TEST_BINARY}"
""",
    )
    return [DefaultInfo(executable = exe)]

argv_test = rule(
    implementation = _argv_test,
    test = True,
)
