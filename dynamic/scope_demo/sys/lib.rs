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

//! scope corpus: the second system DSO. The
//! application root defines a same-named `sys_target = 42`, but this image's
//! own relocation must bind its own definition (`sys_target = 777`) through
//! the system scope — an application symbol must never interpose a system
//! DSO's own relocation.
//!
//! Lifecycle evidence: the constructor/destructor maintain plain data
//! counters (this DSO deliberately makes no libc call from its ctors — a far
//! `puts` through LLD's `__ThumbV7PILongThunk` veneer enters the PLT stub
//! without the Thumb bit, a toolchain interworking quirk). The root observes
//! the constructor count through `sys_ctor_count` (1 per fresh instance, so
//! a reload is visible), and the kernel's reaper oracle logs the destructor
//! run (`DSO_FINI`) before releasing the backing.

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn strlen(value: *const core::ffi::c_char) -> usize;
}

static SYS_CTOR_COUNT: AtomicU32 = AtomicU32::new(0);
static SYS_FINI_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn sys_init() {
    SYS_CTOR_COUNT.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn sys_fini() {
    SYS_FINI_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[used]
#[link_section = ".init_array"]
static SYS_INIT: extern "C" fn() = sys_init;

#[used]
#[link_section = ".fini_array"]
static SYS_FINI: extern "C" fn() = sys_fini;

/// The number of times this instance's constructor ran — 1 for a fresh
/// instance; the root asserts this on every launch to prove a reload.
#[no_mangle]
pub extern "C" fn sys_ctor_count() -> u32 {
    SYS_CTOR_COUNT.load(Ordering::Relaxed)
}

#[no_mangle]
pub static sys_target: i32 = 777;

#[no_mangle]
pub extern "C" fn sys_report() -> i32 {
    unsafe { core::ptr::read_volatile(&sys_target) }
}

/// Retained, but not called by the scope root: its `strlen` reference gives
/// this system DSO a real outgoing dependency on `libc.so.1`, allowing the
/// registry test to verify cross-SCC backing leases without adding a libc call
/// to constructor/destructor execution.
#[no_mangle]
pub extern "C" fn sys_libc_dependency_probe(value: *const core::ffi::c_char) -> usize {
    unsafe { strlen(value) }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
