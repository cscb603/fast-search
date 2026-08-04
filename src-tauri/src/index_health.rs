// v4.2: 索引智能管家（健康检查 + 智能避开编译产物）
// 纯系统命令（mdutil/find/du/open）+ 本地 JSON 状态，零新依赖（serde_json/dirs 已存在）。
// 约束：系统 Spotlight 索引是私有黑盒，不支持条目级删除；
// 系统排除列表(VolumeConfiguration.plist)被 SIP 保护不可读 → 用 app 自维护「已处理清单」JSON 绕开。

use serde::Serialize;
use std::collections::HashSet;
use std::process::Command;

#[derive(Serialize, Clone)]
pub struct DevDirSuggestion {
    pub path: String,
    pub size_bytes: u64,
    pub kind: String,
}

#[derive(Serialize, Clone)]
pub struct IndexHealth {
    pub indexing_enabled: bool,
    pub rebuilding: bool,
    pub dev_dirs: Vec<DevDirSuggestion>,
}

const DEV_DIR_KINDS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "build",
    "DerivedData",
    ".next",
    ".gradle",
];

const MIN_DEV_DIR_BYTES: u64 = 500 * 1024 * 1024; // 500MB 阈值

/// 解析 `mdutil -s /` 输出 → (enabled, rebuilding)
/// 稳定态输出 "Indexing enabled"（无尾点）；重建中带尾点 "Indexing enabled."
pub fn check_spotlight_status() -> (bool, bool) {
    let out = Command::new("/usr/bin/mdutil").args(["-s", "/"]).output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let enabled = s.contains("Indexing enabled") && !s.contains("Indexing disabled");
            let rebuilding =
                enabled && s.trim_end().ends_with('.') && !s.contains("Indexing disabled");
            (enabled, rebuilding)
        }
        Err(_) => (true, false), // 命令失败默认认为正常，避免误报
    }
}

/// 计算目录大小（优先 du -sk，失败回退递归）
fn dir_size_bytes(path: &str) -> u64 {
    if let Ok(o) = Command::new("/usr/bin/du").args(["-sk", path]).output() {
        let s = String::from_utf8_lossy(&o.stdout);
        if let Some(first) = s.split_whitespace().next() {
            if let Ok(kb) = first.parse::<u64>() {
                return kb * 1024;
            }
        }
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_dir() {
                    total += dir_size_bytes(&e.path().to_string_lossy());
                } else {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// 扫描用户目录下超阈值的开发产物目录（过滤已处理）
pub fn detect_dev_dirs(min_bytes: u64, home: &str) -> Vec<DevDirSuggestion> {
    let handled = load_handled();
    let mut found = Vec::new();
    for kind in DEV_DIR_KINDS {
        let out = Command::new("/usr/bin/find")
            .args([home, "-maxdepth", "6", "-type", "d", "-name", kind])
            .output();
        if let Ok(o) = out {
            for p in String::from_utf8_lossy(&o.stdout).lines() {
                let p = p.trim();
                if p.is_empty() || handled.contains(p) {
                    continue;
                }
                let size = dir_size_bytes(p);
                if size >= min_bytes {
                    found.push(DevDirSuggestion {
                        path: p.to_string(),
                        size_bytes: size,
                        kind: kind.to_string(),
                    });
                }
            }
        }
    }
    found.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    found
}

/// 打开系统设置 → 聚焦面板（用户可把巨无霸目录拖入隐私排除）
pub fn open_spotlight_prefs() {
    let _ = Command::new("/usr/bin/open")
        .args(["x-apple.systempreferences:com.apple.Spotlight"])
        .status();
}

// ---- 本地「已处理清单」JSON（绕开 SIP，不读系统排除列表）----

fn handled_path() -> Option<std::path::PathBuf> {
    let mut p = dirs::cache_dir()?;
    p.push("com.xtap.search");
    std::fs::create_dir_all(&p).ok()?;
    p.push("index_health_handled.json");
    Some(p)
}

pub fn load_handled() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(p) = handled_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&s) {
                set.extend(v);
            }
        }
    }
    set
}

pub fn mark_handled(path: &str) {
    let mut set = load_handled();
    set.insert(path.to_string());
    if let Some(p) = handled_path() {
        if let Ok(list) = serde_json::to_string(&set.iter().collect::<Vec<&String>>()) {
            let _ = std::fs::write(&p, list);
        }
    }
}

/// 组合：系统状态 + 开发目录扫描（过滤已处理）
pub fn collect_health(home: &str) -> IndexHealth {
    let (enabled, rebuilding) = check_spotlight_status();
    let dev_dirs = detect_dev_dirs(MIN_DEV_DIR_BYTES, home);
    IndexHealth {
        indexing_enabled: enabled,
        rebuilding,
        dev_dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_spotlight_status_command() {
        // 本机实测：系统盘索引应已开启（macOS 项目，依赖真实 mdutil）
        let (enabled, _rebuilding) = check_spotlight_status();
        assert!(enabled, "本机系统盘索引应已开启");
    }

    #[test]
    fn test_detect_dev_dirs_finds_large_target() {
        let tmp = std::env::temp_dir().join(format!("idx_health_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("myproj/target"));
        let _ = std::fs::write(
            tmp.join("myproj/target/big.bin"),
            vec![0u8; 600 * 1024 * 1024],
        );
        let home = tmp.to_string_lossy().to_string();
        let dirs_found = detect_dev_dirs(MIN_DEV_DIR_BYTES, &home);
        let hit = dirs_found
            .iter()
            .any(|d| d.kind == "target" && d.path.contains("myproj/target"));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(hit, "应检测到超过 500MB 的 myproj/target");
    }
}
