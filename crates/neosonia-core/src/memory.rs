//! 长期记忆：SQLite 存储的事实/偏好/事件/对话记录 + 固定块。
//!
//! 检索用 `LIKE` + 标签 + 重要度 + 时间衰减打分（不依赖 FTS5，避免额外编译特性）。
//! 连接不是 `Sync`，统一用 `parking_lot::Mutex<Connection>` 包住，开启 WAL。
//!
//! 另外提供 4 个内置工具（`memory_remember` / `memory_recall` / `memory_list` /
//! `memory_forget`），由 `AgentEngine::start` 自动注册进 registry。
//!
//! OWNER: core-llm 队友。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Value, json};

use crate::model::{
    AgentEvent, MemoryApi, MemoryItem, MemoryStats, ToolCtx, ToolOutcome, ToolSpec,
};
use crate::registry::{Tool, ToolRegistry};
use crate::util::{now_ms, truncate_chars};

/// `SELECT` 的列顺序必须与 [`row_to_item`] 一致。
const FACT_COLUMNS: &str =
    "id, kind, content, tags, importance, created_at, updated_at, hits, source";

/// 对话记录最多保留多少条（防止库无限膨胀）。
const CONVERSATION_KEEP: i64 = 5000;

/// 候选集上限：先粗筛再精排，避免全表打分。
const CANDIDATE_LIMIT: i64 = 400;

/// 本地 SQLite 记忆库（线程安全）。
pub struct MemoryStore {
    conn: Mutex<Connection>,
    path: std::path::PathBuf,
    /// 事实条数上限，超过后按分数淘汰。默认 5000。
    max_items: AtomicI64,
}

