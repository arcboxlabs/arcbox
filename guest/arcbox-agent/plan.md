# arcbox-agent Sandbox Manager 集成方案

## 架构

### 层次结构

```
macOS（物理机）
└── Guest VM（Linux，通过 Virtualization.framework 启动）
    │
    ├── arcbox-agent        ← 本 crate，运行在 Guest VM 内
    │   ├── port 1024       ← 自定义二进制帧 RPC（Ping / SystemInfo / EnsureRuntime 等）
    │   └── SandboxManager  ← 待集成，管理嵌套 Firecracker sandbox
    │
    └── Firecracker sandboxes（嵌套 microVM，由 arcbox-vm 库管理）
        └── vm-agent        ← 运行在 sandbox 内，port 52 exec/run 服务端
```

### 关键边界

- **arcbox-agent** 运行在 Guest VM 内，是 sandbox 的管理者
- **arcbox-vm**（`hypervisor/arcbox-vm/`）是 sandbox manager 的功能库，由 arcbox-agent 调用
- **vm-agent**（`hypervisor/arcbox-vm/src/bin/vm-agent.rs`）运行在 sandbox 内部，是 arcbox-vm vsock 客户端的对端
- arcbox-agent 与 sandbox 的通信路径：arcbox-agent → arcbox-vm::SandboxManager → vsock → sandbox 内的 vm-agent

### 通信协议

| 端口 | 方向 | 协议 | 用途 |
|------|------|------|------|
| 1024 | macOS host → arcbox-agent | 自定义二进制帧，prost payload | Agent 管理（Ping/SystemInfo/EnsureRuntime 等），待扩展 sandbox 操作 |
| 52 | arcbox-agent（via arcbox-vm）→ sandbox | 自定义二进制帧，JSON StartCommand | 在 sandbox 内执行命令（exec/run） |

Port 1024 wire format（V2）：
```
[u32 BE length][u32 BE type][u16 BE trace_len][trace bytes][payload bytes]
```

Port 52 wire format：
```
[u8 type][u32 LE payload_len][payload bytes]
```

---

## 现状

### arcbox-agent 已有功能（port 1024）

- `Ping` / `GetSystemInfo`
- `EnsureRuntime`：启动 guest 内的 containerd / dockerd
- `RuntimeStatus`：查询 runtime 栈状态
- `PortBindingsChanged` / `PortBindingsRemoved`：port binding 事件推送

### arcbox-vm 已提供的能力

- `SandboxManager`：完整的 sandbox 生命周期管理（create / stop / remove / inspect / list）
- `run_in_sandbox` / `exec_in_sandbox`：通过 vsock 在 sandbox 内执行命令，流式返回输出
- `checkpoint_sandbox` / `restore_sandbox`：快照与恢复
- `subscribe_events`：sandbox 生命周期事件广播（broadcast::Receiver）
- `NetworkManager`：TAP 网卡 + IP 池管理
- `SnapshotCatalog`：快照元数据管理

### sandbox.proto（`comm/arcbox-protocol/proto/sandbox.proto`）

完整的 SandboxService API 定义已存在，包括：
- `CreateSandboxRequest` / `CreateSandboxResponse`
- `RunRequest` / `RunOutput`（流式）
- `ExecInput` / `ExecOutput`（双向流）
- `StopSandboxRequest` / `RemoveSandboxRequest`
- `SandboxInfo` / `SandboxSummary`
- `CheckpointRequest` / `RestoreRequest` 及对应响应
- `SandboxEvent`

`arcbox-protocol` 已将其编译为 `sandbox_v1` prost 模块，可直接使用。

---

## 修改方案

### 核心决策

**不引入 tonic/gRPC**，在 port 1024 的现有自定义二进制帧协议上扩展 sandbox 操作。原因：

- arcbox-agent 已有 tokio + prost，扩展成本低
- tonic 增加 musl cross-compile 复杂度和二进制体积
- 现有 wire format 是流式友好的（TCP 连接，多帧）

**sandbox.proto 的 service 定义**（`rpc Create(...)` 等）在当前方案中不使用，只复用 **message 定义**作为 prost payload 类型。service 定义保留作为 API contract 文档，待将来有需要时可直接加回 gRPC 实现。

