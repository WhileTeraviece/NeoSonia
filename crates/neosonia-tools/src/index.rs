//! 类 Everything 的本地文件名索引（SQLite）。
//!
//! 设计要点：
//! - 索引库放在 `Paths.file_index_db`（`data/file_index.sqlite3`），单连接 +
//!   `parking_lot::Mutex` 串行化写，WAL 模式，扫描时按 2000 条一个事务批量 upsert；
//! - **不依赖 Everything / Windows Search**，用 `walkdir` 自己遍历；只索引文件名和路径，
//!   不读文件内容（内容搜索走 `search_files`）；
//! - `FileIndex::global(&paths)` 用 `OnceLock` 全局共享；数据库打不开时自动降级到
//!   内存库，功能可用（重启即丢），绝不 panic；
//! - 后台线程：先全量扫描，之后每 10 分钟做一次「全量对比」增量扫描（只写有变化的行，
//!   删除已消失的条目），扫描期间 `busy` 置位，`index_stats` 能报告进度；
//! - 工具：`find_files` / `reindex_files` / `index_stats`；所有错误中文、零 emoji、
//!   不 panic，长扫描响应 `ctx.is_cancelled()` 与超时。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use neosonia_core::config::{IndexSettings, Paths, Settings};
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::{Tool, ToolRegistry};
use neosonia_core::util::{expand_tilde, human_bytes, now_ms, truncate_chars};
use parking_lot::{Mutex, RwLock};
use rusqlite::{named_params, params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ============================================================
// 常量
// ============================================================

/// 搜索默认返回条数。
pub const DEFAULT_LIMIT: usize = 50;
/// 搜索最多返回条数（契约规定）。
pub const MAX_LIMIT: usize = 200;
/// 扫描时每个事务批量的条目数。
pub const SCAN_BATCH: usize = 2_000;
/// 后台增量扫描间隔。
pub const INCREMENTAL_INTERVAL_SECS: u64 = 10 * 60;
/// 后台线程等待配置可用的轮询间隔。
const IDLE_RECHECK_SECS: u64 = 60;
/// 进度回传间隔（条目数）。
const PROGRESS_EVERY: u64 = 256;

/// 永远跳过的目录名（小写比较）。用户配置的 `index.exclude` 会追加进来。
const SYSTEM_DIRS: &[&str] = &[
    "windows",
    "appdata",
    "node_modules",
    ".git",
    "target",
    "$recycle.bin",
    "system volume information",
    "$windows.~bt",
    "$windows.~ws",
];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    path        TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    name_lower  TEXT NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    modified_ms INTEGER NOT NULL DEFAULT 0,
    is_dir      INTEGER NOT NULL DEFAULT 0,
    root        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_name_lower ON files(name_lower);
CREATE INDEX IF NOT EXISTS idx_root ON files(root);
"#;

/// 只在内容有变化时真正写行，避免每次全量对比把整张表重写一遍。
const UPSERT_SQL: &str = r#"
INSERT INTO files (path, name, name_lower, size, modified_ms, is_dir, root)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(path) DO UPDATE SET
    name = excluded.name,
    name_lower = excluded.name_lower,
    size = excluded.size,
    modified_ms = excluded.modified_ms,
    is_dir = excluded.is_dir,
    root = excluded.root
WHERE files.name IS NOT excluded.name
   OR files.name_lower IS NOT excluded.name_lower
   OR files.size IS NOT excluded.size
   OR files.modified_ms IS NOT excluded.modified_ms
   OR files.is_dir IS NOT excluded.is_dir
   OR files.root IS NOT excluded.root
"#;

// ============================================================
// 公共数据模型
// ============================================================

/// 一条索引条目。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexEntry {
    /// 绝对路径。
    pub path: String,
    /// 文件名（不含目录）。
    pub name: String,
    /// 字节数（目录为 0）。
    pub size: u64,
    /// 修改时间（Unix 毫秒；未知为 0）。
    pub modified_ms: i64,
    pub is_dir: bool,
}

impl IndexEntry {
    fn depth(&self) -> usize {
        self.path.chars().filter(|c| *c == '\\' || *c == '/').count()
    }
}

/// 一次扫描的统计。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// 本次真正参与扫描的根目录（已展开 `~`、已去重嵌套、已过滤不存在的）。
    pub roots: Vec<String>,
    /// 扫描到的条目总数（文件 + 目录）。
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    /// 实际写入/更新的行数（未变化的行不计）。
    pub upserted: u64,
    /// 清理掉的过期条目数。
    pub deleted: u64,
    /// 跳过的条目数（符号链接等）。
    pub skipped: u64,
    /// 无法读取的条目/目录数。
    pub errors: u64,
    /// 是否因为 `max_entries` 上限而提前结束。
    pub truncated: bool,
    /// 是否被取消。
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

/// 索引库状态。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub total_entries: u64,
    pub files: u64,
    pub dirs: u64,
    /// 数据库字节数（内存降级时为 0）。
    pub db_bytes: u64,
    /// 当前是否正在扫描。
    pub scanning: bool,
    /// 当前扫描已处理的条目数。
    pub scanning_entries: u64,
    /// 当前扫描已进行的毫秒数。
    pub scanning_elapsed_ms: u64,
    /// 数据库打开失败、已降级为内存索引。
    pub degraded: bool,
    /// 上一次扫描的统计。
    pub last_scan: Option<ScanStats>,
}

/// 搜索一页结果（含总数）。
#[derive(Clone, Debug, Default)]
pub struct SearchOutcome {
    /// 满足条件的总条数（glob 过滤之前）。
    pub total: u64,
    pub entries: Vec<IndexEntry>,
}

// ============================================================
// FileIndex
// ============================================================

/// 全局文件索引。线程安全；写操作串行化。
pub struct FileIndex {
    conn: Mutex<Option<Connection>>,
    db_path: PathBuf,
    degraded: AtomicBool,
    busy: AtomicBool,
    progress_scanned: AtomicU64,
    progress_started_ms: AtomicI64,
    last_scan: Mutex<Option<ScanStats>>,
    /// 同一时间只允许一个扫描（后台线程与手动 reindex 互斥）。
    scan_lock: Mutex<()>,
    bg_started: AtomicBool,
}

impl FileIndex {
    /// 打开（或创建）指定路径的索引库；失败时降级为内存库。
    pub fn open_at(path: PathBuf) -> Self {
        let mut degraded = false;
        let conn = match Self::connect_file(&path) {
            Ok(conn) => Some(conn),
            Err(e) => {
                degraded = true;
                tracing::warn!(path = %path.display(), error = %e, "打开文件索引库失败，降级为内存索引");
                match Self::connect_memory() {
                    Ok(conn) => Some(conn),
                    Err(e) => {
                        tracing::warn!(error = %e, "创建内存文件索引也失败，索引功能将不可用");
                        None
                    }
                }
            }
        };
        Self {
            conn: Mutex::new(conn),
            db_path: path,
            degraded: AtomicBool::new(degraded),
            busy: AtomicBool::new(false),
            progress_scanned: AtomicU64::new(0),
            progress_started_ms: AtomicI64::new(0),
            last_scan: Mutex::new(None),
            scan_lock: Mutex::new(()),
            bg_started: AtomicBool::new(false),
        }
    }

