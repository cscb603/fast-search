# 极速搜索 v3.0 升级白皮书

> 路径：/Users/xtap/Documents/AI/极速搜索/.trae/plans/WHITEPAPER-v3-upgrade.md
> 版本：v0.3（2026-07-24 人类优先修订版）| 状态：批准待执行 | 模式：智能执行
> 实测环境：macOS 15 | rustc 1.93.1 | cargo 1.93.1 | 磁盘可用 54GB
> 总代码量：实测 2248 行（5 个现有文件：core lib.rs 1186 / tauri lib.rs 392 / main.js 328 / styles.css 303 / index.html 39）| 新增：~800 行（5 个文件）| 修改：~160 行（6 个文件）
>
> **v0.3 修订摘要（执行 AI 必读，均已实测核查）**：
> 1. **依赖版本全面更新（防编译爆炸）**：tantivy `0.22`→`0.26`、tantivy-jieba `0.11`→`0.20`（0.20 才适配 tantivy 0.26，旧组合编译不过）、notify `7`→`8`（稳定版 8.2.0）、axum `0.7`→`0.8`（仅 fallback 手写时用）
> 2. **MCP 端口冲突自愈**：默认 `9877`（禁用 9876，那是 sts-x 的），但启动时空闲探测 +1，绑定首个空闲端口，写 `mcp_port` 文件 + GUI 状态栏显示；支持 `STS_MCP_PORT` 环境变量强制指定。面向"分享给朋友"场景，避免硬编码撞车
> 3. **MCP 基座复用**：优先 `core_lib::mcp`（sts-x/batch_renamer 已验证），`/health` `/tools` 自动生成，axum/tokio 由它 re-export，不必单加 axum 依赖；编译失败 >2 次才 fallback 手写 axum 0.8
> 4. **人类优先：排除 + 排名**（本次新增，§2.6）：① 硬过滤 `.` 开头隐藏文件与 `._` 开头 AppleDouble 垃圾，永不进结果；② 程序/代码目录与代码文件类型默认排名降级（不隐藏，选"代码"过滤或精确命中文件名可豁免）；③ 收口在 `sort_and_dedup`，所有引擎结果统一遵循
> 5. **验收标准纠错**：`照片→photo` 是别名映射层（P2 fuzzy）的活，不是 BM25 分词（P1）的活；二者拆成两条 P0
> 6. **双模式搜索（本次新增，§2.6）**：人类 GUI = 过滤(`._`/`.`垃圾)+代码降级；AI/MCP = 全量不筛选不降级（`human_mode` 开关分流，单一 `sort_and_dedup` 收口）。MCP 工具暴露可选 `human_filter` 参数，AI 给人看结果时可复用人类过滤

## §0 技能调用指南（智能执行专用）

| Phase | 触发时机 | 调用技能 | 用途 | 调用条件 |
|-------|---------|---------|------|---------|
| 全程 | 代码定位 | `sts-x`（CLI：`/usr/local/bin/sts-x`） | 替代 Grep/Read 整文件，省 ~80% token | 必用 |
| 遇到 Bug/编译反复失败 | 排障 | `systematic-debugging` | 分层定位根因，不盲试 | 遇 Bug 必调 |
| 每个 Phase 完成后 | 代码审查 | `code-reviewer` | 检查新增/修改代码质量 | 必调 |
| 新增模块完成后 | 测试生成 | `test-generator` | 为新增模块生成单元测试 | 必调 |
| 重构时 | 代码重构 | `code-refactorer` | 优化代码结构 | 按需 |
| Rust 疑难问题 | 架构参考 | `rust-expert` | 最佳实践和性能优化 | 按需 |
| P6 收尾门禁 | 工业级体检 | `industrial-code-sop` | `cargo clippy -- -D warnings` 零警告门禁 | 必调 |
| 收尾阶段 | 文件管理 | `workspace-butler` | 清理临时文件、整理目录 | 必调 |

> 🐞 遇 Bug 先调 `systematic-debugging`：复现→分层定位（环境/配置底层先查，别一上来钻代码）→验证假设→最小复现→根因修复。
> 以上技能已确认全部安装于 `~/.workbuddy/skills/`（2026-07-24 实测）。`systematic-debugging` / `industrial-code-sop` 均存在，必须调用，不要跳过。

## §1 核心契约（第一优先级）

