//! The persistent analysis snapshot (PLAN-L2 §1). L2-W3 ships the recording
//! layer only: [`record`] (`BAZEL_RS_SNAPSHOT=record`, observations of the
//! planning window through crate::observed_fs, an eligibility and taint
//! report and a text dump) and [`validate`] (the §1c re-validation, run as a
//! timed dry run over the recording). Nothing is saved or loaded yet: with
//! recording on or off every command's output is byte-identical.

pub mod record;
pub mod validate;
