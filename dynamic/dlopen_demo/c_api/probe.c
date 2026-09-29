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

// Match the existing freestanding C/C++ fixtures: declare the runtime's C ABI
// directly, without introducing a separate libc header distribution.
extern void *dlopen(const char *filename, int flags);
extern void *dlsym(void *handle, const char *symbol);
extern int dlclose(void *handle);
extern char *dlerror(void);

#define RTLD_NOW 0x2
#define RTLD_LOCAL 0
#define RTLD_DEFAULT ((void *)0)

int dlopen_c_api_probe(void) {
  void *handle = dlopen("libdl_provider.so.1", RTLD_NOW | RTLD_LOCAL);
  if (handle == RTLD_DEFAULT) {
    return 1;
  }
  unsigned long (*value)(void) =
      (unsigned long (*)(void))dlsym(handle, "dl_runtime_provider");
  if (value == 0 || dlerror() != 0 || value() != 70) {
    return 2;
  }
  if (dlclose(handle) != 0) {
    return 3;
  }
  if (dlsym(handle, "dl_runtime_provider") != 0 || dlerror() == 0 ||
      dlerror() != 0) {
    return 4;
  }
  return 0;
}
