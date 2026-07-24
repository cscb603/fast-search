//! 自研模糊匹配引擎（前缀树 + 编辑距离 + 别名/缩写）
//! 为搜索提供四类增强召回：
//!
//! 1. 别名扩展（含中文→英文，如 照片→photo）
//! 2. 缩写/前缀匹配（如 ps→photoshop、xcod→xcode）
//! 3. 编辑距离纠错（如 photoshp→photoshop）
//! 4. 名称前缀匹配（如 phot→photos）
//!
//! 这是 rg / Spotlight / BM25 之上的「增强层」，构建失败不影响主链路。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::build_alias_mapping;

/// 编辑距离（Levenshtein），用于纠错（photoshp → Photoshop）
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// 前缀树：按字符建树，叶子保存候选值（目标词），支持前缀快速收集
struct PrefixTrie {
    children: HashMap<char, PrefixTrie>,
    values: Vec<String>,
}

impl PrefixTrie {
    fn new() -> Self {
        PrefixTrie {
            children: HashMap::new(),
            values: Vec::new(),
        }
    }

    /// 沿 `path` 建树，并在叶子记录 `value`
    fn insert(&mut self, path: &str, value: &str) {
        let mut node = self;
        for ch in path.chars() {
            node = node.children.entry(ch).or_insert_with(PrefixTrie::new);
        }
        if !node.values.contains(&value.to_string()) {
            node.values.push(value.to_string());
        }
    }

    /// 收集以 `prefix` 为前缀的所有值（限 `limit` 个）
    fn collect(&self, prefix: &str, limit: usize) -> Vec<String> {
        let mut node = self;
        for ch in prefix.chars() {
            match node.children.get(&ch) {
                Some(n) => node = n,
                None => return Vec::new(),
            }
        }
        let mut out: Vec<String> = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            for v in &n.values {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            if out.len() >= limit {
                break;
            }
            for c in n.children.values() {
                stack.push(c);
            }
        }
        out.truncate(limit);
        out
    }
}

pub struct FuzzyMatcher {
    /// 别名映射（含中文→英文、缩写、App 名）
    aliases: HashMap<String, String>,
    /// 已知名称词表（小写，有界：别名目标 + /Applications 应用名），用于编辑距离纠错
    vocabulary: Vec<String>,
    /// 首字母缩写树（如 "ps" → "photoshop"）
    acronym_trie: PrefixTrie,
    /// 名称前缀树（用于前缀匹配，如 "phot" → "photos"）
    name_trie: PrefixTrie,
}

