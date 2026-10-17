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

// Modified for native Bazel 9.3 argument-binding error texts
// (net/starlark/java/eval/StarlarkFunction.java:385-523 for `def`,
// BuiltinFunction.java:150-160,320-490 for builtins); see vendor provenance.

use std::cmp;
use std::collections::HashMap;
use std::fmt;

use allocative::Allocative;
use dupe::Dupe;
use starlark_derive::Coerce;
use starlark_derive::Freeze;
use starlark_derive::StarlarkPagable;
use starlark_derive::Trace;
use starlark_map::Hashed;
use starlark_map::small_map::SmallMap;
use starlark_syntax::function_error;
use starlark_syntax::other_error;
use starlark_syntax::syntax::def::DefParamIndices;

use crate as starlark;
use crate::__macro_refs::coerce;
use crate::cast::transmute;
use crate::collections::symbol::map::SymbolMap;
use crate::docs::DocParam;
use crate::docs::DocParams;
use crate::docs::DocString;
use crate::errors::did_you_mean::bazel_did_you_mean;
use crate::eval::Arguments;
use crate::eval::Evaluator;
use crate::eval::ParametersParser;
use crate::eval::runtime::arguments::ArgSymbol;
use crate::eval::runtime::arguments::ArgumentsImpl;
use crate::eval::runtime::arguments::FunctionError;
use crate::eval::runtime::arguments::ResolvedArgName;
use crate::eval::runtime::params::display::PARAM_FMT_OPTIONAL;
use crate::eval::runtime::params::display::ParamFmt;
use crate::eval::runtime::params::display::fmt_param_spec;
use crate::hint::unlikely;
use crate::pagable::StarlarkPagable;
use crate::typing::ParamIsRequired;
use crate::typing::Ty;
use crate::values::Heap;
use crate::values::StringValue;
use crate::values::Value;
use crate::values::ValueLike;
use crate::values::dict::Dict;
use crate::values::dict::DictRef;

/// Describe parameter for [`ParametersSpec`].
#[derive(
    Debug, Clone, Copy, Dupe, PartialEq, Eq, PartialOrd, Ord, Trace, Freeze, Allocative
)]
pub enum ParametersSpecParam<V> {
    /// Parameter is required.
    Required,
    /// Parameter is optional (returned as `None`).
    Optional,
    /// Parameter has default value.
    Defaulted(V),
}

impl<V> ParametersSpecParam<V> {
    pub(crate) fn is_required(&self) -> ParamIsRequired {
        match self {
            ParametersSpecParam::Required => ParamIsRequired::Yes,
            ParametersSpecParam::Optional | ParametersSpecParam::Defaulted(_) => {
                ParamIsRequired::No
            }
        }
    }
}

#[derive(
    Debug,
    Copy,
    Clone,
    Dupe,
    Coerce,
    PartialEq,
    Trace,
    Freeze,
    Allocative,
    StarlarkPagable
)]
#[repr(C)]
#[starlark_pagable(bound = "V: StarlarkPagable")]
pub(crate) enum ParameterKind<V> {
    Required,
    /// When optional parameter is not supplied, there's no error,
    /// but the slot remains `None`.
    ///
    /// This is used only in native code, parameters of type `Option<T>` become `Optional`.
    Optional,
    Defaulted(V),
    Args,
    KWargs,
}

#[derive(Debug, Copy, Clone, Dupe, PartialEq, Eq, PartialOrd, Ord)]
enum CurrentParameterStyle {
    /// Parameter can be only filled positionally.
    PosOnly,
    /// Parameter can be filled positionally or by name.
    PosOrNamed,
    /// Parameter can be filled by name only.
    NamedOnly,
    /// No more args accepted.
    NoMore,
}

/// Builder for [`ParametersSpec`]
pub(crate) struct ParametersSpecBuilder<V> {
    function_name: String,
    params: Vec<(String, ParameterKind<V>)>,
    names: SymbolMap<u32>,
    /// Number of parameters that can be filled only positionally.
    positional_only: usize,
    /// Number of parameters that can be filled positionally.
    positional: usize,

    /// Has the no_args been passed
    current_style: CurrentParameterStyle,

    args: Option<usize>,
    kwargs: Option<usize>,
    /// Modified: binding errors use Bazel's `def` texts (see `ParametersSpec`).
    def_style: bool,
}

/// Define a list of parameters. This code assumes that all names are distinct and that
/// `*args`/`**kwargs` occur in well-formed locations.
// V = Value, or FrozenValue
#[derive(Debug, Clone, Trace, Freeze, Allocative, StarlarkPagable)]
#[repr(C)]
#[starlark_pagable(bound = "V: StarlarkPagable")]
pub struct ParametersSpec<V> {
    /// Only used in error messages
    function_name: String,

    /// Parameters in the order they occur.
    param_kinds: Box<[ParameterKind<V>]>,
    /// Parameter names in the order they occur.
    param_names: Box<[String]>,
    /// Mapping from name to index where the argument lives.
    #[freeze(identity)]
    #[starlark_pagable(pagable)]
    pub(crate) names: SymbolMap<u32>,
    #[freeze(identity)]
    #[starlark_pagable(pagable)]
    indices: DefParamIndices,
    /// Modified: `true` for a Starlark `def`/`lambda`, whose argument-binding
    /// errors use Bazel's `StarlarkFunction` texts; otherwise Bazel's
    /// `BuiltinFunction` texts.
    def_style: bool,
}

