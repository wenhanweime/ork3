# SPEC：Catalog 会话分级与聚类质量修复（Session Class & Cluster Hygiene）

> 状态：v0.1 · 已实现
> 日期：2026-08-17
> 关系：本文是 `SPEC-semantic-project-clustering.md` v0.1 的**修复增补**，不替换它。
> 依据：对本机真实 Catalog（`~/.config/ork3-dev/projects/catalog.sqlite3`，12,486 会话 /
> 9,552 条语义分配）的实测审计，全部数字来自 2026-08-17 的直接 SQL 查询，非估计。

---

## 0. One-liner

Catalog 入口不区分「人开的会话」和「机器开的会话」，导致 Projects 与 Clusters 两个视图
都被自动化流量、分类器自产会话和临时目录淹没；本 SPEC 引入会话分级（session class），
堵住分类器自我污染，补齐非 codex 后端的权重统计，并治理聚类主题质量。

**Done when：**

1. Projects 与 Clusters 默认只呈现交互式会话；自动化模板会话折叠为聚合条目，不再形成
   独立巨型主题或巨型目录项目。
2. 语义分类运行任意多轮后，各 Agent 的 scan root 下不新增任何会话文件/记录（专项测试断言）。
3. claude / opencode / grok / pi 会话拥有真实的 `user_turns` / `user_chars`，实质性会话
   进入聚类队列，不再被整体排除。
4. AionUi `codex-temp-*`、`/private/tmp/ork-*`、运行器状态目录等临时 cwd 不再作为用户
   Project 出现。
5. 相同规范化标题的会话只消耗一次分类调用；禁止「未分类会话」类垃圾桶主题。

---

## 1. 实测诊断（2026-08-17）

### 1.1 总量与构成

| 指标 | 实测值 |
|---|---|
| 会话总数 | 12,486（codex 9,727 / opencode 2,192 / grok 243 / claude 216 / pi 108） |
| 最大目录项目 | `.openclaw`（cwd 类）— **8,965** 条会话 |
| 语义分配总数 | 9,552，全部来自 codex 会话 |
| 最大两个主题 | 「OpenClaw watchdog 自动修复」6,286 + 「OpenClaw watchdog 开发」2,677 = **94%** |
| 主题总数 | 89（其中 19 个单会话主题） |
| 零内容会话 | 标题 `New session - <ISO时间戳>` 且 `user_chars=0` 的 opencode 会话 **2,161** 条 |
| 纯噪音项目 | 35 个项目的全部会话 `user_chars < 24` |
| AionUi 临时项目 | `codex-temp-<epoch>` 形态 **29** 个 |
| 重复标题浪费 | 22 个标题模板覆盖 9,506 条会话；9,552 次 LLM 分配中 **9,200** 次花在重复标题上 |

### 1.2 五个根因（含代码定位）

**R1 · 自动化会话未识别（P0）**
8,955 条 codex 会话标题同为 `You are a watchdog assistant helping OpenClaw continue
execution...`，是 OpenClaw watchdog 经 `codex exec` 的无头调用，cwd `~/.openclaw`，占
Catalog 的 72%。证据就在会话文件里：codex `session_meta.payload` 带
`originator: "codex_exec"` / `source: "exec"`，但 `parse_codex`
（`src/projects/adapters.rs:642` 起）只取 `id` 与 `cwd`，丢弃了该字段。这些会话把注入
prompt 计入 `user_chars`（实测 2,856–67,912），轻松通过 substantive 过滤进入聚类。
同类：「抖音滑板内容审核」95 条（content-reviewer 自动化）、`<codex_internal_context>`
20 条。

**R2 · 分类器自我污染（P0）**
语义分类用 `opencode run` 跑批（`src/projects/semantic.rs::backend_command`），每次调用
都在 `opencode.db` 落一条新 session；下一轮扫描把它们捡回 Catalog，形成
`New session - …`、`user_chars=0` 的空会话，cwd 为 ork3 server 当时的工作目录 —— 因此
`herdr-projects/herdr` 项目凭空多出 1,140 条、`agent-session-atlas` 多出 561 条。时间
分布（08-11 日 1,003 条 / 08-14 日 648 条）与分类运行日吻合。`codex exec` 兜底同样落
盘（库中已有 1 条标题为聚类 prompt 本身的会话）。只有 pi 因 `--no-session` 幸免。
这是反馈回路：分类 → 产生新会话 → 入库 → 制造新的未分类量。

