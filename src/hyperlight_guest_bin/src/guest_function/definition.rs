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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use hyperlight_common::func::{
    BorrowingFunction, Borrows, EncodeReturn, Function, ParameterTuple, ReturnCarrier,
    SupportedReturnType,
};
use hyperlight_common::wire::{
    self, ErrorCode, FunctionCallResult, Param, ParameterType, ReturnType,
};
use hyperlight_guest::error::{HyperlightGuestError, Result};

/// The function pointer type for Rust guest functions.
///
/// A registered guest function receives the decoded wire parameters (which
/// borrow from the shared input buffer) plus a mutable slice of the shared
/// output buffer. It serializes its postcard [`FunctionCallResult::Ok`]
/// frame directly into `out` and returns the number of bytes written, so
/// no intermediate `Vec<u8>` for the reply is ever allocated.
pub type GuestFunc = fn(Vec<Param<'_>>, &mut [u8]) -> Result<usize>;

/// The definition of a function exposed from the guest to the host.
///
/// The type parameter `F` is the function pointer type. For Rust guests this
/// is [`GuestFunc`]; the C API uses its own `CGuestFunc` type.
#[derive(Debug, Clone)]
pub struct GuestFunctionDefinition<F: Copy> {
    /// The function name
    pub function_name: String,
    /// The type of the parameter values for the host function call.
    pub parameter_types: Vec<ParameterType>,
    /// The type of the return value from the host function call
    pub return_type: ReturnType,
    /// The function pointer to the guest function.
    pub function_pointer: F,
}

/// Trait for functions that can be converted to a `GuestFunc` pointer.
#[doc(hidden)]
pub trait IntoGuestFunction<Output, Args>
where
    Self: Function<Output, Args, HyperlightGuestError>,
    Self: Copy + 'static,
    Output: SupportedReturnType,
    Args: ParameterTuple,
{
    #[doc(hidden)]
    const ASSERT_ZERO_SIZED: ();

    /// Convert the function into a `GuestFunc` pointer.
    fn into_guest_function(self) -> GuestFunc;
}

/// Trait for functions that can be converted to a `GuestFunctionDefinition<GuestFunc>`
pub trait AsGuestFunctionDefinition<Output, Args>
where
    Self: Function<Output, Args, HyperlightGuestError>,
    Self: IntoGuestFunction<Output, Args>,
    Output: SupportedReturnType,
    Args: ParameterTuple,
{
    /// Get the `GuestFunctionDefinition` for this function
    fn as_guest_function_definition(
        &self,
        name: impl Into<String>,
    ) -> GuestFunctionDefinition<GuestFunc>;
}

/// Encode a guest function's owned return value into the host-bound
/// output slice. Returns the number of bytes written.
fn encode_return_into<R: SupportedReturnType>(value: R, out: &mut [u8]) -> Result<usize> {
    let owned = value.into_owned();
    let result = FunctionCallResult::Ok(owned.as_return_value());
    let written = wire::encode_into(&result, out).map_err(|e| {
        HyperlightGuestError::new(
            ErrorCode::GuestError,
            format!("Failed to encode guest function result: {e:?}"),
        )
    })?;
    Ok(written.len())
}

impl<F: Copy> GuestFunctionDefinition<F> {
    /// Create a new `GuestFunctionDefinition`.
    pub fn new(
        function_name: String,
        parameter_types: Vec<ParameterType>,
        return_type: ReturnType,
        function_pointer: F,
    ) -> Self {
        Self {
            function_name,
            parameter_types,
            return_type,
            function_pointer,
        }
    }

    /// Create a new `GuestFunctionDefinition<GuestFunc>` from a function that
    /// implements `AsGuestFunctionDefinition`.
    pub fn from_fn<Output, Args>(
        function_name: String,
        function: impl AsGuestFunctionDefinition<Output, Args>,
    ) -> GuestFunctionDefinition<GuestFunc>
    where
        Args: ParameterTuple,
        Output: SupportedReturnType,
    {
        function.as_guest_function_definition(function_name)
    }

    /// Verify that `self` has same signature as the provided `parameter_types`.
    pub fn verify_parameters(&self, parameter_types: &[ParameterType]) -> Result<()> {
        // Verify that the function does not have more than `MAX_PARAMETERS` parameters.
        const MAX_PARAMETERS: usize = 11;
        if parameter_types.len() > MAX_PARAMETERS {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestError,
                format!(
                    "Function {} has too many parameters: {} (max allowed is {}).",
                    self.function_name,
                    parameter_types.len(),
                    MAX_PARAMETERS
                ),
            ));
        }

        if self.parameter_types.len() != parameter_types.len() {
            return Err(HyperlightGuestError::new(
                ErrorCode::GuestFunctionIncorrectNoOfParameters,
                format!(
                    "Called function {} with {} parameters but it takes {}.",
                    self.function_name,
                    parameter_types.len(),
                    self.parameter_types.len()
                ),
            ));
        }

        for (i, parameter_type) in self.parameter_types.iter().enumerate() {
            if parameter_type != &parameter_types[i] {
                return Err(HyperlightGuestError::new(
                    ErrorCode::GuestFunctionParameterTypeMismatch,
                    format!(
                        "Expected parameter type {:?} for parameter index {} of function {} but got {:?}.",
                        parameter_type, i, self.function_name, parameter_types[i]
                    ),
                ));
            }
        }

        Ok(())
    }
}