impl<V> ParametersSpecBuilder<V> {
    fn add(&mut self, name: &str, val: ParameterKind<V>) {
        assert!(
            !matches!(val, ParameterKind::Args | ParameterKind::KWargs),
            "adding parameter `{}` to `{}",
            name,
            self.function_name
        );

        // Regular arguments cannot follow `**kwargs`, but can follow `*args`.
        assert!(
            self.current_style < CurrentParameterStyle::NoMore,
            "adding parameter `{}` to `{}",
            name,
            self.function_name
        );
        assert!(
            self.kwargs.is_none(),
            "adding parameter `{}` to `{}",
            name,
            self.function_name
        );

        let i = self.params.len();
        self.params.push((name.to_owned(), val));
        if self.current_style != CurrentParameterStyle::PosOnly {
            let old = self.names.insert(name, i.try_into().unwrap());
            assert!(old.is_none(), "Repeated parameter `{name}`");
        }
        if self.args.is_none() && self.current_style != CurrentParameterStyle::NamedOnly {
            // If you've already seen `args` or `no_args`, you can't enter these
            // positionally
            self.positional = i + 1;
            if self.current_style == CurrentParameterStyle::PosOnly {
                self.positional_only = i + 1;
            }
        }
    }

    /// Add a required parameter. Will be an error if the caller doesn't supply
    /// it. If you want to supply a position-only argument, prepend a `$` to
    /// the name.
    pub(crate) fn required(&mut self, name: &str) {
        self.add(name, ParameterKind::Required);
    }

    /// Add an optional parameter. Will be None if the caller doesn't supply it.
    /// If you want to supply a position-only argument, prepend a `$` to the
    /// name.
    pub(crate) fn optional(&mut self, name: &str) {
        self.add(name, ParameterKind::Optional);
    }

    /// Add an optional parameter. Will be the default value if the caller
    /// doesn't supply it. If you want to supply a position-only argument,
    /// prepend a `$` to the name.
    pub(crate) fn defaulted(&mut self, name: &str, val: V) {
        self.add(name, ParameterKind::Defaulted(val));
    }

    fn param(&mut self, name: &str, param: ParametersSpecParam<V>) {
        match param {
            ParametersSpecParam::Required => self.required(name),
            ParametersSpecParam::Optional => self.optional(name),
            ParametersSpecParam::Defaulted(x) => self.defaulted(name, x),
        }
    }

    /// Add an `*args` parameter which will be an iterable sequence of parameters,
    /// recorded into a [`Vec`]. A function can only have one `args`
    /// parameter. After this call, any subsequent
    /// [`required`](ParametersSpecBuilder::required),
    /// [`optional`](ParametersSpecBuilder::optional) or
    /// [`defaulted`](ParametersSpecBuilder::defaulted)
    /// parameters can _only_ be supplied by name.
    pub(crate) fn args(&mut self) {
        assert!(
            self.args.is_none(),
            "adding *args to `{}`",
            self.function_name
        );
        assert!(
            self.current_style < CurrentParameterStyle::NamedOnly,
            "adding *args to `{}`",
            self.function_name
        );
        assert!(
            self.kwargs.is_none(),
            "adding *args to `{}`",
            self.function_name
        );
        self.params.push(("*args".to_owned(), ParameterKind::Args));
        self.args = Some(self.params.len() - 1);
        self.current_style = CurrentParameterStyle::NamedOnly;
    }

    /// Following parameters can be filled positionally or by name.
    pub(crate) fn no_more_positional_only_args(&mut self) {
        assert_eq!(
            self.current_style,
            CurrentParameterStyle::PosOnly,
            "adding / to `{}`",
            self.function_name
        );
        self.current_style = CurrentParameterStyle::PosOrNamed;
    }

    /// This function has no `*args` parameter, corresponds to the Python parameter `*`.
    /// After this call, any subsequent
    /// [`required`](ParametersSpecBuilder::required),
    /// [`optional`](ParametersSpecBuilder::optional) or
    /// [`defaulted`](ParametersSpecBuilder::defaulted)
    /// parameters can _only_ be supplied by name.
    pub(crate) fn no_more_positional_args(&mut self) {
        assert!(self.args.is_none(), "adding * to `{}`", self.function_name);
        assert!(
            self.current_style < CurrentParameterStyle::NamedOnly,
            "adding * to `{}`",
            self.function_name
        );
        assert!(
            self.kwargs.is_none(),
            "adding * to `{}`",
            self.function_name
        );
        self.current_style = CurrentParameterStyle::NamedOnly;
    }

    /// Add a `**kwargs` parameter which will be a dictionary, recorded into a [`SmallMap`].
    /// A function can only have one `kwargs` parameter.
    /// parameter. After this call, any subsequent
    /// [`required`](ParametersSpecBuilder::required),
    /// [`optional`](ParametersSpecBuilder::optional) or
    /// [`defaulted`](ParametersSpecBuilder::defaulted)
    /// parameters can _only_ be supplied by position.
    pub(crate) fn kwargs(&mut self) {
        assert!(
            self.kwargs.is_none(),
            "adding **kwargs to `{}`",
            self.function_name
        );
        self.params
            .push(("**kwargs".to_owned(), ParameterKind::KWargs));
        self.current_style = CurrentParameterStyle::NoMore;
        self.kwargs = Some(self.params.len() - 1);
    }

    /// Modified: report argument-binding errors with Bazel's `def` texts.
    pub(crate) fn def_style(&mut self) {
        self.def_style = true;
    }

    /// Construct the parameters specification.
    pub(crate) fn finish(self) -> ParametersSpec<V> {
        let ParametersSpecBuilder {
            function_name,
            positional_only,
            positional,
            args,
            current_style,
            kwargs,
            params,
            names,
            def_style,
        } = self;
        let _ = current_style;
        let positional_only: u32 = positional_only.try_into().unwrap();
        let positional: u32 = positional.try_into().unwrap();
        assert!(positional_only <= positional, "building `{function_name}`");
        let (param_names, param_kinds): (Vec<String>, Vec<ParameterKind<V>>) =
            params.into_iter().unzip();
        ParametersSpec {
            function_name,
            param_kinds: param_kinds.into_boxed_slice(),
            param_names: param_names.into_boxed_slice(),
            names,
            indices: DefParamIndices {
                num_positional_only: positional_only,
                num_positional: positional,
                args: args.map(|args| args.try_into().unwrap()),
                kwargs: kwargs.map(|kwargs| kwargs.try_into().unwrap()),
            },
            def_style,
        }
    }
}

