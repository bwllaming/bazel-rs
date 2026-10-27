//! `--attempt_to_print_relative_paths` (runtime/UiOptions.java:221-229, a
//! common option of every command): Bazel's UI prints the location part of an
//! event relative to the workspace or a package path root
//! (runtime/LocationPrinter.java:43-80, runtime/UiEventHandler.java:223-224,
//! :475). The default `--package_path` is `%workspace%`, so only paths under
//! the workspace change. Native events carry their location as the message
//! prefix `PATH[:LINE[:COLUMN]]: ` (crate::engine `located`), so the printer
//! rewrites that prefix of each printed line. Paths under the native cache
//! directory (Bazel's output base, never a package path root) stay absolute.

use std::borrow::Cow;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct LocationPrinter {
    enabled: bool,
    workspace: PathBuf,
    cache: PathBuf,
}

impl LocationPrinter {
    pub fn new(enabled: bool, workspace: PathBuf, cache: PathBuf) -> Self {
        Self {
            enabled,
            workspace,
            cache,
        }
    }

    /// The message as printed: each line's leading event location relative
    /// to the workspace when enabled.
    pub fn print<'a>(&self, message: &'a str) -> Cow<'a, str> {
        if !self.enabled {
            return Cow::Borrowed(message);
        }
        let mut changed = false;
        let lines = message
            .split('\n')
            .map(|line| {
                let printed = self.line(line);
                changed |= matches!(printed, Cow::Owned(_));
                printed
            })
            .collect::<Vec<_>>();
        if changed {
            Cow::Owned(lines.join("\n"))
        } else {
            Cow::Borrowed(message)
        }
    }

    fn line<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let Some((path, rest)) = location_prefix(line) else {
            return Cow::Borrowed(line);
        };
        let path = std::path::Path::new(path);
        // PathFragment.startsWith / relativeTo (segment-wise), never the cache.
        if path.starts_with(&self.cache) {
            return Cow::Borrowed(line);
        }
        match path.strip_prefix(&self.workspace) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                Cow::Owned(format!("{}{rest}", relative.display()))
            }
            _ => Cow::Borrowed(line),
        }
    }
}

/// `(PATH, ":LINE[:COLUMN]: rest")` for a line starting with an absolute
/// event location `PATH[:LINE[:COLUMN]]: ` (Location.toString).
fn location_prefix(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with('/') {
        return None;
    }
    let end = line.find(':')?;
    let rest = &line[end..];
    let mut tail = rest;
    for _ in 0..2 {
        let Some(after) = tail.strip_prefix(':') else {
            break;
        };
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        tail = &after[digits..];
    }
    // The native span form `LINE:COL-COL` or `LINE:COL-LINE:COL` of the
    // same location.
    if tail.len() < rest.len()
        && let Some(after) = tail.strip_prefix('-')
    {
        let span = after
            .split(": ")
            .next()
            .filter(|span| {
                !span.is_empty()
                    && span.split(':').count() <= 2
                    && span
                        .split(':')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
            .map_or(0, str::len);
        if span > 0 {
            tail = &after[span..];
        }
    }
    tail.starts_with(": ").then_some((&line[..end], rest))
}

/// The printer of this command (--attempt_to_print_relative_paths), set
/// once its options are parsed (crate::main).
static PRINTER: std::sync::OnceLock<LocationPrinter> = std::sync::OnceLock::new();

/// Set this command's printer (once).
pub fn set(printer: LocationPrinter) {
    let _ = PRINTER.set(printer);
}

/// `message` as this command prints it (unchanged before [`set`]).
pub fn located(message: &str) -> std::borrow::Cow<'_, str> {
    match PRINTER.get() {
        Some(printer) => printer.print(message),
        None => std::borrow::Cow::Borrowed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printer(enabled: bool) -> LocationPrinter {
        LocationPrinter::new(enabled, "/ws".into(), "/ws/bazel-rs-out".into())
    }

    #[test]
    fn workspace_locations_become_relative() {
        let p = printer(true);
        assert_eq!(
            p.print("/ws/pkg/BUILD.bazel:3:7: in genrule rule //pkg:x: boom"),
            "pkg/BUILD.bazel:3:7: in genrule rule //pkg:x: boom"
        );
        assert_eq!(p.print("/ws/BUILD:2: x"), "BUILD:2: x");
        assert_eq!(
            p.print("/ws/a/BUILD.bazel:1:6-38: x"),
            "a/BUILD.bazel:1:6-38: x"
        );
        assert_eq!(p.print("/ws/a/BUILD:1:6-2:3: x"), "a/BUILD:1:6-2:3: x");
        assert_eq!(
            p.print("/ws/BUILD: no such target"),
            "BUILD: no such target"
        );
        assert_eq!(
            p.print("first\n/ws/a/BUILD:1:1: second"),
            "first\na/BUILD:1:1: second"
        );
        for unchanged in [
            "/wsx/BUILD:1:1: other workspace",
            "/ws/bazel-rs-out/repositories/r/BUILD:1:1: external",
            "/elsewhere/BUILD:1:1: outside",
            "cannot read /ws/BUILD:1:1: not a prefix",
            "/ws/pkg/BUILD.bazel:x:1: not a location",
            "/ws/pkg/BUILD.bazel",
            "/ws/pkg/BUILD:1:2-: x",
        ] {
            assert_eq!(p.print(unchanged), unchanged);
        }
        let off = printer(false);
        assert_eq!(off.print("/ws/BUILD:1:1: x"), "/ws/BUILD:1:1: x");
    }
}
