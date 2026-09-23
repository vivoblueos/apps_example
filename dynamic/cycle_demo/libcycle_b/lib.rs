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

//! Private-cycle corpus: cycle member B — needs A (the back edge) and
//! the shared common. `b_cycle_value` calls back into A across the cycle.

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn a_probe() -> i32;
    fn common_value() -> i32;
}

static B_CTOR_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn b_init() {
    B_CTOR_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[used]
#[link_section = ".init_array"]
static B_INIT: extern "C" fn() = b_init;

extern "C" fn b_fini() {}

#[used]
#[link_section = ".fini_array"]
static B_FINI: extern "C" fn() = b_fini;

#[no_mangle]
pub extern "C" fn b_ctor_count() -> u32 {
    B_CTOR_COUNT.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn b_cycle_value() -> i32 {
    // The back edge: B's call into A binds the *real* A at runtime — the
    // bootstrap stub only keeps the DT_NEEDED on the link line.
    (unsafe { a_probe() }) + (unsafe { common_value() })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
