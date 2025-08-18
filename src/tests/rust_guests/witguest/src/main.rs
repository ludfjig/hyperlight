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

#![no_std]
#![no_main]

extern crate alloc;
extern crate hyperlight_guest;
use crate::bindings::hyperlight::greeting_demo::TimeService;
use alloc::format;
use alloc::string::String;

mod bindings {
    hyperlight_component_macro::guest_bindgen!("interface.wasm");
}

struct Guest {}

impl bindings::hyperlight::greeting_demo::Greeting for Guest {
    fn greet_user(&mut self, username: String) -> String {
        // Call the host to get the current time
        let current_time = (bindings::Host {}).get_current_time();

        format!("Hello {}! The current time is {}", username, current_time)
    }
}

impl bindings::hyperlight::greeting_demo::GreetingDemoExports<bindings::Host> for Guest {
    type Greeting = Self;
    fn greeting(&mut self) -> &mut Self {
        self
    }
}

impl bindings::Guest for Guest {
    fn with_guest_state<R, F: FnOnce(&mut Self) -> R>(f: F) -> R {
        let mut g = Guest {};
        f(&mut g)
    }
}

#[no_mangle]
pub extern "C" fn hyperlight_main() {
    bindings::hyperlight_guest_init::<Guest>();
}

use alloc::vec::Vec;
use hyperlight_common::flatbuffer_wrappers::function_call::FunctionCall;
use hyperlight_common::flatbuffer_wrappers::guest_error::ErrorCode;
use hyperlight_guest::error::{HyperlightGuestError, Result};
#[no_mangle]
pub fn guest_dispatch_function(function_call: FunctionCall) -> Result<Vec<u8>> {
    Err(HyperlightGuestError::new(
        ErrorCode::GuestFunctionNotFound,
        function_call.function_name.clone(),
    ))
}
