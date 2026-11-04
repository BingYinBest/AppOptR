# AGENTS.md

AppOptR（线程优化）：Android 应用线程 CPU 亲和性管理工具，以 Magisk/KernelSU 模块发布。核心是 Rust 守护进程（AppOpt），按 `applist.conf` 规则把目标应用进程/线程绑定到指定 CPU。

## 目录边界

- `AppOpt/` — 用户态主程序（bin 名 AppOpt，Rust 2024 edition）。`web/index.html` 经 `include_str!` 编译期内嵌，**改面板须改此文件并重新编译主程序**。
- `AppOpt-ebpf/` — 内核态 eBPF（aya-ebpf，no_std）。编译产物是 BPF ELF 对象，运行时由主程序在自身同目录按文件名 `AppOpt-ebpf` 加载，缺失自动回退 /proc 轮询。
- `module/` — 模块包：`module.prop` 是版本真相来源；`bin/<arch>/` 放预编译产物；`customize.sh`/`service.sh` 是安装/自启脚本；`webroot/index.html` 只是跳转到 127.0.0.1:8889 的入口页（**面板本体唯一在 `AppOpt/web/index.html`**，勿改这里）；`applist.conf` 是随包示例规则，安装时与旧配置合并（见 `customize.sh::copy_default_rules`）。
- `modules_update/` — 发布 zip（`线程优化vX.Y.Z.zip`）与 `changelog.md`；**zip 与 `module/bin/` 预编译产物均随 git 提交入库**。

## 构建与验证

- 工具链：rustup 管理，stable 1.99.1 + nightly 1.101.0（aarch64 host），`~/.cargo/bin` 需经 `source ~/.cargo/env` 或 `bash -c 'source scripts/env.sh && ...'` 进入 PATH。host 单测/release 可直接跑；交叉编译与 eBPF 由 `scripts/env.sh` 提供，`build.sh`/`build-android.sh`/`build-ebpf.sh`/`package.sh` 内部均 `source env.sh`。
- host 单测：`cd AppOpt && cargo test`（当前 83 个用例全绿，覆盖配置解析/规则匹配/文本编辑/cpuset/缓存，不依赖 Android target 与真机）。
- lint：`cd AppOpt && cargo clippy --all-targets -- -D warnings`（clippy 组件已装；新环境若 minimal profile 缺组件先 `rustup component add clippy`）。
- `./build.sh` = host 回归 → aarch64/x86_64 交叉编译 → eBPF 字节码 → 打包 zip 到 `modules_update/`，产物自动覆盖 `module/bin/`。主程序必须 64 位（32 位 target 触发 `compile_error!`，依赖 `cpu_set_t` 布局）。
- `AppOpt-ebpf` 按 aya 标准用 nightly 以 `bpfel-unknown-none` + `-Z build-std=core` 编译（nightly 需带 rust-src 组件，build-ebpf.sh 依赖它），不是可执行文件；release `opt-level=2`（旧 `s` 不再被新版 bpf-linker 的 LLVM 接受，勿改回）。
- `scripts/env.sh` 已适配本容器（NDK 在 `/root/android-sdk/ndk/30.0.16248370`，prebuilt 目录 `linux-arm64`，bpf-linker 在 `~/.cargo/bin`，可用 `BPF_LINKER` 环境变量覆盖），无需手动适配。
- Cargo.toml 版本规则：`AppOpt/Cargo.toml` 必须与模块版本一致（面板「版本」显示的是 CARGO_PKG_VERSION，当前均 2.4.0）；`AppOpt-ebpf/Cargo.toml` 与之独立（当前 0.3.3），按改动复杂度更新（大改升主/次版本，小改小幅升）。

## 发布规则

