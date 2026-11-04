#!/bin/sh
# 发布打包：把编译产物放入 module/，打成 Magisk 模块 zip 到 modules_update/
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MODULE="$ROOT/module"
OUT_DIR="$ROOT/modules_update"

ARM_BIN="$ROOT/AppOpt/target/aarch64-linux-android/release/AppOpt"
X86_BIN="$ROOT/AppOpt/target/x86_64-linux-android/release/AppOpt"
EBPF_BIN="$ROOT/AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf"

echo "==> 校验产物"
for artifact in "$ARM_BIN" "$X86_BIN" "$EBPF_BIN"; do
    [ -f "$artifact" ] || {
        echo "缺少产物: $artifact（先运行 build-android.sh / build-ebpf.sh）"
        exit 1
    }
done

# 面板 HTML 编译期内嵌进主程序，产物里必须能找到，否则说明主程序没重新编译
grep -aq '<title>AppOpt</title>' "$ARM_BIN" || {
    echo "主程序未包含最新面板（缺少 <title>AppOpt</title>），请重新编译"
    exit 1
}

echo "==> 同步二进制到 module/bin"
cp -f "$ARM_BIN" "$MODULE/bin/arm64-v8a/AppOpt"
cp -f "$X86_BIN" "$MODULE/bin/x86_64/AppOpt"
cp -f "$EBPF_BIN" "$MODULE/bin/arm64-v8a/AppOpt-ebpf"
cp -f "$EBPF_BIN" "$MODULE/bin/x86_64/AppOpt-ebpf"

VERSION="$(sed -n 's/^version=//p' "$MODULE/module.prop" | head -n 1)"
VERSION_CODE="$(sed -n 's/^versionCode=//p' "$MODULE/module.prop" | head -n 1)"
[ -n "$VERSION" ] && [ -n "$VERSION_CODE" ] || {
    echo "module.prop 缺少 version/versionCode"
    exit 1
}

ZIP_NAME="线程优化v${VERSION}.zip"
ZIP_PATH="$OUT_DIR/$ZIP_NAME"

echo "==> 打包 $ZIP_NAME (versionCode=$VERSION_CODE)"
rm -f "$ZIP_PATH"
( cd "$MODULE" && zip -q -r "$ZIP_PATH" . )

echo "==> 校验 zip"
unzip -t "$ZIP_PATH" >/dev/null
unzip -l "$ZIP_PATH" | grep -E "AppOpt$|AppOpt-ebpf$|module.prop$" || {
    echo "zip 内容缺失关键文件"
    exit 1
}
echo "zip written: $ZIP_PATH"