impl MemoryStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_max_items(path, 5000)
    }

    /// 带上限打开（app 传 `settings.memory.max_items`）。
    pub fn open_with_max_items(path: &Path, max_items: i64) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建记忆库目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开内存数据库失败: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .context("设置 SQLite busy_timeout 失败")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("开启 WAL 失败（数据库可能被其它进程占用）")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("设置 synchronous 失败")?;
        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            max_items: AtomicI64::new(max_items.max(1)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS facts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                kind        TEXT    NOT NULL DEFAULT 'fact',
                content     TEXT    NOT NULL,
                tags        TEXT    NOT NULL DEFAULT '[]',
                importance  REAL    NOT NULL DEFAULT 0.5,
                source      TEXT    NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                hits        INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_facts_content ON facts(content);
            CREATE INDEX IF NOT EXISTS idx_facts_updated ON facts(updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_facts_kind ON facts(kind);

            CREATE TABLE IF NOT EXISTS blocks (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                role       TEXT NOT NULL,
                content    TEXT NOT NULL,
                turn_id    TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_conversations_created ON conversations(created_at DESC);
            "#,
        )
        .context("初始化记忆库表结构失败")?;
        conn.pragma_update(None, "user_version", 1)
            .context("写入 schema 版本失败")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 调整事实条数上限（app 热改配置时调用）。
    pub fn set_max_items(&self, max_items: i64) {
        self.max_items.store(max_items.max(1), Ordering::Relaxed);
    }

    pub fn max_items(&self) -> i64 {
        self.max_items.load(Ordering::Relaxed)
    }

    /// 带显式 kind 的写入（工具/agent 内部用；trait 的 `remember` 会自行推断）。
    pub fn remember_kind(
        &self,
        kind: &str,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
    ) -> anyhow::Result<i64> {
        let content = content.trim();
        if content.is_empty() {
            bail!("记忆内容不能为空");
        }
        let kind = normalize_kind(kind);
        let tags = normalize_tags(tags);
        let importance = normalize_importance(importance);
        let tags_json = serde_json::to_string(&tags).context("序列化标签失败")?;
        let source = source.trim();
        let now = now_ms();

        let conn = self.conn.lock();
        let existing: Option<(i64, f64, String)> = conn
            .query_row(
                "SELECT id, importance, tags FROM facts WHERE content = ?1",
                [content],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .context("查询重复记忆失败")?;

        let id = if let Some((id, old_importance, old_tags_raw)) = existing {
            // 同一内容只保留一条：重要度取较大值，标签合并，刷新更新时间。
            let mut merged = parse_tags(&old_tags_raw);
            for tag in &tags {
                if !merged.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                    merged.push(tag.clone());
                }
            }
            let merged_json = serde_json::to_string(&merged).context("序列化标签失败")?;
            conn.execute(
                "UPDATE facts SET importance = ?1, tags = ?2, updated_at = ?3, \
                 source = CASE WHEN source = '' THEN ?4 ELSE source END WHERE id = ?5",
                params![importance.max(old_importance), merged_json, now, source, id],
            )
            .context("更新已有记忆失败")?;
            id
        } else {
            conn.execute(
                "INSERT INTO facts (kind, content, tags, importance, source, created_at, updated_at, hits) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0)",
                params![kind, content, tags_json, importance, source, now],
            )
            .context("写入记忆失败")?;
            conn.last_insert_rowid()
        };
        self.evict_if_needed(&conn)?;
        Ok(id)
    }

    // ------------------------------------------------------------
    // MemoryApi 的同名转发：
    // app 层通常直接持有 `Arc<MemoryStore>`，加这些 inherent 方法后，
    // UI 代码不必再 `use neosonia_core::model::MemoryApi;` 就能调用。
    // 行为与 trait 实现完全一致。
    // ------------------------------------------------------------

    pub fn remember(
        &self,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
    ) -> anyhow::Result<i64> {
        <Self as MemoryApi>::remember(self, content, tags, importance, source)
    }

    pub fn recall(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        <Self as MemoryApi>::recall(self, query, limit)
    }

    pub fn recent(&self, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        <Self as MemoryApi>::recent(self, limit)
    }

    pub fn all(&self, kind: Option<&str>, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        <Self as MemoryApi>::all(self, kind, limit)
    }

    pub fn forget(&self, id: i64) -> anyhow::Result<bool> {
        <Self as MemoryApi>::forget(self, id)
    }

    pub fn update_importance(&self, id: i64, importance: f64) -> anyhow::Result<()> {
        <Self as MemoryApi>::update_importance(self, id, importance)
    }

    pub fn get_block(&self, key: &str) -> anyhow::Result<Option<String>> {
        <Self as MemoryApi>::get_block(self, key)
    }

    pub fn set_block(&self, key: &str, value: &str) -> anyhow::Result<()> {
        <Self as MemoryApi>::set_block(self, key, value)
    }

    pub fn blocks(&self) -> anyhow::Result<BTreeMap<String, String>> {
        <Self as MemoryApi>::blocks(self)
    }

    pub fn log_conversation(&self, role: &str, content: &str, turn_id: &str) -> anyhow::Result<()> {
        <Self as MemoryApi>::log_conversation(self, role, content, turn_id)
    }

    pub fn stats(&self) -> anyhow::Result<MemoryStats> {
        <Self as MemoryApi>::stats(self)
    }

    /// 超出上限时按「重要度 * 0.7 + 新近度 * 0.3」淘汰低分条目。
    fn evict_if_needed(&self, conn: &Connection) -> anyhow::Result<()> {
        let max_items = self.max_items.load(Ordering::Relaxed).max(1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM facts", [], |row| row.get(0))
            .context("统计记忆条数失败")?;
        if count <= max_items {
            return Ok(());
        }
        let excess = (count - max_items) as usize;
        let now = now_ms();
        let mut stmt = conn
            .prepare("SELECT id, importance, updated_at FROM facts")
            .context("准备淘汰查询失败")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .context("读取待淘汰记忆失败")?;
        let mut scored: Vec<(f64, i64)> = Vec::new();
        for row in rows {
            let (id, importance, updated_at) = row.context("读取待淘汰记忆行失败")?;
            let recency = recency_factor(updated_at, now);
            scored.push((importance.clamp(0.0, 1.0) * 0.7 + recency * 0.3, id));
        }
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let victims: Vec<i64> = scored.into_iter().take(excess).map(|(_, id)| id).collect();
        if victims.is_empty() {
            return Ok(());
        }
        let tx = conn.unchecked_transaction().context("开始淘汰事务失败")?;
        for id in victims {
            tx.execute("DELETE FROM facts WHERE id = ?1", [id])
                .context("淘汰旧记忆失败")?;
        }
        tx.commit().context("提交淘汰事务失败")?;
        Ok(())
    }
}

impl MemoryApi for MemoryStore {
    fn remember(
        &self,
        content: &str,
        tags: &[String],
        importance: f64,
        source: &str,
    ) -> anyhow::Result<i64> {
        let kind = infer_kind(content, tags);
        self.remember_kind(&kind, content, tags, importance, source)
    }

    fn recall(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        let limit = limit.clamp(1, 100);
        let tokens = tokenize_query(query);
        let conn = self.conn.lock();

        let candidates = if tokens.is_empty() {
            load_facts(&conn, None, &[], CANDIDATE_LIMIT)?
        } else {
            let mut clauses: Vec<String> = Vec::with_capacity(tokens.len());
            let mut params: Vec<String> = Vec::with_capacity(tokens.len() * 2);
            for token in &tokens {
                let pattern = format!("%{}%", escape_like(token));
                clauses.push(
                    "(lower(content) LIKE ? ESCAPE '\\' OR lower(tags) LIKE ? ESCAPE '\\')"
                        .to_string(),
                );
                params.push(pattern.clone());
                params.push(pattern);
            }
            load_facts(&conn, Some(&clauses.join(" OR ")), &params, CANDIDATE_LIMIT)?
        };

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let now = now_ms();
        let mut scored: Vec<(f64, MemoryItem)> = candidates
            .into_iter()
            .map(|item| (score_item(&item, &tokens, now), item))
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.updated_at_ms.cmp(&a.1.updated_at_ms))
        });
        scored.truncate(limit);

        // 命中后 hits + 1，方便后续淘汰时保留常用记忆。
        for (_, item) in &scored {
            conn.execute("UPDATE facts SET hits = hits + 1 WHERE id = ?1", [item.id])
                .context("更新记忆命中次数失败")?;
        }
        let mut result: Vec<MemoryItem> = scored
            .into_iter()
            .map(|(_, mut item)| {
                item.hits += 1;
                item
            })
            .collect();
        // 结果按分数降序返回，调用方（prompt 注入）依赖这个顺序。
        result.shrink_to_fit();
        Ok(result)
    }

    fn recent(&self, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        let limit = limit.clamp(1, 500);
        let conn = self.conn.lock();
        load_facts(&conn, None, &[], limit as i64)
    }

    fn all(&self, kind: Option<&str>, limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        let limit = if limit == 0 {
            -1
        } else {
            limit.min(5000) as i64
        };
        let conn = self.conn.lock();
        match kind.map(|k| k.trim()).filter(|k| !k.is_empty()) {
            Some(kind) => {
                let sql = format!(
                    "SELECT {FACT_COLUMNS} FROM facts WHERE kind = ?1 \
                     ORDER BY importance DESC, updated_at DESC LIMIT {limit}"
                );
                let mut stmt = conn.prepare(&sql).context("准备记忆查询失败")?;
                let rows = stmt
                    .query_map([kind], row_to_item)
                    .context("查询记忆失败")?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .context("读取记忆行失败")
            }
            None => load_facts(&conn, None, &[], limit),
        }
    }

    fn forget(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute("DELETE FROM facts WHERE id = ?1", [id])
            .context("删除记忆失败")?;
        Ok(changed > 0)
    }

    fn update_importance(&self, id: i64, importance: f64) -> anyhow::Result<()> {
        if !importance.is_finite() {
            bail!("重要度必须是有限数字（0.0 ~ 1.0）");
        }
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE facts SET importance = ?1 WHERE id = ?2",
            params![importance.clamp(0.0, 1.0), id],
        )
        .context("更新记忆重要度失败")?;
        Ok(())
    }

    fn get_block(&self, key: &str) -> anyhow::Result<Option<String>> {
        let key = key.trim();
        if key.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock();
        conn.query_row("SELECT value FROM blocks WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .context("读取固定记忆块失败")
    }

    fn set_block(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let key = key.trim();
        if key.is_empty() {
            bail!("固定记忆块的 key 不能为空");
        }
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO blocks (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now_ms()],
        )
        .context("写入固定记忆块失败")?;
        Ok(())
    }

    fn blocks(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT key, value FROM blocks ORDER BY key")
            .context("准备固定块查询失败")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("查询固定块失败")?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (k, v) = row.context("读取固定块行失败")?;
            out.insert(k, v);
        }
        Ok(out)
    }

    fn log_conversation(&self, role: &str, content: &str, turn_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO conversations (role, content, turn_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![role, content, turn_id, now_ms()],
        )
        .context("写入对话记录失败")?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .context("统计对话记录失败")?;
        if count > CONVERSATION_KEEP {
            conn.execute(
                "DELETE FROM conversations WHERE id NOT IN \
                 (SELECT id FROM conversations ORDER BY id DESC LIMIT ?1)",
                [CONVERSATION_KEEP],
            )
            .context("清理旧对话记录失败")?;
        }
        Ok(())
    }

    fn stats(&self) -> anyhow::Result<MemoryStats> {
        let conn = self.conn.lock();
        let count = |table: &str| -> anyhow::Result<i64> {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            conn.query_row(&sql, [], |row| row.get(0))
                .with_context(|| format!("统计 {table} 失败"))
        };
        let facts = count("facts")?;
        let conversations = count("conversations")?;
        let blocks = count("blocks")?;
        drop(conn);
        Ok(MemoryStats {
            facts,
            conversations,
            blocks,
            db_bytes: db_bytes(&self.path),
        })
    }
}