impl<V> ParametersSpec<V> {
    /// Create a new [`ParametersSpec`] with the given function name and an advance capacity hint.
    pub(crate) fn with_capacity(
        function_name: String,
        capacity: usize,
    ) -> ParametersSpecBuilder<V> {
        ParametersSpecBuilder {
            function_name,
            params: Vec::with_capacity(capacity),
            names: SymbolMap::with_capacity(capacity),
            positional_only: 0,
            positional: 0,
            current_style: CurrentParameterStyle::PosOnly,
            args: None,
            kwargs: None,
            def_style: false,
        }
    }

    /// Create a new [`ParametersSpec`].
    pub fn new_parts<'a>(
        function_name: &str,
        pos_only: impl IntoIterator<Item = (&'a str, ParametersSpecParam<V>)>,
        pos_or_named: impl IntoIterator<Item = (&'a str, ParametersSpecParam<V>)>,
        args: bool,
        named_only: impl IntoIterator<Item = (&'a str, ParametersSpecParam<V>)>,
        kwargs: bool,
    ) -> ParametersSpec<V> {
        let pos_only = pos_only.into_iter();
        let pos_or_named = pos_or_named.into_iter();
        let named_only = named_only.into_iter();

        let mut builder = ParametersSpec::with_capacity(
            function_name.to_owned(),
            pos_only.size_hint().0
                + pos_or_named.size_hint().0
                + args as usize
                + named_only.size_hint().0
                + kwargs as usize,
        );

        for (name, val) in pos_only {
            builder.param(name, val);
        }
        builder.no_more_positional_only_args();
        for (name, val) in pos_or_named {
            builder.param(name, val);
        }
        if args {
            builder.args();
        } else {
            builder.no_more_positional_args();
        }
        for (name, val) in named_only {
            builder.param(name, val);
        }
        if kwargs {
            builder.kwargs();
        }
        builder.finish()
    }

    /// Parameter parse with only named parameters.
    pub fn new_named_only<'a>(
        function_name: &str,
        named_only: impl IntoIterator<Item = (&'a str, ParametersSpecParam<V>)>,
    ) -> ParametersSpec<V> {
        Self::new_parts(
            function_name,
            std::iter::empty(),
            std::iter::empty(),
            false,
            named_only,
            false,
        )
    }

    /// Produce an approximate signature for the function, combining the name and arguments.
    pub fn signature(&self) -> String {
        let mut collector = String::new();
        self.collect_signature(&mut collector);
        collector
    }

    // Generate a good error message for it
    pub(crate) fn collect_signature(&self, collector: &mut String) {
        collector.push_str(&self.function_name);

        // We used to make the "name" of a function include all its parameters, but that is a lot of
        // details and visually crowds out everything else. Try disabling, although we might want it
        // in some contexts, so don't delete it.
    }

    /// Function parameter as they would appear in `def`
    /// (excluding types, default values and formatting).
    pub fn parameters_str(&self) -> String {
        #[cold]
        fn err(args: fmt::Arguments) -> String {
            if cfg!(test) {
                panic!("{}", args);
            }
            format!("<{args}>")
        }

        if let Some(args) = self.indices.args {
            if args != self.indices.num_positional {
                return err(format_args!(
                    "Inconsistent *args: {:?}, args={}, positional={}",
                    self.function_name, args, self.indices.num_positional
                ));
            }
        }
        if let Some(kwargs) = self.indices.kwargs {
            if kwargs as usize + 1 != self.param_kinds.len() {
                return err(format_args!(
                    "Inconsistent **kwargs: {:?}, kwargs={}, param_kinds.len()={}",
                    self.function_name,
                    kwargs,
                    self.param_kinds.len()
                ));
            }
        }

        let pf = |i: usize| {
            let name = self.param_names[i].as_str();
            let name = name.strip_prefix("**").unwrap_or(name);
            let name = name.strip_prefix("*").unwrap_or(name);
            ParamFmt {
                name,
                ty: None::<&str>,
                default: match self.param_kinds[i] {
                    ParameterKind::Defaulted(_) | ParameterKind::Optional => {
                        Some(PARAM_FMT_OPTIONAL)
                    }
                    ParameterKind::Required | ParameterKind::Args | ParameterKind::KWargs => None,
                },
            }
        };

        let mut s = String::new();
        fmt_param_spec(
            &mut s,
            self.indices.pos_only().map(pf),
            self.indices.pos_or_named().map(pf),
            self.indices.args.map(|a| a as usize).map(pf),
            self.indices.named_only(self.param_kinds.len()).map(pf),
            self.indices.kwargs.map(|a| a as usize).map(pf),
        )
        .unwrap();
        s
    }

    pub(crate) fn resolve_name(&self, name: Hashed<&str>) -> ResolvedArgName {
        let hash = name.hash();
        let param_index = self.names.get_hashed_str(name).copied();
        ResolvedArgName { hash, param_index }
    }

    pub(crate) fn has_args_or_kwargs(&self) -> bool {
        self.indices.args.is_some() || self.indices.kwargs.is_some()
    }

    /// Generate documentation for each of the parameters, using a custom formatter for default values.
    pub fn documentation_with_default_value_formatter<F>(
        &self,
        parameter_types: Vec<Ty>,
        mut parameter_docs: HashMap<String, Option<DocString>>,
        formatter: F,
    ) -> DocParams
    where
        F: Fn(&V) -> String,
    {
        assert_eq!(
            self.param_kinds.len(),
            parameter_types.len(),
            "function: `{}`",
            self.function_name,
        );

        let mut dp = |i: usize| -> DocParam {
            let name = self.param_names[i].as_str();
            let name = name.strip_prefix("**").unwrap_or(name);
            let name = name.strip_prefix("*").unwrap_or(name);

            let docs = parameter_docs.remove(name).flatten();

            let name = name.to_owned();

            DocParam {
                name,
                docs,
                typ: parameter_types[i].dupe(),
                default_value: match &self.param_kinds[i] {
                    ParameterKind::Required => None,
                    ParameterKind::Optional => Some(PARAM_FMT_OPTIONAL.to_owned()),
                    ParameterKind::Defaulted(v) => Some(formatter(v)),
                    ParameterKind::Args => None,
                    ParameterKind::KWargs => None,
                },
            }
        };

        DocParams {
            pos_only: self.indices.pos_only().map(&mut dp).collect(),
            pos_or_named: self.indices.pos_or_named().map(&mut dp).collect(),
            args: self.indices.args.map(|a| a as usize).map(&mut dp),
            named_only: self
                .indices
                .named_only(self.param_kinds.len())
                .map(&mut dp)
                .collect(),
            kwargs: self.indices.kwargs.map(|a| a as usize).map(&mut dp),
        }
    }
}

impl<'v, V: ValueLike<'v>> ParametersSpec<V> {
    pub(crate) fn as_value(&self) -> &ParametersSpec<Value<'v>> {
        // Everything is `repr(C)` and `Value` and `FrozenValue` have the same layout.
        unsafe { transmute!(&ParametersSpec<V>, &ParametersSpec<Value>, self) }
    }

    /// Number of function parameters.
    pub fn len(&self) -> usize {
        self.param_kinds.len()
    }
}

impl<'v> ParametersSpec<Value<'v>> {
    /// Move parameters from [`Arguments`] to a list of [`Value`],
    /// using the supplied [`ParametersSpec`].
    #[inline]
    fn collect_impl(
        &self,
        args: &Arguments<'v, '_>,
        slots: &mut [Option<Value<'v>>],
        heap: Heap<'v>,
    ) -> crate::Result<()> {
        self.collect_inline(&args.0, slots, heap)
    }

    /// Collect `N` arguments.
    ///
    /// This function is called by generated code.
    #[inline]
    fn collect_into_impl<const N: usize>(
        &self,
        args: &Arguments<'v, '_>,
        heap: Heap<'v>,
    ) -> crate::Result<[Option<Value<'v>>; N]> {
        let mut slots = [(); N].map(|_| None);
        self.collect(args, &mut slots, heap)?;
        Ok(slots)
    }

    /// A variant of `collect` that is always inlined
    /// for Def and NativeFunction that are hot-spots
    #[inline(always)]
    fn collect_inline_impl<'a, A: ArgumentsImpl<'v, 'a>>(
        &self,
        args: &A,
        slots: &mut [Option<Value<'v>>],
        heap: Heap<'v>,
    ) -> crate::Result<()>
    where
        'v: 'a,
    {
        // If the arguments equal the length and the kinds, and we don't have any other args,
        // then no_args, *args and **kwargs must all be unset,
        // and we don't have to crate args/kwargs objects, we can skip everything else
        if args.pos().len() == (self.indices.num_positional as usize)
            && args.pos().len() == self.param_kinds.len()
            && args.named().is_empty()
            && args.args().is_none()
            && args.kwargs().is_none()
        {
            for (v, s) in args.pos().iter().zip(slots.iter_mut()) {
                *s = Some(*v);
            }

            return Ok(());
        }

        self.collect_slow(args, slots, heap)
    }

    fn collect_slow<'a, A: ArgumentsImpl<'v, 'a>>(
        &self,
        args: &A,
        slots: &mut [Option<Value<'v>>],
        heap: Heap<'v>,
    ) -> crate::Result<()>
    where
        'v: 'a,
    {
        /// Lazily initialized `kwargs` object.
        #[derive(Default)]
        struct LazyKwargs<'v> {
            kwargs: Option<SmallMap<StringValue<'v>, Value<'v>>>,
        }

        impl<'v> LazyKwargs<'v> {
            // Return true if the value is a duplicate
            #[inline(always)]
            fn insert(&mut self, key: Hashed<StringValue<'v>>, val: Value<'v>) -> bool {
                match &mut self.kwargs {
                    None => {
                        let mut mp = SmallMap::with_capacity(12);
                        mp.insert_hashed_unique_unchecked(key, val);
                        self.kwargs = Some(mp);
                        false
                    }
                    Some(mp) => mp.insert_hashed(key, val).is_some(),
                }
            }

            #[inline(always)]
            fn insert_unique_unchecked(&mut self, key: Hashed<StringValue<'v>>, val: Value<'v>) {
                match &mut self.kwargs {
                    None => {
                        let mut mp = SmallMap::with_capacity(12);
                        mp.insert_hashed_unique_unchecked(key, val);
                        self.kwargs = Some(mp);
                    }
                    Some(mp) => {
                        mp.insert_hashed_unique_unchecked(key, val);
                    }
                }
            }

            fn alloc(self, heap: Heap<'v>) -> Value<'v> {
                let kwargs = match self.kwargs {
                    Some(kwargs) => Dict::new(coerce(kwargs)),
                    None => Dict::default(),
                };
                heap.alloc(kwargs)
            }
        }

        let len = self.param_kinds.len();
        // We might do unchecked stuff later on, so make sure we have as many slots as we expect
        assert!(slots.len() >= len);

        let mut star_args = Vec::new();
        let mut kwargs = LazyKwargs::default();
        let mut next_position = 0;

        // First deal with positional parameters
        if args.pos().len() <= (self.indices.num_positional as usize) {
            // fast path for when we don't need to bounce down to filling in args
            for (v, s) in args.pos().iter().zip(slots.iter_mut()) {
                *s = Some(*v);
            }
            next_position = args.pos().len();
        } else {
            for v in args.pos() {
                if next_position < (self.indices.num_positional as usize) {
                    slots[next_position] = Some(*v);
                    next_position += 1;
                } else {
                    star_args.push(*v);
                }
            }
        }

        // Next deal with named parameters
        // The lowest position at which we've written a name.
        // If at the end lowest_name is less than next_position, we got the same variable twice.
        // So no duplicate checking until after all positional arguments
        let mut lowest_name = usize::MAX;
        // Avoid a lot of loop setup etc in the common case
        if !args.names().names().is_empty() {
            for ((name, name_value), v) in args.names().names().iter().zip(args.named()) {
                // Safe to use new_unchecked because hash for the Value and str are the same
                match name.get_index_from_param_spec(self) {
                    None => {
                        kwargs.insert_unique_unchecked(
                            Hashed::new_unchecked(name.small_hash(), *name_value),
                            *v,
                        );
                    }
                    Some(i) => {
                        slots[i] = Some(*v);
                        lowest_name = cmp::min(lowest_name, i);
                    }
                }
            }
        }

        // Next up are the *args parameters
        if let Some(param_args) = args.args() {
            for v in param_args.iterate(heap).map_err(|_| {
                FunctionError::ArgsArrayIsNotIterable(param_args.get_type().to_owned())
            })? {
                if next_position < (self.indices.num_positional as usize) {
                    slots[next_position] = Some(v);
                    next_position += 1;
                } else {
                    star_args.push(v);
                }
            }
        }

        // Check if the named arguments clashed with the positional arguments.
        // Modified: only the explicit positional arguments clash; Bazel binds
        // `*args` elements after the named arguments and lets them overwrite a
        // parameter already given by name (StarlarkFunction.java:460-472,
        // BuiltinFunction.java:307-319, Eval.java:716-741).
        if unlikely(cmp::min(args.pos().len(), self.indices.num_positional as usize) > lowest_name)
        {
            return Err(self.bazel_binding_error(args, heap));
        }

        // Now insert the kwargs, if there are any
        if let Some(param_kwargs) = args.kwargs() {
            match DictRef::from_value(param_kwargs) {
                Some(y) => {
                    for (k, v) in y.iter_hashed() {
                        match StringValue::new(*k.key()) {
                            None => {
                                return Err(FunctionError::ArgsValueIsNotString(
                                    k.key().get_type().to_owned(),
                                )
                                .into());
                            }
                            Some(s) => {
                                let repeat = match self
                                    .names
                                    .get_hashed_string_value(Hashed::new_unchecked(k.hash(), s))
                                {
                                    None => kwargs.insert(Hashed::new_unchecked(k.hash(), s), v),
                                    Some(i) => {
                                        let this_slot = &mut slots[*i as usize];
                                        let repeat = this_slot.is_some();
                                        *this_slot = Some(v);
                                        repeat
                                    }
                                };
                                if unlikely(repeat) {
                                    return Err(self.bazel_binding_error(args, heap));
                                }
                            }
                        }
                    }
                }
                None => {
                    return Err(
                        FunctionError::KwArgsIsNotDict(param_kwargs.get_type().to_owned()).into(),
                    );
                }
            }
        }

        // Modified: Bazel reports surplus positional or unexpected named
        // arguments before missing ones (StarlarkFunction.java:507-523,
        // BuiltinFunction.java:320-430).
        if unlikely(
            (self.indices.args.is_none() && !star_args.is_empty())
                || (self.indices.kwargs.is_none() && kwargs.kwargs.is_some()),
        ) {
            return Err(self.bazel_binding_error(args, heap));
        }

        // We have moved parameters into all the relevant slots, so need to finalise things.
        // We need to set default values and error if any required values are missing
        let kinds = &*self.param_kinds;
        // This code is very hot, and setting up iterators was a noticeable bottleneck.
        for index in next_position..kinds.len() {
            // The number of locals must be at least the number of parameters, see `collect`
            // which reserves `max(_, kinds.len())`.
            let slot = unsafe { slots.get_unchecked_mut(index) };
            let def = unsafe { kinds.get_unchecked(index) };

            // We know that up to next_position got filled positionally, so we don't need to check those
            if slot.is_some() {
                continue;
            }
            match def {
                ParameterKind::Required => {
                    // Modified: Bazel lists every missing parameter.
                    return Err(self.bazel_binding_error(args, heap));
                }
                ParameterKind::Defaulted(x) => {
                    *slot = Some(x.to_value());
                }
                _ => {}
            }
        }

        // Now set the kwargs/args slots, if they are requested, and fail it they are absent but used
        // Note that we deliberately give warnings about missing parameters _before_ giving warnings
        // about unexpected extra parameters, so if a user misspells an argument they get a better error.
        // Surplus positional and unexpected named arguments were rejected above.
        if let Some(args_pos) = self.indices.args {
            slots[args_pos as usize] = Some(heap.alloc_tuple(&star_args));
        }

        if let Some(kwargs_pos) = self.indices.kwargs {
            slots[kwargs_pos as usize] = Some(kwargs.alloc(heap));
        }
        Ok(())
    }

    /// Modified: the Bazel 9.3 argument-binding error for a call that cannot
    /// bind. Bazel's call evaluation (Eval.java:716-756) adds the explicit
    /// positional arguments, the explicit named ones, the `*args` elements
    /// and then the `**kwargs` entries; its argument processor then checks the
    /// positional count, unexpected names and missing parameters. This replays
    /// that order. Only called on the (cold) error path; a builtin replays it
    /// again with its parameter types (`bazel_rebind_error`).
    #[cold]
    #[inline(never)]
    fn bazel_binding_error<'a, A: ArgumentsImpl<'v, 'a>>(
        &self,
        args: &A,
        heap: Heap<'v>,
    ) -> crate::Error
    where
        'v: 'a,
    {
        let star: Vec<Value<'v>> = match args.args() {
            None => Vec::new(),
            Some(star) => match star.iterate(heap) {
                Ok(it) => it.collect(),
                Err(_) => {
                    return FunctionError::ArgsArrayIsNotIterable(star.get_type().to_owned())
                        .into();
                }
            },
        };
        let mut named: Vec<(StringValue<'v>, Value<'v>)> = args
            .names()
            .names()
            .iter()
            .map(|(_, name)| *name)
            .zip(args.named().iter().copied())
            .collect();
        let num_explicit_named = named.len();
        if let Some(param_kwargs) = args.kwargs() {
            let Some(dict) = DictRef::from_value(param_kwargs) else {
                return FunctionError::KwArgsIsNotDict(param_kwargs.get_type().to_owned()).into();
            };
            for (k, v) in dict.iter() {
                match StringValue::new(k) {
                    Some(k) => named.push((k, v)),
                    None => {
                        return FunctionError::ArgsValueIsNotString(k.get_type().to_owned()).into();
                    }
                }
            }
        }
        let call = BazelCall {
            explicit_positional: args.pos(),
            star: &star,
            named: &named,
            num_explicit_named,
        };
        let message = BazelBinder::new(self, &[])
            .bind(&call)
            .unwrap_or_else(|| format!("{}() got invalid arguments", self.bazel_name()));
        FunctionError::BazelBinding(message).into()
    }