### 新增/修改文件

```
guest/arcbox-agent/
├── Cargo.toml          修改：加 arcbox-vm（Linux-only 依赖）
└── src/
    ├── config.rs       新增：VmmConfig 加载逻辑
    ├── sandbox.rs      新增：SandboxManager 封装和各操作处理器
    ├── agent.rs        修改：共享状态加入 SandboxService，路由新消息类型
    └── rpc.rs          修改：新增 sandbox 消息类型定义
```

### 各模块设计

#### `src/config.rs`

加载 `VmmConfig`，优先级：
1. 环境变量 `ARCBOX_VMM_CONFIG` 指定的文件路径
2. `/etc/arcbox/vmm.toml`
3. Guest 环境内置默认值

```
默认值：
  firecracker.binary   = /usr/local/bin/firecracker
  firecracker.data_dir = /var/lib/arcbox/sandboxes
  network.bridge       = arcbox-sb0
  network.cidr         = 10.88.0.0/16
  network.gateway      = 10.88.0.1
  defaults.kernel      = /var/lib/arcbox/kernel/vmlinux
  defaults.rootfs      = /var/lib/arcbox/images/sandbox.ext4
```

#### `src/sandbox.rs`（Linux-only）

```rust
pub struct SandboxService {
    manager: Arc<SandboxManager>,
}
```

每个操作对应一个 async 方法，参数和返回值直接使用 `sandbox_v1` prost 类型：

| 方法 | 委托 | 响应模式 |
|------|------|----------|
| `handle_create` | `manager.create_sandbox()` | 单帧 |
| `handle_stop` | `manager.stop_sandbox()` | 单帧 |
| `handle_remove` | `manager.remove_sandbox()` | 单帧 |
| `handle_inspect` | `manager.inspect_sandbox()` | 单帧 |
| `handle_list` | `manager.list_sandboxes()` | 单帧 |
| `handle_run` | `manager.run_in_sandbox()` | 流式：多帧 RunOutput，最后帧 done=true |
| `handle_exec` | `manager.exec_in_sandbox()` | 双向流 |
| `handle_events` | `manager.subscribe_events()` | 流式：持续推送 SandboxEvent |
| `handle_checkpoint` | `manager.checkpoint_sandbox()` | 单帧 |
| `handle_restore` | `manager.restore_sandbox()` | 单帧 |
| `handle_list_snapshots` | `manager.list_snapshots()` | 单帧 |
| `handle_delete_snapshot` | `manager.delete_snapshot()` | 单帧 |

#### `src/rpc.rs` 新增消息类型

在现有编号后追加，payload 使用 `sandbox_v1` prost 类型：

```
// 已有
PingRequest            = 0x0001  /  PingResponse            = 0x1001
GetSystemInfoRequest   = 0x0002  /  GetSystemInfoResponse   = 0x1002
EnsureRuntimeRequest   = 0x0003  /  EnsureRuntimeResponse   = 0x1003
RuntimeStatusRequest   = 0x0004  /  RuntimeStatusResponse   = 0x1004

// 新增：sandbox CRUD
SandboxCreateRequest   = 0x0020  /  SandboxCreateResponse   = 0x1020
SandboxStopRequest     = 0x0021  /  SandboxStopResponse     = 0x1021
SandboxRemoveRequest   = 0x0022  /  SandboxRemoveResponse   = 0x1022
SandboxInspectRequest  = 0x0023  /  SandboxInspectResponse  = 0x1023
SandboxListRequest     = 0x0024  /  SandboxListResponse     = 0x1024

// 新增：sandbox 工作负载（流式）
SandboxRunRequest      = 0x0030  /  SandboxRunOutput        = 0x1030（多帧）
SandboxExecInit        = 0x0031  /  SandboxExecOutput       = 0x1031（多帧）
SandboxExecInput       = 0x0032  （运行中持续发送）
SandboxEventsRequest   = 0x0033  /  SandboxEvent            = 0x1033（多帧）

// 新增：快照
SandboxCheckpointReq   = 0x0040  /  SandboxCheckpointResp   = 0x1040
SandboxRestoreReq      = 0x0041  /  SandboxRestoreResp      = 0x1041
SandboxListSnapshotsReq= 0x0042  /  SandboxListSnapshotsResp= 0x1042
SandboxDeleteSnapReq   = 0x0043  /  SandboxDeleteSnapResp   = 0x1043
```

