# Superagent

## 多项目

`$CODEX_HOME/superagent/pm-threads` 每行先写线程 ID，后可跟
`project=<名> repo=<仓库> base=<分支> worktrees=<工作区根>`。空行和 `#` 注释忽略，未填写的字段沿用默认值。引擎按工单所属 PM 的项目配置选择仓库、基线分支和工作区根来创建 worktree。dell 上运行
`dell/add-pm.py <项目名> <仓库路径> <基线分支> <工作区根>` 注册 PM；Mac 上用
`CODEXT_ORG_PROJECT=<名> codext-org` 进入对应 PM。
