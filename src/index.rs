//! Semantic codebase index.
//!
//! Walks the workspace, splits each text file into overlapping line-bounded
//! chunks, computes an Ollama embedding per chunk, and persists everything
//! to `.dm-index/index.json` under the workspace root. The `semantic_search`
//! tool the code writer can invoke reads from this index, so the agent can
//! find the right place to look without needing the full file tree in
//! prompt context.
//!
//! The index is built lazily on the first `search` call after start-up;
//! subsequent file changes invalidate the affected paths so the next
//! search re-embeds them. We don't precompute on every file_change to
//! avoid blasting Ollama for noisy editors.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex as AsyncMutex;

use crate::ollama::OllamaClient;

/// Lines per chunk before overlap. Chunks should fit in the embedding
/// model's context window with comfortable margin.
const CHUNK_LINES: usize = 80;
/// Overlap between consecutive chunks so symbol references near a
/// boundary are still discoverable from either side.
const CHUNK_OVERLAP: usize = 20;
/// Skip files larger than this (likely generated, vendored, or binary).
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Cap chunks per file so a single large source file can't dominate the index.
const MAX_CHUNKS_PER_FILE: usize = 60;
/// Cap total entries per workspace; the index is in-memory.
const MAX_TOTAL_ENTRIES: usize = 8_000;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    ".gradle",
    ".idea",
    ".vscode",
    "out",
    ".dm-index",
];

/// Extensions we consider worth embedding. Anything else is skipped.
const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "scala", "swift",
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "rb", "php", "cs", "fs", "fsi", "elm", "ex", "exs",
    "clj", "cljs", "edn", "lua", "pl", "pm", "ml", "mli", "hs", "lhs", "lean", "zig", "v", "sv",
    "html", "htm", "css", "scss", "sass", "less", "vue", "svelte", "astro", "json", "jsonc", "json5",
    "yaml", "yml", "toml", "ini", "cfg", "conf", "env", "md", "mdx", "rst", "txt", "sh", "bash",
    "zsh", "fish", "bat", "ps1", "psm1", "psd1", "sql", "graphql", "gql", "proto", "thrift", "dot",
    "tex", "r", "jl", "ipynb",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedIndex {
    /// Embedding model used to populate the entries; mismatch invalidates.
    pub model: String,
    /// Workspace root the entries are relative to.
    pub root: String,
    pub entries: Vec<IndexEntry>,
    /// path → mtime in epoch seconds, so we can rebuild only stale chunks.
    #[serde(default)]
    pub mtimes: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub snippet: String,
}

/// Thread-safe semantic index. `Clone` is cheap (it shares state).
#[derive(Clone)]
pub struct SemanticIndex {
    inner: Arc<IndexInner>,
}

struct IndexInner {
    /// Persisted state behind a sync mutex (writes are quick; serializing
    /// across the runtime is fine).
    persisted: Mutex<PersistedIndex>,
    /// Path on disk the index serialises to.
    persistence_path: Mutex<Option<PathBuf>>,
    /// Serialise concurrent build/refresh operations.
    build_lock: AsyncMutex<()>,
    /// Paths the file watcher invalidated since the last refresh.
    dirty_paths: Mutex<HashSet<String>>,
}