// ============================================================
// 查询 / 打分辅助
// ============================================================

fn load_facts(
    conn: &Connection,
    where_clause: Option<&str>,
    params: &[String],
    limit: i64,
) -> anyhow::Result<Vec<MemoryItem>> {
    let sql = match where_clause {
        Some(clause) => format!(
            "SELECT {FACT_COLUMNS} FROM facts WHERE {clause} \
             ORDER BY updated_at DESC LIMIT {limit}"
        ),
        None => format!("SELECT {FACT_COLUMNS} FROM facts ORDER BY updated_at DESC LIMIT {limit}"),
    };
    let mut stmt = conn.prepare(&sql).context("准备记忆查询失败")?;
    let rows = stmt
        .query_map(
            params_from_iter(params.iter().map(|s| s.as_str())),
            row_to_item,
        )
        .context("查询记忆失败")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("读取记忆行失败")
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    let tags_raw: String = row.get(3)?;
    Ok(MemoryItem {
        id: row.get(0)?,
        kind: row.get(1)?,
        content: row.get(2)?,
        tags: parse_tags(&tags_raw),
        importance: row.get(4)?,
        created_at_ms: row.get(5)?,
        updated_at_ms: row.get(6)?,
        hits: row.get(7)?,
        source: row.get(8)?,
    })
}

