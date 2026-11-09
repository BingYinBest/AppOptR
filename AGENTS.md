# AGENTS.md

AppOptR（线程优化）：Android 应用线程 CPU 亲和性管理工具，以 Magisk/KernelSU 模块发布。
核心是 Rust 守护进程 `AppOpt`，按 `applist.conf` 规则把目标应用进程/线程绑定到指定 CPU 核心簇。

这份文档是仓库唯一的开发约定来源。

## 1. 目录与模块边界

```
AppOpt/            用户态主程序（lib + bin，Rust 2024 edition）
  src/lib.rs       库入口：常量、全局标志、领域模块声明
  src/main.rs      可执行入口，只调用 daemon::run()
  src/cli.rs       命令行解析（-c/-s/-b/-w/-v/-h）
  src/daemon.rs    守护进程编排：装配配置/拓扑/两种扫描后端，主循环
  src/config/      配置domain：mod.rs（类型+构建）、parse.rs（纯语法解析）、reload.rs（inotify/热加载）
  src/engine/      cache.rs（进程缓存与索引）、proc.rs（/proc 轮询）、ebpf.rs（事件驱动）、dynamic.rs（动态规则）、hot.rs（内置突发接管）
  src/platform/    cpuset.rs（内核 cpuset 文件与拓扑分档）、affinity.rs（读写 proc 与下发亲和性）
  src/rules/       matcher.rs（规则匹配）、editor.rs（规则文本增删改）
  src/web/         mod.rs（HTTP 服务与 REST 路由）、settings.rs（AppOpt.json）
  web/index.html   面板本体，经 include_str! 编译期内嵌
AppOpt-ebpf/      内核态 eBPF（aya-ebpf，no_std）
  src/main.rs      四个 tracepoint 挂载点与 panic handler
  src/maps.rs      ABI 结构体（ProcEvent/TracepointOffsets）与全部 map
  src/events.rs    事件组装与上报判断
module/           模块包
  module.prop      版本真相来源
  bin/<arch>/      预编译产物（arm64-v8a / x86_64），安装时平铺到模块根目录
  customize.sh     安装脚本（由安装器 source，不能 set -e）
  service.sh       开机自启 + core_ctl 放开小核限制
  webroot/index.html  跳转到 127.0.0.1:8889 的入口页（面板本体不在这里）
  applist.conf     随包示例规则，安装时旧配置优先
modules_update/    历史发布 zip 与 changelog.md
```

- `AppOpt` 是单进程单写入者模型：主线程独占 `ProcCache`，配置线程 / eBPF reader / Web 连接线程
  只通过 `Arc<AppConfig>` 快照与 `mpsc` 把数据交给主线程；唯一入口是 `ProcCache::task_apply`。
- 面板 HTML 改动必须重新编译主程序才生效，只改文件等于没改。

## 2. 构建与验证

仓库不再提供构建脚本，命令直接可跑；两个 crate 的 `Cargo.lock` 随仓库提交，保证构建可复现。

```bash
cd AppOpt && source ~/.cargo/env
cargo test            # 94 个用例，不依赖 Android target 与真机
cargo build --release
cargo clippy --all-targets -- -D warnings   # 组件未安装时先 rustup component add clippy
```

- 聚焦单个用例：`cargo test <模块路径或用例名>`，例如 `cargo test config::tests`、`cargo test editor`。
- 主程序必须 64 位：32 位 target 会触发 `compile_error!`（`cpu_set_t` 二进制布局假设）。
- 交叉编译（需自行准备 NDK 与 cargo 链接器配置）：

```bash
rustup target add aarch64-linux-android x86_64-linux-android
export NDK=$HOME/ndk/android-ndk-r27c
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android24-clang"
cargo build --release --target aarch64-linux-android
cargo build --release --target x86_64-linux-android
```

  链接器路径随 NDK 版本与宿主架构变化：解压后确认 `toolchains/llvm/prebuilt/` 下的真实目录名，
  不要照抄 `linux-x86_64`。

- eBPF 产物（架构无关，aya 标准流程）：

```bash
cd AppOpt-ebpf
export CARGO_TARGET_BPFEL_UNKNOWN_NONE_LINKER="$HOME/.cargo/bin/bpf-linker"
cargo +nightly build --release --target bpfel-unknown-none -Z build-std=core
```

  需要 nightly 带 `rust-src`。`[profile.release]` 的 `opt-level` 必须是整数 `2`：
  字符串 `"s"` 会被新版 bpf-linker 的 LLVM 拒绝，字符串 `"2"` 也会被 nightly cargo 误报非法。
