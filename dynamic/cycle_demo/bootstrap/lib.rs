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

//! Build-bootstrap stub for the private cycle: GN requires an
//! acyclic build graph, so the back edge is linked against this stub, which
//! carries the *same* SONAME (`libcycle_a.so.1`) as the real image. The
//! runtime resolves the back-edge symbols against the real image; only the
//! recorded `DT_NEEDED` name matters here.

#![no_std]

/// The stub definition of the back-edge probe: libcycle_b's call keeps the
/// bootstrap on the link line (as-needed), recording the DT_NEEDED. The
/// runtime binding must land on the *real* libcycle_a definition (value 1),
/// which the root's value check proves.
#[no_mangle]
pub extern "C" fn a_probe() -> i32 {
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