- 每次发布必须提升版本：`scripts/bump-version.sh`（无参 = versionCode+1；带参如 `2.4.0` = 改 version 且 versionCode+1），带参时**自动同步** `module/module.prop` 与 `AppOpt/Cargo.toml`；`modules_update/changelog.md` 手动补充；`AppOpt-ebpf/Cargo.toml` 按改动幅度独立升。
- 仅改版本时重跑 `scripts/package.sh` 即可重新打包，无需重编译；打包依赖系统 `zip`。`modules_update/` 相关命令含中文文件名，注意引号。

## 运行时行为

- `./applist.conf`（相对启动时工作目录）由 inotify 热加载：改规则或经 web 保存即时生效，无需重启；首次运行自动创建空配置。web 保存 / inotify / 轮询三入口共用 `config::LAST_MTIME` 短路去重，同一变更只会解析一次。
- CLI：`-c` 配置路径、`-s` 检查间隔（秒）、`-b` cpuset 目录名、`-w` 启用 Web（仅监听 127.0.0.1:8889）；`AppOpt.json` 存面板设置，CLI 参数优先。
- 更换 cpuset 目录名（CLI `-b` 与设置中名字不同，或 web 面板改名）会自动清除旧目录：先把旧目录 tasks 里的任务迁回 `/dev/cpuset` 根再 rmdir（含子目录，任务不迁出即删会 EBUSY），实现在 `cpuset.rs::remove_cpuset_dir`；无需手动清理。
- 模式三态：`0` 自动（eBPF 初始化失败即回退）、`1` 强制 eBPF（失败每 30s 重试）、`2` 强制 /proc。eBPF 依赖内核 BTF（`/sys/kernel/btf/vmlinux`）且同目录存在 `AppOpt-ebpf` 字节码，缺任一即回退——是正常行为不是 bug。
- `module/service.sh`（开机自启）除拉起 `AppOpt -w` 外，还会把每个 cpu 的 `core_ctl/min_cpus` 拉到 `max_cpus`（放开小核限制）；boot 等待上限 30×6s=180s；末尾有注释掉的绿厂 oiface / 米系 joyose 处理开关，需要时手动解注释。

## Web 面板（`AppOpt/web/index.html`）

- 容器内可直接试跑验证前端，无需真机：`cd AppOpt && ./target/debug/AppOpt -w -c <applist.conf>`；`/dev/cpuset` 不可写（容器/无 root）时仍正常启动，面板功能可用，只是亲和性不生效。端口固定 `8889`（`web.rs::WEB_PORT`），不可配置。
- 面板 HTML 经 `include_str!` 内嵌进二进制，改完必须 `cargo build`，只改文件不重编译等于没改。确认改动进包：`unzip -p modules_update/线程优化vX.Y.Z.zip bin/arm64-v8a/AppOpt | grep -a '<内嵌片段>'`。
- 状态页数字是滚动动画（`.num` 为滚动窗口、`.dig` 为 0-9 数字列，靠 `translateY(calc(-1em*var(--d)))` 定位）：窗口需 `align-items:flex-start` 让列顶对齐，用 `center` 会让整列居中、抵消 `--d` 偏移，导致数字错位或空白；改这块用多位数（如 `6314`）和个位数各验一次。

## 规则语法（与常见 cpuset 工具不同）

- 包级 `com.example=0-3`；子进程 `com.example:push=0-3`；线程 `com.example{RenderThread}=0-3`；块语法 `com.example=... { ... }` 内嵌包级与线程级规则。
- 语义核心 `e-core`/`m-core`/`p-core`/`hp-core`/`all-core`（按 cpufreq 频率分组推断：3 档芯片 e/p/h、4 档及以上 e/m/p/h 一一对应，2 档芯片仅 e/h，其余中间档并入 p），可逗号组合；注释以 `#` 或 `//` 开头。
- 动态规则（v2.3.0+）：`Thread-*=elect(档1 | 档2)` 组内负载最高线程绑档1、其余绑档2；`Thread-*=load(档1 | 档2 | 阈值)` 单线程负载>阈值% 绑档1、否则绑档2。动态规则**优先于同包静态线程规则**，组内线程不参与静态合并。