- **一句话**：给极速搜索加上 BM25 语义索引 + 自研模糊匹配 + 缩略图 + FSEvents 实时监听 + MCP 服务，保持"轻快准"，同时让 AI 能通过 MCP 协议直接调用搜索
- **不做什么**：不改 UI 布局和交互流、不引入 ML 语义模型（零 ONNX）、不改数据库 schema、不升版本号/git push/Release
- **约束**：Rust 1.93.1 + edition 2021，macOS 优先，sts-core 与 Tauri 解耦，单文件 ≤500 行
- **新增依赖（版本已按 crates.io 2026-07-24 实测锁定，执行时不要重查、不要擅自改）**：
  - `tantivy = "0.26"`（最新稳定 0.26.1）
  - `tantivy-jieba = "0.20"`（⚠️ 必须 0.20，才适配 tantivy 0.26；0.18 及以下配旧 tantivy，混搭编译不过）
  - `notify = "8"`（最新稳定 8.2.0；9.0 尚在 rc，不用）
  - MCP：优先 `core_lib = { path = "../../rust_master_workspace/libs/core_lib", default-features = false, features = ["mcp"] }`（axum/tokio 由它 re-export，不必单加 axum）；fallback 才手写 `axum = "0.8"`
- **MCP 端口：默认 `9877`，冲突自愈 + 可配置**（详见 §2.6 / Phase 5；❗禁用 9876，那是本机 sts-x MCP 默认端口）
- **UI 原则**：在现有布局内追加元素，不修改现有卡片/按钮/搜索框样式

## §2 不做什么（代码级）

- ❌ 不改 `src/index.html` 的标签页/搜索框/header 结构（只追加缩略图 CSS + 脚本引用）
- ❌ 不改 `src-tauri/src/lib.rs` 的现有 Tauri command 签名（只新增 command）
- ❌ 不改 `src/styles.css` 的现有样式规则（只追加缩略图相关样式）
- ❌ 不删 `build_alias_mapping` 静态映射表（保留兼容，FuzzyMatcher 作为增强层）
- ❌ 不删 `rg_index_search` 和 `memory_index_search`（保留作为 BM25 的 fallback）
- ❌ 不升版本号/不 git push/不 Release
- ❌ 不删 `sort_and_dedup` 的现有计分逻辑（只在其后追加惩罚项，见 §2.6）
- ⚠️ 允许改：`crates/sts-core/src/lib.rs`（加模块声明 + 集成 BM25/Fuzzy/Thumbnail/FSEvents + 过滤/排名）、`crates/sts-core/Cargo.toml`、`src-tauri/Cargo.toml`、`src-tauri/src/lib.rs`（加依赖/新 command/MCP 启动）

## §2.5 后端→UI 映射表

| 后端功能 | Phase | UI 落脚点 | 具体改动 | 用户感知 |
|---------|-------|----------|---------|---------|
| BM25 索引 | P1 | 搜索提示文字 | `search-tip` 更新为 "V7 BM25 语义引擎" | 知道引擎升级 |
| 模糊匹配 | P2 | 搜索结果 | 输入缩写能搜到软件（如 `ps`→Photoshop） | 搜索更智能 |
| 缩略图生成 | P3 | 搜索结果列表 | 图片文件显示真实缩略图替代 emoji | 视觉体验提升 |
| 缩略图缓存 | P3 | 搜索结果列表 | 二次搜索同一文件缩略图秒出 | 更快 |
| FSEvents 监听 | P4 | 索引状态文字 | `indexing-status` 显示"实时监听中" | 知道实时更新 |
| MCP 服务 | P5 | 无（后台服务）+ 状态栏 | AI Agent 可调用搜索；状态栏显示实际端口 | AI 可接入 |
| 人类优先过滤/排名 | P1/P2 | 搜索结果 | `.`/`._` 垃圾不显示；代码类结果靠后 | 日常搜文档/图片更干净 |

**UI 安全边界**：
- ✅ 允许：在现有容器内追加元素、修改 Text 内容、新增 callback
- ❌ 禁止：修改卡片样式、搜索框布局、标签页结构、按钮位置

## §2.6 双模式搜索：人类过滤 vs AI 全量（核心设计，化解矛盾）

