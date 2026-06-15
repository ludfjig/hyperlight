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

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::params::{ParameterType, ReturnType};

/// Definition of a function the host exposes to the guest.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFunctionDefinition {
    pub function_name: String,
    pub parameter_types: Option<Vec<ParameterType>>,
    pub return_type: ReturnType,
}

impl HostFunctionDefinition {
    pub fn new(
        function_name: String,
        parameter_types: Option<Vec<ParameterType>>,
        return_type: ReturnType,
    ) -> Self {
        Self {
            function_name,
            parameter_types,
            return_type,
        }
    }
}

/// Aggregate of all host functions exposed to the guest.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostFunctionDetails {
    pub host_functions: Option<Vec<HostFunctionDefinition>>,
}
