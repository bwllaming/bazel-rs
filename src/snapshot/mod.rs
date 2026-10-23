//! The persistent analysis snapshot (PLAN-L2 §1). L2-W3 ships the recording
//! layer: [`record`] (`BAZEL_RS_SNAPSHOT=record`, observations of the
//! planning window through crate::observed_fs, an eligibility and taint
//! report and a text dump) and [`validate`] (the §1c re-validation, run as a
//! timed dry run over the recording). L2-W4R adds [`replay`]: the local
//! repository rules a run executed, recorded with the pinned repository
//! tables their replay needs, and replayed and compared as a dry run.
//! L2-W4 adds [`codec`]: the binary encoding of the planned request
//! (crate::request) a snapshot saves, and the full-analysis hand-over that
//! round-trips every request through it under
//! `BAZEL_RS_SNAPSHOT_ROUNDTRIP=1`. Nothing is saved or loaded yet: with
//! recording on or off, and with or without the round trip, every command's
//! output is byte-identical.

pub mod codec;
pub mod record;
pub mod replay;
pub mod validate;
