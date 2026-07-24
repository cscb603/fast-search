# 极速搜索 v3.0 回归分析报告

> 写给另一个对话的 AI：你刚完成的 v3.0 升级**越改越慢、丢失了原有功能**，请逐项自查修复。

---

## 一、原版 v2.2 核心架构（不能丢）

原版搜索有三条并行路径，缺一不可：

```
search_files() 入口
  ├─ tokio::join!(                          ← 并行！不是串行！
  │     rg_index_search()   → rg 搜索引缓存  (快，~50ms)
  │     spotlight_search()  → mdfind Spotlight (全盘覆盖面)
  │  )
  └─ sort_and_dedup() → 多因子打分排序
```

**关键代码位置**：`crates/sts-core/src/lib.rs` 原版 `search_files()` 函数第 954-996 行

**你至少丢了这些**：
- `tokio::join!` 并行（改成串行就会慢）
- Spotlight mdfind 搜索（macOS 全盘索引，覆盖面最广）
- 点击历史自学习（`click_history`，记录用户点击，影响排序）
- 多因子打分（名称匹配/路径深度/别名命中/点击次数）

---

## 二、原版排序算法（你大概率丢了）

原版 `sort_and_dedup()` 函数，第 851-950 行，多级打分：

| 匹配类型 | 加分 | 说明 |
|---------|------|------|
| 名称精确相等 / 别名匹配 / 缩写匹配 | +20000 | 最高优先级 |
| 名称连续匹配 | +10000 | 如 "photo shop" 连续命中 "Adobe Photoshop" |
| 名称非连续匹配 | +5000 | 各词分散在名称中 |
| 路径匹配 | +2000 | 词在路径中但不在名称中 |
| 程序类加分 | +10000 | filter_type=app 且以 .app 结尾 |
| 点击历史 | +5000 × 点击次数 | 自学习，越常点越靠前 |
| /Applications 路径 | +5000 | 应用优先 |
| /Desktop 路径 | +1000 | 桌面文件优先 |
| .app/Contents/ 子路径 | -10000 | 应用内部文件降权 |
| 路径深度 | -50 × 层数 | 深层文件降权 |

**你要确认**：v3 的 BM25 搜索返回后，是否仍经过 `sort_and_dedup()` 做最终排序？BM25 分数需要和现有打分体系融合，不能只靠 BM25。

---

## 三、原版索引系统（你大概率丢了一半）

原版 `GlobalIndex` 有三个关键机制：

1. **索引缓存文件**（`~/Library/Caches/com.xtap.search/index.cache`）—— 纯文本，一行一个路径
2. **rg 搜索索引缓存**（`rg_index_search`）—— 用 ripgrep 直接搜缓存文件，~0.1 秒
3. **后台循环更新**（`start_indexing_loop`）—— 每 30 秒检查磁盘变化

**你加了 BM25 是对的，但不能删掉这几样**：
- 索引缓存文件仍然需要（BM25 从它重建）
- rg 搜索必须保留作为 fallback（BM25 索引损坏/不存在时）
- 后台循环不能丢（FSEvents 是增量补充，不是替代）

---

## 四、原版别名映射（200+ 条，你大概率没接上）

`build_alias_mapping()` 第 509-587 行：

- 200+ 条手工映射（ps→Photoshop、vscode→Visual Studio Code 等）
- `/Applications` 动态扫描（自动生成首字母缩写）
- 中文俗称映射（微信→WeChat、剪映→VideoFusion）

**FuzzyMatcher 是增强，不是替代**。四层匹配策略：
1. **alias**：先查静态映射表（原版 `build_alias_mapping`）
2. **acronym**：前缀树首字母缩写（新增）
3. **fuzzy_ed**：编辑距离纠错（新增，如 `photoshp`→`photoshop`）
4. **contains**：子串包含（新增）

第 1 层不能丢——200+ 条精确映射的精度高于任何模糊算法。

---

## 五、缩略图实现检查

`ThumbnailCache` 应该：
- 三级查找：内存缓存 → 磁盘缓存 → `qlmanage -t` 生成
- 异步：用 `tokio::spawn` 不阻塞搜索
- LRU 淘汰：超过 200 条删最旧的 20%
- 前端异步加载：先显示 emoji → `invoke("get_thumbnail")` → 替换为 `<img>`

**你可能忘了**：
- `qlmanage -t -s 128` 的输出文件名是 `原文件名.png`，需要移动到规范化缓存路径
- 前端需要 fallback：缩略图加载失败时保留 emoji
- 缩略图 command 要加超时（qlmanage 对某些格式会卡住）

---

## 六、FSEvents 集成检查

FSEvents 是**增量补充**，不是替代：

```
原版：30 秒轮询 → 检查磁盘变化 → 全量重建
v3 版：FSEvents 事件 → Debouncer(500ms) → 增量更新索引
       └─ 同时保留 1 小时全量重建兜底
```

**两个机制必须共存**。纯 FSEvents 可能丢事件（卸载磁盘、睡眠唤醒等场景）。

---

## 七、MCP 服务检查

`localhost:9876` 的两个端点：

```
GET  /tools  → {"tools":[{"name":"search_files",...},{"name":"search_content",...}]}
POST /search → {"query":"xxx","type":"all"} → {"results":[...]}
```

**注意**：MCP 是给 AI 用的，搜索接口必须和 GUI 共用同一套逻辑，不能写两遍。

---

## 八、原版 v2.2 功能清单（逐项确认）

