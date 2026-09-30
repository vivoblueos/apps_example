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

//! negative corpus: a private DSO whose constructor calls an
//! undefined weak *function*. Shared objects may carry such a reference, but
//! the runtime relocation policy must fail closed — an undefined weak
//! control-flow target never binds to zero — so the whole package link is
//! rejected before any constructor runs.

#![no_std]
#![feature(linkage)]

extern "C" {
    #[linkage = "extern_weak"]
    fn missing_fn() -> i32;
}

extern "C" fn weak_call_ctor() {
    unsafe { missing_fn() };
}

#[used]
#[link_section = ".init_array"]
static WEAK_CALL_CTOR: extern "C" fn() = weak_call_ctor;

/// A benign export the root references so `--as-needed` keeps this DSO in the
/// closure; the weak call itself is what the loader must reject.
#[no_mangle]
pub extern "C" fn weakcall_present() -> i32 {
    1
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
