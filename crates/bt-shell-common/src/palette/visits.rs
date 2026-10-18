//! The order tabs were last looked at: what ⌘P lists first.
//!
//! The newest visit is first, so the tab before the current one is second and ⌘P ⏎ goes
//! back to it; a second ⌘P ⏎ comes back (⌘⇥'s rule). Generic over the key so the rule is tried
//! without a window: the shell uses the tab's identity. It lives in memory — a restart begins
//! from the strip's order — and keeps nothing for a key once told to [`Visits::forget`] it, so
//! it holds at most the tabs that exist.

/// Keys in the order they were visited, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visits<K> {
    newest_first: Vec<K>,
}

impl<K> Default for Visits<K> {
    fn default() -> Self {
        Visits {
            newest_first: Vec::new(),
        }
    }
}

impl<K: Eq + Clone> Visits<K> {
    pub fn new() -> Self {
        Visits::default()
    }

    /// `key` was looked at now: it becomes the newest.
    pub fn visit(&mut self, key: K) {
        self.newest_first.retain(|seen| *seen != key);
        self.newest_first.insert(0, key);
    }

    /// `key` is gone (its tab closed): it keeps no place.
    pub fn forget(&mut self, key: &K) {
        self.newest_first.retain(|seen| seen != key);
    }

    /// Whether `key` has been visited.
    pub fn knows(&self, key: &K) -> bool {
        self.newest_first.contains(key)
    }

    /// `listed` in visiting order: the visited ones newest first, then the never visited ones
    /// in the order `listed` gives (the strip's). A key visited but no longer listed is left out
    /// and stays remembered; a listed key appears once.
    pub fn order(&self, listed: &[K]) -> Vec<K> {
        let mut out: Vec<K> = Vec::with_capacity(listed.len());
        for seen in &self.newest_first {
            if listed.contains(seen) && !out.contains(seen) {
                out.push(seen.clone());
            }
        }
        for key in listed {
            if !out.contains(key) {
                out.push(key.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_visit_comes_first_and_a_revisit_moves_up() {
        let mut v = Visits::new();
        v.visit(1);
        v.visit(2);
        v.visit(3);
        assert_eq!(v.order(&[1, 2, 3]), vec![3, 2, 1]);
        v.visit(1);
        assert_eq!(v.order(&[1, 2, 3]), vec![1, 3, 2]);
    }

    #[test]
    fn tabs_never_visited_keep_the_strips_order_after_the_visited_ones() {
        let mut v = Visits::new();
        v.visit(30);
        v.visit(20);
        // The strip: 10, 20, 30, 40, 50; only 20 and 30 were looked at.
        assert_eq!(v.order(&[10, 20, 30, 40, 50]), vec![20, 30, 10, 40, 50]);
        // Nothing visited at all: the strip's order is the answer.
        assert_eq!(Visits::<i32>::new().order(&[3, 1, 2]), vec![3, 1, 2]);
    }

    #[test]
    fn a_forgotten_tab_keeps_no_place_and_an_unlisted_one_is_left_out() {
        let mut v = Visits::new();
        v.visit(1);
        v.visit(2);
        v.visit(3);
        v.forget(&2);
        assert!(!v.knows(&2));
        assert_eq!(v.order(&[1, 2, 3]), vec![3, 1, 2]);
        // 3 moved to another window: not listed here, still known.
        assert_eq!(v.order(&[1, 2]), vec![1, 2]);
        assert!(v.knows(&3));
    }

    #[test]
    fn a_key_listed_twice_appears_once() {
        let mut v = Visits::new();
        v.visit(1);
        assert_eq!(v.order(&[1, 1, 2]), vec![1, 2]);
    }
}
