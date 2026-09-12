//! SQLite 持久化层。
//!
//! # 并发策略（整个进程一条连接 + `Mutex`）
//!
//! 本进程有两个线程会碰数据库：UI 线程（搜索时每次按键查一次、删除/清空）
//! 和后台轮询线程（每 500ms 可能写一行）。这里**只有一条 `Connection`**，
//! 由 `Arc<Mutex<Db>>` 串行化，而不是给每个线程各开一条连接：
//!
//! - SQLite 本身是单写者模型。多连接并发写只会换来 `SQLITE_BUSY` 重试，
//!   并不会真的并行；串行化反而是最省事、最可预测的做法。
//! - 每次操作都只是「一行文本的读写」，持锁时间是微秒级，UI 线程被阻塞的
//!   感知不到。
//! - 连接池/专用 DB 线程在这个量级下是过度设计（YAGNI）。
//!
//! 即便如此仍然设了 WAL + `busy_timeout`：WAL 让读不被写阻塞，
//! `busy_timeout` 是万一将来真的引入第二条连接时的保险丝。
//!
//! **不要在持锁期间调用 cursive 的方法**：那会形成「UI 线程持 DB 锁 →
//! 触发 cursive 回调 → 回调里又要 DB 锁」的自锁路径。所有调用点都先把查询
//! 结果读出来、释放锁，再去动 UI（见 `main.rs` 里的 `refresh_list`）。

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