| # | 功能 | 原版代码位置 | v3 是否还在？ |
|---|------|-------------|-------------|
| 1 | 全部搜索（filter_type=all） | lib.rs search_files | 待确认 |
| 2 | 图片搜索（类型过滤） | lib.rs SearchStrategy::from_type("image") | 待确认 |
| 3 | 视频搜索 | lib.rs SearchStrategy::from_type("video") | 待确认 |
| 4 | 文档搜索 | lib.rs SearchStrategy::from_type("doc") | 待确认 |
| 5 | 程序搜索 | lib.rs SearchStrategy::from_type("app") | 待确认 |
| 6 | 文件夹搜索 | lib.rs SearchStrategy::from_type("folder") | 待确认 |
| 7 | 内容搜索（rg 全文） | lib.rs search_content | 待确认 |
| 8 | fd 加速扫描 | lib.rs has_fd() + build_index_once | 待确认 |
| 9 | find 兜底（无 fd 时） | lib.rs find_prune_args | 待确认 |
| 10 | Spotlight mdfind 并行 | lib.rs spotlight_search | 待确认 |
| 11 | 点击历史自学习 | lib.rs click_history | 待确认 |
| 12 | 智能耗时显示 | lib.rs:988-993 | 待确认 |
| 13 | 结果去重（HashSet） | lib.rs sort_and_dedup | 待确认 |
| 14 | 垃圾路径过滤（/Contents/MacOS/等） | lib.rs 多处 | 待确认 |
| 15 | 外接盘索引 | lib.rs start_indexing_loop /Volumes | 待确认 |
| 16 | CLI 独立使用 | crates/sts-cli/src/main.rs | 待确认 |
| 17 | 全局快捷键 Cmd+Shift+F | src-tauri/src/lib.rs | 待确认 |
| 18 | 复制路径（含文件引用） | src-tauri/src/lib.rs copy_to_clipboard | 待确认 |
| 19 | 打开文件/文件夹 | src-tauri/src/lib.rs open_file/open_folder | 待确认 |
| 20 | 索引状态轮询 | src/main.js updateIndexingStatus | 待确认 |
| 21 | IME 输入法兼容 | src/main.js compositionstart/end | 待确认 |
| 22 | 内容搜索高亮 | src/main.js highlightKeyword | 待确认 |

---

## 九、为什么越改越慢（常见原因）

| 症状 | 可能原因 |
|------|---------|
| 首次搜索变慢 | BM25 索引未预加载，每次 search 时才 open |
| 每次搜索都慢 | BM25 和 Spotlight 串行了（应该 tokio::join!） |
| 新建目录扫描变慢 | FSEvents 触发全量重建而非增量更新 |
| 越用越慢 | 缩略图生成阻塞了搜索主线程 |
| 某些搜索返回空 | Spotlight 被删了 / rg fallback 被删了 |

**核心原则**：BM25 是**加速层**，不是**唯一层**。搜索路径应该是：

```
search_files()
  ├─ tokio::join!(           ← 三路并行
  │     bm25.search()        ← 新增，1-5ms
  │     spotlight_search()   ← 保留，全盘覆盖
  │     rg_index_search()    ← 保留，fallback
  │  )
  ├─ fuzzy_match() 扩展搜索词 ← 新增
  └─ sort_and_dedup()        ← 保留，多因子排序
```

---

## 十、修复优先级

| 优先级 | 修复项 | 说明 |
|--------|--------|------|
| **P0** | 恢复 `tokio::join!` 三路并行 | 速度问题的根源 |
| **P0** | 恢复 `spotlight_search()` | Spotlight 全盘覆盖不能丢 |
| **P0** | 恢复 `sort_and_dedup()` 多因子打分 | 排序精度问题的根源 |
| **P0** | 恢复 `build_alias_mapping` 200+ 条映射 | FuzzyMatcher 的第一层 |
| **P1** | BM25 索引在 `build_index_once` 时预建 | 避免首次搜索等待 |
| **P1** | FSEvents + 定时全量双保险 | 防止事件丢失 |
| **P1** | 缩略图异步加载不阻塞搜索 | 避免 UI 卡顿 |
| **P2** | 确认 22 项原版功能都在 | 逐项对照检查 |

---

## 十一、快速自查命令

在新对话里跑这些验证：

```bash
# 1. 编译
cd /Users/xtap/Documents/AI/极速搜索 && cargo check --workspace

# 2. 确认 Spotlight 还在
grep -n "mdfind\|spotlight_search" crates/sts-core/src/lib.rs

# 3. 确认 tokio::join 还在（并行搜索）
grep -n "tokio::join" crates/sts-core/src/lib.rs

# 4. 确认 sort_and_dedup 还在
grep -n "sort_and_dedup" crates/sts-core/src/lib.rs

# 5. 确认 build_alias_mapping 还在
grep -n "build_alias_mapping" crates/sts-core/src/lib.rs

# 6. 确认 rg_index_search 还在（作为 fallback）
grep -n "rg_index_search" crates/sts-core/src/lib.rs

# 7. 确认 click_history 还在
grep -n "click_history" crates/sts-core/src/lib.rs

# 8. 确认 CLI 还能用
cargo run -p sts-cli -- search "test" -t all
```

每个 grep 必须有输出。如果某行返回空，说明**那个功能被你删了**。

---

## 十二、参考：原版完整代码

原版 v2.2 的 `lib.rs` 位于 `/Users/xtap/Documents/AI/极速搜索/crates/sts-core/src/lib.rs`，共 1186 行。如果你不确定某个功能原来怎么写的，Read 这个文件对比。

**白皮书 §6.3 第 1 条写得清清楚楚**：「Phase 必须严格按顺序执行，不可跳过，不可并行」。你可能是跳过了 Phase 6（集成联调 + 逐项检查），直接以为改完就完事了。
