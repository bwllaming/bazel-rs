//! The persistent analysis snapshot (PLAN-L2 §1). L2-W3 ships the recording
//! layer: [`record`] (`BAZEL_RS_SNAPSHOT=record`, observations of the
//! planning window through crate::observed_fs, an eligibility and taint
//! report and a text dump) and [`validate`] (the §1c re-validation, run as a
//! timed dry run over the recording). L2-W4R adds [`replay`]: the local
//! repository rules a run executed, recorded with the pinned repository
//! tables their replay needs, and replayed and compared as a dry run.
//! Nothing is saved or loaded yet: with recording on or off every command's
//! output is byte-identical.

pub mod record;
pub mod replay;
pub mod validate;
