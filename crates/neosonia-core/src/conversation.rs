//! 对话历史库：把每轮 user / assistant / tool 消息持久化到 SQLite，
//! 支持会话列表、标题搜索、消息全文搜索、重命名与删除。
//!
//! 数据库路径来自 `Paths.conversations_db`（默认 `data/conversations.sqlite3`），
//! 连接不是 `Sync`，统一用 `parking_lot::Mutex<Connection>` 包住并开启 WAL。
//!
//! OWNER: core-llm 队友。

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::model::{ChatMessage, ImagePart, Role, ToolCall};
use crate::shared::{ConversationHit, ConversationSummary};
use crate::util::{now_ms, truncate_chars};

/// `messages` 表的 SELECT 列顺序，必须与 [`row_to_message`] 一致。
const MESSAGE_COLUMNS: &str = "msg_id, role, content, reasoning, images_json, tool_calls_json, tool_call_id, name, tool_ok, created_at_ms";

/// 已持久化的上下文压缩摘要（只用于给模型的历史，不删原始消息）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSummary {
    pub summary: String,
    /// 摘要覆盖的历史前缀长度（按当时 in-memory 历史条数计）。
    pub covered_len: usize,
    /// 保留原文的最近消息条数。
    pub kept: usize,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub updated_at_ms: i64,
}

/// 对话历史库（线程安全）。
pub struct ConversationStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl ConversationStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建历史库目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开对话历史库失败: {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .context("设置历史库 busy_timeout 失败")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("开启历史库 WAL 失败（文件可能被其它进程占用）")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("设置历史库 synchronous 失败")?;
        let store = Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS conversations (
                id            TEXT PRIMARY KEY,
                title         TEXT NOT NULL DEFAULT '',
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                preview       TEXT NOT NULL DEFAULT '',
                persona       TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at_ms DESC);

            CREATE TABLE IF NOT EXISTS messages (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                conv_id         TEXT NOT NULL,
                msg_id          TEXT NOT NULL DEFAULT '',
                role            TEXT NOT NULL,
                content         TEXT,
                reasoning       TEXT,
                images_json     TEXT,
                tool_calls_json TEXT,
                tool_call_id    TEXT,
                name            TEXT,
                tool_ok         INTEGER,
                created_at_ms   INTEGER NOT NULL,
                seq             INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_conv_seq ON messages(conv_id, seq);
            CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at_ms DESC);

