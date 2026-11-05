# Mac Agent command center × Superagent：接手调研补充

版本：v0.2 调研补充，2026-11-04。配合[评审主文档](superagent-command-center-review.md)使用。状态：**只读调研。没有修改实现，没有运行或新增测试，没有向运行中的任务发送消息。**

源码基线：`c9b4459e86`，已包含 profile 切换修复 `e49ad1451c`。组织引擎另外对照了 build-27 分支 `dell/superagent-integration`。

引用的代码路径默认相对 `codex-rs/`。证据分三级：

- **复核**：接手者本人读源码确认。
- **核实**：调研时读源码确认并给出位置，没有逐行重复复核。
- **推断**：由代码推出，没有运行验证。

真实项目名、逐日数据和统计脚本只保存在本地附件 `~/233-super-agent/verification/mac-command-center-research-20261104/`，本文只引用汇总数字。

## 0. 结论摘要

v0.1 的方向判断成立，但有三类问题被低估了：

- 现有引擎有几处缺陷，会直接妨碍"从面板进入 worker 纠正"。
- 同一个 daemon 中有六套互不知晓的自动续跑机制。
- 历史上"一项目一 PM"和"Dell 专用用户兜底权限"这两个决定都有明确理由，新方向下还没有替代方案。

具体十条：

1. **在 worker 里纠正，引擎收不到。** 引擎靠 `worker-thread:<线程>` 索引识别 worker 线程里的人工输入，但当前源码和 build-27 都没有写入这个索引。人在 worker 里说的话不会入账，Router 也不会知道（复核，G01）。
2. **"查看"可能触发执行。** 面板的"进入"就是 `thread/resume`。resume 结束时会发出空闲事件，持久队列和 Goal 据此发送下一条；而引擎的投递恰好写在这个队列里（复核，F13）。
3. **崩溃后的"不重放"承诺不完整。** 引擎把结果不明的投递标成"等人处理"，但没有撤销已经入队的消息，下次 resume 仍可能执行（推断，高置信，G02）。
4. **六套自动续跑机制互不知晓**：daemon 重启续跑、持久队列、Goal、多 agent 邮箱唤醒、引擎容量续跑、TUI 额度恢复后自动发送。重启后它们之间没有先后顺序。PM 和 worker 都是根线程，都会进入 daemon 的重启续跑名单（核实，F14、F15）。
5. **今天的面板无法承载 Superagent。** `codext agents` 默认启动 managed daemon，而 managed daemon 拒绝启用 Superagent。改走 `--remote` 会失去新建 worktree、按仓库分组等能力（复核，F12）。
6. **"一个总 Router"需要先回应一条历史反证。** 11/03 的实测记录写明：单个 PM 交替处理两个话题时上下文会混，转达也没有项目标签。这正是当初改成"一项目一 PM"的原因（复核，H01）。
7. **Mac 上没有权限兜底。** 历史上 worker 的权限全部放开，安全靠 Dell 上的专用用户：没有 sudo、没有 git 凭据、出口锁定。v0.1 没有权限或隔离的决策项（复核，H02，新增 D13）。
8. **执行器只支持 git worktree。** 家目录 259 个编号目录里，只有 28 个根部是 Git 仓库（核实，F21）。
9. **Router 的输入就是一条不断增长的对话。** 候选卡、报告、确认都以用户消息的形式进入同一个 PM 对话，每条都会触发一个模型回合。系统里没有摘要、话题或工作记录对象（复核加核实，F18）。
10. **改造没有回归保护。** 移植时删除了引擎的全部测试：相对 build-27 删除 8144 行，`test = false`。当前分支又禁止新增测试，而 AGENTS.md 要求界面改动附带 snapshot。实施前必须先定验证规则（复核，G09，新增 D15）。

使用数据（§1）支持"需要一个总 Router"：近 30 天里，每天的活跃会话中位数是 11 个，分布在 7 个项目；顶层回合的并发高峰是 13 个。但真正询问进展的消息只占人工消息约 4%。另外，45% 的"用户消息"是重复的"继续"类短语，大多像是自动发送的。

## 1. 使用量化（近 30 天）

窗口为 2026-10-05 至 11-04，按太平洋时间分日；改按北京时间分日，结论基本不变。数据只读取自两处：`~/.codex/sessions`（10-28 起）和旧备份 `~/.codex-bak/sessions`（此前）。

