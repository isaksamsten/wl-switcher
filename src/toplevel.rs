//! Window selection: which toplevels a request shows, in what order. Pure,
//! unit tested; the Wayland side is SCTK's `ForeignToplevelList`.

use crate::mru::Mru;

/// The state of one ext foreign toplevel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toplevel {
    pub identifier: String,
    pub app_id: String,
    pub title: String,
}

/// Pick the toplevels to show, as indices into `toplevels`. With no `ids`
/// that's every toplevel in announcement order; otherwise the toplevels whose
/// identifier exactly matches one of `ids`, in the order of `ids`. Unknown IDs
/// are passed to `unknown` and skipped; duplicates are shown once. Empty
/// identifiers never match.
pub fn select(toplevels: &[Toplevel], ids: &[String], mut unknown: impl FnMut(&str)) -> Vec<usize> {
    if ids.is_empty() {
        return (0..toplevels.len()).collect();
    }
    let mut out = Vec::new();
    for id in ids {
        match toplevels
            .iter()
            .position(|t| !t.identifier.is_empty() && t.identifier == *id)
        {
            Some(i) if !out.contains(&i) => out.push(i),
            Some(_) => {}
            None => unknown(id),
        }
    }
    out
}

/// The toplevels to show, in the order to show them: `select`ed (the IDs
/// are a filter), then the ones `mru` saw focused by recency, then the rest
/// in their selected order.
pub fn shown(
    toplevels: &[Toplevel],
    ids: &[String],
    mru: &Mru,
    unknown: impl FnMut(&str),
) -> Vec<usize> {
    let mut picked: Vec<(usize, &Toplevel)> = select(toplevels, ids, unknown)
        .into_iter()
        .map(|i| (i, &toplevels[i]))
        .collect();
    mru.sort(&mut picked, |(_, t)| t.identifier.as_str());
    picked.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevels() -> Vec<Toplevel> {
        ["10", "2", "abc", "100", "x y"]
            .into_iter()
            .map(|id| Toplevel {
                identifier: id.into(),
                app_id: "app".into(),
                title: "title".into(),
            })
            .collect()
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn select_quiet(tls: &[Toplevel], wanted: &[&str]) -> Vec<usize> {
        select(tls, &ids(wanted), |_| {})
    }

    #[test]
    fn shown_in_focus_order() {
        let mut mru = Mru::default();
        mru.focus("abc");
        mru.focus("2");
        let order = |wanted: &[&str]| shown(&toplevels(), &ids(wanted), &mru, |_| {});
        assert_eq!(order(&[]), [1, 2, 0, 3, 4]);
        assert_eq!(order(&["x y", "abc", "10"]), [2, 4, 0]);
    }

    #[test]
    fn all_windows_in_announcement_order() {
        assert_eq!(select_quiet(&toplevels(), &[]), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn exact_match_in_id_order() {
        assert_eq!(select_quiet(&toplevels(), &["100", "10", "1", "x"]), [3, 0]);
    }

    #[test]
    fn duplicates_shown_once() {
        // Spaces are part of the ID.
        assert_eq!(select_quiet(&toplevels(), &["2", "x y", "2"]), [1, 4]);
    }

    #[test]
    fn unknown_ids_reported() {
        let mut unknown = Vec::new();
        let picked = select(&toplevels(), &ids(&["nope", "abc"]), |id| {
            unknown.push(id.to_string())
        });
        assert_eq!(picked, [2]);
        assert_eq!(unknown, ["nope"]);
        assert!(select_quiet(&toplevels(), &["zzz"]).is_empty());
    }

    #[test]
    fn empty_identifier_never_matches() {
        let mut tls = toplevels();
        tls[1].identifier.clear();
        assert_eq!(select_quiet(&tls, &["2", "", "abc"]), [2]);
    }
}
