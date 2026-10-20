# AGENTS.md

AppOptR（线程优化）：Android 应用线程 CPU 亲和性管理工具，以 Magisk/KernelSU 模块发布。核心是 Rust 守护进程（AppOpt），按 `applist.conf` 规则把目标应用进程/线程绑定到指定 CPU。

## 目录边界

- `AppOpt/` — 用户态主程序（bin 名 AppOpt，Rust 2024 edition）。`web/index.html` 经 `include_str!` 编译期内嵌，**改面板须改此文件并重新编译主程序**。
- `AppOpt-ebpf/` — 内核态 eBPF（aya-ebpf，no_std）。编译产物是 BPF ELF 对象，运行时由主程序在自身同目录按文件名 `AppOpt-ebpf` 加载，缺失自动回退 /proc 轮询。
- `module/` — 模块包：`module.prop` 是版本真相来源；`bin/<arch>/` 放预编译产物；`customize.sh`/`service.sh` 是安装/自启脚本；`webroot/index.html` 只是跳转到 127.0.0.1:8889 的入口页（**面板本体唯一在 `AppOpt/web/index.html`**，勿改这里）；`applist.conf` 是随包示例规则，安装时与旧配置合并（见 `customize.sh::copy_default_rules`）。
- `modules_update/` — 发布 zip（`线程优化vX.Y.Z.zip`）与 `changelog.md`；**zip 与 `module/bin/` 预编译产物均随 git 提交入库**。

## 构建与验证

- 容器/CI 的 PATH 不含 `~/.cargo/bin`，手动跑工具链命令先 `bash -c 'source scripts/env.sh && ...'`；`./build.sh` 顶层**不** source env.sh，直跑会在第 1 步 `cargo test` 报 not found，完整发布须 `bash -c 'source scripts/env.sh && ./build.sh'`。
- host 单测：`cd AppOpt && cargo test`（覆盖配置解析/规则匹配/文本编辑/cpuset/缓存，不依赖 Android target 与真机）。
- lint：`cd AppOpt && cargo clippy --all-targets -- -D warnings`（当前零警告，CI 同要求）。
- `./build.sh` = host 回归 → aarch64/x86_64 交叉编译 → eBPF 字节码 → 打包 zip 到 `modules_update/`，产物自动覆盖 `module/bin/`。主程序必须 64 位（32 位 target 触发 `compile_error!`，依赖 `cpu_set_t` 布局）。
- `AppOpt-ebpf` 按 aya 标准用 nightly 以 `bpfel-unknown-none` + `-Z build-std=core` 编译，不是可执行文件；release `opt-level=2`（旧 `s` 不再被新版 bpf-linker 的 LLVM 接受，勿改回）。
- 交叉编译环境已装好（rustup stable+nightly、NDK r30、bpf-linker v0.11.1），`scripts/env.sh` 统一配置并 `check_env` 校验，缺工具时按它的提示补齐。NDK 默认路径 `/root/android/ndk/android-ndk-r30`，可用 `NDK` 环境变量覆盖（CI 用的是 GitHub runner 自带 NDK）。
- CI（`.github/workflows/release.yml`）仅在推送 `v*` tag 或手动触发时运行：test job 跑 host 测试+clippy，package job 内联复现交叉编译；日常改动不触发 CI。**`scripts/` 与 `build.sh` 被 `.gitignore` 忽略、从未提交入库（工作区本地专属，新 clone 后不存在）——CI checkout 后没有 `scripts/package.sh`，package job 的打包步骤实际会失败；发布 zip 只在本地/容器产出，CI 只承担交叉编译验证。**
- **注意：`AppOpt/Cargo.toml` 的 crate 版本是历史遗留（当前 2.2.6），与模块版本（2.3.1）不同步；发布时只 bump 模块版本，勿顺手改 Cargo.toml。**（面板「版本」显示的是 CARGO_PKG_VERSION）

## 发布规则

- 每次发布必须提升版本：`scripts/bump-version.sh`（无参 = versionCode+1；带参如 `2.3.2` = 改 version 且 versionCode+1），并同步 `modules_update/changelog.md`。`bump-version.sh` 与 `build.sh` 的提示文案都提到「同步 README.md 版本信息」，但 README 实际无版本字段，无需动。
- 仅改版本时重跑 `scripts/package.sh` 即可重新打包，无需重编译；打包依赖系统 `zip`。