fn parse_tags(raw: &str) -> Vec<String> {
    if let Ok(tags) = serde_json::from_str::<Vec<String>>(raw) {
        return tags;
    }
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        let tag = truncate_chars(tag, 48);
        if out.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
            continue;
        }
        out.push(tag);
        if out.len() >= 16 {
            break;
        }
    }
    out
}

fn normalize_kind(kind: &str) -> String {
    let k = kind.trim().to_ascii_lowercase();
    if k.is_empty() {
        return "fact".to_string();
    }
    // 已知类型直接用，未知类型也保留（允许 note 之类的自定义 kind），只做长度限制。
    truncate_chars(&k, 24)
}

fn normalize_importance(importance: f64) -> f64 {
    if importance.is_finite() {
        importance.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn infer_kind(content: &str, tags: &[String]) -> String {
    let joined = format!("{} {}", tags.join(","), content).to_ascii_lowercase();
    for (needle, kind) in [
        ("preference", "preference"),
        ("偏好", "preference"),
        ("喜好", "preference"),
        ("task", "task"),
        ("任务", "task"),
        ("待办", "task"),
        ("event", "event"),
        ("事件", "event"),
        ("block", "block"),
    ] {
        if joined.contains(needle) {
            return kind.to_string();
        }
    }
    "fact".to_string()
}

/// 把查询切成可做 `LIKE` 匹配的 token；中文按二元组切分。
fn tokenize_query(query: &str) -> Vec<String> {
    const MAX_TOKENS: usize = 12;
    let lower = query.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    for ch in lower.chars() {
        let is_cjk = (ch as u32) >= 0x2E80;
        if is_cjk {
            if !word.is_empty() {
                push_token(&mut tokens, &word);
                word.clear();
            }
            cjk.push(ch);
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            if !cjk.is_empty() {
                flush_cjk(&mut tokens, &cjk);
                cjk.clear();
            }
            word.push(ch);
        } else {
            if !word.is_empty() {
                push_token(&mut tokens, &word);
                word.clear();
            }
            if !cjk.is_empty() {
                flush_cjk(&mut tokens, &cjk);
                cjk.clear();
            }
        }
        if tokens.len() >= MAX_TOKENS {
            break;
        }
    }
    if !word.is_empty() {
        push_token(&mut tokens, &word);
    }
    if !cjk.is_empty() {
        flush_cjk(&mut tokens, &cjk);
    }
    tokens.truncate(MAX_TOKENS);
    tokens
}

fn flush_cjk(tokens: &mut Vec<String>, cjk: &[char]) {
    if cjk.is_empty() {
        return;
    }
    if cjk.len() == 1 {
        push_token(tokens, &cjk[0].to_string());
        return;
    }
    for pair in cjk.windows(2) {
        let token: String = pair.iter().collect();
        push_token(tokens, &token);
    }
}

fn push_token(tokens: &mut Vec<String>, token: &str) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    if tokens.iter().any(|t| t == token) {
        return;
    }
    tokens.push(truncate_chars(token, 32));
}

fn escape_like(token: &str) -> String {
    let mut out = String::with_capacity(token.len() + 4);
    for ch in token.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// 打分 = 文本匹配 * 0.5 + 重要度 * 0.3 + 新近度衰减 * 0.2（+ hits 的微小加成）。
fn score_item(item: &MemoryItem, tokens: &[String], now: i64) -> f64 {
    let hay = format!(
        "{} {}",
        item.content.to_lowercase(),
        item.tags.join(" ").to_lowercase()
    );
    let (matched, total) = if tokens.is_empty() {
        (0usize, 1usize)
    } else {
        (
            tokens.iter().filter(|t| hay.contains(t.as_str())).count(),
            tokens.len(),
        )
    };
    let text_score = matched as f64 / total.max(1) as f64;
    let importance = item.importance.clamp(0.0, 1.0);
    let recency = recency_factor(item.updated_at_ms, now);
    let usage = (item.hits.max(0) as f64).ln_1p() * 0.01;
    (text_score * 0.5 + importance * 0.3 + recency * 0.2 + usage).clamp(0.0, 1.5)
}

/// 90 天半衰期的新近度：0 天 = 1.0。
fn recency_factor(updated_at_ms: i64, now: i64) -> f64 {
    let age_days = ((now - updated_at_ms.max(0)) as f64 / 86_400_000.0).max(0.0);
    (-age_days / 90.0).exp()
}

fn db_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(suffix);
        total += std::fs::metadata(&side).map(|m| m.len()).unwrap_or(0);
    }
    total
}

