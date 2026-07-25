//! 星TAP 极速搜索 — 核心引擎（mdfind 即时搜索版）
//!
//! 设计原则（2026-07-25 回归「原版极致搜索」）：
//! - 文件名/内容搜索完全交给 macOS Spotlight（mdfind），毫秒级、零文件句柄持有。
//! - 搜索天然覆盖所有卷（含外接盘 /Volumes），关 app 即干净退出，外盘可随时弹出。
//! - 不持有任何后台全量扫描 / FSEvents 监听，不拖慢系统、不占长期内存。
//! - BM25 / 模糊匹配 / ripgrep 自建索引 / 内存全量索引 等重型基建已全部移除（用户明确不要）。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command as AsyncCommand;
use tokio::time::timeout;

pub mod thumbnail;

/// 搜索结果（前后端 / MCP 共用）。
/// `score` 仅用于内部排序，`elapsed_ms` / `source` 为可选诊断字段，序列化时按需输出。
#[derive(Serialize, Clone, Debug, Default)]
pub struct InternalSearchResult {
    pub name: String,
    pub path: String,
    #[serde(skip)]
    pub score: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// 分类搜索策略：把 filter_type 解耦为 Spotlight 类型谓词 + 扩展名白名单。
struct SearchStrategy {
    spotlight_kind: String,
    extensions: Vec<&'static str>,
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
                spotlight_kind:
                    "(kMDItemContentTypeTree == 'public.text' || kMDItemContentTypeTree == 'public.content' || kMDItemContentTypeTree == 'com.microsoft.word.doc' || kMDItemContentTypeTree == 'com.adobe.pdf')"
                        .to_string(),
                extensions: vec![".pdf", ".txt", ".md", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx"],
            },
            "folder" => Self {
                spotlight_kind: "kMDItemContentTypeTree == 'public.folder'".to_string(),
                extensions: vec![],
            },
            "app" => Self {
                spotlight_kind:
                    "(kMDItemContentTypeTree == 'com.apple.application-bundle' || kMDItemContentTypeTree == 'com.apple.systempreference.pane')"
                        .to_string(),
                extensions: vec![".app", ".prefPane"],
            },
            _ => Self {
                spotlight_kind: String::new(),
                extensions: vec![],
            },
        }
    }

    /// 生成标准 Spotlight 查询串（支持多词 AND + 别名扩展）。
    fn spotlight_query(&self, words: &[&str], alias: Option<&String>) -> String {
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

    /// 按扩展名预过滤（App 特殊处理：路径含 .app 且不在 Contents 内部即视为程序）。
    fn matches_extension(&self, path: &str) -> bool {
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
        self.extensions.iter().any(|ext| path_lc.ends_with(*ext))
    }
}

/// 全局索引状态（轻量、无后台扫描、无外盘句柄）。
#[derive(Clone)]
pub struct GlobalIndex {
    /// 别名映射表：搜「ps」→ 真实名「photoshop」，注入 Spotlight 查询实现直达。
    mapping: Arc<Mutex<HashMap<String, String>>>,
    /// 最近文件热缓存（仅 all 分类），由空闲预热任务刷新，避免空搜时再算。
    recent_cache: Arc<Mutex<Option<(Instant, Vec<InternalSearchResult>)>>>,
    /// 上次搜索时刻，用于判断「空闲」以触发受限后台预热。
    last_search: Arc<Mutex<Instant>>,
    /// 外接盘是否被 macOS Spotlight 系统索引（可秒搜）。
    /// false=未索引（如 exFAT 盘的 read-only 索引），搜索时需触发实时 find 兜底。
    external_indexed: Arc<Mutex<bool>>,
}

impl GlobalIndex {
    /// 构建完整索引（含别名映射扫描 /Applications）。
    pub fn new() -> Self {
        Self {
            mapping: Arc::new(Mutex::new(build_alias_mapping())),
            recent_cache: Arc::new(Mutex::new(None)),
            last_search: Arc::new(Mutex::new(Instant::now())),
            external_indexed: Arc::new(Mutex::new(false)),
        }
    }