impl Default for SemanticIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticIndex {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(IndexInner {
                persisted: Mutex::new(PersistedIndex::default()),
                persistence_path: Mutex::new(None),
                build_lock: AsyncMutex::new(()),
                dirty_paths: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn entry_count(&self) -> usize {
        self.inner.persisted.lock().entries.len()
    }

    pub fn invalidate_path(&self, path: &str) {
        let normalized = normalize_path(path);
        self.inner.dirty_paths.lock().insert(normalized);
    }

    /// Re-build (or refresh) the index for `workspace_root`. Walks the
    /// tree, embeds each new/changed chunk, and persists to disk. Cheap
    /// no-op if the workspace is fully up to date.
    pub async fn ensure_built(
        &self,
        ollama: &OllamaClient,
        model: &str,
        workspace_root: &Path,
    ) -> Result<()> {
        let _guard = self.inner.build_lock.lock().await;

        // Load from disk on first build, if available.
        let persistence_path = ensure_index_dir(workspace_root)?;
        {
            let mut guard = self.inner.persistence_path.lock();
            *guard = Some(persistence_path.clone());
        }
        if self.entry_count() == 0 {
            if let Ok(loaded) = load_persisted(&persistence_path).await {
                let model_match = loaded.model == model;
                let root_match = loaded.root == workspace_root.display().to_string();
                if model_match && root_match {
                    *self.inner.persisted.lock() = loaded;
                }
            }
        }

        let dirty: HashSet<String> = std::mem::take(&mut *self.inner.dirty_paths.lock());

        let files = walk_workspace_files(workspace_root).await;
        let mut new_entries: Vec<IndexEntry> = Vec::new();
        let mut new_mtimes: HashMap<String, u64> = HashMap::new();

        // Snapshot what we have so we don't hold the lock across awaits.
        let (existing_entries, existing_mtimes, existing_model_root_match) = {
            let p = self.inner.persisted.lock();
            (
                p.entries.clone(),
                p.mtimes.clone(),
                p.model == model && p.root == workspace_root.display().to_string(),
            )
        };

        let mut keep_entries: Vec<IndexEntry> = if existing_model_root_match {
            existing_entries
        } else {
            Vec::new()
        };
        if !existing_model_root_match {
            tracing::info!("semantic index: model or root changed, full rebuild");
        }

        // Drop entries for files that were deleted from the tree.
        let live_files: HashSet<String> = files.iter().cloned().collect();
        keep_entries.retain(|e| live_files.contains(&e.path));

        let mut indexed_files = 0usize;
        for rel in files {
            if new_entries.len() + keep_entries.len() >= MAX_TOTAL_ENTRIES {
                break;
            }
            let abs = workspace_root.join(&rel);
            let metadata = match fs::metadata(&abs).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let prev_mtime = existing_mtimes.get(&rel).copied().unwrap_or(0);
            let force = dirty.contains(&rel) || !existing_model_root_match;
            let unchanged = !force && mtime == prev_mtime && prev_mtime != 0;
            if unchanged {
                new_mtimes.insert(rel.clone(), mtime);
                continue;
            }

            // Re-index this path: drop its existing entries, embed fresh chunks.
            keep_entries.retain(|e| e.path != rel);
            let content = match fs::read_to_string(&abs).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            for chunk in chunk_file(&content) {
                let prompt = format!("File: {rel}\n{}", chunk.text);
                let embedding = match ollama.embed(model, &prompt).await {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("embedding `{rel}` failed: {err}");
                        // Stop trying — Ollama probably doesn't have the model.
                        return Err(anyhow!(
                            "embedding model `{model}` unavailable: {err}. Pull it with `ollama pull {}`.",
                            model.split(':').next().unwrap_or(model)
                        ));
                    }
                };
                new_entries.push(IndexEntry {
                    path: rel.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    content: chunk.text,
                    embedding,
                });
                if new_entries.len() + keep_entries.len() >= MAX_TOTAL_ENTRIES {
                    break;
                }
            }
            new_mtimes.insert(rel.clone(), mtime);
            indexed_files += 1;
        }

        // Compose final state.
        let mut merged_mtimes: HashMap<String, u64> = existing_mtimes;
        for (k, v) in new_mtimes {
            merged_mtimes.insert(k, v);
        }
        // Drop mtimes for files no longer present so the map can't grow forever.
        merged_mtimes.retain(|k, _| live_files.contains(k));

        let mut final_entries = keep_entries;
        final_entries.extend(new_entries);

        let persisted = PersistedIndex {
            model: model.to_string(),
            root: workspace_root.display().to_string(),
            entries: final_entries,
            mtimes: merged_mtimes,
        };
        if let Err(err) = save_persisted(&persistence_path, &persisted).await {
            tracing::warn!("failed to persist semantic index: {err}");
        }
        let entry_total = persisted.entries.len();
        *self.inner.persisted.lock() = persisted;
        tracing::info!(
            "semantic index: {indexed_files} file(s) re-embedded, {entry_total} chunk(s) total"
        );
        Ok(())
    }

    /// Run a query against the index. Returns the top-K hits sorted by
    /// cosine similarity (descending). If the index is empty and we have
    /// a workspace root, callers may want to call `ensure_built` first.
    pub async fn search(
        &self,
        ollama: &OllamaClient,
        model: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Err(anyhow!("query is empty"));
        }
        let query_embedding = ollama
            .embed(model, query)
            .await
            .with_context(|| format!("embedding query with model {model}"))?;

        let entries = self.inner.persisted.lock().entries.clone();
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored: Vec<SearchHit> = entries
            .into_iter()
            .map(|e| {
                let score = cosine_similarity(&query_embedding, &e.embedding);
                let snippet = e
                    .content
                    .lines()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("\n");
                SearchHit {
                    path: e.path,
                    start_line: e.start_line,
                    end_line: e.end_line,
                    score,
                    snippet,
                }
            })
            .collect();
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k.max(1));
        Ok(scored)
    }
}

