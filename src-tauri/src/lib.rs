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
    // P1: nucleo 匹配位置（码点下标，前端高亮用；空 = 不高亮）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    match_pos: Vec<u32>,
    // 内部字段，用于排序优化
    #[serde(skip)]
    score: i32,
}

// 全局索引状态
#[derive(Clone)]
struct GlobalIndex {
    files: Arc<Mutex<Arc<Vec<String>>>>,
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
        let files = Arc::new(Mutex::new(Arc::new(Vec::new())));
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
                *guard = Arc::new(loaded_files);
            }
        }

        Self { files, is_indexing, force_update }
    }

    fn start_indexing(&self) {
        let files_clone = self.files.clone();
        let status_clone = self.is_indexing.clone();
        let force_update_clone = self.force_update.clone();
        tauri::async_runtime::spawn(async move {
            // 记录当前已挂载盘，作为增量扫描的基线
            let mut last_volumes: std::collections::HashSet<String> =
                std::fs::read_dir("/Volumes")
                    .map(|e| e.flatten().map(|x| x.path().to_string_lossy().to_string()).collect())
                    .unwrap_or_default();
            // 让首次全量在启动延迟后发生（避免一打开就狂吃资源）
            let mut last_full_scan = std::time::Instant::now() - Duration::from_secs(700);

            // 启动延迟 6s：先让 UI 起来、用户先能搜，再开始首扫
            sleep(Duration::from_secs(6)).await;

            loop {
                sleep(Duration::from_secs(30)).await;

                let current_volumes: std::collections::HashSet<String> =
                    std::fs::read_dir("/Volumes")
                        .map(|e| e.flatten().map(|x| x.path().to_string_lossy().to_string()).collect())
                        .unwrap_or_default();
                let new_vols: std::collections::HashSet<String> =
                    current_volumes.difference(&last_volumes).cloned().collect();
                let removed_vols: std::collections::HashSet<String> =
                    last_volumes.difference(&current_volumes).cloned().collect();
                let force_now = force_update_clone.load(Ordering::Relaxed);

                if !removed_vols.is_empty() {
                    prune_volume_files(&files_clone, &removed_vols);
                }

                if force_now {
                    force_update_clone.store(false, Ordering::Relaxed);
                    last_volumes = current_volumes;
                    run_full_scan(&files_clone, &status_clone, false).await;
                    last_full_scan = std::time::Instant::now();
                } else if !new_vols.is_empty() {
                    // 仅增量扫描新挂载的外接盘（不重复扫已扫盘，避免卡顿）
                    last_volumes = current_volumes;
                    run_incremental_scan(&files_clone, &status_clone, &new_vols).await;
                    last_full_scan = std::time::Instant::now();
                } else if last_full_scan.elapsed() > Duration::from_secs(600) {
                    last_volumes = current_volumes;
                    // 周期刷新只扫本地常用目录；外置盘靠挂载事件增量索引，不每 10 分钟重扫大盘
                    run_full_scan(&files_clone, &status_clone, false).await;
                    last_full_scan = std::time::Instant::now();
                } else {
                    last_volumes = current_volumes;
                }
            }
        });
    }
}

