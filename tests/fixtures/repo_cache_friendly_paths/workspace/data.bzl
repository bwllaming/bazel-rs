def _data_repo(ctx):
    ctx.file("BUILD.bazel", 'exports_files(glob(["**"]))\n')
    ctx.file("file.txt", "data\n")
    ctx.file("label.txt", "label\n")
    ctx.file("dir/a.txt", "a\n")
    ctx.file("dir/b.txt", "b\n")
    ctx.file("tree/x/y.txt", "y\n")
    ctx.file("tpl.txt", "hello {NAME}\n")

data_repo = repository_rule(_data_repo)
