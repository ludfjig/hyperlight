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

pub(crate) mod error;
pub(crate) mod functions;
pub(crate) mod into_params;
pub(crate) mod param_type;
pub(crate) mod ret_type;
mod utils;

pub use error::Error;
pub use functions::{BorrowingFunction, Function};
pub use into_params::{IntoParam, IntoParameters};
pub use param_type::{Bytes, ParameterTuple, Str, SupportedParameterType};
pub use ret_type::{
    Borrows, BytesRef, EncodeReturn, OwnedReturn, ResultType, ReturnCarrier, StrRef,
    SupportedReturnType,
};

pub use crate::wire::{Param, ParameterType, ReturnType, ReturnValue};