impl FuzzyMatcher {
    /// 从文件列表构建模糊匹配索引
    pub fn build_from_paths(paths: &[String]) -> Arc<FuzzyMatcher> {
        // 基础别名 + 中文→英文扩展（覆盖 §3「照片→photo」等）
        let mut aliases = build_alias_mapping();
        let zh_aliases: &[(&str, &str)] = &[
            ("照片", "photo"),
            ("图片", "image"),
            ("图像", "image"),
            ("视频", "video"),
            ("电影", "movie"),
            ("音乐", "music"),
            ("音频", "audio"),
            ("文档", "document"),
            ("文件", "file"),
            ("游戏", "game"),
            ("应用", "app"),
            ("软件", "app"),
            ("浏览器", "browser"),
            ("网页", "web"),
            ("截图", "screenshot"),
            ("录音", "record"),
            ("相册", "photos"),
            ("笔记", "note"),
            ("日历", "calendar"),
            ("邮件", "mail"),
            ("地图", "map"),
            ("代码", "code"),
            ("项目", "project"),
            ("设计", "design"),
            ("下载", "download"),
            ("桌面", "desktop"),
            ("工作", "work"),
        ];
        for (k, v) in zh_aliases {
            aliases.entry(k.to_string()).or_insert(v.to_string());
        }

        // 有界词表：别名目标 + /Applications 应用名（编辑距离只在这些上跑，保证速度）
        let mut vocabulary: HashSet<String> = HashSet::new();
        for v in aliases.values() {
            vocabulary.insert(v.to_lowercase());
        }
        if let Ok(entries) = std::fs::read_dir("/Applications") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                let base = name.strip_suffix(".app").unwrap_or(&name);
                vocabulary.insert(base.to_string());
                if let Some((stem, _)) = base.rsplit_once('.') {
                    vocabulary.insert(stem.to_string());
                }
            }
        }

        let mut acronym_trie = PrefixTrie::new();
        let mut name_trie = PrefixTrie::new();

        // 别名键（如 ps / 照片）→ 目标词，进缩写树
        for (k, v) in &aliases {
            acronym_trie.insert(k, v);
        }

        // 应用名 → 缩写（空格/非字母分隔取首字母，如 "visual studio code" → "vsc"）
        for name in &vocabulary {
            name_trie.insert(name, name);
            let spaced: String = name
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().next().unwrap_or(' '))
                .collect::<String>()
                .to_lowercase();
            if spaced.len() >= 2 {
                acronym_trie.insert(&spaced, name);
            }
        }

        // 文件列表 → 名称前缀树（前缀匹配，如 phot → photos）
        for path in paths {
            let name = path.split('/').next_back().unwrap_or(path).to_lowercase();
            name_trie.insert(&name, &name);
            if let Some((stem, _)) = name.rsplit_once('.') {
                name_trie.insert(stem, stem);
            }
        }

        Arc::new(FuzzyMatcher {
            aliases,
            vocabulary: vocabulary.into_iter().collect(),
            acronym_trie,
            name_trie,
        })
    }

    /// 给定查询，返回扩展后的候选搜索词（含原词），用于增强检索召回
    pub fn expand_query(&self, keyword: &str) -> Vec<String> {
        let kw = keyword.to_lowercase();
        let mut seen: HashSet<String> = HashSet::new();
        let mut terms: Vec<String> = Vec::new();
        let mut add = |t: &str| {
            let t = t.trim().to_string();
            if !t.is_empty() && seen.insert(t.clone()) {
                terms.push(t);
            }
        };

        // 0) 原词
        add(&kw);
        // 1) 别名扩展（含中文→英文、缩写）
        if let Some(alias) = self.aliases.get(&kw) {
            add(alias);
        }
        // 2) 缩写 / 前缀树命中（vscode → visual studio code、xcod → xcode；ps/照片 等走别名）
        for cand in self.acronym_trie.collect(&kw, 10) {
            add(&cand);
        }
        // 3) 名称前缀匹配（phot → photos）
        for cand in self.name_trie.collect(&kw, 10) {
            add(&cand);
        }
        // 4) 编辑距离纠错（仅对长度>=4 的 query，阈值 2，避免短词误伤）
        if kw.len() >= 4 {
            for name in &self.vocabulary {
                if levenshtein_distance(&kw, name) <= 2 {
                    add(name);
                }
            }
        }

        terms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein_distance("photoshp", "photoshop"), 1);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("中文", "中文"), 0);
    }

    #[test]
    fn test_prefix_trie() {
        let mut t = PrefixTrie::new();
        t.insert("ps", "photoshop");
        t.insert("pr", "premiere");
        let r = t.collect("p", 10);
        assert!(r.contains(&"photoshop".to_string()));
        assert!(r.contains(&"premiere".to_string()));
        assert!(t.collect("zz", 10).is_empty());
    }

    #[test]
    fn test_expand_alias_zh() {
        let paths = vec![
            "/Applications/Adobe Photoshop 2024.app".to_string(),
            "/Users/me/我的照片photo.jpg".to_string(),
        ];
        let fm = FuzzyMatcher::build_from_paths(&paths);
        let terms = fm.expand_query("照片");
        assert!(
            terms.iter().any(|t| t == "photo"),
            "照片 应扩展出 photo, got {:?}",
            terms
        );
    }

    #[test]
    fn test_expand_edit_distance() {
        let paths = vec!["/Applications/Adobe Photoshop 2024.app".to_string()];
        let fm = FuzzyMatcher::build_from_paths(&paths);
        let terms = fm.expand_query("photoshp");
        assert!(
            terms.iter().any(|t| t == "photoshop"),
            "photoshp 应纠错为 photoshop, got {:?}",
            terms
        );
    }

    #[test]
    fn test_expand_acronym() {
        let paths = vec!["/Applications/Visual Studio Code.app".to_string()];
        let fm = FuzzyMatcher::build_from_paths(&paths);
        let terms = fm.expand_query("vscode");
        assert!(
            terms.iter().any(|t| t.contains("visual studio code")),
            "vscode 应扩展出 visual studio code, got {:?}",
            terms
        );
    }
}
