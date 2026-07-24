//! 星TAP 极速搜索 CLI (sts)
//! 独立命令行版本，不依赖 Tauri GUI
//! 可被 AI / Skill / 脚本调用

use clap::{Parser, Subcommand};
use std::collections::HashMap;

// 引入核心搜索引擎（独立 crate，无 Tauri 依赖）
use sts_core::*;

#[derive(Parser)]
#[command(name = "sts")]
#[command(bin_name = "sts")]
#[command(version, about = "星TAP 极速搜索 CLI - 基于 fd/rg 的极速文件检索工具")]
#[command(
    long_about = "星TAP 极速搜索 CLI\n\n基于 fd + ripgrep 的极速文件名搜索和内容搜索。\n支持 Spotlight 集成、索引缓存、别名映射、JSON 输出。\n\n示例:\n  sts search \"关键词\"           # 搜索文件名\n  sts search \"ps\" -t app        # 搜索 Photoshop 程序\n  sts search \"photo\" --time     # 显示搜索耗时\n  sts content \"TODO\"            # 搜 Desktop+Downloads 内容\n  sts content \"TODO\" -p ~/src   # 搜指定目录内容\n  sts content \"TODO\" --json     # JSON 格式输出\n  sts index                     # 构建/更新索引\n  sts index --status            # 查看索引状态"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 搜索文件名
    #[command(alias = "s", alias = "find", alias = "f")]
    Search {
        /// 搜索关键词（支持拼音缩写、中文俗称、英文原名）
        keyword: String,

        /// 搜索类型: all, image, video, audio, pdf, doc, app, folder
        #[arg(short = 't', long, default_value = "all")]
        filter_type: String,

        /// 最大返回结果数
        #[arg(short = 'n', long, default_value = "20")]
        max_results: usize,

        /// 以 JSON 格式输出（方便 AI/脚本解析）
        #[arg(long)]
        json: bool,

        /// 显示搜索耗时（默认智能：慢查询自动显示）
        #[arg(long)]
        time: bool,
    },

    /// 搜索文件内容 (默认搜 Desktop+Downloads，-p 指定路径)
    #[command(alias = "c", alias = "grep", alias = "rg")]
    Content {
        /// 搜索关键词
        keyword: String,

        /// 搜索路径（默认 Desktop + Downloads，指定具体目录更快）
        #[arg(short, long)]
        path: Option<String>,

        /// 文件类型过滤: all, image, video, audio, pdf, doc, code
        #[arg(short = 'T', long, default_value = "all")]
        filter_type: String,

        /// 最大返回结果数
        #[arg(short = 'n', long, default_value = "50")]
        max_results: usize,

        /// 以 JSON 格式输出
        #[arg(long)]
        json: bool,

        /// 显示搜索耗时
        #[arg(long)]
        time: bool,
    },

    /// 构建/更新文件索引
    #[command(alias = "i", alias = "reindex")]
    Index {
        /// 查看索引状态
        #[arg(long)]
        status: bool,

        /// 强制重建索引（忽略缓存）
        #[arg(long)]
        force: bool,
    },

    /// 列出索引中的文件（调试用）
    #[command(alias = "ls")]
    List {
        /// 限制输出条数
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,

        /// 按路径过滤
        #[arg(short, long)]
        filter: Option<String>,
    },
}

fn load_click_history() -> HashMap<String, u32> {
    if let Some(mut path) = dirs_for_cache() {
        path.push("com.xtap.search");
        path.push("click_history.json");
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(history) = serde_json::from_str::<HashMap<String, u32>>(&content) {
                return history;
            }
        }
    }
    HashMap::new()
}

/// 获取缓存目录（不依赖 dirs crate）
fn dirs_for_cache() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join("Library/Caches"))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search {
            keyword,
            filter_type,
            max_results,
            json,
            time,
        } => {
            cmd_search(&keyword, &filter_type, max_results, json, time).await;
        }
        Commands::Content {
            keyword,
            path,
            filter_type,
            max_results,
            json,
            time,
        } => {
            cmd_content(&keyword, path, &filter_type, max_results, json, time).await;
        }
        Commands::Index { status, force } => {
            cmd_index(status, force).await;
        }
        Commands::List { limit, filter } => {
            cmd_list(limit, filter.as_deref());
        }
    }
}

async fn cmd_search(
    keyword: &str,
    filter_type: &str,
    max_results: usize,
    json: bool,
    show_time: bool,
) {
    let start = std::time::Instant::now();

    // CLI 模式：如果缓存过期且不存在，先构建索引（需要 GlobalIndex）
    if is_cache_stale() {
        let index = GlobalIndex::new();
        if index.is_empty() {
            eprintln!("[sts] 索引为空，开始构建...");
            index.build_index_once().await;
        }
    }

    // CLI 搜索：不需要加载索引到内存！rg 直接搜缓存文件 + mdfind 搜 Spotlight
    // GlobalIndex::empty() 创建不加载缓存的空索引，避免 1s+ 的加载时间
    let index = GlobalIndex::empty();
    let click_history = load_click_history();
    let mapping = build_alias_mapping();

    let results = search_files(keyword, filter_type, &index, &click_history, &mapping, true).await;
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_millis();

    if json {
        let output: Vec<serde_json::Value> = results
            .iter()
            .take(max_results)
            .map(|r| {
                let mut obj = serde_json::json!({
                    "name": r.name,
                    "path": r.path,
                });
                // 慢查询或用户要求时才输出耗时
                if show_time || elapsed_ms > 100 {
                    obj["elapsed_ms"] = serde_json::json!(elapsed_ms as u64);
                }
                if let Some(ref source) = r.source {
                    obj["source"] = serde_json::json!(source);
                }
                obj
            })
            .collect();

        // JSON 输出也加上整体统计
        let mut wrapper = serde_json::json!({
            "results": output,
            "total": results.len(),
        });
        if show_time || elapsed_ms > 100 {
            wrapper["elapsed_ms"] = serde_json::json!(elapsed_ms as u64);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&wrapper).unwrap_or_else(|e| {
                eprintln!("[sts] JSON 序列化失败: {}", e);
                "{}".to_string()
            })
        );
    } else {
        if results.is_empty() {
            eprintln!("[sts] 未找到匹配项");
            return;
        }
        for res in results.iter().take(max_results) {
            println!("{}\n  → {}", res.name, res.path);
        }

        // 智能耗时显示：快速搜索简洁输出，慢搜索显示详情
        let shown = max_results.min(results.len());
        if show_time || elapsed_ms > 10 {
            eprintln!(
                "[sts] {} 条结果（显示 {} 条）| {}ms",
                results.len(),
                shown,
                elapsed_ms
            );
        } else {
            eprintln!("[sts] {} 条结果（显示 {} 条）", results.len(), shown);
        }
    }
}

