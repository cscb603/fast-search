//! 星TAP 极速搜索 - 核心搜索引擎
//! 不依赖 Tauri，可独立被 CLI / AI / Skill 调用

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::process::Command as AsyncCommand;

use bm25::Bm25Index;
use fuzzy::FuzzyMatcher;

pub mod bm25;
pub mod fsevents;
pub mod fuzzy;
pub mod thumbnail;

// ============================================================
// 公共数据结构
// ============================================================

#[derive(Serialize, Clone, Debug)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i32>,
    /// 搜索耗时（毫秒），仅慢查询（>100ms）才填充
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// 结果来源引擎
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ContentSearchResult {
    pub path: String,
    pub name: String,
    pub line_number: u64,
    pub line_content: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ContentSearchParams {
    pub keyword: String,
    #[serde(default = "default_search_path")]
    pub path: String,
    #[serde(default)]
    pub filter_type: String,
    #[serde(default = "default_max_content_results")]
    pub max_results: usize,
}

fn default_search_path() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

fn default_max_content_results() -> usize {
    50
}

// 内部搜索结果（含 score，不序列化）
#[derive(Clone, Debug)]
pub(crate) struct InternalSearchResult {
    pub path: String,
    pub name: String,
    pub score: i32,
    pub source: String, // "rg" | "spotlight" | "memory"
}

// ============================================================
// 工具检测（启动时检测一次，缓存结果）
// ============================================================

use std::sync::OnceLock;

static FD_AVAILABLE: OnceLock<bool> = OnceLock::new();
static RG_AVAILABLE: OnceLock<bool> = OnceLock::new();

pub fn has_fd() -> bool {
    *FD_AVAILABLE.get_or_init(|| {
        std::process::Command::new("fd")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

pub fn has_rg() -> bool {
    *RG_AVAILABLE.get_or_init(|| {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

// ============================================================
// fd 排除规则（统一管理，索引构建和后台更新共用）
// ============================================================

/// 返回 fd 的 --exclude 参数列表
fn fd_exclude_args() -> Vec<&'static str> {
    // 这些目录通常是开发/包管理/缓存，体积巨大但搜索价值低
    vec![
        "COS_Mount", // 桌面 webdav 网盘挂载点，网络遍历极慢且无本地搜索价值
        "node_modules",
        ".git",
        "Library",
        "Contents/MacOS",
        ".cache",
        ".Trash",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        ".cargo",
        ".rustup",
        ".nvm",
        ".npm",
        ".yarn",
        "miniforge3",
        "anaconda3",
        "miniconda3",
        "conda",
        ".local/share/Trash",
        ".docker",
        ".gradle",
        ".m2",
        "Pods",
        ".build",
        ".serverless",
        ".terraform",
        "vendor/bundle",
        ".tox",
        ".eggs",
        "*.egg-info",
        "dist",
        "build",
        "out",
        ".next",
        ".nuxt",
    ]
}

/// 返回 find 的 -path/-name 剪枝参数
#[allow(dead_code)]
fn find_prune_args() -> Vec<String> {
    let dirs = [
        "node_modules",
        ".git",
        "Library",
        "Contents/MacOS",
        ".cache",
        ".Trash",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        ".cargo",
        ".rustup",
        ".nvm",
        ".npm",
        ".yarn",
        "miniforge3",
        "anaconda3",
        "miniconda3",
        ".docker",
        ".gradle",
        ".m2",
        "Pods",
        ".build",
        ".serverless",
        "vendor",
        ".tox",
        ".eggs",
        "dist",
        "build",
        "out",
        ".next",
    ];
    let mut args = Vec::new();
    args.push("(".to_string());
    for (i, d) in dirs.iter().enumerate() {
        if i > 0 {
            args.push("-o".to_string());
        }
        args.push("-path".to_string());
        args.push(format!("*/{}/*", d));
    }
    args.push(")".to_string());
    args.push("-prune".to_string());
    args.push("-o".to_string());
    args.push("-print".to_string());
    args
}

// ============================================================
// 搜索策略
// ============================================================

pub struct SearchStrategy {
    pub spotlight_kind: String,
    pub extensions: Vec<&'static str>,
}

impl SearchStrategy {
    pub fn from_type(t: &str) -> Self {
        match t {
            "image" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'public.image'".to_string(),
                extensions: vec![".jpg", ".png", ".jpeg", ".gif", ".webp", ".bmp", ".heic"],
            },
            "video" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'public.movie'".to_string(),
                extensions: vec![".mp4", ".mov", ".avi", ".mkv", ".flv", ".wmv"],
            },
            "audio" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'public.audio'".to_string(),
                extensions: vec![".mp3", ".wav", ".flac", ".aac", ".m4a"],
            },
            "pdf" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'com.adobe.pdf'".to_string(),
                extensions: vec![".pdf"],
            },
            "doc" => Self {
                spotlight_kind: "(kMDItemContentTypeTree == 'public.text' || kMDItemContentTypeTree == 'public.content' || kMDItemContentTypeTree == 'com.microsoft.word.doc' || kMDItemContentTypeTree == 'com.adobe.pdf')".to_string(),
                extensions: vec![".pdf", ".txt", ".md", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx"],
            },
            "folder" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'public.folder'".to_string(),
                extensions: vec![],
            },
            "app" => Self {
                spotlight_kind: "(kMDItemContentTypeTree == 'com.apple.application-bundle' || kMDItemContentTypeTree == 'com.apple.systempreference.pane')".to_string(),
                extensions: vec![".app", ".prefPane"],
            },
            _ => Self {
                spotlight_kind: "".to_string(),
                extensions: vec![],
            },
        }
    }

    pub fn spotlight_query(&self, words: &[&str], alias: Option<&String>) -> String {
        let mut parts = Vec::new();
        for word in words {
            if !word.is_empty() {
                parts.push(format!("kMDItemFSName == '*{}*'cd", word));
            }
        }

        if parts.is_empty() && alias.is_none() {
            return self.spotlight_kind.clone();
        }

        let base_query = if let Some(en_name) = alias {
            let alias_part = format!("kMDItemFSName == '*{}*'cd", en_name);
            if !parts.is_empty() {
                format!("(({}) || {})", parts.join(" && "), alias_part)
            } else {
                alias_part
            }
        } else if parts.len() > 1 {
            format!("({})", parts.join(" && "))
        } else {
            parts[0].clone()
        };

        if self.spotlight_kind.is_empty() {
            base_query
        } else {
            format!("({}) && ({})", base_query, self.spotlight_kind)
        }
    }

    pub fn matches_extension(&self, path: &str) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        let path_lc = path.to_lowercase();
        if self.extensions.contains(&".app")
            && path_lc.contains(".app")
            && !path_lc.contains(".app/contents/")
        {
            return true;
        }
        self.extensions.iter().any(|ext| path_lc.ends_with(ext))
    }
}

// ============================================================
// 全局索引
// ============================================================

#[derive(Clone)]
pub struct GlobalIndex {
    pub files: Arc<Mutex<Vec<String>>>,
    pub is_indexing: Arc<Mutex<bool>>,
    pub force_update: Arc<AtomicBool>,
    pub bm25: Arc<Mutex<Option<Arc<Bm25Index>>>>,
    pub fuzzy: Arc<Mutex<Option<Arc<FuzzyMatcher>>>>,
    pub thumbnails: Arc<thumbnail::ThumbnailCache>,
}

pub fn get_index_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let path = PathBuf::from(home).join("Library/Caches/com.xtap.search/index.cache");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// 判断缓存文件是否需要重建（超过24小时或不存在）
pub fn is_cache_stale() -> bool {
    let index_path = get_index_path();
    if !index_path.exists() {
        return true;
    }
    match std::fs::metadata(&index_path).and_then(|m| m.modified()) {
        Ok(modified) => {
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or(std::time::Duration::MAX);
            age > std::time::Duration::from_secs(86400) // 24小时
        }
        Err(_) => true,
    }
}

impl GlobalIndex {
    /// 创建空索引（不加载缓存文件），CLI 搜索模式使用
    /// rg 直接搜缓存文件 + mdfind 搜 Spotlight，不需要内存索引
    pub fn empty() -> Self {
        Self {
            files: Arc::new(Mutex::new(Vec::new())),
            is_indexing: Arc::new(Mutex::new(false)),
            force_update: Arc::new(AtomicBool::new(false)),
            bm25: Arc::new(Mutex::new(None)),
            fuzzy: Arc::new(Mutex::new(None)),
            thumbnails: Arc::new(thumbnail::ThumbnailCache::default()),
        }
    }

    pub fn new() -> Self {
        let files = Arc::new(Mutex::new(Vec::new()));
        let is_indexing = Arc::new(Mutex::new(false));
        let force_update = Arc::new(AtomicBool::new(false));

        // 尝试加载现有索引
        let index_path = get_index_path();
        if index_path.exists() {
            if let Ok(file) = File::open(&index_path) {
                let reader = BufReader::new(file);
                let mut loaded_files = Vec::new();
                for line in reader.lines().map_while(Result::ok) {
                    loaded_files.push(line);
                }
                eprintln!("[sts] 从缓存加载了 {} 条索引", loaded_files.len());
                let mut guard = files.lock().unwrap();
                *guard = loaded_files;
            }
        }

        Self {
            files,
            is_indexing,
            force_update,
            bm25: Arc::new(Mutex::new(None)),
            fuzzy: Arc::new(Mutex::new(None)),
            thumbnails: Arc::new(thumbnail::ThumbnailCache::default()),
        }
    }

    /// 获取索引条目数量
    pub fn len(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    /// 索引是否为空
    pub fn is_empty(&self) -> bool {
        self.files.lock().unwrap().is_empty()
    }

    /// 获取缩略图（数据 URI）
    pub fn get_thumbnail(&self, path: &str, size: u32) -> Option<String> {
        self.thumbnails.get(path, size)
    }

    /// 一次性索引构建（CLI 模式使用，不启动后台循环）
    pub async fn build_index_once(&self) {
        {
            let mut guard = self.is_indexing.lock().unwrap();
            *guard = true;
        }

        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let mut scan_paths = vec![
            format!("{}/Desktop", home),
            format!("{}/Downloads", home),
            format!("{}/Documents", home),
            "/Applications".to_string(),
        ];

        if std::path::Path::new("/Volumes").exists() {
            scan_paths.push("/Volumes".to_string());
        }

        let mut all_files = Vec::new();
        let use_fd = has_fd();

        for path in &scan_paths {
            if !std::path::Path::new(path).exists() {
                continue;
            }
            eprintln!(
                "[sts] 正在扫描: {} (使用 {})",
                path,
                if use_fd { "fd" } else { "find" }
            );

            let output = if use_fd {
                let mut cmd = AsyncCommand::new("fd");
                cmd.arg("--hidden")
                    .arg("--absolute-path")
                    .arg("--type")
                    .arg("f")
                    .arg("--type")
                    .arg("d")
                    .arg("--max-depth")
                    .arg("20");
                for excl in fd_exclude_args() {
                    cmd.arg("--exclude").arg(excl);
                }
                cmd.arg(".").arg(path);
                cmd.output().await
            } else {
                AsyncCommand::new("find")
                    .arg(path)
                    .args(find_prune_args())
                    .output()
                    .await
            };

            if let Ok(out) = output {
                let content = String::from_utf8_lossy(&out.stdout);
                for line in content.lines() {
                    let p = line.trim().to_string();
                    if p != *path && !p.is_empty() {
                        all_files.push(p);
                    }
                }
            }
        }

        // 保存到缓存
        let index_path = get_index_path();
        if let Ok(mut file) = File::create(&index_path) {
            for f in &all_files {
                let _ = writeln!(file, "{}", f);
            }
        }

        {
            let mut guard = self.files.lock().unwrap();
            *guard = all_files;
        }
        {
            let mut guard = self.is_indexing.lock().unwrap();
            *guard = false;
        }

        let count = {
            let guard = self.files.lock().unwrap();
            guard.len()
        };
        eprintln!("[sts] 索引构建完成，共 {} 条", count);

        // 增强层：构建 BM25 索引（中文分词）
        {
            let files_guard = self.files.lock().unwrap();
            if let Some(bm25) = Bm25Index::create() {
                if bm25.rebuild_from_cache(&files_guard).is_ok() {
                    let mut g = self.bm25.lock().unwrap();
                    *g = Some(bm25);
                }
            }
        }
        // 增强层：构建模糊匹配器（别名/缩写/编辑距离）
        {
            let files_guard = self.files.lock().unwrap();
            let fm = FuzzyMatcher::build_from_paths(&files_guard);
            let mut g = self.fuzzy.lock().unwrap();
            *g = Some(fm);
        }
    }

    /// 创建后台索引循环 Future（由调用方 spawn 到正确的 runtime）
    /// Tauri GUI 模式：`tauri::async_runtime::spawn(index.start_indexing_loop());`
    /// CLI 模式：`tokio::spawn(index.start_indexing_loop());`
    pub fn start_indexing_loop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let files_clone = self.files.clone();
        let status_clone = self.is_indexing.clone();
        let force_update_clone = self.force_update.clone();
        let bm25_clone = self.bm25.clone();
        let fuzzy_clone = self.fuzzy.clone();

        Box::pin(async move {
            // 启动 FSEvents 实时监听：文件变更置位 force_update，由下方循环拾取重建
            let home0 = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            let watch_paths = vec![
                format!("{}/Desktop", home0),
                format!("{}/Downloads", home0),
                format!("{}/Documents", home0),
                "/Applications".to_string(),
            ];
            fsevents::start_watching(watch_paths, force_update_clone.clone());
            // 首次进入循环即强制全量构建（含 BM25 / 模糊匹配），
            // 不依赖外接盘变化或 10 分钟定时，保证启动即可搜。
            force_update_clone.store(true, Ordering::Relaxed);
            let mut last_volumes = std::collections::HashSet::new();
            let mut last_full_scan = std::time::Instant::now();

            loop {
                let mut current_volumes = std::collections::HashSet::new();
                if let Ok(entries) = std::fs::read_dir("/Volumes") {
                    for entry in entries.flatten() {
                        current_volumes.insert(entry.path().to_string_lossy().to_string());
                    }
                }

                let volumes_changed = current_volumes != last_volumes;
                let time_to_update =
                    last_full_scan.elapsed() > tokio::time::Duration::from_secs(600);
                let force_now = force_update_clone.load(Ordering::Relaxed);

                if volumes_changed || time_to_update || force_now {
                    eprintln!(
                        "[sts] 开始更新索引 (原因: {})",
                        if force_now {
                            "手动触发"
                        } else if volumes_changed {
                            "磁盘变化"
                        } else {
                            "定期更新"
                        }
                    );

                    force_update_clone.store(false, Ordering::Relaxed);
                    last_volumes = current_volumes;
                    last_full_scan = std::time::Instant::now();

                    {
                        let mut guard = status_clone.lock().unwrap();
                        *guard = true;
                    }

                    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
                    let mut scan_paths = vec![
                        format!("{}/Desktop", home),
                        format!("{}/Downloads", home),
                        format!("{}/Documents", home),
                        "/Applications".to_string(),
                    ];
                    if std::path::Path::new("/Volumes").exists() {
                        scan_paths.push("/Volumes".to_string());
                    }

                    let mut all_files = Vec::new();
                    let use_fd = has_fd();

                    for path in &scan_paths {
                        if !std::path::Path::new(path).exists() {
                            continue;
                        }

                        let output = if use_fd {
                            let mut cmd = AsyncCommand::new("fd");
                            cmd.arg("--hidden")
                                .arg("--absolute-path")
                                .arg("--type")
                                .arg("f")
                                .arg("--type")
                                .arg("d")
                                .arg("--max-depth")
                                .arg("20");
                            for excl in fd_exclude_args() {
                                cmd.arg("--exclude").arg(excl);
                            }
                            cmd.arg(".").arg(path);
                            cmd.output().await
                        } else {
                            AsyncCommand::new("find")
                                .arg(path)
                                .args(find_prune_args())
                                .output()
                                .await
                        };

                        if let Ok(out) = output {
                            let content = String::from_utf8_lossy(&out.stdout);
                            for line in content.lines() {
                                let p = line.trim().to_string();
                                if p != *path && !p.is_empty() {
                                    all_files.push(p);
                                }
                            }
                        }
                    }

                    let index_path = get_index_path();
                    if let Ok(mut file) = File::create(&index_path) {
                        for f in &all_files {
                            let _ = writeln!(file, "{}", f);
                        }
                    }

                    let count = all_files.len();
                    {
                        let mut guard = files_clone.lock().unwrap();
                        *guard = all_files;
                    }
                    {
                        let mut guard = status_clone.lock().unwrap();
                        *guard = false;
                    }
                    eprintln!("[sts] 索引更新完成，共 {} 条", count);

                    // 增强层：重建 BM25 + 模糊匹配（文件列表已变）
                    {
                        let files_guard = files_clone.lock().unwrap();
                        if let Some(bm25) = Bm25Index::create() {
                            if bm25.rebuild_from_cache(&files_guard).is_ok() {
                                let mut g = bm25_clone.lock().unwrap();
                                *g = Some(bm25);
                            }
                        }
                        let fm = FuzzyMatcher::build_from_paths(&files_guard);
                        let mut g = fuzzy_clone.lock().unwrap();
                        *g = Some(fm);
                    }
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            }
        })
    }
}

impl Default for GlobalIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// 应用别名映射
// ============================================================

pub fn build_alias_mapping() -> HashMap<String, String> {
    let mut new_map = HashMap::new();
    let aliases = [
        // 设计类
        ("ps", "photoshop"),
        ("lr", "lightroom"),
        ("pr", "premiere"),
        ("ae", "after effects"),
        ("ai", "illustrator"),
        ("id", "indesign"),
        ("au", "audition"),
        ("dw", "dreamweaver"),
        ("an", "animate"),
        ("pl", "prelude"),
        ("br", "bridge"),
        ("ch", "character animator"),
        ("me", "media encoder"),
        ("ic", "incopy"),
        ("fs", "fuse"),
        ("sc", "scout"),
        ("st", "stock"),
        ("xd", "xd"),
        ("dc", "acrobat"),
        ("dpp", "digital photo professional"),
        ("fcpx", "final cut pro"),
        ("c4d", "cinema 4d"),
        ("sketch", "sketch"),
        ("figma", "figma"),
        ("photoshop", "photoshop"),
        ("illustrator", "illustrator"),
        ("premiere", "premiere"),
        ("aftereffects", "after effects"),
        ("lightroom", "lightroom"),
        // 社交/办公
        ("wx", "wechat"),
        ("微信", "wechat"),
        ("qq", "qq"),
        ("dd", "dingtalk"),
        ("钉钉", "dingtalk"),
        ("fs", "feishu"),
        ("飞书", "feishu"),
        ("lark", "feishu"),
        ("word", "microsoft word"),
        ("excel", "microsoft excel"),
        ("ppt", "microsoft powerpoint"),
        ("wps", "wpsoffice"),
        ("pdf", "acrobat"),
        ("obs", "obs studio"),
        ("yx", "neteasemail"),
        ("邮箱", "mail"),
        ("notes", "notes"),
        ("memo", "notes"),
        ("wechat", "wechat"),
        ("dingtalk", "dingtalk"),
        ("feishu", "feishu"),
        // 视频/娱乐/AI
        ("jy", "videofusion"),
        ("剪映", "videofusion"),
        ("capcut", "videofusion"),
        ("vf", "videofusion"),
        ("db", "doubao"),
        ("豆包", "doubao"),
        ("doubao", "doubao"),
        ("videofusion", "videofusion"),
        ("db", "douban"),
        ("dy", "douyin"),
        ("bili", "bilibili"),
        ("bz", "bilibili"),
        ("music", "music"),
        ("网易云", "neteasemusic"),
        ("spotify", "spotify"),
        ("douyin", "douyin"),
        ("tiktok", "douyin"),
        ("jianying", "videofusion"),
        ("jianyingpro", "videofusion"),
        // 生产力
        ("wp", "wpsoffice"),
        ("wps", "wpsoffice"),
        ("pages", "pages"),
        ("numbers", "numbers"),
        ("keynote", "keynote"),
        // 工具/开发
        ("llq", "browser"),
        ("浏览器", "browser"),
        ("safari", "safari"),
        ("chrome", "google chrome"),
        ("edge", "microsoft edge"),
        ("fd", "finder"),
        ("访达", "finder"),
        ("zd", "terminal"),
        ("终端", "terminal"),
        ("iterm", "iterm"),
        ("code", "visual studio code"),
        ("vs", "visual studio code"),
        ("vscode", "visual studio code"),
        ("st", "sublime text"),
        ("idea", "intellij idea"),
        ("webstorm", "webstorm"),
        ("py", "pycharm"),
        ("git", "github"),
        ("postman", "postman"),
        ("docker", "docker"),
        // 系统/其他
        ("sz", "settings"),
        ("设置", "settings"),
        ("jh", "calculator"),
        ("计算器", "calculator"),
        ("activity", "activity monitor"),
        ("monitor", "activity monitor"),
        ("disk", "disk utility"),
        ("keychain", "keychain access"),
        ("console", "console"),
        // 通讯/会议
        ("tg", "telegram"),
        ("telegram", "telegram"),
        ("dc", "discord"),
        ("discord", "discord"),
        ("slack", "slack"),
        ("zoom", "zoom.us"),
        ("会议", "zoom.us"),
        ("腾讯会议", "tencent meeting"),
        ("firefox", "firefox"),
        ("ff", "firefox"),
        ("brave", "brave browser"),
        ("arc", "arc"),
        ("opera", "opera"),
        // AI/开发工具
        ("cursor", "cursor"),
        ("windsurf", "windsurf"),
        ("claude", "claude"),
        ("warp", "warp"),
        ("vim", "vim"),
        ("nvim", "neovide"),
        ("blender", "blender"),
        ("unity", "unity hub"),
        ("chatgpt", "chatgpt"),
        ("gpt", "chatgpt"),
        ("ollama", "ollama"),
        // 娱乐/下载
        ("netease", "neteasemusic"),
        ("spotify", "spotify"),
        ("百度网盘", "baidunetdisk"),
        ("百度云", "baidunetdisk"),
        ("迅雷", "thunder"),
        ("qb", "qbittorrent"),
        // 系统工具
        ("预览", "preview"),
        ("图书", "books"),
        ("font", "font book"),
        ("字体册", "font book"),
        ("截图", "screenshot"),
        ("录屏", "quicktime player"),
        ("备忘录", "notes"),
        ("提醒事项", "reminders"),
        ("地图", "maps"),
        ("通讯录", "contacts"),
        ("日历", "calendar"),
    ];

    for (alias, real) in aliases {
        new_map.insert(alias.to_string(), real.to_string());
    }

    // 动态扫描 /Applications + ~/Applications
    let app_dirs: Vec<String> = {
        let mut v = vec!["/Applications".to_string()];
        if let Ok(h) = std::env::var("HOME") {
            v.push(format!("{}/Applications", h));
        }
        v
    };
    for app_dir in &app_dirs {
        if let Ok(entries) = std::fs::read_dir(app_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".app") {
                    let base_name = name.replace(".app", "").to_lowercase();
                    new_map
                        .entry(base_name.clone())
                        .or_insert(base_name.clone());
                    if base_name.contains(' ') || base_name.contains('-') {
                        let short: String = base_name
                            .split([' ', '-'])
                            .filter(|s| !s.is_empty())
                            .map(|s| s.chars().next().unwrap_or(' '))
                            .collect();
                        if short.len() > 1 {
                            new_map.entry(short).or_insert(base_name.clone());
                        }
                    }
                }
            }
        }
    }

    new_map
}

