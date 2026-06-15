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

/// Definitions and functionality to enable guest-to-host function calling,
/// also called "host functions"
///
/// This module includes functionality to do the following
///
/// - Define several prototypes for what a host function must look like,
///   including the number of arguments (arity) they can have, supported argument
///   types, and supported return types
/// - Registering host functions to be callable by the guest
/// - Dynamically dispatching a call from the guest to the appropriate
///   host function
pub(crate) mod host_functions;

/// Re-export for `HostFunction` trait
pub use host_functions::{HostFunction, Registerable};
pub use hyperlight_common::func::{
    Bytes, IntoParam, IntoParameters, OwnedReturn, ParameterTuple, ResultType, Str,
    SupportedParameterType, SupportedReturnType,
};
/// Re-exports of the wire types describing function call shapes.
pub use hyperlight_common::wire::{
    HostFunctionDefinition, HostFunctionDetails, Param, ParameterType, ReturnType, ReturnValue,
};
