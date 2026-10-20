#!/bin/sh
# 发布打包：把编译产物放入 module/，打成 Magisk 模块 zip 到 modules_update/
set -e
. "$(dirname "$0")/env.sh"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODULE="$ROOT/module"
OUT_DIR="$ROOT/modules_update"

echo "==> 校验产物"
for f in \
    "$ROOT/AppOpt/target/aarch64-linux-android/release/AppOpt" \
    "$ROOT/AppOpt/target/x86_64-linux-android/release/AppOpt" \
    "$ROOT/AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf"; do
    [ -f "$f" ] || { echo "缺少产物: $f（先运行 build-android.sh / build-ebpf.sh）"; exit 1; }
done

echo "==> 同步二进制到 module/bin"
cp "$ROOT/AppOpt/target/aarch64-linux-android/release/AppOpt" "$MODULE/bin/arm64-v8a/AppOpt"
cp "$ROOT/AppOpt/target/x86_64-linux-android/release/AppOpt" "$MODULE/bin/x86_64/AppOpt"
cp "$ROOT/AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf" "$MODULE/bin/arm64-v8a/AppOpt-ebpf"
cp "$ROOT/AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf" "$MODULE/bin/x86_64/AppOpt-ebpf"

VERSION=$(grep -E '^version=' "$MODULE/module.prop" | cut -d= -f2)
VERSION_CODE=$(grep -E '^versionCode=' "$MODULE/module.prop" | cut -d= -f2)

ZIP_NAME="线程优化v${VERSION}.zip"
ZIP_PATH="$OUT_DIR/$ZIP_NAME"

echo "==> 打包 $ZIP_NAME (versionCode=$VERSION_CODE)"
rm -f "$ZIP_PATH"
( cd "$MODULE" && zip -q -r "$ZIP_PATH" . )
echo "zip written: $ZIP_PATH"

echo "==> 校验 zip"
unzip -t "$ZIP_PATH" >/dev/null && echo "zip 完整: $ZIP_PATH"
unzip -l "$ZIP_PATH" | grep -E "AppOpt$|AppOpt-ebpf|module.prop"