// Generic blanket: any `F: Function<R, Args, HyperlightGuestError>` that
// is zero-sized can be converted to a `GuestFunc` by dispatching the
// decoded parameter vector through `Function::call_with_params` and
// encoding the result via `OwnedReturn`.
impl<F, R, Args> IntoGuestFunction<R, Args> for F
where
    F: Function<R, Args, HyperlightGuestError>,
    F: Copy + 'static,
    R: SupportedReturnType,
    Args: ParameterTuple,
{
    // Only functions that can be coerced into a function pointer (i.e., "fn" types)
    // can be registered as guest functions. We enforce that `F` is zero-sized at
    // const-eval time, and `Copy` ensures there is no `Drop` impl. The dispatcher
    // creates a fresh `F` via `mem::zeroed`.
    #[doc(hidden)]
    const ASSERT_ZERO_SIZED: () = const { assert!(core::mem::size_of::<Self>() == 0) };

    fn into_guest_function(self) -> GuestFunc {
        |params: Vec<Param<'_>>, out: &mut [u8]| {
            // SAFETY: `F` is zero-sized (enforced by ASSERT_ZERO_SIZED) and
            // has no Drop impl (enforced by the Copy bound), so creating an
            // instance from zero bytes is safe.
            let this = unsafe { core::mem::zeroed::<F>() };
            let value = Function::<R, Args, HyperlightGuestError>::call_with_params(&this, params)?;
            encode_return_into(value, out)
        }
    }
}

impl<F, Args, Output> AsGuestFunctionDefinition<Output, Args> for F
where
    F: IntoGuestFunction<Output, Args>,
    Args: ParameterTuple,
    Output: SupportedReturnType,
{
    fn as_guest_function_definition(
        &self,
        name: impl Into<String>,
    ) -> GuestFunctionDefinition<GuestFunc> {
        // Force the zero-sized assertion to fire at compile time for this F.
        let () = <Self as IntoGuestFunction<Output, Args>>::ASSERT_ZERO_SIZED;
        let parameter_types = Args::TYPE.to_vec();
        let return_type = Output::TYPE;
        let function_pointer = self.into_guest_function();

        GuestFunctionDefinition {
            function_name: name.into(),
            parameter_types,
            return_type,
            function_pointer,
        }
    }
}

/// Parallel of [`IntoGuestFunction`] for guest functions whose return
/// value borrows from the input wire buffer. `Output` is a
/// [`ReturnCarrier`] marker like [`hyperlight_common::func::BytesRef`]
/// rather than the user's return type.
#[doc(hidden)]
pub trait IntoGuestFunctionBorrowed<Output, Args>
where
    Self: BorrowingFunction<Output, Args, HyperlightGuestError>,
    Self: Copy + 'static,
    Output: ReturnCarrier + for<'a> Borrows<'a>,
    Args: ParameterTuple,
{
    #[doc(hidden)]
    const ASSERT_ZERO_SIZED: ();
    /// Convert the function into a `GuestFunc` pointer.
    fn into_guest_function_borrowed(self) -> GuestFunc;
}

/// Parallel of [`AsGuestFunctionDefinition`] for the borrowing path.
pub trait AsGuestFunctionDefinitionBorrowed<Output, Args>
where
    Self: BorrowingFunction<Output, Args, HyperlightGuestError>,
    Self: IntoGuestFunctionBorrowed<Output, Args>,
    Output: ReturnCarrier + for<'a> Borrows<'a>,
    Args: ParameterTuple,
{
    /// Get the `GuestFunctionDefinition` for this function.
    fn as_guest_function_definition_borrowed(
        &self,
        name: impl Into<String>,
    ) -> GuestFunctionDefinition<GuestFunc>;
}

impl<F, R, Args> IntoGuestFunctionBorrowed<R, Args> for F
where
    F: BorrowingFunction<R, Args, HyperlightGuestError>,
    F: Copy + 'static,
    R: ReturnCarrier + for<'a> Borrows<'a>,
    Args: ParameterTuple,
{
    #[doc(hidden)]
    const ASSERT_ZERO_SIZED: () = const { assert!(core::mem::size_of::<Self>() == 0) };

    fn into_guest_function_borrowed(self) -> GuestFunc {
        |params: Vec<Param<'_>>, out: &mut [u8]| {
            // SAFETY: `F` is zero-sized (enforced by ASSERT_ZERO_SIZED) and
            // has no Drop impl (enforced by the Copy bound), so creating an
            // instance from zero bytes is safe.
            let this = unsafe { core::mem::zeroed::<F>() };
            let value =
                <F as BorrowingFunction<R, Args, HyperlightGuestError>>::call_with_params_borrowed(
                    &this, params,
                )?;
            value.encode_into(out).map_err(|e| {
                HyperlightGuestError::new(
                    ErrorCode::GuestError,
                    format!("Failed to encode guest function result: {e:?}"),
                )
            })
        }
    }
}

impl<F, Args, Output> AsGuestFunctionDefinitionBorrowed<Output, Args> for F
where
    F: IntoGuestFunctionBorrowed<Output, Args>,
    Args: ParameterTuple,
    Output: ReturnCarrier + for<'a> Borrows<'a>,
{
    fn as_guest_function_definition_borrowed(
        &self,
        name: impl Into<String>,
    ) -> GuestFunctionDefinition<GuestFunc> {
        let () = <Self as IntoGuestFunctionBorrowed<Output, Args>>::ASSERT_ZERO_SIZED;
        let parameter_types = Args::TYPE.to_vec();
        let return_type = <Output as ReturnCarrier>::TYPE;
        let function_pointer = self.into_guest_function_borrowed();

        GuestFunctionDefinition {
            function_name: name.into(),
            parameter_types,
            return_type,
            function_pointer,
        }
    }
}