    /// 进程内全局唯一实例（用 `Paths.file_index_db`）。打开失败也不会 panic。
    pub fn global(paths: &Paths) -> &'static FileIndex {
        static GLOBAL: OnceLock<FileIndex> = OnceLock::new();
        GLOBAL.get_or_init(|| FileIndex::open_at(paths.file_index_db.clone()))
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// 是否降级为内存索引（重启后数据丢失）。
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    fn connect_file(path: &Path) -> Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建索引库目录失败：{}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开索引库失败：{}", path.display()))?;
        Self::init_conn(conn)
    }

    fn connect_memory() -> Result<Connection> {
        let conn = Connection::open_in_memory().context("创建内存索引库失败")?;
        Self::init_conn(conn)
    }

    fn init_conn(conn: Connection) -> Result<Connection> {
        // WAL + NORMAL：索引数据丢了可以重建，不需要每次 fsync。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.pragma_update(None, "temp_store", "MEMORY");
        let _ = conn.busy_timeout(Duration::from_secs(10));
        conn.execute_batch(SCHEMA).context("初始化索引表失败")?;
        Ok(conn)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.conn.lock();
        let conn = guard.as_mut().ok_or_else(|| {
            anyhow!("文件索引数据库不可用：初始化失败且内存降级也失败，请检查 data 目录权限后重启 NeoSonia")
        })?;
        f(conn)
    }

    // --------------------------------------------------------
    // 扫描
    // --------------------------------------------------------

    /// 同步建立/刷新索引（契约接口）。不可取消，适合小目录或测试。
    pub fn scan_now(
        &self,
        roots: &[PathBuf],
        exclude: &[String],
        max_entries: u32,
    ) -> Result<ScanStats> {
        self.scan_with_cancel(roots, exclude, max_entries, || false)
    }

    /// 可取消的扫描：`cancel` 返回 true 时尽快停止（已提交的批次保留、不做过期清理）。
    pub fn scan_with_cancel(
        &self,
        roots: &[PathBuf],
        exclude: &[String],
        max_entries: u32,
        cancel: impl Fn() -> bool,
    ) -> Result<ScanStats> {
        let _scan_guard = self
            .scan_lock
            .try_lock()
            .ok_or_else(|| anyhow!("已有一次索引扫描正在进行，请稍后再试。"))?;

        let started = Instant::now();
        let started_ms = now_ms();
        self.busy.store(true, Ordering::SeqCst);
        self.progress_scanned.store(0, Ordering::SeqCst);
        self.progress_started_ms.store(started_ms, Ordering::SeqCst);

        let result = self.scan_inner(roots, exclude, max_entries, &cancel, started);

        self.busy.store(false, Ordering::SeqCst);
        self.progress_started_ms.store(0, Ordering::SeqCst);

        match result {
            Ok(stats) => {
                self.progress_scanned.store(stats.entries, Ordering::SeqCst);
                *self.last_scan.lock() = Some(stats.clone());
                Ok(stats)
            }
            Err(e) => Err(e),
        }
    }

    fn scan_inner(
        &self,
        roots: &[PathBuf],
        exclude: &[String],
        max_entries: u32,
        cancel: &dyn Fn() -> bool,
        started: Instant,
    ) -> Result<ScanStats> {
        let roots = resolve_roots(roots);
        if roots.is_empty() {
            bail!(
                "没有可索引的根目录：请检查 index.roots 配置（支持 ~），或确认路径存在且是目录。"
            );
        }
        let root_strings: Vec<String> =
            roots.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let exclude_set = build_exclude_set(exclude);
        let max_entries = if max_entries == 0 { u32::MAX } else { max_entries };

        let mut stats = ScanStats { roots: root_strings.clone(), ..Default::default() };
        let mut rows: Vec<PendingRow> = Vec::with_capacity(SCAN_BATCH);
        let mut cancelled = false;
        let mut total_checked: u64 = 0;

        self.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS scan_seen(path TEXT PRIMARY KEY);",
            )
            .context("准备扫描临时表失败")?;
            conn.execute("DELETE FROM scan_seen", [])
                .context("清空扫描临时表失败")?;

            'roots: for root in &roots {
                let root_str = root.to_string_lossy().to_string();
                let walker = walkdir::WalkDir::new(root)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|entry| {
                        if entry.depth() == 0 {
                            return true;
                        }
                        if !entry.file_type().is_dir() {
                            return true;
                        }
                        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                        !exclude_set.contains(&name)
                    });

                for walked in walker {
                    total_checked += 1;
                    if cancel() {
                        cancelled = true;
                        break 'roots;
                    }
                    if total_checked % PROGRESS_EVERY == 0 {
                        self.progress_scanned.store(stats.entries, Ordering::SeqCst);
                    }
                    let entry = match walked {
                        Ok(e) => e,
                        Err(_) => {
                            stats.errors += 1;
                            continue;
                        }
                    };
                    if entry.depth() == 0 {
                        continue;
                    }
                    if entry.path_is_symlink() {
                        stats.skipped += 1;
                        continue;
                    }
                    let meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            stats.errors += 1;
                            continue;
                        }
                    };
                    let is_dir = meta.is_dir();
                    if !is_dir && !meta.is_file() {
                        stats.skipped += 1;
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.is_empty() {
                        stats.skipped += 1;
                        continue;
                    }
                    let path = entry.path().to_string_lossy().to_string();
                    // 不索引索引库自身（含 WAL/SHM 附属文件），避免自己扫自己。
                    let db_path = self.db_path.to_string_lossy();
                    if path == db_path || path.starts_with(&format!("{db_path}-")) {
                        stats.skipped += 1;
                        continue;
                    }
                    let modified_ms = meta.modified().map(system_time_ms).unwrap_or(0);
                    let size = if is_dir { 0 } else { meta.len() };
                    rows.push(PendingRow {
                        path,
                        name_lower: name.to_ascii_lowercase(),
                        name,
                        size,
                        modified_ms,
                        is_dir,
                        root: root_str.clone(),
                    });
                    stats.entries += 1;
                    if is_dir {
                        stats.dirs += 1;
                    } else {
                        stats.files += 1;
                    }
                    if rows.len() >= SCAN_BATCH {
                        flush_batch(conn, &mut rows, &mut stats)?;
                        self.progress_scanned.store(stats.entries, Ordering::SeqCst);
                    }
                    if stats.entries >= max_entries as u64 {
                        stats.truncated = true;
                        break 'roots;
                    }
                }
            }

            flush_batch(conn, &mut rows, &mut stats)?;

            // 全量对比清理：只有「成功走完（未取消/未截断/无读取错误）」才删除过期条目，
            // 避免把暂时读不到的目录误删。
            if !cancelled && !stats.truncated && stats.errors == 0 {
                for root_str in &root_strings {
                    let deleted = conn
                        .execute(
                            "DELETE FROM files WHERE root = ?1
                             AND path NOT IN (SELECT path FROM scan_seen)",
                            params![root_str],
                        )
                        .context("清理过期索引条目失败")?;
                    stats.deleted += deleted as u64;
                }
            }
            let _ = conn.execute("DELETE FROM scan_seen", []);
            Ok(())
        })?;

        stats.cancelled = cancelled;
        stats.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(stats)
    }

    /// 当前扫描进度：(已处理条目, 是否在扫描, 已用毫秒)。
    pub fn progress(&self) -> (u64, bool, u64) {
        let entries = self.progress_scanned.load(Ordering::SeqCst);
        let busy = self.busy.load(Ordering::SeqCst);
        let started = self.progress_started_ms.load(Ordering::SeqCst);
        let elapsed = if started > 0 {
            (now_ms() - started).max(0) as u64
        } else {
            0
        };
        (entries, busy, elapsed)
    }

    // --------------------------------------------------------
    // 搜索
    // --------------------------------------------------------

    /// 按文件名/路径搜索（契约接口）。
    pub fn search(
        &self,
        query: &str,
        path_filter: Option<&Path>,
        glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IndexEntry>> {
        Ok(self
            .search_with_total(query, path_filter, glob, limit, false)?
            .entries)
    }

    /// 搜索并返回命中总数；`dirs_only` 只返回目录。
    pub fn search_with_total(
        &self,
        query: &str,
        path_filter: Option<&Path>,
        glob: Option<&str>,
        limit: usize,
        dirs_only: bool,
    ) -> Result<SearchOutcome> {
        let raw_query = query.trim();
        if raw_query.is_empty() {
            bail!("搜索关键词为空：find_files 的 query 不能为空。");
        }
        let q = raw_query.to_lowercase();
        let like = format!("%{}%", like_escape(&q));
        let prefix = format!("{}%", like_escape(&q));
        let limit = limit.clamp(1, MAX_LIMIT);

        let (path_eq, path_like) = match path_filter {
            Some(p) => {
                let lower = normalize_path_string(&p.to_string_lossy()).to_lowercase();
                let mut with_sep = lower.trim_end_matches(['\\', '/']).to_string();
                with_sep.push(std::path::MAIN_SEPARATOR);
                (Some(lower), Some(format!("{}%", like_escape(&with_sep))))
            }
            None => (None, None),
        };

        let globs = compile_glob_filter(glob)?;
        let dirs_cond = if dirs_only { " AND is_dir = 1" } else { "" };
        // 注意：ESCAPE '\' 里的反斜杠在 raw string 里原样保留。
        let where_clause = format!(
            r" WHERE (name_lower LIKE :like ESCAPE '\'
                OR lower(path) LIKE :like ESCAPE '\')
              AND (:path IS NULL OR lower(path) = :path
                   OR lower(path) LIKE :pathlike ESCAPE '\'){dirs_cond}"
        );

        let fetch_limit = if globs.is_some() {
            limit.saturating_mul(20).clamp(limit, 5_000)
        } else {
            limit.saturating_mul(4).max(limit)
        };

        let count_sql = format!("SELECT COUNT(*) FROM files{where_clause}");
        let (like_count, path_eq_count, path_like_count) =
            (like.clone(), path_eq.clone(), path_like.clone());
        let total: i64 = self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&count_sql)?;
            let value = stmt.query_row(
                named_params! {
                    ":like": like_count,
                    ":path": path_eq_count,
                    ":pathlike": path_like_count,
                },
                |row| row.get::<_, i64>(0),
            )?;
            Ok(value)
        })?;

        let query_sql = format!(
            r"SELECT path, name, size, modified_ms, is_dir FROM files{where_clause}
              ORDER BY (CASE
                    WHEN name_lower = :q THEN 0
                    WHEN name_lower LIKE :prefix ESCAPE '\' THEN 1
                    WHEN name_lower LIKE :like ESCAPE '\' THEN 2
                    ELSE 3 END) ASC,
                length(path) ASC, path ASC
              LIMIT :limit"
        );

        let mut entries: Vec<IndexEntry> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&query_sql)?;
            let rows = stmt.query_map(
                named_params! {
                    ":q": q,
                    ":prefix": prefix,
                    ":like": like,
                    ":path": path_eq,
                    ":pathlike": path_like,
                    ":limit": fetch_limit as i64,
                },
                |row| {
                    Ok(IndexEntry {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        size: row.get::<_, i64>(2)?.max(0) as u64,
                        modified_ms: row.get(3)?,
                        is_dir: row.get::<_, i64>(4)? != 0,
                    })
                },
            )?;
            let mut out = Vec::new();
            for item in rows {
                out.push(item?);
            }
            Ok(out)
        })?;

        if let Some(set) = &globs {
            entries.retain(|e| glob_matches(set, e));
        }

        // 精确排序：完全匹配(0) > 前缀(1) > 包含(2) > 路径包含(3)，同级目录浅优先。
        entries.sort_by(|a, b| {
            rank_of(a, &q)
                .cmp(&rank_of(b, &q))
                .then_with(|| a.depth().cmp(&b.depth()))
                .then_with(|| a.path.len().cmp(&b.path.len()))
                .then_with(|| a.path.cmp(&b.path))
        });
        entries.truncate(limit);

        Ok(SearchOutcome { total: total.max(0) as u64, entries })
    }

    // --------------------------------------------------------
    // 统计 / 清理
    // --------------------------------------------------------

    pub fn stats(&self) -> Result<IndexStats> {
        let (scanned, busy, elapsed) = self.progress();
        let (total, files, dirs) = self.with_conn(|conn| {
            let row = conn.query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN is_dir = 0 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN is_dir = 1 THEN 1 ELSE 0 END), 0)
                 FROM files",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?;
            Ok((row.0.max(0) as u64, row.1.max(0) as u64, row.2.max(0) as u64))
        })?;
        let db_bytes = self.with_conn(|conn| {
            let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
        })?;
        Ok(IndexStats {
            total_entries: total,
            files,
            dirs,
            db_bytes,
            scanning: busy,
            scanning_entries: scanned,
            scanning_elapsed_ms: elapsed,
            degraded: self.is_degraded(),
            last_scan: self.last_scan.lock().clone(),
        })
    }

    pub fn last_scan(&self) -> Option<ScanStats> {
        self.last_scan.lock().clone()
    }

    /// 清空索引（不删除数据库文件）。
    pub fn clear(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM files", [])?;
            let _ = conn.execute("DELETE FROM scan_seen", []);
            Ok(())
        })?;
        *self.last_scan.lock() = None;
        Ok(())
    }

    // --------------------------------------------------------
    // 后台扫描
    // --------------------------------------------------------

    /// 启动后台扫描线程：先全量扫描，之后每隔 10 分钟做一次全量对比增量。
    ///
    /// 线程是 daemon 语义（进程退出即结束），重复调用只会启动一次。
    pub fn start_background(&'static self, settings: Arc<RwLock<Settings>>) {
        if self.bg_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("neosonia-file-index".to_string())
            .spawn(move || {
                // 等 UI 启动完再做第一次全量，避免开机时抢磁盘。
                std::thread::sleep(Duration::from_secs(2));
                loop {
                    let snapshot = settings.read().clone();
                    let cfg: &IndexSettings = &snapshot.index;
                    if !cfg.enabled || !cfg.autoscan {
                        sleep_secs(IDLE_RECHECK_SECS);
                        continue;
                    }
                    let roots: Vec<PathBuf> = cfg.roots.iter().map(|r| root_from_str(r)).collect();
                    let max_entries = cfg.max_entries.max(1);
                    match self.scan_with_cancel(&roots, &cfg.exclude, max_entries, || false) {
                        Ok(stats) => tracing::info!(
                            entries = stats.entries,
                            upserted = stats.upserted,
                            deleted = stats.deleted,
                            elapsed_ms = stats.elapsed_ms,
                            "文件索引后台扫描完成"
                        ),
                        Err(e) => tracing::warn!(error = %e, "文件索引后台扫描失败"),
                    }
                    sleep_secs(INCREMENTAL_INTERVAL_SECS);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "启动文件索引后台线程失败");
        }
    }
}