**R3 · 权重只对 codex 计算，其余后端被整体排除在聚类外（P0）**
`SessionWeight` 只在 `parse_codex` 里累计；claude/grok parser 不记录，opencode 扫描器
直接 `SessionWeight::default()`（`src/projects/adapters.rs:933` 附近）。实测四个后端
`max(user_chars)` 全为 0。而 `pending_semantic_sessions`（`src/projects/catalog.rs:688`）
要求 `user_chars >= MIN_ANY_CHARS(24)`，于是 claude 216 / opencode 2,192 / grok 243 /
pi 108 条会话**永远进不了聚类队列**。Clusters 实际是「codex-only 视图」，其中 94% 又是
watchdog —— 这就是「聚类不合理」的主体。

**R4 · Projects 视图不过滤垃圾 + 临时目录识别过窄（P1）**
substantive 过滤只用在语义分类入口，Projects 树照单全收 2,161 条空会话与成片 0-turn
会话；35 个项目为纯噪音。ephemeral 检测只硬编码 `paseo-multica-agent-` 前缀
（`src/projects/classifier.rs:5`），漏掉：AionUi 的 29 个
`…/Application Support/AionUi/aionui/codex-temp-<epoch>`、`/private/tmp/ork-direct-accept.*`、
`…/Application Support/dev.ork.ork/general` 与 `dev.runboard.runboard/general` 等运行器
状态目录（三个重名 `general` 项目并存）。

**R5 · 聚类主题质量（P1）**
- 完全相同标题的 8,955 条 watchdog 会话被劈成两个主题（6,286 + 2,677）；批次独立聚类
  加 known_topics 提示只能缓解，近义主题一旦并存无合并机制，永久固化。
- 模型自造 meta 主题「未分类会话」（34 条），把「在吗」「帮我点击 dia 浏览器」等无关
  会话混装；prompt 只禁了「每会话一组」，未禁垃圾桶主题。
- 「AGENTS.md 指令会话」（89 条）是标题提取失败的下游症状：
  `INJECTED_PREAMBLE_MARKERS`（`src/projects/adapters.rs:1123`）漏了
  `# AGENTS.md instructions <INSTRUCTIONS>`（无 " for " 变体）与
  `[Assistant Rules - You MUST follow these instructions]` 两种前导，指令文本成为标题，
  再被 LLM 按指令文本聚类。

---

## 2. Constraints

- 沿用母 SPEC 全部约束：分类只读元数据（§3.2）、人工锁定优先（§3.1）、整批失败回退
  （§3.4）、后端调用必须超时。
- 会话分级是**入库时的事实标注**，不是 LLM 推断；不得调用任何模型来判定 session class。
- 清理历史垃圾数据必须可逆或有备份（沿用现有 `catalog.sqlite3.pre-*` 备份惯例）。
- 不修改用户的 Agent 历史文件（scan root 内只读）。
- 遵循项目 runtime/client 边界：session class、聚合条目属于 server/runtime 事实，走
  Catalog + JSON API/event；折叠展开状态属于 TUI 客户端。

---

## 3. Features

### F12. 会话分级 session_class · P0（解 R1）

`sessions` 表新增 `session_class TEXT NOT NULL DEFAULT 'interactive'`，取值
`interactive` / `automation` / `ephemeral`（ephemeral 保留给未来使用；目录级临时性仍由
assignment evidence 承担）。

判定规则（入库时，纯代码）：

1. codex：`session_meta.payload.originator` 为 `codex_exec`（或 `source == "exec"`）→
   `automation`。交互 TUI 会话的 originator（如 `codex_cli_rs`）→ `interactive`。字段缺失
   → `interactive`（旧版本文件不误伤）。