fn format_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

// ============================================================
// 内置记忆工具（由 AgentEngine::start 注册）
// ============================================================

/// 把 4 个记忆工具注册进 registry。重复调用会覆盖同名工具，幂等。
pub fn register_builtin_tools(reg: &ToolRegistry) {
    reg.register(Arc::new(MemoryRememberTool));
    reg.register(Arc::new(MemoryRecallTool));
    reg.register(Arc::new(MemoryListTool));
    reg.register(Arc::new(MemoryForgetTool));
}

fn arg_tags(args: &Value) -> Vec<String> {
    args.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

struct MemoryRememberTool;

#[async_trait]
impl Tool for MemoryRememberTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "memory_remember",
            "把一条值得长期记住的信息写入长期记忆。适合：用户明确说\"记住\"、长期偏好/身份信息、\
             项目约定、重要日程。不适合：一次性的对话细节、可以从文件重新读到的内容。\
             相同内容重复写入会自动合并（重要度取较大值）。",
            json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "要记住的内容，一句话写清关键实体与结论。"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "可选标签，例如 偏好 / 项目 / 日程 / 技术栈。"
                    },
                    "importance": {
                        "type": "number",
                        "description": "重要度 0.0 ~ 1.0，默认 0.6；用户明确要求长期记住的用 0.8 以上。"
                    }
                },
                "required": ["content"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            return ToolOutcome::err(
                "memory_remember 缺少 content 参数。示例：{\"content\":\"用户偏好简洁的中文回答\",\"tags\":[\"偏好\"]}",
            );
        }
        let tags = arg_tags(&args);
        let importance = args
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.6);
        match ctx.memory().remember(&content, &tags, importance, "agent") {
            Ok(id) => {
                ctx.emit(AgentEvent::MemoryChanged {
                    action: "remember".into(),
                    content: content.clone(),
                });
                ToolOutcome::ok(format!(
                    "已记住（id={id}）：{}",
                    truncate_chars(&content, 80)
                ))
                .with_display("记住一条")
            }
            Err(e) => ToolOutcome::err(format!(
                "写入长期记忆失败：{e}。请检查 data/memory.sqlite3 是否可写、磁盘是否已满。"
            )),
        }
    }
}