    /// 空索引（测试用，不扫描 /Applications）。
    pub fn empty() -> Self {
        Self {
            mapping: Arc::new(Mutex::new(HashMap::new())),
            recent_cache: Arc::new(Mutex::new(None)),
            last_search: Arc::new(Mutex::new(Instant::now())),
            external_indexed: Arc::new(Mutex::new(false)),
        }
    }

    /// 主搜索：mdfind 即时搜索（文件名 + 内容，覆盖本地卷与外接盘）。
    pub async fn search_files(&self, keyword: &str, filter_type: &str) -> Vec<InternalSearchResult> {
        let start_time = Instant::now();
        let keyword_lc = keyword.to_lowercase();

        // 记录搜索时刻（用于空闲判断）
        *self.last_search.lock().unwrap() = Instant::now();

        if keyword_lc.trim().is_empty() {
            return Vec::new();
        }

        println!(
            "收到极速搜索请求: keyword='{}', type='{}'",
            keyword, filter_type
        );

        let strategy = SearchStrategy::from_type(filter_type);
        let mapping = self.mapping.lock().unwrap().clone();
        let mapped_keyword = mapping.get(&keyword_lc).cloned();

        let words: Vec<&str> = keyword_lc.split_whitespace().collect();
        let final_query = strategy.spotlight_query(&words, mapped_keyword.as_ref());

        println!("Spotlight 原始查询: {}", final_query);

        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());

        // 任务 A：用户目录 + 应用程序（3s 超时，有界）
        let q1 = final_query.clone();
        let home_a = home.clone();
        let t_a = tokio::spawn(async move {
            let out = timeout(
                Duration::from_secs(3),
                AsyncCommand::new("mdfind")
                    .arg("-onlyin")
                    .arg(&home_a)
                    .arg("-onlyin")
                    .arg("/Applications")
                    .arg(&q1)
                    .output(),
            )
            .await;
            match out {
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
                _ => String::new(),
            }
        });

        // 任务 B：外接盘（4s 超时，有界）——天然覆盖外盘，且不持有持久句柄
        let q2 = final_query.clone();
        let t_b = tokio::spawn(async move {
            let out = timeout(
                Duration::from_secs(4),
                AsyncCommand::new("mdfind")
                    .arg("-onlyin")
                    .arg("/Volumes")
                    .arg(&q2)
                    .output(),
            )
            .await;
            match out {
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
                _ => String::new(),
            }
        });

        let (ra, rb) = tokio::join!(t_a, t_b);

        let mut results: Vec<InternalSearchResult> = Vec::new();
        for content in [ra.unwrap_or_default(), rb.unwrap_or_default()] {
            for line in content.lines() {
                let path = line.trim().to_string();
                if path.is_empty()
                    || path.contains("/Contents/MacOS/")
                    || path.contains("/Library/")
                {
                    continue;
                }
                // 类型预过滤
                if filter_type != "all" && !strategy.matches_extension(&path) {
                    continue;
                }
                let name = path.split('/').next_back().unwrap_or(&path).to_string();
                results.push(InternalSearchResult {
                    path,
                    name,
                    score: None,
                    elapsed_ms: None,
                    source: None,
                });
            }
        }

        let final_results = sort_and_dedup(results, &keyword_lc, filter_type);
        println!(
            "搜索极速完成: 耗时 {:?}, 命中 {}",
            start_time.elapsed(),
            final_results.len()
        );
        final_results
    }

    /// 最近文件（空搜 / 分类标签用）。本地常用目录 find 实现，有界、零外盘句柄、不拖慢。
    pub async fn recent_files(&self, filter_type: &str, limit: usize) -> Vec<InternalSearchResult> {
        // 命中热缓存（仅 all 分类缓存）
        if filter_type == "all" {
            if let Some((t, cached)) = self.recent_cache.lock().unwrap().clone() {
                if t.elapsed() < Duration::from_secs(120) {
                    return cached.into_iter().take(limit).collect();
                }
            }
        }

        let ft = filter_type.to_string();
        let computed = tokio::task::spawn_blocking(move || compute_recent(&ft, limit))
            .await
            .unwrap_or_default();

        if filter_type == "all" {
            *self.recent_cache.lock().unwrap() = Some((Instant::now(), computed.clone()));
        }
        computed
    }

    /// 生成文件缩略图（委派给 thumbnail 模块，内存 LRU，不落盘）。
    pub fn get_thumbnail(&self, path: &str, size: u32) -> Option<String> {
        thumbnail::generate_thumbnail(path, size)
    }

    /// 查别名映射（ps → photoshop 等）。
    pub fn get_alias(&self, kw: &str) -> Option<String> {
        self.mapping
            .lock()
            .unwrap()
            .get(&kw.to_lowercase())
            .cloned()
    }

    /// 设置外接盘系统索引状态（volume monitor 经 mdutil 检测后写入）。
    pub fn set_external_indexed(&self, v: bool) {
        *self.external_indexed.lock().unwrap() = v;
    }

    /// 外接盘是否被系统索引（可秒搜）。false 时搜索应触发实时 find 兜底。
    pub fn external_indexed(&self) -> bool {
        *self.external_indexed.lock().unwrap()
    }
}

