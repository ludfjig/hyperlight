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

use std::sync::{Arc, Mutex};

use hyperlight_common::for_each_tuple;
use hyperlight_common::func::{Error as FuncError, Function, OwnedReturn, ResultType};
use hyperlight_common::wire::Param;

use super::{ParameterTuple, SupportedReturnType};
use crate::sandbox::UninitializedSandbox;
use crate::sandbox::host_funcs::FunctionEntry;
use crate::{HyperlightError, Result, new_error};

/// A sandbox on which (primitive) host functions can be registered
///
pub trait Registerable {
    /// Register a primitive host function
    fn register_host_function<Args: ParameterTuple, Output: SupportedReturnType>(
        &mut self,
        name: &str,
        hf: impl Into<HostFunction<Output, Args>>,
    ) -> Result<()>;
}
impl Registerable for UninitializedSandbox {
    fn register_host_function<Args: ParameterTuple, Output: SupportedReturnType>(
        &mut self,
        name: &str,
        hf: impl Into<HostFunction<Output, Args>>,
    ) -> Result<()> {
        let mut hfs = self
            .host_funcs
            .try_lock()
            .map_err(|e| new_error!("Error locking at {}:{}: {}", file!(), line!(), e))?;

        let entry = FunctionEntry {
            function: hf.into().into(),
            parameter_types: Args::TYPE,
            return_type: Output::TYPE,
        };

        (*hfs).register_host_function(name.to_string(), entry);
        Ok(())
    }
}

impl Registerable for crate::MultiUseSandbox {
    fn register_host_function<Args: ParameterTuple, Output: SupportedReturnType>(
        &mut self,
        name: &str,
        hf: impl Into<HostFunction<Output, Args>>,
    ) -> Result<()> {
        let mut hfs = self
            .host_funcs
            .try_lock()
            .map_err(|e| new_error!("Error locking at {}:{}: {}", file!(), line!(), e))?;

        let entry = FunctionEntry {
            function: hf.into().into(),
            parameter_types: Args::TYPE,
            return_type: Output::TYPE,
        };

        (*hfs).register_host_function(name.to_string(), entry);
        self.snapshot = None;
        Ok(())
    }
}

impl Registerable for crate::HostFunctions {
    fn register_host_function<Args: ParameterTuple, Output: SupportedReturnType>(
        &mut self,
        name: &str,
        hf: impl Into<HostFunction<Output, Args>>,
    ) -> Result<()> {
        let entry = FunctionEntry {
            function: hf.into().into(),
            parameter_types: Args::TYPE,
            return_type: Output::TYPE,
        };

        self.inner_mut()
            .register_host_function(name.to_string(), entry);
        Ok(())
    }
}

/// A typed host function. Dispatches a borrowed parameter vector to
/// the user's closure via the [`Function`] trait, then converts the
/// return value into an owned form ready for serialization.
#[derive(Clone)]
pub struct HostFunction<Output, Args>
where
    Args: ParameterTuple,
    Output: SupportedReturnType,
{
    func: Arc<dyn Function<Output, Args, HyperlightError>>,
}

/// Type-erased host function used by the registry. The closure takes
/// a borrowed parameter vector and returns an owned return value.
type ErasedHostFn = dyn for<'a> Fn(Vec<Param<'a>>) -> std::result::Result<OwnedReturn, HyperlightError>
    + Send
    + Sync
    + 'static;

pub(crate) struct TypeErasedHostFunction {
    func: Box<ErasedHostFn>,
}

impl TypeErasedHostFunction {
    pub(crate) fn call(&self, args: Vec<Param<'_>>) -> Result<OwnedReturn> {
        (self.func)(args)
    }
}