- 产物位置：`AppOpt/target/{aarch64,x86_64}-linux-android/release/AppOpt`、
  `AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf`。
- `module/bin/` 与 `modules_update/*.zip` 是历史构建产物：**改了源码就必须重建并覆盖**，
  否则包内跑的仍是旧实现（面板也还是旧版）。

## 3. 版本与发布

- 版本真相来源是 `module/module.prop` 的 `version` / `versionCode`；
  `AppOpt/Cargo.toml` 的 `version` 必须与之一致（面板「版本」显示 `CARGO_PKG_VERSION`）。
  `AppOpt-ebpf/Cargo.toml` 独立升级（当前 0.3.5）。
- 每次发布必须提升版本：
  - 大更新（新功能、规则语法变更、用户可见行为变化、架构调整）→ 抬大版本号（如 2.5.x → 2.6.0）
  - 小更新（bug 修复、内部加固、无用户可见变化）→ patch +1，`versionCode` +1
- 一次发布要同步：`module/module.prop`（version/versionCode）、`AppOpt/Cargo.toml`、
  `modules_update/changelog.md`、`README.md`（如含版本字段）。
- 打包：按上面的命令重建双架构主程序与 eBPF 对象，覆盖 `module/bin/<arch>/`，再用系统 `zip`
  把 `module/` 打成 `modules_update/线程优化vX.Y.Z.zip`。中文文件名注意引号。

## 4. 运行时行为

- `./applist.conf`（相对启动时工作目录）由 inotify 热加载：改规则或经 Web 保存即时生效，无需重启；
  首次运行自动创建空配置。Web 保存 / inotify / 轮询三入口共用 `config::LAST_MTIME` 做 mtime 短路，
  同一变更只解析一次。
- CLI：`-c` 配置路径、`-s` 检查间隔（秒）、`-b` cpuset 目录名、`-w` 启用 Web（仅监听 127.0.0.1:8889）、
  `-v` 版本、`-h` 帮助。`AppOpt.json` 存面板设置，CLI 参数优先。
- 更换 cpuset 目录名（CLI `-b` 或面板改名）会自动删除旧目录：先把旧目录 tasks 里的任务迁回
  `/dev/cpuset` 根再 rmdir（含子目录；任务不迁出即删会 EBUSY），实现在
  `platform/cpuset.rs::remove_cpuset_dir`，无需手工清理。
- 模式三态：`0` 自动（eBPF 初始化失败即回退）、`1` 强制 eBPF（失败每 30s 重试）、
  `2` 强制 /proc。eBPF 依赖内核 BTF（`/sys/kernel/btf/vmlinux`）且同目录存在 `AppOpt-ebpf`
  字节码，缺任一即回退——是正常行为，不是 bug（容器里看到「回退到 /proc 轮询」同理）。
- `service.sh`：等 `sys.boot_completed`（上限 30×6s）→ `cd` 到模块目录后拉起 `AppOpt -w`
  → 把每个 cpu 的 `core_ctl/min_cpus` 拉到 `max_cpus`。末尾有注释掉的绿厂 oiface /
  米系 joyose 开关，需要时手动解注释。

## 5. Web 面板

- 端口固定 `8889`（`web/mod.rs::WEB_PORT`），不可配置；只监听回环地址，并校验 Host/Origin。
  `MAX_CONNS=64`、`MAX_BODY_BYTES=16384`、suggest 查询上限 64 字符、suggest 结果缓存 3s（最多 16 项）。
- 面板 3s 轮询 `/api/status`，服务端 `WEB_STATS` 每 2s 更新一次，因此数字最多滞后约 5s。
- `WEB_STATS` 由 `daemon.rs::update_web_stats` 填充；规则数 = 静态规则 + 动态规则。
- 数字滚动动画：`.num` 是滚动窗口、`.dig` 是 0-9 数字列，靠 `translateY(calc(-1em*var(--d)))` 定位。
  窗口必须 `align-items:flex-start`；用 `center` 会让整列居中、抵消 `--d` 偏移，数字会错位或空白。
  每列 20 行（0-9 两轮）：递减时先跳 `cur+10` 再动画到 `d+10`，动画结束归位 `d`，
  否则会出现空白帧。改这块用多位数（如 `6314`）和个位数各验一次。
- 面板结构：状态页（运行时长/核心拓扑/规则与覆盖/进程与绑定）、规则页（搜索 + 应用分组可展开）、
  设置页（模式/扫描间隔/cpuset/配置文件/关于），规则编辑是底部弹层。