> 设计意图：同一套搜索引擎，两种调用方，两种行为——**人类 GUI 要干净**（不掺垃圾、代码靠后），**AI/MCP 要全量**（执行任务或帮人找东西时，必须能搜到一切，不能被过滤掉）。
> 矛盾化解：过滤与排名**不是全局强制**，而是按 `human_mode` 开关。人类模式开过滤+降级；AI 模式全关，纯相关度排序，结果全量返回。

### 2.6.1 人类模式·硬过滤（永不进入结果集）

仅当 `human_mode == true` 时在 `sort_and_dedup` 入口 `retain` 收口：

```rust
if human_mode {
    all_results.retain(|r| !is_system_cruft(&r.name, &r.path));
}
```

- **`.` 开头（隐藏文件/目录）**：`.DS_Store`、`.git`、`.Trashes`、`.Spotlight-V100`、`.fseventsd`、`.cache`、`.npm`、`.config`、`.idea`、`.vscode` 等
- **`._` 开头（AppleDouble 资源分叉）**：macOS 跨盘拷贝/解压产生的 `._filename` 成对垃圾文件
- 边界：过滤 `._x` 不影响同名正常文件 `x`（二者独立，用户文件照常显示）

### 2.6.2 人类模式·排名降级（可搜到，但默认靠后）

仅当 `human_mode == true` 时在现有 `base_score` 计完后追加惩罚项（不覆盖现有相关度逻辑）：

- **程序/代码目录惩罚**（命中路径片段即 `-8000`）：`node_modules`、`.git`、`target`、`build`、`dist`、`DerivedData`、`/usr`、`/System`、`/opt/homebrew`、`~/.cargo`、`Library/Caches`
- **代码文件类型惩罚**（按扩展名 `-5000`）：`.rs .py .js .ts .tsx .go .c .h .cpp .java .rb .sh .toml .json .lock .yaml .yml`
- **文档/媒体/图片类不额外扣分**：`.pdf .doc .docx .xls .xlsx .ppt .txt .md .jpg .jpeg .png .heic .webp .mp4 .mov .mp3 .wav` 等保持自然相关度，靠前显示
- **豁免规则**（取消上述惩罚）：
  - 用户选了 `filter_type == "code"`（显式搜代码）
  - 关键词精确命中文件名（`name_lc == keyword_lc`）或别名/缩写命中（现有 `+20000` 分支）

### 2.6.3 AI 模式·全量搜索（不筛选、不降级）

当 `human_mode == false`（MCP 工具调用）时：

- **不做 `is_system_cruft` 过滤**——AI 执行任务/帮人找东西可能正需要 `.git` 配置、`._` 文件或任意系统文件，必须"都要搜到"
- **不追加代码/程序目录惩罚**——纯 BM25/相关度排序，代码文件该排前就排前
- **返回结果数量放宽**：AI 模式突破人类模式的 `take(100)` 上限（例如 `take(500)` 或按 MCP 调用方要求），保证任务有足够上下文
- **可选 `human_filter` 参数**：MCP `search_files` 工具暴露 `human_filter: bool`（默认 `false`=全量）。当 AI 是"给人看的结果"服务时，可传 `true` 复用人类模式过滤，避免把垃圾推给用户

### 2.6.4 MCP 端口冲突自愈（面向分享场景）

> 工具要分享给朋友，各机器端口占用未知；硬固定易冲突导致 MCP 起不来。

- 启动流程：先试 `9877`；若 `TcpListener::bind` 失败（地址已占用），自动 `+1` 依次探测 `9878`…`9897`（最多 +20），绑定首个空闲端口
- 实际绑定端口写入 `~/Library/Caches/com.xtap.search/mcp_port` 并打印日志；GUI 状态栏显示 `MCP: 9877`（冲突后显示实际端口，如 `MCP: 9881`）
- 支持环境变量 `STS_MCP_PORT` 强制指定（朋友机器想固定时可设，例如 `STS_MCP_PORT=9877`）
- ❗ 禁用 `9876`：那是本机 sts-x 的 MCP 默认端口，会冲突
- 设计理由：MCP 仅 AI 接入用，日常人类搜索不依赖它；即便端口全满 MCP 失败，也不影响 GUI 搜索

### 2.6.5 收口与参数设计（单一入口，两种行为）

