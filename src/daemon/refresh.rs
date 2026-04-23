use std::collections::{BTreeSet, VecDeque};

#[derive(Debug, Default)]
pub struct RefreshQueue {
    pending: VecDeque<String>,
    known: BTreeSet<String>,
}

impl RefreshQueue {
    pub fn mark_dirty(&mut self, root_id: &str) {
        if self.known.insert(root_id.to_string()) {
            self.pending.push_back(root_id.to_string());
        }
    }

    pub fn pop_next(&mut self) -> Option<String> {
        let next = self.pending.pop_front()?;
        self.known.remove(&next);
        Some(next)
    }

    pub fn pop_all(&mut self) -> Vec<String> {
        let pending = self.pending.drain(..).collect::<Vec<_>>();
        self.known.clear();
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::RefreshQueue;

    #[test]
    fn coalesces_duplicate_root_refresh_requests() {
        let mut queue = RefreshQueue::default();
        queue.mark_dirty("root:alpha");
        queue.mark_dirty("root:alpha");
        queue.mark_dirty("root:beta");

        assert_eq!(queue.pop_next(), Some("root:alpha".to_string()));
        assert_eq!(queue.pop_next(), Some("root:beta".to_string()));
        assert_eq!(queue.pop_next(), None);
    }

    #[test]
    fn drains_all_pending_roots_in_one_batch() {
        let mut queue = RefreshQueue::default();
        queue.mark_dirty("root:alpha");
        queue.mark_dirty("root:beta");
        queue.mark_dirty("root:alpha");

        assert_eq!(
            queue.pop_all(),
            vec!["root:alpha".to_string(), "root:beta".to_string()]
        );
        assert_eq!(queue.pop_all(), Vec::<String>::new());
    }
}