/// 历史上限，超出后裁掉最旧的。换成 SQLite 之后仍保留上限，
/// 否则数据库会无限增长。
pub const CAP: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub content: String,
    /// Unix 毫秒时间戳。只是排序用，不直接展示。
    pub created_at: i64,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建数据目录 {} 失败: {e}", parent.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
        Self::init(conn)
    }

    /// 内存库，供单测使用（不碰磁盘、互不干扰）。
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        // pragma_update 内部走 execute_batch，因此 "journal_mode" 这种会返回
        // 一行的 pragma 也不会报 ExecuteReturnedResults。
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("开启 WAL 失败: {e}"))?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| format!("设置 busy_timeout 失败: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clipboard_history (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 content    TEXT    NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_history_created_at
                 ON clipboard_history (created_at DESC);",
        )
        .map_err(|e| format!("初始化表结构失败: {e}"))?;

        Ok(Self { conn })
    }

    /// 插入新内容；若内容已存在则**只更新时间戳**，把它提到最前（MRU）。
    ///
    /// 这里刻意**不用 `INSERT OR REPLACE`**：SQLite 的 REPLACE 语义是
    /// 「先 DELETE 旧行、再 INSERT 新行」，`id` 会变、`rowid` 会变、
    /// 外键会断。`ON CONFLICT ... DO UPDATE` 才是真正的 upsert，id 保持稳定。
    pub fn upsert(&self, content: &str) -> Result<(), String> {
        let timestamp = self.next_timestamp()?;
        self.conn
            .execute(
                "INSERT INTO clipboard_history (content, created_at) VALUES (?1, ?2)
                 ON CONFLICT(content) DO UPDATE SET created_at = excluded.created_at",
                params![content, timestamp],
            )
            .map_err(|e| format!("写入历史失败: {e}"))?;
        Ok(())
    }

    /// 单调递增的毫秒时间戳。
    ///
    /// 直接取 `now_ms` 会在「同一毫秒内连续插入多条」时产生相同的时间戳，
    /// MRU 顺序就不再确定（测试也会变得不稳定）。这里保证新值一定大于
    /// 表中现有的最大值。分两步而不是写成 `INSERT ... SELECT MAX(...)`
    /// 是因为 SQLite 的 upsert 配 `INSERT ... SELECT` 需要额外的 WHERE
    /// 才能消解解析歧义，拆开更直白。
    fn next_timestamp(&self) -> Result<i64, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let max: Option<i64> = self
            .conn
            .query_row("SELECT MAX(created_at) FROM clipboard_history", [], |row| {
                row.get(0)
            })
            .map_err(|e| format!("读取最大时间戳失败: {e}"))?;
        Ok(match max {
            Some(max) => now.max(max + 1),
            None => now,
        })
    }

    /// 按 MRU 倒序取记录；`keyword` 非空时做 `LIKE %keyword%` 模糊匹配。
    pub fn list(&self, keyword: Option<&str>, limit: usize) -> Result<Vec<Entry>, String> {
        let keyword = keyword.map(str::trim).filter(|k| !k.is_empty());

        let (sql, binding) = match keyword {
            Some(keyword) => (
                "SELECT id, content, created_at FROM clipboard_history
                 WHERE content LIKE ?1 ESCAPE '\\'
                 ORDER BY created_at DESC LIMIT ?2",
                Some(format!("%{}%", escape_like(keyword))),
            ),
            None => (
                "SELECT id, content, created_at FROM clipboard_history
                 ORDER BY created_at DESC LIMIT ?1",
                None,
            ),
        };

        let mut statement = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("准备查询失败: {e}"))?;

        let limit = limit as i64;
        let rows = match binding {
            Some(pattern) => statement
                .query_map(params![pattern, limit], row_to_entry)
                .map_err(|e| format!("执行搜索失败: {e}"))?,
            None => statement
                .query_map(params![limit], row_to_entry)
                .map_err(|e| format!("执行查询失败: {e}"))?,
        };

        // 显式收集第 0 行之外的错误，避免 unwrap 把「查询出错」伪装成「没数据」
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row.map_err(|e| format!("读取查询结果失败: {e}"))?);
        }
        Ok(entries)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM clipboard_history WHERE id = ?1", params![id])
            .map_err(|e| format!("删除失败: {e}"))?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM clipboard_history", [])
            .map_err(|e| format!("清空失败: {e}"))?;
        Ok(())
    }

    /// 只保留最新的 [`CAP`] 条，返回被裁掉的行数。
    pub fn prune(&self) -> Result<usize, String> {
        self.conn
            .execute(
                "DELETE FROM clipboard_history WHERE id NOT IN (
                     SELECT id FROM clipboard_history ORDER BY created_at DESC LIMIT ?1)",
                params![CAP as i64],
            )
            .map_err(|e| format!("裁剪历史失败: {e}"))
    }

    pub fn len(&self) -> Result<usize, String> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM clipboard_history", [], |row| {
                row.get(0)
            })
            .map_err(|e| format!("统计条数失败: {e}"))?;
        Ok(count as usize)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        content: row.get(1)?,
        created_at: row.get(2)?,
    })
}

