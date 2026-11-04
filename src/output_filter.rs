//! Which successful actions' output the console shows (docs/RC_OPTIONS.md
//! "Output filtering"):
//!
//! * `--output_filter=REGEX` (BuildRequestOptions.outputFilter,
//!   RegexPatternConverter with `Pattern.DOTALL`) and `--auto_output_filter`
//!   (outputfilter/OutputFilteringModule.java, AutoOutputFilter.java): the
//!   Reporter's OutputFilter, applied where SkyframeActionExecutor dumps a
//!   successful action's recorded stdout/stderr
//!   (`reporter.showOutput(Label.print(owner))`, :1305-1315; a failed
//!   action's output is always reported). The regex is `find()`-matched
//!   against the owner's label; an explicit `--output_filter` wins over
//!   `--auto_output_filter`, whose `packages`/`subpackages` filters are
//!   built from the requested targets' packages once known.
//! * `--experimental_ui_max_stdouterr_bytes` (UiOptions.java:266-310): an
//!   action stream larger than the limit is replaced by
//!   `stdout (PATH) N exceeds maximum size of
//!   --experimental_ui_max_stdouterr_bytes=M bytes; skipping`; the native
//!   profile keeps the stream in memory, so PATH is where it is saved for
//!   the user (`<output base>/execroot/_main/bazel-out/_tmp/actions`).
use std::path::PathBuf;
use std::sync::RwLock;

/// AutoOutputFilter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Auto {
    #[default]
    None,
    All,
    Packages,
    Subpackages,
}

impl Auto {
    /// AutoOutputFilter.Converter (an EnumConverter, "automatic output filter").
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "all" => Ok(Self::All),
            "packages" => Ok(Self::Packages),
            "subpackages" => Ok(Self::Subpackages),
            _ => Err(format!(
                "Not a valid automatic output filter: '{value}' (should be none, all, packages or subpackages)"
            )),
        }
    }
}

/// The output options of one command.
#[derive(Clone, Debug)]
pub struct Options {
    /// `--output_filter` as given (validated).
    pub filter: Option<String>,
    pub auto: Auto,
    /// `--experimental_ui_max_stdouterr_bytes` after its converter (-1 is
    /// the maximum).
    pub max_stdouterr_bytes: usize,
}

/// MaxStdoutErrBytesConverter's maximum, `(Integer.MAX_VALUE - 8) >> 1`.
const MAX_STDOUTERR_BYTES: usize = ((i32::MAX as usize) - 8) >> 1;

impl Default for Options {
    fn default() -> Self {
        Self {
            filter: None,
            auto: Auto::None,
            max_stdouterr_bytes: 1_048_576,
        }
    }
}

/// Converters.RangeConverter (common/options/Converters.java:433-458):
/// Integer.parseInt, then the bounds.
pub fn range_int(input: &str, min: i64, max: i64) -> Result<i64, String> {
    let digits = input.strip_prefix(['+', '-']).unwrap_or(input);
    let value = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| input.parse::<i32>().ok())
        .flatten()
        .map(i64::from)
        .ok_or_else(|| format!("'{input}' is not an int"))?;
    if value < min {
        Err(format!("'{input}' should be >= {min}"))
    } else if value > max {
        Err(format!("'{input}' should be <= {max}"))
    } else {
        Ok(value)
    }
}

/// RegexPatternConverter (common/options/Converters.java:415-431): a Java
/// regular expression compiled with DOTALL.
pub fn compile(pattern: &str) -> Result<fancy_regex::Regex, String> {
    fancy_regex::Regex::new(&format!("(?s){pattern}"))
        .map_err(|error| format!("Not a valid regular expression: {error}"))
}