- 拓扑条按各档核数分宽，档位可见性由 `semanticVisible()` 决定：中间档与两端相同时隐藏，
  避免 2 档芯片重复展示。改分档口径必须同步 `CORE_KEYS`/`topoOf`/`semanticVisible()`
  与芯片按钮，否则面板与后端解析不一致。
- 容器内可直接试跑：`cd AppOpt && ./target/debug/AppOpt -w -c <applist.conf>`；
  无 `/dev/cpuset`、无 cpufreq 时仍能启动，面板功能可用，只是亲和性不生效、档位为空。

## 6. 规则语法（与常见 cpuset 工具不同）

- 包级 `com.example=0-3`；子进程 `com.example:push=0-3`；线程 `com.example{RenderThread}=0-3`；
  块语法 `com.example=... { ... }` 内嵌包级与线程级规则。
- 语义核心 `e-core`/`m-core`/`p-core`/`hp-core`/`all-core`，可逗号组合；注释以 `#` 或 `//` 开头。
  分档按 cpufreq 频率分组推断：2 档仅 e/hp、3 档 e/p/hp、4 档及以上 e/m/p/hp、5 档以上次低进 m 其余进 p。
- 动态规则：`Thread-*=elect(档1 | 档2)` 组内负载最高线程绑档1、其余绑档2；
  `Thread-*=load(档1 | 档2 | 阈值)` 单线程负载 > 阈值% 绑档1、否则档2。首个参数永远是高档。
  动态规则优先于同包静态线程规则，组内线程不参与静态合并。
- 包名/线程名不得含 `=`、`{`、`}`（配置按 `rfind('=')` 与块语法切分，含这些字符会解析错乱）；
  Web 接口 `token_ok` 会拒绝，文件里手写同样无效。
- 动态参数（`engine/dynamic.rs`）：采样周期 `DYN_PROBE=500ms`，与扫描间隔解耦；
  每线程维护 `fast`(权重 60%) / `slow`(权重 15%) 双 EWMA，`fast-slow` 为突发分。
  `load`：`fast > 阈值+5%` 或突发（`fast≥45%` 且突发分≥25）即升档；降档需 `fast ≤ 阈值-5%`、
  突发分 <12、驻留 ≥6s。`elect`：常规换手领先 7% 且驻留 2s；突发接管要求挑战者 `fast≥70%`
  且为突发、现任驻留 1s；现任 `fast≤15%`、挑战者 `fast≥35%` 且领先 20% 时驻留满 1s 即可让位。
  降档还要求 `slow ≤ 阈值+5%`，防止 fast 单点下探导致误降。成员剪枝每 5s 一次，不是每轮。
- 内置突发接管（`engine/hot.rs`，与规则引擎解耦）：拓扑存在 `hp_core` 且不等于全部核心时生效，
  每 `HOT_PROBE=1000ms` 遍历 `cache.tasks`；`fast`(55%)/`slow`(12%) 双 EWMA；
  `fast≥60%` 且（`fast≥75%` 或突发分≥18）升档，驻留≥2s 且 `fast≤22%` 降档；
  并发上限 `HOT_MAX_THREADS=6`（单轮扫描用 `planned` 计数，防同轮多候选一起超额），
  升档后每轮强制校正，降档优先走 `task_apply` 重算规则，
  规则不再命中则回落全核；采样间隔 >3×HOT_PROBE 时直接重置 EWMA。

- 真机调参：`APPOPT_TRACE=<path>` 环境变量开启 `src/trace.rs` 的 CSV 决策日志
  （列 `ts_ms,src,tid,fast,slow,action`，16 MiB 上限）；配套 `tools/trace_report.py`
  产出升档延迟、漏检段、弱升档、每小时换手率。日志只记录 `fast>=60` 的 hot 决策与 `fast>=45`
  的 dyn 候选，平时关闭时零文件写入。

## 7. 架构与数据流

- 主线程每轮：读 `CURRENT_CONFIG` 快照 → 配置变更则重建扫描状态 → 采集（eBPF 事件或 /proc 差分）
  → 周期性 `affinity_sync` → 每 500ms `dyn_eval` → 每 1s `hot_eval` → 每 2s 更新 `WEB_STATS`。
  `/proc` 路径把扫描间隔按 `DYN_PROBE` 切片睡眠，2s 间隔内也能按 500ms 粒度提级突发线程。
