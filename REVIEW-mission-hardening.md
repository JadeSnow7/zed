# 变更审查请求：Mission / Shared Context 加固（两轮，共 15 个文件）

> **这份文档给谁看**：Claude Code（在本机、有 cargo、有网络的环境里）。
> **要你做什么**：按第 6 节的门禁逐条验证。**全部通过后**，按第 8 节分组提交；
> 任何一条不通过，停下来报告，不要自行改写设计意图。
> **这份文件本身不要提交**，见第 8 节。

改动作者：Claude（Cowork 会话，运行在云端沙箱里）。
**关键限制：作者无法编译。** 该会话连接的设备 VM 里没有 cargo，也没有网络出口
（代理返回 403），仓库的 `target/` 也不在挂载视图中。所有改动都只经过静态复查，
**一行代码都没有被编译或运行过**。这是本次审查存在的根本原因，请据此调整信任度：
把第 5 节当成重点排查清单，而不是走个过场。

---

## 1. 背景

这批改动源自一次用 fable5 做的仓库审计。审计范围是这个 fork 相对上游
`zed-industries/zed` 的 delta（10 个提交 / 约 7,750 行：Mission、Shared Context、
Worker Dashboard），不涉及上游代码。

审计报告的事实层被逐条核实过，基本准确。核实结论摘要：

| 审计论断 | 核实 |
|---|---|
| `shared-context-mcp` 未被打包 | 成立，且**三个** bundle 脚本都缺，不只 mac |
| `shared_context.sqlite` 无 WAL / busy_timeout | 成立 |
| `set_thread_mission` 在 entry 不存在时静默丢弃 | 成立 |
| 分支过不了 `./script/clippy` | 成立（`disallowed_methods = "deny"` + `--deny warnings`） |
| `author` 语义过载导致归因断裂 | 成立 |
| Context/Evidence tab 打开后永不刷新 | 成立（`refresh_mission_context` 只在两个构造函数里被调用） |
| 文档漂移三处 | 成立，实际是**四**处 |

审计**漏掉**一条，作者补上并列为 P0：fork 把自己的迁移追加进了上游
`ThreadMetadataDb` 域的数组尾部，而 `sqlez` 按 **index** 比对已存储与当前迁移，
不匹配直接 `bail!`（`crates/sqlez/src/migrations.rs`），`crates/db` 随后退到内存
fallback。上游日后在同一 index 新增迁移 → 一次 rebase 之后表现为数据丢失，而不是
合并冲突。

作者也**修正过自己的一处夸大**：`db.sqlite` 路径已经带 `0-{release_channel}`
（`crates/db/src/db.rs` 的 `db_path`），本 fork 走 dev 通道，所以"与正式版 Zed 撞库"
只发生在同时装了 **Zed Dev** 的机器上，范围比最初说的窄得多。rebase 撞 index 那半
不受影响，仍是硬伤。

---

## 2. 变更清单

```
 M .github/workflows/agent-workspace-macos-build.yml    +30
 M crates/agent_ui/src/mission_context_observer.rs      +/-58
 M crates/agent_ui/src/mission_orchestrator.rs         +/-173
 M crates/agent_ui/src/mission_panel.rs                 +/-54
 M crates/agent_ui/src/mission_views.rs                 +/-85
 M crates/agent_ui/src/thread_metadata_store.rs        +/-246
 M crates/agent_ui/src/worker_dashboard.rs              +/-23
 M crates/shared_context/src/bin/shared_context_mcp.rs  +/-21
 M crates/shared_context/src/mcp_server.rs              +/-32
 M crates/shared_context/src/shared_context.rs         +/-174
 M crates/shared_context/tests/stdio_integration.rs     +/-13
 M script/bundle-linux                                   +6
 M script/bundle-mac                                     +7
 M script/bundle-windows.ps1                            +10
?? .github/workflows/agent-workspace-lint.yml          （新文件）
```

上游文件（`crates/db`、`crates/sqlez`、`crates/paths`、`crates/gpui*`）**一行未动**。

---

## 3. 逐条变更

### 3.1 迁移域隔离（P0）

**动机**：见第 1 节。`ThreadMetadataDb::MIGRATIONS` 尾部被追加了 fork 自己的迁移，
索引空间与上游共享。

