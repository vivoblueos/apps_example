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

//! emutls corpus root: drives two worker threads through the
//! private DSOs' same-named `thread_local` counters and verifies per-image
//! control identity and per-thread value isolation, then repeats the
//! create/join cycle to exercise the per-thread destructor paths.

#![no_std]
#![no_main]
#![feature(c_variadic)]

use core::ffi::{c_char, c_int, c_void};

extern "C" {
    fn printf(format: *const c_char, ...) -> c_int;
    fn foo_tls_next() -> u32;
    fn bar_tls_next() -> u32;
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: usize, retval: *mut *mut c_void) -> c_int;
}

/// One worker: increments both images' counters exactly `loops` times and
/// returns the pair packed as `(foo << 16) | bar` through the argument slot.
extern "C" fn tls_worker(arg: *mut c_void) -> *mut c_void {
    let loops = arg as usize;
    let mut foo = 0u32;
    let mut bar = 0u32;
    for _ in 0..loops {
        foo = unsafe { foo_tls_next() };
        bar = unsafe { bar_tls_next() };
    }
    (((foo as usize) << 16) | bar as usize) as *mut c_void
}

fn run_worker(loops: usize) -> (u32, u32) {
    let mut thread: usize = 0;
    let mut result: *mut c_void = core::ptr::null_mut();
    let rc = unsafe {
        pthread_create(
            &mut thread,
            core::ptr::null(),
            tls_worker,
            loops as *mut c_void,
        )
    };
    if rc != 0 {
        return (u32::MAX, u32::MAX);
    }
    unsafe { pthread_join(thread, &mut result) };
    let packed = result as usize;
    (((packed >> 16) & 0xffff) as u32, (packed & 0xffff) as u32)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn main(
    _argc: c_int,
    _argv: *const *const c_char,
    _envp: *const *const c_char,
) -> c_int {
    // Two threads, different loop counts: full isolation means each thread
    // ends at exactly its own count in both images' counters.
    let (a_foo, a_bar) = run_worker(7);
    let (b_foo, b_bar) = run_worker(13);

    // Repeat the create/join cycle so the per-thread emutls arrays are
    // created and destroyed repeatedly.
    let mut repeat_ok = true;
    for _ in 0..3 {
        let (r_foo, r_bar) = run_worker(5);
        repeat_ok &= r_foo == 5 && r_bar == 5;
    }

    let ok = a_foo == 7 && a_bar == 7 && b_foo == 13 && b_bar == 13 && repeat_ok;
    unsafe {
        printf(
            b"tls: a_foo=%d a_bar=%d b_foo=%d b_bar=%d repeat=%d\n\0".as_ptr() as *const c_char,
            a_foo as c_int,
            a_bar as c_int,
            b_foo as c_int,
            b_bar as c_int,
            repeat_ok as c_int,
        );
    }
    ok as c_int
}
