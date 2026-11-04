#!/bin/sh
# AppOpt 一键发布流程：host 回归 → 双架构主程序 → eBPF → 打包模块 zip
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

. "$ROOT/scripts/env.sh"

command -v cargo >/dev/null 2>&1 || {
    echo "缺少 cargo（请先 rustup toolchain install stable）"
    exit 1
}

echo "==== 1/4 host 测试与构建验证 ===="
cd "$ROOT/AppOpt"
cargo test
cargo build --release
cd "$ROOT"

echo "==== 2/4 交叉编译主程序 ===="
sh "$ROOT/scripts/build-android.sh"

echo "==== 3/4 编译 eBPF 字节码 ===="
sh "$ROOT/scripts/build-ebpf.sh"

echo "==== 4/4 打包模块 ===="
sh "$ROOT/scripts/package.sh"

echo ""
echo "发布完成。若改了版本号，请确认 modules_update/changelog.md 已同步更新。"
