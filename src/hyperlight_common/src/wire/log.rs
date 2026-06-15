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

use log::Level;
use serde::{Deserialize, Serialize};

/// Log levels supported across the guest-host boundary. Numeric values
/// are preserved from the flatbuffer schema for C API and tracing
/// compatibility.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Information = 2,
    Warning = 3,
    Error = 4,
    Critical = 5,
    None = 6,
}

impl From<u8> for LogLevel {
    fn from(val: u8) -> LogLevel {
        match val {
            0 => LogLevel::Trace,
            1 => LogLevel::Debug,
            2 => LogLevel::Information,
            3 => LogLevel::Warning,
            4 => LogLevel::Error,
            5 => LogLevel::Critical,
            _ => LogLevel::None,
        }
    }
}

impl From<&LogLevel> for Level {
    fn from(val: &LogLevel) -> Level {
        match val {
            LogLevel::Trace => Level::Trace,
            LogLevel::Debug => Level::Debug,
            LogLevel::Information => Level::Info,
            LogLevel::Warning => Level::Warn,
            LogLevel::Error => Level::Error,
            LogLevel::Critical => Level::Error,
            LogLevel::None => Level::Trace,
        }
    }
}

impl From<Level> for LogLevel {
    fn from(val: Level) -> LogLevel {
        match val {
            Level::Trace => LogLevel::Trace,
            Level::Debug => LogLevel::Debug,
            Level::Info => LogLevel::Information,
            Level::Warn => LogLevel::Warning,
            Level::Error => LogLevel::Error,
        }
    }
}

/// A log record emitted by the guest. Logs are infrequent and small,
/// so we own all strings rather than borrowing.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuestLogData {
    pub message: String,
    pub source: String,
    pub level: LogLevel,
    pub caller: String,
    pub source_file: String,
    pub line: u32,
}

impl GuestLogData {
    pub fn new(
        message: String,
        source: String,
        level: LogLevel,
        caller: String,
        source_file: String,
        line: u32,
    ) -> Self {
        Self {
            message,
            source,
            level,
            caller,
            source_file,
            line,
        }
    }
}
