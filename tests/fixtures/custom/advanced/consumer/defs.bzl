load("//advanced/provider_defs:defs.bzl", "OtherInfo", ConsumerPayload = "PayloadInfo")

def _consume_impl(ctx):
    dep = ctx.attr.dep
    if ConsumerPayload not in dep or OtherInfo in dep or DefaultInfo not in dep:
        fail("provider identity or membership mismatch")
    if len(dep[DefaultInfo].files.to_list()) != 0:
        fail("provider-only file leaked into default outputs")
    payload = dep[ConsumerPayload]
    suffix = "%s|%s|%s|%s\n" % (
        payload.message, ",".join(payload.words),
        payload.metadata.tag, payload.metadata.numbers[1],
    )
    out = ctx.actions.declare_file("consumed.txt")
    ctx.actions.run_shell(
        inputs = [payload.file],
        outputs = [out],
        command = 'cat "$1" > "$2"; printf "%s" "$3" >> "$2"',
        arguments = [payload.file.path, out.path, suffix],
    )
    return [DefaultInfo(files = depset([out]))]

consume = rule(implementation = _consume_impl, attrs = {"dep": attr.label(mandatory = True)})