async fn cmd_content(
    keyword: &str,
    path: Option<String>,
    filter_type: &str,
    max_results: usize,
    json: bool,
    show_time: bool,
) {
    let start = std::time::Instant::now();

    let params = ContentSearchParams {
        keyword: keyword.to_string(),
        path: path.unwrap_or_default(), // 空值 → search_content 默认搜 Desktop+Downloads
        filter_type: filter_type.to_string(),
        max_results,
    };

    match search_content(params).await {
        Ok(results) => {
            let elapsed = start.elapsed();
            let elapsed_ms = elapsed.as_millis();

            if json {
                let mut wrapper = serde_json::json!({
                    "results": results,
                    "total": results.len(),
                });
                if show_time || elapsed_ms > 100 {
                    wrapper["elapsed_ms"] = serde_json::json!(elapsed_ms as u64);
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&wrapper).unwrap_or_else(|e| {
                        eprintln!("[sts] JSON 序列化失败: {}", e);
                        "{}".to_string()
                    })
                );
            } else {
                if results.is_empty() {
                    eprintln!("[sts] 未在文件内容中找到匹配项");
                    return;
                }
                for res in &results {
                    println!("{}:{}: {}", res.name, res.line_number, res.line_content);
                    println!("  → {}", res.path);
                }
                if show_time || elapsed_ms > 100 {
                    eprintln!("[sts] {} 条结果 | {}ms", results.len(), elapsed_ms);
                } else {
                    eprintln!("[sts] {} 条结果", results.len());
                }
            }
        }
        Err(e) => {
            eprintln!("[sts] 错误: {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_index(status: bool, force: bool) {
    if status {
        // 查状态时只需读文件元信息，不加载索引内容
        let index_path = get_index_path();
        let cache_exists = index_path.exists();
        let cache_size = if cache_exists {
            std::fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let cache_age = if cache_exists {
            std::fs::metadata(&index_path)
                .and_then(|m| m.modified())
                .map(|t| {
                    let age = std::time::SystemTime::now()
                        .duration_since(t)
                        .unwrap_or(std::time::Duration::ZERO);
                    format!("{}h{}m", age.as_secs() / 3600, (age.as_secs() % 3600) / 60)
                })
                .unwrap_or_else(|_| "未知".to_string())
        } else {
            "不存在".to_string()
        };

        // 快速统计行数（不加载到内存）
        let line_count = if cache_exists {
            std::process::Command::new("wc")
                .arg("-l")
                .arg(&index_path)
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        } else {
            0
        };

        println!("索引状态: 已就绪");
        println!("索引条目: {}", line_count);
        println!("缓存路径: {}", index_path.display());
        println!("缓存大小: {} KB", cache_size / 1024);
        println!("缓存年龄: {}", cache_age);
        println!(
            "缓存过期: {}",
            if is_cache_stale() {
                "是（需重建）"
            } else {
                "否"
            }
        );
        println!("fd 可用: {}", has_fd());
        println!("rg 可用: {}", has_rg());
        return;
    }

    let index = GlobalIndex::new();

    if force {
        let index_path = get_index_path();
        if index_path.exists() {
            let _ = std::fs::remove_file(&index_path);
            eprintln!("[sts] 已删除旧索引缓存");
        }
        {
            let mut guard = index.files.lock().unwrap();
            guard.clear();
        }
    }

    eprintln!("[sts] 开始构建索引...");
    let start = std::time::Instant::now();
    index.build_index_once().await;
    let elapsed = start.elapsed();

    let count = index.len();
    let cache_size = std::fs::metadata(get_index_path())
        .map(|m| m.len() / 1024)
        .unwrap_or(0);
    eprintln!(
        "[sts] 索引构建完成: {} 条 | {} KB | {:?}",
        count, cache_size, elapsed
    );
}

fn cmd_list(limit: usize, filter: Option<&str>) {
    let index = GlobalIndex::new();
    let guard = index.files.lock().unwrap();

    if guard.is_empty() {
        eprintln!("[sts] 索引为空，请先运行 `sts index` 构建索引");
        return;
    }

    let mut count = 0;
    for path in guard.iter() {
        if count >= limit {
            break;
        }
        if let Some(f) = filter {
            if !path.to_lowercase().contains(&f.to_lowercase()) {
                continue;
            }
        }
        println!("{}", path);
        count += 1;
    }
    eprintln!("[sts] 显示 {} / {} 条", count, guard.len());
}
