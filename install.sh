#!/usr/bin/bash
# Android SDK/NDK + Rust Android targets + bpf-linker 一键安装
set -euo pipefail

# ================= 可调整配置 =================
ANDROID_HOME="${ANDROID_HOME:-$HOME/android/sdk}"
NDK_DIR="${NDK_DIR:-$HOME/android/ndk}"
CMDLINE_TOOLS_ZIP_URL="https://googledownloads.cn/android/repository/commandlinetools-linux-15859902_latest.zip"
SDK_ARM_URL="https://github.com/HomuHomu833/android-sdk-custom/releases/download/37.0.0/android-sdk-aarch64-linux-gnu.tar.xz"
NDK_ARM_URL="https://github.com/HomuHomu833/android-ndk-custom/releases/download/r30/android-ndk-r30-aarch64-linux-gnu.tar.xz"
BPF_LINKER_VERSION="v0.11.1"
# ==============================================

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

ARCH="$(uname -m)"
case "$ARCH" in
  aarch64|arm64)
    IS_AARCH64=true
    DEFAULT_JAVA="/usr/lib/jvm/java-26-openjdk-arm64"
    BPF_TARGET="aarch64-unknown-linux-musl"
    BPF_SHA="341ec1c595496877cae2b073544c2226d78a922739632b5732dbaa48507f1380"
    ;;
  x86_64|amd64)
    IS_AARCH64=false
    DEFAULT_JAVA="/usr/lib/jvm/java-26-openjdk-amd64"
    BPF_TARGET="x86_64-unknown-linux-musl"
    BPF_SHA=""   # 如需校验请自行填入 x86_64 资产的 sha256
    ;;
  *) die "不支持的架构: $ARCH" ;;
esac

JAVA_HOME="${JAVA_HOME:-$DEFAULT_JAVA}"
if [ "$(id -u)" -eq 0 ]; then SUDO=""; else SUDO="sudo"; fi

# ---------- 1. 基础依赖 ----------
if command -v apt-get >/dev/null 2>&1; then
  log "安装基础依赖 (curl/unzip/zip/zstd)..."
  $SUDO apt-get update -qq
  DEBIAN_FRONTEND=noninteractive $SUDO apt-get install -y -qq curl unzip zip zstd ca-certificates
else
  warn "未检测到 apt-get，请自行确保 curl/unzip/zip/zstd 已安装"
fi

# ---------- 2. Android command line tools ----------
log "安装 Android command line tools..."
mkdir -p "$ANDROID_HOME"
(
  cd "$ANDROID_HOME"
  curl -fL --retry 3 -O "$CMDLINE_TOOLS_ZIP_URL"
  tmpdir="$(mktemp -d)"
  unzip -q -o commandlinetools-linux-*.zip -d "$tmpdir"
  rm -rf cmdline-tools/latest
  mkdir -p cmdline-tools/latest
  mv "$tmpdir"/cmdline-tools/* cmdline-tools/latest/
  rm -rf "$tmpdir"
)

export ANDROID_HOME
export JAVA_HOME
export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"

[ -d "$JAVA_HOME" ] || warn "JAVA_HOME 不存在: $JAVA_HOME（sdkmanager 可能失败）"

# ---------- 3. 接受许可并安装 SDK 组件 ----------
log "接受 SDK 许可并安装 platform/build-tools..."
( set +o pipefail; yes | sdkmanager --licenses >/dev/null ) || true
( set +o pipefail; yes | sdkmanager --install \
    "platforms;android-37.0" \
    "build-tools;37.0.0" \
    "platform-tools" )

# ---------- 4. aarch64 原生 build-tools/platform-tools ----------
if $IS_AARCH64; then
  log "替换为 aarch64 原生 build-tools/platform-tools..."
  mkdir -p "$HOME/armtools37"
  (
    cd "$HOME/armtools37"
    curl -fL --retry 3 -O "$SDK_ARM_URL"
    tar -xJf android-sdk-aarch64-linux-gnu.tar.xz
  )
  cp -a "$HOME/armtools37/android-sdk/build-tools/37.0.0/." "$ANDROID_HOME/build-tools/37.0.0/"
  cp -a "$HOME/armtools37/android-sdk/platform-tools/." "$ANDROID_HOME/platform-tools/"
else
  log "非 aarch64，跳过 build-tools/platform-tools 替换"
fi

# ---------- 5. NDK ----------
if $IS_AARCH64; then
  log "安装自定义 aarch64 NDK r30..."
  mkdir -p "$NDK_DIR"
  (
    cd "$NDK_DIR"
    curl -fL --retry 3 -O "$NDK_ARM_URL"
    tar -xJf android-ndk-r30-aarch64-linux-gnu.tar.xz
  )
else
  warn "x86_64 请自行执行: sdkmanager --install \"ndk;<version>\""
fi

# ---------- 6. bpf-linker ----------
log "安装 bpf-linker ($BPF_TARGET)..."
mkdir -p "$HOME/.local/bin"
tmp_zst="$(mktemp --suffix=.tar.zst)"
curl -fL --retry 3 -o "$tmp_zst" \
  "https://github.com/aya-rs/bpf-linker/releases/download/${BPF_LINKER_VERSION}/bpf-linker-${BPF_TARGET}.tar.zst"
if [ -n "$BPF_SHA" ]; then
  echo "$BPF_SHA  $tmp_zst" | sha256sum -c -
else
  warn "该架构未内置 sha256，跳过校验"
fi
( cd "$HOME/.local/bin" && tar --zstd -xpf "$tmp_zst" )
rm -f "$tmp_zst"

# ---------- 7. Rust ----------
log "安装 Rust 工具链与 Android targets..."
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --no-modify-path
fi
rustup target add aarch64-linux-android x86_64-linux-android
rustup toolchain install nightly --component rust-src
rustup component add clippy

# ---------- 8. 生成环境变量文件 ----------
ENV_FILE="$HOME/.android-env.sh"
cat > "$ENV_FILE" <<EOF
export JAVA_HOME="$JAVA_HOME"
export ANDROID_HOME="$ANDROID_HOME"
export ANDROID_SDK_ROOT="\$ANDROID_HOME"
export PATH="\$ANDROID_HOME/cmdline-tools/latest/bin:\$ANDROID_HOME/platform-tools:\$HOME/.cargo/bin:\$HOME/.local/bin:\$PATH"
EOF

log "安装完成！"
cat <<EOF

请执行以下命令让环境变量生效（或加入 ~/.bashrc / ~/.zshrc）：
  source $ENV_FILE

关键路径：
  ANDROID_HOME = $ANDROID_HOME
  JAVA_HOME    = $JAVA_HOME
  NDK          = $NDK_DIR
  bpf-linker   = $HOME/.local/bin/bpf-linker
EOF