| 编号 | 观察 | 数值 | 对设计的含义 |
| --- | --- | --- | --- |
| U01 | 规模 | 活跃顶层会话 229 个；每天新建中位数 6，最多 18；涉及 54 个编号项目，外加家目录和其他目录 | Router 面对的是几十个项目、数百个会话，而不是几张工单 |
| U02 | 并发 | 78% 的小时至少有一个回合在跑；忙碌分钟内顶层并发中位数 2、P90 5、最大 13；含子 agent 最大 28 | 引擎现有槽位是全局 6、每 PM 4，单 Router 时实际上限为 4，远低于现有用量 |
| U03 | 同项目并行 | 36% 的"项目-天"有 2 个以上会话活跃，最多 9 个 | R05 是常态，不是特例 |
| U04 | 长会话 | 持续超过 1 天且超过 10 轮的会话有 45 个（20%），承载 55%–72% 的用户消息；它们全部压缩过，次数中位数 17、最多 400；51% 的会话输入峰值超过上下文窗口的 80% | 上下文压力今天就存在，不只是 Router 将来的问题；工作记录必须在多次压缩后仍然有效 |
| U05 | 自动续跑 | 45% 的用户消息是 12 个固定的"继续"类短语；其中 90% 在上一回合结束后 3 秒内，或 95–130 秒后发出；另有 Goal 驱动的回合 663 次 | 已经存在大量自动驱动；纳入引擎管理后必须明确由谁续跑，见 D04 |
| U06 | 进展询问 | 真正询问进展或状态的消息约每天 3 条，占人工消息 3.9%；人工消息长度中位数 24 字；32% 是在回合运行中插入的 | 用户主要是插话和纠正，而不是反复追问进度；面板进展视图的价值在于让用户不必问 |
| U07 | 跨会话引用 | 20% 的活跃会话出现过其他会话的 UUID，其中一半跨项目；最长引用链 5 个会话，最大连通簇 13 个 | R04 的接续关系可以从显式引用中自动提取，作为候选 |
| U08 | cwd 不可靠 | 30% 的活跃会话 cwd 是家目录；8 个会话在运行中途换过目录；`projects` 表为空 | 项目归属不能只靠 cwd，需要显式登记，再辅以候选推断 |
| U09 | 子 agent | 窗口内新建 1146 个，但只有 25% 的顶层会话用过；10-25 之后几乎停用（每周 7 个）；97.6% 为一层深 | 需要支持 helper 管理，但它不是主要负载 |
| U10 | 中断 | 中断 265 次，涉及 43% 的会话；中断后到下一条消息的间隔中位数 12 秒 | 中断大多是为了改方向，纠正和接管动作要快 |

局限：

- 自动续跑是靠重复短语和时间间隔启发式识别的，没有核实来源是排队、定时器还是快捷输入。
- 会话引用只统计用户消息里显式出现的 UUID。
- 话题漂移无法自动判断。

## 2. 已核实的现有缺陷

这些问题与新设计无关，今天的代码里就存在。评审可以决定其中哪些先作为独立缺陷修复。

