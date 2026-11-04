#!/bin/sh
# 提升模块版本：
#   bump-version.sh          → 仅 versionCode +1
#   bump-version.sh 2.5.1    → version 改为 2.5.1 且 versionCode +1
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MODPROP="$ROOT/module/module.prop"
APP_CARGO="$ROOT/AppOpt/Cargo.toml"

NEW_VERSION="${1:-}"

if [ -n "$NEW_VERSION" ]; then
    printf '%s' "$NEW_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || {
        echo "版本号格式非法: $NEW_VERSION（应形如 2.5.1）"
        exit 1
    }
fi

CUR_VERSION="$(sed -n 's/^version=//p' "$MODPROP" | head -n 1)"
CUR_CODE="$(sed -n 's/^versionCode=//p' "$MODPROP" | head -n 1)"
[ -n "$CUR_VERSION" ] && [ -n "$CUR_CODE" ] || {
    echo "module.prop 缺少 version/versionCode"
    exit 1
}
NEXT_CODE=$((CUR_CODE + 1))

if [ -n "$NEW_VERSION" ]; then
    sed -i "s/^version=.*/version=$NEW_VERSION/" "$MODPROP"
    sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" "$APP_CARGO"
    echo "version: $CUR_VERSION -> $NEW_VERSION（module.prop 与 AppOpt/Cargo.toml 已同步）"
fi

sed -i "s/^versionCode=.*/versionCode=$NEXT_CODE/" "$MODPROP"
echo "versionCode: $CUR_CODE -> $NEXT_CODE"
echo "完成。后续：补充 modules_update/changelog.md，按需提升 AppOpt-ebpf 版本，再运行 scripts/package.sh 重新打包"
