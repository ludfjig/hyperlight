/*
Copyright 2025 The Hyperlight Authors.

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

use thiserror::Error;

use crate::wire::ParameterType;

/// Errors returned by the function dispatch machinery.
///
/// Variants carry only static, copy-friendly metadata so the error
/// type does not borrow from the wire buffer.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A parameter's wire tag did not match the type the dispatched
    /// function expected.
    #[error("Failed to convert parameter of type {0:?} to {1}")]
    ParameterValueConversionFailure(ParameterType, &'static str),

    /// A function returned a value whose wire tag did not match the
    /// type the caller expected.
    #[error("Failed to convert return value of type {0} to {1}")]
    ReturnValueConversionFailure(&'static str, &'static str),

    /// A function was invoked with the wrong arity.
    #[error("The number of arguments to the function is wrong: got {0} expected {1}")]
    UnexpectedNoOfArguments(usize, usize),
}
