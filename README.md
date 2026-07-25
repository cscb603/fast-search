# ⚡️ 星TAP | 极速搜索 (Fast Search)
> **English / SEO:** StarTap Fast Search (macOS) is an Everything-like lightning file search for Mac. Millisecond results, full-disk + external-drive indexing, smart ranking, and fuzzy search — built with Rust + Tauri, fully local and private. The fast Spotlight alternative Mac users have been waiting for.
>
> **Tags:** macOS file search · Spotlight alternative · Everything for Mac · desktop search · fast finder · Rust + Tauri · local search · fuzzy search

> **堪比 Everything 的 Mac 极速文件搜索神器。毫秒级响应，告别 Spotlight 的迟钝。**

[中文介绍](#-为什么需要它) | [English Introduction](#-why-this-project)

---

## 🌟 为什么需要它？

在 Windows 上我们有 Everything，但在 Mac 上，系统自带的 Spotlight 经常搜不到文件，或者索引缓慢。
**星TAP | 极速搜索** 是专门为追求极致速度的用户打造的：
- 🚀 **毫秒级搜索**：无论你有多少文件，输入即显示，真正的零延迟。
- 🔍 **全盘扫描**：自动索引你的桌面、下载、文档、应用程序，甚至包括**外接硬盘**和 **U盘**。
- 🧠 **智能排序**：自动记录你的点击习惯，越常用的文件排得越靠前。
- 💻 **极简操作**：按下快捷键，输入关键词，直接回车打开。

## 📸 界面预览

![极速搜索界面1](极速搜索界面1.jpg)
![极速搜索界面2](极速搜索界面2.jpg)

### ✨ 它能解决什么问题？
- **Spotlight 搜不到？** 我们直接调用底层索引，哪怕是隐藏文件夹里的东西也能翻出来。
- **外接盘搜索慢？** 自动监控 `/Volumes` 变化，插上 U盘即刻完成索引。
- **文件太多记不住？** 模糊搜索算法，只要记得文件名的一部分就能找到。
- **免费且纯净**：完全本地运行，不联网，不占内存，这就是你要的 Everything Mac 版。

---

## 🛠 快速上手 (Quick Start)

> **🎉 点击前往下载：[最新版星TAP极速搜索 (.dmg)](https://github.com/cscb603/fast-search/releases/latest)**

1. 下载并解压，将 `星TAP 极速搜索.app` 拖入你的**应用程序**文件夹，或直接安装 `.dmg` 文件。
2. 首次启动时，它会自动在后台扫描文件建立索引（通常只需几秒钟）。
3. **快捷键**：默认支持快捷键呼出（可在设置中配置）。

---

## ✨ 2026 年 2 月重大升级 (v1.0.0 工业级版)

- 🦀 **Rust + Tauri 2.0 重构**：底座全面升级，性能更稳健，兼容性更强。
- 🎨 **品牌视觉同步**：接入星TAP实验室统一视觉标准，图标更精致。
- 🛠️ **工业级工作流**：通过星TAP实验室 Master Workflow 重新编译，优化了二进制体积与启动速度。
- 🛡️ **安全增强**：完全本地运行，所有索引数据加密存储。

---

## 🤓 技术细节 (For Techies)

本项目基于 **Tauri 2.0 + Rust** 开发，追求极致的系统性能与内存安全。

- **核心逻辑**：
  - 使用 Rust 的 `std::process` 异步调用底层 `find` 指令，并结合自研的 `GlobalIndex` 缓存机制。
  - **索引持久化**：索引结果加密存储在 `~/Library/Caches/` 下，重启秒开。
  - **动态监听**：后台线程每 30 秒轮询 `/Volumes` 状态，实时更新移动存储索引。
- **搜索算法**：
  - 基于点击频次的权重排序（Click History Ranking）。
  - 支持高性能的正则匹配与模糊过滤。
- **UI 架构**：采用 Tauri 的原生渲染引擎，安装包极小且 UI 响应迅速。

---

## English Introduction

### ⚡️ Why this project?
Frustrated with the slow or incomplete indexing of macOS Spotlight? **Fast Search** brings the "Everything-like" experience to Mac. Built with **Rust and Tauri**, it offers millisecond-level search results with zero latency.

### ✨ Key Features
- **Instant Search**: Type and find results immediately.
- **External Drive Support**: Automatically indexes USB drives and external SSDs.
- **Smart Ranking**: Your most-used files move to the top automatically.
- **Privacy Focused**: Works entirely offline. No data ever leaves your computer.

---

## 开源协议 (License)
MIT License

---

## 📥 Download

| | |
|---|---|
| **Releases** | https://github.com/cscb603/fast-search/releases/latest |
| **StarTAP Lab** | 极致速度，极简生活 · Extreme speed, minimalist life |

> Keywords: macOS file search · Spotlight alternative · Everything for Mac · desktop search · fast finder · Rust + Tauri · local search · fuzzy search
