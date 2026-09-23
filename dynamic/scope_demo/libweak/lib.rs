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

//! scope corpus: the weak definitions, linked with SysV
//! `DT_HASH` only so this artifact also exercises the SysV hash path. The
//! root discovers this image before libstrong; the frozen scope must skip
//! these weak definitions once the strong ones appear.

#![no_std]
#![feature(linkage)]

#[no_mangle]
#[linkage = "weak"]
pub static scope_value: i32 = 222;

#[no_mangle]
#[linkage = "weak"]
pub extern "C" fn scope_fn() -> i32 {
    2220
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
