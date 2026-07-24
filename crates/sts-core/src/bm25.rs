//! BM25 语义索引引擎（tantivy 0.26 + tantivy-jieba 0.20 中文分词）
//! 从 index.cache 重建倒排索引，提供基于 BM25 相关度的文件名搜索增强。
//! BM25 是 rg / Spotlight 的「增强层」，构建失败不影响主搜索链路。

use std::path::PathBuf;
use std::sync::Arc;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::{DocAddress, Index, TantivyDocument};
use tantivy_jieba::JiebaTokenizer;

use crate::{InternalSearchResult, SearchStrategy};

/// BM25 索引存储目录（与 index.cache 同级的 bm25_index/）
pub fn bm25_index_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join("Library/Caches/com.xtap.search/bm25_index")
}

pub struct Bm25Index {
    index: Index,
    name_field: Field,
    path_field: Field,
}

impl Bm25Index {
    /// 创建（或重建）BM25 索引目录并返回句柄。
    /// 会先删除旧目录以保证干净重建（BM25 全量重建，不增量）。
    pub fn create() -> Option<Arc<Bm25Index>> {
        let dir = bm25_index_dir();
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 重建前清空旧索引，避免 segment 累积
        let _ = std::fs::remove_dir_all(&dir);
        // tantivy 0.26 的 Index::create_in_dir 不会自动创建目录，需先建好
        let _ = std::fs::create_dir_all(&dir);

        let mut schema_builder = Schema::builder();
        // 文件名：中文分词索引 + 存储
        let name_field = schema_builder.add_text_field(
            "name",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("jieba")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored(),
        );
        // 路径：仅存储（用于结果回填，不进倒排索引）
        let path_field = schema_builder.add_text_field("path", TextOptions::default().set_stored());
        let schema = schema_builder.build();

        let index = Index::create_in_dir(&dir, schema).ok()?;
        // 注册中文分词器
        index.tokenizers().register("jieba", JiebaTokenizer::new());

        Some(Arc::new(Bm25Index {
            index,
            name_field,
            path_field,
        }))
    }

    /// 从文件列表重建 BM25 索引（文件名 + 路径写入倒排索引）
    pub fn rebuild_from_cache(&self, files: &[String]) -> std::io::Result<()> {
        let mut writer = self
            .index
            .writer(50_000_000)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut added = 0usize;
        for path in files {
            let name = path.split('/').next_back().unwrap_or(path);
            let mut doc = TantivyDocument::new();
            doc.add_text(self.name_field, name);
            doc.add_text(self.path_field, path);
            if writer.add_document(doc).is_ok() {
                added += 1;
            }
        }
        writer
            .commit()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        eprintln!("[sts] BM25 索引构建完成，共 {} 条", added);
        Ok(())
    }

    /// BM25 搜索：返回文件名匹配的内部结果（source="bm25"）
    pub(crate) fn search(
        &self,
        keyword: &str,
        filter_type: &str,
        limit: usize,
    ) -> Vec<InternalSearchResult> {
        let reader = match self.index.reader() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let searcher = reader.searcher();

        // 仅以 name 字段建立查询（path 不进倒排索引）
        let query_parser = QueryParser::for_index(&self.index, vec![self.name_field]);
        let query = match query_parser.parse_query(keyword) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };

        let top_docs: Vec<(f32, DocAddress)> = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .unwrap_or_default();

        let mut results = Vec::new();
        for (_score, addr) in top_docs {
            let retrieved: TantivyDocument = match searcher.doc(addr) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let path = retrieved
                .get_first(self.path_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if path.is_empty() {
                continue;
            }
            let name = path.split('/').next_back().unwrap_or(&path).to_string();
            results.push(InternalSearchResult {
                path,
                name,
                score: 0,
                source: "bm25".to_string(),
            });
        }

        // 类型过滤（BM25 只索引 name/path，需后端按扩展名二次过滤）
        if filter_type != "all" {
            let strategy = SearchStrategy::from_type(filter_type);
            results.retain(|r| strategy.matches_extension(&r.path));
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::Schema;

    /// 验证 tantivy 0.26 修复（create_in_dir 需先建目录）后 BM25 中文分词搜索可用。
    /// 用临时目录隔离，不依赖 HOME/Library/Caches。
    #[test]
    fn test_bm25_chinese_tokenize_search() {
        let dir = std::env::temp_dir().join("sts_bm25_self_test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let mut schema_builder = Schema::builder();
        let name_field = schema_builder.add_text_field(
            "name",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("jieba")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored(),
        );
        let path_field = schema_builder.add_text_field("path", TextOptions::default().set_stored());
        let schema = schema_builder.build();

        let index = Index::create_in_dir(&dir, schema).expect("create_in_dir 应成功（已先建目录）");
        index.tokenizers().register("jieba", JiebaTokenizer::new());

        let files = vec![
            "/Users/xtap/Desktop/合肥照片2026.txt".to_string(),
            "/Users/xtap/Desktop/旅游照片合集.pdf".to_string(),
            "/Users/xtap/Documents/report.docx".to_string(),
        ];
        {
            let mut writer = index.writer(50_000_000).unwrap();
            for p in &files {
                let name = p.split('/').next_back().unwrap();
                let mut doc = TantivyDocument::new();
                doc.add_text(name_field, name);
                doc.add_text(path_field, p);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }

        let bm25 = Bm25Index {
            index,
            name_field,
            path_field,
        };

        // 分词：『合肥』应精确命中名含『合肥照片2026』的文件
        let res = bm25.search("合肥", "all", 10);
        assert!(
            res.iter().any(|r| r.path.contains("合肥照片2026")),
            "BM25 应能用分词搜到 合肥照片2026，实际: {:?}",
            res
        );

        // 分词：『照片』应同时命中多个含该词的文件（jieba 切词）
        let res2 = bm25.search("照片", "all", 10);
        assert!(
            res2.len() >= 2,
            "BM25 应分词命中多个含『照片』文件，实际: {:?}",
            res2
        );
    }
}