            CREATE TABLE IF NOT EXISTS conv_summaries (
                conv_id       TEXT PRIMARY KEY,
                summary       TEXT NOT NULL DEFAULT '',
                covered_len   INTEGER NOT NULL DEFAULT 0,
                kept          INTEGER NOT NULL DEFAULT 0,
                before_tokens INTEGER NOT NULL DEFAULT 0,
                after_tokens  INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS conv_plans (
                conv_id       TEXT PRIMARY KEY,
                steps_json    TEXT NOT NULL DEFAULT '[]',
                updated_at_ms INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .context("初始化对话历史表结构失败")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 确保会话存在，返回会话 id。
    ///
    /// - `conv_id = Some(id)` 且库里有 → 更新 updated_at/persona 后返回该 id；
    /// - `conv_id = Some(id)` 但库里没有 → 用这个 id 新建；
    /// - `conv_id = None` → 生成新 id 并新建。
    pub fn ensure(
        &self,
        conv_id: Option<&str>,
        first_user_text: &str,
        persona: &str,
    ) -> anyhow::Result<String> {
        let now = now_ms();
        let title = title_from(first_user_text);
        let persona = persona.trim();
        let conn = self.conn.lock();

        if let Some(id) = conv_id.map(str::trim).filter(|id| !id.is_empty()) {
            let exists: bool = conn
                .query_row("SELECT 1 FROM conversations WHERE id = ?1", [id], |_| {
                    Ok(true)
                })
                .optional()
                .context("查询会话是否存在失败")?
                .unwrap_or(false);
            if exists {
                conn.execute(
                    "UPDATE conversations SET updated_at_ms = ?1, \
                     persona = CASE WHEN ?2 = '' THEN persona ELSE ?2 END WHERE id = ?3",
                    params![now, persona, id],
                )
                .context("更新会话元数据失败")?;
                return Ok(id.to_string());
            }
            insert_conversation(&conn, id, &title, persona, now)?;
            return Ok(id.to_string());
        }

        let id = uuid::Uuid::new_v4().to_string();
        insert_conversation(&conn, &id, &title, persona, now)?;
        Ok(id)
    }

    /// 追加一条消息。会自动更新会话的 message_count / updated_at / preview。
    pub fn append(&self, conv_id: &str, msg: &ChatMessage) -> anyhow::Result<()> {
        let conv_id = conv_id.trim();
        if conv_id.is_empty() {
            bail!("append 会话 id 不能为空");
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction().context("开始消息事务失败")?;

        // 会话不存在时补建，避免产生孤儿消息（例如历史库被手工清理过）。
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM conversations WHERE id = ?1",
                [conv_id],
                |_| Ok(true),
            )
            .optional()
            .context("查询会话是否存在失败")?
            .unwrap_or(false);
        let now = now_ms();
        if !exists {
            let title = msg
                .content
                .as_deref()
                .map(title_from)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "新对话".to_string());
            insert_conversation(&tx, conv_id, &title, "", now)?;
        }

        let seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conv_id = ?1",
                [conv_id],
                |row| row.get(0),
            )
            .context("计算消息序号失败")?;
        let created_at_ms = if msg.created_at_ms > 0 {
            msg.created_at_ms
        } else {
            now
        };
        let images_json = if msg.images.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&msg.images).context("序列化图片消息失败")?)
        };
        let tool_calls_json = if msg.tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&msg.tool_calls).context("序列化工具调用失败")?)
        };
        let tool_ok = msg.tool_ok.map(|ok| if ok { 1i64 } else { 0i64 });

        tx.execute(
            "INSERT INTO messages \
             (conv_id, msg_id, role, content, reasoning, images_json, tool_calls_json, \
              tool_call_id, name, tool_ok, created_at_ms, seq) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                conv_id,
                msg.id,
                role_str(msg.role),
                msg.content,
                msg.reasoning_content,
                images_json,
                tool_calls_json,
                msg.tool_call_id,
                msg.name,
                tool_ok,
                created_at_ms,
                seq
            ],
        )
        .context("写入消息失败")?;

        let preview = preview_of(msg);
        tx.execute(
            "UPDATE conversations SET message_count = message_count + 1, \
             updated_at_ms = ?1, preview = ?2 WHERE id = ?3",
            params![now.max(created_at_ms), preview, conv_id],
        )
        .context("更新会话摘要失败")?;

        tx.commit().context("提交消息事务失败")?;
        Ok(())
    }

    /// 读取某个会话的消息（按 seq 升序）。`limit = 0` 表示全部。
    pub fn messages(&self, conv_id: &str, limit: usize) -> anyhow::Result<Vec<ChatMessage>> {
        let conn = self.conn.lock();
        let sql_limit = if limit == 0 {
            -1
        } else {
            limit.min(20_000) as i64
        };
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conv_id = ?1 \
             ORDER BY seq DESC LIMIT {sql_limit}"
        );
        let mut stmt = conn.prepare(&sql).context("准备消息查询失败")?;
        let rows = stmt
            .query_map([conv_id], row_to_message)
            .context("查询消息失败")?;
        let mut messages = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("读取消息行失败")?;
        messages.reverse();
        Ok(messages)
    }

    /// 分页读取：`before_seq = None` 取最后 `limit` 条；`Some(seq)` 取 `seq` 之前
    /// 的最后 `limit` 条。返回按 seq 升序的 `(seq, ChatMessage)`；`limit = 0` 表示全部。
    pub fn messages_before(
        &self,
        conv_id: &str,
        before_seq: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<(i64, ChatMessage)>> {
        use rusqlite::types::Value as SqlValue;
        let conn = self.conn.lock();
        let sql_limit = if limit == 0 {
            -1
        } else {
            limit.min(20_000) as i64
        };
        let mut params_vec: Vec<SqlValue> = vec![SqlValue::Text(conv_id.to_string())];
        let sql = match before_seq {
            Some(_) => {
                params_vec.push(SqlValue::Integer(before_seq.unwrap_or(0)));
                format!(
                    "SELECT seq, {MESSAGE_COLUMNS} FROM messages \
                     WHERE conv_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT {sql_limit}"
                )
            }
            None => format!(
                "SELECT seq, {MESSAGE_COLUMNS} FROM messages \
                 WHERE conv_id = ?1 ORDER BY seq DESC LIMIT {sql_limit}"
            ),
        };
        let mut stmt = conn.prepare(&sql).context("准备分页消息查询失败")?;
        let rows = stmt
            .query_map(params_from_iter(params_vec), |row| {
                let seq: i64 = row.get(0)?;
                let message = row_to_message_at(row, 1)?;
                Ok((seq, message))
            })
            .context("查询分页消息失败")?;
        let mut out = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("读取分页消息行失败")?;
        out.reverse();
        Ok(out)
    }

    /// 会话最后一条消息的 seq（空会话返回 -1）。
    pub fn last_seq(&self, conv_id: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) FROM messages WHERE conv_id = ?1",
            [conv_id],
            |row| row.get(0),
        )
        .context("读取会话最大 seq 失败")
    }

    /// 保存（覆盖）上下文压缩摘要：只存摘要与统计，不修改原始消息。
    pub fn save_summary(
        &self,
        conv_id: &str,
        summary: &str,
        covered_len: usize,
        kept: usize,
        before_tokens: u64,
        after_tokens: u64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO conv_summaries \
             (conv_id, summary, covered_len, kept, before_tokens, after_tokens, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(conv_id) DO UPDATE SET \
               summary = excluded.summary, covered_len = excluded.covered_len, \
               kept = excluded.kept, before_tokens = excluded.before_tokens, \
               after_tokens = excluded.after_tokens, updated_at_ms = excluded.updated_at_ms",
            params![
                conv_id,
                summary,
                covered_len as i64,
                kept as i64,
                before_tokens as i64,
                after_tokens as i64,
                now_ms()
            ],
        )
        .context("写入对话压缩摘要失败")?;
        Ok(())
    }

    /// 读取上下文压缩摘要（没有返回 None）。
    pub fn summary(&self, conv_id: &str) -> anyhow::Result<Option<StoredSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT summary, covered_len, kept, before_tokens, after_tokens, updated_at_ms \
                 FROM conv_summaries WHERE conv_id = ?1",
            )
            .context("准备压缩摘要查询失败")?;
        let row = stmt
            .query_row([conv_id], |row| {
                Ok(StoredSummary {
                    summary: row.get::<_, String>(0)?,
                    covered_len: row.get::<_, i64>(1)?.max(0) as usize,
                    kept: row.get::<_, i64>(2)?.max(0) as usize,
                    before_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    after_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                    updated_at_ms: row.get(5)?,
                })
            })
            .optional()
            .context("读取压缩摘要失败")?;
        Ok(row)
    }

    /// 删除某个会话的压缩摘要（会话被重置/清空时调用）。
    pub fn clear_summary(&self, conv_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let removed = conn
            .execute("DELETE FROM conv_summaries WHERE conv_id = ?1", [conv_id])
            .context("删除压缩摘要失败")?;
        Ok(removed > 0)
    }

    /// 保存当前会话的 Work Plan（空列表 = 清除计划）。
    pub fn save_plan(&self, conv_id: &str, steps: &[crate::model::PlanStep]) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        if steps.is_empty() {
            conn.execute("DELETE FROM conv_plans WHERE conv_id = ?1", [conv_id])
                .context("清除会话计划失败")?;
            return Ok(());
        }
        let json = serde_json::to_string(steps).context("序列化会话计划失败")?;
        conn.execute(
            "INSERT INTO conv_plans (conv_id, steps_json, updated_at_ms) VALUES (?1, ?2, ?3) \
             ON CONFLICT(conv_id) DO UPDATE SET \
               steps_json = excluded.steps_json, updated_at_ms = excluded.updated_at_ms",
            params![conv_id, json, now_ms()],
        )
        .context("写入会话计划失败")?;
        Ok(())
    }

    /// 读取当前会话的 Work Plan（没有返回空）。
    pub fn plan(&self, conv_id: &str) -> anyhow::Result<Vec<crate::model::PlanStep>> {
        let conn = self.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT steps_json FROM conv_plans WHERE conv_id = ?1",
                [conv_id],
                |row| row.get(0),
            )
            .optional()
            .context("读取会话计划失败")?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    /// 删除某个会话的计划。
    pub fn clear_plan(&self, conv_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let removed = conn
            .execute("DELETE FROM conv_plans WHERE conv_id = ?1", [conv_id])
            .context("删除会话计划失败")?;
        Ok(removed > 0)
    }

    /// 会话列表（按最近更新倒序）。`query` 非空时按标题或消息内容做 LIKE 过滤。
    pub fn list(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<ConversationSummary>> {
        let limit = if limit == 0 {
            -1
        } else {
            limit.min(500) as i64
        };
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        let conn = self.conn.lock();
        let mut out = Vec::new();
        match query {
            Some(q) => {
                let pattern = format!("%{}%", escape_like(q));
                let sql = format!(
                    "SELECT id, title, created_at_ms, updated_at_ms, message_count, preview, persona \
                     FROM conversations c \
                     WHERE c.title LIKE ?1 ESCAPE '\\' OR EXISTS (\
                        SELECT 1 FROM messages m \
                        WHERE m.conv_id = c.id AND m.content LIKE ?1 ESCAPE '\\'\
                     ) \
                     ORDER BY c.updated_at_ms DESC LIMIT {limit}"
                );
                let mut stmt = conn.prepare(&sql).context("准备会话搜索失败")?;
                let rows = stmt
                    .query_map([pattern], row_to_summary)
                    .context("搜索会话失败")?;
                for row in rows {
                    out.push(row.context("读取会话行失败")?);
                }
            }
            None => {
                let sql = format!(
                    "SELECT id, title, created_at_ms, updated_at_ms, message_count, preview, persona \
                     FROM conversations ORDER BY updated_at_ms DESC LIMIT {limit}"
                );
                let mut stmt = conn.prepare(&sql).context("准备会话列表查询失败")?;
                let rows = stmt
                    .query_map([], row_to_summary)
                    .context("查询会话列表失败")?;
                for row in rows {
                    out.push(row.context("读取会话行失败")?);
                }
            }
        }
        Ok(out)
    }

    /// 取单个会话的摘要（不存在返回 None）。
    pub fn get(&self, conv_id: &str) -> anyhow::Result<Option<ConversationSummary>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, title, created_at_ms, updated_at_ms, message_count, preview, persona \
             FROM conversations WHERE id = ?1",
            [conv_id],
            row_to_summary,
        )
        .optional()
        .context("读取会话摘要失败")
    }

    /// 全文搜索消息（大小写不敏感的 LIKE），返回命中片段。
    pub fn search_messages(&self, q: &str, limit: usize) -> anyhow::Result<Vec<ConversationHit>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = if limit == 0 {
            100
        } else {
            limit.min(500) as i64
        };
        let pattern = format!("%{}%", escape_like(q));
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT m.conv_id, c.title, m.role, m.content, m.created_at_ms \
             FROM messages m JOIN conversations c ON c.id = m.conv_id \
             WHERE m.content LIKE ?1 ESCAPE '\\' \
             ORDER BY m.created_at_ms DESC LIMIT {limit}"
        );
        let mut stmt = conn.prepare(&sql).context("准备消息搜索失败")?;
        let rows = stmt
            .query_map([pattern], |row| {
                let content: Option<String> = row.get(3)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    content.unwrap_or_default(),
                    row.get::<_, i64>(4)?,
                ))
            })
            .context("搜索消息失败")?;
        let mut out = Vec::new();
        for row in rows {
            let (conversation_id, conversation_title, role, content, at_ms) =
                row.context("读取搜索命中行失败")?;
            out.push(ConversationHit {
                conversation_id,
                conversation_title,
                role,
                snippet: make_snippet(&content, q),
                at_ms,
            });
        }
        Ok(out)
    }

    pub fn rename(&self, conv_id: &str, title: &str) -> anyhow::Result<()> {
        let title = title.trim();
        if title.is_empty() {
            bail!("会话标题不能为空");
        }
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE conversations SET title = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![truncate_chars(title, 120), now_ms(), conv_id],
        )
        .context("重命名会话失败")?;
        Ok(())
    }

    pub fn delete(&self, conv_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction().context("开始删除事务失败")?;
        tx.execute("DELETE FROM messages WHERE conv_id = ?1", [conv_id])
            .context("删除会话消息失败")?;
        let changed = tx
            .execute("DELETE FROM conversations WHERE id = ?1", [conv_id])
            .context("删除会话失败")?;
        tx.commit().context("提交删除事务失败")?;
        Ok(changed > 0)
    }

    /// (会话数, 消息数)
    pub fn stats(&self) -> anyhow::Result<(i64, i64)> {
        let conn = self.conn.lock();
        let conversations: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .context("统计会话数失败")?;
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .context("统计消息数失败")?;
        Ok((conversations, messages))
    }
}

