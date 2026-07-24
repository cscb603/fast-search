# 极速搜索 v3.0 升级 — 新对话启动指令（复制下方全文到干净新对话）

---

执行：极速搜索 v3.0 升级（v0.3 | 智能执行模式）

以下白皮书是唯一行为准则。只需执行，不需重新设计，不逐 Phase 确认，一路到底。
白皮书路径：`/Users/xtap/Documents/AI/极速搜索/.trae/plans/WHITEPAPER-v3-upgrade.md`
（第一步先 Read 它——注意开头「v0.3 修订摘要」6 条；第二步读防坑指南 `/Users/xtap/Documents/AI/_shared-knowledge/ai-common-pitfalls.md`；然后严格按 §0 技能调用 + §1 契约 + §8 实施顺序执行）

本机环境：rustc 1.93.1、cargo 1.93.1、磁盘可用 54GB
项目路径：`/Users/xtap/Documents/AI/极速搜索`

核心契约：加 BM25 索引（tantivy 0.26 + tantivy-jieba 0.20）+ 自研模糊匹配（前缀树+编辑距离）+ 缩略图（LRU+qlmanage）+ FSEvents 实时监听（notify 8）+ MCP 服务（**默认端口 9877，冲突自动+1 探测、写 mcp_port 文件、支持 STS_MCP_PORT 环境变量**，优先复用 core_lib::mcp 基座）。保持轻快准，不改 UI 布局，不引入 ML 模型。**双模式搜索（§2.6）**：人类 GUI = `.`/`._` 系统垃圾硬过滤 + 代码/程序目录排名降级（选"代码"过滤或精确命中文件名可豁免）；AI/MCP = 全量不筛选不降级，MCP 工具暴露可选 `human_filter` 参数，AI 给人看结果时可复用人类过滤。

技能调用：按 §0 表格——代码定位全程用 sts-x（省 ~80% token）；每 Phase 完成后调 code-reviewer；新增模块后调 test-generator；遇 Bug 先调 systematic-debugging（分层定位→验证假设→最小复现→根因修复）再改代码；P6 收尾调 industrial-code-sop 门禁 + workspace-butler 清理。

实施顺序：P1 BM25 → P2 模糊匹配 → P3 缩略图 → P4 FSEvents → P5 MCP → P6 集成审查。P1 在 GlobalIndex 一次性声明所有新字段，后续 Phase 只赋值不改结构（§6）。

成功标准：编译零错误、缩略图可见、中文分词+中英别名搜索（注意二者分属 P1/P2，见 §3）、缩写匹配、**人类模式系统垃圾(. / ._)不出现**、**人类模式代码结果不抢前列**、**AI 模式全量返回（不过滤不降级）**、原有功能不退化、clippy 零警告。停在部署交接处：不升版本、不 git push、不 Release。

铁律：依赖版本已锁定（§1），不要擅自改；MCP 默认 9877 不是 9876（9876 是 sts-x 的），端口冲突自动+1 探测，写 mcp_port 文件，支持 STS_MCP_PORT 强制指定；改前先 Read 文件确认当前行号；全程只 cargo check 不 release build；首次拉依赖遇 SSL 错按 VPN 重连处理，5s 间隔耐心重试；小问题记 TODO 继续，大问题暂停报告。每完成一步输出「✅ Phase X 完成」+ 关键结果。

---

## 附：本地技能就绪清单（2026-07-24 已核验，无需额外安装）

| 技能 | 状态 | 用途 |
|------|------|------|
| sts-x | ✅ `/usr/local/bin/sts-x` + skill | 代码定位 |
| systematic-debugging | ✅ 已装 | 遇 Bug 分层排障 |
| code-reviewer | ✅ 已装 | 每 Phase 审查 |
| test-generator | ✅ 已装 | 新模块单测 |
| code-refactorer | ✅ 已装 | 按需重构 |
| rust-expert | ✅ 已装 | Rust 疑难 |
| industrial-code-sop | ✅ 已装 | clippy 门禁 |
| workspace-butler | ✅ 已装 | 收尾清理 |