struct MemoryRecallTool;

#[async_trait]
impl Tool for MemoryRecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "memory_recall",
            "按关键词检索长期记忆（文本匹配 + 标签 + 重要度 + 新近度）。\
             在需要回忆用户偏好、以前做过的事、项目约定时使用；查不到就说明没有记录，不要编造。",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索关键词或自然语言问题。" },
                    "limit": { "type": "integer", "description": "最多返回几条，默认 5，最大 50。" }
                },
                "required": ["query"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return ToolOutcome::err(
                "memory_recall 缺少 query 参数。示例：{\"query\":\"用户对我的称呼\"}",
            );
        }
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        match ctx.memory().recall(&query, limit) {
            Ok(items) if items.is_empty() => ToolOutcome::ok(format!(
                "没有找到与「{}」相关的长期记忆。",
                truncate_chars(&query, 60)
            )),
            Ok(items) => {
                let mut out = format!("找到 {} 条相关记忆：\n", items.len());
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. [{}] {}（重要度 {:.2}，{}）\n",
                        i + 1,
                        item.kind,
                        item.content,
                        item.importance,
                        format_ms(item.updated_at_ms)
                    ));
                    if !item.tags.is_empty() {
                        out.push_str(&format!("   标签: {}\n", item.tags.join(", ")));
                    }
                }
                ToolOutcome::ok(out).with_display(format!("找到 {} 条记忆", items.len()))
            }
            Err(e) => ToolOutcome::err(format!("检索长期记忆失败：{e}")),
        }
    }
}

struct MemoryListTool;

#[async_trait]
impl Tool for MemoryListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "memory_list",
            "列出长期记忆条目（可按 kind 过滤：fact / preference / event / task / block）。\
             用于浏览记忆库；按关键词找具体内容请用 memory_recall。",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "可选，按类型过滤。" },
                    "limit": { "type": "integer", "description": "最多返回几条，默认 20。" }
                }
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        match ctx.memory().all(kind.as_deref(), limit) {
            Ok(items) if items.is_empty() => ToolOutcome::ok("长期记忆库为空。".to_string()),
            Ok(items) => {
                let mut out = format!("共 {} 条记忆：\n", items.len());
                for item in &items {
                    out.push_str(&format!(
                        "#{} [{}] {}（重要度 {:.2}，{}）\n",
                        item.id,
                        item.kind,
                        truncate_chars(&item.content, 200),
                        item.importance,
                        format_ms(item.updated_at_ms)
                    ));
                }
                ToolOutcome::ok(out).with_display(format!("{} 条记忆", items.len()))
            }
            Err(e) => ToolOutcome::err(format!("列出长期记忆失败：{e}")),
        }
    }
}