// ============================================================
// 扫描辅助
// ============================================================

struct PendingRow {
    path: String,
    name: String,
    name_lower: String,
    size: u64,
    modified_ms: i64,
    is_dir: bool,
    root: String,
}

fn flush_batch(conn: &mut Connection, rows: &mut Vec<PendingRow>, stats: &mut ScanStats) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().context("开始索引写事务失败")?;
    {
        let mut upsert = tx.prepare_cached(UPSERT_SQL)?;
        let mut mark = tx.prepare_cached("INSERT OR IGNORE INTO scan_seen(path) VALUES (?1)")?;
        for row in rows.drain(..) {
            let changed = upsert
                .execute(params![
                    row.path,
                    row.name,
                    row.name_lower,
                    row.size as i64,
                    row.modified_ms,
                    row.is_dir as i64,
                    row.root,
                ])
                .context("写入索引条目失败")?;
            stats.upserted += changed as u64;
            mark.execute([row.path.as_str()])
                .context("写入扫描临时表失败")?;
        }
    }
    tx.commit().context("提交索引写事务失败")?;
    Ok(())
}

fn build_exclude_set(extra: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = SYSTEM_DIRS.iter().map(|s| s.to_ascii_lowercase()).collect();
    for name in extra {
        let name = name.trim().to_ascii_lowercase();
        if !name.is_empty() {
            set.insert(name);
        }
    }
    set
}