// ============================================================
// 核心搜索函数
// ============================================================

/// Spotlight 文件名搜索（并行搜索用户目录 + 外接盘）
async fn spotlight_search(
    keyword_lc: &str,
    filter_type: &str,
    mapping: &HashMap<String, String>,
) -> Vec<InternalSearchResult> {
    let strategy = SearchStrategy::from_type(filter_type);
    let mapped_keyword = mapping.get(keyword_lc).cloned();
    let words: Vec<&str> = keyword_lc.split_whitespace().collect();
    let final_query = strategy.spotlight_query(&words, mapped_keyword.as_ref());

    let mut tasks = vec![];

    // 任务 A: 用户目录 + 应用程序
    let q1 = final_query.clone();
    tasks.push(tokio::spawn(async move {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            AsyncCommand::new("mdfind")
                .arg("-onlyin")
                .arg(&home)
                .arg("-onlyin")
                .arg("/Applications")
                .arg(&q1)
                .output(),
        )
        .await;
        match output {
            Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(),
        }
    }));

    // 任务 B: 外接盘
    let q_vol = final_query.clone();
    tasks.push(tokio::spawn(async move {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            AsyncCommand::new("mdfind")
                .arg("-onlyin")
                .arg("/Volumes")
                .arg(&q_vol)
                .output(),
        )
        .await;
        match output {
            Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(),
        }
    }));

    let task_results = futures::future::join_all(tasks).await;

    let mut results = Vec::new();
    for content in task_results.into_iter().flatten() {
        for line in content.lines() {
            let path = line.trim().to_string();
            if path.is_empty() || path.contains("/Contents/MacOS/") || path.contains("/Library/") {
                continue;
            }
            let name = path.split('/').next_back().unwrap_or(&path).to_string();
            results.push(InternalSearchResult {
                path,
                name,
                score: 0,
                source: "spotlight".to_string(),
            });
        }
    }
    results
}

