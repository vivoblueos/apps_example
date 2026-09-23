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

//! `libfoo.so.1` — depends on `libcommon.so.1` and the system `libc.so.1`.

#![no_std]

extern "C" {
    fn common_value() -> i32;
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

/// A function the root calls; crosses the foo→common dependency edge.
#[no_mangle]
pub extern "C" fn foo_value() -> i32 {
    unsafe { common_value() + 2 }
}

#[no_mangle]
pub extern "C" fn foo_report() {
    static MSG: &[u8] = b"foo ran\n";
    unsafe {
        write(1, MSG.as_ptr(), MSG.len());
    }
}

#[used]
#[link_section = ".init_array"]
static FOO_INIT: extern "C" fn() = foo_init;

extern "C" fn foo_init() {}

#[used]
#[link_section = ".fini_array"]
static FOO_FINI: extern "C" fn() = foo_fini;

extern "C" fn foo_fini() {}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
