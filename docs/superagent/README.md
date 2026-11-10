# Superagent 与 Router

Superagent 是跑在 app-server daemon 里的组织引擎。Router 是它管的一个常驻会话：替用户看着本机所有会话，并按用户的意思推进其中的会话。本文写的是 `main` 上的现状；早先的设计讨论稿在 [history/](history/)。

## 能做什么

**看。** Router 不加载会话就能读本机所有会话。它的上下文里常驻这几样东西：自己的规则、最近活跃的会话索引、等用户处理的会话（报错、等批准、等回答）、推进中的会话、用户的长期规矩。每一回合只告诉它比上一回合多了什么；历史被压缩后再给一次全文。

**推进。**

1. 用户说要做什么，Router 先问清楚，用一句话复述打算怎么做。
2. 用户同意后，Router 用 `start` 开始推进。它可以新建文件夹和新会话（默认用 Router 自己的账号；用户指定了账号时带上 `account`，见下面的换号），第一句话由 Router 自己写。
3. 被推进的会话每做完一步，引擎取它最后一段回答作为结论，用一张 `[通知]` 卡片交给 Router。通知送给开这次推进的那个 Router。Router 正在回答时，通知先在引擎里等；它空下来时，攒着的几路并成一条送。送之前逐条核对：Router 已经回过、停过或交回过的那一路，通知就不送了。
4. Router 对每个结论二选一：转告用户，或者用 `say` 回 worker。停着的一路也直接 `say`，不用先 `resume`；用户同意多推几步时，`say` 带 `max_steps`；`until` 说清这一步做到哪停（引擎把它接在话后面告诉 worker）。
5. `say` 要带依据：它接着哪句话说（`builds_on`），它为了源头的哪一点（`serves`），都要照抄原话。可以抄的出处：这一路最近 5 条结论、用户原话和纠正、用户在会话里打的字、用户对 Router 说过的话（抄了就记进这一路的纠正）、长期规矩；`builds_on` 还可以抄别的路的结论，这一步记下抄的是哪一路。引擎核对不上就退回，退回时说明找过哪些地方，并给出这一路最近结论的开头；退回后这一路在 Router 的清单上标着"你上一句被退回，这一路还在等你"，直到一句话发出去。有方法名、skill 名、`A1` 这类编号的话也退回，因为说给人听要用人话。
6. 用户纠正时：worker 还在做，就把话插进当前回合（steer）；做错了，就打断（`interrupt`）。Router 对已经有回合在跑的会话说新一步时，话会插进那一轮，不会白等一轮新回合。
7. 要等别的路交结论、等一段时间、等某个文件、等用户回话时，Router 用 `wait` 写明等什么。这一路显示为"等待：…"，不进等用户处理的清单；等到了引擎把它交回 Router 并通知（等的是别的路时，和那一路的结论在同一条消息里）；设了提醒时间还没等到，提醒一次。Router 对这一路做任何事，等待就结束。
8. 用户说的话如果对所有路或今后都有效，Router 用 `remember` 照抄原话记成长期规矩（全部、一个文件夹下的各路、或一路），`forget` 删掉。长期规矩存在账本里，常驻 Router 的上下文，压缩后也不丢，并且可以当作 `say` 的依据。

推进分两档：

- **简单**：一句话的目标，一个文件夹，不建锚定，用户自己验收。
- **锚定**：项目有 `.engineering`，每一步结束跑 oev2 判定；可交付、偏离目标、没有进展时停下。"没有进展"是连续两步既没有回填记录、文件夹里也没有任何写入（提交也算写入）。判定程序自己出错时不停，这一步照常交给 Router，下一步结束再判；记录写得不对时也不停，结论里写"记录要修"，由 Router 让 worker 补上。

一次推进会停在下面四种状态之一，Router 都会收到通知：交了结论、可交付等用户验收、需要用户决定（只有偏离目标和真的没有进展两种，接着做必须带用户的原话：`say` 的 `correction`）、已暂停（附原因）。

推进中出意外时：

- 回合结束的钩子先于结束落进会话记录。引擎以钩子为准，等记录跟上（最多一分钟）再取结论，所以不会卡在"正在做"，额度失败也不会被读成 interrupted。
- 暂停期间那一步做完了，结论照样记下，交回后 Router 接着它说。
- 一步因为网络断开、workspace routing 失败、服务出错而失败，45 秒后在原账号上重发一次，再失败才暂停。

**巡检。** 引擎每分钟自己看一遍推进中的会话：等待的东西到了就叫醒；一步做完却没读到结论的再读一次；再把看到的情况标在这一路的清单行上（"注意：…"）。标的有：交了结论半小时以上没人回、一步做了一个半小时以上而文件夹一小时没有写入（这两条会通知 Router 一次）、步数快用完、最近两次结论开头一样、连着 5 步都不到 3 分钟、账号上的会话太多。状态一变，标记跟着清掉。

**事件。** 推进的每次状态、步数、账号变化，以及开始和释放，都往 `$CODEX_HOME/superagent/events.jsonl` 追加一行 JSON（超过 8 MB 换到 `events.1.jsonl`）。`superagent/status` 的 `drives` 用同样的形状列出每一路。外面要盯推进的，读这个文件就行，不用逐个扫会话记录。