/// 用 rg/grep 搜索索引缓存文件（比内存线性遍历快 50-100 倍）
/// rg 搜索缓存文件只需 ~0.1秒，内存遍历需 10+ 秒
async fn rg_index_search(
    keyword_lc: &str,
    filter_type: &str,
    mapping: &HashMap<String, String>,
) -> Vec<InternalSearchResult> {
    let index_path = get_index_path();
    if !index_path.exists() {
        return Vec::new();
    }

    let strategy = SearchStrategy::from_type(filter_type);
    let mapped_keyword = mapping.get(keyword_lc).cloned();

    // 构建搜索词列表：原词 + 别名映射 + 分词
    let mut search_terms = vec![keyword_lc.to_string()];
    if let Some(ref en_name) = mapped_keyword {
        let en_lc = en_name.to_lowercase();
        if en_lc != keyword_lc {
            search_terms.push(en_lc);
        }
    }
    // 多词拆分搜索：如果输入 "photo shop"，同时搜 "photo" 和 "shop"
    let words: Vec<&str> = keyword_lc.split_whitespace().collect();
    if words.len() > 1 {
        for word in &words {
            if !search_terms.contains(&word.to_string()) {
                search_terms.push(word.to_string());
            }
        }
    }

    let use_rg = has_rg();
    let mut all_results = Vec::new();
    let mut seen_paths = HashSet::new();

    for term in &search_terms {
        let output = if use_rg {
            // rg 搜索缓存文件，-i 忽略大小写，--max-count 限制每词最多匹配
            AsyncCommand::new("rg")
                .arg("-i")
                .arg("--max-count")
                .arg("300")
                .arg("--no-filename")
                .arg("--color")
                .arg("never")
                .arg(term)
                .arg(&index_path)
                .output()
                .await
        } else {
            // 回退到 grep
            AsyncCommand::new("grep")
                .arg("-i")
                .arg("-m")
                .arg("300")
                .arg(term)
                .arg(&index_path)
                .output()
                .await
        };

        if let Ok(out) = output {
            if !out.status.success() {
                continue;
            }
            let content = String::from_utf8_lossy(&out.stdout);
            for line in content.lines() {
                let path = line.trim().to_string();
                if path.is_empty() {
                    continue;
                }

                // 去重（用 HashSet 替代 Vec::iter().any，O(1) vs O(n)）
                if !seen_paths.insert(path.clone()) {
                    continue;
                }

                // 过滤类型
                if filter_type != "all" {
                    if filter_type == "folder" {
                        let is_likely_dir = !path.contains('.') || path.ends_with(".app");
                        if !is_likely_dir {
                            continue;
                        }
                    } else if !strategy.matches_extension(&path) {
                        continue;
                    }
                }

                // 过滤垃圾路径
                if path.contains("/Contents/MacOS/") || path.contains("/Library/") {
                    continue;
                }

                let name = path.split('/').next_back().unwrap_or(&path).to_string();
                let name_lc = name.to_lowercase();
                let is_name_match = name_lc.contains(term.as_str());

                let score = if is_name_match { 100 } else { 50 };

                all_results.push(InternalSearchResult {
                    path,
                    name,
                    score,
                    source: if use_rg { "rg" } else { "grep" }.to_string(),
                });
            }
        }

        // 找够结果就不搜更多词了
        if all_results.len() >= 300 {
            break;
        }
    }

    all_results
}