**改法**：新增 `MissionDb` 域承载 `missions` 表与两个 `ALTER TABLE sidebar_threads`，
用 `db::static_connection!(MissionDb, [ThreadMetadataDb])` 拿依赖排序
（`crates/vim/src/state.rs` 的 `static_connection!(VimDb, [WorkspaceDb])` 是同样用法）。
同一个 `db.sqlite`——域只是迁移命名空间——所以跨表查询不变。

包在 `mod mission_db { #![allow(dead_code) ... }` 里，因为 `static_connection!` 会
生成没人调用的连接访问器（`global` / `open_test_db`），而 `script/clippy` 用
`--deny warnings`。

**连带修的测试夹具**（不修就会挂，且挂的不只是 Mission 测试）：

- 5 处 `db::open_test_db::<ThreadMetadataDb>` → `::<(ThreadMetadataDb, MissionDb)>`。
  不改的话测试库里没有 `mission_id`/`role` 列，而 `ThreadMetadataDb::LIST_QUERY`
  会 select 它们，**每一个** sidebar 相关测试都会失败。
- `run_thread_metadata_migrations` 改为跑两个域。
- `test_mission_migration_preserves_existing_threads_with_null_mission_and_role`
  里的 `MIGRATIONS[..len - 1]` 索引算术删掉了，改成"只跑上游域"。

**验收**：Gate A + B + C。

---

### 3.2 `shared_context.sqlite` 加 WAL 与 busy_timeout（P1）

**动机**：这是全系统唯一被多进程并发读写的库（Zed 进程内 observer + 每个外部
Harness 一个 `shared-context-mcp` 子进程），却是唯一没有并发防护的。默认
`journal=DELETE` 下写事务会阻塞读；默认 `busy_timeout=0` 下写-写冲突立即返回
`SQLITE_BUSY`，而写失败只 `log::error!` 后丢行，没有重试。

**改法**：`SharedContextStore::open` 补 `with_db_initialization_query`，对齐
`crates/db` 的 `DB_INITIALIZE_QUERY`（WAL + busy_timeout=500 + synchronous=NORMAL）。
注释里写清两个 PRAGMA 各自解决什么（WAL 管读-写，busy_timeout 管写-写，缺一不可），
以及同步盘下 `-shm` 的限制。

---

### 3.3 clippy（P1）

`crates/shared_context/tests/stdio_integration.rs` 的 `McpProcess::spawn` 用了 4 个
`disallowed_methods`（`std::process::Command` 的 `stdin`/`stdout`/`stderr`/`spawn`）。

**改法**：函数级 `#[allow(clippy::disallowed_methods)]` + 理由注释。
`crates/project/tests/integration/project_tests.rs` 有 12 处同样的先例。
没有改用 `smol::process::Command`，因为 `smol::process::Command::from()` 不保留
stdio 配置，而这个测试的全部意义就是走真实管道。

---

### 3.4 三平台打包 + 绝对路径 + DB env（P0）

**动机**：`ensure_shared_context_server` 往用户 settings 写裸命令名
`"shared-context-mcp"`，靠 PATH 解析；而三个 bundle 脚本都只 build/copy `zed` 和
`cli`。结果：所有非源码构建里，跨 Harness 协作静默不存在——但 Zed 进程内 observer
照常写 artifacts/evidence，所以功能"看起来半工作"。附带一个轻度命令劫持面。

**改法**（三件事一次修完）：

