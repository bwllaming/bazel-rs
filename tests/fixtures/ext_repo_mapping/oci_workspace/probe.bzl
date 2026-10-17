def _probe(ctx):
    print("N45|oci_probe|resolved=%s" % (ctx.toolchains["//:tt"] != None))
    return []

probe = rule(_probe, toolchains = [config_common.toolchain_type("//:tt", mandatory = False)])
