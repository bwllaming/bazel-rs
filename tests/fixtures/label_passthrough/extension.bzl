load(":producer.bzl", "render")

def _repository(ctx):
    print("LP_REPOSITORY_CALLBACK")
    ctx.file("BUILD.bazel", 'exports_files(["data.txt"])\n', executable=False)
    ctx.file("data.txt", ctx.attr.text, executable=False)

maker = repository_rule(implementation=_repository, attrs={"text": attr.string()})

def _extension(ctx):
    print("LP_EXTENSION_CALLBACK")
    maker(name="made", text=render())

files = module_extension(implementation=_extension)
