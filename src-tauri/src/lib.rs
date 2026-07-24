//! 星TAP 极速搜索 - Tauri GUI 入口
//! 核心搜索逻辑在 sts-core crate 中，与 Tauri 完全解耦

use serde::Serialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_cli::CliExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
use tokio::time::{sleep, Duration};

use sts_core::{build_alias_mapping, ContentSearchParams, GlobalIndex};

mod mcp;

// Tauri 前端用的搜索结果结构（保持 API 兼容）
#[derive(Serialize, Clone)]
struct SearchResult {
    path: String,
    name: String,
    #[serde(skip)]
    #[allow(dead_code)]
    score: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

// 内容搜索结果（rg 输出）
#[derive(Serialize, Clone, Debug)]
struct ContentSearchResult {
    path: String,
    name: String,
    line_number: u64,
    line_content: String,
}

// 内容搜索请求参数（Tauri 前端传入）
#[derive(serde::Deserialize, Debug)]
struct ContentSearchRequest {
    keyword: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    filter_type: String,
    #[serde(default = "default_max_content_results")]
    max_results: usize,
}

fn default_max_content_results() -> usize {
    50
}

// 应用缓存
#[derive(Clone)]
struct AppCache {
    mapping: Arc<Mutex<HashMap<String, String>>>,
    click_history: Arc<Mutex<HashMap<String, u32>>>,
    index: GlobalIndex,
    /// MCP 服务实际绑定端口（None=未启动/占用失败），供 GUI 状态栏显示
    mcp_port: Arc<Mutex<Option<u16>>>,
}

impl AppCache {
    fn new() -> Self {
        let cache = Self {
            mapping: Arc::new(Mutex::new(build_alias_mapping())),
            click_history: Arc::new(Mutex::new(HashMap::new())),
            index: GlobalIndex::new(),
            mcp_port: Arc::new(Mutex::new(None)),
        };
        cache.load_click_history();
        // BM25 语义索引 + 模糊匹配器在索引构建/更新时由 build_index_once /
        // start_indexing_loop 自动重建（见 sts-core），此处无需手动触发
        cache
    }

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