/// 转义 `LIKE` 的通配符。
///
/// 不转义的话，用户在搜索框里打一个 `%` 就会匹配到全部记录，
/// 打 `_` 会匹配任意单字符——看起来像「搜索失灵」。
fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(db: &Db, keyword: Option<&str>) -> Vec<String> {
        db.list(keyword, 1000)
            .unwrap()
            .into_iter()
            .map(|e| e.content)
            .collect()
    }

    #[test]
    fn new_entry_goes_to_the_top() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("first").unwrap();
        db.upsert("second").unwrap();
        assert_eq!(contents(&db, None), vec!["second", "first"]);
    }

    #[test]
    fn duplicate_is_promoted_without_new_row() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("a").unwrap();
        db.upsert("b").unwrap();
        db.upsert("c").unwrap();
        assert_eq!(contents(&db, None), vec!["c", "b", "a"]);

        db.upsert("a").unwrap();
        assert_eq!(contents(&db, None), vec!["a", "c", "b"]);
        assert_eq!(db.len().unwrap(), 3, "提升不应产生新行");
    }

    /// 这是不用 `INSERT OR REPLACE` 的直接理由：REPLACE 会换掉 id。
    #[test]
    fn upsert_keeps_the_row_id_stable() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("keep-my-id").unwrap();
        let first = db.list(None, 10).unwrap()[0].id;

        db.upsert("something-else").unwrap();
        db.upsert("keep-my-id").unwrap();
        let again = db.list(None, 10).unwrap()[0].id;

        assert_eq!(again, first, "重复内容必须复用同一行，而不是删掉重建");
    }

    #[test]
    fn ordering_is_strict_even_in_the_same_millisecond() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..50 {
            db.upsert(&format!("item-{i}")).unwrap();
        }
        let expected: Vec<String> = (0..50).rev().map(|i| format!("item-{i}")).collect();
        assert_eq!(contents(&db, None), expected, "同一毫秒内插入也不能乱序");
    }

    #[test]
    fn search_is_a_substring_match() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("cargo build --release").unwrap();
        db.upsert("git push origin main").unwrap();
        db.upsert("cargo test").unwrap();

        assert_eq!(contents(&db, Some("cargo")), vec!["cargo test", "cargo build --release"]);
        assert_eq!(contents(&db, Some("push")), vec!["git push origin main"]);
        assert!(contents(&db, Some("nothing-here")).is_empty());
    }

    #[test]
    fn empty_search_returns_everything() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("a").unwrap();
        assert_eq!(contents(&db, Some("   ")), vec!["a"]);
    }

    /// 回归：搜索框里输入 `%` 不能变成「匹配全部」。
    #[test]
    fn like_wildcards_in_the_keyword_are_escaped() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("100% done").unwrap();
        db.upsert("nothing else").unwrap();

        assert_eq!(contents(&db, Some("%")), vec!["100% done"]);
        assert!(contents(&db, Some("_")).is_empty(), "下划线不应匹配任意字符");
        assert_eq!(contents(&db, Some("100% d")), vec!["100% done"]);
    }

    #[test]
    fn search_handles_multibyte_content() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("今天下午的会议纪要").unwrap();
        db.upsert("unrelated").unwrap();
        assert_eq!(contents(&db, Some("会议")), vec!["今天下午的会议纪要"]);
    }

    #[test]
    fn delete_removes_only_the_target() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("a").unwrap();
        db.upsert("b").unwrap();
        db.upsert("c").unwrap();

        let id = db.list(None, 10).unwrap()[1].id; // "b"
        db.delete(id).unwrap();
        assert_eq!(contents(&db, None), vec!["c", "a"]);
    }

    #[test]
    fn clear_empties_the_table() {
        let db = Db::open_in_memory().unwrap();
        db.upsert("a").unwrap();
        db.clear().unwrap();
        assert_eq!(db.len().unwrap(), 0);
        assert!(contents(&db, None).is_empty());
    }

    #[test]
    fn prune_keeps_only_the_newest_cap_entries() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..CAP + 25 {
            db.upsert(&format!("item-{i}")).unwrap();
        }
        assert_eq!(db.len().unwrap(), CAP + 25);

        let removed = db.prune().unwrap();
        assert_eq!(removed, 25);
        assert_eq!(db.len().unwrap(), CAP);

        let listed = contents(&db, None);
        assert_eq!(listed[0], format!("item-{}", CAP + 24), "最新的必须还在");
        assert!(
            !listed.contains(&"item-0".to_string()),
            "最旧的应被裁掉"
        );
    }

    #[test]
    fn content_is_stored_verbatim_with_newlines() {
        let db = Db::open_in_memory().unwrap();
        let text = "line1\nline2\ttabbed";
        db.upsert(text).unwrap();
        assert_eq!(db.list(None, 10).unwrap()[0].content, text);
    }

    #[test]
    fn escaping_only_touches_wildcards() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
        assert_eq!(escape_like("中文"), "中文");
    }

    #[test]
    fn opening_a_file_database_creates_directories_and_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("history.db");

        let db = Db::open(&path).unwrap();
        db.upsert("persisted").unwrap();
        assert!(path.exists(), "数据库文件应被创建");

        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn reopening_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        {
            let db = Db::open(&path).unwrap();
            db.upsert("first").unwrap();
            db.upsert("second").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(contents(&db, None), vec!["second", "first"]);
    }
}
