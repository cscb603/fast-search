use serde::Serialize;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
  use tauri::{State, AppHandle, Manager};
  use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, Modifiers, Code};
  use tauri_plugin_cli::CliExt;
  use tokio::time::{sleep, Duration};
use tokio::process::Command as AsyncCommand;

#[derive(Serialize, Clone)]
 struct SearchResult {
    path: String,
    name: String,
    // 内部字段，用于排序优化
    #[serde(skip)]
    score: i32,
}

// 全局索引状态
#[derive(Clone)]
struct GlobalIndex {
    files: Arc<Mutex<Vec<String>>>,
    is_indexing: Arc<Mutex<bool>>,
    force_update: Arc<AtomicBool>,
}

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// 获取索引文件路径
fn get_index_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let path = PathBuf::from(home).join("Library/Caches/com.xtap.search/index.cache");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

impl GlobalIndex {
    fn new() -> Self {
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
                println!("从缓存加载了 {} 条索引", loaded_files.len());
                let mut guard = files.lock().unwrap();
                *guard = loaded_files;
            }
        }

        Self { files, is_indexing, force_update }
    }

    fn start_indexing(&self) {
        let files_clone = self.files.clone();
        let status_clone = self.is_indexing.clone();
        let force_update_clone = self.force_update.clone();
        tauri::async_runtime::spawn(async move {
            let mut last_volumes = std::collections::HashSet::new();
            let mut last_full_scan = std::time::Instant::now();
            
            loop {
                // 检查外接盘是否有变化
                let mut current_volumes = std::collections::HashSet::new();
                if let Ok(entries) = std::fs::read_dir("/Volumes") {
                    for entry in entries.flatten() {
                        current_volumes.insert(entry.path().to_string_lossy().to_string());
                    }
                }

                let volumes_changed = current_volumes != last_volumes;
                let time_to_update = last_full_scan.elapsed() > Duration::from_secs(600);
                let force_now = force_update_clone.load(Ordering::Relaxed);
                
                if volumes_changed || time_to_update || force_now {
                    println!("开始更新索引 (原因: {})...", 
                        if force_now { "手动触发" } else if volumes_changed { "磁盘变化" } else { "定期更新" });
                    
                    force_update_clone.store(false, Ordering::Relaxed);
                    last_volumes = current_volumes;
                    last_full_scan = std::time::Instant::now();
                    
                    {
                        let mut guard = status_clone.lock().unwrap();
                        *guard = true;
                    }
                    
                    // 扫描路径：本地常用 + 外接盘 + 应用程序
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
                    for path in scan_paths {
                        if !std::path::Path::new(&path).exists() { continue; }
                        println!("正在扫描路径: {} ...", path);
                        
                        let output = AsyncCommand::new("find")
                            .arg(&path)
                            .args(["(", "-path", "*/node_modules/*", "-o", "-path", "*/.git/*", "-o", "-path", "*/Library/*", "-o", "-path", "*/Contents/MacOS/*", "-o", "-name", ".*", ")", "-prune", "-o", "-print"])
                            .output()
                            .await;

                        if let Ok(out) = output {
                            let content = String::from_utf8_lossy(&out.stdout);
                            let mut count = 0;
                            for line in content.lines() {
                                let p = line.to_string();
                                if p != path && !p.is_empty() {
                                    all_files.push(p);
                                    count += 1;
                                }
                            }
                            println!("路径 {} 扫描完成，找到 {} 个文件", path, count);
                        }
                    }
                    
                    // 保存到缓存文件
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
                    println!("索引更新完成，共 {} 条数据，已持久化到本地", count);
                }
                
                // 每 30 秒检查一次外接盘状态，如果没有变化且距离上次更新超过 10 分钟，也更新一次
                sleep(Duration::from_secs(30)).await;
            }
        });
    }
}

#[tauri::command]
fn get_indexing_status(state: State<'_, AppCache>) -> bool {
    *state.index.is_indexing.lock().unwrap()
}

// 应用缓存
#[derive(Clone)]
struct AppCache {
    mapping: Arc<Mutex<HashMap<String, String>>>,
    click_history: Arc<Mutex<HashMap<String, u32>>>, // 新增：点击历史记录 (路径 -> 点击次数)
    index: GlobalIndex,
}

