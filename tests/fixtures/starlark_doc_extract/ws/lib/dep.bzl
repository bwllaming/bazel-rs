"""Dependency module."""

DepInfo = provider(doc = "A provider from the dep.", fields = ["x"])

def dep_helper(a):
    """Helper in dep.

    Args:
      a: the a.
    """
    return a