/// 内存索引搜索（备用，仅在 rg/grep 不可用且缓存文件不存在时使用）
#[allow(dead_code)]
fn memory_index_search(
    keyword_lc: &str,
    filter_type: &str,
    index_files: &[String],
    mapping: &HashMap<String, String>,
) -> Vec<InternalSearchResult> {
    let mut results = Vec::new();
    let mut fallback_results = Vec::new();
    let strategy = SearchStrategy::from_type(filter_type);
    let mapped_keyword = mapping.get(keyword_lc).cloned();

    let volumes_exist: std::collections::HashSet<String> =
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            entries
                .flatten()
                .map(|e| e.path().to_string_lossy().to_string())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

    let words: Vec<&str> = keyword_lc.split_whitespace().collect();
    let max_results = 200;

    for path in index_files.iter() {
        if results.len() >= max_results {
            break;
        }

        if filter_type != "all" {
            if filter_type == "folder" {
                let is_likely_dir = !path.contains('.') || path.ends_with(".app");
                if !is_likely_dir {
                    continue;
                }
            } else if !strategy.matches_extension(path) {
                continue;
            }
        }

        if path.starts_with("/Volumes/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 3 {
                let vol_path = format!("/Volumes/{}", parts[2]);
                if !volumes_exist.contains(&vol_path) {
                    continue;
                }
            }
        }

        let name = path.split('/').next_back().unwrap_or(path).to_string();
        let name_lc = name.to_lowercase();

        let mut matched_count = 0;
        let mut name_only_match = true;
        for word in &words {
            if name_lc.contains(word) {
                matched_count += 1;
            } else {
                name_only_match = false;
                let path_lc = path.to_lowercase();
                if path_lc.contains(word) {
                    matched_count += 1;
                }
            }
        }

        if matched_count < words.len() {
            if let Some(en_name) = mapped_keyword.as_ref() {
                if name_lc.contains(en_name.to_lowercase().as_str()) {
                    matched_count = words.len();
                }
            }
            if matched_count < words.len() && keyword_lc.len() >= 2 {
                let initials: String = name_lc
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().next().unwrap_or(' '))
                    .collect();
                if initials.contains(keyword_lc) {
                    matched_count = words.len();
                }
            }
        }

        if matched_count == words.len() {
            let score = if name_only_match { 100 } else { 50 };
            results.push(InternalSearchResult {
                path: path.clone(),
                name,
                score,
                source: "memory".to_string(),
            });
        } else if matched_count > 0 && words.len() > 1 {
            fallback_results.push(InternalSearchResult {
                path: path.clone(),
                name,
                score: 0,
                source: "memory".to_string(),
            });
        }
    }

    if results.len() < 20 {
        results.extend(fallback_results.into_iter().take(50));
    }

    results
}

