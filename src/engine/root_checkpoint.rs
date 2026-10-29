//! Opt-in serial, current-demand root cut. No historical names or coverage enter
//! its selection or admission. The callback sees lookup-only completed results.
use super::{ConfigId, Engine};
use crate::repository::CurrentRefresh;
use std::fmt;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootCut {
    pub(crate) ordinal: u64,
    pub(crate) registration: [u8; 32],
}

pub enum Outcome<T = ()> {
    Continue(T),
    AcceptedQuery(Box<super::cquery::Graph>),
}

pub(super) type Callback<'a> = dyn FnMut(CurrentRefresh, RootCut) -> Result<Outcome, String> + 'a;

/// Admission ceilings, not a promise to bound finishing a root. Environment
/// controls may only lower these ceilings (zero disables overlap entirely).
#[derive(Clone, Copy)]
pub struct Budget {
    pub(crate) millis: u64,
    pub(crate) nodes: usize,
    pub(crate) rss_mb: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            millis: 30_000,
            nodes: 50_000,
            rss_mb: 1024,
        }
    }
}

pub(super) struct Request {
    pub(super) first: Option<ConfigId>,
    started: Instant,
    budget: Budget,
}

impl Request {
    pub(super) fn new(budget: Budget) -> Self {
        Self {
            first: None,
            started: Instant::now(),
            budget,
        }
    }

    fn admission(&self, nodes: usize, rss: Option<u64>) -> Admission {
        // The caller has already obtained the single RSS sample, even when a
        // zero control short-circuits the elapsed comparison below.
        Admission::sample(self.budget, nodes, rss, || self.started.elapsed())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failed {
    MaxMsZero,
    MaxNodesZero,
    MaxRssBytesZero,
    ElapsedExceeded,
    NodesExceeded,
    RssUnknown,
    RssExceeded,
}

impl Failed {
    // The original predicate's short-circuit order. Unknown/excess are the
    // distinct ways its final RSS predicate can fail.
    const ORDER: [Self; 7] = [
        Self::MaxMsZero,
        Self::MaxNodesZero,
        Self::MaxRssBytesZero,
        Self::ElapsedExceeded,
        Self::NodesExceeded,
        Self::RssUnknown,
        Self::RssExceeded,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::MaxMsZero => "max_ms_zero",
            Self::MaxNodesZero => "max_nodes_zero",
            Self::MaxRssBytesZero => "max_rss_bytes_zero",
            Self::ElapsedExceeded => "elapsed_exceeded",
            Self::NodesExceeded => "nodes_exceeded",
            Self::RssUnknown => "rss_unknown",
            Self::RssExceeded => "rss_exceeded",
        }
    }
}

/// Retained decision inputs, not another measurement. Formatting is deferred
/// until a refusal under EXPLAIN; it can inspect only these already-held values.
struct Admission {
    budget: Budget,
    nodes: usize,
    rss: Option<u64>,
    elapsed: Option<Duration>,
    first_failed: Option<Failed>,
}

impl Admission {
    fn sample(
        budget: Budget,
        nodes: usize,
        rss: Option<u64>,
        elapsed: impl FnOnce() -> Duration,
    ) -> Self {
        let mut sample = Self {
            budget,
            nodes,
            rss,
            // Exactly the old elapsed comparison point: all controls positive,
            // after the caller's RSS acquisition. No rounded-millisecond test.
            elapsed: (budget.millis > 0 && budget.nodes > 0 && budget.rss_mb > 0).then(elapsed),
            first_failed: None,
        };
        let first_failed = sample.failed().next();
        sample.first_failed = first_failed;
        sample
    }

    fn failed(&self) -> impl Iterator<Item = Failed> + '_ {
        Failed::ORDER
            .into_iter()
            .filter(|criterion| match criterion {
                Failed::MaxMsZero => self.budget.millis == 0,
                Failed::MaxNodesZero => self.budget.nodes == 0,
                Failed::MaxRssBytesZero => self.budget.rss_mb == 0,
                Failed::ElapsedExceeded => self
                    .elapsed
                    .is_some_and(|elapsed| elapsed > Duration::from_millis(self.budget.millis)),
                Failed::NodesExceeded => self.nodes > self.budget.nodes,
                Failed::RssUnknown => self.rss.is_none(),
                Failed::RssExceeded => self
                    .rss
                    .is_some_and(|rss| rss > self.budget.rss_mb * (1 << 20)),
            })
    }
}

