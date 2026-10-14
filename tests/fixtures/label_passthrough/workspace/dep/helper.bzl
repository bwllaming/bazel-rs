def copy_dep(value):
    return Label(value)
def local_dep():
    return Label("//:same")
def shared_dep():
    return Label("@shared//:same")
