#!/bin/sh
# AppOpt 一键发布流程：host 回归 → 双架构主程序 → eBPF → 打包模块 zip
set -e

cd "$(dirname "$0")"

chmod +x scripts/*.sh

echo "==== 1/4 host 测试与构建验证 ===="
cd AppOpt
cargo test
cargo build --release
cd ..

echo "==== 2/4 交叉编译主程序 ===="
scripts/build-android.sh

echo "==== 3/4 编译 eBPF 字节码 ===="
scripts/build-ebpf.sh

echo "==== 4/4 打包模块 ===="
scripts/package.sh

echo ""
echo "发布完成。若版本号有变化，请检查："
echo "  - module/module.prop 的 version/versionCode（可用 scripts/bump-version.sh 提升）"
echo "  - modules_update/changelog.md 更新日志"
echo "  - README.md 版本信息"