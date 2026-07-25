//! 星TAP 极速搜索 CLI — 轻量独立版本，不依赖 Tauri。
//! 基于 macOS Spotlight(mdfind) 即时搜索，毫秒级、覆盖外接盘。

use clap::{Parser, Subcommand};
use sts_core::GlobalIndex;

#[derive(Parser)]
#[command(name = "sts", about = "星TAP 极速搜索 CLI（mdfind 即时搜索）", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 按文件名/内容搜索（Spotlight 毫秒级）
    Search {
        /// 搜索关键词
        keyword: String,
        /// 类型过滤：all/image/video/audio/doc/folder/app
        #[arg(short = 't', long, default_value = "all")]
        filter_type: String,
        /// 返回条数上限
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: usize,
        /// 以 JSON 格式输出
        #[arg(short = 'j', long)]
        json: bool,
        /// 显示耗时
        #[arg(short = 'w', long)]
        time: bool,
    },
    /// 列出最近修改的文件（空搜场景）
    Recent {
        /// 类型过滤
        #[arg(short = 't', long, default_value = "all")]
        filter_type: String,
        /// 返回条数上限
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// 以 JSON 格式输出
        #[arg(short = 'j', long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let index = GlobalIndex::new();

    match cli.command {
        Commands::Search {
            keyword,
            filter_type,
            limit,
            json,
            time,
        } => {
            let start = std::time::Instant::now();
            let results = index.search_files(&keyword, &filter_type).await;
            let elapsed_ms = start.elapsed().as_millis() as u64;

            let taken: Vec<_> = results.into_iter().take(limit).collect();
            if json {
                let items: Vec<serde_json::Value> = taken
                    .iter()
                    .map(|r| {
                        let mut obj = serde_json::json!({ "name": r.name, "path": r.path });
                        if time || elapsed_ms > 100 {
                            obj["elapsed_ms"] = serde_json::json!(elapsed_ms);
                        }
                        obj
                    })
                    .collect();
                let mut wrapper = serde_json::json!({ "results": items, "total": taken.len() });
                if time || elapsed_ms > 100 {
                    wrapper["elapsed_ms"] = serde_json::json!(elapsed_ms);
                }
                println!("{}", wrapper);
            } else {
                for r in &taken {
                    println!("{} -> {}", r.name, r.path);
                }
                eprintln!("[sts] 命中 {} 条 | 耗时 {}ms", taken.len(), elapsed_ms);
            }
        }
        Commands::Recent {
            filter_type,
            limit,
            json,
        } => {
            let start = std::time::Instant::now();
            let results = index.recent_files(&filter_type, limit).await;
            let elapsed_ms = start.elapsed().as_millis() as u64;

            if json {
                let items: Vec<serde_json::Value> = results
                    .iter()
                    .map(|r| serde_json::json!({ "name": r.name, "path": r.path }))
                    .collect();
                let mut wrapper = serde_json::json!({ "results": items, "total": results.len() });
                wrapper["elapsed_ms"] = serde_json::json!(elapsed_ms);
                println!("{}", wrapper);
            } else {
                for r in &results {
                    println!("{} -> {}", r.name, r.path);
                }
                eprintln!("[sts] 最近文件 {} 条 | 耗时 {}ms", results.len(), elapsed_ms);
            }
        }
    }
}