**换号。** 引擎在账号用完之前就换（引擎查账号不花模型额度），失败后再换只是兜底。

- 提前换 Router 的号：引擎每 2 分钟只读一次 Router 正在用的那个账号的额度（经 daemon 读，一个账号很快）。剩余额度低于软阈值（10%），或者低于硬阈值（3%）、在冷却、用不了，才去读整个账号池（`codex-account-status`，十来个账号要好几秒），挑号换过去：Router 空闲时马上换；Router 正在回答时不打断，等这一轮结束再换。账号健康时整个账号池一次也不读。
- 读来的额度留用一段时间（两个查看周期，默认 4 分钟）：一轮结束时、引擎给 Router 送通知或任何话之前，都按读过的看一次，大多不用再读，所以下一轮总在有余量的账号上开始。这一轮结束要换号时，Router 排队的话先等一等（最多一分钟），换好由引擎接着开始。
- daemon 重启后 Router 还没加载时，引擎读它加载时会用的账号（保存的账号绑定，没有就用创建它的账号），要换就直接改绑定，不加载它；用户打开 Router 说第一句话时，已经在新账号上。
- 提前换推进会话的号：每一步交出去之前只读一次会话自己的账号（四分钟内读过就不再读），规则相同；需要换才读整个账号池，先换再交这一步，推进记录里的账号跟着变。一步做到一半不换。
- 挑号：按 `codex-account-next list` 的顺序，重置最早的优先，跳过免费账号和还在冷却的。引擎自己换号时，跳过已经有 4 路在做的账号（推进中的会话和 Router 都算），除非别的账号也都这么满。提前换只换到剩余不低于软阈值的账号；账号已经快用完时，才退一步接受剩余不低于硬阈值、也不低于 5% 的账号。没有能换的就不动，只记一次警告。同一个账号在一轮（推进的一步）之后只会被换走一次，不会来回换。
- 兜底：推进的一步、或 Router 自己的一轮，因为账号额度用完（`usageLimitExceeded`）或模型满载（`serverOverloaded`，"Selected model is at capacity"）而失败时，这个账号冷却（额度用完的冷却到重置时间，满载的冷却 15 分钟），换号后把这一步重发、把这一轮的话重新交给 Router。
- 兜底：Router 的一轮卡在重连里结束不了（同一轮 3 分钟内出现 2 次满载或额度用完）时，引擎打断这一轮，按上一条换号，再把这一轮的话交给 Router。没有能换的账号时不打断。Router 的每一轮最多因此换一次。
- 冷却提前结束：额度用完的账号常被用户用重置卡充回。引擎之后新读到这个账号（读它自己或读整个账号池）能用、剩余不低于软阈值、而且比它用完时读到的多，就在这次读时结束它的冷却，同一次检查就能换到它。用完时读到的额度本来就不低的账号（读不出用完的是哪项额度）照旧冷却到重置时间；满载的冷却不提前结束。提前结束的冷却记在 `superagent/status` 的 `endedCooldowns` 里（最近 30 次）。
- 手动：Router 可以用 `switch_account` 给会话换号。用户指定新的推进用哪个账号时，`start` 带上 `account`：新会话直接在这个账号上开，已有的会话在第一步之前换过去，这一步不会被提前换走。
- 每次换号都记在 `superagent/status` 的 `accountMoves` 里（最近 30 次）。
- 重置卡（默认关）：`CODEXT_SUPERAGENT_RESET_CARDS_PER_DAY` 设成大于 0 时，没有任何账号能接手（当前账号已用完、别的也都用完或在冷却）的那一刻，引擎自己用一张重置卡。只给已经用完、而且自然重置还在 12 小时以外的账号用，自然重置最晚的先用；同一个账号先用最早过期的卡。一张卡把账号充回满额，并从用的那一刻重新算 7 天，所以快自然重置的账号不值得用卡。用了就通知 Router，记在 `superagent/status` 的 `resetCards` 里（最近 30 次）；24 小时内最多用这么多张。

可调的值放在 daemon 的环境变量里，daemon 启动时读一次：

| 环境变量 | 默认 | 含义 |
| --- | --- | --- |
| `CODEXT_SUPERAGENT_QUOTA_SOFT_PERCENT` | 10 | 剩余额度低于它，在两轮（两步）之间换号；额度用完在冷却的账号新读到不低于它，冷却提前结束 |
| `CODEXT_SUPERAGENT_QUOTA_HARD_PERCENT` | 3 | 低于它（或在冷却、用不了）算快用完；不高于软阈值 |
| `CODEXT_SUPERAGENT_QUOTA_WATCH_SECS` | 120 | 多久看一次 Router 的账号；读过的额度留用两个周期。0 关掉定时查看，一轮结束、送话前和每步前照样看（读过的留用 4 分钟） |
| `CODEXT_SUPERAGENT_ROUTER_RESCUE_THRESHOLD` | 2 | Router 同一轮出现几次满载或额度用完就打断换号；0 关掉 |
| `CODEXT_SUPERAGENT_ROUTER_RESCUE_WINDOW_SECS` | 180 | 上面这几次要落在多少秒之内 |
| `CODEXT_SUPERAGENT_MAX_PER_ACCOUNT` | 4 | 一个账号上有几路在做，引擎自己换号时就跳过它；0 关掉 |
| `CODEXT_SUPERAGENT_RESET_CARDS_PER_DAY` | 0 | 没有账号能接手时，引擎 24 小时内最多自己用几张重置卡；0 不用，留给用户 |
| `CODEXT_SUPERAGENT_RESET_CARD_MIN_HOURS` | 12 | 自然重置还有多少小时以上的账号才值得用卡 |

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

