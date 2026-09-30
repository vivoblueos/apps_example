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
#![no_main]
#![feature(c_variadic)]

use core::{
    ffi::{c_char, c_int, c_void},
    sync::atomic::{AtomicUsize, Ordering},
};
const NOW: i32 = 2;
const GLOBAL: i32 = 0x100;
const DEFAULT: *mut c_void = core::ptr::null_mut();

extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *mut c_char;
    fn write(fd: c_int, data: *const u8, len: usize) -> isize;
    fn pthread_create(
        tid: *mut usize,
        attr: *const c_void,
        entry: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(tid: usize, result: *mut *mut c_void) -> c_int;
    fn usleep(microseconds: u32) -> c_int;
    fn dl_base_count() -> usize;
    fn dlopen_c_api_probe() -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn read(fd: c_int, data: *mut u8, len: usize) -> isize;
    fn close(fd: c_int) -> c_int;
}

static EVENTS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static WORKERS_OPEN: AtomicUsize = AtomicUsize::new(0);
static SHARED_HANDLE: AtomicUsize = AtomicUsize::new(0);
static EXIT_MODE: AtomicUsize = AtomicUsize::new(0);

#[no_mangle]
pub extern "C" fn dl_event(tag: usize) {
    if tag == 2 || tag == 3 {
        assert!(
            EVENTS[0].load(Ordering::SeqCst) > EVENTS[1].load(Ordering::SeqCst),
            "dependency must outlive its consumer"
        );
    }
    EVENTS[tag].fetch_add(1, Ordering::SeqCst);
    if tag == 3 && EXIT_MODE.load(Ordering::SeqCst) != 0 {
        assert_eq!(EVENTS[6].load(Ordering::SeqCst), 1);
        print(b"DLOPEN_EXIT_FINI\n");
    }
    if tag == 6 {
        assert_eq!(EVENTS[3].load(Ordering::SeqCst), 0);
        print(b"DLOPEN_KEY_FINI\n");
    }
    if tag == 5 && EXIT_MODE.load(Ordering::SeqCst) != 0 {
        print(b"DLOPEN_EXIT_NESTED_FINI\n");
    }
}

fn print(message: &[u8]) {
    unsafe {
        write(1, message.as_ptr(), message.len());
    }
}
unsafe fn load(name: &[u8], flags: i32) -> *mut c_void {
    let handle = dlopen(name.as_ptr().cast(), flags);
    if handle.is_null() {
        let error = dlerror();
        if !error.is_null() {
            let mut len = 0;
            while *error.add(len) != 0 {
                len += 1;
            }
            write(1, error.cast(), len);
            print(b"\n");
        }
        panic!("dlopen fixture failed");
    }
    handle
}
unsafe fn value(handle: *mut c_void, name: &[u8]) -> usize {
    let pointer = dlsym(handle, name.as_ptr().cast());
    assert!(!pointer.is_null());
    let function: extern "C" fn() -> usize = core::mem::transmute(pointer);
    function()
}
unsafe fn error_once() {
    assert!(!dlerror().is_null());
    assert!(dlerror().is_null());
}

