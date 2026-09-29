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
use core::ffi::{c_char, c_void};
extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
    fn dl_event(tag: usize);
}
static mut VALUE: usize = 0;
extern "C" fn load_child() {
    unsafe {
        let handle = dlopen(b"libdl_dep.so.1\0".as_ptr().cast(), 2);
        assert!(!handle.is_null());
        let pointer = dlsym(handle, b"dl_dep_value\0".as_ptr().cast());
        assert!(!pointer.is_null());
        let function: extern "C" fn() -> usize = core::mem::transmute(pointer);
        VALUE = function();
        assert_eq!(dlclose(handle), 0);
    }
}
extern "C" fn init() {
    load_child();
    unsafe {
        dl_event(4);
    }
}
extern "C" fn fini() {
    load_child();
    unsafe {
        dl_event(5);
    }
}
#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = init;
#[used]
#[link_section = ".fini_array"]
static FINI: extern "C" fn() = fini;
#[no_mangle]
pub extern "C" fn dl_nested_value() -> usize {
    unsafe { VALUE }
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    extern "C" {
        fn write(fd: i32, data: *const u8, len: usize) -> isize;
    }
    let message = b"DLOPEN_FAIL nested\n";
    unsafe {
        write(1, message.as_ptr(), message.len());
    }
    loop {}
}
