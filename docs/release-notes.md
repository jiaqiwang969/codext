# codext 0.167.0：Superagent Router 版

以 openai/codex `rust-v0.167.0` 为底，加上 Codext 底座和 Superagent。Superagent 由两部分组成：常驻 daemon 里的推进引擎，以及 Router 会话。这一版已经在作者的日常环境里运行，正推进一个六路并行的有限元项目。

## 这是什么

- **Codext 底座**（细节见 [CHANGED.md](https://github.com/jiaqiwang969/codext/blob/main/CHANGED.md)）：
  - 账号池：多个 ChatGPT 账号可以按会话切换，`/profile`，`codex-account-next`。
  - 状态栏显示模型、目录、额度和账号。
  - `auth.json` 热重载。
  - 改用本地验证过的包更新。
- **Superagent**：
  - Router 是一个常驻会话，替你看本机的所有会话，并按你的意思推进其中几路。每一路是一个文件夹加一个工作会话，可以锚定到工程目标。
  - 引擎是 daemon 里的程序，不花额度，负责记账、核对、等待、巡检和换号。
  - 说明见 [docs/superagent/README.md](https://github.com/jiaqiwang969/codext/blob/main/docs/superagent/README.md)。

## 这一版新增

**Router 和推进**
- **回话依据更宽**。Router 对工作会话说的每句话，都要写明接着哪句原话、为了什么，由引擎核对。可以引用的出处扩大到：用户对 Router 说过的话、长期规矩、这一路最近 5 条结论、别的路的结论。编造的、带方法名或编号的话，照样退回。
- **等待**。一路要等别的路交结论、等一段时间、等某个文件、等用户回话时，Router 写明在等什么，等到了由引擎叫醒它。
- **长期规矩**。用户说的、今后都有效的话，照抄原话记进引擎账本，可以对全部、某个文件夹或某一路生效。上下文压缩、daemon 重启后都还在，也能当回话的依据。
- **少打扰用户**。只有方向偏了、真的没进展，才停下来要用户定，而且接着做时必须带上用户的原话。记录写错、判定程序出错这类事，由 Router 让工作会话自己补。
- **按项目分 Router**。大项目可以有自己的 Router（`router.py create <项目> <目录>`），已有的推进用 `adopt` 接过来，通知只送给开它的那个 Router。
- **起草者**（默认关，按路打开）。一路交了结论，先由起草会话写下一句，核对通过就直接发。

**引擎**
- **事件流**。每次状态、步数、账号变化都追加到 `$CODEX_HOME/superagent/events.jsonl`；`superagent/status` 的 `drives` 按同样的形状列出每一路。
- **每分钟巡检**。在每路的清单行上标出：交了结论半小时没人回、做了一个半小时文件夹没动、步数快用完、原地打转、一个账号上的会话太多。
- **更稳**：
  - 回合结束以钩子为准，不再卡在"正在做"。
  - 暂停期间做完的那一步，结论照样留着。
  - 网络断开或服务出错时，45 秒后重发一次。
  - 通知先攒着，合成一条送，过期的不送。
- **只在 unix socket 的 daemon 里跑**，每个 `CODEX_HOME` 一把锁。对同一目录跑 `codex exec`，不会再多起一个引擎。

**换号**
- **提前换**。剩余额度低于软阈值时，在两轮或两步之间换号；低于硬阈值、在冷却中或账号不可用时，同样在交界处换。Router 的一轮里第一次报满载或额度用完，就打断、换号，再把原话交回（`CODEXT_SUPERAGENT_ROUTER_RESCUE_THRESHOLD`）。
- **分散**。每个账号上最多 4 路（`CODEXT_SUPERAGENT_MAX_PER_ACCOUNT`）。额度用完的账号一读到额度回来，冷却就提前结束。
- **重置卡**（默认关，`CODEXT_SUPERAGENT_RESET_CARDS_PER_DAY`）。没有账号能接手时，引擎可以按规则用卡：只给已经用完、而且离自然重置还远的账号用。

**终端界面**
- 引擎、Router 或其他客户端给一个会话换号后，正开着这个会话的窗口，状态栏的账号、额度和"weekly limit"提示会当场跟着换。这是新增的 app-server v2 通知 `thread/accountProfile/updated`。

## 设置项（环境变量，在启动 daemon 时给）

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `CODEXT_SUPERAGENT_QUOTA_WATCH_SECS` | 120 | 多久看一次 Router 账号的额度；0 关掉定时查看 |
| `CODEXT_SUPERAGENT_QUOTA_SOFT_PERCENT` | 10 | 低于它就在两轮之间换号 |
| `CODEXT_SUPERAGENT_QUOTA_HARD_PERCENT` | 3 | 低于它算快用完 |
| `CODEXT_SUPERAGENT_ROUTER_RESCUE_THRESHOLD` | 2 | Router 一轮里满载或用完几次就打断换号（建议 1） |
| `CODEXT_SUPERAGENT_MAX_PER_ACCOUNT` | 4 | 每个账号最多几路 |
| `CODEXT_SUPERAGENT_RESET_CARDS_PER_DAY` | 0 | 引擎 24 小时内最多自己用几张重置卡 |

## 验证

- 引擎测试全部通过；`codex-app-server-protocol` 322 项、`codex-app-server` 库 425 项通过，另有新增的服务端和界面测试。
- 模拟模型的端到端：推进 39 项、Router 20 项全部通过，服务日志没有错误。
- 真实模型在实验室试跑：
  - 按指定账号开推进；
  - 长期规矩写进账本；
  - 等的文件出现后，50 秒内叫醒 Router；
  - Router 的回话通过核对，"做到哪停"由引擎接在话后面。

## 安装

- 从下面的附件里下载对应平台的压缩包：macOS arm64/x64、Linux x64/arm64（musl）、Windows x64。
- 打开 Superagent：
  1. 在 `config.toml` 里写 `[features] superagent = true`；
  2. 在 `~/.codex/superagent/router-mode` 里写 `drive`；
  3. 用 `scripts/superagent/router.py create` 建 Router。
- 从源码装、更新和回退，见 [docs/superagent/updating.md](https://github.com/jiaqiwang969/codext/blob/main/docs/superagent/updating.md)。

## 已知限制

- 不发布到 npm：npm 上的包名属于上游 codext。
- 子 agent 一类的会话在后台被换号时，窗口要等下次打开它才对齐。用共享默认账号的会话，窗口没法切回默认账号。
- 实验室里见过，Router 偶尔把"做到哪停"这类本该自己掌握的话转告给工作会话；不影响后面的等待和推进。