2. 兜底模板启发式（对全部后端，Catalog 内后处理）：同一规范化标题（trim + 空白折叠 +
   lowercase）出现 ≥ `automation_title_threshold`（默认 20）→ 该组全部标记 `automation`。
   阈值可配置，规则重跑幂等。
3. 其他后端出现可靠的无头标记时（如未来 opencode/pi 提供 originator 等价物）在各自
   adapter 内映射，不做猜测。

**呈现（server 侧语义，TUI 遵循）：**

- Projects / Clusters 快照默认排除 `automation` 会话的独立条目；同一标题模板聚合为一个
  **模板聚合条目**（如 `OpenClaw watchdog × 8955`），排序按该组最新 `last_activity_at`。
- Sessions 视图与全文搜索不排除任何 class。
- 语义分类队列（`pending_semantic_sessions`）排除 `automation`。已有的 watchdog 等
  automation 会话对应的 `semantic_assignments` 行在迁移时删除（见 F16），相关主题自然
  消失或缩水。

**Acceptance criteria：**

1. Given watchdog 的 codex exec 会话，When 扫描入库，Then `session_class='automation'`，
   Projects 快照的 `.openclaw` 项目不含它们的独立条目，Clusters 无 watchdog 主题。
2. Given originator 字段缺失的旧 codex 文件，When 入库，Then 判为 interactive，不误伤。
3. Given 同一标题出现 20+ 次但 originator 正常，When 后处理，Then 该组标为 automation
   且重跑不改变结果（幂等）。
4. Given automation 会话，When 全文搜索命中，Then 仍可打开查看（Sessions 视图不受限）。

### F13. 分类器零残留 · P0（解 R2）

后端调用不得在任何被扫描的 root 产生会话记录。

实现要求（按后端）：

- `opencode run`：以一次性 scratch 目录作为数据根调用（设 `XDG_DATA_HOME` 指向
  `TempDir`，或使用 opencode 提供的等效 no-persist 机制，以实测为准）；调用结束丢弃。
- `codex exec`：同理，设 `CODEX_HOME` 指向 scratch 目录（或等效 no-session 标志，
  以实测为准）。
- `pi`：保持现有 `--no-session`。
- `run_backend`（`src/projects/semantic.rs`）统一注入这些环境变量/参数；scratch 目录由
  调用方创建并在批结束后删除。

**Acceptance criteria：**

1. Given 一轮完整分类（含 opencode 与 codex 兜底路径），When 结束，Then 各 scan root 的
   会话计数与运行前一致（集成测试用 fake backend 脚本断言环境变量已被覆盖指向 scratch）。
2. Given `backend_command`/`run_backend`，When 构造 opencode 或 codex 调用，Then 专项
   单测断言数据根环境变量指向非 scan-root 的临时路径（对齐既有 pi `--no-session` 测试）。
3. Given 分类批失败，When scratch 清理，Then 不留残余目录。

### F14. 全后端权重统计 · P0（解 R3）

- claude parser：对 `type=="user"` 的真实用户消息 `weight.record()`（复用
  `strip_injected_preamble`；跳过 sidechain 与工具结果）。
- opencode 扫描器：从 `opencode.db` 的消息表按 session 聚合用户消息数与字符数（一条 SQL
  聚合，不逐文件读 `storage/`）；标题为 `New session - <ISO时间戳>` 默认模板时视为无标题，
  交给 `safe_title`/fallback 处理。
- grok / pi parser：按各自 transcript 结构记录用户消息。
- 权重语义区分「未知」与「为 0」：无法统计的 adapter 输出 `NULL`（迁移把既有全 0 的非
  codex 行重置为 NULL），`pending_semantic_sessions` 的 SQL 对 NULL 权重不做
  `user_chars >= 24` 淘汰，改由标题质量兜底（有 safe_title 即可入队）。
- 入队顺序照旧 `last_activity_at DESC`；先落地 F17 的标题去重再放行存量，避免 2,700+
  条积压一次性打爆免费配额。

**Acceptance criteria：**

1. Given claude/opencode/grok/pi 的真实会话样本，When 重扫，Then `user_turns`/`user_chars`
   非零且与用户消息一致（fixture 断言）。
