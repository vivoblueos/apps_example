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

#![no_std]
static mut COUNT: usize = 0;
extern "C" fn init() {
    extern "C" {
        fn dlopen(name: *const core::ffi::c_char, flags: i32) -> *mut core::ffi::c_void;
        fn dlsym(
            handle: *mut core::ffi::c_void,
            name: *const core::ffi::c_char,
        ) -> *mut core::ffi::c_void;
        fn dlclose(handle: *mut core::ffi::c_void) -> i32;
        fn dlerror() -> *mut core::ffi::c_char;
        fn write(fd: i32, data: *const u8, len: usize) -> isize;
    }
    unsafe {
        // A new system DSO depending on this still-Initializing libc cannot
        // become registry-Ready yet. Its completed constructor is rolled back,
        // and a subsequent private load must still acquire the runtime gate.
        let system = dlopen(b"libscope_sys.so\0".as_ptr().cast(), 2);
        if system.is_null() {
            assert!(!dlerror().is_null());
            assert!(dlerror().is_null());
            let message = b"DLOPEN_STARTUP_ROLLBACK\n";
            write(1, message.as_ptr(), message.len());
        } else {
            // Later applications import an already-Ready cached libc.
            assert_eq!(dlclose(system), 0);
        }
        // The startup libc batch is still Initializing; dlopen must borrow it
        // instead of waiting for this very constructor to complete.
        let handle = dlopen(b"libdl_dep.so.1\0".as_ptr().cast(), 2);
        assert!(!handle.is_null());
        let function = dlsym(handle, b"dl_dep_value\0".as_ptr().cast());
        assert!(!function.is_null());
        let function: extern "C" fn() -> usize = core::mem::transmute(function);
        assert_eq!(function(), 41);
        assert_eq!(dlclose(handle), 0);
        COUNT += 1;
    }
}
extern "C" fn fini() {
    extern "C" {
        fn write(fd: i32, data: *const u8, len: usize) -> isize;
    }
    let message = b"DLOPEN_BASE_FINI\n";
    unsafe {
        write(1, message.as_ptr(), message.len());
    }
}
#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = init;
#[used]
#[link_section = ".fini_array"]
static FINI: extern "C" fn() = fini;
#[no_mangle]
pub extern "C" fn dl_base_count() -> usize {
    unsafe { COUNT }
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    extern "C" {
        fn write(fd: i32, data: *const u8, len: usize) -> isize;
    }
    let message = b"DLOPEN_FAIL startup constructor\n";
    unsafe {
        write(1, message.as_ptr(), message.len());
    }
    loop {}
}
