# Superagent 与 Router

Superagent 是跑在 app-server daemon 里的组织引擎。Router 是它管的一个常驻会话：替用户看着本机所有会话，并按用户的意思推进其中的会话。本文写的是 `main` 上的现状；早先的设计讨论稿在 [history/](history/)。

## 能做什么

**看。** Router 不加载会话就能读本机所有会话。它的上下文里常驻三样东西：自己的规则、最近活跃的会话索引、等用户处理的会话（报错、等批准、等回答）。每一回合只告诉它比上一回合多了什么；历史被压缩后再给一次全文。

**推进。**

1. 用户说要做什么，Router 先问清楚，用一句话复述打算怎么做。
2. 用户同意后，Router 用 `start` 开始推进。它可以新建文件夹和新会话（用 Router 自己的账号），第一句话由 Router 自己写。
3. 被推进的会话每做完一步，引擎取它最后一段回答作为结论，用一张 `[通知]` 卡片交给 Router。
4. Router 对每个结论二选一：转告用户，或者用 `say` 回 worker。
5. `say` 要带依据：它接着哪句话说（`builds_on`），要照抄 worker 最近结论、用户原话或纠正里的一段；它为了源头的哪一点（`serves`），要照抄用户原话、纠正或验收清单里的一段。引擎核对不上就退回。有方法名、skill 名、`A1` 这类编号的话也退回，因为说给人听要用人话。
6. 用户纠正时：worker 还在做，就把话插进当前回合（steer）；做错了，就打断（`interrupt`）。

推进分两档：

- **简单**：一句话的目标，一个文件夹，不建锚定，用户自己验收。
- **锚定**：项目有 `.engineering`，每一步结束跑 oev2 判定；可交付、偏离目标、没有进展时停下。

一次推进会停在下面四种状态之一，Router 都会收到通知：交了结论、可交付等用户验收、需要用户决定、已暂停（附原因）。

**换号。**

- 推进的一步因为账号额度用完（`usageLimitExceeded`）或模型满载（`serverOverloaded`，"Selected model is at capacity"）而失败时，引擎让这个账号冷却：额度用完的冷却到重置时间，满载的冷却 15 分钟。
- 然后按 `codex-account-next list` 的顺序挑下一个账号：重置最早的优先，跳过免费账号、剩余不到 5% 的和还在冷却的。会话换到新账号后，同一步重发。
- Router 自己的回合也一样，每次失败只换一次。
- Router 也可以用 `switch_account` 手动换。

**权限。** 推进的步骤用完全访问：不加沙箱，不问批准，可以用本机软件和浏览器。

**工单组织。** Dell build-27 的工单流程（PM、worker、工单、发件箱、恢复）还在，`org` 档位可用。

## 怎么开

1. 在 `~/.codex/config.toml` 打开功能：

   ```toml
   [features]
   superagent = true
   ```

2. `~/.codex/superagent/router-mode` 写 `drive`。不写或写 `observe` 只看不推；写 `org` 时还会带上工单组织。
3. 建 Router 会话并登记：`python3 -I scripts/superagent/router.py create`，再用 `pin` 把它放到 `/agents` 第一行。
4. 打开：`codex-account-next <账号>`（或直接 `codext`），输入 `/agents`，第一行就是 Router。
5. 自动换号要用 `~/.local/bin/codex-account-status`，它由 `scripts/install-codex-account-tools.py` 安装。
6. 可选：`~/.codex/agents/` 里的 worker、helper、drafter 定义；skills `hdae-development`、`hdae-engineering`。这两个 skill 是 Router 想问题的思路，不是要说出口的话术。

daemon 可以随时重启。重启后引擎逐个检查推进中的会话：已经交出去、但没看到开始的那一步不会自动重发，而是暂停并通知 Router。

## 代码在哪

引擎是 `codex-rs/ext/superagent/`（crate `codex-superagent-extension`）：

| 位置 | 内容 |
| --- | --- |
| `router.rs`、`router/drives.rs` | Router 的规则、会话索引、等用户的会话、推进状态 |
| `drive/mod.rs` | 推进记录和状态，开始、暂停、恢复、停止、交接、重启对账 |
| `drive/say.rs` | `say` 的依据核对和话的检查，插话 |
| `drive/report.rs` | 取结论、判定、通知卡片 |
| `drive/accounts.rs`、`drive/switch.rs` | 账号池、挑号、冷却、换号（含 Router 自己） |
| `drive/anchor.rs`、`drive/audit.rs` | 锚定目标和 oev2 判定 |
| `drive/context.rs` | 给被推进会话的常驻规则（`<superagent_driven>`） |
| `drive/feedback.rs`、`drive/lifecycle.rs` | 用户验收反馈；回合开始和结束的钩子 |
| `tools/drive.rs`、`tools/read.rs` | Router 的工具 `superagent_drive`、`superagent_read` |
| `tools/pm.rs`、`tools/worker.rs` | 工单组织的工具 |
| `core/`、`executor/`、`store/`、`effects.rs`、`outbox.rs`、`recovery.rs` | build-27 的组织引擎和持久化的效果队列 |
| `host.rs` | 引擎使用宿主的接口 `SuperagentHost` |

宿主在 codex 里的接入点见 [seams.md](seams.md)，最大的一处是 `codex-rs/app-server/src/superagent_host.rs`。账本是 SQLite home 下的 `superagent_1.sqlite`，表结构在 `codex-rs/state/superagent_migrations/`。

## 测试

- 引擎：`cd codex-rs && cargo test -p codex-superagent-extension`。推进的测试在 `ext/superagent/tests/drive.rs`。
- 端到端：实验室的 `~/242-superagent-codext-lab/tests/scripts/drive_e2e.py`，用 mock 模型跑简单、锚定、换号、Router 换号四个场景。
- 真实模型：在实验室 daemon 上让 Router 推进一个小目标，看通知、`say`、插话、换号是否正常。

## 发到日常

`scripts/superagent/promote.sh` 依次做三件事：构建本机包，更新实验室 daemon，再更新日常 daemon 并替换 `~/.local/bin/codex` 和 `codext`（替换前留备份）。改动先在实验室测，用户同意后再发到日常。

升级上游和更新 superagent 的流程见 [updating.md](updating.md)。
