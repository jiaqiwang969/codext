# Superagent

组织协调引擎，来自 Dell build-27（`a3f27e2f8`），现接入 Codext 0.166。
默认关闭；独立组织 app-server 的有效配置中设 `[features] superagent = true` 才启用。
系统托管配置优先于用户配置；Dell 的现有托管配置会强制开启此功能。
引擎负责工单、输入、指派、报告、发件箱和恢复；宿主通过正常线程接口执行。

## 多项目与入口

`$CODEX_HOME/superagent/pm-threads` 每行先写线程 ID，后可跟
`project=<名> repo=<仓库> base=<分支> worktrees=<工作区根> account=<profile>`。
空行和 `#` 注释忽略，未填写的字段沿用默认值。工单按所属 PM 的项目配置选择
仓库、基线分支和工作区。`account=` 是线程使用的 profile；经 ATM 时，真实出站
账号由网关选择，不能用这个字段证明。

现有部署工具 `dell/add-pm.py` 注册 PM；Mac 上用
`CODEXT_ORG_PROJECT=<名> codext-org` 进入对应 PM。worker 是独立根线程，输入和
报告仍经过协调引擎。Agent command center 的 Router/worker 界面整合尚未实现。

## 状态与隔离

初始化时启用 `experimentalApi`，然后调用无参数的 `superagent/status`。
结果包含 `exitCode`、分组、工单、待处理问题和各部分健康状态：0 正常，2 异常或
工单长时间无进展，3 尚未完成启动或 PM 配置。状态健康不代表模型链路验收通过。

组织账本为 SQLite home 下的 `superagent_1.sqlite`。只有启用引擎才打开它；
发生损坏或迁移失败时保留错误，不用空库自动替换。独立候选必须使用自己的
`CODEX_HOME`、`CODEX_SQLITE_HOME`、socket 和 `CODEXT_SUPERAGENT_ROOT`。最后一个
变量指定 `src/codext`、`wt`、`org` 所在的绝对根目录；未设时沿用组织用户 home。

恢复先对账再分发。有在途但投递结果不明的工单等待人处理，不盲目重放。停机先停止
调度，再等待已开始的投递结束，然后才写 daemon 快照。managed daemon 的快照跳过
组织驱动的线程（PM 线程与未结束工单的 worker 线程），由引擎对账续跑，所以组织
可以运行在日常 managed daemon 中。

## 容量错误

保留 build-27 的模型降级设置，未指定 fallback model 时关闭。自动续跑要求最终
容量或额度错误、没有报告、相同指派，且等待期间没有新输入、报告或活跃工作。
中途重连提示不授权新回合，主动中断也不自动续跑。默认最多 3 次，计数与回执
持久化；设置 `CODEXT_SUPERAGENT_CAPACITY_CONTINUE_MAX=0` 关闭续跑。
旧版缺少失败回合证据的待续跑动作不会被自动执行。

构建、部署状态与尚待真实模型请求验证的边界见
[整合合同](../../../docs/superagent-0.166-integration.md)和
[宿主接口说明](../../app-server/README.md#organization-coordinator-experimental)。