impl fmt::Display for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "first_failed={} all_supported_failed=[",
            self.first_failed.map_or("none", Failed::name)
        )?;
        for (index, failed) in self.failed().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            f.write_str(failed.name())?;
        }
        f.write_str("] elapsed_ns=")?;
        match self.elapsed {
            Some(elapsed) => write!(f, "{}", elapsed.as_nanos())?,
            None => f.write_str("not_evaluated")?,
        }
        write!(
            f,
            " max_ms={} nodes={} max_nodes={} rss_bytes=",
            self.budget.millis, self.nodes, self.budget.nodes
        )?;
        match self.rss {
            Some(rss) => write!(f, "{rss}")?,
            None => f.write_str("unknown")?,
        }
        write!(f, " max_rss_bytes={}", self.budget.rss_mb * (1 << 20))
    }
}

impl Engine {
    pub(super) fn root_cut(
        &mut self,
        ordinal: usize,
        nodes: usize,
        callback: &mut Callback<'_>,
    ) -> Result<Outcome, String> {
        let Some(registration) = self
            .root_checkpoint
            .as_ref()
            .and_then(|request| request.first)
        else {
            return Ok(Outcome::Continue(()));
        };
        // Take BEFORE admission/callback: a decline or miss cannot search later
        // roots for better coverage. All necessary fallback state stays live.
        let request = self.root_checkpoint.take().expect("active root request");
        let mut admission = None;
        let reason = if self.loading_phase_threads != 1
            || self.prefetch.is_some()
            || !self.repositories.cquery_events_quiescent()
        {
            Some("requires already-serial quiescent sources")
        } else if !self.warnings.is_empty() {
            Some("queued diagnostics still belong to ordinary command completion")
        } else {
            let sampled = request.admission(nodes, crate::memory_limit::rss_bytes());
            let refused = sampled.first_failed.is_some();
            admission = Some(sampled);
            refused.then_some("current work exceeds lookup admission budget")
        };
        if let Some(reason) = reason {
            if crate::snapshot::explain() {
                if let Some(admission) = admission {
                    crate::snapshot::diag::line_fmt(format_args!(
                        "bazel-rs-cquery-root: declined: {reason}; {admission}"
                    ));
                } else {
                    crate::snapshot::diag::line_fmt(format_args!(
                        "bazel-rs-cquery-root: declined: {reason}"
                    ));
                }
            }
            return Ok(Outcome::Continue(()));
        }
        let cut = RootCut {
            ordinal: ordinal as u64,
            registration: registration.bytes(),
        };
        if crate::snapshot::explain() {
            crate::snapshot::diag::line_fmt(format_args!(
                "bazel-rs-cquery-root: cut ordinal={} registration={registration}",
                cut.ordinal
            ));
        }
        callback(CurrentRefresh::seal(self.repositories.clone(), true), cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(sample: &Admission, expected: &[Failed]) {
        assert_eq!(sample.first_failed, expected.first().copied());
        assert_eq!(sample.failed().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn exact_equalities_pass_and_one_nanosecond_over_is_not_rounded_away() {
        let budget = Budget::default();
        let limit = Duration::from_millis(budget.millis);
        let equal = Admission::sample(budget, budget.nodes, Some(1024 << 20), || limit);
        check(&equal, &[]);
        assert_eq!(
            equal.to_string(),
            "first_failed=none all_supported_failed=[] elapsed_ns=30000000000 max_ms=30000 nodes=50000 max_nodes=50000 rss_bytes=1073741824 max_rss_bytes=1073741824"
        );
        let over = Admission::sample(budget, budget.nodes, Some(1024 << 20), || {
            limit + Duration::from_nanos(1)
        });
        check(&over, &[Failed::ElapsedExceeded]);
        assert!(over.to_string().contains("elapsed_ns=30000000001 "));
        check(
            &Admission::sample(budget, 0, Some(0), || Duration::ZERO),
            &[],
        );
    }

    #[test]
    fn independent_failures_distinguish_unknown_rss_from_excess() {
        let budget = Budget {
            millis: 2,
            nodes: 2,
            rss_mb: 1,
        };
        for (elapsed, nodes, rss, failed) in [
            (
                Duration::from_nanos(2_000_001),
                2,
                Some(1 << 20),
                Failed::ElapsedExceeded,
            ),
            (
                Duration::from_millis(2),
                3,
                Some(1 << 20),
                Failed::NodesExceeded,
            ),
            (Duration::from_millis(2), 2, None, Failed::RssUnknown),
            (
                Duration::from_millis(2),
                2,
                Some((1 << 20) + 1),
                Failed::RssExceeded,
            ),
        ] {
            let sample = Admission::sample(budget, nodes, rss, || elapsed);
            check(&sample, &[failed]);
            assert!(sample.to_string().contains(&format!(
                "first_failed={} all_supported_failed=[{}]",
                failed.name(),
                failed.name()
            )));
            if rss.is_none() {
                assert!(
                    sample
                        .to_string()
                        .contains("rss_bytes=unknown max_rss_bytes=1048576")
                );
            }
        }
    }

    #[test]
    fn zero_controls_never_evaluate_elapsed_but_retain_the_prior_rss_sample() {
        let defaults = Budget::default();
        for (budget, expected) in [
            (
                Budget {
                    millis: 0,
                    ..defaults
                },
                Failed::MaxMsZero,
            ),
            (
                Budget {
                    nodes: 0,
                    ..defaults
                },
                Failed::MaxNodesZero,
            ),
            (
                Budget {
                    rss_mb: 0,
                    ..defaults
                },
                Failed::MaxRssBytesZero,
            ),
        ] {
            let sample = Admission::sample(budget, 0, Some(0), || {
                panic!("zero controls must short-circuit the clock")
            });
            check(&sample, &[expected]);
            assert_eq!(sample.elapsed, None);
            let text = sample.to_string();
            assert!(text.contains("elapsed_ns=not_evaluated"));
            assert!(text.contains("rss_bytes=0 "));
        }
    }

    #[test]
    fn simultaneous_failures_follow_predicate_order_and_do_not_invent_elapsed() {
        let budget = Budget {
            millis: 2,
            nodes: 2,
            rss_mb: 1,
        };
        let over = Duration::from_nanos(2_000_001);
        let sample = Admission::sample(budget, 3, Some((1 << 20) + 1), || over);
        check(
            &sample,
            &[
                Failed::ElapsedExceeded,
                Failed::NodesExceeded,
                Failed::RssExceeded,
            ],
        );
        assert_eq!(
            sample.to_string(),
            "first_failed=elapsed_exceeded all_supported_failed=[elapsed_exceeded,nodes_exceeded,rss_exceeded] elapsed_ns=2000001 max_ms=2 nodes=3 max_nodes=2 rss_bytes=1048577 max_rss_bytes=1048576"
        );
        check(
            &Admission::sample(budget, 3, None, || over),
            &[
                Failed::ElapsedExceeded,
                Failed::NodesExceeded,
                Failed::RssUnknown,
            ],
        );
        check(
            &Admission::sample(budget, 3, None, || Duration::ZERO),
            &[Failed::NodesExceeded, Failed::RssUnknown],
        );
        let zero = Admission::sample(
            Budget {
                millis: 0,
                nodes: 0,
                rss_mb: 0,
            },
            3,
            Some(1),
            || panic!("unmeasured elapsed"),
        );
        check(
            &zero,
            &[
                Failed::MaxMsZero,
                Failed::MaxNodesZero,
                Failed::MaxRssBytesZero,
                Failed::NodesExceeded,
                Failed::RssExceeded,
            ],
        );
        assert_eq!(
            zero.to_string(),
            "first_failed=max_ms_zero all_supported_failed=[max_ms_zero,max_nodes_zero,max_rss_bytes_zero,nodes_exceeded,rss_exceeded] elapsed_ns=not_evaluated max_ms=0 nodes=3 max_nodes=0 rss_bytes=1 max_rss_bytes=0"
        );
        check(
            &Admission::sample(
                Budget {
                    millis: 0,
                    ..budget
                },
                0,
                None,
                || panic!("unmeasured elapsed"),
            ),
            &[Failed::MaxMsZero, Failed::RssUnknown],
        );
    }

    #[test]
    fn clock_is_sampled_once_after_rss_and_formatting_only_uses_retained_duration() {
        let rss_read = std::cell::Cell::new(false);
        let clock_reads = std::cell::Cell::new(0);
        let sample = Admission::sample(
            Budget::default(),
            1,
            {
                rss_read.set(true);
                Some(1)
            },
            || {
                assert!(rss_read.get());
                clock_reads.set(clock_reads.get() + 1);
                Duration::from_nanos(30_000_000_001)
            },
        );
        check(&sample, &[Failed::ElapsedExceeded]);
        for _ in 0..2 {
            assert!(sample.to_string().contains("elapsed_ns=30000000001"));
        }
        assert_eq!(clock_reads.get(), 1);
        assert!(
            Admission::sample(Budget::default(), 1, Some(1), || Duration::MAX)
                .to_string()
                .contains(&format!("elapsed_ns={}", Duration::MAX.as_nanos()))
        );
    }
}