/// 扫描单个路径，返回该路径下（递归）的全部文件列表（rg 优先，find 兜底）
async fn scan_path(path: &str) -> Vec<String> {
    if !std::path::Path::new(path).exists() { return Vec::new(); }
    let output = {
        let rg = AsyncCommand::new("rg")
            .args(["--files", "-g", "!node_modules/**", "-g", "!.git/**",
                   "-g", "!Library/**", "-g", "!**/Contents/MacOS/**", "-g", "!.**"])
            .arg(path)
            .output()
            .await;
        match rg {
            Ok(ref out) if out.status.success() => rg,
            _ => fallback_find(path).await,
        }
    };
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty() && *l != path)
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 全量重建索引（本地常用目录 + /Applications；include_volumes=true 时含外接盘）
async fn run_full_scan(
    files_clone: &Arc<Mutex<Arc<Vec<String>>>>,
    status_clone: &Arc<Mutex<bool>>,
    include_volumes: bool,
) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let mut scan_paths = vec![
        format!("{}/Desktop", home),
        format!("{}/Downloads", home),
        format!("{}/Documents", home),
        "/Applications".to_string(),
    ];
    if include_volumes {
        scan_paths.push("/Volumes".to_string());
    }
    *status_clone.lock().unwrap() = true;
    let mut all_files = Vec::new();
    for p in &scan_paths {
        let batch = scan_path(p).await;
        println!("路径 {} 扫描完成，找到 {} 个文件", p, batch.len());
        // 增量合并到共享索引，用户马上可搜
        let mut cur = (**files_clone.lock().unwrap()).clone();
        cur.extend(batch.iter().cloned());
        *files_clone.lock().unwrap() = Arc::new(cur);
        all_files.extend(batch);
    }
    let index_path = get_index_path();
    if let Ok(mut file) = File::create(&index_path) {
        for f in &all_files { let _ = writeln!(file, "{}", f); }
    }
    *files_clone.lock().unwrap() = Arc::new(all_files);
    *status_clone.lock().unwrap() = false;
    println!("索引更新完成，共 {} 条数据，已持久化到本地", files_clone.lock().unwrap().len());
}

/// 增量扫描新挂载的外接盘，仅合并新增文件（不重建全量，避免卡顿）
async fn run_incremental_scan(
    files_clone: &Arc<Mutex<Arc<Vec<String>>>>,
    status_clone: &Arc<Mutex<bool>>,
    vols: &std::collections::HashSet<String>,
) {
    *status_clone.lock().unwrap() = true;
    let mut added: Vec<String> = Vec::new();
    for v in vols {
        added.extend(scan_path(v).await);
    }
    if !added.is_empty() {
        let mut cur = (**files_clone.lock().unwrap()).clone();
        cur.extend(added.iter().cloned());
        let index_path = get_index_path();
        if let Ok(mut file) = File::create(&index_path) {
            for f in cur.iter() { let _ = writeln!(file, "{}", f); }
        }
        *files_clone.lock().unwrap() = Arc::new(cur);
    }
    *status_clone.lock().unwrap() = false;
    println!("外接盘增量索引完成，新增 {} 条", added.len());
}

