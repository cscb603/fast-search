//! MCP HTTP 服务（给 AI / Agent 用的「另一张脸」）。
//! 复用 core_lib::mcp 基座：`/health`、`/tools`、`/` 壳层自动生成，
//! 业务只挂 `POST /search`、`POST /content`、`POST /thumbnail` 三个 handler。
//!
//! 端口策略（§2.6.4）：默认 9877（禁用 9876=sts-x），占用则 +1 探测到 9897；
//! 支持 `STS_MCP_PORT` 强制指定；实际端口写 `mcp_port` 文件并回传 GUI 状态栏。
//!
//! 双模式：MCP 默认 `human_mode=false`（AI 全量，不过滤不降级）；
//! 工具入参 `human_filter=true` 时复用人类模式过滤（给人看的结果）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use core_lib::mcp::axum::{extract::State, routing::post, Json, Router};
use core_lib::mcp::{McpServer, Tool};
use serde::Deserialize;
use serde_json::json;
use sts_core::{ContentSearchParams, GlobalIndex};

const DEFAULT_PORT: u16 = 9877;
const MAX_PROBE: u16 = 20; // 9877..=9897

#[derive(Clone)]
struct McpState {
    index: GlobalIndex,
    mapping: Arc<Mutex<HashMap<String, String>>>,
}

fn default_all() -> String {
    "all".to_string()
}
fn default_max_results() -> usize {
    50
}
fn default_thumb_size() -> u32 {
    128
}

// ---------------- 文件名搜索 ----------------

#[derive(Deserialize)]
struct SearchReq {
    keyword: String,
    #[serde(default = "default_all", alias = "filterType")]
    filter_type: String,
    /// true = 复用人类过滤（去系统垃圾 + 代码降级）；默认 false = AI 全量
    #[serde(default)]
    human_filter: bool,
    #[serde(default)]
    limit: Option<usize>,
}

async fn handle_search(
    State(st): State<McpState>,
    Json(req): Json<SearchReq>,
) -> Json<serde_json::Value> {
    let mapping = st.mapping.lock().unwrap().clone();
    let empty_clicks: HashMap<String, u32> = HashMap::new();

    // 空关键词时返回最近文件（与 GUI 一致）
    let results = if req.keyword.trim().is_empty() {
        sts_core::recent_files(&req.filter_type, req.limit.unwrap_or(100)).await
    } else {
        sts_core::search_files(
            &req.keyword,
            &req.filter_type,
            &st.index,
            &empty_clicks,
            &mapping,
            req.human_filter,
        )
        .await
    };
    let take = req.limit.unwrap_or(100);
    let items: Vec<serde_json::Value> = results
        .into_iter()
        .take(take)
        .map(|r| json!({ "name": r.name, "path": r.path }))
        .collect();
    Json(json!({ "count": items.len(), "results": items }))
}

// ---------------- 内容搜索 ----------------

#[derive(Deserialize)]
struct ContentReq {
    keyword: String,
    #[serde(default)]
    path: String,
    #[serde(default = "default_all")]
    filter_type: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

async fn handle_content(Json(req): Json<ContentReq>) -> Json<serde_json::Value> {
    let params = ContentSearchParams {
        keyword: req.keyword,
        path: req.path,
        filter_type: req.filter_type,
        max_results: req.max_results,
    };
    match sts_core::search_content(params).await {
        Ok(rs) => {
            let items: Vec<serde_json::Value> = rs
                .into_iter()
                .map(|r| json!({ "path": r.path, "name": r.name, "line": r.line_number, "content": r.line_content }))
                .collect();
            Json(json!({ "count": items.len(), "results": items }))
        }
        Err(e) => Json(json!({ "error": e })),
    }
}

// ---------------- 缩略图 ----------------

#[derive(Deserialize)]
struct ThumbReq {
    path: String,
    #[serde(default = "default_thumb_size")]
    size: u32,
}

async fn handle_thumbnail(
    State(st): State<McpState>,
    Json(req): Json<ThumbReq>,
) -> Json<serde_json::Value> {
    let index = st.index.clone();
    // qlmanage 阻塞，放阻塞线程池
    let uri = core_lib::mcp::tokio::task::spawn_blocking(move || {
        index.get_thumbnail(&req.path, req.size)
    })
    .await
    .ok()
    .flatten();
    Json(json!({ "thumbnail": uri }))
}

// ---------------- 端口探测 & 启动 ----------------

fn mcp_port_file() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let dir = PathBuf::from(home).join("Library/Caches/com.xtap.search");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("mcp_port")
}

