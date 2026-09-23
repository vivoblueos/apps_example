// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `libcommon.so.1` — the diamond's shared private dependency.
//!
//! A minimal no_std shared object over the system libc: one function symbol,
//! one data symbol, and a constructor/fini pair so the lifecycle fixtures can
//! verify shared dependency construction runs exactly once per link.

#![no_std]

/// A cross-DSO data symbol (exercises GLOB_DAT/RELATIVE data relocations).
#[no_mangle]
pub static COMMON_BASE: i32 = 40;

/// The shared function every dependent calls.
#[no_mangle]
pub extern "C" fn common_value() -> i32 {
    COMMON_BASE
}

/// Constructor: runs once per generation when the image is initialized.
#[used]
#[link_section = ".init_array"]
static COMMON_INIT: extern "C" fn() = common_init;

extern "C" fn common_init() {
    // Placeholder; the lifecycle fixture records construction order here in a
    // Constructor side effects are observed by the integration test.
}

/// Destructor: runs in exact reverse order when the group reaps.
#[used]
#[link_section = ".fini_array"]
static COMMON_FINI: extern "C" fn() = common_fini;

extern "C" fn common_fini() {}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