impl AppCache {
    fn new() -> Self {
        let cache = Self {
            mapping: Arc::new(Mutex::new(HashMap::new())),
            click_history: Arc::new(Mutex::new(HashMap::new())),
            index: GlobalIndex::new(),
        };
        cache.load_click_history(); // 启动时加载历史
        cache.update();
        cache
    }

    // 从磁盘加载点击历史
    fn load_click_history(&self) {
        if let Some(mut path) = dirs::cache_dir() {
            path.push("com.xtap.search");
            path.push("click_history.json");
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(history) = serde_json::from_str::<HashMap<String, u32>>(&content) {
                    let mut mine = self.click_history.lock().unwrap();
                    *mine = history;
                    println!("从缓存加载了 {} 条点击历史", mine.len());
                }
            }
        }
    }

    // 保存点击历史到磁盘
    fn save_click_history(&self) {
        if let Some(mut path) = dirs::cache_dir() {
            path.push("com.xtap.search");
            let _ = std::fs::create_dir_all(&path);
            path.push("click_history.json");
            let mine = self.click_history.lock().unwrap();
            if let Ok(content) = serde_json::to_string(&*mine) {
                let _ = std::fs::write(path, content);
            }
        }
    }

    fn update(&self) {
        let mut new_map = HashMap::new();
        // 1. 核心工业级映射表 - 覆盖设计、社交、工具、办公等常用软件
        // 支持拼音缩写、中文俗称、英文原名
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
            ("db", "douban"), ("dy", "douyin"), ("bili", "bilibili"), ("bz", "bilibili"), ("music", "music"), 
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

        // 2. 动态扫描 /Applications 以补充映射 (处理带中文名的 App)
        if let Ok(entries) = std::fs::read_dir("/Applications") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".app") {
                    let base_name = name.replace(".app", "").to_lowercase();
                    // 记录全名
                    new_map.entry(base_name.clone()).or_insert(base_name.clone());
                    
                    // 如果名字包含空格或特殊字符，建立简写映射
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

        let mut guard = self.mapping.lock().unwrap();
        *guard = new_map;
    }
}

/// 搜索策略配置，解耦不同分类的搜索逻辑
struct SearchStrategy {
    spotlight_kind: String,
    extensions: Vec<&'static str>,
}