- 中心收口：`sort_and_dedup(results, keyword, filter_type, click_history, mapping, human_mode: bool)` —— 所有引擎（rg / Spotlight / BM25 / fuzzy）结果都经此函数，按 `human_mode` 决定过滤/降级与否
- 调用方接线（**不改 Tauri command 对外签名，只在内部 core fn 加参数**）：
  - GUI 的 Tauri command `search_files` → 内部调 `search_files(..., human_mode = true)`
  - MCP 工具 `search_files` / `search_content` → 内部调 `search_files(..., human_mode = false)`（默认全量；`human_filter = true` 时转 `true`）
- 建议在 Phase 1 的 lib.rs 集成步骤中顺手实现（只改 `sort_and_dedup` + `search_files` 两个内部函数，影响全局）

## §3 成功标准（逐条验收）

| 级别 | 标准 | 量化指标 | 用户可见 |
|------|------|---------|---------|
| P0 | 编译通过 | `cargo check --workspace` 零错误 | — |
| P0 | 缩略图显示 | 图片搜索结果显示真实缩略图 | 是 |
| P0 | 中文分词搜索（BM25/P1） | 输入"合肥"能搜到名含"合肥照片2026"的文件（连续中文被 jieba 切词后可部分命中） | 是 |
| P0 | 中英别名搜索（fuzzy/P2） | 输入"照片"能搜到名含"photo"的文件（走别名映射层，**不是** BM25 的职责） | 是 |
| P0 | 缩写匹配 | 输入 `ps` 搜到 Photoshop，`vscode` 搜到 VS Code | 是 |
| P0 | 人类模式·系统垃圾过滤 | GUI 搜任何词都不会出现 `.DS_Store` / `._x` / `.git` 等 | 是 |
| P0 | 人类模式·代码不抢前列 | GUI 搜"报告"时 doc/pdf 排前，`.rs` 文件靠后（除非选"代码"过滤） | 是 |
| P0 | AI 模式·全量搜索 | MCP `search_files` 返回含代码/系统文件的全量结果，不过滤不降级；`human_filter=true` 时可复用人类过滤 | 否（AI 用） |
| P0 | 原有功能不退化 | 所有现有搜索类型（全部/图片/视频/文档/程序/文件夹/内容搜索）正常工作 | 是 |
| P1 | FSEvents 实时更新 | 新增文件后 5 秒内可搜到 | 是 |
| P1 | MCP 服务可用 | `curl localhost:9877/tools`（或实际端口）返回工具列表；端口冲突时自动+1 仍能起；sts-x 的 9876 不受影响 | 否（AI 用） |
| P1 | 搜索速度不退化 | 冷搜索 <200ms，热搜索 <50ms | 是 |
| P2 | 编辑距离纠错 | 输入 `photoshp` 能搜到 Photoshop | 是 |
| P2 | clippy 零警告 | `cargo clippy --all-targets` 无新增警告 | — |

## §4 火箭发射前检查清单（Phase 0 · 预检）

### 4.1 环境实测数据

| 项目 | 实测值 | 要求 | 状态 |
|------|--------|------|------|
| rustc | 1.93.1 | ≥ 1.75 | ✅ |
| cargo | 1.93.1 | ≥ 1.75 | ✅ |
| 磁盘可用 | 54 GB | ≥ 5 GB | ✅ |
| 当前编译 | 通过 | 零错误 | ✅ |
| 端口 9877 | 空闲（执行时再 `lsof -iTCP:9877` 复验；占用会自动+1） | 可探测空闲 | ⏳ |
| core_lib 基座 | `/Users/xtap/Documents/AI/rust_master_workspace/libs/core_lib/src/mcp/` 存在（2026-07-24 实测） | 存在 | ✅ |
| 参考文件 | sts-x-3/DESIGN.md、星TAP_Pro 白皮书、防坑指南均存在（2026-07-24 实测） | 存在 | ✅ |

### 4.2 预检清单

```
[✅] 1. 环境版本检查：rustc 1.93.1 / cargo 1.93.1
[✅] 2. 磁盘空间检查：54GB 可用
[✅] 3. 项目完整性检查：5 个源文件存在，编译通过
[✅] 4. 当前编译状态检查：cargo check 零错误
[✅] 5. 参考文件存在性（已实测，见 4.1）
[✅] 6. 依赖版本（已按 crates.io 实测锁定，见 §1；执行时不必重查、不要擅自改）
[ ] 7. 备份关键文件：cp crates/sts-core/src/lib.rs{,.bak} 等（Phase 尾删）
[ ] 8. UI 现有属性盘点：执行时 Read src/main.js + index.html 确认
[✅] 9. 磁盘空间预留确认：编译 ~2-3GB（tantivy 依赖树较大），充足
[ ] 10. 读防坑指南 /Users/xtap/Documents/AI/_shared-knowledge/ai-common-pitfalls.md
[ ] 11. 网络纪律：首次 cargo check 会拉 tantivy 全依赖树；本机 VPN 自动重连，
       遇 SSL_ERROR_SYSCALL/connection reset 视为"重连窗口"，5s 间隔重试
       （最多 ~3.5min），不要急着换镜像/改策略
```

