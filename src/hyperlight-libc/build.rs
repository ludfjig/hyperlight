/*
Copyright 2024 The Hyperlight Authors.

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

use std::path::{Path, PathBuf};
use std::{env, fs};

fn copy_includes<P: AsRef<Path>, Q: AsRef<Path> + std::fmt::Debug>(include_dir: P, base: Q) {
    let entries = fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("could not open include dir {:?}: {}", base, e));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("could not read include dir {:?}: {}", base, e));
        let src = entry.path();
        let dst = include_dir.as_ref().join(entry.file_name());
        let kind = entry
            .file_type()
            .unwrap_or_else(|e| panic!("could not find type of {:?}: {}", src, e));
        if kind.is_dir() {
            fs::create_dir_all(&dst)
                .unwrap_or_else(|e| panic!("could not create include dir {:?}, {}", &dst, e));
            copy_includes(&dst, src);
        } else if Some(std::ffi::OsStr::new("h")) == src.extension() {
            fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("could not copy header {:?}, {}", &src, e));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=third_party");
    println!("cargo:rerun-if-changed=include");

    let mut cfg = cc::Build::new();

    // prinf
    cfg.include("third_party/printf")
        .file("third_party/printf/printf.c");

    // libc
    let entries = glob::glob("third_party/musl/**/*.[cs]") // .c and .s files
        .expect("glob pattern should be valid")
        .filter_map(Result::ok);
    cfg.files(entries);

    cfg.include("third_party/musl/src/include")
        .include("third_party/musl/include")
        .include("third_party/musl/src/internal")
        .include("third_party/musl/arch/generic")
        .include("third_party/musl/arch/x86_64");

    let is_pe = env::var("CARGO_CFG_WINDOWS").is_ok();

    cfg.define("HYPERLIGHT", None); // used in certain musl files for conditional compilation

    // silence compiler warnings
    cfg.flag("-Wno-unused-command-line-argument") // including .s files makes clang believe arguments are unused
        .flag("-Wno-sign-compare")
        .flag("-Wno-bitwise-op-parentheses")
        .flag("-Wno-unknown-pragmas")
        .flag("-Wno-shift-op-parentheses")
        .flag("-Wno-logical-op-parentheses")
        .flag("-Wno-unused-but-set-variable")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-string-plus-int");

    if is_pe {
        cfg.flag("-Wno-unused-label");
        cfg.flag("-Wno-unused-variable");
        cfg.compiler("clang-cl");
    } else {
        cfg.flag("-fPIC");
        // This is a terrible hack, because
        // - we need stack clash protection, because we have put the
        //   stack right smack in the middle of everything in the guest
        // - clang refuses to do stack clash protection unless it is
        //   required by a target ABI (Windows, MacOS) or the target is
        //   is Linux or FreeBSD (see Clang.cpp RenderSCPOptions
        //   https://github.com/llvm/llvm-project/blob/1bb52e9/clang/lib/Driver/ToolChains/Clang.cpp#L3724).
        //   Hopefully a flag to force stack clash protection on generic
        //   targets will eventually show up.
        cfg.flag("--target=x86_64-unknown-linux-none");

        // We don't support stack protectors at the moment, but Arch Linux clang
        // auto-enables them for -linux platforms, so explicitly disable them.
        cfg.flag("-fno-stack-protector");
        cfg.flag("-fstack-clash-protection");
        cfg.flag("-mstack-probe-size=4096");
        cfg.compiler("clang");
    }

    if cfg!(windows) {
        env::set_var("AR_x86_64_unknown_none", "llvm-ar");
    } else {
        env::set_var("AR_x86_64_pc_windows_msvc", "llvm-lib");
    }

    cfg.compile("hyperlight_guest");

    let out_dir = env::var("OUT_DIR").expect("cargo OUT_DIR not set");
    let include_dir = PathBuf::from(&out_dir).join("include");
    fs::create_dir_all(&include_dir)
        .unwrap_or_else(|e| panic!("Could not create include dir {:?}: {}", &include_dir, e));

    copy_includes(&include_dir, "third_party/printf/");
    copy_includes(&include_dir, "include");
    copy_includes(&include_dir, "third_party/musl/include");
    copy_includes(&include_dir, "third_party/musl/arch/generic");
    copy_includes(&include_dir, "third_party/musl/arch/x86_64");
    copy_includes(&include_dir, "third_party/musl/src/internal");

    /* do not canonicalize: clang has trouble with UNC paths */
    let include_str = include_dir
        .to_str()
        .expect("out dir include dir was not valid utf-8");
    println!("cargo::metadata=include={}", include_str);
}
