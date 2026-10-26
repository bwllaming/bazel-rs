def _repo(rctx):
    print("in repo rule", rctx.name)
    rctx.file("BUILD", "filegroup(name = 'f', visibility = ['//visibility:public'])")

repo = repository_rule(_repo)

def _ext(mctx):
    print("in extension")
    repo(name = "r")

ext = module_extension(_ext)