    /// Modified: the Bazel binding error for `args`, if they cannot bind to
    /// this signature (checking each argument against `param_tys`, the
    /// builtin's parameter types, as Bazel's argument processor does); `None`
    /// if they can (or `*args`/`**kwargs` are invalid).
    #[cold]
    #[inline(never)]
    fn bazel_rebind_error_impl(
        &self,
        args: &Arguments<'v, '_>,
        param_tys: &[Ty],
        heap: Heap<'v>,
    ) -> Option<crate::Error> {
        let star: Vec<Value<'v>> = match args.0.args {
            Some(star) => star.iterate(heap).ok()?.collect(),
            None => Vec::new(),
        };
        let mut named: Vec<(StringValue<'v>, Value<'v>)> = args
            .0
            .names
            .names()
            .iter()
            .map(|(_, name)| *name)
            .zip(args.0.named.iter().copied())
            .collect();
        let num_explicit_named = named.len();
        if let Some(param_kwargs) = args.0.kwargs {
            for (k, v) in DictRef::from_value(param_kwargs)?.iter() {
                named.push((StringValue::new(k)?, v));
            }
        }
        let call = BazelCall {
            explicit_positional: args.0.pos,
            star: &star,
            named: &named,
            num_explicit_named,
        };
        BazelBinder::new(self, param_tys)
            .bind(&call)
            .map(|message| function_error!("{message}"))
    }

    /// The name Bazel uses in binding errors: a `def`'s own name (its
    /// `function_name` is `file.name`), or the builtin method name.
    fn bazel_name(&self) -> &str {
        if self.def_style {
            match self.function_name.rsplit_once('.') {
                Some((_, name)) => name,
                None => &self.function_name,
            }
        } else {
            &self.function_name
        }
    }

    /// Indices of the parameters that are neither `*args` nor `**kwargs`.
    fn bazel_ordinary_params(&self) -> impl Iterator<Item = usize> + '_ {
        self.param_kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| !matches!(kind, ParameterKind::Args | ParameterKind::KWargs))
            .map(|(index, _)| index)
    }
}