fn failures_and_startup() {
    unsafe {
        assert!(dlerror().is_null());
        assert_eq!(dlopen_c_api_probe(), 0);
        assert!(dlopen(b"missing.so\0".as_ptr().cast(), NOW).is_null());
        error_once();
        for flags in [0, 3, 0x4000] {
            assert!(dlopen(core::ptr::null(), flags).is_null());
            error_once();
        }
        assert_eq!(dlclose(DEFAULT), -1);
        error_once();
        assert_eq!(dlclose(usize::MAX as *mut c_void), -1);
        error_once();
        assert!(dlsym(DEFAULT, core::ptr::null()).is_null());
        error_once();
        let root = dlopen(core::ptr::null(), NOW);
        assert!(!root.is_null());
        assert!(!dlsym(root, b"main\0".as_ptr().cast()).is_null());
        assert!(!dlsym(root, b"write\0".as_ptr().cast()).is_null());
        assert_eq!(dlclose(root), 0);
        assert_eq!(dl_base_count(), 1);
        let base = load(b"libdl_base.so.1\0", NOW);
        assert_eq!(value(base, b"dl_base_count\0"), 1);
        assert_eq!(dlclose(base), 0);
        assert_eq!(dl_base_count(), 1);
        for _ in 0..3 {
            assert!(dlopen(b"libdl_bad.so.1\0".as_ptr().cast(), NOW).is_null());
            error_once();
        }
        let system = load(b"libscope_sys.so\0", NOW);
        assert_eq!(value(system, b"sys_report\0"), 777);
        assert_eq!(value(system, b"sys_ctor_count\0"), 1);
        let same = load(b"/system/lib/libscope_sys.so.1\0", NOW);
        assert_eq!(same, system);
        assert_eq!(dlclose(system), 0);
        assert_eq!(value(same, b"sys_ctor_count\0"), 1);
        assert_eq!(dlclose(same), 0);
        let fresh = load(b"libscope_sys.so\0", NOW);
        assert_ne!(fresh, system);
        assert_eq!(value(fresh, b"sys_ctor_count\0"), 1);
        assert_eq!(dlclose(fresh), 0);
    }
    print(b"DLOPEN_FAILURES_OK\n");
}

fn reference_counts() {
    unsafe {
        let dep_fini = EVENTS[1].load(Ordering::SeqCst);
        let plugin_fini = EVENTS[3].load(Ordering::SeqCst);
        let first = load(b"libdl_plugin.so.1\0", NOW);
        assert_eq!(value(first, b"dl_plugin_value\0"), 42);
        assert_eq!(value(first, b"dl_dep_value\0"), 41);
        assert_eq!(value(first, b"dl_plugin_ctor_count\0"), 1);
        assert_eq!(value(first, b"dl_plugin_next\0"), 1);
        assert!(dlerror().is_null());
        assert!(dlsym(first, b"dl_absolute_zero\0".as_ptr().cast()).is_null());
        assert!(
            dlerror().is_null(),
            "a defined zero-valued symbol is not an error"
        );
        assert!(dlsym(first, b"dl_plugin_hidden\0".as_ptr().cast()).is_null());
        error_once();
        assert!(dlsym(DEFAULT, b"dl_plugin_value\0".as_ptr().cast()).is_null());
        error_once();
        let again = load(b"/apps/dlopen/lib/../lib/libdl_plugin.so.1\0", NOW);
        assert_eq!(first, again);
        assert_eq!(value(again, b"dl_plugin_next\0"), 2);
        assert_eq!(dlclose(first), 0);
        assert_eq!(value(again, b"dl_plugin_ctor_count\0"), 1);
        assert_eq!(EVENTS[3].load(Ordering::SeqCst), plugin_fini);
        assert_eq!(dlclose(again), 0);
        assert_eq!(EVENTS[3].load(Ordering::SeqCst), plugin_fini + 1);
        assert_eq!(EVENTS[1].load(Ordering::SeqCst), dep_fini + 1);

        // GLOBAL includes a dependency that was already loaded LOCAL.
        let dep = load(b"libdl_dep.so.1\0", NOW);
        let plugin = load(b"libdl_plugin.so.1\0", NOW | GLOBAL);
        assert_eq!(value(DEFAULT, b"dl_dep_value\0"), 41);
        assert_eq!(dlclose(dep), 0);
        assert_eq!(dlclose(plugin), 0);
        assert!(dlsym(DEFAULT, b"dl_dep_value\0".as_ptr().cast()).is_null());
        error_once();
        assert!(dlsym(first, b"dl_plugin_value\0".as_ptr().cast()).is_null());
        error_once();
        assert_eq!(dlclose(first), -1);
        error_once();
        let fresh = load(b"libdl_plugin.so.1\0", NOW);
        assert_ne!(fresh, first);
        assert_eq!(value(fresh, b"dl_plugin_next\0"), 1);
        assert_eq!(value(fresh, b"dl_plugin_ctor_count\0"), 1);
        assert_eq!(dlclose(fresh), 0);

        let dep = load(b"libdl_dep.so.1\0", NOW);
        let dep_fini = EVENTS[1].load(Ordering::SeqCst);
        let plugin = load(b"libdl_plugin.so.1\0", NOW);
        assert_eq!(dlclose(dep), 0);
        assert_eq!(EVENTS[1].load(Ordering::SeqCst), dep_fini);
        assert_eq!(value(plugin, b"dl_plugin_value\0"), 42);
        assert_eq!(dlclose(plugin), 0);
        assert_eq!(EVENTS[1].load(Ordering::SeqCst), dep_fini + 1);
    }
    print(b"DLOPEN_REFS_OK\n");
}

