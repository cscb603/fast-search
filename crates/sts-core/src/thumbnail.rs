//! 缩略图生成（macOS qlmanage）+ 内存 LRU 缓存。
//! 为图片/视频/PDF/设计文件等提供 128px 缩略图，返回 base64 PNG data URI，
//! 前端可直接 `<img src=...>` 渲染。生成失败返回 None，不影响主链路。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// 支持生成缩略图的扩展名（其余类型前端用图标兜底）
const THUMBNAIL_EXTS: &[&str] = &[
    // 图片
    "jpg", "jpeg", "png", "gif", "bmp", "tiff", "tif", "webp", "heic", "heif", "raw", "cr2", "nef",
    "arw", "dng", "psd", "ai", "svg", "icns", // 视频
    "mp4", "mov", "avi", "mkv", "m4v", "webm", // 文档
    "pdf", "key", "pages", "numbers", "ppt", "pptx", "doc", "docx",
];

/// 判断路径是否可生成缩略图
pub fn can_thumbnail(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => THUMBNAIL_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

fn thumb_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let dir = PathBuf::from(home).join("Library/Caches/com.xtap.search/thumbnails");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 单条缓存值：base64 PNG data URI
type DataUri = String;

/// 简易 LRU：HashMap 存值 + Vec 记录访问顺序（容量小，Vec 足够快）
pub struct ThumbnailCache {
    inner: Mutex<LruInner>,
    capacity: usize,
}

struct LruInner {
    map: HashMap<String, DataUri>,
    order: Vec<String>, // 末尾 = 最近使用
}

impl ThumbnailCache {
    pub fn new(capacity: usize) -> Self {
        ThumbnailCache {
            inner: Mutex::new(LruInner {
                map: HashMap::new(),
                order: Vec::new(),
            }),
            capacity,
        }
    }

    /// 缓存键：路径 + mtime（文件变更后自动失效）
    fn cache_key(path: &str) -> String {
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{}:{}", path, mtime)
    }

    /// 获取缩略图 data URI（命中缓存直接返回，否则用 qlmanage 生成）
    pub fn get(&self, path: &str, size: u32) -> Option<DataUri> {
        if !can_thumbnail(path) || !std::path::Path::new(path).exists() {
            return None;
        }
        let key = Self::cache_key(path);

        // 命中缓存：更新 LRU 顺序
        {
            let mut g = self.inner.lock().unwrap();
            if g.map.contains_key(&key) {
                g.order.retain(|k| k != &key);
                g.order.push(key.clone());
                return g.map.get(&key).cloned();
            }
        }

        // 未命中：生成
        let uri = generate_thumbnail(path, size)?;

        let mut g = self.inner.lock().unwrap();
        g.map.insert(key.clone(), uri.clone());
        g.order.push(key);
        // 逐出最旧
        while g.order.len() > self.capacity {
            let oldest = g.order.remove(0);
            g.map.remove(&oldest);
        }
        Some(uri)
    }
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self::new(500)
    }
}

/// 进程内单调计数器，保证并发生成时输出子目录唯一（避免同名文件互相覆盖）
static THUMB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 调用 qlmanage 生成缩略图 PNG，读回并编码为 base64 data URI
pub fn generate_thumbnail(path: &str, size: u32) -> Option<DataUri> {
    // 每次生成用唯一临时子目录，避免不同目录下同名文件（如两个 photo.jpg）并发覆盖
    let seq = THUMB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out_dir = thumb_cache_dir().join(format!("gen-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&out_dir).ok()?;

    // qlmanage -t -s <size> -o <out_dir> <file>
    let result = (|| {
        let status = std::process::Command::new("qlmanage")
            .arg("-t")
            .arg("-s")
            .arg(size.to_string())
            .arg("-o")
            .arg(&out_dir)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        // qlmanage 输出文件名 = <原文件名>.png
        let file_name = std::path::Path::new(path).file_name()?.to_string_lossy();
        let png_path = out_dir.join(format!("{}.png", file_name));
        if !png_path.exists() {
            return None;
        }
        let bytes = std::fs::read(&png_path).ok()?;
        Some(format!("data:image/png;base64,{}", base64_encode(&bytes)))
    })();

    // 清理临时子目录（无论成败），缩略图已进内存 LRU，避免磁盘堆积
    let _ = std::fs::remove_dir_all(&out_dir);
    result
}

/// 轻量 base64 编码（标准表，无外部依赖）
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(TABLE[b0 >> 2] as char);
        out.push(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_can_thumbnail() {
        assert!(can_thumbnail("/x/a.jpg"));
        assert!(can_thumbnail("/x/A.PNG"));
        assert!(can_thumbnail("/x/movie.mp4"));
        assert!(can_thumbnail("/x/doc.pdf"));
        assert!(!can_thumbnail("/x/code.rs"));
        assert!(!can_thumbnail("/x/noext"));
    }

    #[test]
    fn test_lru_evict() {
        let cache = ThumbnailCache::new(2);
        {
            let mut g = cache.inner.lock().unwrap();
            g.map.insert("a".into(), "ua".into());
            g.order.push("a".into());
            g.map.insert("b".into(), "ub".into());
            g.order.push("b".into());
        }
        // 手动模拟第三个插入触发逐出
        {
            let mut g = cache.inner.lock().unwrap();
            g.map.insert("c".into(), "uc".into());
            g.order.push("c".into());
            while g.order.len() > cache.capacity {
                let oldest = g.order.remove(0);
                g.map.remove(&oldest);
            }
            assert!(!g.map.contains_key("a"));
            assert!(g.map.contains_key("b"));
            assert!(g.map.contains_key("c"));
        }
    }
}
