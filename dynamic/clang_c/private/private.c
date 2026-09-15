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

// The private DSO of the C fixture. It has no SONAME requirement of its own —
// the loader keys it by the path the application resolved it to — and it is
// deliberately plain C: no runtime library, only the `libc.so.1` symbols the
// root also imports.
//
// `private_state` is written by the constructor and read afterwards by both
// exported functions, so the root's reads exercise `R_ARM_GLOB_DAT`/`ABS32`
// against a DSO-owned data object rather than only against functions.

extern int printf(const char *format, ...);

static int private_state;

__attribute__((constructor)) static void clang_c_private_constructor(void) {
  private_state = 37;
  printf("clang-c: private ctor state=%d\n", private_state);
}

__attribute__((destructor)) static void clang_c_private_destructor(void) {
  printf("clang-c: private fini state=%d\n", private_state);
}

int clang_c_private_base(void) { return private_state; }

int clang_c_private_value(int input) { return private_state + input; }
