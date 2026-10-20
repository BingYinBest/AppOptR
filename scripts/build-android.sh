#!/bin/sh
# 交叉编译主程序（AppOpt）到 aarch64 与 x86_64 Android 目标
set -e
. "$(dirname "$0")/env.sh"
check_env

cd "$(dirname "$0")/../AppOpt"

echo "==> 编译 aarch64-linux-android"
cargo build --release --target aarch64-linux-android

echo "==> 编译 x86_64-linux-android"
cargo build --release --target x86_64-linux-android

echo "==> 产物:"
ls -la target/aarch64-linux-android/release/AppOpt target/x86_64-linux-android/release/AppOpt