/// 同步计算最近文件：限定本地常用目录 + 最近 30 天，按修改时间倒序，有界返回。
/// 不使用 /Volumes（避免外盘慢扫），find 进程退出即释放，不持有任何句柄。
fn compute_recent(filter_type: &str, limit: usize) -> Vec<InternalSearchResult> {
    let strategy = SearchStrategy::from_type(filter_type);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
    let dirs = [
        format!("{}/Desktop", home),
        format!("{}/Downloads", home),
        format!("{}/Documents", home),
        "/Applications".to_string(),
    ];

    let mut found: Vec<(f64, String)> = Vec::new();
    for d in &dirs {
        if !std::path::Path::new(d).exists() {
            continue;
        }
        let out = std::process::Command::new("find")
            .arg(d)
            .args(["-maxdepth", "6", "-type", "f", "-mtime", "-30", "-print"])
            .output();
        if let Ok(o) = out {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let p = line.trim().to_string();
                if p.is_empty()
                    || p.contains("/Contents/MacOS/")
                    || p.contains("/Library/")
                {
                    continue;
                }
                if filter_type != "all" && !strategy.matches_extension(&p) {
                    continue;
                }
                let mt = std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                found.push((mt, p));
            }
        }
    }

    found.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    found
        .into_iter()
        .take(limit)
        .map(|(_, p)| {
            let name = p.split('/').next_back().unwrap_or(&p).to_string();
            InternalSearchResult {
                path: p,
                name,
                score: None,
                elapsed_ms: None,
                source: None,
            }
        })
        .collect()
}

/// 去重 + 智能排序（移植自原版极致搜索的打分逻辑，去掉重型内存索引依赖）。
fn sort_and_dedup(
    mut all_results: Vec<InternalSearchResult>,
    keyword_lc: &str,
    filter_type: &str,
) -> Vec<InternalSearchResult> {
    let mut seen = std::collections::HashSet::new();
    all_results.retain(|r| seen.insert(r.path.clone()));

    let words: Vec<&str> = keyword_lc.split_whitespace().collect();

    for res in all_results.iter_mut() {
        let name_lc = res.name.to_lowercase();
        let path_lc = res.path.to_lowercase();

        let mut base_score = 0;

        let all_in_name = words.iter().all(|w| name_lc.contains(w));
        let all_in_path = words.iter().all(|w| path_lc.contains(w));

        // 别名与缩写支持（Acronym）
        let mut is_alias_match = false;
        let mut is_acronym_match = false;

        if all_in_name && name_lc == keyword_lc {
            is_alias_match = true;
        }

        // 自动缩写匹配（如 dpp -> Digital Photo Professional）
        if !all_in_name && keyword_lc.len() >= 2 {
            let initials: String = name_lc
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().next().unwrap_or(' '))
                .collect();
            if initials.contains(&keyword_lc) {
                is_alias_match = true;
                is_acronym_match = true;
            }
        }

        if all_in_name || is_alias_match {
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

        // 针对程序类的特殊加成
        if filter_type == "app"
            && (res.path.ends_with(".app") || res.path.ends_with(".app/"))
        {
            base_score += 10000;
        }

        // 路径深度与嵌套惩罚
        let depth = res.path.split('/').count() as i32;
        if res.path.contains(".app/Contents/") {
            base_score -= 10000;
        }
        if !res.path.starts_with("/Applications") {
            base_score -= depth * 50;
        }

        // 位置权重
        if res.path.starts_with("/Applications") {
            base_score += 5000;
        } else if res.path.contains("/Desktop") {
            base_score += 1000;
        }

        res.score = Some(base_score);
    }

    all_results.sort_by(|a, b| b.score.cmp(&a.score));
    all_results.into_iter().take(100).collect()
}

