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

//! Private-cycle corpus: cycle member A — needs B and the shared
//! common. The constructor increments a data counter the root observes.

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn b_cycle_value() -> i32;
    fn common_value() -> i32;
}

static A_CTOR_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn a_init() {
    A_CTOR_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[used]
#[link_section = ".init_array"]
static A_INIT: extern "C" fn() = a_init;

extern "C" fn a_fini() {}

#[used]
#[link_section = ".fini_array"]
static A_FINI: extern "C" fn() = a_fini;

#[no_mangle]
pub extern "C" fn a_ctor_count() -> u32 {
    A_CTOR_COUNT.load(Ordering::Relaxed)
}

/// The real back-edge probe: libcycle_b's call must bind this definition at
/// runtime (value 1), never the bootstrap stub's (value 0).
#[no_mangle]
pub extern "C" fn a_probe() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn a_cycle_value() -> i32 {
    // Cross the cycle edge: A calls into B, which calls back into A.
    (unsafe { b_cycle_value() }) + (unsafe { common_value() }) + 1
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
