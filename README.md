# AppOptR

#### 介绍
Android 应用 CPU 亲和性管理工具

 **使用说明请参考** 

http://appopt.suto.top

### 模块说明

| 模块 | 功能 |
|------|------|
| `main.rs` | 程序入口，CLI 解析与双模式主循环编排 |
| `config.rs` | 配置解析、inotify 热加载与降级轮询 |
| `cpuset.rs` | CPU 拓扑检测与 cpuset 管理 |
| `rule_match.rs` | 规则匹配（包级/线程/动态组）与 comm 转包名 |
| `rule_edit.rs` | 规则增删改与文本归一化 |
| `dynamic.rs` | 动态规则（elect/load）负载评估与档位切换 |
| `apply_affinity.rs` | 亲和性应用 |
| `cache.rs` | 统一缓存 |
| `ebpf_mode.rs` | eBPF 事件驱动 |
| `proc_mode.rs` | Proc 轮询模式 |
| `web.rs` | 轻量 HTTP 管理面板服务 |
| `appopt-ebpf` | eBPF 内核态 |

### 命令行参数

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `-c <file>` | 指定配置文件路径 | `./applist.conf` |
| `-s <seconds>` | 检查间隔（秒，≥1） | `2` |
| `-b <name>` | 指定 BASE_CPUSET 目录名（不可含 `/`） | `AppOpt` |
| `-w` | 启用 Web 管理面板（仅监听 127.0.0.1:8889） | 关闭 |
| `-v` | 显示版本信息 | — |
| `-h` | 显示帮助 | — |

### 规则语法参考

规则配置文件默认 `./applist.conf`（可用 `-c` 指定），由 inotify 热加载，修改保存后即时生效。注释以 `#` 或 `//` 开头。

**包级规则**

```
com.example=0-3
com.example=e-core,p-core
```

将整个应用进程绑定到指定 CPU。右侧支持 CPU 编号（`0-3`、`0,2,4`）或语义核心（可逗号组合）。

**子进程规则**

```
com.example:push=0-3
```

包名带 `:子进程名` 限定子进程，如 `com.tencent.mm:push`。

**线程规则**

```
com.example{RenderThread}=0-3
```

将应用内指定线程绑定到 CPU，线程名支持 fnmatch 通配（`*`、`?`、`[...]`）。

**块语法**

```
com.example=0-3 {
    RenderThread=6-7
    Thread-1=0-5
}
```

块内为线程规则，块首可带包级规则（`com.example=... {` 形式）。

**语义核心**

按 cpufreq 频率分组推断档位：

| 名称 | 含义 |
|------|------|
| `e-core` | 能效小核 |
| `m-core` | 中频档（4 档及以上芯片的第二档） |
| `p-core` | 性能中核 |
| `hp-core` | 高性能大核 |
| `all-core` | 所有核心 |

可逗号组合使用，如 `e-core,p-core`、`p-core,hp-core`。2 档芯片仅 e/h，3 档芯片 e/p/h，4 档及以上 e/m/p/h 一一对应。

**动态规则（v2.3.0+）**

```
Thread-*=elect(6-7 | 0-5)    组内负载最高线程绑 6-7，其余绑 0-5
Thread-*=load(7 | 4-6 | 50)  单线程负载>50% 绑 7，否则绑 4-6
```

- `elect`：组内负载最高线程绑高档，其余绑低档；换人需超现任 20% 且驻留 5s
- `load`：负载 >阈值+5% 升高档、<阈值-5% 降低档（5% 滞回带），且驻留 5s
- 动态规则优先于同包静态线程规则，组内线程不参与静态合并
