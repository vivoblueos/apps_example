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

//! The `std` dynamic fixture.
//!
//! Every other Rust fixture in the tree is `no_std`: they prove the ELF and
//! relocation contract, but they do not exercise `std`'s own runtime. This one
//! does — `std` is linked statically into the PIE while `libc.so.1` stays
//! dynamic, so `lang_start`, the lazy runtime initializers, the global
//! allocator's `calloc`/`realloc`/`memalign` path and `std::io::stdout` all run
//! against the shared C library through the same loader contract the C and C++
//! fixtures use.
//!
//! It reached the tree only after `librs` grew the four symbols `std` actually
//! needs (`calloc`, `realloc`, `memalign`, `abort`); before that the link failed
//! on exactly those and nothing else.

fn main() {
    println!("rust-std: hello");

    // Heap: `Vec` growth goes through `realloc`, and `with_capacity` is the
    // path that reaches `calloc`-style zeroed allocation in std's allocator.
    let mut values: Vec<u32> = Vec::with_capacity(8);
    for index in 0..8u32 {
        values.push(index * 2);
    }
    println!(
        "rust-std: sum={} len={}",
        values.iter().sum::<u32>(),
        values.len()
    );

    // Formatting + a heap String.
    let joined = values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<String>>()
        .join(",");
    println!("rust-std: joined={}", joined);

    // std's own runtime state: this is what would fault first if `std`'s
    // initialization had not completed against the dynamic libc.
    println!("rust-std: thread={:?}", std::thread::current().id());
}
