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

//! scope corpus: a protected definition with owner-local
//! self-first binding. The root defines a same-named strong `self_value`
//! earlier in the application scope, but this image's own reference must
//! still bind its own definition (the linker binds intra-image references
//! statically for protected symbols; the loader's self-first rule is the
//! backstop for artifacts that still carry such relocations).

#![no_std]

use core::arch::global_asm;

// Mark the exported definition STV_PROTECTED at the object level.
global_asm!(".protected self_value");

#[no_mangle]
pub static self_value: i32 = 333;

#[no_mangle]
pub extern "C" fn protected_report() -> i32 {
    unsafe { core::ptr::read_volatile(&self_value) }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