/// Modified: a call in Bazel's argument order (see
/// `ParametersSpec::bazel_binding_error`).
struct BazelCall<'a, 'v> {
    explicit_positional: &'a [Value<'v>],
    star: &'a [Value<'v>],
    /// Explicit named arguments, then the `**kwargs` entries.
    named: &'a [(StringValue<'v>, Value<'v>)],
    num_explicit_named: usize,
}

/// Modified: a replay of Bazel's argument processors over a call:
/// `StarlarkFunction.ArgumentProcessor` (StarlarkFunction.java:385-523) for a
/// `def`, `BuiltinFunction.ArgumentProcessor` (BuiltinFunction.java:307-430,
/// 470-490) for a builtin, which checks each argument's type as it is added
/// (`checkParamValue`, :400-412), before the positional count.
struct BazelBinder<'s, 'v> {
    spec: &'s ParametersSpec<Value<'v>>,
    name: &'s str,
    /// The builtin's parameter types, aligned with `spec.param_kinds`, or
    /// empty (no type checks; always for a `def`).
    param_tys: &'s [Ty],
    filled: Vec<bool>,
    /// Positional arguments added so far (Bazel's `numNonSurplusPositionalArgs`
    /// / `argIndex + unexpectedPositionalArgCount`).
    num_positional: usize,
    residual: Vec<&'s str>,
    unexpected: Vec<&'s str>,
}