引擎只在 unix socket 服务里跑（托管 daemon，或专门的 `--listen unix://`）。`codex exec`、没有 daemon 的 TUI、stdio 服务即使打开了功能也不跑引擎，所以对日常目录跑 `codex exec` 不会再多起一个引擎、把正在做的几路误判成"没有正常结束"。同一个 `CODEX_HOME` 上，引擎还要先拿到 `superagent/engine.lock` 才启动：新 daemon 在旧的还没退完时，等锁放开再起引擎。

## 代码在哪

引擎是 `codex-rs/ext/superagent/`（crate `codex-superagent-extension`）：

| 位置 | 内容 |
| --- | --- |
| `router.rs`、`router/drives.rs`、`router/standing.rs` | Router 的规则、会话索引、等用户的会话、推进状态、长期规矩 |
| `drive/mod.rs` | 推进记录和状态，开始、暂停、恢复、停止、交接、重启对账 |
| `drive/look.rs`、`drive/turns.rs`、`drive/retry.rs` | 一步结束后的查看（等记录跟上、暂停时留结论）；回合钩子说了什么；网络失败重发一次 |
| `drive/say.rs`、`drive/basis.rs`、`drive/standing.rs` | `say` 的依据核对和话的检查，插话；依据可以从哪里来；长期规矩 |
| `drive/notices.rs` | 给 Router 的通知：Router 忙时攒着，空下来并成一条，过期的不送，送给开这次推进的 Router |
| `drive/wait.rs`、`drive/sweep.rs`、`drive/events.rs` | 停放等待；每分钟的巡检和标记；事件流和 `superagent/status` 里的 `drives` |
| `drive/report.rs`、`drive/progress.rs` | 取结论、判定、通知卡片；文件夹里有没有新写入 |
| `drive/accounts.rs`、`drive/cooldown.rs`、`drive/policy.rs`、`drive/load.rs`、`drive/reset_cards.rs` | 账号池、挑号、要不要换号的判断（`plan`）；账号冷却，读到额度回来就提前结束；可调的值；每个账号上有几路；没有账号能接手时用重置卡 |
| `drive/pool.rs`、`drive/step_account.rs`、`drive/switch.rs` | 读账号的额度和账号池、换号记录；每步前的检查、指定账号开始推进；推进会话失败后的换号 |
| `drive/router_account.rs`、`drive/router_rescue.rs` | Router 提前换号（定时查看、一轮结束、送话前）和兜底（失败或卡在重连的一轮） |
| `drive/anchor.rs`、`drive/audit.rs` | 锚定目标和 oev2 判定 |
| `drive/context.rs` | 给被推进会话的常驻规则（`<superagent_driven>`） |
| `drive/feedback.rs`、`drive/lifecycle.rs` | 用户验收反馈；回合开始和结束的钩子 |
| `tools/drive.rs`、`tools/read.rs` | Router 的工具 `superagent_drive`、`superagent_read` |
| `tools/pm.rs`、`tools/worker.rs` | 工单组织的工具 |
| `core/`、`executor/`、`store/`、`effects.rs`、`outbox.rs`、`recovery.rs` | build-27 的组织引擎和持久化的效果队列 |
| `host.rs` | 引擎使用宿主的接口 `SuperagentHost` |

宿主在 codex 里的接入点见 [seams.md](seams.md)，最大的一处是 `codex-rs/app-server/src/superagent_host.rs`。账本是 SQLite home 下的 `superagent_1.sqlite`，表结构在 `codex-rs/state/superagent_migrations/`。

## 测试

- 引擎：`cd codex-rs && cargo test -p codex-superagent-extension`。推进的测试在 `ext/superagent/tests/drive.rs`，换号的在 `tests/accounts.rs`。
- 端到端：实验室的 `~/242-superagent-codext-lab/tests/scripts/drive_e2e.py`，用 mock 模型跑简单、锚定、换号、Router 换号四个场景。
- 真实模型：在实验室 daemon 上让 Router 推进一个小目标，看通知、`say`、插话、换号是否正常。

## 发到日常

`scripts/superagent/promote.sh` 依次做三件事：构建本机包，更新实验室 daemon，再更新日常 daemon 并替换 `~/.local/bin/codex` 和 `codext`（替换前留备份）。改动先在实验室测，用户同意后再发到日常。

升级上游和更新 superagent 的流程见 [updating.md](updating.md)。
