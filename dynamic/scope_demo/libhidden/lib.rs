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

//! scope corpus: a hidden definition, visible only to its own
//! image. `hidden_report` proves owner access; any other image's lookup must
//! never see this `hidden_probe` (libstrong's strong one wins instead).

#![no_std]

use core::arch::global_asm;

// Mark the exported definition STV_HIDDEN at the object level.
global_asm!(".hidden hidden_probe");

#[no_mangle]
pub static hidden_probe: i32 = 999;

#[no_mangle]
pub extern "C" fn hidden_report() -> i32 {
    // Owner access: the reference binds this image's own definition.
    unsafe { core::ptr::read_volatile(&hidden_probe) }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