/// 判断是否为系统垃圾文件：`.` 开头隐藏文件（.DS_Store/.git/.config 等）
/// 与 `._` 开头 AppleDouble 资源分叉。人类模式永不进结果集。
fn is_system_cruft(name: &str, _path: &str) -> bool {
    name.starts_with('.')
}

/// 排序与去重
fn sort_and_dedup(
    results: Vec<InternalSearchResult>,
    keyword_lc: &str,
    filter_type: &str,
    click_history: &HashMap<String, u32>,
    mapping: &HashMap<String, String>,
    human_mode: bool,
) -> Vec<SearchResult> {
    let mapped_keyword = mapping.get(keyword_lc).cloned();
    let mut all_results = results;

    let mut seen = HashSet::new();
    all_results.retain(|r| seen.insert(r.path.clone()));

    // 人类模式：硬过滤系统垃圾（. 开头隐藏文件 / ._ AppleDouble）
    if human_mode {
        all_results.retain(|r| !is_system_cruft(&r.name, &r.path));
    }

    // 收集来源信息
    let sources: HashSet<String> = all_results.iter().map(|r| r.source.clone()).collect();
    let source_str = sources.into_iter().collect::<Vec<_>>().join("+");

    for res in all_results.iter_mut() {
        let name_lc = res.name.to_lowercase();
        let path_lc = res.path.to_lowercase();

        let mut base_score = 0;
        let words: Vec<&str> = keyword_lc.split_whitespace().collect();
        let mut all_in_name = words.iter().all(|w| name_lc.contains(w));
        let all_in_path = words.iter().all(|w| path_lc.contains(w));

        let mut is_alias_match = false;
        let mut is_acronym_match = false;

        if let Some(en_name) = mapped_keyword.as_ref() {
            if name_lc.contains(en_name) {
                all_in_name = true;
                is_alias_match = true;
            }
        }

        if !all_in_name && keyword_lc.len() >= 2 {
            let initials: String = name_lc
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().next().unwrap_or(' '))
                .collect();
            if initials.contains(keyword_lc) {
                all_in_name = true;
                is_acronym_match = true;
            }
        }

        if all_in_name {
            if is_alias_match || is_acronym_match || name_lc == keyword_lc {
                base_score += 20000;
            } else {
                let mut is_continuous = true;
                let mut last_pos = 0;
                for word in words.iter() {
                    if let Some(pos) = name_lc[last_pos..].find(word) {
                        last_pos += pos + word.len();
                    } else {
                        is_continuous = false;
                        break;
                    }
                }
                if is_continuous {
                    base_score += 10000;
                    if name_lc.starts_with(words[0]) {
                        base_score += 5000;
                    }
                } else {
                    base_score += 5000;
                }
            }
        } else if all_in_path {
            base_score += 2000;
        }

        if filter_type == "app" && (res.path.ends_with(".app") || res.path.ends_with(".app/")) {
            base_score += 10000;
        }

        if let Some(&clicks) = click_history.get(&res.path) {
            base_score += (clicks as i32) * 5000;
        }

        let depth = res.path.split('/').count() as i32;
        if res.path.contains(".app/Contents/") {
            base_score -= 10000;
        }
        if !res.path.starts_with("/Applications") {
            base_score -= depth * 50;
        }
        if res.path.starts_with("/Applications") {
            base_score += 5000;
        } else if res.path.contains("/Desktop") {
            base_score += 1000;
        }

        // 颜色配置文件降级：3dl/icc/csp/cube 等色彩查找表格式排后
        if let Some(ext) = res.path.rsplit('.').next_back().map(|e| e.to_lowercase()) {
            let color_exts = [
                "3dl", "icc", "csp", "cube", "lut", "dcp", "mga", "hdr", "exr",
            ];
            if color_exts.contains(&ext.as_str()) {
                base_score -= 8000;
            }
            if filter_type == "image" {
                let img_priority = [
                    "jpg", "jpeg", "png", "heic", "heif", "raw", "arw", "cr2", "nef", "dng",
                    "tiff", "tif", "webp", "gif", "bmp", "psd", "svg",
                ];
                if img_priority.contains(&ext.as_str()) {
                    base_score += 3000;
                }
            }
        }

        // 人类模式：代码/程序目录与代码类型排名降级（不隐藏，豁免精确/别名命中）
        if human_mode {
            let code_dirs = [
                "node_modules",
                ".git",
                "target",
                "build",
                "dist",
                "DerivedData",
                "usr",
                "System",
                "opt/homebrew",
                ".cargo",
                "Library/Caches",
            ];
            if code_dirs.iter().any(|d| res.path.contains(d)) {
                base_score -= 8000;
            }
            if let Some(ext) = res.path.rsplit('.').next_back() {
                let code_exts = [
                    "rs", "py", "js", "ts", "tsx", "go", "c", "h", "cpp", "java", "rb", "sh",
                    "toml", "json", "lock", "yaml", "yml",
                ];
                if code_exts.contains(&ext.to_lowercase().as_str()) {
                    base_score -= 5000;
                }
            }
            // 豁免：精确命中文件名或别名/缩写命中时不降级
            if name_lc == keyword_lc || is_alias_match || is_acronym_match {
                base_score += 13000;
            }
        }

        res.score = base_score;
    }

    all_results.sort_by(|a, b| b.score.cmp(&a.score));

    let take_n = if human_mode { 100 } else { 500 };
    all_results
        .into_iter()
        .take(take_n)
        .map(|r| SearchResult {
            path: r.path,
            name: r.name,
            score: if r.score != 0 { Some(r.score) } else { None },
            elapsed_ms: None, // 由调用方填充
            source: Some(source_str.clone()),
        })
        .collect()
}