## 改代码必知的坑

- 内核 `comm` 限 15 字节：短包名走 `pkgs` HashSet 直接命中，长包名须读 `/proc/{pid}/cmdline` 确认包名避免截断误配（`rule_match.rs::comm_to_pkg`）；eBPF 白名单按「前 15 字节 + 末 15 字节」展开成两个 `[u8;15]` 键（`ebpf_mode.rs::comm_keys_build`），勿改成整包名字符串键。主程序侧长包名匹配已改为 `AppConfig::comm_prefixes/comm_suffixes` 哈希索引（仅包名 >15 字节时构建），勿回退成线性扫描。
- 动态规则两处坑（极易漏改）：动态规则包名必须并入 `AppConfig.pkgs`（否则进程不被跟踪、不进 eBPF 白名单）；扫描线程名（`tid_comm`）的条件必须同时含 `has_thread_rules || has_dyn_rules`，否则动态组永远匹配不到成员。
- **动态组成员索引**：`ProcCache` 的 `dyn_members`/`tid_group` 只在 `task_apply` 命中 `dyn_group_of` 时登记，`dyn_eval` 按该索引迭代（已非全表扫描）；直接改 `cache.tasks`（如写测试替身）必须同步登记索引，否则动态组评估为空。
- **proc 轮询状态**：`ProcScanState::tracked_pids` 是 `HashMap<i32, String>`（pid→包名缓存），每轮免读 cmdline，60s reconcile 周期重读复核；`proc_tasks` 只在线程集合变化时对新增 tid 读 comm（旧 tid 从缓存取）。`ProcCache::by_pid` 是 pid→tids 反向索引，维护点在 `task_apply`/`task_del`/`remove_pkgs`/`retain_pkg_tids`，改删除路径勿绕过。
- cpuset 目录创建幂等：目录存在且 `cpus`/`mems` 内容一致时跳过重写（重复写控制文件触发内核调度域重建/任务迁移）；比较按位图等价、兼容内核规范化输出（`0,1,2` → `0-2`）。`cfg.cpuset_dirs` 缓存类型为 `HashMap<CpuSet, String>`（`rule_match.rs::cpuset_dir_cached`）。
- `config.rs` 热加载：`LAST_MTIME` 为全局 AtomicI64，web 保存（`config_reload_now`）、inotify、轮询共用它实现 mtime 短路；`ParsedRule::Static` 已携带解析好的 `CpuSet`（勿在 add 时二次 `parse_cpu_spec`）。
- eBPF RingBuf 满载静默丢事件：`AppOpt-ebpf` 侧 `DROPPED_COUNT` map 计数，主程序 `ebpf_mode.rs::dropped_take` 消费并告警；丢失不致命（下次 fork/exec 重试），勿当成错误处理。
- 批量迁移走 `apply_affinity.rs::affinity_sync_batch`（cpuset tasks 按目录分组写），零散调用仍用 `affinity_set`：改动 `affinity_sync`（`cache.rs`）时两者语义要保持一致（先比 `sched_getaffinity` 短路、ESRCH 视为死亡、写 cpuset 失败仅告警不中断）。
- 规则按包名分桶索引 + 结果缓存短路（未变化跳过 apply），改匹配逻辑时保持这两点，勿按全量线性扫描重写。
- 核心分层与面板联动：档位映射唯一实现在 `cpuset.rs::assign_groups`（2 档仅 e/h、3 档 e/p/h、4 档 e/m/p/h、5+ 档次低进 m 其余进 p）。改分层后必须同步 `web/index.html` 的 `CORE_KEYS`/`topoOf`/`semanticVisible()` 与芯片按钮（m-core 仅 4 档+ 芯片显示、p-core 在 2 档芯片隐藏），否则面板按钮与后端解析不一致；m-core 在 <4 档芯片解析为空集，规则校验会拒掉该行。

## 当前上下文
- 项目根目录: ~/workspace
- 运行环境: 本地容器（PRoot）