impl SearchStrategy {
    fn from_type(t: &str) -> Self {
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

    fn matches_extension(&self, path: &str) -> bool {
        if self.extensions.is_empty() { return true; }
        let path_lc = path.to_lowercase();
        // 针对 App 的特殊处理：只要路径中包含 .app 且不在 Contents 内部，就认为是程序
        if self.extensions.contains(&".app")
            && path_lc.contains(".app") && !path_lc.contains(".app/contents/") {
            return true;
        }
        self.extensions.iter().any(|ext| path_lc.ends_with(ext))
    }
}

#[tauri::command]
async fn search_files(
    keyword: String, 
    filter_type: String, 
    state: State<'_, AppCache>, 
    _app: AppHandle
) -> Result<Vec<SearchResult>, String> {
    search_files_internal(keyword, filter_type, state.inner().clone()).await
}

async fn search_files_internal(
    keyword: String, 
    filter_type: String, 
    state: AppCache
) -> Result<Vec<SearchResult>, String> {
    let start_time = std::time::Instant::now();
    let keyword_lc = keyword.to_lowercase();
    
    if keyword_lc.trim().is_empty() {
        return Ok(Vec::new());
    }

    println!("收到极速搜索请求: keyword='{}', type='{}'", keyword, filter_type);

    // 1. 并行执行搜索任务
    let spotlight_handle = {
        let keyword_lc = keyword_lc.clone();
        let filter_type_inner = filter_type.clone();
        let strategy = SearchStrategy::from_type(&filter_type_inner);
        let mapping = state.mapping.lock().unwrap().clone();
        let mapped_keyword = mapping.get(&keyword_lc).cloned();
        
        tokio::spawn(async move {
            let mut results = Vec::new();
            
            // 使用策略对象生成标准 Spotlight 查询
            let words: Vec<&str> = keyword_lc.split_whitespace().collect();
            let final_query = strategy.spotlight_query(&words, mapped_keyword.as_ref());
            
            println!("Spotlight 原始查询: {}", final_query);

            let mut tasks = vec![];
            
            // 任务 A: 用户目录 + 应用程序
            let q1 = final_query.clone();
            tasks.push(tokio::spawn(async move {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    AsyncCommand::new("mdfind")
                        .arg("-onlyin").arg(home)
                        .arg("-onlyin").arg("/Applications")
                        .arg(&q1)
                        .output()
                ).await;
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
                        .arg("-onlyin").arg("/Volumes")
                        .arg(&q_vol)
                        .output()
                ).await;
                match output {
                    Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
                    _ => String::new(),
                }
            }));

            // 2. 并行执行所有任务
            let task_results = futures::future::join_all(tasks).await;

            for content in task_results.into_iter().flatten() {
                for line in content.lines() {
                    let path = line.trim().to_string();
                    if path.is_empty() || path.contains("/Contents/MacOS/") || path.contains("/Library/") { continue; }
                    
                    let name = path.split('/').next_back().unwrap_or(&path).to_string();
                    results.push(SearchResult { path, name, score: 0 });
                }
            }
            results
        })
    };

    let memory_handle = {
        let keyword_lc = keyword_lc.clone();
        let filter_type = filter_type.clone();
        let index_files = state.index.files.clone();
        let strategy = SearchStrategy::from_type(&filter_type);
        let mapping = state.mapping.lock().unwrap().clone();
        
        tokio::spawn(async move {
            let mut results = Vec::new();
            let mut fallback_results = Vec::new();
            let start = std::time::Instant::now();
            let mapped_keyword = mapping.get(&keyword_lc).cloned();
            
            let volumes_exist: std::collections::HashSet<String> = if let Ok(entries) = std::fs::read_dir("/Volumes") {
                entries.flatten().map(|e| e.path().to_string_lossy().to_string()).collect()
            } else {
                std::collections::HashSet::new()
            };

            let words: Vec<&str> = keyword_lc.split_whitespace().collect();
            let guard = index_files.lock().unwrap();
            
            for path in guard.iter() {
                // 1. 类型预过滤 (使用 Strategy 解耦)
                if filter_type != "all" {
                    if filter_type == "folder" {
                        // 改进文件夹判断逻辑：不包含点，或者是以 .app 结尾的目录（在 macOS 中 app 也是文件夹）
                        let is_likely_dir = !path.contains('.') || path.ends_with(".app");
                        if !is_likely_dir { continue; }
                    } else if !strategy.matches_extension(path) {
                        continue;
                    }
                }

                // 2. 快速排除离线外接盘
                if path.starts_with("/Volumes/") {
                    let parts: Vec<&str> = path.split('/').collect();
                    if parts.len() >= 3 {
                        let vol_path = format!("/Volumes/{}", parts[2]);
                        if !volumes_exist.contains(&vol_path) { continue; }
                    }
                }

                let name = path.split('/').next_back().unwrap_or(path).to_string();
                let name_lc = name.to_lowercase();
                let path_lc = path.to_lowercase();
                
                // 3. 多词匹配逻辑 (仿 Everything：多词 AND 匹配)
                let mut matched_count = 0;
                for word in &words {
                    if name_lc.contains(word) || path_lc.contains(word) {
                        matched_count += 1;
                    }
                }

                // 4. 别名与缩写补充逻辑
                if matched_count < words.len() {
                    // 别名映射
                    if let Some(en_name) = mapped_keyword.as_ref() {
                        if name_lc.contains(en_name) {
                            matched_count = words.len();
                        }
                    }
                    // 自动缩写 (如 dpp -> Digital Photo Professional)
                    if matched_count < words.len() && keyword_lc.len() >= 2 {
                        let initials: String = name_lc
                            .split(|c: char| !c.is_alphanumeric())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.chars().next().unwrap_or(' '))
                            .collect();
                        if initials.contains(&keyword_lc) {
                            matched_count = words.len();
                        }
                    }
                }
                
                if matched_count == words.len() {
                    results.push(SearchResult { path: path.clone(), name, score: 0 });
                } else if matched_count > 0 && words.len() > 1 {
                    // 记录部分匹配的结果，作为 fallback
                    fallback_results.push(SearchResult { path: path.clone(), name, score: 0 });
                }

                if results.len() > 1000 { break; }
            }

            // 如果严格匹配结果太少，合并部分匹配的结果
            if results.len() < 20 {
                results.extend(fallback_results.into_iter().take(50));
            }

            println!("内存索引搜索耗时: {:?}", start.elapsed());
            results
        })
    };

    // 等待所有并行任务完成
    let (spotlight_res, memory_res) = tokio::join!(spotlight_handle, memory_handle);
    let spotlight_results = spotlight_res.unwrap_or_default();
    let memory_results = memory_res.unwrap_or_default();
    
    println!("Spotlight 返回: {} 条, 内存索引返回: {} 条", spotlight_results.len(), memory_results.len());
    
    let mut all_results = [spotlight_results, memory_results].concat();

    // 2. 移除重复项并预计算权重
    let mut seen = std::collections::HashSet::new();
    let history = state.click_history.lock().unwrap().clone();
    let mapping = state.mapping.lock().unwrap().clone();
    let mapped_keyword = mapping.get(&keyword_lc).cloned();
    
    all_results.retain(|r| seen.insert(r.path.clone()));

    for res in all_results.iter_mut() {
        let name_lc = res.name.to_lowercase();
        let path_lc = res.path.to_lowercase();
        
        // A. 基础匹配权重 (智能多词加权)
        let mut base_score = 0;

        let words: Vec<&str> = keyword_lc.split_whitespace().collect();
        let mut all_in_name = words.iter().all(|w| name_lc.contains(w));
        let all_in_path = words.iter().all(|w| path_lc.contains(w));

        // 别名与缩写支持 (Acronym)
        let mut is_alias_match = false;
        let mut is_acronym_match = false;

        // 1. 静态别名映射 (如 ps -> photoshop)
        if let Some(en_name) = mapped_keyword.as_ref() {
            if name_lc.contains(en_name) {
                all_in_name = true;
                is_alias_match = true;
            }
        }

        // 2. 自动缩写匹配 (如 dpp -> Digital Photo Professional)
        if !all_in_name && keyword_lc.len() >= 2 {
            let initials: String = name_lc
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().next().unwrap_or(' '))
                .collect();
            if initials.contains(&keyword_lc) {
                all_in_name = true;
                is_acronym_match = true;
            }
        }

        // 权重分配逻辑优化
        if all_in_name {
            if is_alias_match || is_acronym_match || name_lc == keyword_lc {
                base_score += 20000; // 进一步提高权重，确保绝对置顶
            } else {
                // 检查连续性
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
                        base_score += 5000; // 增加开头匹配加成
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
            && (res.path.ends_with(".app") || res.path.ends_with(".app/")) {
            base_score += 10000; // 确保 .app 目录排在其他文件前面
        }

        // B. 点击历史加成 (权重最高，体现自学习)
        if let Some(&clicks) = history.get(&res.path) {
            base_score += (clicks as i32) * 5000; // 显著提高点击权重
        }

        // C. 路径深度与嵌套惩罚
        let depth = res.path.split('/').count() as i32;
        
        // 惩罚嵌套在 .app 包内部的子程序 (如 Digital Photo Professional 4.app/Contents/Resources/...)
        if res.path.contains(".app/Contents/") {
            base_score -= 10000; 
        }

        if !res.path.starts_with("/Applications") {
            base_score -= depth * 50;
        }

        // D. 位置权重
        if res.path.starts_with("/Applications") {
            base_score += 5000; // 提高应用目录基础分
        } else if res.path.contains("/Desktop") {
            base_score += 1000;
        }

        res.score = base_score;
    }

    // 3. 最终排序 (仅根据预计算的 score)
    all_results.sort_by(|a, b| b.score.cmp(&a.score));

    let final_results: Vec<SearchResult> = all_results.into_iter().take(100).collect();
    println!("搜索极速完成: 耗时: {:?}", start_time.elapsed());
    
    Ok(final_results)
}