| 编号 | 问题 | 依据 | 证据 |
| --- | --- | --- | --- |
| G01 | worker 线程里的人工输入不入账 | intake 用 `worker-thread:<id>` 判断是不是 worker 线程（`ext/superagent/src/core/engine.rs:809-815`、`ext/superagent/src/intake.rs:198-211`），但当前源码和 build-27 都没有写入方。因此 `app-server/README.md` 里"worker 的人类输入也会被记录"的说法不成立，也违背了历史决定 #9（人可以直接对 worker 说话） | 复核 |
| G02 | 结果不明的投递仍可能被执行 | 投递 = resume 后写入持久队列（`app-server/src/superagent_host.rs:290-300`）；队列在 resume 后自动发送（`ext/queue/src/service.rs:129-135, 538-547`）；对账只把工单标为 blocked（`ext/superagent/src/executor/codext_thread.rs:472-500`），不撤销队列项 | 推断，高置信 |
| G03 | 第 1000 张工单起，显示名失效 | 生成格式为 `t{n:03}`，会产生 `t1000_…`（`ext/superagent/src/core/tickets.rs:123`）；解析只认 3 位数字（`ext/superagent/src/core/thread_rules.rs:54-64`）。计数按 PM 累计，超过后点名直达和提及解析全部失效 | 复核 |
| G04 | Router 的候选卡看不到疑似重复的工单 | 新请求线程没有订阅，所以 `related_tickets` 恒为空；`referenced` 只认 32 位哈希 id，不认 `tNNN` 显示名（`ext/superagent/src/pm.rs:288-296`）；正文提及写进了 `meta.mentions`，但没有读取方（`ext/superagent/src/intake.rs:121-131`） | 核实 |
| G05 | status 里 worker 的线程 id 永远为空 | `Assignment.executor_ref` 和 `Execution.directory` 从未写入（`ext/superagent/src/status.rs:463`）。工单与线程的映射只能从 kv `codext-session:<ticket>` 重建 | 核实 |
| G06 | 崩溃窗口内部分消息可能重复 | effect 处理完才删除，属于至少一次语义，失败时无限重试（`ext/superagent/src/effects.rs:587-636`）。回复、转达和候选卡可能重复 | 推断 |
| G07 | 引擎就绪前到达的用户输入被直接丢弃 | 运行时未就绪时，`record_turn` 直接返回，不留回执（`ext/superagent/src/intake.rs:195-197`） | 复核 |
| G08 | 源码硬编码 Dell 部署默认值，包括两个账号 profile 名 | 包括：`/home` 下的路径、`superagent/integration` 基线分支、两个固定 profile、6/4/3 并发上限、15M token 预算（`ext/superagent/src/executor/codext_thread.rs:138-161`）。v0.1 说"仓库不保存账号标识"，这里与之不一致 | 复核 |
| G09 | 引擎测试全部移除 | 相对 build-27 删除 8144 行，主要是测试；Cargo 设 `test = false`。本分支又禁止新增测试 | 复核 |
| G10 | 移植残留，影响小 | `app-server/src/request_processors/thread_lifecycle.rs:627-678` 与 `679-730` 两段代码重复，后一段不可达（是否来自上游未查）；`ext/superagent/src/recovery.rs` 引用的行号已经漂移 | 核实 |

## 3. 对 v0.1 F01–F11 的修正

| 条目 | 判断 | 补充或更正 |
| --- | --- | --- |
| F01 | 准确，但不完整 | 协议共 7 个方法，还有 read/move/delete，全部是实验接口。create 必须带 `idempotencyKey`。root 可以是尚不存在的路径，path 没有唯一约束。`threads.project_id` 是单列，所以一个线程最多属于一个项目。projectId 不从 cwd 推导，只能在 `thread/start`、`thread/metadata/update` 或 `project/import` 时显式绑定。list 不能按 root 过滤 |
| F02 | 准确，但不完整 | 分组有 Project、Directory、Status、Model 四种，外加置顶组。分组 key 含相对 cwd，所以同一项目的不同子目录会被拆成不同组。只有根线程参与分组。没有使用 projectId。remote 模式下 Project 分组退化为 cwd |
| F03 | 基本准确 | 遗漏三点：用户输入没有回执；effect 是至少一次语义；恢复流程不覆盖持久队列（G02） |
| F04 | 方向对，但有误导 | worker 输入归属实际失效（G01）。正文提及没有被用作上下文。现有设计本来就是每条新请求一个领域线程，不存在"整段 Router 对话被订阅给工单"的现状风险 |
| F05 | 准确 | 补充：一次 route 调用最多 20 项；scope 就是标题，没有验收字段；推断出的候选不可接受；accept 之后，同一请求还能 attach 到别的工单，这是扇出而不是拆分 |
| F06 | 准确，但不完整 | 项目在 launch 时按"工单首个来源 → PM → 项目"解析，不保存在工单上。强依赖 git worktree。单 Router 时，每 PM 上限 4 就是全局上限。另见 G08 |
| F07 | 准确，但不完整 | 冲突不止 daemon 续跑：resume 发出的空闲事件还会触发队列和 Goal。PM 线程同样会被续跑。两套恢复之间没有先后顺序。面板强制使用 managed daemon（F12）。如果在日常配置里打开 superagent，日常 daemon 会直接启动失败 |
| F08 | 方向对，表述需收紧 | `canAcceptDirectInput=false` 只针对 v2 的 ThreadSpawn 子 agent，线程未加载时该字段为空。Superagent worker 是根线程，值为 true，原生层面没有任何所有权屏障。`turn/interrupt` 可能不受这个守卫约束（推断） |
| F09 | 数值准确，但有误导 | 新请求的相关工单恒为空（G04）。候选卡的 batch/handed_back 和报告的 details 数组都没有长度上限 |
| F10 | 准确 | worker 没有读取工具（`ReadScope::Worker` 从未被使用）。没有搜索或筛选：`store.search` 存在，但没有对外暴露 |
| F11 | 准确，而且说轻了 | 另有多处全量扫描：status 读取全部决定记录；启动对账扫描全部工单；不带工单 id 的 answer 扫描全部工单；outbox 每发一条消息都遍历全部会话记录。工单以整块 JSON 存储，每次保存都全量重写 |

