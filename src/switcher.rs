//! The switching model: windows grouped into apps, a selection, and the two
//! modes. Pure, unit tested; the overlay renders it and input drives it.
//!
//! Windows arrive in most-recently-used order (the focused window first).
//! Apps are the distinct app IDs in the order of their most recent window, so
//! app 0 is the app in front. Each app keeps its windows in MRU order.
//!
//! - `Apps` mode (Alt+Tab): a horizontal row of icons, one per app. Picking
//!   an app picks its most recent window.
//! - `Windows` mode (Alt+`): the selected app's windows as a vertical strip
//!   of previews.
//!
//! Like macOS, opening skips what is already in front: Alt+Tab starts on app
//! 1, Alt+` on the front app's window 1.

/// One window, as the switcher knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// The ext-foreign-toplevel identifier; what a pick returns.
    pub id: String,
    pub app_id: String,
    pub title: String,
}

/// The windows of one app ID, as indices into `Switcher::windows`, MRU first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub app_id: String,
    pub windows: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Apps,
    Windows,
}

/// What one key press (or one client invocation) does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Alt+Tab
    NextApp,
    /// Alt+Shift+Tab
    PrevApp,
    /// Alt+`
    NextWindow,
    /// Alt+Shift+`
    PrevWindow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switcher {
    windows: Vec<Window>,
    apps: Vec<App>,
    mode: Mode,
    /// Selected app, an index into `apps`.
    app: usize,
    /// Selected window of the selected app, an index into its `windows`.
    window: usize,
}

/// `i + step`, wrapping around in `0..len`. `len` must not be 0.
fn wrap(i: usize, step: isize, len: usize) -> usize {
    (i as isize + step).rem_euclid(len as isize) as usize
}

fn group(windows: &[Window]) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        match apps.iter_mut().find(|a| a.app_id == w.app_id) {
            Some(app) => app.windows.push(i),
            None => apps.push(App {
                app_id: w.app_id.clone(),
                windows: vec![i],
            }),
        }
    }
    apps
}

