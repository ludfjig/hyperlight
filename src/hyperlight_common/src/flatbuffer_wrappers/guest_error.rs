// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.

use alloc::string::String;

#[cfg(feature = "tracing")]
use tracing::{Span, instrument};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
/// `ErrorCode` represents an error that occurred in the Hyperlight Guest.
pub enum ErrorCode {
    NoError = 0,
    UnsupportedParameterType = 2,
    GuestFunctionNameNotProvided = 3,
    GuestFunctionNotFound = 4,
    GuestFunctionIncorrecNoOfParameters = 5,
    GispatchFunctionPointerNotSet = 6,
    OutbError = 7,
    UnknownError = 8,
    GsCheckFailed = 10,
    TooManyGuestFunctions = 11,
    FailureInDlmalloc = 12,
    MallocFailed = 13,
    GuestFunctionParameterTypeMismatch = 14,
    GuestError = 15,
    ArrayLengthParamIsMissing = 16,
    HostFunctionError = 17,
}

impl From<u64> for ErrorCode {
    fn from(error_code: u64) -> Self {
        match error_code {
            0 => Self::NoError,
            2 => Self::UnsupportedParameterType,
            3 => Self::GuestFunctionNameNotProvided,
            4 => Self::GuestFunctionNotFound,
            5 => Self::GuestFunctionIncorrecNoOfParameters,
            6 => Self::GispatchFunctionPointerNotSet,
            7 => Self::OutbError,
            8 => Self::UnknownError,
            10 => Self::GsCheckFailed,
            11 => Self::TooManyGuestFunctions,
            12 => Self::FailureInDlmalloc,
            13 => Self::MallocFailed,
            14 => Self::GuestFunctionParameterTypeMismatch,
            15 => Self::GuestError,
            16 => Self::ArrayLengthParamIsMissing,
            17 => Self::HostFunctionError,
            _ => Self::UnknownError,
        }
    }
}

impl From<ErrorCode> for u64 {
    fn from(error_code: ErrorCode) -> Self {
        match error_code {
            ErrorCode::NoError => 0,
            ErrorCode::UnsupportedParameterType => 2,
            ErrorCode::GuestFunctionNameNotProvided => 3,
            ErrorCode::GuestFunctionNotFound => 4,
            ErrorCode::GuestFunctionIncorrecNoOfParameters => 5,
            ErrorCode::GispatchFunctionPointerNotSet => 6,
            ErrorCode::OutbError => 7,
            ErrorCode::UnknownError => 8,
            ErrorCode::GsCheckFailed => 10,
            ErrorCode::TooManyGuestFunctions => 11,
            ErrorCode::FailureInDlmalloc => 12,
            ErrorCode::MallocFailed => 13,
            ErrorCode::GuestFunctionParameterTypeMismatch => 14,
            ErrorCode::GuestError => 15,
            ErrorCode::ArrayLengthParamIsMissing => 16,
            ErrorCode::HostFunctionError => 17,
        }
    }
}

/// `GuestError` represents an error that occurred in the Hyperlight Guest.
#[derive(Debug, Clone)]
pub struct GuestError {
    /// The error code.
    pub code: ErrorCode,
    /// The error message.
    pub message: String,
}

impl GuestError {
    #[cfg_attr(feature = "tracing", instrument(skip_all, parent = Span::current(), level= "Trace"))]
    pub fn new(code: ErrorCode, message: String) -> Self {
        Self { code, message }
    }
}

impl Default for GuestError {
    #[cfg_attr(feature = "tracing", instrument(parent = Span::current(), level= "Trace"))]
    fn default() -> Self {
        Self {
            code: ErrorCode::NoError,
            message: String::new(),
        }
    }
}
