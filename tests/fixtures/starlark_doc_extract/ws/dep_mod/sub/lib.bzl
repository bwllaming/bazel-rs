"""External lib."""
ExtInfo = provider(doc = "External provider.", fields = {"v": "The v."})
def ext_function(x = Label("//sub:lib"), y = Label("@dep_mod//sub:other")):
    """External function."""
    return x
