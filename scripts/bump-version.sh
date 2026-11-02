#!/bin/sh
# 提升模块版本：规则
#   bump-version.sh            → versionCode +1
#   bump-version.sh 2.2.7      → version 改为 2.2.7 且 versionCode +1
set -e

MODPROP="$(cd "$(dirname "$0")/.." && pwd)/module/module.prop"

NEW_VERSION="${1:-}"

CUR_CODE=$(grep -E '^versionCode=' "$MODPROP" | cut -d= -f2)
NEXT_CODE=$((CUR_CODE + 1))

if [ -n "$NEW_VERSION" ]; then
    sed -i "s/^version=.*/version=$NEW_VERSION/" "$MODPROP"
    echo "version: -> $NEW_VERSION"
    APP_CARGO="$(cd "$(dirname "$0")/.." && pwd)/AppOpt/Cargo.toml"
    if [ -f "$APP_CARGO" ]; then
        sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" "$APP_CARGO"
        echo "AppOpt/Cargo.toml version: -> $NEW_VERSION"
    fi
fi
sed -i "s/^versionCode=.*/versionCode=$NEXT_CODE/" "$MODPROP"
echo "versionCode: $CUR_CODE -> $NEXT_CODE"
echo "修改完成: $MODPROP（同步更新 modules_update/changelog.md；AppOpt-ebpf/Cargo.toml 按改动幅度独立升）"