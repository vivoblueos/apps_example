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

//! Private-cycle corpus root: calls across the A↔B cycle edge and
//! reports the constructor counts — exactly one per group, in the frozen
//! SCC order the LIFECYCLE oracle asserts.

#![no_std]
#![no_main]
#![feature(c_variadic)]

use core::ffi::{c_char, c_int};

extern "C" {
    fn printf(format: *const c_char, ...) -> c_int;
    fn a_cycle_value() -> i32;
    fn a_ctor_count() -> u32;
    fn b_ctor_count() -> u32;
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
    // a_cycle_value = b_cycle_value + common + 1
    // = (real a_probe=1 + common=40) + 40 + 1 = 82.
    let value = unsafe { a_cycle_value() };
    let a_ctor = unsafe { a_ctor_count() };
    let b_ctor = unsafe { b_ctor_count() };
    let ok = value == 82 && a_ctor == 1 && b_ctor == 1;
    unsafe {
        printf(
            b"cycle: value=%d a_ctor=%d b_ctor=%d\n\0".as_ptr() as *const c_char,
            value,
            a_ctor as c_int,
            b_ctor as c_int,
        );
    }
    ok as c_int
}