2. Given 权重未知（NULL）的会话且标题可用，When 取 pending 队列，Then 会入队而不是被
   0 值淘汰。
3. Given `New session - <时间戳>` 默认标题且无用户消息，When 入库，Then 不入聚类队列。

### F15. 临时目录识别泛化 · P1（解 R4）

将 `is_ephemeral_agent_cwd`（`src/projects/classifier.rs`）从单前缀硬编码改为规则集：

1. cwd 位于系统 temp（现有逻辑保留，前缀列表扩展为可维护常量表，含
   `paseo-multica-agent-*`、`ork-direct-accept.*` 等）。
2. cwd 位于 `~/Library/Application Support/<AppId>/` 下且末段匹配临时模式
   `<name>-temp-<10+位数字>`（覆盖 AionUi `codex-temp-<epoch>`）。
3. cwd 末段为运行器状态目录（如 `…/Application Support/dev.*/general`）：按
   「`Application Support` 直下子目录再一层的固定名 `general`」模式识别。
4. 目录已不存在且末段匹配 `-temp-<epoch>`/uuid 形态 → ephemeral（沿用现有「目录消失仍
   识别」的语义）。

保持现有保守原则：规则必须同时满足位置 + 命名两类证据，普通用户目录（哪怕重名）不受
影响；每条规则配正反 fixture 测试。

**Acceptance criteria：**

1. Given 实测的 29 个 AionUi `codex-temp-*` cwd，When 重新分类，Then 归入
   `Ephemeral agent sessions`，Projects 快照不出现。
2. Given `/private/tmp/ork-direct-accept.E1hQlA/state/general`，同上。
3. Given 用户真实项目 `~/Projects/my-temp-1234567890`（非 Application Support、目录存在、
   git 仓库），When 分类，Then 仍是正常项目。

### F16. 主题卫生与合并 · P1（解 R5）

1. **垃圾桶主题黑名单**：`parse_response` 增加规范化标签黑名单
   （未分类/其他/杂项/misc/uncategorized/other/no clear topic 等），命中 → 整批
   `BatchError::Failed`，沿用整批回退语义。
2. **前导标记补全**：`INJECTED_PREAMBLE_MARKERS` 增加
   `# AGENTS.md instructions <INSTRUCTIONS>` 变体（建议将现有 `"AGENTS.md instructions for"`
   放宽为 `"AGENTS.md instructions"`）与 `"[Assistant Rules - You MUST follow"`；受影响
   会话在重扫后标题被重新提取，指纹变化自动触发重分类。
3. **低频主题合并 pass**：新增维护动作（挂在空闲回填循环里，频率 ≤ 每天一次，或提供
   手动 CLI 入口）：把当前全部主题标签（89 个量级，仅标签，不含会话数据）送分类后端，
   请求输出合并建议 `{"merges":[{"into":"标签A","from":["标签B"]}]}`；解析校验后合并
   `semantic_assignments.topic_key`（改写 from → into）。合并只在标签层执行，不触碰
   `assignments`，失败整体放弃。首轮预期效果：watchdog 双主题合一（在 F12 落地后该例
   已被移除，但机制对未来近义主题堆积仍然必要）。

**Acceptance criteria：**

1. Given 回复含主题「未分类会话」，When 解析，Then 整批拒绝。
2. Given 以 `# AGENTS.md instructions <INSTRUCTIONS>` 开头、随后才是真实请求的首条消息，
   When 提取标题，Then 标题来自真实请求。
3. Given 合并建议含未知标签或 into/from 重叠冲突，When 解析，Then 整体拒绝，无部分合并。

### F17. 分类前置标题去重 · P2（省配额）

`pending_semantic_sessions` 出队后、分批前，按规范化标题指纹分组：

- 组内已有任一会话有 `semantic_assignments` → 其余直接继承该主题（写入各自指纹），
  不送 LLM。
- 全组未分类 → 只送一名代表，结果回填全组。

实测收益：9,552 次分配中 9,200 次可省。

**Acceptance criteria：**