fn global_scope() {
    unsafe {
        let provider = load(b"libdl_provider.so.1\0", NOW);
        assert!(dlsym(DEFAULT, b"dl_runtime_provider\0".as_ptr().cast()).is_null());
        error_once();
        // LAZY is admitted but unresolved strong references still fail eagerly.
        assert!(dlopen(b"libdl_consumer.so.1\0".as_ptr().cast(), 1).is_null());
        error_once();
        let promoted = load(b"libdl_provider.so.1\0", NOW | GLOBAL);
        assert_eq!(provider, promoted);
        assert_eq!(value(DEFAULT, b"dl_runtime_provider\0"), 70);
        let root = dlopen(core::ptr::null(), NOW);
        assert_eq!(value(root, b"dl_runtime_provider\0"), 70);
        let consumer = load(b"libdl_consumer.so.1\0", NOW);
        assert_eq!(value(consumer, b"dl_consumer_value\0"), 77);
        assert_eq!(dlclose(provider), 0);
        assert_eq!(dlclose(promoted), 0);
        // The consumer's relocation keeps the now-unopened provider mapped.
        assert_eq!(value(consumer, b"dl_consumer_value\0"), 77);
        assert_eq!(dlclose(consumer), 0);
        assert!(dlsym(root, b"dl_runtime_provider\0".as_ptr().cast()).is_null());
        error_once();
        assert_eq!(dlclose(root), 0);
    }
    print(b"DLOPEN_GLOBAL_OK\n");
}

fn recursive_and_tls() {
    unsafe {
        let nested = load(b"libdl_nested.so.1\0", NOW);
        assert_eq!(value(nested, b"dl_nested_value\0"), 41);
        assert_eq!(EVENTS[4].load(Ordering::SeqCst), 1);
        assert_eq!(dlclose(nested), 0);
        assert_eq!(EVENTS[5].load(Ordering::SeqCst), 1);
        let tls = load(b"libdl_tls.so.1\0", NOW);
        assert_eq!(value(tls, b"dl_tls_next\0"), 8);
        assert_eq!(value(tls, b"dl_tls_next\0"), 9);
        assert_eq!(dlclose(tls), 0);
        let fresh = load(b"libdl_tls.so.1\0", NOW);
        assert_ne!(tls, fresh);
        assert_eq!(value(fresh, b"dl_tls_next\0"), 8);
        assert_eq!(dlclose(fresh), 0);
    }
    print(b"DLOPEN_RECURSIVE_TLS_OK\n");
}

extern "C" fn concurrent_worker(_: *mut c_void) -> *mut c_void {
    unsafe {
        assert!(dlerror().is_null(), "dlerror must be thread-local");
        let handle = load(b"libdl_tls.so.1\0", NOW);
        let previous = SHARED_HANDLE
            .compare_exchange(0, handle as usize, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|old| old);
        assert!(previous == 0 || previous == handle as usize);
        WORKERS_OPEN.fetch_add(1, Ordering::SeqCst);
        while WORKERS_OPEN.load(Ordering::SeqCst) != 4 {
            usleep(10_000);
        }
        assert_eq!(value(handle, b"dl_tls_next\0"), 8);
        assert_eq!(value(handle, b"dl_tls_next\0"), 9);
        assert!(dlsym(handle, b"missing_worker_symbol\0".as_ptr().cast()).is_null());
        error_once();
        assert_eq!(dlclose(handle), 0);
        core::ptr::null_mut()
    }
}

