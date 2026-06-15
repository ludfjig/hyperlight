/*
Copyright 2025  The Hyperlight Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use alloc::vec::Vec;

use super::utils::for_each_tuple;
use super::{
    Borrows, Error, ParameterTuple, ResultType, ReturnCarrier, SupportedParameterType,
    SupportedReturnType,
};
use crate::wire::Param;

/// A callable that dispatches a borrowed parameter vector to a typed
/// return.
///
/// `Args` is a carrier tuple shape (e.g. `(Str, i32)`); the actual
/// user-facing borrowed types are recovered inside each `impl` from
/// the `SupportedParameterType::Borrowed<'a>` projections.
pub trait Function<Output: SupportedReturnType, Args, E: From<Error>>:
    Send + Sync + 'static
{
    /// Dispatch the call.
    fn call_with_params<'a>(&self, params: Vec<Param<'a>>) -> Result<Output, E>;
}

macro_rules! impl_function_tuple {
    ([$N:expr] ($($p:ident: $P:ident),*)) => {
        impl<F, R, E, $($P),*> Function<R::ReturnType, ($($P,)*), E> for F
        where
            F: for<'a> Fn($(<$P as SupportedParameterType>::Borrowed<'a>),*) -> R
                + Send + Sync + 'static,
            $($P: SupportedParameterType,)*
            R: ResultType<E>,
            E: From<Error> + core::fmt::Debug,
        {
            #[allow(unused_variables, non_snake_case)]
            fn call_with_params<'a>(&self, params: Vec<Param<'a>>) -> Result<R::ReturnType, E> {
                match <[Param<'a>; $N]>::try_from(params) {
                    Ok([$($p,)*]) => {
                        $(
                            let $p = <$P as SupportedParameterType>::from_param($p)?;
                        )*
                        (self)($($p),*).into_result()
                    }
                    Err(v) => Err(Error::UnexpectedNoOfArguments(v.len(), $N).into()),
                }
            }
        }
    };
}

macro_rules! impl_function_dispatch {
    ([$N:expr] ($($t:tt)*)) => { impl_function_tuple!([$N] ($($t)*)); };
}
for_each_tuple!(impl_function_dispatch);

/// Guest-side dispatcher that calls a function whose return value may
/// borrow from the input wire buffer. Unlike [`Function`], the return
/// type is projected through the lifetime-parameterized [`Borrows`]
/// trait, so signatures like `fn(&'a [u8]) -> &'a [u8]` are
/// expressible.
///
/// `Output` is a carrier marker implementing [`ReturnCarrier`] plus
/// `for<'a> Borrows<'a>`. For owned types the carrier is the type
/// itself; for borrowed returns it is the marker (e.g. `BytesRef`)
/// whose `Borrows<'a>::Out` is `&'a [u8]`.
///
/// The fallible (`Result<_, E>`) wrapper is not supported on this
/// path. Owned-return fallible functions keep using [`Function`].
/// Helper trait that binds a closure `F` to its borrowed parameter
/// tuple and its return type at a single lifetime `'a`. This is the
/// canonical workaround for E0582 when an HRTB closure return type
/// would otherwise be a projection through a separate trait.
///
/// The dispatch trait [`BorrowingFunction`] is expressed as
/// `for<'a> F: BorrowingFnAt<'a, Args, Ret>`, which keeps `'a` in
/// trait input position.
pub trait BorrowingFnAt<'a, Args: ParameterTuple, Out> {
    /// Apply the closure to the borrowed parameter tuple.
    fn apply(&self, params: Vec<Param<'a>>) -> Result<Out, Error>;
}

macro_rules! impl_borrowing_fn_at_tuple {
    ([$N:expr] ($($p:ident: $P:ident),*)) => {
        impl<'a, F, Out $(, $P)*> BorrowingFnAt<'a, ($($P,)*), Out> for F
        where
            F: Fn($(<$P as SupportedParameterType>::Borrowed<'a>),*) -> Out,
            $($P: SupportedParameterType,)*
        {
            #[allow(unused_variables, non_snake_case)]
            fn apply(&self, params: Vec<Param<'a>>) -> Result<Out, Error> {
                match <[Param<'a>; $N]>::try_from(params) {
                    Ok([$($p,)*]) => {
                        $(
                            let $p = <$P as SupportedParameterType>::from_param($p)?;
                        )*
                        Ok((self)($($p),*))
                    }
                    Err(v) => Err(Error::UnexpectedNoOfArguments(v.len(), $N)),
                }
            }
        }
    };
}

macro_rules! impl_borrowing_fn_at_dispatch {
    ([$N:expr] ($($t:tt)*)) => { impl_borrowing_fn_at_tuple!([$N] ($($t)*)); };
}
for_each_tuple!(impl_borrowing_fn_at_dispatch);

/// Guest-side dispatcher that calls a function whose return value may
/// borrow from the input wire buffer. Unlike [`Function`], the return
/// type is projected through the lifetime-parameterized [`Borrows`]
/// trait, so signatures like `fn(&'a [u8]) -> &'a [u8]` are
/// expressible.
///
/// `Output` is a carrier marker implementing [`ReturnCarrier`] plus
/// `for<'a> Borrows<'a>`. For owned types the carrier is the type
/// itself; for borrowed returns it is the marker (e.g. `BytesRef`)
/// whose `Borrows<'a>::Out` is `&'a [u8]`.
///
/// The fallible (`Result<_, E>`) wrapper is not supported on this
/// path. Owned-return fallible functions keep using [`Function`].
pub trait BorrowingFunction<Output, Args, E>: Send + Sync + 'static
where
    Output: ReturnCarrier + for<'a> Borrows<'a>,
    Args: ParameterTuple,
    E: From<Error>,
{
    /// Dispatch the call. The returned value borrows from `params`.
    fn call_with_params_borrowed<'a>(
        &self,
        params: Vec<Param<'a>>,
    ) -> Result<<Output as Borrows<'a>>::Out, E>;
}

impl<F, Output, Args, E> BorrowingFunction<Output, Args, E> for F
where
    F: for<'a> BorrowingFnAt<'a, Args, <Output as Borrows<'a>>::Out> + Send + Sync + 'static,
    Output: ReturnCarrier + for<'a> Borrows<'a>,
    Args: ParameterTuple,
    E: From<Error> + core::fmt::Debug,
{
    fn call_with_params_borrowed<'a>(
        &self,
        params: Vec<Param<'a>>,
    ) -> Result<<Output as Borrows<'a>>::Out, E> {
        BorrowingFnAt::apply(self, params).map_err(Into::into)
    }
}