#### `src/agent.rs` 修改

**共享状态扩展：**
```rust
static SANDBOX_SERVICE: OnceLock<Arc<SandboxService>> = OnceLock::new();
```

**初始化顺序（`run()` 内）：**
1. 加载 `VmmConfig`（`config::load()`）
2. `SandboxManager::new(config)` — 创建 TAP bridge、IP pool
3. 将 `SandboxService` 存入 `SANDBOX_SERVICE`
4. 现有 RPC 循环加入 sandbox 消息类型路由

**流式操作处理：**

识别到流式请求（Run / Exec / Events）时，`tokio::spawn` 独立 task 持续向该连接写帧，主 accept 循环不阻塞继续接受新连接，连接关闭时 task 自动结束。

### 依赖变化

`guest/arcbox-agent/Cargo.toml`：
```toml
[target.'cfg(target_os = "linux")'.dependencies]
arcbox-vm = { workspace = true }
```

`arcbox-vm` 依赖 `fc-sdk`，后者通过 Unix socket 和进程 spawn 与 Firecracker 通信，不依赖动态库，可以正常 musl 静态编译。

### 分阶段实现顺序

**阶段 1（核心）**
- `config.rs` + `sandbox.rs` 初始化
- 单帧操作：Create / Stop / Remove / Inspect / List

**阶段 2（工作负载）**
- `SandboxRun` 流式实现

**阶段 3（双向流）**
- `SandboxExec` + `SandboxEvents`

**阶段 4（快照）**
- Checkpoint / Restore / ListSnapshots / DeleteSnapshot

### 不做的事

- 不引入 tonic / gRPC
- 不修改 `arcbox-grpc` crate
- 不修改 `sandbox.proto` message 定义（已完备）
- sandbox.proto 的 service 定义暂不删除（保留作 API contract 文档）
- 不改变 port 1024 现有消息类型的编号或行为
- `vm-agent.rs` 保持原位不动（它属于 sandbox 内部，与本次集成无关）

---

## CLI 集成方案

### 完整调用链

```
arcbox sandbox create <machine> ...
  │
  │  gRPC / Unix socket（tonic）
  ▼
arcbox-daemon
  │  SandboxService gRPC 服务端（新增）
  │  路由：查找 machine 对应的 VM CID → AgentClient
  │
  │  port 1024 vsock（自定义二进制帧）
  ▼
arcbox-agent（Guest VM 内）
  │  处理 SandboxCreate/Stop/Run/... 消息
  ▼
arcbox-vm::SandboxManager
  │
  │  vsock port 52
  ▼
vm-agent（Firecracker sandbox 内）
```

### 各层修改

#### 1. arcbox-cli（`app/arcbox-cli/`）

新增 `src/commands/sandbox.rs`，在 `Commands` 枚举加入 `Sandbox` 变体，使用 `SandboxServiceClient`（tonic）。命令结构：

```
arcbox sandbox create  <machine> [--kernel <k>] [--rootfs <r>] [--name <n>] [--cpus N] [--memory MB]
arcbox sandbox stop    <machine> <id>
arcbox sandbox rm      <machine> <id>
arcbox sandbox ls      <machine>
arcbox sandbox inspect <machine> <id>
arcbox sandbox run     <machine> <id> -- <cmd>      # 流式输出，非交互
arcbox sandbox exec    <machine> <id>               # 交互式 TTY
arcbox sandbox checkpoint <machine> <id> --snapshot <s>
arcbox sandbox restore    <machine> <snapshot>
```

`<machine>` 参数指定 sandbox 所在的 Guest VM，是所有 sandbox 命令的必选参数，用于 daemon 侧路由。

命名风格与 `arcbox machine` 保持一致（`ls` / `rm` alias、`--force` flag 等）。

#### 2. arcbox-grpc（`comm/arcbox-grpc/`）

