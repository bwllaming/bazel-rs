def copy_root(value):
    return Label(value)
def local_root():
    return Label("//:same")
def shared_root():
    return Label("@shared//:same")