## §5 文件组织与空间管理

### 5.1 新增文件

| 文件 | 路径 | 预计大小 |
|------|------|---------|
| BM25 模块 | `crates/sts-core/src/bm25.rs` | ~170 行 |
| 模糊匹配模块 | `crates/sts-core/src/fuzzy.rs` | ~200 行 |
| 缩略图模块 | `crates/sts-core/src/thumbnail.rs` | ~150 行 |
| FSEvents 模块 | `crates/sts-core/src/fsevents.rs` | ~130 行 |
| MCP 服务模块 | `src-tauri/src/mcp.rs` | ~80 行（core_lib 基座路线）/ ~150 行（fallback 手写） |
| 过滤/排名辅助 | 并入 `crates/sts-core/src/lib.rs` 的 `is_system_cruft` + 排名惩罚（不单开文件） | +30 行 |

### 5.2 修改文件

| 文件 | 修改内容 | 预计改动 |
|------|---------|---------|
| `crates/sts-core/Cargo.toml` | 加 tantivy + tantivy-jieba + notify | +3 行 |
| `crates/sts-core/src/lib.rs` | 加 mod 声明 + GlobalIndex 扩展 + 搜索流程集成 + `is_system_cruft` + 排名惩罚 | +90 行 |
| `src-tauri/Cargo.toml` | 加 core_lib（mcp feature）；fallback 才加 axum | +1 行 |
| `src-tauri/src/lib.rs` | 加 MCP 启动（冲突自愈端口）+ 缩略图 command | +45 行 |
| `src/main.js` | 缩略图渲染逻辑 | +30 行 |
| `src/styles.css` | 缩略图样式 | +15 行 |

### 5.3 备份文件

| 文件 | 备份路径 |
|------|---------|
| `crates/sts-core/src/lib.rs` | `crates/sts-core/src/lib.rs.bak` |
| `src-tauri/src/lib.rs` | `src-tauri/src/lib.rs.bak` |
| `src/main.js` | `src/main.js.bak` |

### 5.4 空间预算

| 项目 | 预计占用 | 累计 |
|------|---------|------|
| 源码新增 | ~50 KB | 可忽略 |
| 编译产物 | ~2 GB | 2 GB |
| BM25 索引文件 | ~20 MB | 2.02 GB |
| 缩略图缓存 | ~50 MB | 2.07 GB |
| 当前可用 | 54 GB | ✅ 充足 |

## §6 Phase 间交叉修改冲突分析

### 6.1 冲突矩阵

| 文件 | P1 | P2 | P3 | P4 | P5 | P6 | 冲突风险 |
|------|----|----|----|----|----|----|---------|
| `lib.rs` | mod + GlobalIndex + 过滤/排名 | 集成 FuzzyMatcher | 集成 ThumbnailCache | 集成 FSEvents | — | — | ⚠️ 高 |
| `Cargo.toml` (core) | +tantivy | — | — | +notify | — | — | ⚠️ 中 |
| `Cargo.toml` (tauri) | — | — | — | — | +core_lib | — | 低 |
| `tauri lib.rs` | — | — | +thumbnail cmd | — | +mcp cmd | — | ⚠️ 中 |
| `main.js` | — | — | — | — | — | +thumbnail | 低 |

### 6.2 风险详解

- **lib.rs 高冲突**：P1-P4 都改同一个文件。P1 先改结构体，P2-P4 在已有结构体上追加字段。只要 P1 的 GlobalIndex 结构体定义好所有新字段（初始化为 None），后续 Phase 只需激活对应字段即可。
- **规避策略**：P1 在 GlobalIndex 中一次性声明所有新字段（bm25/fuzzy/thumbnails），P2-P4 只做初始化赋值，不改变结构体定义。

### 6.3 执行铁律

