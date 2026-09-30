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
extern "C" {
    fn dl_event(tag: usize);
    fn dl_dep_value() -> usize;
    fn pthread_key_create(
        key: *mut u32,
        destructor: Option<extern "C" fn(*mut core::ffi::c_void)>,
    ) -> i32;
    fn pthread_key_delete(key: u32) -> i32;
    fn pthread_setspecific(key: u32, value: *const core::ffi::c_void) -> i32;
}
static mut COUNT: usize = 0;
static mut CALLS: usize = 0;
static mut KEY: u32 = u32::MAX;
extern "C" fn key_fini(_: *mut core::ffi::c_void) {
    unsafe {
        assert_eq!(pthread_key_delete(KEY), 0);
        KEY = u32::MAX;
        dl_event(6);
    }
}
#[no_mangle]
pub extern "C" fn dl_plugin_set_key() -> usize {
    unsafe {
        assert_eq!(pthread_key_create(&raw mut KEY, Some(key_fini)), 0);
        assert_eq!(
            pthread_setspecific(KEY, 1usize as *const core::ffi::c_void),
            0
        );
    }
    1
}
extern "C" fn init() {
    unsafe {
        COUNT += 1;
        dl_event(2);
    }
}
extern "C" fn fini() {
    unsafe {
        dl_event(3);
    }
}
#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = init;
#[used]
#[link_section = ".fini_array"]
static FINI: extern "C" fn() = fini;
#[no_mangle]
pub extern "C" fn dl_plugin_value() -> usize {
    unsafe { dl_dep_value() + 1 }
}
#[no_mangle]
pub extern "C" fn dl_plugin_ctor_count() -> usize {
    unsafe { COUNT }
}
#[no_mangle]
pub extern "C" fn dl_plugin_next() -> usize {
    unsafe {
        CALLS += 1;
        CALLS
    }
}
core::arch::global_asm!(".hidden dl_plugin_hidden");
// Listed in rustc's export script; the fixture link replaces it with SHN_ABS 0.
#[no_mangle]
pub static dl_absolute_zero: usize = 0;
#[no_mangle]
pub extern "C" fn dl_plugin_hidden() -> usize {
    999
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