## 运行时行为

- `./applist.conf`（相对启动时工作目录）由 inotify 热加载：改规则或经 web 保存即时生效，无需重启；首次运行自动创建空配置。
- CLI：`-c` 配置路径、`-s` 检查间隔（秒）、`-b` cpuset 目录名、`-w` 启用 Web（仅监听 127.0.0.1:8889）；`AppOpt.json` 存面板设置，CLI 参数优先。
- 更换 cpuset 目录名（CLI `-b` 与设置中名字不同，或 web 面板改名）会自动清除旧目录：先把旧目录 tasks 里的任务迁回 `/dev/cpuset` 根再 rmdir（含子目录，任务不迁出即删会 EBUSY），实现在 `cpuset.rs::remove_cpuset_dir`；无需手动清理。
- 模式三态：`0` 自动（eBPF 初始化失败即回退）、`1` 强制 eBPF（失败每 30s 重试）、`2` 强制 /proc。eBPF 依赖内核 BTF（`/sys/kernel/btf/vmlinux`）且同目录存在 `AppOpt-ebpf` 字节码，缺任一即回退——是正常行为不是 bug。

## 规则语法（与常见 cpuset 工具不同）

- 包级 `com.example=0-3`；子进程 `com.example:push=0-3`；线程 `com.example{RenderThread}=0-3`；块语法 `com.example=... { ... }` 内嵌包级与线程级规则。
- 语义核心 `e-core`/`m-core`/`p-core`/`hp-core`/`all-core`（按 cpufreq 频率分组推断：3 档芯片 e/p/h、4 档及以上 e/m/p/h 一一对应，2 档芯片仅 e/h，其余中间档并入 p），可逗号组合；注释以 `#` 或 `//` 开头。
- 动态规则（v2.3.0+）：`Thread-*=elect(档1 | 档2)` 组内负载最高线程绑档1、其余绑档2；`Thread-*=load(档1 | 档2 | 阈值)` 单线程负载>阈值% 绑档1、否则档2。动态规则**优先于同包静态线程规则**，组内线程不参与静态合并。

## 改代码必知的坑

- 内核 `comm` 限 15 字节：短包名走 `pkgs` HashSet 直接命中，长包名须读 `/proc/{pid}/cmdline` 确认包名避免截断误配（`rule_match.rs::comm_to_pkg`）；eBPF 白名单按「前 15 字节 + 末 15 字节」展开成两个 `[u8;15]` 键（`ebpf_mode.rs::comm_keys_build`），勿改成整包名字符串键。
- 动态规则两处坑（极易漏改）：动态规则包名必须并入 `AppConfig.pkgs`（否则进程不被跟踪、不进 eBPF 白名单）；扫描线程名（`tid_comm`）的条件必须同时含 `has_thread_rules || has_dyn_rules`，否则动态组永远匹配不到成员。
- cpuset 目录创建幂等：目录存在且 `cpus`/`mems` 内容一致时跳过重写（重复写控制文件触发内核调度域重建/任务迁移）；比较按位图等价、兼容内核规范化输出（`0,1,2` → `0-2`）。
- Web `/api/rules` 中动态规则的 `spec` 是 `elect(...)`/`load(...)` 文本（`dyn:true`）；面板以图形化方式（模式/档位/阈值）新增与编辑动态规则；cpus 文本上限 128。
- 规则按包名分桶索引 + 结果缓存短路（未变化跳过 apply），改匹配逻辑时保持这两点，勿按全量线性扫描重写。
- 核心分层与面板联动：档位映射唯一实现在 `cpuset.rs::assign_groups`（2 档仅 e/h、3 档 e/p/h、4 档 e/m/p/h、5+ 档次低进 m 其余进 p）。改分层后必须同步 `web/index.html` 的 `CORE_KEYS`/`topoOf`/`semanticVisible()` 与芯片按钮（m-core 仅 4 档+ 芯片显示、p-core 在 2 档芯片隐藏），否则面板按钮与后端解析不一致；m-core 在 <4 档芯片解析为空集，规则校验会拒掉该行。