    fn update_mapping(&self) {
        let new_map = build_alias_mapping();
        let mut guard = self.mapping.lock().unwrap();
        *guard = new_map;
    }
}

#[tauri::command]
fn get_indexing_status(state: State<'_, AppCache>) -> bool {
    *state.index.is_indexing.lock().unwrap()
}

#[tauri::command]
async fn search_files(
    keyword: String,
    filter_type: String,
    state: State<'_, AppCache>,
    _app: AppHandle,
) -> Result<Vec<SearchResult>, String> {
    let keyword_lc = keyword.to_lowercase();

    // 空关键词时返回最近文件（按修改日期倒序）
    if keyword_lc.trim().is_empty() {
        let results = sts_core::recent_files(&filter_type, 50).await;
        let converted: Vec<SearchResult> = results
            .into_iter()
            .map(|r| SearchResult {
                path: r.path,
                name: r.name,
                score: r.score.unwrap_or(0),
                elapsed_ms: r.elapsed_ms,
                source: r.source,
            })
            .collect();
        return Ok(converted);
    }

    println!(
        "收到极速搜索请求: keyword='{}', type='{}'",
        keyword, filter_type
    );

    let click_history = state.click_history.lock().unwrap().clone();
    let mapping = state.mapping.lock().unwrap().clone();

    let results = sts_core::search_files(
        &keyword,
        &filter_type,
        &state.index,
        &click_history,
        &mapping,
        true, // 人类 GUI 模式：过滤系统垃圾 + 代码降级
    )
    .await;

    // 转换为 Tauri 前端格式
    let final_results: Vec<SearchResult> = results
        .into_iter()
        .map(|r| SearchResult {
            path: r.path,
            name: r.name,
            score: r.score.unwrap_or(0),
            elapsed_ms: r.elapsed_ms,
            source: r.source,
        })
        .collect();

    Ok(final_results)
}

#[tauri::command]
async fn get_thumbnail(
    path: String,
    size: Option<u32>,
    state: State<'_, AppCache>,
) -> Result<Option<String>, String> {
    let index = state.index.clone();
    // qlmanage 是阻塞的 IO/进程调用，放到阻塞线程池，避免卡住异步 runtime
    let uri = tauri::async_runtime::spawn_blocking(move || {
        index.get_thumbnail(&path, size.unwrap_or(128))
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(uri)
}

#[tauri::command]
async fn search_content_command(
    request: ContentSearchRequest,
    _state: State<'_, AppCache>,
) -> Result<Vec<ContentSearchResult>, String> {
    let params = ContentSearchParams {
        keyword: request.keyword,
        path: request.path,
        filter_type: request.filter_type,
        max_results: request.max_results,
    };

    let results = sts_core::search_content(params).await?;

    // 转换为 Tauri 前端格式
    let final_results: Vec<ContentSearchResult> = results
        .into_iter()
        .map(|r| ContentSearchResult {
            path: r.path,
            name: r.name,
            line_number: r.line_number,
            line_content: r.line_content,
        })
        .collect();

    Ok(final_results)
}

#[tauri::command]
fn open_file(path: String, state: State<'_, AppCache>) -> Result<(), String> {
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
    {
        let mut history = state.click_history.lock().unwrap();
        let count = history.entry(path.clone()).or_insert(0);
        *count += 1;
        println!(
            "自我学习: 用户打开了 {} 的位置, 当前点击次数: {}",
            path, count
        );
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
    let script = format!(
        "set theFile to (POSIX file \"{}\")\nset theClipboardData to {{file:theFile}}\nset the clipboard to theFile",
        path
    );

    let output = Command::new("osascript").arg("-e").arg(&script).output();

    match output {
        Ok(out) if out.status.success() => Ok(()),
        _ => {
            let mut child = Command::new("pbcopy").spawn().map_err(|e| e.to_string())?;
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin
                    .write_all(path.as_bytes())
                    .map_err(|e| e.to_string())?;
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

#[tauri::command]
fn get_mcp_port(state: State<'_, AppCache>) -> Option<u16> {
    *state.mcp_port.lock().unwrap()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // GUI 应用经 launchd 拉起时 PATH 极窄（仅 /usr/bin:/bin:/usr/sbin:/sbin），
    // 导致 fd / rg 等 Homebrew 工具找不到，索引回退到慢速 find、搜索回退到 grep。
    // 这里把常见 Homebrew / 系统 bin 目录补进 PATH，保证快速搜索链路可用。
    if let Ok(current) = std::env::var("PATH") {
        let extra = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
        let mut parts: Vec<&str> = current.split(':').collect();
        for e in extra.iter() {
            if !parts.contains(e) {
                parts.push(e);
            }
        }
        let new_path = parts.join(":");
        std::env::set_var("PATH", &new_path);
        eprintln!("[sts] PATH 已补齐: {}", new_path);
    }

    let app_cache = AppCache::new();
    let cache_clone = app_cache.clone();

    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyF);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_cli::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, s, _event| {
                    if s == &shortcut {
                        if let Some(window) = app.get_webview_window("main") {
                            let is_visible = window.is_visible().unwrap_or(false);
                            if is_visible {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                                let _ = window.set_always_on_top(true);
                            }
                        }
                    }
                })
                .build(),
        )
        .manage(app_cache)
        .setup(move |app| {
            // 处理 CLI 参数（兼容旧的 Tauri CLI 模式）
            let mut is_cli_mode = false;
            if let Ok(matches) = app.cli().matches() {
                if let Some(query_arg) = matches.args.get("query") {
                    let query = query_arg.value.as_str().unwrap_or("").to_string();
                    let filter_type = matches
                        .args
                        .get("type")
                        .and_then(|t| t.value.as_str())
                        .unwrap_or("all")
                        .to_string();

                    if !query.is_empty() {
                        is_cli_mode = true;
                        let app_handle = app.handle().clone();
                        let state = app_handle.state::<AppCache>();
                        let state_inner = state.inner().clone();
                        let click_history = state_inner.click_history.lock().unwrap().clone();
                        let mapping = state_inner.mapping.lock().unwrap().clone();
                        let index = state_inner.index.clone();

                        tauri::async_runtime::spawn(async move {
                            let results = sts_core::search_files(
                                &query,
                                &filter_type,
                                &index,
                                &click_history,
                                &mapping,
                                true,
                            )
                            .await;

                            for res in results.iter().take(10) {
                                println!("{} -> {}", res.name, res.path);
                            }
                            std::process::exit(0);
                        });
                    }
                }
            }

            if !is_cli_mode {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            app.global_shortcut()
                .register(shortcut)
                .map_err(|e| e.to_string())?;

            let window = app.get_webview_window("main").unwrap();
            let window_clone = window.clone();
            window.on_window_event(move |event| match event {
                tauri::WindowEvent::Focused(false) => {
                    let _ = window_clone.hide();
                }
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let _ = window_clone.hide();
                    api.prevent_close();
                }
                _ => {}
            });

            // 启动后台索引任务（使用 Tauri 的 async runtime，而非裸 tokio::spawn）
            tauri::async_runtime::spawn(cache_clone.index.start_indexing_loop());

            // 启动 MCP 服务（给 AI/Agent 用；端口冲突自愈，实际端口回传 GUI 状态栏）
            {
                let port = mcp::start(cache_clone.index.clone(), cache_clone.mapping.clone());
                if let Some(p) = port {
                    eprintln!("[sts] MCP 已启动: http://127.0.0.1:{}/tools", p);
                }
                let state = app.state::<AppCache>();
                *state.mcp_port.lock().unwrap() = port;
            }

            // 后台映射更新任务
            let cache_for_update = cache_clone.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    cache_for_update.update_mapping();
                    sleep(Duration::from_secs(3600)).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search_files,
            search_content_command,
            open_file,
            open_folder,
            record_click,
            get_indexing_status,
            trigger_index_update,
            copy_to_clipboard,
            get_thumbnail,
            get_mcp_port
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
