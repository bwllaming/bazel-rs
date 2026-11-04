def format(target):
    p = providers(target)
    return "%s|%s" % (target.label, sorted(p.keys()) if p != None else None)
