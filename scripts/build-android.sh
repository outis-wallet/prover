#!/bin/sh
# The prover as a static library for 64-bit Android phones (API 28 and up), built with the NDK's
# clang: target/aarch64-linux-android/release/libos_wasm_probe.a. An app links it into its JNI
# library and calls the functions in crates/os_wasm_probe/include/outis_prover.h.
#
# `scripts/build-android.sh --bin [cargo args]` builds the command line instead,
# target/aarch64-linux-android/release/os_native_probe, to run on a phone over adb (e.g. with
# `--features gpu`: `adb push` it with captures/ to /data/local/tmp and run `os_native_probe replay
# captures/pay` there; copy captures/ with `cp -rL`, its casm files are symlinks).
#
# Needs the cairo-lang venv (README; VENV, default .venv) and the NDK (ANDROID_NDK_HOME, default
# $ANDROID_HOME/ndk/27.1.12297006; ANDROID_HOME defaults to ~/Android/Sdk on Linux and
# ~/Library/Android/sdk on macOS). The Rust toolchain and its Android target come from
# rust-toolchain.toml.
set -e
cd "$(dirname "$0")/.."
api=28
case "$(uname -s)" in
  Linux) host=linux-x86_64; default_sdk="$HOME/Android/Sdk" ;;
  Darwin) host=darwin-x86_64; default_sdk="$HOME/Library/Android/sdk" ;; # x86_64 binaries run on Apple silicon too
  *) echo "build-android.sh: unsupported host $(uname -s)" >&2; exit 1 ;;
esac
sdk="${ANDROID_HOME:-$default_sdk}"
ndk="${ANDROID_NDK_HOME:-$sdk/ndk/27.1.12297006}"
bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
[ -x "$bin/aarch64-linux-android$api-clang" ] || { echo "No NDK clang in $bin (set ANDROID_HOME or ANDROID_NDK_HOME)" >&2; exit 1; }
venv="${VENV:-.venv}"
[ -f "$venv/bin/activate" ] || { echo "No cairo-lang venv at $venv (see README, or set VENV)" >&2; exit 1; }
. "$venv/bin/activate"

export CC_aarch64_linux_android="$bin/aarch64-linux-android$api-clang"
export CXX_aarch64_linux_android="$bin/aarch64-linux-android$api-clang++"
export AR_aarch64_linux_android="$bin/llvm-ar"
export RANLIB_aarch64_linux_android="$bin/llvm-ranlib"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$bin/aarch64-linux-android$api-clang"
# tikv-jemalloc-sys asks the linker for libgcc, which NDK r23 and later no longer ship (their
# unwinder is libunwind): a one-line linker script stands in for it. An app linking the static
# library with the NDK puts the same directory on its library path, or links -lunwind for -lgcc.
shim="$PWD/target/android-libgcc"
mkdir -p "$shim"
echo 'INPUT(-lunwind)' > "$shim/libgcc.a"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-L native=$shim"
if [ "$1" = --bin ]; then
  shift
  RUSTC_WRAPPER= cargo build --release --target aarch64-linux-android -p os_wasm_probe \
    --bin os_native_probe "$@"
  ls -l target/aarch64-linux-android/release/os_native_probe
else
  RUSTC_WRAPPER= cargo rustc --release --target aarch64-linux-android -p os_wasm_probe --lib \
    --crate-type staticlib "$@"
  ls -l target/aarch64-linux-android/release/libos_wasm_probe.a
fi