## 4. 新增事实（整合的关键约束）

| 编号 | 事实 | 依据 | 证据 |
| --- | --- | --- | --- |
| F12 | 面板与 Superagent 的部署冲突 | 不带 `--remote` 时，`codext agents` 启动 managed daemon（`cli/src/main.rs:2567-2584`）；managed daemon 拒绝启用 Superagent（`app-server/src/lib.rs:603-607`）；`unix://` 不带路径时默认使用 daemon 的 socket（`app-server-transport/src/transport/mod.rs:130-143`） | 复核前两条，核实第三条 |
| F13 | 查看即可能续跑 | 附着运行中线程和冷恢复两条路径，结束时都会发出空闲事件（`app-server/src/request_processors/thread_lifecycle.rs:1259-1264`、`thread_processor.rs:4690-4692`）；持久队列在非中断的空闲时发送队首；Goal 处于 Active 时，每次空闲启动一个续跑回合。因此面板"查看"应改用 `thread/read` 加 `thread/turns/list`，或在 resume 前先做所有权检查 | 复核 |
| F14 | 六套自动驱动互不知晓 | ① daemon 重启续跑（`request_processors/daemon_continuation.rs`）；② 持久队列，空闲时发送，另有每 10 秒的唤醒监视（`ext/queue/src/service.rs`）；③ Goal 空闲续跑（`ext/goal`）；④ 多 agent 邮箱唤醒（`core/src/session/handlers.rs:81-100`）；⑤ 引擎容量续跑，每个指派最多 3 次（`ext/superagent/src/executor/codext_thread/capacity.rs`）；⑥ TUI 额度恢复或切号后自动发送暂存的提示，每个已附着的 TUI 各自运行（`tui/src/chatwidget/turn_runtime.rs:481-495`）。重启时引擎启动与 daemon 恢复是两个独立任务，之间没有等待 | 核实 |
| F15 | daemon 重启续跑的范围 | 进入快照的条件：已加载、非 ephemeral、无父线程、不是非根 agent（`app-server/src/request_processors/daemon_snapshot.rs:15-55`），所以 PM 和 worker 都在名单里。快照只在计划内重启时写入，包括 restart、update，以及每小时检查一次的自动更新器；崩溃、`stop` 或全新 `start` 不会恢复。每个中断回合只续跑一次 | 复核条件，核实其余 |
| F16 | 原生层面没有所有权标记 | worker 和 Router 在来源类型、根线程身份、持久化方式上都与普通会话没有区别，归属信息只存在于引擎库和 `pm-threads` 文件中。可作为候选的标记位置是线程附件：`thread/attachment/add` 支持幂等写入，`thread/attachmentOwner/list` 可按 `(type, key)` 反查线程 | 核实 |
| F17 | worker 在面板中通常显示为 Inactive | 引擎创建线程后会退掉占位订阅（`app-server/src/request_processors/superagent.rs:7-15`）；没有订阅者的空闲线程默认 60 秒后卸载。面板把未加载线程显示为 Inactive，即使工单仍在进行 | 复核 |
| F18 | Router 每轮看到什么 | turn-input 贡献者只记录输入，不注入任何内容（`ext/superagent/src/intake.rs:163-181`）。用户原话、被插入的候选卡、排队的报告转达、确认与待决问题，全部以用户消息进入同一条 PM 对话，每条都触发一个回合。报告里的 changes/tests/risks/evidence 数组不截断。对话大小只受 Codext 通用压缩约束 | 复核贡献者，核实其余 |
| F19 | Router 只有 7 个工具 | 工具策略只放行 `superagent_route/read/ticket_note/ticket_retry/request_status/trigger_requeue` 和 `update_plan`（`app-server/src/superagent_host.rs:58-80`）。Router 不能列出原生会话，不能读项目文件，不能建目录或登记项目。账本只认识它自己建的工单，所以 R04/C02 要求的"识别在谈哪个 session"目前没有数据来源 | 复核 |
| F20 | 工单没有验收态 | 报告 ready 且没有 follows 时，工单立即结束为 Completed（`ext/superagent/src/core/assignments.rs:284-313`）。Merged、Closed 和"等人评审"阶段实际不可达。由 Executor 结束的工单，人发来消息会被唤醒；由人结束的永不复活 | 复核 |
| F21 | 执行器的形态 | 每张工单执行 `git worktree add -b superagent/<name>`，不支持非 Git 目录（`ext/superagent/src/executor/codext_thread.rs:711-734`）。账号池每个 profile 最多 3 个 worker；旧会话的账号不在池内时直接失败。槽位规则为全局 6、每 PM 4，人工消息可多占 1 个（`ext/superagent/src/executor/slots.rs:48-84`） | 复核槽位，核实其余 |
| F22 | 引擎配置来自环境变量 | 引擎参数读取 `CODEXT_SUPERAGENT_*` 环境变量（`ext/superagent/src/lib.rs:200-299`）；daemon 拉起子进程时只传 `-c features.*`，环境在 daemon 启动那一刻固定。`max_workers` 和 `max_per_pm` 没有可调入口 | 核实 |
| F23 | 面板结构 | 面板是单个会话界面里的全屏视图，"进入"会替换当前会话，其他空闲线程随之退订。只有根线程成行，没有执行树。没有给线程发消息的输入框。草稿和排队消息只存在客户端内存里。每页加载 10 条，搜索只在已加载的行中进行。`thread/loaded/list` 只在首次加载时调用。⇧P 切换账号作用于当前已进入的线程，而不是选中的行 | 核实 |
| F24 | 账号切换的副作用 | 切换线程账号会顺带改掉该连接的默认 profile（`app-server/src/request_processors/thread_auth_profile.rs:136`）；引擎按启动时记录的账号统计负载，面板切号后计数会与实际不符；已在运行的子线程沿用旧运行时（推断） | 核实 |