impl<'s, 'v> BazelBinder<'s, 'v> {
    fn new(spec: &'s ParametersSpec<Value<'v>>, param_tys: &'s [Ty]) -> Self {
        let param_tys = if !spec.def_style && param_tys.len() == spec.param_kinds.len() {
            param_tys
        } else {
            &[]
        };
        BazelBinder {
            spec,
            name: spec.bazel_name(),
            param_tys,
            filled: vec![false; spec.param_kinds.len()],
            num_positional: 0,
            residual: Vec::new(),
            unexpected: Vec::new(),
        }
    }

    fn plural(n: usize) -> &'static str {
        if n == 1 { "" } else { "s" }
    }

    fn multiple_values(&self, key: &str) -> String {
        if self.spec.def_style {
            format!("{}() got multiple values for parameter '{key}'", self.name)
        } else {
            format!("{}() got multiple values for argument '{key}'", self.name)
        }
    }

    /// `checkParamValue` for parameter `index` (builtins only).
    fn check_type(&self, index: usize, value: Value<'v>) -> Result<(), String> {
        match self.param_tys.get(index) {
            Some(ty) => match crate::values::error::bazel_param_type_mismatch(
                self.name,
                &self.spec.param_names[index],
                ty,
                value,
            ) {
                Some(error) => Err(error),
                None => Ok(()),
            },
            None => Ok(()),
        }
    }

    fn bind(&mut self, call: &BazelCall<'s, 'v>) -> Option<String> {
        let result = (|| {
            for value in call.explicit_positional {
                self.positional(*value)?;
            }
            for (key, value) in &call.named[..call.num_explicit_named] {
                self.named(key.as_str(), *value)?;
            }
            for value in call.star {
                self.positional(*value)?;
            }
            for (key, value) in &call.named[call.num_explicit_named..] {
                self.named(key.as_str(), *value)?;
            }
            Ok(())
        })();
        match result {
            Err(error) => Some(error),
            Ok(()) => self.finish(),
        }
    }

    /// `addPositionalArg`: fills the next positional parameter, overwriting a
    /// value already bound by name without a duplicate check (Bazel binds
    /// `*args` elements after the named arguments).
    fn positional(&mut self, value: Value<'v>) -> Result<(), String> {
        let index = self.num_positional;
        if index < self.spec.indices.num_positional as usize {
            self.check_type(index, value)?;
            self.filled[index] = true;
            self.num_positional += 1;
        } else if self.spec.indices.args.is_none() {
            self.num_positional += 1;
        }
        Ok(())
    }

    /// `addNamedArg`: `Err(error)` for an error Bazel raises immediately.
    fn named(&mut self, key: &'s str, value: Value<'v>) -> Result<(), String> {
        let spec = self.spec;
        let error = if let Some(index) = spec.names.get_str(key) {
            let index = *index as usize;
            self.check_type(index, value)?;
            if !self.filled[index] {
                self.filled[index] = true;
                return Ok(());
            }
            self.multiple_values(key)
        } else if spec.indices.kwargs.is_some() {
            if !self.residual.contains(&key) {
                self.residual.push(key);
                return Ok(());
            }
            if spec.def_style {
                self.multiple_values(key)
            } else {
                format!(
                    "{}() got multiple values for keyword argument '{key}'",
                    self.name
                )
            }
        } else if spec.def_style {
            // Reported by `call()` after the positional count.
            self.unexpected.push(key);
            return Ok(());
        } else if spec.param_names[..spec.indices.num_positional_only as usize]
            .iter()
            .any(|param| param == key)
        {
            format!(
                "{}() got named argument for positional-only parameter '{key}'",
                self.name
            )
        } else {
            format!(
                "{}() got unexpected keyword argument '{key}'{}",
                self.name,
                bazel_did_you_mean(
                    key,
                    spec.bazel_ordinary_params()
                        .map(|index| spec.param_names[index].as_str())
                )
            )
        };
        Err(error)
    }

    /// `call()`: the positional count, then (`def`) the unexpected names,
    /// then the missing parameters.
    fn finish(&self) -> Option<String> {
        let spec = self.spec;
        let name = self.name;
        let max = spec.indices.num_positional as usize;
        if spec.indices.args.is_none() && self.num_positional > max {
            let got = self.num_positional;
            return Some(if max > 0 {
                format!(
                    "{name}() accepts no more than {max} positional argument{} but got {got}",
                    Self::plural(max)
                )
            } else if spec.def_style {
                format!("{name}() does not accept positional arguments, but got {got}")
            } else {
                format!("{name}() got unexpected positional argument")
            });
        }
        if !self.unexpected.is_empty() {
            // A spelling hint only for exactly one (StarlarkFunction.java:385-397),
            // over the parameters a name can bind (Bazel defs have no
            // positional-only parameters).
            let hint = match self.unexpected.as_slice() {
                [only] => bazel_did_you_mean(
                    only,
                    spec.bazel_ordinary_params()
                        .filter(|index| *index >= spec.indices.num_positional_only as usize)
                        .map(|index| spec.param_names[index].as_str()),
                ),
                _ => String::new(),
            };
            return Some(format!(
                "{name}() got unexpected keyword argument{}: {}{hint}",
                Self::plural(self.unexpected.len()),
                self.unexpected.join(", ")
            ));
        }
        let mut positional = Vec::new();
        let mut keyword = Vec::new();
        for index in spec.bazel_ordinary_params() {
            if self.filled[index] || !matches!(spec.param_kinds[index], ParameterKind::Required) {
                continue;
            }
            if index < max {
                positional.push(spec.param_names[index].as_str());
            } else {
                keyword.push(spec.param_names[index].as_str());
            }
        }
        let (kind, missing) = if !positional.is_empty() {
            ("positional", positional)
        } else if !keyword.is_empty() {
            let kind = if spec.def_style {
                "keyword-only"
            } else {
                "named"
            };
            (kind, keyword)
        } else {
            return None;
        };
        Some(format!(
            "{name}() missing {} required {kind} argument{}: {}",
            missing.len(),
            Self::plural(missing.len()),
            missing.join(", ")
        ))
    }
}