```
1. Phase 必须严格按顺序执行，不可跳过，不可并行
2. 每个 Phase 修改前先 Read 目标文件，确认当前行号
3. 修改后立即 cargo check，通过才进入下一 Phase
4. 如果编译检查失败 >2 次，暂停并报告
5. P1 在 GlobalIndex 中一次性声明所有新字段，后续 Phase 只赋值不修改结构
```

## §7 经验教训

### 7.1 从本次对话中提取

| 经验 | 来源 | 应用 |
|------|------|------|
| 先出白皮书再编码，避免方向跑偏 | 本次对话 | ✅ 正在执行 |
| 自研模块保持 100-200 行，不膨胀 | 星TAP_Pro 对话 | 每个模块 ≤200 行 |
| 保留 fallback 路径，不删旧代码 | 星TAP_Pro 对话 | rg_index_search 保留 |
| 抽象层设计降低耦合 | 星TAP_Pro VectorIndex trait | bm25 模块独立，不污染 lib.rs |
| 并行搜索多引擎 | 星TAP_Pro 对话 | BM25 + Spotlight 并行 |
| 依赖版本先实测锁定再写白皮书 | 本次对话 | tantivy 0.26 / tantivy-jieba 0.20 / notify 8 |
| 人类优先：过滤系统垃圾 + 代码降级 | 本次对话（用户要求） | §2.6 |

### 7.2 设计原则

- **渐进增强**：BM25 优先，rg 兜底，Spotlight 补充
- **零破坏**：所有现有 API 签名不变，新增字段用 Option
- **轻依赖**：仅新增 3 个业务 crate（tantivy + tantivy-jieba + notify），MCP 复用 core_lib 基座（不新增 axum 依赖，除非 fallback）

### 7.3 时效性检查（2026-07-24 实测 crates.io）

| 依赖 | 锁定版本 | 状态 | 结论 |
|------|---------|------|------|
| tantivy | 0.26.1 | 最新稳定 | ✅ 用 0.26 |
| tantivy-jieba | 0.20 | 适配 tantivy 0.26 | ✅ 用 0.20（0.18 及以下配旧 tantivy） |
| notify | 8.2.0 | 最新稳定（9 在 rc） | ✅ 用 8 |
| axum | 0.8.9 | fallback 手写时才用 | ⚠️ 优先 core_lib::mcp，不单加 |

## §8 实施顺序

### Phase 1：BM25 索引引擎（核心）+ 人类优先过滤/排名

**1.1** 修改 `crates/sts-core/Cargo.toml`：添加 `tantivy = "0.26"` 和 `tantivy-jieba = "0.20"`（⚠️ 版本已实测锁定，不要降级；tantivy 0.26 的 TopDocs collector API 与 0.22 有差异，以 docs.rs 0.26 为准）

**1.2** 新建 `crates/sts-core/src/bm25.rs`（~170 行）
- `Bm25Index` 结构体：封装 tantivy Index/Reader/Schema
- `open()`：创建或打开 BM25 索引目录
- `rebuild_from_cache()`：从 index.cache 重建 BM25 索引（中文分词）
- `search()`：BM25 搜索，带类型过滤，返回 InternalSearchResult
- `bm25_index_dir()`：索引存储路径

**1.3** 修改 `crates/sts-core/src/lib.rs`
- 添加 `pub mod bm25;` + `use bm25::Bm25Index;`
- GlobalIndex 一次性声明新字段：`bm25` / `fuzzy` / `thumbnails`（后续 Phase 只赋值）
- `build_index_once()` 末尾追加 BM25 索引构建
- `search_files()` 中优先走 BM25，fallback 到 rg + Spotlight
- **顺手实现 §2.6 双模式**：`sort_and_dedup` 增加 `human_mode: bool` 参数——`true` 时 `retain` 硬过滤 + 代码/程序目录惩罚（豁免见 §2.6.2），`false` 时纯相关度全量；核心 `search_files` 同步加 `human_mode` 参数，GUI command 传 `true`（MCP 传 `false`，见 Phase 5）。这一处改动全局生效，不必等 P2

**验证**：`cargo check -p sts-core` → ✅

### Phase 2：自研模糊匹配引擎