- `ProcCache` 持有 `tasks` / `by_pid` / `dyn_members` / `tid_group` / `hot` 五张表，
  维护点是 `task_apply` / `task_del` / `remove_pkgs` / `retain_pkg_tids`；改删除路径勿绕过。
- 性能结构：每轮成本 ≈ `200µs` 底噪 + `0.7–0.8µs × 纳管线程` + `5.7µs × 动态成员`；
  动态成员评估是主要周期开销。`hot_eval` 仅在存在 `hp_core` 时每 1s 读一次全量线程 stat
  （单线程读取约 6µs，256 线程 ≈ 0.16% CPU），无大核机型零成本。
- 已知控制面弱点（改 Web 安全策略前先读）：无鉴权，仅回环绑定 + Host/Origin 校验；
  删除线程规则不回收存量绑定（`engine/cache.rs::needs_update` 的 `!is_thread_rule && old.is_thread_rule`
  提前返回）；eBPF 与 /proc 模式切换会重建 `ProcCache`，两模式不共享缓存。
- 页面的 3 个 tab、展开分组、编辑弹层都是真实 JS 状态；改面板时同步更新截图/状态清单。

## 8. 改代码必知的坑

- **eBPF 事件语义**：线程创建（pthread_create）只触发 `sched_process_fork`，不触发 exec/rename；
  `engine/ebpf.rs::event_dispatch` 的 FORK 分支必须处理，否则新建线程永远不命中线程级规则
  （游戏「线程绑定无效」的典型症状）。用 tracepoint 的 `child_tgid` 区分线程 fork 与子进程 fork
  （子进程 exec 时再处理，勿按父进程包名误绑）；老内核无该字段时偏移注入为 0，用户态回退读
  `/proc/{tid}/status` 的 Tgid。
- 事件结构体 `EbpfProcEvent`（`engine/ebpf.rs`）与内核态 `ProcEvent`（`AppOpt-ebpf/src/maps.rs`）
  必须字段顺序一致、同为 `#[repr(C)]`，改一处要同步另一处。
- 内核 `comm` 限 15 字节：短包名走 `pkgs` HashSet 直接命中，长包名须读 `/proc/{pid}/cmdline` 确认包名
  避免截断误配（`rules/matcher.rs::comm_to_pkg`）；eBPF 白名单按「前 15 字节 + 末 15 字节」
  展开成两个 `[u8;15]` 键（`engine/ebpf.rs::comm_keys_build`），勿改成整包名字符串键。
  主程序侧长包名匹配用 `AppConfig::comm_prefixes/comm_suffixes` 哈希索引（仅包名 >15 字节时构建），
  勿回退成线性扫描。
- 动态规则两个必查点：动态规则包名必须并入 `AppConfig.pkgs`（否则进程不被跟踪、不进 eBPF 白名单）；
  扫描线程名（`tid_comm`）的条件必须同时含 `has_thread_rules || has_dyn_rules`，
  否则动态组永远匹配不到成员。
- 动态组成员索引：`dyn_members`/`tid_group` 只在 `task_apply` 命中 `dyn_group_of` 时登记，
  `dyn_eval` 按索引迭代（已非全表扫描）；直接改 `cache.tasks`（如写测试替身）必须同步登记索引。
  `apply_tier` 遇线程死亡走 `task_del`（会清 `by_pid`/`tid_group`），勿直接 `tasks.remove`。
- 动态组标识是 `config::dyn_group_id`（包名+线程模式派生的 FNV-1a 64 位），不能用规则下标——
  热加载后下标会变，会让成员与 elect 获胜者串组。删除规则后旧组由 `engine/dynamic.rs::prune_groups` 清理。
- cpuset tasks 句柄缓存：`platform/cpuset.rs::tasks_file` 缓存打开的 tasks 文件以省掉每次
  open/close；目录被删除或重建后必须调 `invalidate_tasks_files`
  （`create_cpuset_dir`/`remove_cpuset_dir`/`set_base_cpuset` 已内置），否则写入已删除的 inode 而静默失效。
- cpuset 目录创建幂等：目录存在且 `cpus`/`mems` 内容一致时跳过重写（重复写控制文件会触发内核调度域
  重建/任务迁移）；比较按位图等价，兼容内核规范化输出（`0,1,2` → `0-2`）。
  `cfg.cpuset_dirs` 缓存类型为 `HashMap<CpuSet, String>`（`rules/matcher.rs::cpuset_dir_cached`）。