/// 获取最近文件（搜索框空时使用）
/// 用 mdfind 按 Spotlight 排序取 Desktop/Downloads/Documents 中的最近文件，毫秒级
pub async fn recent_files(filter_type: &str, max_results: usize) -> Vec<SearchResult> {
    let start = std::time::Instant::now();
    let kind_query = match filter_type {
        "image" => "kMDItemContentTypeTree == 'public.image'",
        "video" => "kMDItemContentTypeTree == 'public.movie'",
        "doc" => "(kMDItemContentTypeTree == 'public.text' || kMDItemContentTypeTree == 'com.microsoft.word.doc' || kMDItemContentTypeTree == 'com.adobe.pdf')",
        "app" => "kMDItemKind == 'Application'",
        "folder" => "kMDItemContentType == 'public.folder'",
        _ => "",
    };
    let query = if kind_query.is_empty() {
        "kMDItemContentTypeTree == 'public.item'".to_string()
    } else {
        kind_query.to_string()
    };
    let mut cmd = AsyncCommand::new("mdfind");
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
    for dir in &["Desktop", "Downloads", "Documents"] {
        cmd.arg("-onlyin").arg(format!("{}/{}", home, dir));
    }
    if filter_type == "app" {
        cmd.arg("-onlyin").arg("/Applications");
    }
    cmd.arg(&query);
    let output = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output()).await;
    let mut results = Vec::new();
    if let Ok(Ok(o)) = output {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let path = line.trim().to_string();
            if path.is_empty() || path.contains("/.Trash/") || path.contains("/Library/Caches/") {
                continue;
            }
            let name = path.split('/').next_back().unwrap_or(&path).to_string();
            if name.starts_with('.') || name.starts_with("._") {
                continue;
            }
            results.push(SearchResult {
                path,
                name,
                score: Some(0),
                elapsed_ms: None,
                source: Some("spotlight".to_string()),
            });
            if results.len() >= max_results {
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_millis();
    if elapsed > 50 {
        eprintln!(
            "[sts] 最近文件: {}ms, {} 条 (filter={})",
            elapsed,
            results.len(),
            filter_type
        );
    }
    results
}

/// 主搜索入口（文件名搜索）
/// 自动计时，快速搜索（<100ms）不返回耗时信息
pub async fn search_files(
    keyword: &str,
    filter_type: &str,
    _index: &GlobalIndex,
    click_history: &HashMap<String, u32>,
    mapping: &HashMap<String, String>,
    _human_mode: bool,
) -> Vec<SearchResult> {
    let keyword_lc = keyword.to_lowercase();
    if keyword_lc.trim().is_empty() {
        return Vec::new();
    }

    let start = std::time::Instant::now();

    // 智能搜索策略：根据缓存是否存在选择引擎
    let cache_exists = get_index_path().exists();
    let rg_ok = has_rg();

    // 模糊扩展查询词（别名/缩写/编辑距离/前缀），扩展召回
    let expanded: Vec<String> = {
        let g = _index.fuzzy.lock().unwrap();
        match &*g {
            Some(fm) => fm.expand_query(&keyword_lc),
            None => vec![keyword_lc.clone()],
        }
    };

    let mut all_results: Vec<InternalSearchResult> = Vec::new();
    for kw in &expanded {
        if cache_exists && (rg_ok || which_grep()) {
            let (rg_res, spotlight_res) = tokio::join!(
                rg_index_search(kw, filter_type, mapping),
                spotlight_search(kw, filter_type, mapping),
            );
            all_results.extend(rg_res);
            all_results.extend(spotlight_res);
        } else {
            eprintln!("[sts] 索引缓存不存在，仅使用 Spotlight 搜索");
            all_results.extend(spotlight_search(kw, filter_type, mapping).await);
        }
    }

    // BM25 增强层（中文分词匹配，直接搜原词）
    {
        let g = _index.bm25.lock().unwrap();
        if let Some(bm25) = &*g {
            let bm25_res = bm25.search(&keyword_lc, filter_type, 50);
            all_results.extend(bm25_res);
        }
    }

    let mut results = sort_and_dedup(
        all_results,
        &keyword_lc,
        filter_type,
        click_history,
        mapping,
        _human_mode,
    );

    // 智能计时：仅慢查询（>100ms）才标注耗时
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if elapsed_ms > 100 {
        for r in results.iter_mut() {
            r.elapsed_ms = Some(elapsed_ms);
        }
    }

    results
}

/// 检查 grep 是否可用（回退用）
fn which_grep() -> bool {
    std::process::Command::new("grep")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 内容搜索入口（rg / grep）
/// 默认搜索 Desktop/Documents/Downloads（避免全 HOME 目录慢搜）
/// 5 秒超时保护，防止在巨大目录上卡死
pub async fn search_content(
    params: ContentSearchParams,
) -> Result<Vec<ContentSearchResult>, String> {
    let keyword = params.keyword.trim().to_string();
    if keyword.is_empty() {
        return Ok(Vec::new());
    }

    // 默认搜索路径：仅 Desktop + Downloads（Documents 太大不适合实时搜索）
    // 用户可通过 -p 指定特定目录搜索
    let search_path = if params.path.is_empty() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        vec![format!("{}/Desktop", home), format!("{}/Downloads", home)]
    } else {
        vec![params.path.clone()]
    };

    // 过滤掉不存在的路径
    let search_paths: Vec<String> = search_path
        .into_iter()
        .filter(|p| std::path::Path::new(p).exists())
        .collect();

    if search_paths.is_empty() {
        return Err("搜索路径不存在".to_string());
    }

    let max_results = params.max_results;
    let use_rg = has_rg();
    let start_time = std::time::Instant::now();

    // 对每个路径并行搜索，5 秒超时
    let mut tasks = Vec::new();
    for path in &search_paths {
        let keyword_clone = keyword.clone();
        let filter_type_clone = params.filter_type.clone();
        let path_clone = path.clone();

        let task = tokio::spawn(async move {
            let output = if use_rg {
                let mut cmd = AsyncCommand::new("rg");
                cmd.arg("--line-number")
                    .arg("--no-heading")
                    .arg("--with-filename")
                    .arg("--color")
                    .arg("never")
                    .arg("--max-count")
                    .arg("10")
                    .arg("--max-depth")
                    .arg("20")
                    .arg("--hidden")
                    // 逐个排除目录（rg 不支持花括号展开）
                    .arg("--glob")
                    .arg("!.git")
                    .arg("--glob")
                    .arg("!node_modules")
                    .arg("--glob")
                    .arg("!Library")
                    .arg("--glob")
                    .arg("!Contents/MacOS")
                    .arg("--glob")
                    .arg("!*.app/Contents")
                    .arg("--glob")
                    .arg("!.cache")
                    .arg("--glob")
                    .arg("!.Trash")
                    .arg("--glob")
                    .arg("!target")
                    .arg("--glob")
                    .arg("!__pycache__")
                    .arg("--glob")
                    .arg("!*.pyc")
                    .arg("--glob")
                    .arg("!.venv")
                    .arg("--glob")
                    .arg("!venv")
                    .arg("--glob")
                    .arg("!.cargo")
                    .arg("--glob")
                    .arg("!.rustup")
                    .arg("--glob")
                    .arg("!.nvm")
                    .arg("--glob")
                    .arg("!miniforge3")
                    .arg("--glob")
                    .arg("!anaconda3")
                    .arg("--glob")
                    .arg("!*.min.js")
                    .arg("--glob")
                    .arg("!*.min.css")
                    .arg("--glob")
                    .arg("!*.map")
                    .arg("--encoding")
                    .arg("auto");

                if !filter_type_clone.is_empty() && filter_type_clone != "all" {
                    let strategy = SearchStrategy::from_type(&filter_type_clone);
                    if !strategy.extensions.is_empty() {
                        for ext in &strategy.extensions {
                            cmd.arg("--glob").arg(format!("*{}", ext));
                        }
                    }
                }

                cmd.arg(&keyword_clone).arg(&path_clone).output().await
            } else {
                let mut cmd = AsyncCommand::new("grep");
                cmd.arg("-rn")
                    .arg("--binary-files=without-match")
                    .arg("--exclude-dir=.git")
                    .arg("--exclude-dir=node_modules")
                    .arg("--exclude-dir=Library")
                    .arg("--exclude-dir=target")
                    .arg("--exclude-dir=.cache")
                    .arg("--exclude-dir=__pycache__")
                    .arg("--exclude-dir=.venv")
                    .arg("--exclude-dir=venv");

                if !filter_type_clone.is_empty() && filter_type_clone != "all" {
                    let strategy = SearchStrategy::from_type(&filter_type_clone);
                    if !strategy.extensions.is_empty() {
                        let include_patterns: Vec<String> = strategy
                            .extensions
                            .iter()
                            .map(|e| format!("*{}", e))
                            .collect();
                        cmd.arg("--include").arg(include_patterns.join(","));
                    }
                }

                cmd.arg(&keyword_clone).arg(&path_clone).output().await
            };

            match output {
                Ok(out) => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
                Err(e) => Err(e.to_string()),
            }
        });

        // 8 秒超时保护
        let timed = tokio::time::timeout(std::time::Duration::from_secs(8), task);
        tasks.push(timed);
    }

    let task_results = futures::future::join_all(tasks).await;
    let elapsed = start_time.elapsed();

    let mut results = Vec::new();
    for result in task_results {
        let content = match result {
            Ok(Ok(Ok(c))) => c,
            Ok(Ok(Err(e))) => {
                eprintln!("[sts] 内容搜索部分路径失败: {}", e);
                continue;
            }
            Ok(Err(e)) => {
                eprintln!("[sts] 内容搜索任务失败: {}", e);
                continue;
            }
            Err(_) => {
                eprintln!("[sts] 内容搜索超时（5秒）");
                continue;
            }
        };

        for line in content.lines() {
            if results.len() >= max_results {
                break;
            }

            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() >= 3 {
                let path = parts[0].to_string();
                let line_number = parts[1].parse::<u64>().unwrap_or(0);
                let line_content = parts[2].trim().to_string();

                if path.contains("/Contents/MacOS/")
                    || path.contains("/Library/")
                    || path.contains("/.git/")
                {
                    continue;
                }

                let name = path.split('/').next_back().unwrap_or(&path).to_string();
                results.push(ContentSearchResult {
                    path,
                    name,
                    line_number,
                    line_content,
                });
            }
        }
    }

    let elapsed_ms = elapsed.as_millis();
    if elapsed_ms > 100 {
        eprintln!(
            "[sts] 内容搜索完成: {}ms, {} 条结果 (使用 {})",
            elapsed_ms,
            results.len(),
            if use_rg { "rg" } else { "grep" }
        );
    } else {
        eprintln!(
            "[sts] 内容搜索完成: {} 条结果 (使用 {})",
            results.len(),
            if use_rg { "rg" } else { "grep" }
        );
    }

    Ok(results)
}
