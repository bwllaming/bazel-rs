SourceInfo = provider(fields = ["file"])

def _source_info(ctx):
    return [SourceInfo(file = ctx.file.source), DefaultInfo(files = depset([]))]

source_info = rule(implementation = _source_info, attrs = {
    "source": attr.label(allow_single_file = True),
})

def _forward_source(ctx):
    return [ctx.attr.source[SourceInfo], DefaultInfo(files = depset([]))]

forward_source = rule(implementation = _forward_source, attrs = {
    "source": attr.label(),
})

def _inspect_source(ctx):
    source = ctx.attr.source[SourceInfo].file
    default = ctx.file.definition_default
    metadata = ctx.actions.declare_file(ctx.label.name + "/metadata.txt")
    copy = ctx.actions.declare_file("copy.txt", sibling = metadata)
    text = str(ctx.label) + "\n"
    text += source.path + "|" + source.short_path + "|" + str(source.owner) + "\n"
    text += default.path + "|" + default.short_path + "|" + str(default.owner) + "\n"
    text += copy.short_path + "|" + str(copy.owner) + "\n"
    ctx.actions.write(metadata, text)
    ctx.actions.run_shell(inputs = [source], outputs = [copy], command = "cat " + source.path + " > " + copy.path)
    return [DefaultInfo(files = depset([metadata, copy]))]

inspect_source = rule(implementation = _inspect_source, attrs = {
    "source": attr.label(),
    "definition_default": attr.label(default = Label("//p:data.txt"), allow_single_file = True),
})
