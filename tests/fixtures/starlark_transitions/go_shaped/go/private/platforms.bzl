# Shaped after rules_go 0.63.0 go/private/platforms.bzl (a subset).
GOOS_GOARCH = (
    ("darwin", "arm64"),
    ("linux", "amd64"),
)

CGO_GOOS_GOARCH = {
    ("darwin", "arm64"): None,
    ("linux", "amd64"): None,
}