## 5. 历史研究与教训（v0.1 未吸收）

来源路径相对家目录。PL 指 `233-super-agent/plan/codext-superagent-plan.md`，T03 指 `233-super-agent/plan/design/t03-engine-design.md`，ARCH 指 `239-jtufem/org-run/ARCH-NOTES.md`，CASES 指 `237-codext-org/` 下的实测用例库。

| 编号 | 历史内容 | 对新方向的意义 | 来源 |
| --- | --- | --- | --- |
| H01 | 改成"一项目一 PM"的理由：PM 是一条对话，两个话题交错时上下文会混，转达给用户时没有项目标签；"上下文膨胀"的验证没有出结果 | R01 必须正面回答：总 Router 如何避免当时观察到的混淆。这一条应进入 D01、D06 | ARCH O3（复核） |
| H02 | 权限全放开，兜底是 Dell 专用用户 `codext-org`：没有 sudo，没有私钥和 git 凭据，出口锁定。v1.0 的原话是，如果放在 Mac 上，"能改你所有文件、推任何仓库、登录机群" | 迁回 Mac 后兜底消失，需要新的决策项 D13 | PL §2.9、v1.0 计划 §8（复核） |
| H03 | 拆单的既有倾向："规划工单 → 人批准 → 每项各开一单"；A0 定过"每轮最多开一张工单"；ARCH O1 建议加一个很薄的 Project/Goal 对象，汇总多张工单，但不让 PM 自己规划 | D01、D02 已有可以直接评审的候选，不是从零开始 | ARCH O1（复核）、PL:339、v1.0:155-159 |
| H04 | 实测用例库：A4-2"刚才那个公差样例"（新 worker 没有上下文，改错了代码）、B3 相似不等于指向、B4 点名后 worker 越界（只改了提示词，未根治）、退回后新工单扇出、过时候选卡、PM 忙时插话被丢 | 应作为 D03、D05 的回归基线；B4 仍未解决 | CASES/results、T03:673-677 |
| H05 | ready 不等于验收；medium 强度交出 26 行空壳却报 ready；引擎侧检查清单和验收组都没做 | 新增决策项 D14 | `239-jtufem/org-run/RUN1-RETRO.md:18`、PL:214-234 |
| H06 | 健康不等于进展；长工单 3.5 小时没有动静；外部巡检用另一个账号，按退出码 0–3 行事；工单加了"最近动态"字段，但跨工单发现的回流通道没做 | v0.1 没有停滞检测或巡检，归入 D14 | ARCH:91-99、T03:587-640 |
| H07 | 已定的否决和条件：Goal 预算与续跑被否决，因为会与执行器争抢生命周期；续跑需同时满足四个条件；resume 必须重新套用角色；指令在线程创建时固定，改规则对已有线程无效 | D04、D05 应直接继承 | T05:44-57、ARCH:97, 109 |
| H08 | 容量错误：五个账号加用户本地会话同时中招，两次停机；成因有三种互相矛盾的说法；同一账号在个人会话和组织两处同时在线的影响未决；managed 配置层压过了角色文件，所有 worker 实际一直按 max 强度运行，生效值不可见 | Mac 上同一个 daemon 会让 Router、worker 和个人会话共用账号，正好落在这个风险上；生效配置应在面板可见 | `239-jtufem/org-run/LOG.md:80-88`、`SEAL.md:22, 37`、RUN1-RETRO.md:15-19 |
| H09 | merge-map 曾决定删除原型的 React 面板，只保留 status JSON；原型面板按"三个问题"组织，原则包括"计数等于列表长度"和"健康状态不占空间"，动作有 stop/dismiss/unsubscribe/pause | R06 需要显式撤销这个删除决定；原型的面板原则和动作集可以复用；引擎已有这些动作的方法，但没有外部调用方 | `233-super-agent/plan/design/merge-map.md:136-140`、`233-super-agent/dsh-superagent-prototype/docs/005-panel.md` |
| H10 | 组织线程关闭 Codext memories，以免跨工单串味；项目级 `notes=`/`refs=`；工单卡要写明输入清单，否则 worker 把仓库历史当成任务（一次浪费 3.9M token） | D06 的记忆方案要与"关闭 memories"的理由一起评审 | merge-map.md:204、ARCH:43-45 |
| H11 | 文档之间的矛盾：PL:624 附录仍写"原生工具 + 后台引擎"为 A1 主线，与改定后的决定 #7 冲突；T03:550 把"已发出"等同于"已显示"，与 v0.1 §6.2 要求区分"已保存/已投递/已确认/已完成"冲突 | 评审前应清理，避免引用到过时结论 | PL:389, 624、T03:550 |
| H12 | 研究笔记建议做"纯 Codext 对照"的消融实验，并提醒"单人连续给一个项目下指令，外层未必划算" | 可作为 D12 分阶段发布的退出条件 | `233-super-agent/research/superagent-innovation/analysis.md:85-87` |

