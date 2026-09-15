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

//! scope corpus: the strong definitions that must win the
//! application scope over libweak's earlier-discovered weak ones, and the
//! strong `hidden_probe` that must shadow libhidden's invisible definition.

#![no_std]

#[no_mangle]
pub static scope_value: i32 = 111;

#[no_mangle]
pub extern "C" fn scope_fn() -> i32 {
    1110
}

#[no_mangle]
pub static hidden_probe: i32 = 555;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