/// 展开 `~`、转绝对路径、去掉嵌套（父目录已被更外层根覆盖的子根丢弃）、过滤不存在的。
fn resolve_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = roots
        .iter()
        .map(|p| normalize_root_path(p.clone()))
        .filter(|p| p.is_dir())
        .collect();
    candidates.sort_by_key(|p| p.components().count());
    candidates.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if kept.iter().any(|root| candidate == *root || candidate.starts_with(root)) {
            continue;
        }
        kept.push(candidate);
    }
    kept
}

fn root_from_str(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return PathBuf::new();
    }
    let expanded = expand_tilde(trimmed);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    normalize_root_path(neosonia_core::util::normalize_lexical(&absolute))
}

fn normalize_root_path(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() {
        return path;
    }
    let normalized = neosonia_core::util::normalize_lexical(&path);
    #[cfg(windows)]
    {
        let text = normalized.to_string_lossy().replace('/', "\\");
        PathBuf::from(text)
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn normalize_path_string(raw: &str) -> String {
    #[cfg(windows)]
    {
        raw.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        raw.to_string()
    }
}

fn system_time_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sleep_secs(secs: u64) {
    std::thread::sleep(Duration::from_secs(secs));
}

// ============================================================
// 搜索辅助
// ============================================================

fn like_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            _ => out.push(ch),
        }
    }
    out
}

fn compile_glob_filter(glob: Option<&str>) -> Result<Option<globset::GlobSet>> {
    let Some(raw) = glob else {
        return Ok(None);
    };
    let patterns: Vec<&str> = raw
        .split([',', ';'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let built = globset::GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .map_err(|e| anyhow!("glob 模式 {pattern:?} 非法：{e}"))?;
        builder.add(built);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| anyhow!("构建 glob 过滤失败：{e}"))
}

fn glob_matches(set: &globset::GlobSet, entry: &IndexEntry) -> bool {
    let normalized = entry.path.replace('\\', "/");
    set.is_match(&normalized) || set.is_match(&entry.name)
}

fn rank_of(entry: &IndexEntry, query_lower: &str) -> u8 {
    let name = entry.name.to_lowercase();
    if name == query_lower {
        0
    } else if name.starts_with(query_lower) {
        1
    } else if name.contains(query_lower) {
        2
    } else if entry.path.to_lowercase().contains(query_lower) {
        3
    } else {
        4
    }
}

fn format_timestamp(ms: i64) -> String {
    if ms <= 0 {
        return "未知".to_string();
    }
    match chrono::TimeZone::timestamp_millis_opt(&chrono::Local, ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "未知".to_string(),
    }
}

fn format_entry(index: usize, entry: &IndexEntry) -> String {
    let kind = if entry.is_dir { " [目录]" } else { "" };
    let size = if entry.is_dir { "-".to_string() } else { human_bytes(entry.size) };
    format!(
        "{}. {}{}  · {} · {}",
        index + 1,
        entry.path,
        kind,
        size,
        format_timestamp(entry.modified_ms)
    )
}

// ============================================================
// 工具：find_files / reindex_files / index_stats
// ============================================================

crate::tool_impl!(
    FindFilesTool,
    "find_files",
    "在本地文件索引里按文件名/路径极速搜索（类 Everything，不依赖任何外部程序）。\n\
     什么时候用：用户想找某个名字的文件/目录在哪（例如“找一下三体.pdf”“找所有 .rs 文件”），\
     比 search_files 快得多，因为走的是已经建好的索引。\n\
     参数：query 关键词（必填，大小写不敏感的子串；完全匹配/前缀匹配会排在前面）；\
     path 只在这个目录（含子目录）下找（可选）；glob 通配符过滤（可选，如 \"*.rs\"，多个用逗号分隔）；\
     limit 返回条数（默认 50，最大 200）；dirs_only 只找目录（默认 false）。\n\
     注意：只搜文件名和路径，不搜文件内容（内容搜索请用 search_files）；\
     索引为空时先调用 reindex_files，或在设置页开启本地索引等待后台扫描。",
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "文件名/路径关键词，大小写不敏感"},
            "path": {"type": "string", "description": "只在这个目录下搜索（可选，支持 ~）"},
            "glob": {"type": "string", "description": "通配符过滤，如 *.rs、*.png,*.jpg（可选）"},
            "limit": {"type": "integer", "description": "返回条数，默认 50，最大 200"},
            "dirs_only": {"type": "boolean", "description": "只返回目录，默认 false"}
        },
        "required": ["query"]
    }),
    find_files_handler
);

crate::tool_impl!(
    ReindexFilesTool,
    "reindex_files",
    "建立或刷新本地文件索引。首次使用 find_files 前，或文件变动很大、索引明显过期时调用。\n\
     参数：paths 要扫描的目录（可选，字符串或数组，支持 ~；不传则用设置页 index.roots 里的根目录，\
     此时不写回设置）；timeout_secs 最长扫描秒数（默认 600，最大 3600）。\n\
     扫描会跳过系统目录、node_modules、.git、target、AppData 等，并按 index.max_entries 截断；\
     可以取消；扫描期间 UI 状态里能看到进度。返回扫描条目数、写入数、清理数和耗时。",
    json!({
        "type": "object",
        "properties": {
            "paths": {
                "oneOf": [
                    {"type": "string"},
                    {"type": "array", "items": {"type": "string"}}
                ],
                "description": "要扫描的目录（可选；不传则用设置里的索引根目录）"
            },
            "timeout_secs": {"type": "integer", "description": "最长扫描秒数，默认 600，最大 3600"}
        }
    }),
    reindex_files_handler
);

crate::tool_impl!(
    IndexStatsTool,
    "index_stats",
    "查看本地文件索引的状态：索引了多少文件/目录、数据库大小、是否正在扫描、上次扫描耗时、\
     索引根目录和设置开关。\n\
     什么时候用：find_files 找不到东西、怀疑索引没建好，或用户问“索引建到哪了 / 占多大空间”时。无参数。",
    json!({"type": "object", "properties": {}}),
    index_stats_handler
);

/// 注册 3 个索引工具（由 `lib.rs::register_all` 调用）。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(FindFilesTool));
    reg.register(Arc::new(ReindexFilesTool));
    reg.register(Arc::new(IndexStatsTool));
}

/// 三个工具的规格（测试/文档用）。
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        FindFilesTool.spec(),
        ReindexFilesTool.spec(),
        IndexStatsTool.spec(),
    ]
}

// ------------------------------------------------------------
// find_files
// ------------------------------------------------------------

