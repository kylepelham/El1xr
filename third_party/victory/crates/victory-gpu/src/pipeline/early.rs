//! The early list in the order the title needs it: the keys of the title
//! pipelines drawn or dispatched with, in the order this run first used
//! them, then those only earlier runs used, in their order. The workers make
//! the early pipelines lowest rank first, so a boot that goes the way the
//! last one went finds what it draws first made first (with the list kept
//! as a set, the order the title created them in decided: run 19 made 353
//! ray-tracing resolve programs before the world's mesh pipelines, which
//! were still being made when the world appeared).

use std::collections::{HashMap, HashSet};

pub(crate) struct Early {
    /// Rank of each key: its line in the file read at the start, or after
    /// the last line for a key first used in this run.
    rank: HashMap<String, u32>,
    /// The file's keys, in its order.
    listed: Vec<String>,
    /// This run's keys, in the order of their first use.
    this_run: Vec<String>,
    in_run: HashSet<String>,
    /// Whether this run used a key the file does not list, or the keys in
    /// another order than the file's.
    pub(crate) changed: bool,
    max: usize,
}

impl Early {
    /// The list in `text`, one key a line, at most `max` kept.
    pub(crate) fn read(text: &str, max: usize) -> Early {
        let mut early = Early {
            rank: HashMap::new(),
            listed: Vec::new(),
            this_run: Vec::new(),
            in_run: HashSet::new(),
            changed: false,
            max,
        };
        for key in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if early.listed.len() < max && !early.rank.contains_key(key) {
                early.rank.insert(key.to_owned(), early.listed.len() as u32);
                early.listed.push(key.to_owned());
            }
        }
        early
    }

    pub(crate) fn len(&self) -> usize {
        self.rank.len()
    }

    /// The rank of `key` if this or an earlier run used it.
    pub(crate) fn rank(&self, key: &str) -> Option<u32> {
        self.rank.get(key).copied()
    }

    /// This run used `key` (once per key is enough).
    pub(crate) fn note(&mut self, key: &str) {
        if self.this_run.len() >= self.max || self.in_run.contains(key) {
            return;
        }
        if self.listed.get(self.this_run.len()).is_none_or(|k| k != key) {
            self.changed = true;
        }
        let known = self.rank.contains_key(key);
        if !known {
            if self.rank.len() >= self.max {
                return;
            }
            self.rank.insert(
                key.to_owned(),
                (self.listed.len() + self.this_run.len()) as u32,
            );
            self.changed = true;
        }
        self.in_run.insert(key.to_owned());
        self.this_run.push(key.to_owned());
    }

    /// The list to write: this run's first uses, then the rest of the file.
    pub(crate) fn lines(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.this_run
            .iter()
            .chain(&self.listed)
            .map(String::as_str)
            .filter(|k| seen.insert(*k))
            .take(self.max)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_order_is_the_rank() {
        let early = Early::read("b\na\n\nc\na\n", 8);
        assert_eq!(early.len(), 3);
        assert_eq!(early.rank("b"), Some(0));
        assert_eq!(early.rank("a"), Some(1));
        assert_eq!(early.rank("c"), Some(2));
        assert_eq!(early.rank("d"), None);
        assert!(!early.changed);
    }

    #[test]
    fn this_runs_first_uses_lead_the_next_list() {
        let mut early = Early::read("old1\nold2\nold3", 8);
        early.note("old3");
        early.note("new");
        early.note("old3");
        early.note("old1");
        assert!(early.changed);
        assert_eq!(early.rank("new"), Some(4), "after every listed key");
        assert_eq!(early.lines(), ["old3", "new", "old1", "old2"]);
    }

    #[test]
    fn a_run_that_uses_the_keys_in_the_files_order_changes_nothing_to_save() {
        let mut early = Early::read("a\nb", 8);
        early.note("a");
        early.note("b");
        assert!(!early.changed);
        let mut early = Early::read("a\nb", 8);
        early.note("b");
        assert!(early.changed, "another order is saved");
        assert_eq!(early.lines(), ["b", "a"]);
    }

    #[test]
    fn the_list_stays_under_its_cap() {
        let mut early = Early::read("a\nb\nc", 2);
        assert_eq!(early.len(), 2);
        early.note("z");
        assert_eq!(early.rank("z"), None);
        early.note("b");
        assert_eq!(early.lines(), ["b", "a"]);
    }
}
