//! 剪贴板历史：MRU 去重、100 条上限、删除与清空。
//!
//! 空文本过滤与敏感内容过滤**不在这里**——它们发生在监听线程（见 `clipboard.rs`），
//! 所以本模块只需要处理去重与容量，没有「拒绝」分支。

/// 历史条数上限。
pub const CAP: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry {
    /// 稳定标识：MRU 重排后仍能定位到同一条，选中项因此不依赖索引。
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 全新内容，插入到顶部。
    Inserted(u64),
    /// 已存在，提升到顶部。
    Promoted(u64),
}

#[derive(Debug, Default)]
pub struct History {
    /// 索引 0 = 最近使用。
    items: Vec<ClipEntry>,
    next_id: u64,
}

impl History {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    /// 从持久化内容恢复。文件中越靠前的越新，因此按顺序分配 id。
    /// 文件内容视为不可信：内部重复项在此处就地去掉。
    pub fn from_texts(texts: impl IntoIterator<Item = String>) -> Self {
        let mut history = Self::new();
        for text in texts {
            if text.trim().is_empty() || history.items.iter().any(|e| e.text == text) {
                continue;
            }
            let id = history.next_id;
            history.next_id += 1;
            history.items.push(ClipEntry { id, text });
            if history.items.len() >= CAP {
                break;
            }
        }
        history
    }

    pub fn items(&self) -> &[ClipEntry] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|e| e.id == id)
    }

    pub fn get(&self, id: u64) -> Option<&ClipEntry> {
        self.items.iter().find(|e| e.id == id)
    }

    /// 供持久化使用，顺序即 MRU 顺序。
    pub fn texts(&self) -> Vec<String> {
        self.items.iter().map(|e| e.text.clone()).collect()
    }

    /// 插入新内容或把已存在的内容提升到顶部。
    pub fn push_or_promote(&mut self, text: String) -> Outcome {
        if let Some(position) = self.items.iter().position(|e| e.text == text) {
            if position == 0 {
                return Outcome::Promoted(self.items[0].id);
            }
            let entry = self.items.remove(position);
            let id = entry.id;
            self.items.insert(0, entry);
            return Outcome::Promoted(id);
        }

        let id = self.next_id;
        self.next_id += 1;
        self.items.insert(0, ClipEntry { id, text });
        if self.items.len() > CAP {
            self.items.truncate(CAP);
        }
        Outcome::Inserted(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        match self.index_of(id) {
            Some(position) => {
                self.items.remove(position);
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(history: &History) -> Vec<&str> {
        history.items().iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn inserts_new_item_at_top() {
        let mut history = History::new();
        assert_eq!(history.push_or_promote("first".into()), Outcome::Inserted(1));
        assert_eq!(history.push_or_promote("second".into()), Outcome::Inserted(2));
        assert_eq!(texts(&history), vec!["second", "first"]);
    }

    #[test]
    fn duplicate_is_promoted_without_new_entry() {
        let mut history = History::new();
        history.push_or_promote("a".into());
        history.push_or_promote("b".into());
        history.push_or_promote("c".into());
        assert_eq!(texts(&history), vec!["c", "b", "a"]);

        assert_eq!(history.push_or_promote("a".into()), Outcome::Promoted(1));
        assert_eq!(texts(&history), vec!["a", "c", "b"]);
        assert_eq!(history.len(), 3, "提升不应产生新条目");
    }

    #[test]
    fn promoting_item_already_on_top_is_noop() {
        let mut history = History::new();
        history.push_or_promote("a".into());
        history.push_or_promote("b".into());
        assert_eq!(history.push_or_promote("b".into()), Outcome::Promoted(2));
        assert_eq!(texts(&history), vec!["b", "a"]);
    }

    #[test]
    fn ids_are_stable_across_reordering() {
        let mut history = History::new();
        history.push_or_promote("a".into());
        history.push_or_promote("b".into());
        history.push_or_promote("c".into());
        assert_eq!(texts(&history), vec!["c", "b", "a"]);

        let id_of_a = history.items()[2].id;
        assert_eq!(history.index_of(id_of_a), Some(2));

        // 把 a 提升到顶部后，它的 id 不变、只是索引变成 0。
        // 选中项就靠这个不变量跟着条目走，而不是靠索引加减。
        assert_eq!(
            history.push_or_promote("a".into()),
            Outcome::Promoted(id_of_a)
        );
        assert_eq!(history.index_of(id_of_a), Some(0));
        assert_eq!(history.get(id_of_a).unwrap().text, "a");
        assert_eq!(texts(&history), vec!["a", "c", "b"]);
    }

    #[test]
    fn respects_cap_and_evicts_oldest() {
        let mut history = History::new();
        for i in 0..CAP {
            history.push_or_promote(format!("item-{i}"));
        }
        assert_eq!(history.len(), CAP);

        history.push_or_promote("newest".into());
        assert_eq!(history.len(), CAP, "超出上限必须淘汰而不是无限增长");
        assert_eq!(history.items()[0].text, "newest");
        assert!(
            !history.items().iter().any(|e| e.text == "item-0"),
            "最旧的一条应被淘汰"
        );
    }

    #[test]
    fn remove_deletes_only_the_target() {
        let mut history = History::new();
        history.push_or_promote("a".into());
        history.push_or_promote("b".into());
        history.push_or_promote("c".into());
        let id = history.items()[1].id;

        assert!(history.remove(id));
        assert_eq!(texts(&history), vec!["c", "a"]);
        assert!(!history.remove(id), "重复删除应返回 false 而不是 panic");
    }

    #[test]
    fn clear_empties_history() {
        let mut history = History::new();
        history.push_or_promote("a".into());
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.index_of(1), None);
    }

    #[test]
    fn from_texts_restores_order_and_drops_duplicates() {
        let history = History::from_texts(vec![
            "newest".to_string(),
            "older".to_string(),
            "newest".to_string(),
            "   ".to_string(),
        ]);
        assert_eq!(texts(&history), vec!["newest", "older"]);
    }

    #[test]
    fn from_texts_keeps_ids_unique() {
        let history = History::from_texts(vec!["a".to_string(), "b".to_string()]);
        assert_ne!(history.items()[0].id, history.items()[1].id);
    }

    #[test]
    fn from_texts_truncates_to_cap() {
        let history = History::from_texts((0..CAP + 20).map(|i| format!("item-{i}")));
        assert_eq!(history.len(), CAP);
    }
}