sandbox.proto 的 **service 定义在此层正式生效**。`build.rs` 已经编译了 `sandbox.proto`，但目前只有 prost message 类型被用到。现在加入 tonic service 生成：

```toml
# comm/arcbox-grpc/build.rs 修改：对 sandbox.proto 也生成 tonic service stubs
```

`lib.rs` 新增导出：
```rust
pub use v1::sandbox_service_client::SandboxServiceClient;
pub use v1::sandbox_service_server::{SandboxService, SandboxServiceServer};
pub use v1::sandbox_snapshot_service_client::SandboxSnapshotServiceClient;
pub use v1::sandbox_snapshot_service_server::{SandboxSnapshotService, SandboxSnapshotServiceServer};
```

#### 3. arcbox-core（`app/arcbox-core/`）

**`src/agent_client.rs` 扩展：**

在现有 `MessageType` 枚举追加 sandbox 消息类型（0x0020–0x0043，与 arcbox-agent/rpc.rs 完全对齐），并为 `AgentClient` 新增对应方法：

```rust
// 单帧操作
pub async fn sandbox_create(&mut self, req: CreateSandboxRequest) -> Result<CreateSandboxResponse>
pub async fn sandbox_stop(&mut self, req: StopSandboxRequest) -> Result<()>
pub async fn sandbox_remove(&mut self, req: RemoveSandboxRequest) -> Result<()>
pub async fn sandbox_inspect(&mut self, req: InspectSandboxRequest) -> Result<SandboxInfo>
pub async fn sandbox_list(&mut self) -> Result<Vec<SandboxSummary>>

// 流式操作（返回 Stream）
pub async fn sandbox_run(&mut self, req: RunRequest) -> Result<impl Stream<Item = Result<RunOutput>>>
pub async fn sandbox_events(&mut self) -> Result<impl Stream<Item = Result<SandboxEvent>>>

// 快照
pub async fn sandbox_checkpoint(&mut self, req: CheckpointRequest) -> Result<CheckpointResponse>
pub async fn sandbox_restore(&mut self, req: RestoreRequest) -> Result<RestoreResponse>
```

流式方法内部：发送请求帧后，循环读取响应帧直到 `done=true`，包装为 `tokio_stream::wrappers` 返回给调用者。

**新增 `src/sandbox.rs`（或在现有 `src/machine.rs` 内扩展）：**

实现 gRPC `SandboxService` trait，核心逻辑：
1. 从请求中取出 `machine_id`（或 `machine_name`）
2. 查找对应 VM 的 `AgentClient`（已有 VM 状态管理，可复用）
3. 调用 `agent_client.sandbox_*()` 方法
4. 将结果转换为 gRPC 响应返回

#### 4. 路由设计：machine_id 字段

sandbox 操作必须路由到正确的 VM。两个方案：

| 方案 | 做法 | 优缺点 |
|------|------|--------|
| A（推荐）| proto message 加顶层 `machine_id` 字段（wrapper） | 干净，不污染 sandbox.proto 现有定义 |
| B | 修改 sandbox.proto 的各 Request message 加 `machine_id` | 更简单，但修改了 proto schema |

推荐方案 A：在 daemon 侧定义 wrapper request，gRPC service 接口层做一层薄包装，核心 sandbox.proto message 保持不变。

#### 5. 流式桥接

`SandboxRun` 和 `SandboxEvents` 涉及 gRPC server-side streaming 与 port 1024 自定义帧流式之间的桥接：

```
gRPC client                 daemon (SandboxService impl)        arcbox-agent (port 1024)
    │                              │                                    │
    │  ServerStreaming RPC ────────►│                                    │
    │                              │── SandboxRunRequest frame ─────────►│
    │                              │                                    │ (tokio spawn)
    │◄─ RunOutput gRPC message ────│◄── SandboxRunOutput frame ─────────│
    │◄─ RunOutput gRPC message ────│◄── SandboxRunOutput frame ─────────│
    │◄─ (done) ───────────────────│◄── SandboxRunOutput(done=true) ────│
```

daemon 侧在 `AgentClient::sandbox_run()` 中用 channel 桥接：读取帧 → 发送到 channel → gRPC streaming sender 从 channel 消费并发送给 CLI。

