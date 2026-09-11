//! 历史落盘：路径解析、原子写入、0600 权限、损坏文件容错。
//!
//! 剪贴板历史里出现密码/token 的概率很高，因此文件权限收紧到 0600，并且
//! 用「写临时文件 → rename」保证不会留下半截 JSON。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 文件格式版本，用于将来演进。
const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    /// 越靠前越新（MRU 顺序）。
    items: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    /// 文件不存在：首次运行，属正常情况。
    NotFound,
    Io(String),
    Parse(String),
    /// 版本号不认识（比当前程序新），此时不猜测、不覆盖。
    Version(u32),
}

impl LoadError {
    pub fn message(&self) -> String {
        match self {
            LoadError::NotFound => "暂无历史文件，已从空历史开始".to_string(),
            LoadError::Io(e) => format!("读取历史文件失败（{e}），已从空历史开始"),
            LoadError::Parse(e) => format!("历史文件无法解析（{e}），已从空历史开始"),
            LoadError::Version(v) => format!("历史文件版本 {v} 高于当前程序，已从空历史开始"),
        }
    }
}

/// 默认存储路径：`<data_dir>/ditto/history.json`。
/// macOS 为 `~/Library/Application Support/...`，Linux 为 `~/.local/share/...`。
pub fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("ditto").join("history.json"))
}

pub fn load(path: &Path) -> Result<Vec<String>, LoadError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::NotFound),
        Err(e) => return Err(LoadError::Io(e.to_string())),
    };

    let parsed: HistoryFile =
        serde_json::from_str(&raw).map_err(|e| LoadError::Parse(e.to_string()))?;

    if parsed.version != VERSION {
        return Err(LoadError::Version(parsed.version));
    }
    Ok(parsed.items)
}

/// 原子写入：先写临时文件再 rename，避免崩溃/断电留下半截文件。
pub fn save(path: &Path, items: &[String]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("路径无父目录: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;

    let payload = HistoryFile {
        version: VERSION,
        items: items.to_vec(),
    };
    let json = serde_json::to_string_pretty(&payload).map_err(|e| format!("序列化失败: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;

    restrict_permissions(&tmp);

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换 {} 失败: {e}", path.display())
    })?;

    // rename 会保留临时文件的权限位，这里再设一次以防某些平台行为差异。
    restrict_permissions(path);
    Ok(())
}

/// 收紧到 0600（仅属主可读写）。非 Unix 平台为空实现。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<String> {
        vec!["最新".to_string(), "line1\nline2".to_string(), "旧".to_string()]
    }

    #[test]
    fn round_trips_content_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");

        save(&path, &sample()).unwrap();
        assert_eq!(load(&path).unwrap(), sample());
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("history.json");

        save(&path, &sample()).unwrap();
        assert_eq!(load(&path).unwrap(), sample());
    }

    #[test]
    fn missing_file_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert_eq!(load(&path), Err(LoadError::NotFound));
    }

    #[test]
    fn corrupt_file_reports_parse_error_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, "{ this is not json").unwrap();

        assert!(matches!(load(&path), Err(LoadError::Parse(_))));
    }

    #[test]
    fn unknown_version_is_rejected_rather_than_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, r#"{"version": 99, "items": ["x"]}"#).unwrap();

        assert_eq!(load(&path), Err(LoadError::Version(99)));
    }

    #[test]
    fn empty_history_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");

        save(&path, &[]).unwrap();
        assert_eq!(load(&path).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn save_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        save(&path, &sample()).unwrap();

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    #[test]
    fn overwriting_replaces_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");

        save(&path, &sample()).unwrap();
        save(&path, &["只有这一条".to_string()]).unwrap();
        assert_eq!(load(&path).unwrap(), vec!["只有这一条".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        save(&path, &sample()).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "剪贴板历史必须仅属主可读写，实际 {mode:o}");
    }
}