## 6. 对决策项的补充输入

本节不作决定，只列出新证据及其含义。

| 决策 | 新证据 | 含义 |
| --- | --- | --- |
| D01 话题模型 | H01、H03、F18；U04 显示长会话平均压缩 17 次以上 | Topic 不只是展示层对象：它要承担"让一个 Router 不混淆多个项目"的职责，并在 Router 压缩后仍能重建当前目标 |
| D02 Router 职责 | F05、F19、H03 | 现有引擎一请求一工单；历史倾向是"规划工单 + 人批准"。如果总 Router 还要识别旧会话（R04），工具集必须扩展到原生会话和项目查询 |
| D03 输入归属 | G01、G03、G04、F23；H04 用例库 | 现有直达规则在规模上有硬上限（t1000），疑似重复检测实际无效；面板的⇧P 已经是一个"目标歧义"的现成例子 |
| D04 生命周期 | F13、F14、F15、G02、H07；U05 显示大量自动"继续" | 所有权不只涉及 daemon 和引擎，还涉及队列、Goal、邮箱唤醒和 TUI 自动发送。候选规则：每个根线程任一时刻只有一个持久登记的所有者，所有自动驱动在发送前查询所有者；查看用只读接口 |
| D05 人工接管 | G01、F08、F16、F23；H04 的 B4 | 前提是先修 G01，否则接管后的决定不会进入账本。worker 是根线程，原生层任何客户端都能直接发消息，接管需要引擎侧的控制状态，而不是 UI 开关 |
| D06 持久记忆 | F18、F20、H10；U04 | 目前没有任何摘要或工作记录对象；最接近的是 600 字报告、120 字最近动态和决定记录。memories 关闭的理由需要和新的记忆方案一起评审 |
| D07 查询与通知 | F11 的补充、F18；U01、U02 | 每条通知都会触发一个 Router 回合；数据规模是几十个项目、数百个会话、并发十余个。领域事件总线已存在但没有订阅者，可以作为面板和合并层的接入点 |
| D08 项目创建 | F01、F21、U08 | 执行器的 git worktree 前提与"项目根多数不是 Git 仓库"直接冲突；一个线程最多属于一个项目，嵌套子项目需要另行表示 |
| D09 旧任务纳管 | U07、F16 | 显式 UUID 引用可以自动生成候选关联；纳管标记可以放在线程附件里 |
| D10 面板信息架构 | F12、F17、F23、H09 | 面板目前没有对话区，"进入"即替换会话；只有根线程成行；引擎 worker 会显示为 Inactive。原型面板的原则可以复用 |
| D11 资源与账号 | F21、F22、F24、G08、H08；U02 | 现有并发上限远低于实际使用；账号池写死；Router、worker 与个人会话共用账号的风险没有处理 |
| D12 发布与回退 | G09、F22、H12 | 没有测试覆盖，配置靠环境变量；建议把"与纯 Codext 对照"作为阶段退出条件 |

