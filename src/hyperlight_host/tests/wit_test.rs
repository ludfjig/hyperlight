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
#![allow(clippy::disallowed_macros)]

extern crate alloc;
mod bindings {
    hyperlight_component_macro::host_bindgen!("../tests/rust_guests/witguest/interface.wasm");
}

struct Host {}

impl bindings::hyperlight::greeting_demo::TimeService for Host {
    fn get_current_time(&mut self) -> alloc::string::String {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

#[allow(refining_impl_trait)]
impl bindings::hyperlight::greeting_demo::GreetingDemoImports for Host {
    type TimeService = Self;
    fn time_service(&mut self) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod wit_test {
    use super::*;
    use crate::bindings::hyperlight::greeting_demo::{Greeting, GreetingDemoExports};
    use hyperlight_host::{GuestBinary, UninitializedSandbox};
    use hyperlight_testing::wit_guest_as_string;

    #[test]
    fn test_greeting_demo() {
        let path = wit_guest_as_string().unwrap();
        let binary_path = GuestBinary::FilePath(path);
        let uninit = UninitializedSandbox::new(binary_path, None).unwrap();
        let mut sandbox =
            bindings::hyperlight::greeting_demo::GreetingDemo::instantiate(uninit, Host {});

        // Test the greeting functionality
        let username = "Alice";
        let greeting = sandbox.greeting().greet_user(username.to_string());

        println!("{}", greeting);
    }
}