// ============================================================
// 辅助函数
// ============================================================

fn insert_conversation(
    conn: &Connection,
    id: &str,
    title: &str,
    persona: &str,
    now: i64,
) -> anyhow::Result<()> {
    let exists: bool = conn
        .query_row("SELECT 1 FROM conversations WHERE id = ?1", [id], |_| {
            Ok(true)
        })
        .optional()
        .context("查询会话是否存在失败")?
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO conversations \
         (id, title, created_at_ms, updated_at_ms, message_count, preview, persona) \
         VALUES (?1, ?2, ?3, ?3, 0, '', ?4)",
        params![id, title, now, persona],
    )
    .context("新建会话失败")?;
    Ok(())
}

/// 会话标题：首条用户消息去掉换行后取前 30 字。
pub fn title_from(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len().min(120));
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if last_space || cleaned.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        cleaned.push(ch);
    }
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "新对话".to_string();
    }
    let title: String = cleaned.chars().take(30).collect();
    title.trim().to_string()
}

fn preview_of(msg: &ChatMessage) -> String {
    let text = match msg.content.as_deref() {
        Some(content) if !content.trim().is_empty() => content.trim().to_string(),
        _ if msg.has_images() => "(图片)".to_string(),
        _ => String::new(),
    };
    truncate_chars(&text, 80)
}