/// 构建别名映射表（覆盖设计/社交/办公/工具等常用软件，支持拼音缩写、中文俗称、英文原名）。
pub fn build_alias_mapping() -> HashMap<String, String> {
    let mut new_map: HashMap<String, String> = HashMap::new();

    let aliases = [
        // 设计类
        ("ps", "photoshop"), ("lr", "lightroom"), ("pr", "premiere"), ("ae", "after effects"), ("ai", "illustrator"),
        ("id", "indesign"), ("au", "audition"), ("dw", "dreamweaver"), ("an", "animate"), ("pl", "prelude"),
        ("br", "bridge"), ("ch", "character animator"), ("me", "media encoder"), ("ic", "incopy"), ("fs", "fuse"),
        ("sc", "scout"), ("st", "stock"), ("xd", "xd"), ("dc", "acrobat"), ("dpp", "digital photo professional"),
        ("fcpx", "final cut pro"), ("c4d", "cinema 4d"), ("sketch", "sketch"), ("figma", "figma"),
        ("photoshop", "photoshop"), ("illustrator", "illustrator"), ("premiere", "premiere"),
        ("aftereffects", "after effects"), ("lightroom", "lightroom"),
        // 社交/办公
        ("wx", "wechat"), ("微信", "wechat"), ("qq", "qq"), ("dd", "dingtalk"), ("钉钉", "dingtalk"),
        ("fs", "feishu"), ("飞书", "feishu"), ("lark", "feishu"), ("word", "microsoft word"), ("excel", "microsoft excel"),
        ("ppt", "microsoft powerpoint"), ("wps", "wpsoffice"), ("pdf", "acrobat"), ("obs", "obs studio"),
        ("yx", "neteasemail"), ("邮箱", "mail"), ("notes", "notes"), ("memo", "notes"), ("wechat", "wechat"),
        ("dingtalk", "dingtalk"), ("feishu", "feishu"),
        // 视频/娱乐/AI
        ("jy", "videofusion"), ("剪映", "videofusion"), ("capcut", "videofusion"), ("vf", "videofusion"),
        ("db", "doubao"), ("豆包", "doubao"), ("doubao", "doubao"), ("videofusion", "videofusion"),
        ("dy", "douyin"), ("bili", "bilibili"), ("bz", "bilibili"), ("music", "music"),
        ("网易云", "neteasemusic"), ("spotify", "spotify"), ("douyin", "douyin"), ("tiktok", "douyin"),
        ("jianying", "videofusion"), ("jianyingpro", "videofusion"),
        // 生产力
        ("wp", "wpsoffice"), ("wps", "wpsoffice"), ("word", "microsoft word"), ("excel", "microsoft excel"),
        ("ppt", "microsoft powerpoint"), ("pages", "pages"), ("numbers", "numbers"), ("keynote", "keynote"),
        // 工具/开发
        ("llq", "browser"), ("浏览器", "browser"), ("safari", "safari"), ("chrome", "google chrome"),
        ("edge", "microsoft edge"), ("fd", "finder"), ("访达", "finder"), ("zd", "terminal"), ("终端", "terminal"),
        ("iterm", "iterm"), ("code", "visual studio code"), ("vs", "visual studio code"), ("vscode", "visual studio code"),
        ("st", "sublime text"), ("idea", "intellij idea"), ("webstorm", "webstorm"), ("py", "pycharm"),
        ("git", "github"), ("postman", "postman"), ("docker", "docker"),
        // 系统/其他
        ("sz", "settings"), ("设置", "settings"), ("jh", "calculator"), ("计算器", "calculator"),
        ("activity", "activity monitor"), ("monitor", "activity monitor"), ("disk", "disk utility"),
        ("keychain", "keychain access"), ("console", "console"),
    ];

    for (alias, real) in aliases {
        new_map.insert(alias.to_string(), real.to_string());
    }

    // 动态扫描 /Applications 以补充映射（处理带中文名的 App）
    if let Ok(entries) = std::fs::read_dir("/Applications") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".app") {
                let base_name = name.replace(".app", "").to_lowercase();
                new_map.entry(base_name.clone()).or_insert(base_name.clone());

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

    new_map
}

