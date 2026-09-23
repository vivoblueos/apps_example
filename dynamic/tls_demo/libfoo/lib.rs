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

//! emutls corpus: a per-image `thread_local` counter. Its
//! `EmutlsControl` is an ordinary data symbol of this image; the
//! `__emutls_get_address` reference must bind the unique system libc, never a
//! second static runtime.

#![no_std]
#![feature(thread_local)]

#[thread_local]
static TLS_COUNTER: core::cell::Cell<u32> = core::cell::Cell::new(0);

#[no_mangle]
pub extern "C" fn foo_tls_next() -> u32 {
    let next = TLS_COUNTER.get() + 1;
    TLS_COUNTER.set(next);
    next
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