fn role_str(role: Role) -> &'static str {
    role.as_str()
}

fn parse_role(raw: &str) -> Role {
    match raw.trim().to_ascii_lowercase().as_str() {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
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

/// 取命中位置前后各 40 个字符作为片段；找不到时退回开头 80 字。
fn make_snippet(content: &str, query: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let lower_chars: Vec<char> = content.to_lowercase().chars().collect();
    let q: Vec<char> = query.to_lowercase().chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let mut found = None;
    // 极少数字符小写化后长度会变（例如 ẞ -> ss），此时退回简单片段。
    if !q.is_empty() && lower_chars.len() == chars.len() {
        let last = lower_chars.len().saturating_sub(q.len());
        for start in 0..=last {
            if lower_chars[start..start + q.len()] == q[..] {
                found = Some((start, start + q.len()));
                break;
            }
        }
    }
    let (start, end) = match found {
        Some((hit_start, hit_end)) => (
            hit_start.saturating_sub(40),
            (hit_end + 40).min(chars.len()),
        ),
        None => (0, chars.len().min(80)),
    };
    let mut snippet: String = chars[start..end].iter().collect();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ")
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationSummary> {
    Ok(ConversationSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
        message_count: row.get::<_, i64>(4)?.max(0) as u32,
        preview: row.get(5)?,
        persona: row.get(6)?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessage> {
    row_to_message_at(row, 0)
}

/// 从 `base` 列开始按 [`MESSAGE_COLUMNS`] 的顺序读取一条消息。
fn row_to_message_at(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<ChatMessage> {
    let mut message = ChatMessage::default();
    if let Some(id) = row.get::<_, Option<String>>(base)? {
        if !id.trim().is_empty() {
            message.id = id;
        }
    }
    let role: String = row.get(base + 1)?;
    message.role = parse_role(&role);
    message.content = row
        .get::<_, Option<String>>(base + 2)?
        .filter(|s| !s.is_empty());
    message.reasoning_content = row
        .get::<_, Option<String>>(base + 3)?
        .filter(|s| !s.is_empty());
    message.images = row
        .get::<_, Option<String>>(base + 4)?
        .and_then(|raw| serde_json::from_str::<Vec<ImagePart>>(&raw).ok())
        .unwrap_or_default();
    message.tool_calls = row
        .get::<_, Option<String>>(base + 5)?
        .and_then(|raw| serde_json::from_str::<Vec<ToolCall>>(&raw).ok())
        .unwrap_or_default();
    message.tool_call_id = row
        .get::<_, Option<String>>(base + 6)?
        .filter(|s| !s.is_empty());
    message.name = row
        .get::<_, Option<String>>(base + 7)?
        .filter(|s| !s.is_empty());
    message.tool_ok = row.get::<_, Option<i64>>(base + 8)?.map(|v| v != 0);
    message.created_at_ms = row.get(base + 9)?;
    Ok(message)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FunctionCall, ToolCall};

    struct TempStore {
        _dir: tempfile::TempDir,
        store: ConversationStore,
    }

    fn open_store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(&dir.path().join("conv.sqlite3")).unwrap();
        TempStore { _dir: dir, store }
    }

    #[test]
    fn ensure_creates_and_reuses_conversation() {
        let ts = open_store();
        let id = ts
            .store
            .ensure(None, "帮我整理\n收件箱 里的邮件", "default")
            .unwrap();
        assert!(!id.is_empty());
        // 同一 id 再 ensure 不会新建。
        let again = ts.store.ensure(Some(&id), "另一条消息", "dev").unwrap();
        assert_eq!(id, again);
        let list = ts.store.list(None, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "帮我整理 收件箱 里的邮件");
        assert_eq!(list[0].persona, "dev", " persona 应被更新");
        // 指定不存在的 id 会按该 id 新建。
        let custom = ts.store.ensure(Some("custom-1"), "自定义", "").unwrap();
        assert_eq!(custom, "custom-1");
        assert_eq!(ts.store.list(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn title_strips_newlines_and_caps_length() {
        assert_eq!(title_from("  你好\n\n世界  "), "你好 世界");
        assert_eq!(title_from(""), "新对话");
        let long = "一二三四五六七八九十".repeat(5);
        assert_eq!(title_from(&long).chars().count(), 30);
    }

    #[test]
    fn append_and_read_messages_roundtrip() {
        let ts = open_store();
        let id = ts.store.ensure(None, "测试", "default").unwrap();

        let user = ChatMessage::user("第一问");
        ts.store.append(&id, &user).unwrap();
        let assistant = ChatMessage::assistant_with_calls(
            Some("我来查".into()),
            Some("思考".into()),
            vec![ToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                },
            }],
        );
        ts.store.append(&id, &assistant).unwrap();
        let tool = ChatMessage::tool_result("c1", "read_file", "文件内容", true);
        ts.store.append(&id, &tool).unwrap();
        let image = ChatMessage::user_with_images(
            "看截图",
            vec![crate::model::ImagePart::png(
                "aGVsbG8=".into(),
                10,
                20,
                Some("shot.png".into()),
            )],
        );
        ts.store.append(&id, &image).unwrap();

        let messages = ts.store.messages(&id, 0).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text(), "第一问");
        assert_eq!(messages[1].reasoning_content.as_deref(), Some("思考"));
        assert_eq!(messages[1].tool_calls[0].function.name, "read_file");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].tool_ok, Some(true));
        assert_eq!(messages[3].images.len(), 1);
        assert_eq!(messages[3].images[0].width, 10);

        // limit 只取最后 2 条，且保持时间顺序。
        let tail = ts.store.messages(&id, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1].images.len(), 1);

        let summary = ts.store.get(&id).unwrap().unwrap();
        assert_eq!(summary.message_count, 4);
        assert_eq!(summary.preview, "看截图");
    }

    #[test]
    fn list_filters_by_title_or_content() {
        let ts = open_store();
        let a = ts.store.ensure(None, "邮件整理", "").unwrap();
        let b = ts.store.ensure(None, "代码审查", "").unwrap();
        ts.store
            .append(&a, &ChatMessage::user("附件里提到了季度报告"))
            .unwrap();
        let by_title = ts.store.list(Some("代码"), 10).unwrap();
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].id, b);
        let by_content = ts.store.list(Some("季度报告"), 10).unwrap();
        assert_eq!(by_content.len(), 1);
        assert_eq!(by_content[0].id, a);
        assert!(ts.store.list(Some("不存在的词"), 10).unwrap().is_empty());
    }

    #[test]
    fn search_messages_returns_snippet() {
        let ts = open_store();
        let id = ts.store.ensure(None, "搜索测试", "").unwrap();
        let content = format!("{}关键错误{}", "前置内容".repeat(20), "后置内容".repeat(20));
        ts.store.append(&id, &ChatMessage::user(content)).unwrap();
        let hits = ts.store.search_messages("关键错误", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, id);
        assert_eq!(hits[0].conversation_title, "搜索测试");
        assert!(hits[0].snippet.contains("关键错误"));
        assert!(hits[0].snippet.contains('…'));
        assert!(ts.store.search_messages("  ", 10).unwrap().is_empty());
    }

    #[test]
    fn rename_delete_and_stats() {
        let ts = open_store();
        let id = ts.store.ensure(None, "旧标题", "").unwrap();
        ts.store.append(&id, &ChatMessage::user("hello")).unwrap();
        ts.store.rename(&id, "新标题").unwrap();
        assert_eq!(ts.store.get(&id).unwrap().unwrap().title, "新标题");
        assert!(ts.store.rename(&id, "  ").is_err());

        let (convs, msgs) = ts.store.stats().unwrap();
        assert_eq!((convs, msgs), (1, 1));
        assert!(ts.store.delete(&id).unwrap());
        assert!(!ts.store.delete(&id).unwrap());
        assert_eq!(ts.store.stats().unwrap(), (0, 0));
        assert!(ts.store.messages(&id, 0).unwrap().is_empty());
    }

    #[test]
    fn append_creates_missing_conversation() {
        let ts = open_store();
        ts.store
            .append("orphan", &ChatMessage::user("没有 ensure 过"))
            .unwrap();
        let summary = ts.store.get("orphan").unwrap().unwrap();
        assert_eq!(summary.title, "没有 ensure 过");
        assert_eq!(ts.store.messages("orphan", 0).unwrap().len(), 1);
    }

    #[test]
    fn messages_before_paginates_with_cursor() {
        let ts = open_store();
        let id = ts.store.ensure(None, "分页测试", "").unwrap();

        // 空会话：没有消息，last_seq = -1。
        assert!(ts.store.messages_before(&id, None, 10).unwrap().is_empty());
        assert_eq!(ts.store.last_seq(&id).unwrap(), -1);

        for index in 0..3 {
            ts.store
                .append(&id, &ChatMessage::user(format!("消息{index}")))
                .unwrap();
        }
        let all = ts.store.messages_before(&id, None, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "应按 seq 升序返回"
        );
        assert_eq!(ts.store.last_seq(&id).unwrap(), 3);

        // before=None 取最后 2 条。
        let latest = ts.store.messages_before(&id, None, 2).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].0, 2);
        assert_eq!(latest[1].1.text(), "消息2");

        // 跨页连续：用上一页第一条的 seq 作为下一页的 before。
        let older = ts.store.messages_before(&id, Some(2), 2).unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].0, 1);
        assert_eq!(older[0].1.text(), "消息0");

        // 边界：before=1 之前没有消息；超出最大 seq 返回全部。
        assert!(
            ts.store
                .messages_before(&id, Some(1), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ts.store.messages_before(&id, Some(99), 10).unwrap().len(),
            3
        );
        assert_eq!(ts.store.last_seq("不存在的会话").unwrap(), -1);
    }

    #[test]
    fn summary_roundtrip_and_clear() {
        let ts = open_store();
        let id = ts.store.ensure(None, "摘要测试", "").unwrap();
        assert!(ts.store.summary(&id).unwrap().is_none());

        ts.store
            .save_summary(&id, "早前的对话摘要", 3, 2, 1000, 120)
            .unwrap();
        let stored = ts.store.summary(&id).unwrap().unwrap();
        assert_eq!(stored.summary, "早前的对话摘要");
        assert_eq!(stored.covered_len, 3);
        assert_eq!(stored.kept, 2);
        assert_eq!(stored.before_tokens, 1000);
        assert_eq!(stored.after_tokens, 120);

        // 覆盖同一会话。
        ts.store
            .save_summary(&id, "新的摘要", 5, 4, 2000, 150)
            .unwrap();
        assert_eq!(ts.store.summary(&id).unwrap().unwrap().summary, "新的摘要");

        assert!(ts.store.clear_summary(&id).unwrap());
        assert!(!ts.store.clear_summary(&id).unwrap());
        assert!(ts.store.summary(&id).unwrap().is_none());
    }

    #[test]
    fn plan_roundtrip_and_clear() {
        let ts = open_store();
        let id = ts.store.ensure(None, "计划测试", "").unwrap();
        assert!(ts.store.plan(&id).unwrap().is_empty());

        let steps = vec![
            crate::model::PlanStep {
                title: "读代码".into(),
                status: "done".into(),
            },
            crate::model::PlanStep {
                title: "改实现".into(),
                status: "in_progress".into(),
            },
        ];
        ts.store.save_plan(&id, &steps).unwrap();
        assert_eq!(ts.store.plan(&id).unwrap(), steps);
        // 别的会话不受影响（会话隔离）。
        let other = ts.store.ensure(None, "另一个会话", "").unwrap();
        assert!(ts.store.plan(&other).unwrap().is_empty());

        // 空列表 = 清空。
        ts.store.save_plan(&id, &[]).unwrap();
        assert!(ts.store.plan(&id).unwrap().is_empty());
        assert!(!ts.store.clear_plan(&id).unwrap());
    }

    #[test]
    fn like_escaping_and_snippet_helpers() {
        assert_eq!(escape_like("100%_x"), "100\\%\\_x");
        let snippet = make_snippet("abcdefg", "cd");
        assert!(snippet.contains("cd"));
        assert_eq!(make_snippet("", "x"), "");
    }
}
