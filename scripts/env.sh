#!/bin/sh
# AppOpt 交叉编译公共环境：rustup toolchain + NDK clang 链接器 + bpf-linker
# 被其它 scripts 脚本 source 使用
set -u

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

NDK="${NDK:-/root/android/ndk/android-ndk-r30}"
NDK_BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
BPF_TARGET="bpfel-unknown-none"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android24-clang"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK_BIN/x86_64-linux-android24-clang"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar"
export CARGO_TARGET_X86_64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar"
export CARGO_TARGET_BPFEL_UNKNOWN_NONE_LINKER="$HOME/.local/bin/bpf-linker"

# 检测必备工具，缺失时打印安装提示并退出
check_env() {
    local missing=0
    command -v cargo >/dev/null 2>&1 || { echo "缺少 cargo（请先 rustup toolchain install stable）"; missing=1; }
    command -v rustup >/dev/null 2>&1 || { echo "缺少 rustup（https://rustup.rs）"; missing=1; }
    [ -x "$NDK_BIN/aarch64-linux-android24-clang" ] || { echo "缺少 NDK clang: $NDK_BIN/aarch64-linux-android24-clang"; missing=1; }
    [ -x "$NDK_BIN/x86_64-linux-android24-clang" ] || { echo "缺少 NDK clang: $NDK_BIN/x86_64-linux-android24-clang"; missing=1; }
    [ -x "$HOME/.local/bin/bpf-linker" ] || { echo "缺少 bpf-linker（下载 aya-rs/bpf-linker 预编译包到 ~/.local/bin）"; missing=1; }
    rustup target list --installed 2>/dev/null | grep -q '^aarch64-linux-android$' || { echo "缺少 target: rustup target add aarch64-linux-android"; missing=1; }
    rustup target list --installed 2>/dev/null | grep -q '^x86_64-linux-android$' || { echo "缺少 target: rustup target add x86_64-linux-android"; missing=1; }
    rustup toolchain list 2>/dev/null | grep -q nightly || { echo "缺少 nightly: rustup toolchain install nightly --component rust-src"; missing=1; }
    rustup component list --toolchain nightly 2>/dev/null | grep -q '^rust-src' || { echo "缺少 nightly rust-src（eBPF build-std 需要）"; missing=1; }
    if [ "$missing" -ne 0 ]; then
        echo ""
        echo "环境检查未通过，请先补齐上述工具。"
        exit 1
    fi
}