async fn find_files_handler(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let query = match crate::arg_str_req(&args, "query") {
        Ok(q) => q,
        Err(e) => return e,
    };
    let limit = crate::arg_u64(&args, "limit", DEFAULT_LIMIT as u64).clamp(1, MAX_LIMIT as u64)
        as usize;
    let dirs_only = crate::arg_bool(&args, "dirs_only", false);
    let glob = crate::arg_str(&args, "glob").filter(|g| !g.trim().is_empty());
    let path_filter = crate::arg_str(&args, "path")
        .filter(|p| !p.trim().is_empty())
        .map(|raw| {
            let expanded = expand_tilde(raw.trim());
            if expanded.is_absolute() {
                neosonia_core::util::normalize_lexical(&expanded)
            } else {
                neosonia_core::util::normalize_lexical(&crate::workspace_dir(ctx).join(expanded))
            }
        });

    let index = FileIndex::global(&ctx.paths());
    ctx.phase("正在查询本地文件索引...");
    let started = Instant::now();
    let task = {
        let query = query.clone();
        let glob = glob.clone();
        let path_filter = path_filter.clone();
        tokio::task::spawn_blocking(move || {
            index.search_with_total(
                &query,
                path_filter.as_deref(),
                glob.as_deref(),
                limit,
                dirs_only,
            )
        })
    };
    let joined = tokio::time::timeout(Duration::from_secs(30), task).await;
    let outcome = match joined {
        Ok(Ok(Ok(outcome))) => outcome,
        Ok(Ok(Err(e))) => return ToolOutcome::err(format!("查询本地文件索引失败：{e}")),
        Ok(Err(join_err)) => {
            return ToolOutcome::err(format!("查询本地文件索引的后台线程异常结束：{join_err}"))
        }
        Err(_) => return ToolOutcome::err("查询本地文件索引超时（30 秒），请稍后重试。"),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if outcome.total == 0 {
        return ToolOutcome::ok(format!(
            "本地索引里没有匹配“{query}”的条目（查询用时 {elapsed_ms} 毫秒）。\n\
             如果索引还没建立，请先调用 reindex_files，或在设置页开启本地索引；\
             索引只收录文件名/路径，不搜文件内容（内容搜索用 search_files）。"
        ))
        .with_display(format!("0 个匹配“{query}”"));
    }

    let mut lines = Vec::with_capacity(outcome.entries.len() + 3);
    lines.push(format!(
        "本地索引命中 {} 个匹配“{}”的条目（索引共 {} 条），显示前 {} 条（查询用时 {} 毫秒）：",
        outcome.total,
        query,
        index.stats().map(|s| s.total_entries).unwrap_or(0),
        outcome.entries.len(),
        elapsed_ms
    ));
    for (i, entry) in outcome.entries.iter().enumerate() {
        lines.push(format_entry(i, entry));
    }
    if (outcome.total as usize) > outcome.entries.len() {
        lines.push(format!(
            "还有 {} 个结果未显示；可以加 glob / path 缩小范围，或调大 limit（最大 {}）。",
            outcome.total as usize - outcome.entries.len(),
            MAX_LIMIT
        ));
    }
    lines.push("提示：只搜索文件名/路径；要按文件内容搜索请用 search_files。".to_string());
    ToolOutcome::ok(lines.join("\n")).with_display(format!(
        "命中 {} 个（显示 {}）",
        outcome.total,
        outcome.entries.len()
    ))
}

// ------------------------------------------------------------
// reindex_files
// ------------------------------------------------------------

async fn reindex_files_handler(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    let cfg = &settings.index;
    let custom = crate::arg_string_list(&args, "paths");
    let timeout_secs = crate::arg_u64(&args, "timeout_secs", 600).clamp(10, 3600);

    let (requested, scope) = if custom.iter().any(|p| !p.trim().is_empty()) {
        (
            custom
                .into_iter()
                .filter(|p| !p.trim().is_empty())
                .collect::<Vec<_>>(),
            "参数 paths",
        )
    } else {
        if cfg.roots.is_empty() {
            return ToolOutcome::err(
                "没有可索引的目录：设置页的 index.roots 是空的，请在参数 paths 里给目录，\
                 或去设置页添加索引根目录。",
            );
        }
        (cfg.roots.clone(), "设置里的 index.roots")
    };

    let roots: Vec<PathBuf> = requested
        .iter()
        .map(|raw| {
            let expanded = expand_tilde(raw.trim());
            let absolute = if expanded.is_absolute() {
                expanded
            } else {
                crate::workspace_dir(ctx).join(expanded)
            };
            normalize_root_path(neosonia_core::util::normalize_lexical(&absolute))
        })
        .collect();
    let existing: Vec<&PathBuf> = roots.iter().filter(|p| p.is_dir()).collect();
    if existing.is_empty() {
        let shown: Vec<String> = roots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        return ToolOutcome::err(format!(
            "没有可索引的目录（{scope}）：{} 都不存在或不是目录。\n\
             请检查路径拼写；支持 ~ 展开，相对路径相对工作目录。",
            truncate_chars(&shown.join("、"), 500)
        ));
    }

    let index = FileIndex::global(&ctx.paths());
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = cancel.clone();
    let exclude = cfg.exclude.clone();
    let max_entries = cfg.max_entries.max(1);
    let task_roots = existing.iter().map(|p| (*p).clone()).collect::<Vec<_>>();
    let requested_count = roots.len();
    ctx.phase("正在建立本地文件索引...");
    let started = Instant::now();
    let mut handle = tokio::task::spawn_blocking(move || {
        index.scan_with_cancel(&task_roots, &exclude, max_entries, move || {
            task_cancel.load(Ordering::SeqCst)
        })
    });

    let mut timed_out = false;
    let mut user_cancelled = false;
    let joined = loop {
        tokio::select! {
            joined = &mut handle => break Some(joined),
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                if ctx.is_cancelled() && !user_cancelled {
                    user_cancelled = true;
                    cancel.store(true, Ordering::SeqCst);
                }
                if started.elapsed() > Duration::from_secs(timeout_secs) && !timed_out {
                    timed_out = true;
                    cancel.store(true, Ordering::SeqCst);
                }
                if timed_out {
                    break None;
                }
                let (entries, _busy, elapsed_ms) = index.progress();
                ctx.phase(format!(
                    "正在扫描本地文件：已处理 {entries} 条，用时 {} 秒 / 最多 {timeout_secs} 秒",
                    elapsed_ms / 1000
                ));
            }
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if timed_out {
        return ToolOutcome::err(format!(
            "索引扫描超时（{timeout_secs} 秒）：已扫描的部分条目已经写入索引，find_files 仍可用；\
             可以调大 timeout_secs（最大 3600）只扫描某个子目录（paths），或减少索引根目录。"
        ))
        .with_display("索引扫描超时");
    }
    let stats = match joined {
        Some(Ok(Ok(stats))) => stats,
        Some(Ok(Err(e))) => return ToolOutcome::err(format!("建立索引失败：{e}")),
        Some(Err(join_err)) => {
            return ToolOutcome::err(format!("建立索引的后台线程异常结束：{join_err}"))
        }
        None => return ToolOutcome::err("建立索引被中断，请重试。"),
    };

    if user_cancelled || stats.cancelled {
        return ToolOutcome::err(format!(
            "已取消索引扫描（已处理 {} 条，用时 {} 毫秒；已扫描部分已写入索引）。",
            stats.entries, elapsed_ms
        ))
        .with_display("索引扫描已取消");
    }

    let mut lines = Vec::new();
    lines.push("本地文件索引已更新：".to_string());
    lines.push(format!("- 根目录：{} 个（{}）", stats.roots.len(), scope));
    for root in &stats.roots {
        lines.push(format!("  - {root}"));
    }
    if stats.roots.len() < requested_count {
        lines.push(format!(
            "- 注意：请求的 {} 个路径里有 {} 个不存在/不是目录，或已被外层根目录覆盖，已跳过。",
            requested_count,
            requested_count - stats.roots.len()
        ));
    }
    lines.push(format!(
        "- 扫描条目：{}（文件 {}，目录 {}）",
        stats.entries, stats.files, stats.dirs
    ));
    lines.push(format!(
        "- 新增/更新：{} 行；清理过期：{} 行",
        stats.upserted, stats.deleted
    ));
    if stats.skipped > 0 || stats.errors > 0 {
        lines.push(format!(
            "- 跳过 {} 条，读取失败 {} 条{}",
            stats.skipped,
            stats.errors,
            if stats.errors > 0 { "（有读取错误，本次未清理过期条目）" } else { "" }
        ));
    }
    if stats.truncated {
        lines.push(format!(
            "- 已达到条目上限 max_entries={}，剩余目录未索引；可在设置页调大上限后重新扫描。",
            cfg.max_entries
        ));
    }
    lines.push(format!("- 用时：{} 毫秒", elapsed_ms));
    lines.push(String::new());
    lines.push(
        "现在可以用 find_files 按文件名搜索了。若想让索引在后台自动维护，\
         在设置页开启「本地索引」的自动扫描。"
            .to_string(),
    );
    ToolOutcome::ok(lines.join("\n"))
        .with_display(format!("已索引 {} 条（{} 毫秒）", stats.entries, elapsed_ms))
}

// ------------------------------------------------------------
// index_stats
// ------------------------------------------------------------

async fn index_stats_handler(ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    let cfg = &settings.index;
    let index = FileIndex::global(&ctx.paths());
    let task = {
        tokio::task::spawn_blocking(move || index.stats())
    };
    let stats = match tokio::time::timeout(Duration::from_secs(15), task).await {
        Ok(Ok(Ok(stats))) => stats,
        Ok(Ok(Err(e))) => return ToolOutcome::err(format!("读取索引状态失败：{e}")),
        Ok(Err(join_err)) => {
            return ToolOutcome::err(format!("读取索引状态的后台线程异常结束：{join_err}"))
        }
        Err(_) => return ToolOutcome::err("读取索引状态超时（15 秒），请稍后重试。"),
    };

    let mut lines = Vec::new();
    lines.push("本地文件索引状态：".to_string());
    lines.push(format!(
        "- 索引条目：{}（文件 {}，目录 {}）",
        stats.total_entries, stats.files, stats.dirs
    ));
    lines.push(format!("- 数据库大小：{}", human_bytes(stats.db_bytes)));
    if stats.scanning {
        lines.push(format!(
            "- 状态：正在扫描（已处理 {} 条，已用 {}.{:01} 秒）",
            stats.scanning_entries,
            stats.scanning_elapsed_ms / 1000,
            (stats.scanning_elapsed_ms % 1000) / 100
        ));
    } else {
        lines.push("- 状态：空闲，可以搜索".to_string());
    }
    if stats.degraded {
        lines.push(
            "- 注意：索引库打开失败，当前使用内存索引（重启后丢失）。请检查 data 目录权限。"
                .to_string(),
        );
    }
    match &stats.last_scan {
        Some(last) => lines.push(format!(
            "- 上次扫描：{} 条，写入 {} 行，清理 {} 行，用时 {} 毫秒{}",
            last.entries,
            last.upserted,
            last.deleted,
            last.elapsed_ms,
            if last.truncated { "（达到 max_entries 上限）" } else { "" }
        )),
        None => lines.push("- 上次扫描：还没有扫描过".to_string()),
    }
    if !stats.last_scan.as_ref().map(|s| s.roots.is_empty()).unwrap_or(true) {
        if let Some(last) = &stats.last_scan {
            lines.push("- 已索引根目录：".to_string());
            for root in &last.roots {
                lines.push(format!("  - {root}"));
            }
        }
    } else {
        lines.push("- 已索引根目录：（无）".to_string());
    }
    lines.push(format!(
        "- 设置：enabled={}，autoscan={}，max_entries={}，配置的根目录 {} 个",
        cfg.enabled,
        cfg.autoscan,
        cfg.max_entries,
        cfg.roots.len()
    ));
    if stats.total_entries == 0 {
        lines.push(
            "索引为空：请调用 reindex_files（可用 paths 指定目录）建立索引；\
             或在设置页开启本地索引（enabled + autoscan）等待后台扫描。"
                .to_string(),
        );
    } else if !stats.scanning {
        lines.push("提示：需要按文件名找东西时用 find_files；按内容找用 search_files。".to_string());
    }

    ToolOutcome::ok(lines.join("\n")).with_display(format!(
        "索引 {} 条（{}）",
        stats.total_entries,
        if stats.scanning { "扫描中" } else { "空闲" }
    ))
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::registry::Tool;
    use serde_json::json;

    // ---------- 临时目录 / 夹具 ----------

    fn temp_base(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "neosonia-index-{tag}-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        ));
        let _ = std::fs::create_dir_all(&path);
        path
    }

    fn new_index(tag: &str) -> (PathBuf, FileIndex) {
        let base = temp_base(tag);
        // 扫描根目录和索引库分开放，避免把索引库自己扫进去干扰断言。
        let root = base.join("root");
        let _ = std::fs::create_dir_all(&root);
        let index = FileIndex::open_at(base.join("data").join("file_index.sqlite3"));
        (root, index)
    }

    fn write_file(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, content);
        path
    }

    /// 标准夹具：
    /// root/
    ///   alpha.txt                (完全匹配 alpha)
    ///   Beta.MD
    ///   alpha_note.txt
    ///   sub/alpha_child.txt
    ///   sub/deep/gamma.rs
    ///   node_modules/alpha_ignored.js
    ///   .git/alpha_git.txt
    ///   target/alpha_target.txt
    ///   skipme/alpha_skipme.txt
    fn fixture(tag: &str) -> (PathBuf, FileIndex) {
        let (root, index) = new_index(tag);
        write_file(&root, "alpha.txt", "hello alpha");
        write_file(&root, "Beta.MD", "beta");
        write_file(&root, "alpha_note.txt", "note");
        write_file(&root, "sub/alpha_child.txt", "child");
        write_file(&root, "sub/deep/gamma.rs", "fn main() {}");
        write_file(&root, "node_modules/alpha_ignored.js", "ignored");
        write_file(&root, ".git/alpha_git.txt", "ignored");
        write_file(&root, "target/alpha_target.txt", "ignored");
        write_file(&root, "skipme/alpha_skipme.txt", "custom exclude");
        (root, index)
    }

    fn names(entries: &[IndexEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    // ---------- 纯函数 ----------

    #[test]
    fn like_escape_escapes_wildcards() {
        assert_eq!(like_escape("a%b_c\\d"), "a\\%b\\_c\\\\d");
        assert_eq!(like_escape("普通中文"), "普通中文");
    }

    #[test]
    fn rank_prefers_exact_then_prefix_then_contains_then_path() {
        let exact = IndexEntry { path: "C:\\x\\alpha".into(), name: "alpha".into(), ..Default::default() };
        let prefix = IndexEntry { path: "C:\\x\\alpha2".into(), name: "alpha2".into(), ..Default::default() };
        let contains = IndexEntry { path: "C:\\x\\xalpha".into(), name: "xalpha".into(), ..Default::default() };
        let path_only = IndexEntry { path: "C:\\x\\alpha\\file.bin".into(), name: "file.bin".into(), ..Default::default() };
        assert_eq!(rank_of(&exact, "alpha"), 0);
        assert_eq!(rank_of(&prefix, "alpha"), 1);
        assert_eq!(rank_of(&contains, "alpha"), 2);
        assert_eq!(rank_of(&path_only, "alpha"), 3);
        assert_eq!(rank_of(&IndexEntry { name: "zzz".into(), path: "C:\\zzz\\q".into(), ..Default::default() }, "alpha"), 4);
    }

    #[test]
    fn format_timestamp_handles_unknown() {
        assert_eq!(format_timestamp(0), "未知");
        assert_ne!(format_timestamp(1_700_000_000_000), "未知");
    }

    // ---------- 扫描与搜索 ----------

    #[test]
    fn scan_indexes_fixture_and_ranks_results() {
        let (root, index) = fixture("basic");
        let stats = index
            .scan_now(&[root.clone()], &["skipme".to_string()], 10_000)
            .expect("scan");
        assert!(!stats.truncated);
        assert!(!stats.cancelled);
        assert_eq!(stats.roots.len(), 1);
        assert!(stats.files >= 5, "files={}", stats.files);
        assert!(stats.dirs >= 2, "dirs={}", stats.dirs);
        assert!(stats.upserted > 0);

        let exact = index
            .search_with_total("alpha.txt", None, None, 50, false)
            .expect("search exact");
        assert_eq!(
            names(&exact.entries).first().map(String::as_str),
            Some("alpha.txt"),
            "完全匹配应排第一: {:?}",
            names(&exact.entries)
        );

        let outcome = index
            .search_with_total("alpha", None, None, 50, false)
            .expect("search");
        let got = names(&outcome.entries);
        assert!(got.contains(&"alpha_note.txt".to_string()));
        assert!(got.contains(&"alpha_child.txt".to_string()));
        // 被排除的目录（.git/target/node_modules/自定义 skipme）不应出现
        assert!(!got.iter().any(|n| n.contains("ignored")), "got={got:?}");
        assert!(!got.iter().any(|n| n.contains("git") || n.contains("target") || n.contains("skipme")));

        // 大小/修改时间有值
        let first = outcome
            .entries
            .iter()
            .find(|e| e.name == "alpha.txt")
            .expect("alpha.txt 应在结果里");
        assert_eq!(first.size, "hello alpha".len() as u64);
        assert!(first.modified_ms > 0);
        assert!(!first.is_dir);
    }

    #[test]
    fn search_is_case_insensitive_and_supports_dirs_only() {
        let (root, index) = fixture("case");
        index.scan_now(&[root], &[], 10_000).unwrap();
        let outcome = index.search_with_total("beta", None, None, 10, false).unwrap();
        assert_eq!(names(&outcome.entries), vec!["Beta.MD".to_string()]);

        let dirs = index
            .search_with_total("sub", None, None, 10, true)
            .unwrap();
        assert!(dirs.entries.iter().all(|e| e.is_dir));
        assert!(names(&dirs.entries).contains(&"sub".to_string()));
    }

    #[test]
    fn glob_filters_and_path_filter_narrow_results() {
        let (root, index) = fixture("glob");
        index.scan_now(&[root.clone()], &[], 10_000).unwrap();

        let only_txt = index
            .search_with_total("alpha", None, Some("*.txt"), 50, false)
            .unwrap();
        assert!(only_txt.entries.iter().all(|e| e.name.ends_with(".txt")));
        assert!(only_txt.entries.iter().any(|e| e.name == "alpha.txt"));

        let only_rs = index
            .search_with_total("alpha", None, Some("*.rs"), 50, false)
            .unwrap();
        assert!(only_rs.entries.is_empty(), "{:?}", names(&only_rs.entries));

        let sub_dir = root.join("sub");
        let scoped = index
            .search_with_total("alpha", Some(&sub_dir), None, 50, false)
            .unwrap();
        assert!(!scoped.entries.is_empty());
        assert!(
            scoped.entries.iter().all(|e| e.path.starts_with(sub_dir.to_string_lossy().as_ref())),
            "{:?}",
            names(&scoped.entries)
        );
    }

    #[test]
    fn search_empty_query_is_an_error_not_panic() {
        let (_root, index) = new_index("empty-query");
        assert!(index.search("", None, None, 10).is_err());
        assert!(index.search("   ", None, None, 10).is_err());
    }

    #[test]
    fn max_entries_truncates_scan() {
        let (root, index) = fixture("truncate");
        let stats = index.scan_now(&[root], &[], 2).unwrap();
        assert!(stats.truncated);
        assert_eq!(stats.entries, 2);
        assert!(stats.upserted <= 2);
    }

    #[test]
    fn second_scan_removes_stale_entries() {
        let (root, index) = fixture("stale");
        index.scan_now(&[root.clone()], &[], 10_000).unwrap();
        let stale = root.join("alpha_note.txt");
        assert!(index
            .search_with_total("alpha_note", None, None, 10, false)
            .unwrap()
            .entries
            .len()
            >= 1);
        std::fs::remove_file(&stale).unwrap();
        let stats = index.scan_now(&[root.clone()], &[], 10_000).unwrap();
        assert!(stats.deleted >= 1, "deleted={}", stats.deleted);
        let after = index
            .search_with_total("alpha_note", None, None, 10, false)
            .unwrap();
        assert!(after.entries.is_empty(), "{:?}", names(&after.entries));
    }

    #[test]
    fn stats_reports_counts_and_last_scan() {
        let (root, index) = fixture("stats");
        let scan = index.scan_now(&[root], &[], 10_000).unwrap();
        let stats = index.stats().unwrap();
        assert_eq!(stats.total_entries, stats.files + stats.dirs);
        assert_eq!(stats.total_entries, scan.entries);
        assert!(stats.db_bytes > 0);
        assert!(!stats.scanning);
        assert!(stats.last_scan.is_some());
        assert!(!stats.degraded);

        index.clear().unwrap();
        let cleared = index.stats().unwrap();
        assert_eq!(cleared.total_entries, 0);
        assert!(cleared.last_scan.is_none());
    }

    #[test]
    fn degraded_index_falls_back_to_memory() {
        let base = temp_base("degraded");
        let not_a_db = base.join("not-a-database");
        std::fs::write(&not_a_db, "这不是 sqlite 文件，用来触发降级").unwrap();
        let index = FileIndex::open_at(not_a_db);
        assert!(index.is_degraded(), "打开非数据库文件应降级");

        let root = base.join("files");
        write_file(&root, "降级测试.txt", "x");
        let stats = index.scan_now(&[root], &[], 100).unwrap();
        assert!(stats.entries >= 1);
        let found = index.search("降级测试", None, None, 10).unwrap();
        assert_eq!(found.len(), 1);
        let stats = index.stats().unwrap();
        assert!(stats.total_entries >= 1);
    }

    #[test]
    fn scan_with_cancel_stops_quickly() {
        let (root, index) = fixture("cancel");
        let stats = index
            .scan_with_cancel(&[root], &[], 100_000, || true)
            .unwrap();
        assert!(stats.cancelled);
    }

    #[test]
    fn scan_missing_root_is_error() {
        let (_base, index) = new_index("missing-root");
        let missing = std::env::temp_dir().join("neosonia-index-definitely-missing-dir-xyz");
        let err = index.scan_now(&[missing], &[], 100).unwrap_err().to_string();
        assert!(err.contains("没有可索引"), "got: {err}");
    }

    #[test]
    fn tools_are_registered_with_expected_specs() {
        let reg = ToolRegistry::new();
        register(&reg);
        let names = reg.names();
        for expected in ["find_files", "reindex_files", "index_stats"] {
            assert!(names.iter().any(|n| n == expected), "缺少工具 {expected}");
        }
        let specs = tool_specs();
        assert_eq!(specs.len(), 3);
        let find = specs.iter().find(|s| s.name == "find_files").unwrap();
        assert_eq!(find.parameters["type"], "object");
        assert!(find.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "query"));
        assert!(!find.description.is_empty());
    }

    // ---------- 工具 handler 级测试（用临时 FakeHost） ----------

    #[tokio::test]
    async fn find_files_handler_rejects_empty_query() {
        let (ctx, _host) = fake_ctx("handler-empty");
        let out = FindFilesTool
            .call(&ctx, json!({"query": "   "}))
            .await;
        assert!(!out.ok);
        assert!(out.content.contains("query"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn reindex_handler_reports_missing_paths() {
        let (ctx, _host) = fake_ctx("handler-reindex");
        let missing = ctx
            .paths()
            .root
            .join("definitely-missing-dir")
            .to_string_lossy()
            .to_string();
        let out = ReindexFilesTool
            .call(&ctx, json!({"paths": [missing]}))
            .await;
        assert!(!out.ok);
        assert!(out.content.contains("没有可索引"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn index_stats_handler_returns_text() {
        let (ctx, _host) = fake_ctx("handler-stats");
        let out = IndexStatsTool.call(&ctx, json!({})).await;
        assert!(out.ok, "got: {}", out.content);
        assert!(out.content.contains("索引"), "got: {}", out.content);
    }

    // ---------- 假 Host（handler 测试用） ----------

    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;

    use neosonia_core::model::{
        AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest,
    };
    use parking_lot::RwLock;

    struct NullMemory;

    impl MemoryApi for NullMemory {
        fn remember(&self, _: &str, _: &[String], _: f64, _: &str) -> Result<i64> {
            Ok(0)
        }
        fn recall(&self, _: &str, _: usize) -> Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn recent(&self, _: usize) -> Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn all(&self, _: Option<&str>, _: usize) -> Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn forget(&self, _: i64) -> Result<bool> {
            Ok(false)
        }
        fn update_importance(&self, _: i64, _: f64) -> Result<()> {
            Ok(())
        }
        fn get_block(&self, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn set_block(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn blocks(&self) -> Result<BTreeMap<String, String>> {
            Ok(BTreeMap::new())
        }
        fn log_conversation(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn stats(&self) -> Result<MemoryStats> {
            Ok(MemoryStats::default())
        }
    }

    struct FakeHost {
        settings: Arc<RwLock<Settings>>,
        paths: Paths,
        cancelled: AtomicBool,
    }

    impl AgentHost for FakeHost {
        fn settings(&self) -> Arc<RwLock<Settings>> {
            self.settings.clone()
        }
        fn paths(&self) -> Paths {
            self.paths.clone()
        }
        fn memory(&self) -> Arc<dyn MemoryApi> {
            Arc::new(NullMemory)
        }
        fn emit(&self, _event: AgentEvent) {}
        fn spawn_subagent(&self, _req: SubagentRequest) -> Result<String> {
            Ok("test".to_string())
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    fn fake_ctx(tag: &str) -> (ToolCtx, Arc<FakeHost>) {
        let root = temp_base(tag);
        let paths = Paths::from_root(root);
        let _ = paths.ensure();
        let host = Arc::new(FakeHost {
            settings: Arc::new(RwLock::new(Settings::default())),
            paths,
            cancelled: AtomicBool::new(false),
        });
        (ToolCtx::new(host.clone(), "turn", "call"), host)
    }

    // ---------- 真机验证（默认 ignore，NEOSONIA_VERIFY=1 才执行） ----------
    //
    // 运行（正式 crate）：
    //   $env:NEOSONIA_VERIFY="1"
    //   cargo test --offline -p neosonia-tools -- --ignored --nocapture verify_real_machine_index
    //
    // 走 ToolRegistry::call 真调（不是直接调 FileIndex API），只索引 workspace/index-demo。

    #[tokio::test]
    #[ignore = "真机验证：workspace/index-demo -> reindex_files -> find_files，需 NEOSONIA_VERIFY=1"]
    async fn verify_real_machine_index() {
        if !std::env::var("NEOSONIA_VERIFY").map(|v| v.trim() == "1").unwrap_or(false) {
            eprintln!("[skip] NEOSONIA_VERIFY != 1");
            return;
        }
        // cargo test 的 cwd 是包目录（crates/neosonia-tools），用 Paths::resolve()
        // 从测试可执行文件往上找真正的项目根（带 Cargo.toml + crates/ 的目录）。
        let project_root = Paths::resolve().root;
        let demo_dir = project_root.join("workspace").join("index-demo");
        let _ = std::fs::create_dir_all(demo_dir.join("子目录"));
        for (rel, content) in [
            ("NeoSonia_索引演示_alpha.txt", "alpha demo content"),
            ("读我_中文文件名.md", "# 中文文件名演示\nNeoSonia index demo"),
            ("子目录/index-demo-note.txt", "deep note"),
        ] {
            let path = demo_dir.join(rel);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, content).expect("write demo fixture");
        }

        // 真机：Paths 用项目根，索引库就是 app 自己的 data/file_index.sqlite3。
        let paths = Paths::from_root(project_root.clone());
        let host = Arc::new(FakeHost {
            settings: Arc::new(RwLock::new(Settings::default())),
            paths,
            cancelled: AtomicBool::new(false),
        });
        let ctx = ToolCtx::new(host, "verify-turn", "verify-call");

        // 走 ToolRegistry，和模型/agent 的真实调用路径一致。
        let registry = ToolRegistry::new();
        register(&registry);
        println!("=== index::register 后 registry 工具: {:?} ===", registry.names());
        assert_eq!(registry.len(), 3);

        let demo_arg = demo_dir.to_string_lossy().to_string();
        let started = Instant::now();
        let reindex = registry
            .call(
                &ctx,
                "reindex_files",
                &json!({"paths": [demo_arg], "timeout_secs": 300}),
            )
            .await;
        let reindex_ms = started.elapsed().as_millis();
        println!("=== reindex_files（ToolRegistry::call，{reindex_ms} 毫秒）===\n{}", reindex.content);
        assert!(reindex.ok, "reindex 失败: {}", reindex.content);

        let started = Instant::now();
        let find = registry
            .call(&ctx, "find_files", &json!({"query": "index-demo", "limit": 20}))
            .await;
        let find_ms = started.elapsed().as_millis();
        println!("=== find_files(query=\"index-demo\"，{find_ms} 毫秒）===\n{}", find.content);
        assert!(find.ok, "find 失败: {}", find.content);
        for expected in [
            "NeoSonia_索引演示_alpha.txt",
            "读我_中文文件名.md",
            "index-demo-note.txt",
        ] {
            assert!(
                find.content.contains(expected),
                "find_files 没有命中 {expected}: {}",
                find.content
            );
        }

        let started = Instant::now();
        let find_name = registry
            .call(
                &ctx,
                "find_files",
                &json!({"query": "NeoSonia_索引演示", "limit": 10}),
            )
            .await;
        println!(
            "=== find_files(query=\"NeoSonia_索引演示\"，{} 毫秒）===\n{}",
            started.elapsed().as_millis(),
            find_name.content
        );
        assert!(find_name.ok);
        assert!(find_name.content.contains("NeoSonia_索引演示_alpha.txt"));

        let stats = registry.call(&ctx, "index_stats", &json!({})).await;
        println!("=== index_stats（ToolRegistry::call）===\n{}", stats.content);
        assert!(stats.ok);
        assert!(stats.content.contains("索引"));
    }
}