impl Switcher {
    /// Open on `windows` (MRU order) and apply the `first` step from the
    /// focused window. None without windows.
    pub fn open(windows: Vec<Window>, first: Step) -> Option<Self> {
        if windows.is_empty() {
            return None;
        }
        let apps = group(&windows);
        let mut s = Self {
            windows,
            apps,
            mode: Mode::Apps,
            app: 0,
            window: 0,
        };
        s.step(first);
        Some(s)
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    pub fn apps(&self) -> &[App] {
        &self.apps
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The selected app's index.
    pub fn selected_app(&self) -> usize {
        self.app
    }

    /// The selected window's index within the selected app.
    pub fn selected_window(&self) -> usize {
        self.window
    }

    /// The window a pick returns now.
    pub fn picked(&self) -> &Window {
        &self.windows[self.apps[self.app].windows[self.window]]
    }

    fn app_len(&self) -> usize {
        self.apps[self.app].windows.len()
    }

    pub fn step(&mut self, step: Step) {
        match step {
            Step::NextApp | Step::PrevApp => {
                let dir = if step == Step::NextApp { 1 } else { -1 };
                self.mode = Mode::Apps;
                self.app = wrap(self.app, dir, self.apps.len());
                self.window = 0;
            }
            Step::NextWindow | Step::PrevWindow => {
                let dir = if step == Step::NextWindow { 1 } else { -1 };
                if self.mode == Mode::Windows {
                    self.window = wrap(self.window, dir, self.app_len());
                    return;
                }
                self.mode = Mode::Windows;
                // The front app's window 0 is the focused window: skip it,
                // as opening does. Another app's most recent window is the
                // natural start.
                self.window = match (self.app, dir) {
                    (0, _) => wrap(0, dir, self.app_len()),
                    (_, 1) => 0,
                    _ => self.app_len() - 1,
                };
            }
        }
    }

    /// Select app `i` (mouse hover), back in apps mode. Out of range: no-op.
    pub fn select_app(&mut self, i: usize) {
        if i < self.apps.len() && (i != self.app || self.mode != Mode::Apps) {
            self.mode = Mode::Apps;
            self.app = i;
            self.window = 0;
        }
    }

    /// Select window `i` of the selected app (mouse hover in the strip).
    pub fn select_window(&mut self, i: usize) {
        if self.mode == Mode::Windows && i < self.app_len() {
            self.window = i;
        }
    }

    /// The window list changed while open (a window closed). Keeps the
    /// selection on the same window if it survives, else on its neighbor.
    /// False if no windows are left.
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(gone) = self.windows.iter().position(|w| w.id == id) else {
            return true;
        };
        let picked = self.apps[self.app].windows[self.window];
        let mode = self.mode;
        let (app, window) = (self.app, self.window);
        let app_id = self.apps[self.app].app_id.clone();

        self.windows.remove(gone);
        if self.windows.is_empty() {
            return false;
        }
        self.apps = group(&self.windows);
        let renumber = |i: usize| if i > gone { i - 1 } else { i };

        if picked != gone {
            // Find the same window again.
            let picked = renumber(picked);
            for (a, entry) in self.apps.iter().enumerate() {
                if let Some(w) = entry.windows.iter().position(|&i| i == picked) {
                    self.app = a;
                    self.window = w;
                    return true;
                }
            }
            unreachable!("the selected window survived");
        }
        match self.apps.iter().position(|a| a.app_id == app_id) {
            // The app lives on: stay on it, on the neighboring window.
            Some(a) => {
                self.app = a;
                self.window = window.min(self.app_len() - 1);
            }
            // Its last window closed: the app at its place, in apps mode.
            None => {
                self.mode = Mode::Apps;
                self.app = app.min(self.apps.len() - 1);
                self.window = 0;
                return true;
            }
        }
        self.mode = mode;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spec`: "app:id" per window, MRU order.
    fn windows(spec: &[&str]) -> Vec<Window> {
        spec.iter()
            .map(|s| {
                let (app, id) = s.split_once(':').unwrap();
                Window {
                    id: id.into(),
                    app_id: app.into(),
                    title: format!("{app} {id}"),
                }
            })
            .collect()
    }

    fn open(spec: &[&str], first: Step) -> Switcher {
        Switcher::open(windows(spec), first).unwrap()
    }

    const MIXED: &[&str] = &["term:1", "web:2", "term:3", "mail:4", "web:5"];

    #[test]
    fn groups_in_mru_order() {
        let s = open(MIXED, Step::NextApp);
        let apps: Vec<_> = s.apps().iter().map(|a| a.app_id.as_str()).collect();
        assert_eq!(apps, ["term", "web", "mail"]);
        assert_eq!(s.apps()[0].windows, [0, 2]);
        assert_eq!(s.apps()[1].windows, [1, 4]);
    }

    #[test]
    fn empty_is_none() {
        assert!(Switcher::open(Vec::new(), Step::NextApp).is_none());
    }

    #[test]
    fn alt_tab_opens_on_the_previous_app() {
        let s = open(MIXED, Step::NextApp);
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Apps, "2"));
        let s = open(MIXED, Step::PrevApp);
        assert_eq!(s.picked().id, "4");
    }

    #[test]
    fn alt_tab_cycles_and_wraps() {
        let mut s = open(MIXED, Step::NextApp);
        s.step(Step::NextApp);
        assert_eq!(s.picked().id, "4");
        s.step(Step::NextApp);
        assert_eq!(s.picked().id, "1");
        s.step(Step::PrevApp);
        assert_eq!(s.picked().id, "4");
    }

    #[test]
    fn single_app_alt_tab_stays() {
        let s = open(&["term:1", "term:2"], Step::NextApp);
        assert_eq!(s.picked().id, "1");
    }

    #[test]
    fn alt_grave_opens_on_the_front_apps_next_window() {
        let s = open(MIXED, Step::NextWindow);
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Windows, "3"));
        let s = open(&["term:1", "term:2", "term:3"], Step::PrevWindow);
        assert_eq!(s.picked().id, "3");
    }

    #[test]
    fn alt_grave_on_another_app_starts_at_its_most_recent() {
        let mut s = open(MIXED, Step::NextApp); // web
        s.step(Step::NextWindow);
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Windows, "2"));
        s.step(Step::NextWindow);
        assert_eq!(s.picked().id, "5");
        s.step(Step::NextWindow);
        assert_eq!(s.picked().id, "2");

        let mut s = open(MIXED, Step::NextApp);
        s.step(Step::PrevWindow);
        assert_eq!(s.picked().id, "5");
    }

    #[test]
    fn tab_collapses_the_strip_and_moves_on() {
        let mut s = open(MIXED, Step::NextWindow);
        s.step(Step::NextApp);
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Apps, "2"));
    }

    #[test]
    fn mouse_selection() {
        let mut s = open(MIXED, Step::NextApp);
        s.select_app(2);
        assert_eq!(s.picked().id, "4");
        s.select_app(9);
        assert_eq!(s.picked().id, "4");
        s.select_window(0); // apps mode: ignored
        s.select_app(0);
        s.step(Step::NextWindow);
        s.select_window(0);
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Windows, "1"));
        s.select_app(0); // back to the icons
        assert_eq!(s.mode(), Mode::Apps);
    }

    #[test]
    fn closing_another_window_keeps_the_selection() {
        let mut s = open(MIXED, Step::NextApp);
        s.step(Step::NextWindow);
        s.step(Step::NextWindow); // web:5
        assert!(s.remove("1"));
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Windows, "5"));
        assert!(s.remove("nope"));
        assert_eq!(s.picked().id, "5");
    }

    #[test]
    fn closing_the_selected_window_moves_to_a_neighbor() {
        let mut s = open(MIXED, Step::NextApp);
        s.step(Step::PrevWindow); // web:5, the last of its app
        assert!(s.remove("5"));
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Windows, "2"));
    }

    #[test]
    fn closing_an_apps_last_window_falls_back_to_apps_mode() {
        let mut s = open(MIXED, Step::PrevApp); // mail:4
        s.step(Step::NextWindow);
        assert!(s.remove("4"));
        // mail was the last app; the one now at its place is web.
        assert_eq!((s.mode(), s.picked().id.as_str()), (Mode::Apps, "2"));
    }

    #[test]
    fn closing_everything() {
        let mut s = open(&["term:1"], Step::NextApp);
        assert!(!s.remove("1"));
    }
}