#[derive(Debug)]
struct Chunk {
    start_line: usize,
    end_line: usize,
    text: String,
}

fn chunk_file(content: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < lines.len() && chunks.len() < MAX_CHUNKS_PER_FILE {
        let end = (start + CHUNK_LINES).min(lines.len());
        let text = lines[start..end].join("\n");
        chunks.push(Chunk {
            start_line: start + 1,
            end_line: end,
            text,
        });
        if end == lines.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
    }
    chunks
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..len {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom < 1e-9 {
        0.0
    } else {
        dot / denom
    }
}

fn ensure_index_dir(workspace_root: &Path) -> Result<PathBuf> {
    let dir = workspace_root.join(".dm-index");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating index dir {}", dir.display()))?;
    Ok(dir.join("index.json"))
}

async fn load_persisted(path: &Path) -> Result<PersistedIndex> {
    let bytes = fs::read(path).await?;
    let parsed: PersistedIndex = serde_json::from_slice(&bytes)?;
    Ok(parsed)
}

async fn save_persisted(path: &Path, persisted: &PersistedIndex) -> Result<()> {
    let bytes = serde_json::to_vec(persisted)?;
    fs::write(path, bytes).await?;
    Ok(())
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/").trim_start_matches("./").trim_start_matches('/').to_string()
}

async fn walk_workspace_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut reader = match fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = reader.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if !has_text_extension(&path) {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel_s = rel.to_string_lossy().replace('\\', "/");
                    if !rel_s.is_empty() {
                        out.push(rel_s);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn has_text_extension(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => TEXT_EXTENSIONS.iter().any(|t| t.eq_ignore_ascii_case(ext)),
        None => {
            // Allow common extensionless config files.
            matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("Makefile" | "Dockerfile" | "Procfile" | "Justfile" | ".env")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basic() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs()) < 1e-6);
        assert!((cosine_similarity(&[1.0, 1.0], &[1.0, 1.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_zero_vectors() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn chunking_one_chunk_for_short_files() {
        let chunks = chunk_file("line1\nline2\nline3\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn chunking_overlaps_long_files() {
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("line {i}\n"));
        }
        let chunks = chunk_file(&content);
        assert!(chunks.len() >= 2);
        // Each chunk should have at most CHUNK_LINES lines.
        for c in &chunks {
            assert!(c.end_line - c.start_line < CHUNK_LINES);
        }
        // Consecutive chunks should overlap (start of next < end of prev).
        for win in chunks.windows(2) {
            assert!(win[1].start_line <= win[0].end_line);
        }
    }

    #[test]
    fn text_extension_allowlist() {
        assert!(has_text_extension(Path::new("foo/bar.rs")));
        assert!(has_text_extension(Path::new("baz.html")));
        assert!(!has_text_extension(Path::new("photo.png")));
        assert!(has_text_extension(Path::new("Makefile")));
    }

    #[test]
    fn search_returns_top_k_sorted() {
        let index = SemanticIndex::new();
        {
            let mut p = index.inner.persisted.lock();
            p.model = "test".into();
            p.entries = vec![
                IndexEntry {
                    path: "a.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    content: "alpha".into(),
                    embedding: vec![1.0, 0.0, 0.0],
                },
                IndexEntry {
                    path: "b.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    content: "beta".into(),
                    embedding: vec![0.0, 1.0, 0.0],
                },
                IndexEntry {
                    path: "c.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    content: "gamma".into(),
                    embedding: vec![0.9, 0.1, 0.0],
                },
            ];
        }
        // Compute cosine against [1,0,0]: a→1, c→~0.994, b→0
        let query = vec![1.0, 0.0, 0.0];
        let entries = index.inner.persisted.lock().entries.clone();
        let mut scored: Vec<(String, f32)> = entries
            .iter()
            .map(|e| (e.path.clone(), cosine_similarity(&query, &e.embedding)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        assert_eq!(scored[0].0, "a.rs");
        assert_eq!(scored[1].0, "c.rs");
        assert_eq!(scored[2].0, "b.rs");
    }
}
