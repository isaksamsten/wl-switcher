//! Focus tracking: the order windows were last focused in, kept by the
//! daemon for as long as it runs. Pure, unit tested.
//!
//! The daemon learns about focus from `mru-event-stream`, a command it runs
//! with `sh -c` that prints the ID of each window that gains focus, one per
//! line, as it happens. The IDs are the compositor's IPC window IDs, which on
//! umbriel and niri are the ext-foreign-toplevel identifiers, so they match
//! the toplevel list as they are.
//!
//! Requested IDs are a filter. The order shown is: windows by when they were
//! last focused, then windows never seen focused (since the daemon started)
//! in the order they were requested, or announced without a request.

/// Remembered IDs at most. The stream may name windows the toplevel list
/// never announces (or announces after the focus event), so not every ID
/// gets forgotten on close; this bounds that.
const MAX_IDS: usize = 1024;

/// Ext identifiers, most recently focused first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mru {
    order: Vec<String>,
}

/// The ID in one line of the stream's output; None for blank lines.
pub fn parse_line(line: &str) -> Option<&str> {
    let id = line.trim();
    (!id.is_empty()).then_some(id)
}

impl Mru {
    /// `id` got focus.
    pub fn focus(&mut self, id: &str) {
        match self.order.iter().position(|i| i == id) {
            Some(0) => {}
            Some(i) => self.order[..=i].rotate_right(1),
            None => {
                self.order.insert(0, id.to_string());
                self.order.truncate(MAX_IDS);
            }
        }
    }

    /// `id` closed.
    pub fn forget(&mut self, id: &str) {
        self.order.retain(|i| i != id);
    }

    #[cfg(test)]
    pub fn ids(&self) -> &[String] {
        &self.order
    }

    /// Sort `items` into the order to show: focused ones by recency first,
    /// the rest keeping their current relative order.
    pub fn sort<T>(&self, items: &mut [T], id: impl Fn(&T) -> &str) {
        // Stable, so the unranked keep their order.
        items.sort_by_key(|item| {
            self.order
                .iter()
                .position(|i| i == id(item))
                .unwrap_or(usize::MAX)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(parse_line("42\n"), Some("42"));
        assert_eq!(parse_line("  abc  "), Some("abc"));
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line(" \t"), None);
    }

    #[test]
    fn focus_moves_to_front() {
        let mut m = Mru::default();
        for id in ["a", "b", "c"] {
            m.focus(id);
        }
        assert_eq!(m.ids(), ["c", "b", "a"]);
        m.focus("a");
        assert_eq!(m.ids(), ["a", "c", "b"]);
        m.focus("a");
        assert_eq!(m.ids(), ["a", "c", "b"]);
        m.forget("c");
        assert_eq!(m.ids(), ["a", "b"]);
        m.forget("nope");
        assert_eq!(m.ids(), ["a", "b"]);
    }

    #[test]
    fn bounded() {
        let mut m = Mru::default();
        for i in 0..MAX_IDS + 10 {
            m.focus(&i.to_string());
        }
        assert_eq!(m.ids().len(), MAX_IDS);
        assert_eq!(m.ids()[0], (MAX_IDS + 9).to_string());
    }

    #[test]
    fn requested_ids_are_a_filter() {
        let mut m = Mru::default();
        for id in ["1", "2", "3"] {
            m.focus(id);
        }
        // Requested 1, 9, 3, 7: 3 and 1 by recency, then the never focused
        // ones as requested.
        let mut shown = vec!["1", "9", "3", "7"];
        m.sort(&mut shown, |s| s);
        assert_eq!(shown, ["3", "1", "9", "7"]);
    }

    #[test]
    fn without_history_the_given_order_stays() {
        let mut shown = vec!["5", "2", "8"];
        Mru::default().sort(&mut shown, |s| s);
        assert_eq!(shown, ["5", "2", "8"]);
    }
}