/// 外接盘卸载时，从内存索引 + 持久化文件剔除该盘全部文件（离线盘不再显示在结果里）
fn prune_volume_files(
    files_clone: &Arc<Mutex<Arc<Vec<String>>>>,
    vols: &std::collections::HashSet<String>,
) {
    let cur = (**files_clone.lock().unwrap()).clone();
    let kept: Vec<String> = cur
        .into_iter()
        .filter(|f| {
            if !f.starts_with("/Volumes/") { return true; }
            let vol = f.split('/').take(3).collect::<Vec<_>>().join("/");
            !vols.contains(&vol)
        })
        .collect();
    let n = kept.len();
    let index_path = get_index_path();
    if let Ok(mut file) = File::create(&index_path) {
        for f in &kept { let _ = writeln!(file, "{}", f); }
    }
    *files_clone.lock().unwrap() = Arc::new(kept);
    println!("已剔除离线盘文件，索引剩余 {} 条", n);
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
            {
                let mut mine = self.click_history.lock().unwrap();
                // P3a: 上限清理，防无界增长与权重漂移（超 10000 条只保留点击最高的 5000）
                if mine.len() > 10_000 {
                    let mut entries: Vec<(String, u32)> = mine.drain().collect();
                    entries.sort_by(|a, b| b.1.cmp(&a.1));
                    entries.truncate(5_000);
                    *mine = entries.into_iter().collect();
                    println!("点击历史超限清理: 保留 {} 条", mine.len());
                }
                if let Ok(content) = serde_json::to_string(&*mine) {
                    let _ = std::fs::write(&path, content);
                }
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
            // 高频中文口语名 + 清理管家类（2026-07-31 补全）
            ("腾讯会议", "wemeet"), ("wemeet", "wemeet"),
            ("企业微信", "wecom"), ("企微", "wecom"), ("wecom", "wecom"),
            ("百度网盘", "baidunetdisk"), ("网盘", "baidunetdisk"), ("baidunetdisk", "baidunetdisk"),
            ("迅雷", "thunder"), ("thunder", "thunder"), ("语雀", "yuque"), ("yuque", "yuque"),
            ("notion", "notion"), ("滴答清单", "ticktick"), ("ticktick", "ticktick"),
            ("微博", "weibo"), ("weibo", "weibo"), ("小红书", "xiaohongshu"), ("xhs", "xiaohongshu"), ("xiaohongshu", "xiaohongshu"),
            ("电脑管家", "tencent lemon"), ("腾讯电脑管家", "tencent lemon"), ("柠檬清理", "tencent lemon"), ("lemon", "tencent lemon"), ("tencentlemon", "tencent lemon"),
            ("cleanmymac", "cleanmymac x"), ("cleanmymac x", "cleanmymac x"), ("appcleaner", "app cleaner"), ("app cleaner", "app cleaner"), ("macbooster", "macbooster"),
            ("备忘录", "notes"), ("提醒事项", "reminders"), ("reminders", "reminders"),
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

/// 转义 NSPredicate 字符串字面量特殊字符（S4：防异常查询/转义破坏）
fn escape_predicate(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// 是否像扩展名：纯字母数字、长度 2-5（用于把 "tiff" 这类词转成后缀匹配，命中 Spotlight 索引）
fn is_like_ext(w: &str) -> bool {
    !w.is_empty() && w.len() >= 2 && w.len() <= 5 && w.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 执行一次 mdfind 查询，带超时与超时后进程清理。
/// 优先使用原生 -name 标志（实测 NSPredicate `*term*cd` 在本机返回空结果），
/// 仅复杂查询（含 path:/ size:> 等过滤条件）才回退 NSPredicate。
/// kill_on_drop(true) 双保险，杜绝子进程残留堆积卡死。
async fn run_mdfind_simple(term: &str, scope: &str, secs: u64) -> String {
    // 原生 -name 标志：mdfind -name 'jpg' → 实测返回 42 条（NSPredicate 返回 0）
    let child = match AsyncCommand::new("mdfind")
        .arg("-onlyin").arg(scope).arg("-name").arg(term)
        .kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => String::new(),
    }
}

/// NSPredicate 版 mdfind（仅复杂查询使用：path:/ size:> 等）
async fn run_mdfind_predicate(query: &str, scope: &str, secs: u64) -> String {
    let child = match AsyncCommand::new("mdfind")
        .arg("-onlyin").arg(scope).arg(query)
        .kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => String::new(),
    }
}

/// 解析 mdfind 原始输出为 SearchResult 列表（cap 2000 防爆炸）
fn parse_mdfind_results(joined: &[String]) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut cap = 0usize;
    for content in joined {
        for line in content.lines() {
            if cap >= 2000 { break; }
            let path = line.trim().to_string();
            if path.is_empty() || path.contains("/Contents/MacOS/") || path.contains("/Library/") { continue; }
            let name = path.split('/').next_back().unwrap_or(&path).to_string();
            results.push(SearchResult { path, name, score: 0, match_pos: Vec::new() });
            cap += 1;
        }
        if cap >= 2000 { break; }
    }
    results
}

/// 缩略图磁盘缓存（path@size -> data URI），避免重复 qlmanage（频繁搜索/切 tab 时大量省资源）
static THUMB_CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
    std::sync::OnceLock::new();
fn thumb_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    THUMB_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// P4c: Everything-lite 语法解析结果
#[derive(Clone)]
struct LiteQuery {
    terms: Vec<String>,        // 普通词（已去引号）
    ext: Option<String>,       // 扩展名过滤（*.png）
    path: Option<String>,      // 路径子串（path:/x/）
    size: Option<(char, u64)>, // 大小过滤（size:>100m）
}

/// 解析 Everything-lite 语法：`*.ext` / `path:/x/` / `size:>100m` / `"短语"` / 普通词
/// 非法 token（如 size:abc）降级为普通词，保证不吞查询
fn parse_lite_syntax(kw: &str) -> LiteQuery {
    let mut lite = LiteQuery { terms: Vec::new(), ext: None, path: None, size: None };
    let mut phrase = String::new();
    let mut in_quote = false;
    for token in kw.split_whitespace() {
        // 引号短语（跨空格）优先合并：`"hello world"` → 整词
        if in_quote {
            phrase.push(' ');
            phrase.push_str(token.trim_end_matches('"'));
            if token.ends_with('"') {
                in_quote = false;
                lite.terms.push(std::mem::take(&mut phrase));
            }
            continue;
        }
        if token.starts_with('"') && !token.ends_with('"') && token.len() > 1 {
            in_quote = true;
            phrase = token.trim_start_matches('"').to_string();
            continue;
        }
        if let Some(rest) = token.strip_prefix("*.") {
            let ext: String = rest.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            if !ext.is_empty() { lite.ext = Some(ext); }
        } else if let Some(rest) = token.strip_prefix("path:") {
            let p = rest.trim().trim_matches('/').to_lowercase();
            if !p.is_empty() { lite.path = Some(p); }
        } else if let Some(rest) = token.strip_prefix("size:") {
            if let Some((op, num)) = parse_size(rest) { lite.size = Some((op, num)); }
            else { lite.terms.push(token.to_string()); } // 解析失败降级普通词
        } else {
            let t = token.trim_matches('"');
            if !t.is_empty() { lite.terms.push(t.to_string()); }
        }
    }
    // 未闭合引号：累积短语降级为普通词
    if in_quote && !phrase.is_empty() { lite.terms.push(phrase); }
    lite
}

/// 解析 size 条件：`>100m` / `<2g`（k/m/g/b 单位，1k=1024）
fn parse_size(s: &str) -> Option<(char, u64)> {
    let s = s.trim();
    let (op, rest) = if let Some(r) = s.strip_prefix('>') { ('>', r) }
        else if let Some(r) = s.strip_prefix('<') { ('<', r) }
        else { return None; };
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() { return None; }
    let unit: char = rest.chars().nth(num.len()).unwrap_or('b');
    let mult: u64 = match unit.to_ascii_lowercase() {
        'k' => 1024,
        'm' => 1024 * 1024,
        'g' => 1024 * 1024 * 1024,
        _ => 1,
    };
    num.parse::<u64>().ok().map(|n| (op, n * mult))
}

/// 构造 NSPredicate：词(文件名 OR 路径, AND) + 别名(OR 逃生) + ext/path/size + 分类，全部 AND
fn build_lite_query(strategy: &SearchStrategy, lite: &LiteQuery, alias: Option<&String>) -> String {
    let mut conds: Vec<String> = Vec::new();
    let mut term_parts: Vec<String> = Vec::new();
    for w in &lite.terms {
        let esc = escape_predicate(w);
        let mut per = format!(
            "(kMDItemFSName == '*{}*'cd || kMDItemPath == '*{}*'cd)",
            esc, esc
        );
        // 像扩展名的词额外加后缀匹配（命中 filename 索引，避免前缀通配符全库扫描）
        if is_like_ext(w) {
            per = format!("({} || kMDItemFSName == '*.{}'cd)", per, esc);
        }
        term_parts.push(per);
    }
    let base = if term_parts.is_empty() && alias.is_none() {
        None
    } else if term_parts.is_empty() {
        Some(format!("kMDItemFSName == '*{}*'cd", escape_predicate(alias.unwrap())))
    } else if let Some(a) = alias {
        Some(format!("({}) || kMDItemFSName == '*{}*'cd", term_parts.join(" && "), escape_predicate(a)))
    } else {
        Some(format!("({})", term_parts.join(" && ")))
    };
    if let Some(b) = base { conds.push(b); }
    if let Some(ext) = &lite.ext {
        conds.push(format!("kMDItemFSName == '*.{}'cd", escape_predicate(ext)));
    }
    if let Some(p) = &lite.path {
        conds.push(format!("kMDItemPath contains '{}'cd", escape_predicate(p)));
    }
    if let Some((op, bytes)) = lite.size {
        conds.push(format!("kMDItemFSSize {} {}", op, bytes));
    }
    if !strategy.spotlight_kind.is_empty() {
        conds.push(strategy.spotlight_kind.clone());
    }
    if conds.is_empty() { return strategy.spotlight_kind.clone(); }
    format!("({})", conds.join(" && "))
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

// 常见扩展名兄弟形式：tiff<->tif, jpeg<->jpg（查询其一也应命中另一种写法）
fn ext_sibling(ext: &str) -> Option<&'static str> {
    match ext {
        "tiff" => Some("tif"),
        "tif" => Some("tiff"),
        "jpeg" => Some("jpg"),
        "jpg" => Some("jpeg"),
        _ => None,
    }
}

async fn search_files_internal(
    keyword: String, 
    filter_type: String, 
    state: AppCache
) -> Result<Vec<SearchResult>, String> {
    let start_time = std::time::Instant::now();
    let keyword_lc = keyword.to_lowercase();
    let lite = parse_lite_syntax(&keyword_lc);
    
    if keyword_lc.trim().is_empty() {
        return Ok(Vec::new());
    }

    println!("收到极速搜索请求: keyword='{}', type='{}'", keyword, filter_type);
    let t0 = std::time::Instant::now();

    // ── 全局硬超时：即使极端情况也不卡死（Everything 原则：查询 < 1s）──
    let total_timeout = Duration::from_secs(3);

    // 判断是否为简单查询（纯关键词，无 path:/ size:> 等复杂过滤）
    let is_simple_query = lite.path.is_none() && lite.size.is_none() && lite.ext.is_none()
        && lite.terms.len() <= 2;

    // ── 并行执行：Spotlight + 内存索引同时跑，谁快谁先贡献结果 ──
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
    let spotlight_task = {
        let keyword_lc = keyword_lc.clone();
        let filter_type_inner = filter_type.clone();
        let strategy = SearchStrategy::from_type(&filter_type_inner);
        let mapping = state.mapping.lock().unwrap().clone();
        let mapped_keyword = mapping.get(&keyword_lc).cloned();
        let lite = lite.clone();

        tokio::spawn(async move {
            if is_simple_query && !lite.terms.is_empty() {
                // 简单查询：用原生 -name 标志（实测 NSPredicate *term*cd 返回空）
                // 只取第一个词作为 -name 参数（mdfind -name 不支持多词 AND）
                let term = &lite.terms[0];
                println!("[SPOTLIGHT] 简单查询: mdfind -name '{}'", term);
                let (home_out, app_out) = tokio::join!(
                    run_mdfind_simple(term, &home, 2),
                    run_mdfind_simple(term, "/Applications", 2)
                );
                let joined = vec![home_out, app_out];
                parse_mdfind_results(&joined)
            } else {
                // 复杂查询（含 ext/path/size）：回退 NSPredicate
                let final_query = build_lite_query(&strategy, &lite, mapped_keyword.as_ref());
                println!("[SPOTLIGHT] 复杂查询: {}", final_query);
                let (home_out, app_out) = tokio::join!(
                    run_mdfind_predicate(&final_query, &home, 2),
                    run_mdfind_predicate(&final_query, "/Applications", 2)
                );
                let joined = vec![home_out, app_out];
                parse_mdfind_results(&joined)
            }
        })
    };

    let memory_task = {
        let keyword_lc = keyword_lc.clone();
        let filter_type = filter_type.clone();
        let index_files = state.index.files.clone();
        let strategy = SearchStrategy::from_type(&filter_type);
        let mapping = state.mapping.lock().unwrap().clone();
        let lite_for_mem = lite.clone();

        tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            let mut fallback_results = Vec::new();
            let start = std::time::Instant::now();
            let mapped_keyword = mapping.get(&keyword_lc).cloned();
            let volumes_exist: std::collections::HashSet<String> =
                if let Ok(entries) = std::fs::read_dir("/Volumes") {
                    entries.flatten().map(|e| e.path().to_string_lossy().to_string()).collect()
                } else { std::collections::HashSet::new() };
            let words: Vec<&str> = lite_for_mem.terms.iter().map(|s| s.as_str()).collect();
            let snapshot = index_files.lock().unwrap().clone();
            
            // 扩展名预过滤（最便宜的检查，先排除大量不匹配项）
            let quick_ext_filter = lite_for_mem.ext.as_ref().map(|ext| format!(".{}", ext));
            // 是否为常见扩展名（这类词不应触发缩写匹配）
            let is_common_ext = words.len() == 1 && matches!(words[0], "jpg"|"jpeg"|"png"|"gif"|"mp4"|"pdf"|"doc"|"txt"|"tiff"|"tif");

            for path in snapshot.iter() {
                // 1. 类型预过滤
                if filter_type != "all" {
                    if filter_type == "folder" {
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
                // 3. 扩展名快速过滤（在分配 name/to_lowercase 之前）
                if let Some(ref qext) = quick_ext_filter {
                    if !path.to_lowercase().ends_with(qext.as_str()) { continue; }
                }
                let name = path.split('/').next_back().unwrap_or(path).to_string();
                let name_lc = name.to_lowercase();
                let path_lc = path.to_lowercase();
                // 4. 内存侧 ext/path 过滤
                if let Some(ext) = &lite_for_mem.ext {
                    if !name_lc.ends_with(&format!(".{}", ext)) { continue; }
                }
                if let Some(p) = &lite_for_mem.path {
                    if !path_lc.contains(p) { continue; }
                }
                // 5. 多词匹配逻辑
                let mut matched_count = 0;
                for word in &words {
                    if name_lc.contains(word) || path_lc.contains(word) { matched_count += 1; }
                }
                // 5b. 单常见扩展名：兄弟形式亦视为命中（tiff<->tif, jpg<->jpeg）
                if matched_count < words.len() && is_common_ext && words.len() == 1 {
                    if let Some(sib) = ext_sibling(words[0]) {
                        if name_lc.contains(sib) || path_lc.contains(sib) {
                            matched_count = words.len();
                        }
                    }
                }
                // 6. 别名补充（仅精确别名，不触发缩写噪声）
                if matched_count < words.len() {
                    if let Some(en_name) = mapped_keyword.as_ref() {
                        if name_lc.contains(en_name) { matched_count = words.len(); }
                    }
                    // 缩写匹配限制：仅对 ≥3 字符且非常见扩展名的查询启用
                    // （修复 "tiff" 匹配到 522 个无关文件缩写的噪声问题）
                    if matched_count < words.len() && !is_common_ext && keyword_lc.len() >= 3 && words.len() == 1 {
                        let initials: String = name_lc
                            .split(|c: char| !c.is_alphanumeric())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.chars().next().unwrap_or(' '))
                            .collect();
                        if initials.contains(&keyword_lc) { matched_count = words.len(); }
                    }
                }
                if matched_count == words.len() {
                    results.push(SearchResult { path: path.clone(), name, score: 0, match_pos: Vec::new() });
                } else if matched_count > 0 && words.len() > 1 {
                    fallback_results.push(SearchResult { path: path.clone(), name, score: 0, match_pos: Vec::new() });
                }
                if results.len() > 1000 { break; }
            }
            if results.len() < 20 {
                results.extend(fallback_results.into_iter().take(50));
            }
            println!("[MEMORY] 搜索耗时: {:?}, 结果数: {}", start.elapsed(), results.len());
            results
        })
    };

    // ── 并行等待两个搜索源，全局 3s 超时保护 ──
    let (spotlight_results, memory_results) = match tokio::time::timeout(total_timeout, async {
        let s = spotlight_task.await.unwrap_or_default();
        let m = memory_task.await.unwrap_or_default();
        (s, m)
    }).await {
        Ok((s, m)) => (s, m),
        Err(_) => {
            println!("[TIMEOUT] 全局 3s 超时！返回已有结果");
            (Vec::new(), Vec::new())
        }
    };

    println!("[TIMING] 全部搜索完成: 耗时: {:?}, spotlight={}, memory={}", 
             t0.elapsed(), spotlight_results.len(), memory_results.len());
    println!("Spotlight 返回: {} 条, 内存索引返回: {} 条", spotlight_results.len(), memory_results.len());
    
    let mut all_results = [spotlight_results, memory_results].concat();

    // 2. 移除重复项并预计算权重
    let mut seen = std::collections::HashSet::new();
    let history = state.click_history.lock().unwrap().clone();
    let mapping = state.mapping.lock().unwrap().clone();
    let mapped_keyword = mapping.get(&keyword_lc).cloned();
    
    all_results.retain(|r| seen.insert(r.path.clone()));

    // P1: nucleo 模糊匹配器（复用单例，135KB 暂存不重复分配）
    let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);

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
            base_score += (clicks.min(10) as i32) * 5000; // 显著提高点击权重（P3a：封顶 10 次防单文件长期主导）
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

        // P1: nucleo 模糊匹配分（子序列贴合度，微秒级，越贴合分越高）
        if !keyword_lc.is_empty() {
            let mut hbuf: Vec<char> = Vec::new();
            let mut nbuf: Vec<char> = Vec::new();
            if let Some(nscore) = matcher.fuzzy_match(
                nucleo::Utf32Str::new(name_lc.as_str(), &mut hbuf),
                nucleo::Utf32Str::new(keyword_lc.as_str(), &mut nbuf),
            ) {
                base_score += nscore as i32;
            }
        }

        res.score = base_score;
    }

    // 3. 最终排序 (仅根据预计算的 score)
    all_results.sort_by(|a, b| b.score.cmp(&a.score));

    // P1: Top-N 高亮位置（仅最佳 30 条跑 fuzzy_indices，控制开销）
    for res in all_results.iter_mut().take(30) {
        let name_lc = res.name.to_lowercase();
        let mut hbuf: Vec<char> = Vec::new();
        let mut nbuf: Vec<char> = Vec::new();
        let mut positions = Vec::new();
        if matcher.fuzzy_indices(
            nucleo::Utf32Str::new(name_lc.as_str(), &mut hbuf),
            nucleo::Utf32Str::new(keyword_lc.as_str(), &mut nbuf),
            &mut positions,
        ).is_some() {
            res.match_pos = positions;
        }
    }

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