fn concurrent_and_errors() {
    unsafe {
        assert!(dlsym(DEFAULT, b"missing_main_symbol\0".as_ptr().cast()).is_null());
        let mut threads = [0usize; 4];
        for thread in &mut threads {
            assert_eq!(
                pthread_create(
                    thread,
                    core::ptr::null(),
                    concurrent_worker,
                    core::ptr::null_mut()
                ),
                0
            );
        }
        for thread in threads {
            assert_eq!(pthread_join(thread, core::ptr::null_mut()), 0);
        }
        error_once();
    }
    print(b"DLOPEN_THREADS_OK\n");
}

unsafe fn exchange_file(path: &[u8], bytes: &mut [u8], writing: bool) {
    let flags = if writing { 512 | 1024 | 1 } else { 0 };
    let fd = open(path.as_ptr().cast(), flags, 0o644i32);
    assert!(fd >= 0);
    let count = if writing {
        write(fd, bytes.as_ptr(), bytes.len())
    } else {
        read(fd, bytes.as_mut_ptr(), bytes.len())
    };
    assert_eq!(count, bytes.len() as isize);
    assert_eq!(close(fd), 0);
}

unsafe fn holding_app() {
    let handle = load(b"libdl_plugin.so.1\0", NOW);
    let mut bytes = (handle as usize).to_ne_bytes();
    exchange_file(b"/dlopen-handle\0", &mut bytes, true);
    loop {
        let fd = open(b"/dlopen-release\0".as_ptr().cast(), 0);
        if fd >= 0 {
            close(fd);
            break;
        }
        usleep(10_000);
    }
    assert_eq!(value(handle, b"dl_plugin_next\0"), 1);
    assert_eq!(dlclose(handle), 0);
    print(b"DLOPEN_OWNER_OK\n");
}

unsafe fn foreign_app() {
    let mut bytes = [0u8; core::mem::size_of::<usize>()];
    exchange_file(b"/dlopen-handle\0", &mut bytes, false);
    let foreign = usize::from_ne_bytes(bytes) as *mut c_void;
    assert!(dlsym(foreign, b"dl_plugin_value\0".as_ptr().cast()).is_null());
    error_once();
    assert_eq!(dlclose(foreign), -1);
    error_once();
    let own = load(b"libdl_plugin.so.1\0", NOW);
    assert_ne!(own, foreign);
    assert_eq!(value(own, b"dl_plugin_next\0"), 1);
    assert_eq!(dlclose(own), 0);
    print(b"DLOPEN_FOREIGN_OK\n");
}

#[no_mangle]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char, _: *const *const c_char) -> c_int {
    unsafe {
        if argc > 1 {
            let mode = **argv.add(1);
            match mode as u8 {
                b'h' => {
                    holding_app();
                    return 0;
                }
                b'f' => {
                    foreign_app();
                    return 0;
                }
                b'e' => {
                    EXIT_MODE.store(1, Ordering::SeqCst);
                    let handle = load(b"libdl_plugin.so.1\0", NOW);
                    assert_eq!(value(handle, b"dl_plugin_value\0"), 42);
                    assert_eq!(value(handle, b"dl_plugin_set_key\0"), 1);
                    let nested = load(b"libdl_nested.so.1\0", NOW);
                    assert_eq!(value(nested, b"dl_nested_value\0"), 41);
                    print(b"DLOPEN_EXIT_OPEN\n");
                    // Outstanding runtime handles must be finalized at exit.
                    return 0;
                }
                _ => {}
            }
        }
    }
    failures_and_startup();
    reference_counts();
    global_scope();
    recursive_and_tls();
    concurrent_and_errors();
    print(b"DLOPEN_ALL_OK\n");
    0
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    struct Writer;
    impl core::fmt::Write for Writer {
        fn write_str(&mut self, message: &str) -> core::fmt::Result {
            print(message.as_bytes());
            Ok(())
        }
    }
    use core::fmt::Write;
    let _ = writeln!(Writer, "DLOPEN_FAIL {info}");
    loop {}
}
