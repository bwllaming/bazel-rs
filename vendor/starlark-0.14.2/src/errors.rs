/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Modified: Bazel's SpellChecker is public (`bazel_suggest`, ...); see vendor
// provenance.

//! Error types used by Starlark.

pub use starlark_syntax::frame::Frame;

pub use crate::analysis::EvalMessage;
pub use crate::analysis::EvalSeverity;
pub use crate::analysis::Lint;

pub(crate) mod did_you_mean;

// Modified: Bazel's SpellChecker port, public for bazel-rs so that it has one
// spelling checker; see vendor provenance.
pub use crate::errors::did_you_mean::bazel_did_you_mean;
pub use crate::errors::did_you_mean::bazel_edit_distance;
pub use crate::errors::did_you_mean::bazel_suggest;
