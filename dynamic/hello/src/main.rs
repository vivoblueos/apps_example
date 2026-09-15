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

//! fixture: a minimal dynamic application.
//!
//! This crate is a bare `no_std`/`no_main` PIE: `blueos_scrt1::_start` is its
//! ELF entry, which resolves this crate's `main` symbol and tail-calls
//! `__librs_start_main(main, info)` from the shared `libc.so.1`. Everything
//! printed below goes through libc's `write`, i.e. through a real cross-DSO
//! `JUMP_SLOT` relocation, which is the point of the fixture.

#![no_std]
#![no_main]

use core::ffi::{c_char, c_int};

extern "C" {
    fn write(fd: c_int, buf: *const c_char, count: usize) -> isize;
    fn getauxval(key: usize) -> usize;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Write a byte slice to fd 1 through libc's `write`.
fn put(fd: c_int, bytes: &[u8]) {
    unsafe {
        write(fd, bytes.as_ptr() as *const c_char, bytes.len());
    }
}

#[no_mangle]
pub extern "C" fn main(
    argc: c_int,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    put(1, b"hello dynamic app\n");

    // argc/argv round-trip: argv[0] is the application path from the kernel's
    // start storage.
    let argc_bytes = &[b'0' + (argc as u8), b'\n'];
    put(1, argc_bytes);
    if argc > 0 && !argv.is_null() {
        // SAFETY: argv is the pinned C-style pointer array from the kernel
        // start storage, valid for the whole application lifetime.
        unsafe {
            let name = core::ffi::CStr::from_ptr(*argv);
            put(1, b"argv0=");
            put(1, name.to_bytes());
            put(1, b"\n");
        }
    }

    // AT_PHDR must be resolvable through the shared libc's getauxval, which
    // proves the per-application auxv context round-tripped the SWI boundary.
    let at_phdr: usize = 3;
    let phdr = unsafe { getauxval(at_phdr) };
    if phdr == 0 {
        put(1, b"auxv: no AT_PHDR\n");
    } else {
        put(1, b"auxv: AT_PHDR ok\n");
    }

    let _ = envp;
    0
}