/// 用 qlmanage 生成缩略图，返回 base64 data URI（前端懒加载）。
/// 每个文件独立临时目录（按路径 hash），避免并发 remove_dir_all 互相删除导致失败；
/// 命中缓存直接返回，避免频繁搜索/切 tab 时重复 qlmanage 风暴。
#[tauri::command]
fn get_thumbnail(path: String, size: Option<u32>) -> Result<Option<String>, String> {
    let size = size.unwrap_or(128);
    let cache_key = format!("{}@{}", path, size);
    if let Some(cached) = thumb_cache().lock().unwrap().get(&cache_key) {
        return Ok(Some(cached.clone()));
    }

    let dir = std::env::temp_dir().join(format!("sts-thumb-{}-{}", std::process::id(), simple_hash(&path)));
    let _ = std::fs::create_dir_all(&dir);

    let output = Command::new("qlmanage")
        .arg("-t")
        .arg("-s")
        .arg(size.to_string())
        .arg("-o")
        .arg(&dir)
        .arg(&path)
        .output()
        .map_err(|e| format!("qlmanage failed: {}", e))?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(None);
    }

    let file_name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let thumb_path = dir.join(format!("{}.png", file_name));

    let result = std::fs::read(&thumb_path).ok().map(|data| {
        use base64::engine::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        format!("data:image/png;base64,{}", b64)
    });

    let _ = std::fs::remove_dir_all(&dir);
    if let Some(b64) = &result {
        thumb_cache().lock().unwrap().insert(cache_key, b64.clone());
    }
    Ok(result)
}