struct MemoryForgetTool;

#[async_trait]
impl Tool for MemoryForgetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "memory_forget",
            "按 id 删除一条长期记忆（id 可由 memory_list / memory_recall 得到）。\
             删除不可恢复；不确定时先 memory_list 确认。",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "记忆条目的数字 id。" }
                },
                "required": ["id"]
            }),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let Some(id) = args.get("id").and_then(|v| v.as_i64()) else {
            return ToolOutcome::err(
                "memory_forget 缺少整数 id 参数。请先调用 memory_list 查看 id。",
            );
        };
        match ctx.memory().forget(id) {
            Ok(true) => {
                ctx.emit(AgentEvent::MemoryChanged {
                    action: "forget".into(),
                    content: format!("id={id}"),
                });
                ToolOutcome::ok(format!("已删除记忆 #{id}。"))
            }
            Ok(false) => ToolOutcome::err(format!(
                "没有找到 id={id} 的记忆（可能已被删除）。请先调用 memory_list 确认。"
            )),
            Err(e) => ToolOutcome::err(format!("删除记忆失败：{e}")),
        }
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore {
        _dir: tempfile::TempDir,
        store: MemoryStore,
    }

    fn open_store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&dir.path().join("mem.sqlite3")).unwrap();
        TempStore { _dir: dir, store }
    }

    #[test]
    fn remember_dedupes_same_content() {
        let ts = open_store();
        let tags = vec!["偏好".to_string()];
        let id1 = ts
            .store
            .remember("用户喜欢简洁的中文回答", &tags, 0.5, "test")
            .unwrap();
        let id2 = ts
            .store
            .remember("用户喜欢简洁的中文回答", &tags, 0.9, "test2")
            .unwrap();
        assert_eq!(id1, id2, "相同内容应合并为同一条记忆");
        let items = ts.store.all(None, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert!((items[0].importance - 0.9).abs() < 1e-6, "重要度取较大值");
    }

    #[test]
    fn remember_rejects_empty_content() {
        let ts = open_store();
        assert!(ts.store.remember("   ", &[], 0.5, "test").is_err());
    }

    #[test]
    fn recall_ranks_by_text_match_and_importance() {
        let ts = open_store();
        ts.store
            .remember_kind(
                "preference",
                "用户喜欢深色主题",
                &["偏好".into()],
                0.9,
                "test",
            )
            .unwrap();
        ts.store
            .remember_kind(
                "fact",
                "项目使用 Rust 与 fenestra",
                &["项目".into()],
                0.4,
                "test",
            )
            .unwrap();
        ts.store
            .remember_kind("event", "昨天测试了邮件发送", &["邮件".into()], 0.2, "test")
            .unwrap();

        let hits = ts.store.recall("深色主题", 5).unwrap();
        assert_eq!(hits.len(), 1, "只有一个条目命中关键词");
        assert!(hits[0].content.contains("深色主题"));
        assert_eq!(hits[0].hits, 1, "命中后 hits 应 +1");

        // 无关键词时返回候选（按分数排序），且数量受 limit 限制。
        let top = ts.store.recall("", 2).unwrap();
        assert_eq!(top.len(), 2);
        assert!(top[0].importance >= top[1].importance);
    }

    #[test]
    fn recall_handles_like_wildcards() {
        let ts = open_store();
        ts.store.remember("进度 100%", &[], 0.5, "test").unwrap();
        ts.store.remember("普通条目", &[], 0.5, "test").unwrap();
        let hits = ts.store.recall("100%", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("100%"));
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("a%_b"), "a\\%\\_b");
        assert_eq!(escape_like("path\\to"), "path\\\\to");
    }

    #[test]
    fn recent_all_forget_and_blocks() {
        let ts = open_store();
        let id = ts
            .store
            .remember_kind("task", "整理邮件收件箱", &[], 0.5, "test")
            .unwrap();
        ts.store.log_conversation("user", "你好", "turn-1").unwrap();
        ts.store
            .log_conversation("assistant", "你好，需要我做什么？", "turn-1")
            .unwrap();

        assert_eq!(ts.store.recent(10).unwrap().len(), 1);
        assert_eq!(ts.store.all(Some("task"), 10).unwrap().len(), 1);
        assert!(ts.store.all(Some("preference"), 10).unwrap().is_empty());

        ts.store.set_block("persona", "NeoSonia").unwrap();
        ts.store.set_block("user", "开发者").unwrap();
        assert_eq!(ts.store.get_block("persona").unwrap().unwrap(), "NeoSonia");
        assert!(ts.store.get_block("不存在").unwrap().is_none());
        assert_eq!(ts.store.blocks().unwrap().len(), 2);

        ts.store.update_importance(id, 0.95).unwrap();
        assert!((ts.store.all(Some("task"), 1).unwrap()[0].importance - 0.95).abs() < 1e-6);

        let stats = ts.store.stats().unwrap();
        assert_eq!(stats.facts, 1);
        assert_eq!(stats.blocks, 2);
        assert_eq!(stats.conversations, 2);
        assert!(stats.db_bytes > 0);

        assert!(ts.store.forget(id).unwrap());
        assert!(!ts.store.forget(id).unwrap(), "重复删除返回 false");
        assert!(ts.store.stats().unwrap().facts == 0);
    }

    #[test]
    fn eviction_keeps_high_importance_items() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_with_max_items(&dir.path().join("mem.sqlite3"), 3).unwrap();
        store
            .remember_kind("fact", "低分 1", &[], 0.1, "test")
            .unwrap();
        store
            .remember_kind("fact", "低分 2", &[], 0.15, "test")
            .unwrap();
        store
            .remember_kind("fact", "高分", &[], 0.95, "test")
            .unwrap();
        store
            .remember_kind("fact", "低分 3", &[], 0.2, "test")
            .unwrap();
        // 第 5 条触发淘汰，条数回落到上限。
        store
            .remember_kind("fact", "中分", &[], 0.6, "test")
            .unwrap();
        let items = store.all(None, 100).unwrap();
        assert_eq!(items.len(), 3);
        let contents: Vec<&str> = items.iter().map(|i| i.content.as_str()).collect();
        assert!(contents.contains(&"高分"));
        assert!(!contents.contains(&"低分 1"), "最低分的应被淘汰");
    }

    #[test]
    fn tokenizer_splits_cjk_into_bigrams() {
        let tokens = tokenize_query("我喜欢咖啡");
        assert!(tokens.contains(&"咖啡".to_string()));
        assert!(tokens.contains(&"喜欢".to_string()));
        let mixed = tokenize_query("read_file 中文");
        assert!(mixed.contains(&"read_file".to_string()));
        assert!(mixed.iter().any(|t| t == "中文"));
        assert!(tokenize_query("  ,.!?  ").is_empty());
    }

    #[test]
    fn kind_inference_and_tag_roundtrip() {
        assert_eq!(infer_kind("用户偏好简洁", &[]), "preference");
        assert_eq!(infer_kind("明天要交付", &["task".into()]), "task");
        assert_eq!(infer_kind("随便一句", &[]), "fact");
        assert_eq!(normalize_kind(""), "fact");
        assert_eq!(normalize_kind("PREFERENCE"), "preference");
        let tags = normalize_tags(&[" a ".into(), "A".into(), "".into()]);
        assert_eq!(tags, vec!["a".to_string()]);
        assert_eq!(
            parse_tags("[\"x\",\"y\"]"),
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(parse_tags("x,y"), vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn builtin_tools_are_registered() {
        let reg = ToolRegistry::new();
        register_builtin_tools(&reg);
        let mut names = reg.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "memory_forget".to_string(),
                "memory_list".to_string(),
                "memory_recall".to_string(),
                "memory_remember".to_string(),
            ]
        );
    }
}
