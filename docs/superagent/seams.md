# Superagent 在 codex 里的接入点

引擎 crate `codex-rs/ext/superagent/` 自成一体，升级上游时整个带过去。下表列的是它在 codex 其他地方的接入点。升级到新的上游版本时，要按新代码的结构把这些地方重新接上。

本次候选基于 `rust-v0.167.0`，引擎及其测试与旧主线完全一致。表中改动量保留相对 `rust-v0.166.0` 的来源记录；0.167 适配与验证见 [升级记录](../reapply-0.167.0.md)。

## app-server（宿主）

| 文件 | 做什么 | 改动 | 提交 |
| --- | --- | --- | --- |
| `app-server/src/superagent_host.rs`（新） | 实现引擎的 `SuperagentHost`：开会话、发消息、插话、打断，读会话的回合（含错误类型），查会话当前账号（未加载的会话答它加载时会用的账号：保存的绑定，没有就用创建它的账号，不加载它），换账号（`bind_profile`，未加载的会话只改绑定），开始排队的输入（`start_queued`：Router 一轮结束要换号时，引擎让它排队的话先等，换好后由引擎开始）。推进的步骤用完全访问（`DangerFullAccess`、`Never`），`turn_trigger = "drive"`；交出一步时回答这句话是开了新的一轮，还是插进了正在跑的那一轮（`start_step` 返回 `StepStart`），core 没收下的输入报错；读一个账号的重置卡、给它用一张（`reset_cards`、`use_reset_card`，经账号处理器）。给 Router 和 worker 定工具策略 | +789/-26 | c2fd5a6e7c 及之后 8 个 superagent 提交，3aa97711fa、d997bcbe16、d574ce1265、3e3b3e2400 |
| `app-server/src/request_processors/superagent.rs`（新） | 引擎自己的伪连接 `SUPERAGENT_CONNECTION`；Router 不加载会话就能读会话列表和回合，以及未加载会话加载时会用的账号（`thread_account_profile_internal`，只读元数据） | +176/-2 | c2fd5a6e7c、23b6ca8d7c、722ba7c5df、d997bcbe16 |
| `app-server/src/superagent_status.rs`（新） | 处理 `superagent/status` | +26 | c2fd5a6e7c |
| `app-server/src/message_processor.rs` | 功能打开时建引擎服务并交给各处理器；只有 unix socket 服务（托管 daemon，或专门的 `--listen unix://`）建，`codex exec`、没有 daemon 的 TUI 和 stdio 服务不建（`MessageProcessorArgs::superagent_host`），否则它们会在同一个 `CODEX_HOME` 上多起一个引擎；停机时先停调度，再写快照（`drain_superagent`）；宿主拿到账号处理器（重置卡用） | +101/-39 | c2fd5a6e7c、e5bae0ebfc、d59d6ce405、f7a09ce436 |
| `app-server/src/lib.rs` | 声明两个模块；unix socket 服务才可以跑引擎（`superagent_host`）；停机前调用 `drain_superagent` | +11/-7 | c2fd5a6e7c、e5bae0ebfc、d59d6ce405、f7a09ce436 |
| `app-server/src/in_process.rs` | 进程内 app-server（`codex exec`、没有 daemon 的 TUI）不跑引擎 | +3 | d59d6ce405 |
| `app-server/src/request_processors/account_processor/rate_limit_resets.rs` | 给引擎的两个方法：按账号列重置卡（`superagent_reset_cards`）、按卡 id 用一张（`superagent_use_reset_card`），和 `account/rateLimitResetCredit/consume` 走同一个后端客户端 | +59 | 3e3b3e2400 |
| `app-server/src/message_processor_tracing_tests.rs`、`message_processor_workspace_routing_tests.rs`、`user_verification_test_support.rs` | 测试里构造 `MessageProcessorArgs` 时带上 `superagent_host` | +1 ×3 | d59d6ce405 |
| `app-server/src/extensions.rs` | `thread_extensions_with_superagent`：把引擎的工具、上下文和回合钩子装到每个线程上 | +12 | c2fd5a6e7c |
| `app-server/src/request_processors/thread_processor.rs` | 线程处理器带上引擎；`thread_start_inner` 回报新线程 id（引擎开会话时用）；按线程套工具策略；几个读列表和回合的内部方法改成 `pub(super)` 给引擎用 | +28/-4 | c2fd5a6e7c、23b6ca8d7c |
| `app-server/src/request_processors/turn_processor.rs` | 用户在回合中插话时告诉引擎（`note_steered_input`）；被驱动者拒绝的自动回合（`DeclinedByDriver`）不算失败 | +12/-1 | c2fd5a6e7c、c8ff38095b |
| `app-server/src/request_processors/daemon_snapshot.rs` | daemon 恢复快照时跳过引擎管的线程，由引擎自己对账续跑 | +7 | e5bae0ebfc |
| `app-server/src/request_processors/thread_auth_profile.rs` | 引擎的伪连接从不挂订阅者，换账号时不检查它是否已关闭 | +9/-1 | c2fd5a6e7c、1aa844246e |
| `app-server/src/bespoke_event_handling.rs` | 错误和警告记进引擎的事件记录（`incidents::note`） | +8 | c2fd5a6e7c |
| `app-server/src/request_processors.rs` | 声明 `superagent` 子模块 | +1 | c2fd5a6e7c |
| `app-server/Cargo.toml` | 依赖引擎 crate | +1 | c2fd5a6e7c |
| `app-server/README.md` | 宿主接口说明（Organization coordinator 一节），含 Router 换号时排队的话先等、状态里的 `accountMoves`、未加载会话的账号 | +74/-16 | c2fd5a6e7c、c8ff38095b、3aa97711fa、d997bcbe16 |