1. `script/bundle-mac`（+ 一行 codesign）、`script/bundle-linux`（进 `libexec/`）、
   `script/bundle-windows.ps1`（进 `bin\`，随 `zed.iss` 的 `bin\*` 一起装，并加进
   签名列表）都加上 `--package shared_context` 与拷贝。
2. 新增 `shared_context_server_path(cx)`：macOS 走
   `App::path_for_auxiliary_executable`（`crates/zed/src/main.rs` 找 bundled git 就是
   这么做的），其余走 `current_exe()` 同级目录，再回退 `bin/` 子目录。**解析不到就
   不写 settings**，而是把错误从 oneshot 送回已有的错误提示面——比写一条永远
   spawn 失败的命令强。
3. settings 里同时写 `env: {ZED_SHARED_CONTEXT_DB_PATH: <绝对路径>}`。这顺带解决了
   审计标为 hypothesis 的 `--user-data-dir` 分裂：`paths::set_custom_data_dir` 是
   进程内 `OnceLock`，子进程无从继承（已核实 `crates/paths/src/paths.rs`）。

`DB_PATH_ENV_VAR` / `default_db_path()` / `db_path_from_env()` 统一放进
`shared_context` crate，原本散在 3 处的路径字符串收拢成一份。

**新增断言**（原测试只检查了"注册了一个 server"）：命令路径必须 `is_absolute()`，
env 必须带 db 路径。

---

### 3.5 CI（P1）

- 现有 macOS workflow 加一步：不只 stat 文件，而是对打进 bundle 的二进制发一次
  `initialize` JSON-RPC 并断言回包含 `shared-context-mcp`——顺带能抓到"打进去了但
  起不来"（缺 dylib、架构不对）。
- 新增 `.github/workflows/agent-workspace-lint.yml`：`cargo fmt --check` +
  `script/clippy -p shared_context -p agent_ui`。**刻意不跑 `--workspace`**：3 vCPU
  hosted runner 上全量是一小时以上的活，那种 job 迟早被人关掉。

---

### 3.6 Mission 关联原子化（P1）

**动机**：`set_thread_mission` 在 `entry(thread_id)` 不存在时静默 `return`，而
metadata entry 只在线程第一次 `RootThreadUpdated` 时创建——对外部 Harness 是数秒级
的异步过程。orchestrator 因此打了个补丁：订阅 `RootThreadUpdated`，在**每一次**事件
上重设 mission+role，订阅 `.detach()` 永不解除。后果有两个：竞态下仍可能出孤儿
worker；更要命的是这个订阅**永久重申创建时的 mission/role**，任何未来的重分配功能
都会在下一条 entry 到来时被静默回滚。

**改法与审计建议不同，请注意**：审计建议"让 mission/role 进 `CreateThreadOptions`"。
作者没有采纳。理由：顺着 `create_thread_with_options` → `create_agent_thread_with_server`
看下去那是个九个位置参数的签名，再塞两个进去只是把竞态搬了个地方，病灶仍在 store
层。

实际改法是在 store 里**寄存**：

- 新字段 `ThreadMetadataStore::pending_mission_assignments`。
- `set_thread_mission` 找不到 entry 时把赋值停在那儿，而不是丢弃。
- **`save_internal` 消费它**——不是 `handle_conversation_event`。选 `save_internal`
  是因为它是所有写入的唯一漏斗，每条创建 entry 的路径都覆盖到；而 `reload()` 走
  `cache_thread_metadata` 不经过它，所以从数据库读回的行不受影响。
- 线程删除时清理寄存，避免泄漏。
- orchestrator 里整段 `RootThreadUpdated` 订阅删除，只剩一次调用。

**新增两个测试**：
- `test_mission_assigned_before_entry_exists_is_applied_on_create`——孤儿 worker 场景。
- `test_parked_mission_assignment_does_not_revert_later_changes`——寄存只消费一次，
  普通元数据更新不能复活它去回滚一次重分配。这正是老订阅的毛病。

---

### 3.7 `author` 结构化归因（P1，审计原评 P2）

**动机**（作者认为应升级到 P1，因为这是功能不正确而非体验劣化）：
`worker_dashboard` 用 `row.author == role` 过滤"本 worker 记录的内容"。但 Harness
自己经 MCP 记录时 `author` 是它自报的名字（如 `"claude-code"`），observer 记录时才
是 role。结果：**worker 主动记录的 decisions 永远不会出现在它自己的 dashboard 里**。

**改法**：三张表各加 `role TEXT`（可空）。语义切分：

- `author` = **谁记录的**：`zed-observer`，或 Harness 自报名。保留原义。
- `role` = **这是谁的活**。per-worker 视图过滤的是这个。

连带：

- observer 不再往 `author` 里塞 role，现在恒为 `OBSERVER_AUTHOR`。
- MCP 三个 `record_*` 工具 schema 加 `role`，描述直说"从 `<zed-mission-context>` 原样
  抄过来，不传的话你记的东西不会出现在你自己的页面上"。`optional_role` 缺省是
  `None` 而非占位符——没 role 的行诚实地无归属，比把所有忘传的 Harness 归到一个
  虚构 worker 下强。
- `mission_prompt` 把 role 重复成一条指令（`Pass role: 'coding'`），因为该值只存在于
  prompt 里，行一旦落库没有别的东西能还原它。
- `worker_dashboard` 过滤改 role；`mission_views::worker_for_author` 改名
  `worker_for_role` 并改判据；Evidence/Decision 署名行改成"有 role 显示 role，否则
  回落 author"；搜索框把 role 也纳入匹配。
- **迁移带尽力回填**：`UPDATE ... SET role = author WHERE author NOT IN
  ('zed-observer', 'unknown')`。老的 observer 行里 `author` 就是 role，抄过来能救回
  归属；Harness 自报的行没东西可还原，留 NULL 不猜。
- `stdio_integration.rs` 端到端 pin 住 `role` 的往返。

---

### 3.8 面板门控、刷新入口、文档漂移（P2）

- **门控**：新字段 `MissionPanel::refreshes_only_while_active`。dock 实例 true
  （有 `set_active` 钩子，回来时会 refresh，关着时白算的都省了），sidebar 实例 false
  （没有钩子，一门控就永远不刷新）。这是通知放大链的止血点：`ThreadMetadataStore`
  在**每个线程的每条新 entry** 上都会被写，每次通知的代价是一次 mission 列表查询、
  一次全量树重建，选择变化时还有三次 Shared Context 查询，且每个存活面板实例各付
  一份。
- **刷新入口**：Shared Context 与 Evidence 两个 tab 的 header 各加一个 `RotateCw`
  按钮。此前**根本没有刷新入口**，开着的 tab 只能关掉重开。
- **文档漂移修了四处**（审计报了三处）：`mission_views.rs` 的 "queue pins them"、
  "reloads on request"、"store can't be observed"，外加 `mission_panel.rs` 模块文档
  里"`ThreadMetadataStore` 不发变更事件"——而这个文件自己就在 observe 它。

---

## 4. 有意未采纳 / 有意未修

审计的 "Refactoring Traps" 三条作者完全同意，**没有动**：

- 没有把 `shared_context` 合并进 `context_server`（独立性是刻意的，headless 二进制不能拖 gpui）。
- 没有为 `ThreadMetadataStore` 引入事件总线（放大源是消费侧无门控，已从消费侧修）。
- 没有把三个 source of truth 合一。

其他有意留下的：

| 项 | 为什么留 |
|---|---|
| **同 role 的两个 worker 仍会互相吞并** | role 不是 worker 唯一标识。真修要把 per-worker id（thread_id）写进 prompt 契约。当前 orchestrator 每 role 只起一个 worker，暂不咬人。**这是个已知的半修**，不要当成已解决。 |
| `APP_NAME` 未改 | 用户明确选择。改它会触发 `crates/zed/src/main.rs` 的编译期断言，连带要重命名 `zed` 二进制（Cargo.toml `[[bin]]`、三个 bundle 脚本、Info.plist），rebase 冲突面变大。与 Zed Dev 共库的窄窗口保留。 |
| `shared_context.sqlite` 无 channel 分隔 | dev/preview/stable 构建共用一个文件。已记录，未修。 |
| `since` + `LIMIT 50` 增量消费丢中间行 | 排在 Later。注意 `ORDER BY created_at DESC` 意味着丢的是**最旧的**，正是增量消费者尚未读到的那批。 |
| 冲突检测用 `path.file_name()` | 排在 Later。`src/a/config.rs` 与 `src/b/config.rs` 会被误报为争用。 |
| Mission 无删除 / GC / 结束态 | 需要先回答产品问题（Mission 的结束态是什么）。 |
| Mission UI 无 feature flag | 用户选择"先自己用"。回上游前必须加。 |
| MCP 无鉴权 | 任意本地进程可读写任意 mission_id。本地单用户下可接受。 |

---

## 5. 作者无法验证的部分 —— 请重点排查

**这一节是本次审查的核心。** 以下是作者做了静态判断、但没有编译器确认过的地方，
按风险从高到低：

1. **`mod mission_db { #![allow(dead_code)] ... }` 是否真的消除了 `static_connection!`
   展开出的死代码警告。** 作者的推理是：`#[allow]` 挂在 struct 上覆盖不到宏展开的
   inherent impl，所以改用模块内层属性。**没验证过。** 若 `script/clippy` 仍报
   `global` / `open_test_db` never used，需要换法。
2. **`sqlez` 的 6 元组 `Column`**（evidence 查询从 5 列变 6 列）。作者查到
   `crates/sqlez/src/bindable.rs` 有 `impl_tuple_row_traits!` 到 7 元及以上，判断可用。
3. **`Option<String>` 作为 `Bind` / `Column`**。参照 `exit_code: Option<i32>` 的既有
   用法推断可用。
4. **`collections::HashMap::from_iter`** 写进 `ContextServerCommand::env`。
   `collections::HashMap = FxHashMap`，`FxBuildHasher: Default`，所以 `FromIterator`
   应当可用；`crates/settings_content/src/project.rs` 用的也是 `collections::HashMap`。
5. **`assert_eq!(command.path, server_path)`**：`PathBuf` vs `&Path`，依赖
   `impl PartialEq<&Path> for PathBuf`。
6. **rustfmt 重排**。有几处新行接近 100 列（尤其
   `db::open_test_db::<(ThreadMetadataDb, MissionDb)>(` 那 5 行）。**先跑
   `cargo fmt` 再看 diff**，如果 fmt 大幅重排了作者写的代码，那是预期内的，不是 bug。
7. **observer 里 `role` 被 move 进 `async move` block 的借用**。`observe_entry` 里
   Execute 分支先 `role.clone()` 后 `return`，Edit 分支再把 `role` move 进
   `background_spawn`。作者判断没问题，但这是最容易被借用检查器打回的形状。
8. **`is_some_and(|role| matches(role))`** —— 刻意写成显式闭包而非 `is_some_and(matches)`，
   避免依赖闭包的 `Copy`。若 clippy 报 `redundant_closure`，注意它属于 `style` 组
   （仓库里是 `allow`），理论上不该触发。
9. **`IconButton` / `Tooltip::text` / `cx.listener` 在 `SharedContextView` 和
   `EvidenceView` 上可用**。`IconButton` 来自 `ui::prelude::*`（已确认 prelude 导出）。
10. **`App::path_for_auxiliary_executable` 的接收者类型**。作者确认过 `crates/gpui/src/app.rs`
    里 `impl App`（从 777 行开始）内有这个方法，不只是 `Application` 上那个同名包装。

作者做过的机械检查（可信度中等，不能替代编译）：括号/花括号/方括号在剥离注释与
字符串后平衡；所有 `record_*` 调用点已随签名更新；改名后的符号无残留引用。

---

## 6. 验证门禁

**逐条执行。任何一条失败 → 停下来报告，不要提交。**

### Gate A — 静态检查

```bash
cargo fmt --all
git diff --stat                       # 记录 fmt 造成的重排，属预期
cargo check -p shared_context -p agent_ui
./script/clippy -p shared_context -p agent_ui
```

通过判据：`clippy` 零 error 零 warning（脚本带 `--deny warnings`）。
特别确认第 5 节第 1 条（`mission_db` 模块的死代码）没有报出来。

### Gate B — 测试

```bash
cargo test -p shared_context
cargo test -p agent_ui
```

通过判据：全绿。**重点看这几个**——它们是本次改动的直接靶子：

- `test_mission_assigned_before_entry_exists_is_applied_on_create`（新）
- `test_parked_mission_assignment_does_not_revert_later_changes`（新）
- `test_mission_migration_preserves_existing_threads_with_null_mission_and_role`（改）
- `test_mission_migration_runs_on_empty_database`
- `shared_context_server_is_added_only_when_missing`（加了绝对路径 / env 断言）
- `mission_prompt_carries_the_identity_and_role`（加了 `Pass role:` 与"无游离反斜杠"断言）
- `record_and_read_decisions_artifacts_and_evidence`（加了 author/role 分离断言）
- `crates/shared_context/tests/stdio_integration.rs`（端到端 role 往返）
- 全部 `sidebar` / `thread_metadata_store` 测试——迁移域拆分若夹具没修好，**这里会
  大面积挂**，那是最灵敏的信号。

### Gate C — 迁移域拆分的运行时验证（最高风险项）

> **前置：必须先删掉旧的 dev 库。**
> ```bash
> rm -rf ~/Library/Application\ Support/Zed/db/0-dev
> ```
> 不删的话，`MissionDb` 域会在已有 `mission_id` 列的库上重跑
> `ALTER TABLE sidebar_threads ADD COLUMN mission_id` → duplicate column →
> 整个 db 开库失败并退到内存库。用户已确认接受丢弃现有本地数据。

冷库验证：

```bash
cargo run                              # 或跑 bundle 出来的 app
```

1. sidebar 正常列出线程，Mission 面板可打开、无报错。
2. 检查域是否分开注册：
   ```bash
   sqlite3 ~/Library/Application\ Support/Zed/db/0-dev/db.sqlite \
     "SELECT domain, COUNT(*) FROM migrations GROUP BY domain;"
   ```
   通过判据：**同时存在 `ThreadMetadataDb` 和 `MissionDb` 两行**，且
   `ThreadMetadataDb` 的条数不再包含 mission 那一条。
3. `SELECT name FROM sqlite_master WHERE name='missions';` 有结果。

### Gate D — 打包与二进制

```bash
script/bundle-mac
app=$(find target -maxdepth 5 -iname "*.app" -print -quit)
ls -la "$app/Contents/MacOS/shared-context-mcp"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | ZED_SHARED_CONTEXT_DB_PATH=/tmp/smoke.sqlite "$app/Contents/MacOS/shared-context-mcp" | head -n 1
```

通过判据：二进制存在且可执行；回包里含 `"shared-context-mcp"`。

然后在 app 里创建一个 Mission，检查用户 settings：

```bash
grep -A 10 '"shared-context"' ~/.config/zed/settings.json
```

通过判据：`command` 是**绝对路径**（不是裸名），且 `env` 里有
`ZED_SHARED_CONTEXT_DB_PATH`。

### Gate E — 端到端归因（这条修的是功能正确性，值得手动走一遍）

1. 创建一个带外部 Harness worker 的 Mission。
2. 让该 Harness 调 `record_decision`，**带上 `role`**（prompt 里已经指示它这么做）。
3. 打开那个 worker 的 dashboard。

通过判据：这条 decision **出现在该 worker 自己的页面上**。改动前它永远不会出现——
这就是本次修复的意义。若它仍不出现，说明 Harness 没有按 prompt 传 `role`，那是
prompt 措辞问题，请报告而不是改过滤逻辑。

### Gate F — 人工代码审查

除了跑命令，请针对第 5 节那十条各看一眼实现，以及：

- `save_internal` 里消费寄存的位置是否真的覆盖了所有创建 entry 的路径，
  且**没有**覆盖 `reload()`（那是刻意的）。
- 迁移里的回填 `UPDATE` 是否可能误伤——特别是有没有哪个 Harness 会把自己叫
  `zed-observer` 或 `unknown`。
- `shared_context_server_path` 的回退顺序在 Linux 安装布局（`libexec/`）和 Windows
  安装布局（`bin\`）下是否真的对得上第 3.4 节改的那三个脚本。

---

## 7. 门禁失败时怎么办

- **编译错误 / clippy 报错**：属于第 5 节预期风险，直接修，修完重跑 Gate A。这类
  修复不需要回来问。
- **测试失败**：先判断是"实现有 bug"还是"测试断言写错了"。**不要通过放宽断言来让
  测试变绿**——新加的那几个测试断言的正是本次要修的行为。
- **Gate C 失败**：停下来报告。迁移域是数据安全相关，不要自行改迁移内容。
- **Gate E 失败**：报告，不要改过滤逻辑去迁就。

---

## 8. 通过后的提交计划

全部门禁通过后，按下面分四个提交。**当前分支是 `feature/worker-dashboard`**，
直接提交在这个分支上（这是用户既有的工作分支，不是默认分支）。

**先把这份文件删掉，它不进仓库：**

```bash
rm REVIEW-mission-hardening.md
```

**Commit 1 — 迁移域隔离**
```
crates: agent_ui/src/thread_metadata_store.rs
```
```
agent_ui: Move Mission migrations into their own sqlez domain

Appending to ThreadMetadataDb's migration array claimed an index in a
list upstream also appends to. sqlez compares stored migrations by
index and aborts the database open on a mismatch, so an upstream
migration landing at that index would surface one rebase later as a
dropped database rather than a merge conflict.

MissionDb depends on ThreadMetadataDb so the topological sort runs it
after sidebar_threads exists. Same file, separate index space.

Test fixtures that opened a database with only the upstream domain now
open both: LIST_QUERY selects mission_id/role, so a single-domain
connection fails every sidebar read, not just Mission ones.
```

**Commit 2 — 交付与并发止血**
```
crates: shared_context/*, agent_ui/src/mission_orchestrator.rs（仅 path/env 部分）,
        agent_ui/src/mission_context_observer.rs（仅 default_db_path 部分）,
        script/bundle-*, .github/workflows/*
```
```
shared_context: Ship the MCP binary and make its database concurrency-safe

The bundle scripts built only zed and cli, so every packaged build
registered a bare `shared-context-mcp` command that PATH could not
resolve -- cross-Harness collaboration was silently absent in
production while Zed's in-process observer kept writing, which made it
look half-working. Settings now carry an absolute path resolved from
the bundle, plus ZED_SHARED_CONTEXT_DB_PATH so a child cannot open a
different database than the Zed that spawned it.

shared_context.sqlite is the one database several processes really do
write at once, and was the one without WAL or a busy timeout. Failed
writes are logged and dropped, never retried, so this was the
difference between a slow write and a lost one.

Adds a lint workflow: nothing in this fork ran clippy, which is how
four disallowed_methods errors sat in the stdio test unnoticed.
```

**Commit 3 — Mission 关联原子化**
```
crates: agent_ui/src/thread_metadata_store.rs, agent_ui/src/mission_orchestrator.rs
```
```
agent_ui: Assign a thread's Mission atomically with its metadata row

set_thread_mission dropped the assignment when the thread had no
metadata entry yet, which for an external Harness is the normal case --
the entry appears seconds later on the first RootThreadUpdated. The
orchestrator worked around it by re-asserting the assignment from a
never-released subscription, which also meant any future reassignment
would be silently reverted by the next entry.

The store now parks an assignment it cannot apply and save_internal
applies it in the write that creates the row. reload() populates the
cache directly and is deliberately not affected.
```

**Commit 4 — 归因与面板**
```
crates: shared_context/*（role 列部分）, agent_ui/src/worker_dashboard.rs,
        agent_ui/src/mission_views.rs, agent_ui/src/mission_panel.rs,
        agent_ui/src/mission_context_observer.rs（role 部分）
```
```
agent_ui: Record Mission role separately from author

The worker dashboard filtered Shared Context rows with author == role,
but author is whoever recorded a row -- zed-observer, or a Harness's
self-reported name. A worker's own record_decision calls therefore
never appeared on its own page.

Rows now carry a role alongside the author, the observer stops writing
a role into the author column, and the MCP tools ask for it. Known gap:
role does not uniquely identify a worker, so two workers sharing a role
still merge.

Also gates the dock panel's store observation on visibility (the store
notifies on every new entry in every thread), adds the refresh control
the Context and Evidence tabs never had, and corrects four places where
the module docs described behaviour the code does not have.
```

**不要加 `Co-Authored-By` 或其他 trailer。** 已核实这个 fork 现有的 10 个提交
（`de2c5b9584`、`1275f1ead1`、`ca909fbcdf` …）都没有任何 trailer，提交消息格式是
`<crate>: <祈使句摘要>` + 约 80 列换行的正文。上面四条草稿已按这个格式写。

---

## 9. 回滚

改动尚未提交，全部在工作区。放弃的话：

```bash
git checkout -- .
rm .github/workflows/agent-workspace-lint.yml REVIEW-mission-hardening.md
```

注意这不会恢复被删掉的 `~/Library/Application Support/Zed/db/0-dev`。
