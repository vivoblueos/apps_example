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

//! negative corpus root: an ordinary root whose only private
//! dependency carries the undefined weak call. The package link must be
//! rejected at relocation, so this main must never run.

#![no_std]
#![no_main]

use core::ffi::{c_char, c_int};

extern "C" {
    fn weakcall_present() -> i32;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn main(
    _argc: c_int,
    _argv: *const *const c_char,
    _envp: *const *const c_char,
) -> c_int {
    // Unreachable: the package link is rejected before any constructor runs.
    unsafe { weakcall_present() }
}
