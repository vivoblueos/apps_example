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

//! scope corpus: an undefined weak data reference. The frozen
//! scope binds it to zero (asserted by the SCOPE_BIND oracle with
//! `provider=none`); this image stores the resolved value in a data slot and
//! reports it, so the runtime evidence is `weakdata_value == 0`.

#![no_std]

use core::arch::global_asm;

// A native-width absolute data relocation against the undefined weak symbol:
// LLD keeps it dynamic, and the loader's relocation policy binds undefined
// weak *data* to zero when no definition is available.
macro_rules! weak_data_slot {
    ($word:literal) => {
        global_asm!(
            ".weak missing_data",
            ".global weakdata_value",
            ".section .data.weakdata_value",
            ".align 4",
            "weakdata_value:",
            $word,
        );
    };
}

#[cfg(target_pointer_width = "32")]
weak_data_slot!(".word missing_data");
#[cfg(target_pointer_width = "64")]
weak_data_slot!(".quad missing_data");

extern "C" {
    static weakdata_value: usize;
}

#[no_mangle]
pub extern "C" fn weakdata_read() -> usize {
    unsafe { core::ptr::read_volatile(&weakdata_value) }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
