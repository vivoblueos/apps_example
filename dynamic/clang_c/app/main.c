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

// The C fixture's root: a freestanding C PIE whose whole runtime is the shared
// `libc.so.1`, plus one private DSO. `blueos_scrt1::_start` is the ELF entry
// and reaches this image-local `main` through its GOT slot.
//
// This exists because every other dynamic fixture in the tree is Rust or
// freestanding C++, which left "can the loader run a plain C program?" as an
// inference rather than a result. The answer it is here to pin down is yes:
// nothing in the loader is language-specific, and C needs no runtime beyond the
// libc surface the other fixtures already use.
//
// It deliberately touches the parts of the C ABI the C++ fixture does not:
// `malloc`/`free` (heap across the DSO boundary), `strlen`/`memcpy`/`memcmp`
// (libc string routines reached by `BL` through the PLT), and reads of a data
// object owned by the private DSO.

#include <stddef.h>

extern int printf(const char *format, ...);
extern void *malloc(size_t size);
extern void free(void *ptr);
extern size_t strlen(const char *string);
extern void *memcpy(void *destination, const void *source, size_t count);
extern int memcmp(const void *left, const void *right, size_t count);

extern int clang_c_private_base(void);
extern int clang_c_private_value(int input);

// A file-scope object in the root's own data segment, so the image has a
// writable BSS word that only relocation + startup can have initialized.
static int root_generation;

__attribute__((constructor)) static void c_root_constructor(void) {
  root_generation = 1;
}

// Fill a heap block, read it back through a second view of it, and release it.
// Returns 1 only if the bytes survived the round trip.
static int heap_round_trip(void) {
  const size_t length = 64;
  char *buffer = malloc(length);
  if (buffer == NULL) {
    return 0;
  }
  for (size_t index = 0; index < length; index++) {
    buffer[index] = (char)(index + 1);
  }

  char mirror[64];
  memcpy(mirror, buffer, length);
  const int same = memcmp(mirror, buffer, length) == 0;
  free(buffer);
  return same;
}

int main(int argc, char *argv[], char *envp[]) {
  (void)envp;

  // The DSO's constructor ran before this one's body, so its state is already
  // visible through the exported accessor.
  const int base = clang_c_private_base();
  const int result = clang_c_private_value(5) + 7;

  const char *argument = argc > 1 ? argv[1] : "-";
  printf("clang-c: base=%d generation=%d\n", base, root_generation);
  printf("clang-c: result=%d argc=%d argv1=%s argv1len=%u\n", result, argc,
         argument, (unsigned)strlen(argument));
  printf("clang-c: heap ok=%d\n", heap_round_trip());

  // 49 == (37 + 5) + 7 when the DSO constructor ran exactly once and the
  // relocation of its data object landed on the right address.
  return result == 49 ? 0 : 1;
}