/// 回退方案：系统 find 命令（rg 不可用时使用）。
async fn fallback_find(path: &str) -> Result<std::process::Output, std::io::Error> {
    AsyncCommand::new("find")
        .arg(path)
        .args(["(", "-path", "*/node_modules/*", "-o", "-path", "*/.git/*",
               "-o", "-path", "*/Library/*", "-o", "-path", "*/Contents/MacOS/*",
               "-o", "-name", ".*", ")", "-prune", "-o", "-print"])
        .output()
        .await
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
            copy_to_clipboard,
            get_thumbnail
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lite_syntax() {
        // 纯扩展名
        let q = parse_lite_syntax("*.png");
        assert_eq!(q.ext.as_deref(), Some("png"));
        assert!(q.terms.is_empty());

        // 混合：词 + path + size
        let q = parse_lite_syntax("report path:/docs/ size:>100m");
        assert_eq!(q.terms, vec!["report"]);
        assert_eq!(q.path.as_deref(), Some("docs"));
        assert_eq!(q.size, Some(('>', 100 * 1024 * 1024)));

        // 短语（去引号保留空格）
        let q = parse_lite_syntax("\"hello world\" ps");
        assert_eq!(q.terms, vec!["hello world", "ps"]);

        // 非法 size 降级普通词
        let q = parse_lite_syntax("size:abc");
        assert_eq!(q.terms, vec!["size:abc"]);
        assert!(q.size.is_none());

        // 单位换算
        assert_eq!(parse_size(">2g"), Some(('>', 2 * 1024 * 1024 * 1024)));
        assert_eq!(parse_size("<500k"), Some(('<', 500 * 1024)));
        assert_eq!(parse_size(">100"), Some(('>', 100)));
        assert_eq!(parse_size("100m"), None); // 无 >/< 不解析
    }

    #[test]
    fn test_build_lite_query() {
        let s = SearchStrategy::from_type("all");
        // 扩展名查询
        let q = parse_lite_syntax("blue *.jpg");
        let sql = build_lite_query(&s, &q, None);
        assert!(sql.contains("'*.jpg'cd"), "应含扩展名条件: {}", sql);
        assert!(sql.contains("blue"), "应含关键词: {}", sql);

        // 别名 OR 逃生
        let q = parse_lite_syntax("ps");
        let sql = build_lite_query(&s, &q, Some(&"photoshop".to_string()));
        assert!(sql.contains("photoshop"), "应含别名: {}", sql);
        assert!(sql.contains("||"), "别名应为 OR: {}", sql);
    }
}