impl<'v> ParametersSpec<Value<'v>> {
    /// Check if current parameters can be filled with given arguments signature.
    #[allow(clippy::needless_range_loop)]
    fn can_fill_with_args_impl(&self, pos: usize, names: &[&str]) -> bool {
        let mut filled = vec![false; self.param_kinds.len()];
        for p in 0..pos {
            if p < (self.indices.num_positional as usize) {
                filled[p] = true;
            } else if self.indices.args.is_some() {
                // Filled into `*args`.
            } else {
                return false;
            }
        }
        if pos > (self.indices.num_positional as usize) && self.indices.args.is_none() {
            return false;
        }
        for name in names {
            match self.names.get_str(name) {
                Some(i) => {
                    if filled[*i as usize] {
                        // Duplicate argument.
                        return false;
                    }
                    filled[*i as usize] = true;
                }
                None => {
                    if self.indices.kwargs.is_none() {
                        return false;
                    }
                }
            }
        }
        for (filled, p) in filled.iter().zip(self.param_kinds.iter()) {
            if *filled {
                continue;
            }
            match p {
                ParameterKind::Args => {}
                ParameterKind::KWargs => {}
                ParameterKind::Defaulted(_) => {}
                ParameterKind::Optional => {}
                ParameterKind::Required => return false,
            }
        }
        true
    }

