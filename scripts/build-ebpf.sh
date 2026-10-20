#!/bin/sh
# 编译内核态 eBPF 程序（AppOpt-ebpf，BPF ELF 对象，与设备架构无关）
set -e
. "$(dirname "$0")/env.sh"
check_env

cd "$(dirname "$0")/../AppOpt-ebpf"

echo "==> 编译 $BPF_TARGET (build-std=core)"
cargo +nightly build --release --target "$BPF_TARGET" -Z build-std=core

echo "==> 产物:"
ls -la target/"$BPF_TARGET"/release/AppOpt-ebpf