#[tauri::command]
fn open_file(path: String, state: State<'_, AppCache>) -> Result<(), String> {
    // 记录点击，实现自我学习
    {
        let mut history = state.click_history.lock().unwrap();
        let count = history.entry(path.clone()).or_insert(0);
        *count += 1;
        println!("自我学习: 用户点击了 {}, 当前点击次数: {}", path, count);
    }
    state.save_click_history();

    Command::new("open")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("无法打开文件: {}", e))?;
    Ok(())
}

#[tauri::command]
fn open_folder(path: String, state: State<'_, AppCache>) -> Result<(), String> {
    // 记录点击
    {
        let mut history = state.click_history.lock().unwrap();
        let count = history.entry(path.clone()).or_insert(0);
        *count += 1;
        println!("自我学习: 用户打开了 {} 的位置, 当前点击次数: {}", path, count);
    }
    state.save_click_history();

    let folder_path = if std::path::Path::new(&path).is_dir() {
        path
    } else {
        std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string())
    };

    Command::new("open")
        .arg(folder_path)
        .spawn()
        .map_err(|e| format!("无法打开文件夹: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn copy_to_clipboard(path: String) -> Result<(), String> {
    // 根据文件类型决定复制方式 (macOS 特有逻辑)
    // 如果是文件，尝试复制文件对象；如果失败，则复制路径
    let script = format!(
        "set theFile to (POSIX file \"{}\")\nset theClipboardData to {{file:theFile}}\nset the clipboard to theFile",
        path
    );
    
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output();

    match output {
        Ok(out) if out.status.success() => Ok(()),
        _ => {
            // Fallback: 如果 AppleScript 失败，使用 pbcopy 复制路径字符串
            let mut child = Command::new("pbcopy")
                .spawn()
                .map_err(|e| e.to_string())?;
            
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin.write_all(path.as_bytes()).map_err(|e| e.to_string())?;
            }
            
            child.wait().map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

#[tauri::command]
fn record_click(path: String, state: State<'_, AppCache>) -> Result<(), String> {
    let mut history = state.click_history.lock().unwrap();
    let count = history.entry(path).or_insert(0);
    *count += 1;
    state.save_click_history();
    Ok(())
}

#[tauri::command]
fn trigger_index_update(state: State<'_, AppCache>) -> Result<(), String> {
    state.index.force_update.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_cache = AppCache::new();
    let cache_clone = app_cache.clone();

    // 定义快捷键: Command + Shift + F (更不容易被占用)
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyF);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new()
            .with_handler(move |app, s, _event| {
                if s == &shortcut {
                    if let Some(window) = app.get_webview_window("main") {
                        let is_visible = window.is_visible().unwrap_or(false);
                        if is_visible {
                            let _ = window.hide();
                        } else {
                            let _ = window.show();
                            let _ = window.set_focus();
                            // 显式确保窗口置顶并激活
                            let _ = window.set_always_on_top(true);
                        }
                    }
                }
            })
            .build())
        .manage(app_cache)
        .setup(move |app| {
            // 处理 CLI 参数
            let mut is_cli_mode = false;
            if let Ok(matches) = app.cli().matches() {
                if let Some(query_arg) = matches.args.get("query") {
                    let query = query_arg.value.as_str().unwrap_or("").to_string();
                    let filter_type = matches.args.get("type")
                        .and_then(|t| t.value.as_str())
                        .unwrap_or("all")
                        .to_string();
                    
                    if !query.is_empty() {
                        is_cli_mode = true;
                        let app_handle = app.handle().clone();
                        let state = app_handle.state::<AppCache>();
                        let state_inner = state.inner().clone();
                        
                        tauri::async_runtime::spawn(async move {
                            // 执行搜索逻辑 (复用 search_files 的内部逻辑)
                            match search_files_internal(query, filter_type, state_inner).await {
                                Ok(results) => {
                                    for res in results.iter().take(10) {
                                        println!("{} -> {}", res.name, res.path);
                                    }
                                    std::process::exit(0);
                                }
                                Err(e) => {
                                    eprintln!("搜索出错: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        });
                    }
                }
            }

            // 如果不是 CLI 模式，显示窗口
            if !is_cli_mode {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            // 注册快捷键
            app.global_shortcut().register(shortcut).map_err(|e| e.to_string())?;

            // 监听窗口事件
            let window = app.get_webview_window("main").unwrap();
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                match event {
                    tauri::WindowEvent::Focused(false) => {
                        let _ = window_clone.hide();
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        // 拦截关闭请求，改为隐藏窗口
                        let _ = window_clone.hide();
                        api.prevent_close();
                    }
                    _ => {}
                }
            });

            // 启动后台索引任务
            cache_clone.index.start_indexing();
            
            // 后台映射更新任务 (每小时更新一次别名表)
            let cache_for_update = cache_clone.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    cache_for_update.update();
                    sleep(Duration::from_secs(3600)).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search_files, 
            open_file, 
            open_folder, 
            record_click,
            get_indexing_status,
            trigger_index_update,
            copy_to_clipboard
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        });
}
