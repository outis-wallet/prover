#!/bin/sh
# The prover as a static library for 64-bit Android phones (API 28 and up), built with the NDK's
# clang: target/aarch64-linux-android/release/libos_wasm_probe.a. An app links it into its JNI
# library and calls the functions in crates/os_wasm_probe/include/outis_prover.h.
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
RUSTC_WRAPPER= cargo rustc --release --target aarch64-linux-android -p os_wasm_probe --lib \
  --crate-type staticlib "$@"
ls -l target/aarch64-linux-android/release/libos_wasm_probe.a