impl From<FuncError> for HyperlightError {
    fn from(e: FuncError) -> Self {
        match e {
            FuncError::ParameterValueConversionFailure(from, to) => {
                HyperlightError::ParameterValueConversionFailure(from, to)
            }
            FuncError::ReturnValueConversionFailure(from, to) => {
                HyperlightError::ReturnValueConversionFailure(from, to)
            }
            FuncError::UnexpectedNoOfArguments(got, expected) => {
                HyperlightError::UnexpectedNoOfArguments(got, expected)
            }
        }
    }
}

impl<Args, Output> From<HostFunction<Output, Args>> for TypeErasedHostFunction
where
    Args: ParameterTuple,
    Output: SupportedReturnType,
{
    fn from(func: HostFunction<Output, Args>) -> TypeErasedHostFunction {
        TypeErasedHostFunction {
            func: Box::new(move |args: Vec<Param<'_>>| {
                let r = func.func.call_with_params(args)?;
                Ok(r.into_owned())
            }),
        }
    }
}

/// Adapter that holds a `FnMut` closure behind a `Mutex` and exposes
/// it as a [`Function`] implementation. The `Marker` type parameter
/// disambiguates the tuple shape so different arities produce
/// distinct concrete types without overlapping impls.
struct TupleAdapter<F, R, E, Marker> {
    inner: Mutex<F>,
    _m: core::marker::PhantomData<fn(E, Marker) -> R>,
}

macro_rules! impl_host_function {
    ([$N:expr] ($($p:ident: $P:ident),*)) => {
        impl<F, R, $($P),*> From<F> for HostFunction<R::ReturnType, ($($P,)*)>
        where
            F: for<'a> FnMut($(<$P as super::SupportedParameterType>::Borrowed<'a>),*) -> R
                + Send + 'static,
            ($($P,)*): ParameterTuple,
            R: ResultType<HyperlightError> + 'static,
            R::ReturnType: SupportedReturnType,
            $($P: super::SupportedParameterType,)*
        {
            #[allow(non_snake_case, unused_parens)]
            fn from(func: F) -> HostFunction<R::ReturnType, ($($P,)*)> {
                HostFunction {
                    func: Arc::new(TupleAdapter::<F, R::ReturnType, HyperlightError, ($($P,)*)> {
                        inner: Mutex::new(func),
                        _m: core::marker::PhantomData,
                    }),
                }
            }
        }

        impl<F, R, E, $($P),*> Function<R::ReturnType, ($($P,)*), E>
            for TupleAdapter<F, R::ReturnType, E, ($($P,)*)>
        where
            F: for<'a> FnMut($(<$P as super::SupportedParameterType>::Borrowed<'a>),*) -> R
                + Send + 'static,
            $($P: super::SupportedParameterType,)*
            R: ResultType<E> + 'static,
            R::ReturnType: SupportedReturnType,
            E: From<FuncError> + core::fmt::Debug + 'static,
        {
            #[allow(non_snake_case, unused_parens, unused_variables, clippy::unused_unit)]
            fn call_with_params<'a>(
                &self,
                params: Vec<Param<'a>>,
            ) -> core::result::Result<R::ReturnType, E> {
                match <[Param<'a>; $N]>::try_from(params) {
                    Ok([$($p,)*]) => {
                        $(let $p = <$P as super::SupportedParameterType>::from_param($p)?;)*
                        let mut f = self.inner.lock().map_err(|_| {
                            FuncError::ParameterValueConversionFailure(
                                hyperlight_common::wire::ParameterType::Int,
                                "lock poisoned",
                            )
                        })?;
                        (f)($($p),*).into_result()
                    }
                    Err(v) => Err(FuncError::UnexpectedNoOfArguments(v.len(), $N).into()),
                }
            }
        }
    };
}

// Wrap the macro that lets us bridge user closures with arbitrary
// `FnMut(P1::Borrowed<'_>, .., Pn::Borrowed<'_>) -> R` shapes into a
// uniform `Function<R, (P1, .., Pn), HyperlightError>` adapter.
for_each_tuple!(impl_host_function);
