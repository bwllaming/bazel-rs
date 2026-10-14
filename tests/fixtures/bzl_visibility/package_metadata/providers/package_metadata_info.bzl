"""Declares provider `PackageMetadataInfo` (mirrors the supply-chain layout)."""

visibility("public")

PackageMetadataInfo = provider(
    doc = """Provider for declaring metadata about a Bazel package.""",
    fields = {
        "metadata": "The File containing metadata about the package.",
        "purl": "The identifier of the package (a package URL string).",
    },
)

def package_metadata_purl(info):
    return info.purl