1. Given 100 条同标题会话且其一已分类，When 下一轮，Then 其余 99 条零 LLM 调用完成分配。
2. Given 同标题组代表分类失败，When 回退，Then 全组保持未分类（无部分继承）。

### F18. Projects 视图降噪 · P2（解 R4 余量）

- 快照中 thin 会话（低于 substantive 线且 class=interactive）不删除，但 server 侧在
  `ProjectSummary` 中给出 `thin_count`，会话列表把 thin 会话排到分页尾部；TUI 折叠为
  「+N 条简短会话」。
- 全部会话均 thin 的项目排序沉底（在 `latest DESC` 之后加一个 substantive-first 排序键）。

**Acceptance criteria：**

1. Given `pot` 项目含 12 条 0-turn 会话与若干真实会话，When 打开 Projects，Then 真实
   会话在前，thin 折叠计数正确。
2. Given 35 个纯 thin 项目，When 排序，Then 全部位于有实质会话的项目之后。

---

## 4. 数据模型增补

```sql
ALTER TABLE sessions ADD COLUMN session_class TEXT NOT NULL DEFAULT 'interactive'
    CHECK (session_class IN ('interactive', 'automation', 'ephemeral'));
-- 权重语义改为可空："未知"（NULL）区别于"确认为 0"
-- SQLite 无法直接改列可空性；迁移策略：保留现有列，新增
--   user_weight_known INTEGER NOT NULL DEFAULT 0  -- 0 = 未统计，1 = 已统计
-- 或等效方案，由实现者按 catalog.rs 既有 table_has_column 迁移模式选型。
CREATE INDEX IF NOT EXISTS sessions_class ON sessions(session_class);
```

迁移与一次性清理（启动时自动，沿用 `pre-*` 备份惯例先复制 db 文件）：

1. 备份 `catalog.sqlite3` → `catalog.sqlite3.pre-session-class-<date>`。
2. 回填 `session_class`：重扫时由 adapter 判定；模板启发式后处理一次。
3. 删除分类器自产垃圾：`title LIKE 'New session - %' AND user_chars = 0 AND backend =
   'opencode'` 的 sessions（连带 assignments/semantic_assignments 级联）。**先落地 F13**
   再清理，否则下一轮分类又会长回来。
4. 删除 automation 会话的 `semantic_assignments` 行（主题瘦身）。
5. 非 codex 后端的全 0 权重行标记为「未统计」，待重扫回填。

---

## 5. 实施顺序

| 阶段 | 内容 | 理由 |
|---|---|---|
| 1 | F13 零残留 | 不先堵反馈回路，后面清理无意义 |
| 2 | F12 session_class + §4 迁移清理 | 一刀去掉 72% 噪音，两个视图立即可用 |
| 3 | F14 权重 + F17 去重（同批落地） | 放行 2,700+ 真实会话进聚类，且不打爆配额 |
| 4 | F15 临时目录 + F16 主题卫生 | 剩余长尾 |
| 5 | F18 视图降噪 | 纯呈现优化 |

每阶段独立可交付、可单独验证；阶段 1–2 完成即解决用户主诉的大部分。

---

## 6. Boundaries

- **Always：** 分级判定纯代码不经 LLM；清理前备份；整批失败回退；scan root 只读。
- **Ask first：** 删除除 §4 清单外的任何历史数据；改变 Sessions 视图的可见性；
  合并 pass 的自动执行频率高于每日一次。
- **Never：** 把 transcript 正文送入分类或合并请求；因分级/清理错误丢失用户手动锁定的
  归类；在 Projects/Clusters 以外的路径（搜索、Sessions）隐藏 automation 会话。

---

## 7. Open questions

1. 模板启发式阈值默认 20 是否合适？（实测最小的自动化模板 15 条：`Reply with exactly:
   pong`；但 15 也可能是真人重复粘贴。首轮先取 20，观察漏网量。）
2. `automation` 聚合条目放在 Projects 树的什么位置：挂在其 cwd 项目下折叠，还是全局
   一个「Automation」分组？（倾向前者，与目录视图语义一致。）
3. opencode 无头调用未来若提供 no-persist 官方开关，是否替换 XDG_DATA_HOME 方案。
