---
name: build-publish
description: AppOpt 发布流程已脚本化：./build.sh 一键（host 测试→双架构→eBPF→打包），scripts/ 分步脚本；交叉编译工具链已装（rustup/NDK clang/bpf-linker）
---
# AppOpt 构建与发布（已脚本化）

- 一键发布：仓库根 `./build.sh`（host cargo test+release → scripts/build-android.sh → scripts/build-ebpf.sh → scripts/package.sh 打 zip 到 modules_update/）。
- 分步脚本在 `scripts/`：env.sh（公共环境+检测）、build-android.sh、build-ebpf.sh、package.sh、bump-version.sh（versionCode+1，可带版本参数）。
- CI：.github/workflows/release.yml（tag v* 或手动触发，runner 自带 NDK）。
- 交叉编译工具链（已装于本容器）：rustup 在 ~/.cargo（stable + nightly/rust-src），Android target aarch64/x86_64-linux-android 已 add；bpf-linker v0.11.1 在 ~/.local/bin；链接器用 NDK r30 的 aarch64/x86_64-linux-android24-clang（/root/android/ndk/android-ndk-r30/toolchains/llvm/prebuilt/linux-x86_64/bin）。
- eBPF 编译：`cargo +nightly build --release --target bpfel-unknown-none -Z build-std=core`（env.sh 已 export CARGO_TARGET_BPFEL_UNKNOWN_NONE_LINKER）。
- 注意：AppOpt-ebpf 的 [profile.release] opt-level 必须用整数 2（原 "s" 会被新版 bpf-linker 的 LLVM 拒绝报 -Os 不支持；字符串 "2" 在该 nightly cargo 也会被误报非法，整数 2 正常）。勿改回 "s"。
- 发布仍需人工同步：module/module.prop 版本、changelog.md、README.md。

# AppOpt 测试

- 单元测试（AppOpt 目录 `cargo test`，58 项）：config 解析纯函数（parse_rules/parse_outer/spec_like 等）、cpuset 解析（parse_cpu_spec/to_range_string）、rule_match 匹配、rule_edit 文本增删改（用 /tmp 临时文件）。
- parse_rules 是纯函数（不触盘），fixture 用 include_str!("../../module/applist.conf")。
- 测试拓扑一律 cpuset_enabled=false 避免碰 /dev/cpuset。
- host 编译注意：libc::inotify_rm_watch 第二参数 Android 是 u32、Linux 是 i32，交叉平台写法用 `as _`。

# AppOpt 性能优化要点（已落地）

- AppConfig 含索引：`rules_by_pkg`（pkg→规则下标）、`exact_thread_rules`（pkg→精确线程名→规则，仅无 fnmatch 元字符）、`cpuset_dirs: Mutex<HashSet>`（已建 cpuset 目录缓存，避免重复 mkdir/chmod/chown/写文件）。新增字段需在 load_config 与测试 helper 同步构造。
- `thread_affinity` 走索引：精确线程名 O(1)，通配符规则按包扫描（pattern_has_meta 检测 `*?[`）；有等价性对照测试 `indexed_matches_linear_reference`。
- `affinity_set` 的 cpuset tasks 路径用栈缓冲（256B）零堆分配。
- proc 模式 `cache_sync` 已移除逐 pid kill 探活：sysinfo 进程数减少触发全扫，死线程由 affinity_sync 的 ESRCH 清理兜底（最迟 5×interval）。
- eBPF 事件 channel 为 `mpsc::Receiver<Vec<EbpfProcEvent>>` 批量传输，接收端逐条 event_dispatch。
- web suggest 有 3s TTL 缓存（SUGGEST_PKG_CACHE / SUGGEST_THREAD_CACHE，静态用 BTreeMap 因 HashMap::new 非 const）。