## 协议

| 文件 | 做什么 | 改动 | 提交 |
| --- | --- | --- | --- |
| `app-server-protocol/src/protocol/v2/superagent.rs`（新） | `superagent/status` 的请求和响应类型 | +18 | c2fd5a6e7c |
| `app-server-protocol/src/protocol/common.rs` | 注册 `superagent/status`（实验接口） | +6 | c2fd5a6e7c |
| `app-server-protocol/src/protocol/v2/mod.rs` | 导出上面的类型 | +2 | c2fd5a6e7c |
| `app-server-protocol/schema/precomputed/app-server-exports-experimental.json.zst` | 实验接口导出，重新生成 | 二进制 | c2fd5a6e7c |

## 状态库

| 文件 | 做什么 | 改动 | 提交 |
| --- | --- | --- | --- |
| `state/src/superagent.rs`（新） | 打开账本 `superagent_1.sqlite` | +10 | c2fd5a6e7c |
| `state/superagent_migrations/0001_superagent.sql`、`0002_superagent_inflight.sql`（新） | 账本的表结构 | +103 | c2fd5a6e7c |
| `state/src/sqlite.rs`、`migrations.rs`、`lib.rs`、`telemetry.rs` | 接入账本和它的迁移 | +36/-1 | c2fd5a6e7c |
| `state/BUILD.bazel` | 迁移目录加进 Bazel 的 `compile_data` | +1 | c2fd5a6e7c |

## 扩展接口和 core：一个驱动者拥有线程的自动回合

| 文件 | 做什么 | 改动 | 提交 |
| --- | --- | --- | --- |
| `ext/extension-api/src/turn_arbiter.rs`（新）、`registry.rs`、`lib.rs` | `AutomaticTurnArbiter`：自动回合（Goal、daemon 恢复）开始前先问登记的驱动者 | +52 | c8ff38095b |
| `core/src/session/turn_input.rs` | 自动回合提交前调用 `admit_automatic_turn`，被拒就返回 `DeclinedByDriver` | +16 | c8ff38095b |
| `protocol/src/turn_input.rs` | 新原因 `NotSubmittedReason::DeclinedByDriver` | +4 | c8ff38095b |
| `core/tests/suite/turn_input_submission.rs` | 上面这条的测试 | +53 | c8ff38095b |

## 其他

| 文件 | 做什么 | 改动 | 提交 |
| --- | --- | --- | --- |
| `features/src/lib.rs` | `Feature::Superagent`，默认关 | +8 | c2fd5a6e7c |
| `core/config.schema.json` | `[features] superagent` | +6 | c2fd5a6e7c |
| `Cargo.toml`（workspace）、`Cargo.lock` | 引擎 crate 进 workspace，以及它的依赖 | +25 | c2fd5a6e7c、49602be125 |
| `async-utils/src/lib.rs` | 未优化构建的主线程栈改成 64 MiB。dev 构建从 `/agents` 进会话时曾栈溢出 | +8/-2 | 1a343061ad |
| `scripts/codext-org.sh`、`scripts/install-codex-account-tools.py` | 组织 PM 的启动器，以及安装它的脚本 | +89/-1 | d16bdb73f8 |

0.167 已按旧表说明省去无人引用的工程提示脚手架和 Dell 专用系统标识覆盖。其余接入点均已重接。

## 升级时怎么用这张表

1. 把 `codex-rs/ext/superagent/` 整个带过去，包括 `tests/`，并加进 workspace。
2. 按顺序接：先状态库、协议、扩展接口和 core，再 app-server。上游改了结构，就按新结构接同一个需求，不要硬套旧代码。
3. 引擎只通过 `host.rs` 的 `SuperagentHost` 用宿主。宿主接口变了，尽量只改 `superagent_host.rs`，不改引擎。
4. 接完构建，再跑引擎测试和实验室端到端，见 [updating.md](updating.md)。
5. 接入点有增减时，更新这张表。