**2.1** 新建 `crates/sts-core/src/fuzzy.rs`（~200 行）
- `PrefixTrie`：前缀树，支持首字母缩写快速匹配
- `levenshtein_distance()`：编辑距离计算
- `FuzzyMatcher`：组合别名映射 + 缩写匹配 + 编辑距离 + 包含匹配
- `build_from_paths()`：从文件列表构建索引
- `fuzzy_match()`：四级匹配策略（alias → acronym → fuzzy_ed → contains）
- 含单元测试

**2.2** 修改 `crates/sts-core/src/lib.rs`
- 添加 `pub mod fuzzy;` + `use fuzzy::FuzzyMatcher;`
- GlobalIndex 激活 `fuzzy` 字段
- `build_index_once()` 末尾追加模糊匹配索引构建
- `search_files()` 中集成 fuzzy_match，扩展搜索词

**验证**：`cargo check -p sts-core` → ✅

### Phase 3：缩略图缓存管理器

**3.1** 新建 `crates/sts-core/src/thumbnail.rs`（~150 行）
- `ThumbnailCache`：LRU 内存缓存 + 磁盘缓存 + qlmanage 异步生成
- `get_thumbnail()`：三级查找（内存→磁盘→生成）
- `pregenerate()`：后台预生成，不阻塞
- `supports_thumbnail()`：判断文件类型是否支持缩略图
- LRU 淘汰：超过 200 条时淘汰最旧 20%

**3.2** 修改 `crates/sts-core/src/lib.rs`：添加 `pub mod thumbnail;` + GlobalIndex 激活 `thumbnails` 字段

**3.3** 修改 `src-tauri/src/lib.rs`：新增 `get_thumbnail` command（接收 path → 返回 base64）

**3.4** 修改 `src/main.js`：`renderResults()` 中图片文件用 `<img>` 替换 emoji 图标，异步加载

**3.5** 修改 `src/styles.css`：追加 `.result-thumbnail` 样式

**验证**：`cargo check --workspace` → ✅

### Phase 4：FSEvents 文件监听

**4.1** 修改 `crates/sts-core/Cargo.toml`：添加 `notify = "8"`（默认 features 在 macOS 即走 FSEvents，无需显式 feature；最新稳定 8.2.0）

**4.2** 新建 `crates/sts-core/src/fsevents.rs`（~130 行）
- `FileWatcher`：封装 notify 的 FSEvents 监听
- `Debouncer`：防抖处理器（500ms 合并，去重后批量触发）

**4.3** 修改 `crates/sts-core/src/lib.rs`
- 添加 `pub mod fsevents;`
- `start_indexing_loop()` 改为 FSEvents 驱动，保留定时全量更新兜底（1 小时）

**验证**：`cargo check -p sts-core` → ✅

### Phase 5：MCP HTTP 服务（默认 9877，冲突自愈，优先复用 core_lib 基座）

**主路线（推荐）：复用 `core_lib::mcp`**（已在 sts-x v3.1.2 / batch_renamer 两项目验证，省 ~120 行手写壳层，`/health` `/tools` 自动生成）

**5.1** 修改 `src-tauri/Cargo.toml`：添加
`core_lib = { path = "../../rust_master_workspace/libs/core_lib", default-features = false, features = ["mcp"] }`
（❗路径以 src-tauri 为基准需实测确认层级：项目在 `/Users/xtap/Documents/AI/极速搜索/src-tauri`，core_lib 在 `/Users/xtap/Documents/AI/rust_master_workspace/libs/core_lib`，即 `../../rust_master_workspace/libs/core_lib`；写完先 `cargo check -p star-tap-fast-search` 验证路径。axum/tokio 由 `core_lib::mcp` re-export，不必单加 axum 依赖）

**5.2** 新建 `src-tauri/src/mcp.rs`（~80 行，比手写方案薄）
- 用 `McpServer::new(...)` + `Tool::new(...)` 声明 search_files / search_content / get_thumbnail 三个工具
- `search_files` / `search_content` 工具 schema 含可选 `human_filter: bool`（默认 `false` = 全量 AI 模式）；handler 内部调 `search_files(..., human_mode = !human_filter)`
- 业务 Router 只写 `POST /search`（`GET /tools` `GET /health` 由基座自动生成）
- 端口按 §2.6.4 冲突自愈逻辑绑定（默认 9877，占用 +1 探测，写 `mcp_port` 文件）
- 用法参考：`core_lib/src/mcp/mod.rs` + batch_renamer 的接入示例（见 §10 参考代码位置）