- proc 轮询状态：`ProcScanState::tracked_pids` 是 `HashMap<i32, String>`（pid→包名缓存），每轮免读
  cmdline，60s reconcile 周期重读复核；`proc_tasks` 只在线程集合变化时对新增 tid 读 comm
  （旧 tid 从 `cache.tasks` 取）。`ProcCache::by_pid` 是 pid→tids 反向索引。
- 批量迁移走 `platform/affinity.rs::affinity_sync_batch`（cpuset tasks 按目录分组写），零散调用仍用
  `affinity_set`：改 `ProcCache::affinity_sync` 时两者语义要保持一致（先比 `sched_getaffinity` 短路、
  ESRCH 视为死亡、写 cpuset 失败仅告警不中断）。
- 规则按包名分桶索引 + 结果缓存短路（未变化跳过 apply），改匹配逻辑时保持这两点，勿按全量线性扫描重写。
- `config/reload.rs` 热加载：`LAST_MTIME` 为全局 AtomicI64，Web 保存（`config_reload_now`）、inotify、
  轮询共用它实现 mtime 短路；`ParsedRule::Static` 已携带解析好的 `CpuSet`，勿在 add 时二次 `parse_cpu_spec`。
- eBPF RingBuf 满载会静默丢事件：内核侧 `DROPPED_COUNT` 计数，主程序 `dropped_take` 消费并告警；
  丢失不致命（下次 fork/exec 重试），勿当错误处理。
- 平台差异：`libc::inotify_rm_watch` 第二参数在 Android 是 `u32`、Linux 是 `i32`，交叉写法用 `as _`。

## 9. 性能约定（已落地，勿回退）

- `AppConfig` 带索引：`rules_by_pkg`（pkg→规则下标）、`exact_thread_rules`（pkg→精确线程名→规则，
  仅无 fnmatch 元字符）、`cpuset_dirs`（已建目录缓存，避免重复 mkdir/chmod/chown/写文件）。
  新增字段需在 `load_config` 与测试 helper 同步构造。
- `thread_affinity` 走索引：精确线程名 O(1)，通配符规则按包扫描（`pattern_has_meta` 检测 `*?[`）；
  有等价性对照测试 `indexed_matches_linear_reference`。
- `affinity_set` 的 cpuset tasks 路径用 256B 栈缓冲，零堆分配。
- proc 模式已移除逐 pid kill 探活：sysinfo 进程数减少触发全扫，死线程由 `affinity_sync` 的 ESRCH 兜底
  （最迟 5×interval）。
- eBPF 事件通道是 `mpsc::Receiver<Vec<EbpfProcEvent>>` 批量传输，接收端逐条 `event_dispatch`。
- Web suggest 有 3s TTL 缓存（`SUGGEST_PKG_CACHE` / `SUGGEST_THREAD_CACHE`，静态用 `BTreeMap`
  因为 `HashMap::new` 不是 const）。

## 10. 代码风格

- 同一文件、同一逻辑块内不混用等价写法。例：测试断言空集统一用 `CpuSet::new()` 或 `set_of(&[])` 中的一个。
- 新增/修改前先看该文件与周边的命名、构造方式、惯用法并向其对齐，不自创第二套等价写法。
- 被指出写法不一致时直接统一，不辩解「本来等价 / 没必要」。
- 多处同类改动在一次提交内保持形态一致，不留局部特例。
- 模块职责按「变化原因」划分：解析、运行时、平台、规则、Web 各归其位；不为了模式名加层次。
- 只用模块已有的错误表达方式；告警走 `eprintln!` 并带上下文，不静默吞错。
- 代码注释一律用英文，不写中文注释；用户可见的说明文案（`ui_print`、CLI 帮助、面板文案）保持中文。

## 11. 测试

- `cd AppOpt && cargo test`，当前 94 个用例：配置解析纯函数（`parse_rules`/`parse_outer`/`spec_like` 等）、
  cpuset 解析（`parse_cpu_spec`/`to_range_string`）、规则匹配、规则文本增删改（用 /tmp 临时文件）、
  缓存与动态规则。
- `parse_rules` 是纯函数（不触盘），fixture 用 `include_str!("../../../module/applist.conf")`。
- 测试拓扑一律 `cpuset_enabled=false`，避免碰 `/dev/cpuset`。
- 动态规则用例依赖真实 CPU 负载采样，直接断言平局会 flaky——先隔离负载再断言（如暂停非 winner 线程）。
- 改模块路径后先跑全量测试；跨模块移动代码只允许「搬移 + 调整可见性」，逻辑改动单独提交。