impl Options {
    /// One of these options (build/run/test): `Some(used_next)`.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        for name in [
            "output_filter",
            "auto_output_filter",
            "experimental_ui_max_stdouterr_bytes",
        ] {
            let Some(parsed) = crate::rc_options::value_option(arg, next, name) else {
                continue;
            };
            return Some(parsed.and_then(|(form, value, used)| {
                let failed = |error: String| format!("While parsing option {form}: {error}");
                match name {
                    "output_filter" => {
                        compile(value).map_err(failed)?;
                        self.filter = Some(value.to_owned());
                    }
                    "auto_output_filter" => self.auto = Auto::parse(value).map_err(failed)?,
                    _ => {
                        // RangeConverter(-1, MAX): `-1` is no limit.
                        let number =
                            range_int(value, -1, MAX_STDOUTERR_BYTES as i64).map_err(failed)?;
                        self.max_stdouterr_bytes = if number < 0 {
                            MAX_STDOUTERR_BYTES
                        } else {
                            number as usize
                        };
                    }
                }
                Ok(used)
            }));
        }
        None
    }
}

struct Policy {
    filter: Option<fancy_regex::Regex>,
    auto: Auto,
    max_stdouterr_bytes: usize,
    /// Where an oversized stream is saved.
    saved: Option<PathBuf>,
}

static POLICY: RwLock<Option<Policy>> = RwLock::new(None);

/// Install this command's options; `saved` is the directory oversized
/// streams are written to. The auto filter waits for [`set_targets`].
pub fn install(options: &Options, saved: PathBuf) {
    let filter = options
        .filter
        .as_deref()
        .map(|pattern| compile(pattern).expect("validated when parsed"));
    let filter = filter.or(match options.auto {
        // OUTPUT_NOTHING.
        Auto::All => Some(fancy_regex::Regex::new("(?!)").expect("valid")),
        _ => None,
    });
    *POLICY.write().unwrap_or_else(|error| error.into_inner()) = Some(Policy {
        filter,
        auto: if options.filter.is_some() {
            Auto::None
        } else {
            options.auto
        },
        max_stdouterr_bytes: options.max_stdouterr_bytes,
        saved: Some(saved),
    });
}

/// OutputFilteringModule.targetParsingComplete: the `packages` and
/// `subpackages` filters of the requested targets (`label` is the
/// canonical `//pkg:name` / `@@repo//pkg:name`; only the package matters).
pub fn set_targets<'a>(packages: impl IntoIterator<Item = &'a str>) {
    let mut guard = POLICY.write().unwrap_or_else(|error| error.into_inner());
    let Some(policy) = guard.as_mut() else {
        return;
    };
    if !matches!(policy.auto, Auto::Packages | Auto::Subpackages) {
        return;
    }
    // getPackages: //java/foo and //javatests/foo are one package; an
    // alphabetically sorted set (ImmutableSortedSet natural order).
    let mut names: Vec<String> = packages
        .into_iter()
        .map(|name| {
            if name.starts_with("java/") || name.starts_with("javatests/") {
                format!("java(tests)?{}", &name[name.find('/').expect("a slash")..])
            } else {
                name.to_owned()
            }
        })
        .collect();
    names.sort_by(|a, b| crate::native_rules::java_string_cmp(a, b));
    names.dedup();
    const SYSTEM: &str = "(unknown)|@@bazel_tools//tools:internal_platform";
    let pattern = if policy.auto == Auto::Packages {
        format!("{SYSTEM}|^//({}):", names.join("|"))
    } else {
        // A package below one already listed adds nothing.
        let mut kept: Vec<String> = Vec::new();
        for name in names {
            if kept
                .last()
                .is_some_and(|previous| name.starts_with(&format!("{previous}/")))
            {
                continue;
            }
            kept.push(name);
        }
        format!("{SYSTEM}|^//({})[/:]", kept.join("|"))
    };
    // Pattern.compile without flags; an unparsable package regex (Bazel
    // does not escape them) shows everything rather than failing late.
    policy.filter = fancy_regex::Regex::new(&pattern).ok();
}