/// 探测可用端口：STS_MCP_PORT 优先，否则 9877 起 +1 探测到 9897。
fn probe_port() -> Option<u16> {
    if let Ok(p) = std::env::var("STS_MCP_PORT") {
        match p.parse::<u16>() {
            Ok(port) if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() => {
                return Some(port)
            }
            Ok(port) => {
                eprintln!("[sts] STS_MCP_PORT={} 被占用，MCP 未启动", port);
                return None;
            }
            Err(_) => eprintln!("[sts] STS_MCP_PORT 非法，回退默认探测"),
        }
    }
    (DEFAULT_PORT..=DEFAULT_PORT + MAX_PROBE)
        .find(|&port| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok())
}

/// 构建 MCP Server（含三工具声明 + 业务路由）。抽出以便测试其 into_router()。
fn build_server(state: McpState) -> McpServer {
    let biz = Router::new()
        .route("/search", post(handle_search))
        .route("/content", post(handle_content))
        .route("/thumbnail", post(handle_thumbnail))
        .with_state(state);

    let search_schema = json!({
        "type": "object",
        "properties": {
            "keyword": { "type": "string", "description": "搜索关键词（支持中文分词、别名、缩写、编辑距离纠错）" },
            "filter_type": { "type": "string", "description": "类型过滤：all/image/video/audio/doc/code 等", "default": "all" },
            "human_filter": { "type": "boolean", "description": "true=复用人类模式过滤(去系统垃圾+代码降级)；默认 false=AI 全量", "default": false },
            "limit": { "type": "integer", "description": "返回条数上限，默认 100", "default": 100 }
        },
        "required": ["keyword"]
    });
    let content_schema = json!({
        "type": "object",
        "properties": {
            "keyword": { "type": "string", "description": "文件内容检索词（rg 后端）" },
            "path": { "type": "string", "description": "限定搜索目录，空=Desktop+Downloads+Documents+/Volumes（外接盘自动扫）；用 in:/自定义路径 指定其他位置" },
            "filter_type": { "type": "string", "description": "类型过滤", "default": "all" },
            "max_results": { "type": "integer", "description": "最大结果数", "default": 50 }
        },
        "required": ["keyword"]
    });
    let thumb_schema = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "文件绝对路径" },
            "size": { "type": "integer", "description": "缩略图边长像素，默认 128", "default": 128 }
        },
        "required": ["path"]
    });

    let instructions = "星TAP 极速搜索 MCP：文件名搜索(search_files) / 内容搜索(search_content) / 缩略图(get_thumbnail)。\
        默认 AI 全量模式(不过滤系统文件、不降级代码)；search_files 传 human_filter=true 可复用人类过滤。";

    McpServer::new("star-tap-search", env!("CARGO_PKG_VERSION"), instructions)
        .tool(Tool::new(
            "search_files",
            "按文件名搜索（BM25 中文分词 + 别名/缩写/编辑距离扩展）",
            search_schema,
        ))
        .tool(Tool::new(
            "search_content",
            "按文件内容搜索（ripgrep 后端）",
            content_schema,
        ))
        .tool(Tool::new(
            "get_thumbnail",
            "生成文件缩略图，返回 base64 PNG data URI",
            thumb_schema,
        ))
        .merge(biz)
}

/// 启动 MCP 服务；返回实际绑定端口（供 GUI 状态栏显示）。失败返回 None。
pub fn start(index: GlobalIndex, mapping: Arc<Mutex<HashMap<String, String>>>) -> Option<u16> {
    let port = probe_port()?;
    let _ = std::fs::write(mcp_port_file(), port.to_string());
    eprintln!("[sts] MCP 服务端口: {}（写入 mcp_port）", port);

    let server = build_server(McpState { index, mapping });

    tauri::async_runtime::spawn(async move {
        if let Err(e) = server.serve("127.0.0.1", port).await {
            eprintln!("[sts] MCP 服务异常退出: {}", e);
        }
    });

    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_lib::mcp::tokio;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn test_probe_port_in_range() {
        // 未设 STS_MCP_PORT 时应能在 9877..=9897 找到空闲端口（CI/本机通常空闲）
        std::env::remove_var("STS_MCP_PORT");
        if let Some(p) = probe_port() {
            assert!((DEFAULT_PORT..=DEFAULT_PORT + MAX_PROBE).contains(&p));
        }
    }

    #[tokio::test]
    async fn test_tools_endpoint() {
        // 用基座 into_router() 起服务，raw TCP 请求 /tools 验证三工具已暴露
        let state = McpState {
            index: GlobalIndex::empty(),
            mapping: Arc::new(Mutex::new(HashMap::new())),
        };
        let router = build_server(state).into_router();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = core_lib::mcp::axum::serve(listener, router).await;
        });
        // 给服务器一点启动时间
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /tools HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            addr
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);

        assert!(resp.contains("200 OK"), "应返回 200，实际: {}", resp);
        assert!(resp.contains("search_files"), "工具列表应含 search_files");
        assert!(
            resp.contains("search_content"),
            "工具列表应含 search_content"
        );
        assert!(resp.contains("get_thumbnail"), "工具列表应含 get_thumbnail");
    }
}
