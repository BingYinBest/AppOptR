#!/bin/sh
# AppOpt 交叉编译公共环境：rustup toolchain + NDK clang 链接器 + bpf-linker
# 被其它 scripts 脚本 source 使用
set -u

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

NDK="${NDK:-/root/android-sdk/ndk/30.0.16248370}"
NDK_BIN="$NDK/toolchains/llvm/prebuilt/linux-arm64/bin"
BPF_LINKER="${BPF_LINKER:-$HOME/.cargo/bin/bpf-linker}"
BPF_TARGET="bpfel-unknown-none"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android24-clang"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK_BIN/x86_64-linux-android24-clang"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar"
export CARGO_TARGET_X86_64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar"
export CARGO_TARGET_BPFEL_UNKNOWN_NONE_LINKER="$BPF_LINKER"

# 累积缺失项，最后统一输出，避免逐条报错打断阅读
ENV_MISSING=""

need_cmd() {
    command -v "$1" >/dev/null 2>&1 ||
        ENV_MISSING="${ENV_MISSING}\n  - 缺少命令 $1：$2"
}

need_exec() {
    [ -x "$1" ] ||
        ENV_MISSING="${ENV_MISSING}\n  - 缺少可执行文件 $1：$2"
}

need_rust_target() {
    rustup target list --installed 2>/dev/null | grep -qx "$1" ||
        ENV_MISSING="${ENV_MISSING}\n  - 缺少 target $1：rustup target add $1"
}

check_env() {
    ENV_MISSING=""

    need_cmd cargo "请先 rustup toolchain install stable"
    need_cmd rustup "请先安装 https://rustup.rs"
    need_exec "$NDK_BIN/aarch64-linux-android24-clang" "请安装 Android NDK（NDK=$NDK）"
    need_exec "$NDK_BIN/x86_64-linux-android24-clang" "请安装 Android NDK（NDK=$NDK）"
    need_exec "$BPF_LINKER" "请安装 bpf-linker，或用 BPF_LINKER 指向现有路径"

    need_rust_target aarch64-linux-android
    need_rust_target x86_64-linux-android

    rustup toolchain list 2>/dev/null | grep -q nightly ||
        ENV_MISSING="${ENV_MISSING}\n  - 缺少 nightly 工具链：rustup toolchain install nightly --component rust-src"
    rustup component list --toolchain nightly 2>/dev/null | grep -q '^rust-src' ||
        ENV_MISSING="${ENV_MISSING}\n  - 缺少 nightly rust-src（eBPF build-std 需要）：rustup component add rust-src --toolchain nightly"

    if [ -n "$ENV_MISSING" ]; then
        printf "环境检查未通过，请先补齐：%b\n" "$ENV_MISSING"
        exit 1
    fi
}
