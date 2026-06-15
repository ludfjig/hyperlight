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

#![no_main]

use std::sync::{Mutex, OnceLock};

use hyperlight_common::wire::{ErrorCode, Param};
use hyperlight_host::func::ReturnType;
use hyperlight_host::sandbox::SandboxConfiguration;
use hyperlight_host::sandbox::uninitialized::GuestBinary;
use hyperlight_host::{HyperlightError, MultiUseSandbox, UninitializedSandbox};
use hyperlight_testing::simple_guest_for_fuzzing_as_string;
use libfuzzer_sys::arbitrary::{Arbitrary, Result as ArbResult, Unstructured};
use libfuzzer_sys::fuzz_target;

static SANDBOX: OnceLock<Mutex<MultiUseSandbox>> = OnceLock::new();

/// Owned variant of a wire `Param`. Backing storage for the borrows
/// `Param<'_>` carries.
#[derive(Debug)]
enum OwnedParam {
    Int(i32),
    UInt(u32),
    Long(i64),
    ULong(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    String(String),
    VecBytes(Vec<u8>),
}

impl<'a> Arbitrary<'a> for OwnedParam {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbResult<Self> {
        match u.int_in_range(0..=8)? {
            0 => Ok(OwnedParam::Int(u.arbitrary()?)),
            1 => Ok(OwnedParam::UInt(u.arbitrary()?)),
            2 => Ok(OwnedParam::Long(u.arbitrary()?)),
            3 => Ok(OwnedParam::ULong(u.arbitrary()?)),
            4 => Ok(OwnedParam::Float(u.arbitrary()?)),
            5 => Ok(OwnedParam::Double(u.arbitrary()?)),
            6 => Ok(OwnedParam::Bool(u.arbitrary()?)),
            7 => Ok(OwnedParam::String(u.arbitrary()?)),
            _ => Ok(OwnedParam::VecBytes(u.arbitrary()?)),
        }
    }
}

impl OwnedParam {
    fn as_param(&self) -> Param<'_> {
        match self {
            OwnedParam::Int(v) => Param::Int(*v),
            OwnedParam::UInt(v) => Param::UInt(*v),
            OwnedParam::Long(v) => Param::Long(*v),
            OwnedParam::ULong(v) => Param::ULong(*v),
            OwnedParam::Float(v) => Param::Float(*v),
            OwnedParam::Double(v) => Param::Double(*v),
            OwnedParam::Bool(v) => Param::Bool(*v),
            OwnedParam::String(s) => Param::String(s.as_str()),
            OwnedParam::VecBytes(b) => Param::VecBytes(b.as_slice()),
        }
    }
}

fuzz_target!(
    init: {
        let mut cfg = SandboxConfiguration::default();
        cfg.set_output_data_size(64 * 1024);
        cfg.set_input_data_size(64 * 1024);
        cfg.set_scratch_size(512 * 1024);
        let u_sbox = UninitializedSandbox::new(
            GuestBinary::FilePath(simple_guest_for_fuzzing_as_string().expect("Guest Binary Missing")),
            Some(cfg)
        )
        .unwrap();

        let mu_sbox: MultiUseSandbox = u_sbox.evolve().unwrap();
        SANDBOX.set(Mutex::new(mu_sbox)).unwrap();
    },

    |data: (String, ReturnType, Vec<OwnedParam>)| {
        let (host_func_name, host_func_return, owned_params) = data;
        let mut sandbox = SANDBOX.get().unwrap().lock().unwrap();
        let mut params: Vec<Param<'_>> = Vec::with_capacity(owned_params.len() + 1);
        params.push(Param::String(host_func_name.as_str()));
        for p in &owned_params {
            params.push(p.as_param());
        }
        if let Err(e) = sandbox.call_type_erased_guest_function_by_name("FuzzHostFunc", host_func_return, params) {
            match e {
                HyperlightError::HostFunctionNotFound(_) => {}
                HyperlightError::GuestError(ErrorCode::HostFunctionError, msg) if msg == format!("HostFunction {} was not found", host_func_name) => {}
                HyperlightError::UnexpectedNoOfArguments(_, _) => {},
                HyperlightError::GuestError(ErrorCode::HostFunctionError, msg) if msg.contains("The number of arguments to the function is wrong") => {}
                HyperlightError::ParameterValueConversionFailure(_, _) => {},
                HyperlightError::GuestError(ErrorCode::HostFunctionError, msg) if msg.contains("Failed To Convert Parameter Value") => {}
                _ => panic!("Guest Aborted with Unexpected Error: {:?}", e),
            }
        }
    }
);