**初版实现**：使用 `tokio::sync::mpsc::unbounded_channel`，`tokio::spawn` 独立 task 读帧并转发，先跑通功能。

**已知问题（集成后处理）**：unbounded channel 在 CLI 消费慢时会导致 daemon 侧内存无限增长。正确做法是换成 bounded channel（容量 16–64），让阻塞从 HTTP/2 window 反向传播到 TCP，进而限速 arcbox-agent。同时需要处理 CLI 断开时的清理（向 arcbox-agent 发取消帧，kill sandbox 内进程，释放 task）。`SandboxExec` 双向流还需要 `select!` 同时监听两个方向的关闭。

#### 6. 新增/修改文件汇总

```
comm/arcbox-grpc/
├── build.rs            修改：对 sandbox.proto 启用 tonic service 代码生成
└── src/lib.rs          修改：导出 SandboxServiceClient / SandboxServiceServer

app/arcbox-core/
├── src/agent_client.rs 修改：追加 sandbox MessageType 枚举值和 sandbox_* 方法
└── src/sandbox.rs      新增：实现 gRPC SandboxService trait，路由到 AgentClient

app/arcbox-daemon/
└── src/main.rs         修改：注册 SandboxServiceServer 到 gRPC 服务器

app/arcbox-cli/
├── src/commands/mod.rs 修改：加入 Sandbox 变体
└── src/commands/sandbox.rs  新增：CLI 命令实现
```

### 不做的事（CLI 阶段）

- 不修改 sandbox.proto 的 message 定义（只加 service 生成）
- 不在 arcbox-agent/rpc.rs 的消息编号上做任何改变
- `arcbox sandbox exec` 双向流交互模式留到 arcbox-agent 阶段 3 完成后再实现

---

## 讨论记录

### Q：vm-agent.rs 做了什么？arcbox-vm 为什么是 host 侧 crate？

`vm-agent.rs` 是运行在 sandbox 内部的 exec 服务器，监听 vsock port 52。它处理来自 arcbox-vm vsock 客户端的请求：
- 解析 JSON 编码的 `StartCommand`（cmd、env、working_dir、tty 等）
- 非交互模式：`std::process::Command` + 管道，三线程并发读写
- 交互模式：`openpty` + `fork` + `execvp`，PTY master 读写
- 支持 timeout（`SIGKILL`）、`MSG_RESIZE`（`TIOCSWINSZ`）、`MSG_EOF`

`arcbox-vm` 是 host 侧 crate 是因为它依赖 `fc-sdk`（Firecracker SDK），fc-sdk 提供 `FirecrackerProcessBuilder`、`VmBuilder` 等 API，这些只有在运行 Firecracker 进程的那一侧才有意义。在本项目中，"运行 Firecracker 的那一侧"就是 Guest VM（arcbox-agent 所在的环境）。

### Q：host 和 guest 具体指什么？

澄清后的正确理解：

- **物理机（macOS）**：运行 Virtualization.framework，启动 Guest VM
- **Guest VM（Linux）**：运行 arcbox-agent，arcbox-agent 使用 arcbox-vm 库管理 Firecracker sandbox
- **Sandbox（嵌套 microVM）**：由 arcbox-vm 在 Guest VM 内启动的 Firecracker 实例，vm-agent 运行在这里

在 arcbox-vm 的代码语境里，"host" 指 Guest VM（arcbox-agent 所在层），"guest" 指 sandbox（vm-agent 所在层）。这是嵌套虚拟化。

### Q：不用 gRPC，sandbox.proto 还有必要吗？

sandbox.proto 包含两类内容：

1. **Service 定义**（`rpc Create(...)`）：gRPC 专用，当前方案不使用，是死代码
2. **Message 定义**（`CreateSandboxRequest`、`SandboxInfo` 等）：与 gRPC 无关，prost 编译后可直接用作 payload 的序列化类型

当前方案：service 定义保留作文档，message 定义通过 `sandbox_v1` 复用。若将来 host 侧需要暴露 gRPC，可直接加回 service 实现，message 不需改动。