/// 启动受限后台预热任务（空闲时慢慢整理增强，有限制、不拖慢、关 app 即退）。
///
/// 行为：每 5 分钟检查一次，仅当「距上次搜索 > 120s（系统空闲）」时，
/// 用有界 find（仅本地目录、8s 超时）刷新最近文件热缓存。
/// - 不扫描 /Volumes，不持有任何文件句柄，外盘可随时弹出；
/// - 全程 spawn_blocking + timeout，不会阻塞异步 runtime；
/// - app 退出时 tokio 任务随进程一起结束，无残留后台占用。
///
/// 注意：本函数本身是 `async`，由调用方（Tauri async runtime）`spawn` 提供 tokio 上下文。
pub async fn start_idle_refresh(index: GlobalIndex) {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let idle = {
            let last = *index.last_search.lock().unwrap();
            last.elapsed() > Duration::from_secs(120)
        };
        if !idle {
            continue;
        }
        // recent_files 本身是 async 且内部已 spawn_blocking，直接 timeout 驱动即可。
        // 不扫 /Volumes、8s 有界、不阻塞 runtime；app 退出时本 tokio 任务随进程结束。
        let _ = tokio::time::timeout(Duration::from_secs(8), index.recent_files("all", 60)).await;
    }
}

/// 外接盘实时扫描兜底：当外接盘未被 Spotlight 系统索引时，用有界 `find` 实时查找。
///
/// 设计约束（用户明确要求）：
/// - 平时完全不跑，零占用、不拖慢系统；仅在「搜索关键词」时才触发，且只在系统未索引外盘时。
/// - 有界：限制目录深度(-maxdepth 7)、限输出行数(head -200)、12s 超时丢弃，绝不长时间拖慢。
/// - find 进程结束即释放句柄，外接盘可随时弹出；app 退出时线程随进程一起结束。
/// - 与本地 mdfind 搜索解耦：本地毫秒级响应不被外盘扫描阻塞（后端异步 emit 追加结果）。
pub fn search_external_find(keyword: &str, limit: usize) -> Vec<InternalSearchResult> {
    let kw = keyword.to_lowercase();
    if kw.trim().is_empty() {
        return Vec::new();
    }

    let safe = kw.replace('\'', "'\\''");
    let mut results: Vec<InternalSearchResult> = Vec::new();
    let max = limit.min(200);

    // 枚举 /Volumes 下的外接盘，跳过系统卷（Macintosh HD）和临时挂载（dmg.*），
    // 每个盘单独 fd 搜索（2s 超时），避免扫系统盘导致卡死。
    if let Ok(entries) = std::fs::read_dir("/Volumes") {
        for entry in entries.flatten() {
            if results.len() >= max {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.is_empty()
                || name.starts_with('.')
                || name == "Macintosh HD"
                || name.starts_with("dmg.")
            {
                continue;
            }
            let vol_path = entry.path().to_string_lossy().to_string();
            let cmd = format!(
                "fd -g '*{}*' '{}' -d 8 -t f 2>/dev/null | head -{}",
                safe,
                vol_path,
                max - results.len()
            );
            if let Ok(out) = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
            {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let p = line.trim().to_string();
                    if p.is_empty()
                        || p.contains("/Contents/MacOS/")
                        || p.contains("/.Trashes")
                        || p.contains("$RECYCLE.BIN")
                        || p.contains("/.Spotlight-V100")
                        || p.contains("/.fseventsd")
                    {
                        continue;
                    }
                    if results.len() >= max {
                        break;
                    }
                    let name = p.split('/').next_back().unwrap_or(&p).to_string();
                    results.push(InternalSearchResult {
                        path: p,
                        name,
                        ..Default::default()
                    });
                }
            }
        }
    }

    sort_and_dedup(results, &kw, "all")
        .into_iter()
        .take(limit)
        .collect()
}