**5.3** 修改 `src-tauri/src/lib.rs`：添加 `mod mcp;` + setup 中 `tauri::async_runtime::spawn` 启动 MCP 服务（Tauri 2 自带 tokio runtime，不需自建 runtime）；MCP 工具 handler 统一以 `human_mode = false` 调搜索核心（全量），由 `human_filter` 入参决定是否转人类模式；把实际端口回传给 GUI 状态栏

**Fallback（仅当 core_lib path 依赖导致编译失败 >2 次）**：改手写 `axum = "0.8"`（最新稳定 0.8.9，注意 0.8 与 0.7 的 Router API 有差异），自己写 `/tools` `/search` 两个路由，~150 行；并在报告中注明已降级。

**验证**：`cargo check --workspace` → ✅，`curl localhost:9877/tools`（或实际端口）返回 JSON

### Phase 6：集成联调 + 代码审查

**6.1** 端到端测试：GUI 缩略图、中文分词、中英别名、缩写匹配、**人类模式系统垃圾不出现**、**人类模式代码结果靠后**、内容搜索、MCP（含端口冲突自愈验证：故意占 9877 看是否自动 9878；**AI 模式 `human_mode=false` 验证 `._`/`.git`/代码文件全量返回、不受人类过滤影响**；`human_filter=true` 时回到人类过滤）
**6.2** 调用 code-reviewer 审查全部代码
**6.3** 调用 test-generator 为新模块生成测试
**6.4** 调用 workspace-butler 清理临时文件

**验证**：`cargo check --workspace && cargo clippy --all-targets` → ✅ 零警告

## §9 验收与交付边界

- 运行：`cargo check --workspace && cargo clippy --all-targets`
- **到此停止**：不升版本、不 git push、不 Release、不部署

## §10 自我处置

- 小问题：独立判断，记 TODO 继续
- 大问题：暂停，贴文件+行号+尝试方案，报告用户
- 不编造：任何 API 先用 sts-x 定位现有源码确认（`sts-x file "关键词" -p <目录>` 或 `sts-x search`）；crate API 以 docs.rs 对应版本为准
- 依赖版本已在本白皮书 v0.3 按 crates.io 实测锁定（tantivy 0.26 / tantivy-jieba 0.20 / notify 8 / axum 0.8 仅 fallback），执行时不必重查、不要擅自改版本
- 智能模式：Phase 间不暂停确认，一路到底
- 行号偏移：每个 Phase 修改前先 Read 文件确认当前行号
- 空间原则：全程只用编译检查，不执行 release build

### 参考代码位置

| 要什么 | 去哪里读 |
|--------|---------|
| 现有搜索流程 | `crates/sts-core/src/lib.rs:954-996` |
| 现有排序/去重（过滤+排名收口） | `crates/sts-core/src/lib.rs:850-950`（`sort_and_dedup`） |
| 现有 GlobalIndex | `crates/sts-core/src/lib.rs:240-245` |
| 现有别名映射 | `crates/sts-core/src/lib.rs:509-587` |
| 现有 Tauri commands | `src-tauri/src/lib.rs:107-257` |
| 现有前端渲染 | `src/main.js:99-155` |
| 现有前端 HTML | `src/index.html:1-39` |
| 现有索引缓存路径 | `crates/sts-core/src/lib.rs:249`（`~/Library/Caches/com.xtap.search/index.cache`；BM25 索引目录放同级 `bm25_index/`，缩略图缓存放同级 `thumbnails/`，MCP 实际端口写 `mcp_port`） |
| core_lib MCP 基座 | `/Users/xtap/Documents/AI/rust_master_workspace/libs/core_lib/src/mcp/mod.rs`（McpServer/Tool/ai_instructions! 用法） |
| MCP 接入实例（sts-x） | `/Users/xtap/Documents/AI/sts-x` 的 serve 模块（core_lib::mcp::axum 引入方式） |
| sts-x-3 参考设计 | `/Users/xtap/Documents/AI/sts-x-3/DESIGN.md` |
| 星TAP_Pro 经验 | `/Users/xtap/Documents/AI/星TAP_Pro_Source/.trae/plans/WHITEPAPER-startap-pro-v2-upgrade.md` |
| 防坑指南（Phase 0 必读） | `/Users/xtap/Documents/AI/_shared-knowledge/ai-common-pitfalls.md` |