    /// Create a [`ParametersParser`] for given arguments.
    #[inline]
    fn parser_impl<R, F>(
        &self,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
        k: F,
    ) -> crate::Result<R>
    where
        F: FnOnce(&mut ParametersParser<'v, '_>, &mut Evaluator<'v, '_, '_>) -> crate::Result<R>,
    {
        eval.alloca_init(
            self.len(),
            || None,
            |slots, eval| {
                self.collect_inline(&args.0, slots, eval.heap())?;
                let mut parser = ParametersParser::new(slots, &self.param_names);
                let r = k(&mut parser, eval)?;
                if !parser.is_eof() {
                    return Err(other_error!(
                        "Parser for `{}` did not consume all arguments",
                        self.function_name
                    ));
                }
                Ok(r)
            },
        )
    }
}

impl<'v, V: ValueLike<'v>> ParametersSpec<V> {
    /// Collect `N` arguments.
    ///
    /// This function is called by generated code.
    #[inline]
    pub fn collect_into<const N: usize>(
        &self,
        args: &Arguments<'v, '_>,
        heap: Heap<'v>,
    ) -> crate::Result<[Option<Value<'v>>; N]> {
        self.as_value().collect_into_impl(args, heap)
    }

    /// Move parameters from [`Arguments`] to a list of [`Value`],
    /// using the supplied [`ParametersSpec`].
    #[inline]
    pub fn collect(
        &self,
        args: &Arguments<'v, '_>,
        slots: &mut [Option<Value<'v>>],
        heap: Heap<'v>,
    ) -> crate::Result<()> {
        self.as_value().collect_impl(args, slots, heap)
    }

    /// Generate documentation for each of the parameters.
    ///
    /// # Arguments
    /// * `parameter_types` should be a mapping of parameter index to type
    /// * `parameter_docs` should be a mapping of parameter name to possible documentation for
    ///                    that parameter
    #[inline]
    pub fn documentation(
        &self,
        parameter_types: Vec<Ty>,
        parameter_docs: HashMap<String, Option<DocString>>,
    ) -> DocParams {
        self.as_value().documentation_with_default_value_formatter(
            parameter_types,
            parameter_docs,
            |v| v.to_value().to_repr(),
        )
    }

    /// Create a [`ParametersParser`] for given arguments.
    #[inline]
    pub fn parser<R, F>(
        &self,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
        k: F,
    ) -> crate::Result<R>
    where
        F: FnOnce(&mut ParametersParser<'v, '_>, &mut Evaluator<'v, '_, '_>) -> crate::Result<R>,
    {
        self.as_value().parser_impl(args, eval, k)
    }

    /// A variant of `collect` that is always inlined
    /// for Def and NativeFunction that are hot-spots
    #[inline(always)]
    pub(crate) fn collect_inline<'a, A: ArgumentsImpl<'v, 'a>>(
        &self,
        args: &A,
        slots: &mut [Option<Value<'v>>],
        heap: Heap<'v>,
    ) -> crate::Result<()>
    where
        'v: 'a,
    {
        self.as_value().collect_inline_impl(args, slots, heap)
    }

    /// Check if current parameters can be filled with given arguments signature.
    pub fn can_fill_with_args(&self, pos: usize, names: &[&str]) -> bool {
        self.as_value().can_fill_with_args_impl(pos, names)
    }

    /// Modified: replace a generic positional-binding error (`Wrong number of
    /// positional arguments`, `extra named parameter(s) for call to function`)
    /// of a builtin with this spec with Bazel's text, when `args` indeed
    /// cannot bind to it, and a parameter type error with Bazel's `in call to`
    /// text. A binding error is replayed with the builtin's parameter types
    /// `param_tys` (Bazel checks each argument's type while binding). Any
    /// other error is returned unchanged.
    #[cold]
    #[inline(never)]
    pub(crate) fn bazel_rebind_error(
        &self,
        error: crate::Error,
        args: &Arguments<'v, '_>,
        param_tys: &[Ty],
        heap: Heap<'v>,
    ) -> crate::Error {
        let generic = match error.kind() {
            crate::ErrorKind::Function(e) => matches!(
                e.downcast_ref::<FunctionError>(),
                Some(FunctionError::WrongNumberOfArgs { .. })
                    | Some(FunctionError::ExtraNamedArg { .. })
                    | Some(FunctionError::BazelBinding(_))
            ),
            // A parameter of this builtin with a value of the wrong type
            // (`UnpackValue::unpack_named_param` in generated code).
            crate::ErrorKind::Value(e) => {
                if let Some(message) = e
                    .downcast_ref::<crate::values::error::IncorrectParameterTypeNamedWithExpected>()
                    .and_then(|e| e.bazel_message(self.as_value().bazel_name()))
                {
                    return function_error!("{message}");
                }
                false
            }
            _ => false,
        };
        if !generic {
            return error;
        }
        self.as_value()
            .bazel_rebind_error_impl(args, param_tys, heap)
            .unwrap_or(error)
    }
}