/// Reporter.showOutput for an action owned by `owner` (the action id's
/// owner part, `L` or `L[config]`).
pub fn shows_output(owner: &str) -> bool {
    let guard = POLICY.read().unwrap_or_else(|error| error.into_inner());
    let Some(Policy {
        filter: Some(filter),
        ..
    }) = guard.as_ref()
    else {
        return true;
    };
    let label = match owner.rfind('[') {
        Some(index) if owner.ends_with(']') => &owner[..index],
        _ => owner,
    };
    filter.is_match(label).unwrap_or(true)
}

/// The limit and the saved-streams directory of this command.
fn limit_and_saved() -> (usize, Option<PathBuf>) {
    let guard = POLICY.read().unwrap_or_else(|error| error.into_inner());
    guard.as_ref().map_or((1_048_576, None), |policy| {
        (policy.max_stdouterr_bytes, policy.saved.clone())
    })
}

/// The saved file of action `id`'s stream `name`.
fn saved_file(directory: &std::path::Path, name: &str, id: &str) -> PathBuf {
    directory.join(format!(
        "{name}-{}",
        id.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    ))
}

fn notice(name: &str, path: &std::path::Path, size: u64, limit: usize) -> Vec<u8> {
    format!(
        "{name} ({}) {size} exceeds maximum size of --experimental_ui_max_stdouterr_bytes={limit} bytes; skipping\n",
        path.display(),
    )
    .into_bytes()
}

/// [`small_enough`] of a stream still in its file `log` (`size` bytes):
/// `None` when it is small enough to read, else the notice, the file
/// copied where the notice says.
pub fn oversized(name: &str, id: &str, log: &std::path::Path, size: u64) -> Option<Vec<u8>> {
    let (limit, saved) = limit_and_saved();
    if size <= limit as u64 {
        return None;
    }
    let path = saved
        .map(|directory| {
            let path = saved_file(&directory, name, id);
            let _ = std::fs::create_dir_all(&directory);
            let _ = std::fs::copy(log, &path);
            path
        })
        .unwrap_or_default();
    Some(notice(name, &path, size, limit))
}

/// UiEventHandler.getContentIfSmallEnough: `bytes`, or the oversize notice
/// (the stream saved to a file named after the action).
pub fn small_enough<'a>(name: &str, id: &str, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
    let (limit, saved) = limit_and_saved();
    if bytes.len() <= limit {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let path = saved
        .map(|directory| {
            let path = saved_file(&directory, name, id);
            let _ = std::fs::create_dir_all(&directory);
            let _ = std::fs::write(&path, bytes);
            path
        })
        .unwrap_or_default();
    std::borrow::Cow::Owned(notice(name, &path, bytes.len() as u64, limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_like_bazel() {
        let mut options = Options::default();
        assert_eq!(options.parse("--output_filter=^//a", None), Some(Ok(false)));
        assert_eq!(options.filter.as_deref(), Some("^//a"));
        assert_eq!(
            options.parse("--auto_output_filter", Some("Packages")),
            Some(Ok(true))
        );
        assert_eq!(options.auto, Auto::Packages);
        assert_eq!(
            options.parse("--auto_output_filter=x", None),
            Some(Err("While parsing option --auto_output_filter=x: Not a valid automatic output filter: 'x' (should be none, all, packages or subpackages)".into()))
        );
        assert!(
            options
                .parse("--output_filter=[", None)
                .unwrap()
                .unwrap_err()
                .starts_with(
                    "While parsing option --output_filter=[: Not a valid regular expression: "
                )
        );
        assert_eq!(
            options.parse("--experimental_ui_max_stdouterr_bytes=-1", None),
            Some(Ok(false))
        );
        assert_eq!(options.max_stdouterr_bytes, MAX_STDOUTERR_BYTES);
        assert!(
            options
                .parse("--experimental_ui_max_stdouterr_bytes=-2", None)
                .unwrap()
                .is_err()
        );
        assert_eq!(
            options
                .parse("--nooutput_filter", None)
                .unwrap()
                .unwrap_err(),
            "--nooutput_filter :: Illegal use of 'no' prefix on non-boolean option: --nooutput_filter"
        );
        assert_eq!(options.parse("--other", None), None);
    }
}
