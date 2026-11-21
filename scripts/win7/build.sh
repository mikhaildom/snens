#!/usr/bin/env bash
# Сборка snens под Windows 7: таргет x86_64-win7-windows-gnu (tier 3 → build-std).
#
# Нужно на машине сборки (Windows x64):
#   rustup toolchain install nightly-x86_64-pc-windows-gnu --profile minimal -c rust-src
#     — GNU-хост: build-скрипты линкуются его собственным линкером, MSVC не нужен;
#   w64devkit (https://github.com/skeeto/w64devkit) — GCC для C-кода (ring,
#     sqlite, sqlite-vec, tree-sitter) и итоговой линковки. Путь — $W64DEVKIT.
#
# Никаких target-cpu=native: Win7-машины часто без SSE4/AVX (AMD E-300).
# C-код SIMD сам не включает: sqlite-vec — только по SQLITE_VEC_ENABLE_AVX,
# ring выбирает реализацию по CPUID во время работы.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
W64DEVKIT="${W64DEVKIT:-$REPO/../toolchain/w64devkit}"
[ -x "$W64DEVKIT/bin/gcc" ] || { echo "нет gcc в $W64DEVKIT/bin — задай W64DEVKIT" >&2; exit 1; }
SHIM="$REPO/target/win7-shim"
mkdir -p "$SHIM"
# В w64devkit нет libgcc_eh.a: раскрутка стека (_Unwind_*) лежит в libgcc.a.
# rustc всегда просит -lgcc_eh — подкладываем пустой архив (и хосту, и цели).
[ -f "$SHIM/libgcc_eh.a" ] || "$W64DEVKIT/bin/ar" rcs "$SHIM/libgcc_eh.a"
export PATH="$W64DEVKIT/bin:$HOME/.cargo/bin:$PATH"
export LIBRARY_PATH="$(cygpath -w "$SHIM" 2>/dev/null || echo "$SHIM")"
export CC_x86_64_win7_windows_gnu=gcc CXX_x86_64_win7_windows_gnu=g++ AR_x86_64_win7_windows_gnu=ar
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_LINKER=gcc
# -static: libgcc/winpthread внутрь exe, рядом не нужны DLL MinGW.
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_RUSTFLAGS="-C target-cpu=x86-64 -C link-args=-static"
cd "$REPO"
cargo +nightly-x86_64-pc-windows-gnu build -Z build-std=std,panic_abort \
  --release --target x86_64-win7-windows-gnu --workspace "$@"
