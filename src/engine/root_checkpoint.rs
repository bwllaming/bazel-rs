//! Opt-in serial, current-demand root cut. No historical names or coverage enter
//! its selection or admission. The callback sees lookup-only completed results.
use super::{ConfigId, Engine};
use crate::repository::CurrentRefresh;
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

    fn admitted(&self, nodes: usize, rss: Option<u64>) -> bool {
        self.budget.millis > 0
            && self.budget.nodes > 0
            && self.budget.rss_mb > 0
            && self.started.elapsed() <= Duration::from_millis(self.budget.millis)
            && nodes <= self.budget.nodes
            && rss.is_some_and(|rss| rss <= self.budget.rss_mb * (1 << 20))
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
        let reason = if self.loading_phase_threads != 1
            || self.prefetch.is_some()
            || !self.repositories.cquery_events_quiescent()
        {
            Some("requires already-serial quiescent sources")
        } else if !self.warnings.is_empty() {
            Some("queued diagnostics still belong to ordinary command completion")
        } else if !request.admitted(nodes, crate::memory_limit::rss_bytes()) {
            Some("current work exceeds lookup admission budget")
        } else {
            None
        };
        if let Some(reason) = reason {
            if crate::snapshot::explain() {
                crate::snapshot::diag::line_fmt(format_args!(
                    "bazel-rs-cquery-root: declined: {reason}"
                ));
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
    #[test]
    fn admission_never_assumes_a_single_root_is_small_or_rss_is_known() {
        let request = Request::new(Budget {
            nodes: 2,
            rss_mb: 1,
            ..Budget::default()
        });
        assert!(request.admitted(2, Some(1 << 20)));
        assert!(!request.admitted(3, Some(1)));
        assert!(!request.admitted(1, Some((1 << 20) + 1)));
        assert!(!request.admitted(1, None));
        assert!(
            !Request::new(Budget {
                millis: 0,
                ..Budget::default()
            })
            .admitted(0, Some(0))
        );
    }
}