#[cfg(test)]
mod search_tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::runtime::Runtime;

    use crate::{AppCache, GlobalIndex};

    // 复刻 AppCache 最小构造（不触发真实索引扫描/文件 IO）
    fn make_cache(index_paths: Vec<String>) -> AppCache {
        let files = Arc::new(Mutex::new(Arc::new(index_paths)));
        let index = GlobalIndex {
            files,
            is_indexing: Arc::new(Mutex::new(false)),
            force_update: Arc::new(AtomicBool::new(false)),
        };
        AppCache {
            mapping: Arc::new(Mutex::new(HashMap::new())),
            click_history: Arc::new(Mutex::new(HashMap::new())),
            index,
        }
    }

    // 回归测试：tiff 搜索应秒回、且不被缩写噪声误伤（v2.0.5 根治点）
    #[test]
    fn tiff_search_is_fast_and_noise_free() {
        let index = vec![
            "/Users/xtap/Pictures/photo.tiff".to_string(),
            "/Users/xtap/Documents/report.TIFF".to_string(),
            "/Users/xtap/Downloads/scan_2024.tif".to_string(),
            // 缩写噪声：tiff 绝不应匹配到这类文件名
            "/Users/xtap/Documents/this.is.fine.friend.txt".to_string(),
            "/Users/xtap/Movies/clip.mp4".to_string(),
        ];
        let cache = make_cache(index);
        let rt = Runtime::new().unwrap();
        let start = Instant::now();
        let res = rt
            .block_on(super::search_files_internal(
                "tiff".to_string(),
                "all".to_string(),
                cache,
            ))
            .expect("search ok");
        let elapsed = start.elapsed();

        let paths: Vec<&str> = res.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.iter().any(|p| p.ends_with("photo.tiff")), "应命中真实 .tiff");
        assert!(paths.iter().any(|p| p.ends_with("report.TIFF")), "应命中大写 .TIFF");
        assert!(paths.iter().any(|p| p.ends_with("scan_2024.tif")), "应命中 .tif");
        assert!(
            !paths.iter().any(|p| p.contains("this.is.fine.friend")),
            "缩写噪声 this.is.fine.friend 不应被 tiff 匹配"
        );
        assert!(elapsed.as_secs() < 3, "搜索应在 3s 硬超时内返回, 实际 {:?}", elapsed);
        println!(
            "[TEST] tiff 搜索返回 {} 条, 耗时 {:?}（内存索引路径验证通过）",
            res.len(),
            elapsed
        );
    }
}
