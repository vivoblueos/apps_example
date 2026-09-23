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

extern "C" int printf(const char *format, ...);

namespace {

int private_state;

class PrivateValue {
public:
  explicit constexpr PrivateValue(int base) : base_(base) {}

  constexpr int add(int value) const { return base_ + value; }

private:
  int base_;
};

__attribute__((constructor)) void initialize_private() {
  private_state = 37;
  printf("clang-cxx: private ctor state=%d\n", private_state);
}

__attribute__((destructor)) void finalize_private() {
  printf("clang-cxx: private fini state=%d\n", private_state);
}

} // namespace

extern "C" int clang_cxx_private_value(int input) {
  return PrivateValue(private_state).add(input);
}
