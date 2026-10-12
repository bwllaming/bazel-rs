PayloadInfo = provider(fields = {
    "file": "A generated File intentionally absent from DefaultInfo.files.",
    "message": "Scalar payload.",
    "words": "List payload retained across configured targets.",
    "metadata": "Nested struct/list payload.",
})

# A different exported provider must not compare equal just because it was
# created in the same .bzl module or has a similarly named field.
OtherInfo = provider(fields = ["file"])
