# 更新计划

本 fork 有两类更新：跟上游 openai/codex 升级，以及更新 superagent 本身。两类都先在实验室测，用户同意后才进 `main`、发到日常。

## 三层

1. **上游**：openai/codex。只跟稳定版 tag `rust-vX.Y.Z`，不跟 alpha、beta、rc。
2. **Codext 底座**：账号池和 `/profile`、状态栏、复制草稿、会话改名、额度恢复、过载重试等。需求清单是 [CHANGED.md](../../CHANGED.md)，0.166 的重做记录在 [reapply-0.166.0.md](../reapply-0.166.0.md)。
3. **Superagent**：引擎 crate `codex-rs/ext/superagent/` 加上它在 codex 里的接入点（[seams.md](seams.md)）。

底座直接从 openai/codex 的 tag 重做，不再等 Loongphy/codext。Loongphy 的分支只在它先做过同一处适配时拿来参考，不从它合并。

## 分支

- `main`：唯一的主线，等于上游稳定版加底座加 superagent。日常仓库 `~/00-nixos-config/codext` 停在 `main`。
- `feat/rust-vX.Y.Z`：一次上游升级。做完快进到 `main`。
- `feat/router-*`：superagent 的新功能。测完快进到 `main`。
- 提交前缀：引擎用 `superagent:`；接入点用所在 crate，例如 `app-server:`、`core:`；底座和其他改动照常写。

## 跟上游升级一次

1. **看有没有新版。**

   ```sh
   git ls-remote --tags --refs https://github.com/openai/codex.git 'rust-v*' \
     | sed 's|.*refs/tags/||' | grep -E '^rust-v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1
   ```

   和 `CHANGED.md` 开头写的当前上游版本比，一样就不用升级。

2. **重做底座。** 按 `.agents/skills/codex-upstream-reapply` 从新 tag 建 `feat/rust-vX.Y.Z`，逐条对照 `CHANGED.md` 在新代码上重做。上游已经做了的条目要删掉记录，不重做。
3. **带上 superagent。** 把 `codex-rs/ext/superagent/` 整个拷过去（包括测试，内容不变），再按 [seams.md](seams.md) 逐个重接接入点。上游改了结构，就按新结构接同一个需求。
4. **构建和测试。** 在 `codex-rs` 下：

   ```sh
   CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo build -p codex-cli
   cargo test -p codex-superagent-extension
   ```

   两条都要通过。

5. **在实验室试。**

   ```sh
   cd ~/242-superagent-codext-lab/codext && git fetch origin && git checkout feat/rust-vX.Y.Z
   scripts/superagent/promote.sh build
   scripts/superagent/promote.sh lab <实验室账号>
   python3 -I ~/242-superagent-codext-lab/tests/scripts/drive_e2e.py ~/242-superagent-codext-lab/build/current/bin/codex
   ```

   端到端的事实要全部通过。之后再用真模型让实验室的 Router 推进一个小目标，确认通知、`say`、插话和换号都正常。

6. **用户验收后再发。** 快进 `main` 并推送；日常仓库切到新的 `main`，再运行 `scripts/superagent/promote.sh daily`。

节奏：上游大约一周出一个稳定版，每个都跟；忙的时候可以攒两个一起跟。

## 更新 superagent

1. 从 `main` 开 `feat/router-<名字>`。只改引擎和必要的接入点；接入点有增减时，更新 [seams.md](seams.md)。
2. 测试：引擎测试，实验室端到端，再用真模型试一次。
3. 用户同意后快进 `main`，用 `promote.sh daily` 发到日常。

## 发布

- `rust-release.yml` 只能手动触发：Actions → rust-release → Run workflow。推送 `main` 不再自动构建五个平台。
- 日常用的是本机构建的包，经 `promote.sh` 安装，不经过 GitHub Release。

## 脱离 Loongphy 还要做的事

这些地方还指向 Loongphy，下次升级时一起改：

- 更新检查和更新命令：`tui/src/updates.rs`、`update_action.rs`、`update_prompt.rs`、`npm_registry.rs`、`history_cell/notices.rs`，`cli/src/doctor/updates.rs`，`cli/src/main.rs`。它们查 Loongphy/codext 的 Release，提示安装 `@loongphy/codext`。那个包没有 superagent，装了可能把日常的 `codext` 和 daemon 换成没有 Router 的版本。改成指向本 fork 的 Release，或者关掉更新提示。在这之前，TUI 提示有新版本时不要照它的命令装。
- npm 包名：`codex-cli/package.json`、`codex-cli/bin/codex.js`、`codex-cli/scripts/install_native_deps.py` 和 `rust-release.yml` 的 npm 发布都用 `@loongphy/codext`，本 fork 没有它的发布权限。
- `rust-release.yml` 里的 `Loongphy/setup-zig` action。