建议新增的决策项：

| 决策 | 具体问题与候选方向 | 需要的评审输出 |
| --- | --- | --- |
| D13 权限与执行隔离 | worker 在 Mac 上以用户身份运行。采用什么沙箱模式？每张工单的写入边界是 worktree、项目子目录还是只读？推送和凭据怎么限制？组织配置和钩子能否放在 worker 不可写的位置？ | 权限矩阵，以及与 Dell 专用用户方案的对照 |
| D14 验收质量与巡检 | ready 由谁验收？如何检测停滞？"健康不等于进展"怎么呈现？生效配置和最近动态是否在面板可见？ | 工单验收态、停滞阈值、巡检归属 |
| D15 实施阶段的验证规则 | 当前重做分支禁止新增测试和 snapshot，但 AGENTS.md 要求界面改动附带 snapshot；引擎测试已全部删除。是否在独立分支恢复 build-27 的测试？是否以 CASES 用例库作为验收基线？ | 实施分支的检查规则，以及回归基线 |

## 7. 建议的评审顺序

这是排序建议，不是实施授权。

1. **先定，会阻塞实施：**
   - D04：续跑所有权，涉及 F13–F15 和 G02。
   - D13：权限与隔离。
   - D15：验证规则。
   - D01/D06：总 Router 如何不混淆，回应 H01。
2. **可以并行评审：** D03、D05（取决于 G01 的修复方向）、D08（取决于 git worktree 的去留）、D10。
3. **可以后置：** D07（U01、U02 已给出数量级）、D09、D11、D12、D14。

G01–G03 是今天就存在的缺陷，与新设计无关。评审可以决定是否先在现有结构上独立修复。

## 8. 验收基线补充

在 v0.1 §8 的基础上增加：

- 以 CASES 用例库的 A–F、S1 两轮结果作为 D03、D05 的回归基线，重点覆盖 A4-2、B3、B4、退回后扇出、过时候选卡和忙时插话。
- 在面板中查看 worker 时，不触发任何新回合。
- 人在 worker 里补充的纠正，Router 能看到，并记入原工单。
- 崩溃后结果不明的投递，在人确认之前不会因为任何 resume 而执行。
- 同一个 PM 下开出第 1000 张工单后，直达和提及仍然有效。
- 在非 Git 的编号项目里建工单，能按项目习惯落地。
- daemon 计划内重启后，PM 和 worker 不会被两套机制重复续跑。
- 面板切换账号后，引擎的账号负载计数与实际一致。

## 9. 调研方法

- **源码：** 读了 Superagent 引擎（`ext/superagent`，约 16k 行，对照 build-27）、app-server 的线程生命周期、恢复、输入、项目与账号处理，以及 TUI 面板（`tui/src/app/agents_overview*`、`agent_center/`）。
- **历史资料：** 读了 `~/233-super-agent` 的计划、设计、验证与原型文档，`~/239-jtufem/org-run` 的运行记录，以及 `~/237-codext-org` 的用例库。
- **使用数据：** 流式读取会话记录，并以只读方式打开状态数据库，只输出聚合数字，不保存消息正文。
- **未做的事：** 没有连接 daemon，没有读取凭据，没有运行构建或测试。
