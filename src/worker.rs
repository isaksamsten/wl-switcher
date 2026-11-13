//! A worker thread for everything slow: loading fonts, desktop entries and
//! icons, and rendering labels, title pills and letter tiles. Results come
//! back through a calloop channel, and the main thread turns them into shm
//! buffers (see cache.rs).
//!
//! The worker lives as long as the daemon, so its caches (font system,
//! desktop entries, decoded icons) stay warm. The daemon asks it for every
//! app's icon and label ahead of time, so a show has them on its first
//! frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, mpsc};

use smithay_client_toolkit::reexports::calloop::channel;

use crate::icons::{self, Icons};
use crate::pixels::Image;
use crate::text::{self, Text};

enum Job {
    /// The icon for `key`; a letter tile in `font` if there is none.
    Icon { key: icons::Key, font: String },
    /// An app's name label.
    Label(LabelKey),
    /// A pill with arbitrary text (a window title).
    Pill { tag: u64, request: PillRequest },
    /// Reload the desktop entries if they changed.
    Refresh,
    /// Forget decoded icons, e.g. because the config changed.
    ClearIcons,
}

/// What a label is rendered for.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelKey {
    pub app_id: String,
    /// Logical pixels.
    pub max_width: i32,
    pub scale: i32,
    pub style: text::Style,
}

/// What a pill is rendered for. A result is only used while it is still
/// what the overlay wants.
#[derive(Clone, Debug, PartialEq)]
pub struct PillRequest {
    pub text: String,
    /// Logical pixels.
    pub max_width: i32,
    pub scale: i32,
    pub style: text::Style,
}

pub enum Done {
    Icon {
        key: icons::Key,
        image: Arc<Image>,
    },
    Label {
        key: LabelKey,
        pill: text::Pill,
    },
    /// The desktop entries changed, so cached icons and names may be stale.
    IconsReloaded,
    Pill {
        tag: u64,
        request: PillRequest,
        pill: text::Pill,
    },
}

/// The worker, and what was requested from it and not received yet.
pub struct Assets {
    jobs: mpsc::Sender<Job>,
    icons: RefCell<Vec<icons::Key>>,
    labels: RefCell<Vec<LabelKey>>,
}

impl Assets {
    /// Start the worker thread; it loads the fonts when idle. Insert the
    /// returned channel into the event loop and pass its results to
    /// `received`.
    pub fn spawn() -> std::io::Result<(Self, channel::Channel<Done>)> {
        let (jobs, inbox) = mpsc::channel();
        let (done, results) = channel::channel();
        std::thread::Builder::new()
            .name("wl-switcher-worker".into())
            .spawn(move || run(inbox, done))?;
        let assets = Self {
            jobs,
            icons: RefCell::new(Vec::new()),
            labels: RefCell::new(Vec::new()),
        };
        Ok((assets, results))
    }

    /// Fire and forget: if the worker died, there are no icons or labels.
    fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Request the icon for `key`, unless it is on its way already. It
    /// arrives as `Done::Icon`.
    pub fn icon(&self, key: &icons::Key, font: &str) {
        let mut pending = self.icons.borrow_mut();
        if !pending.contains(key) {
            pending.push(key.clone());
            self.send(Job::Icon {
                key: key.clone(),
                font: font.to_string(),
            });
        }
    }

    /// Request the label for `key`, unless it is on its way already. It
    /// arrives as `Done::Label`.
    pub fn label(&self, key: &LabelKey) {
        let mut pending = self.labels.borrow_mut();
        if !pending.contains(key) {
            pending.push(key.clone());
            self.send(Job::Label(key.clone()));
        }
    }

    pub fn pill(&self, tag: u64, request: PillRequest) {
        self.send(Job::Pill { tag, request });
    }

    /// Check for changed desktop entries (answered with `IconsReloaded` if so).
    pub fn refresh(&self) {
        self.send(Job::Refresh);
    }

    /// Forget all decoded icons in the worker.
    pub fn clear_icons(&self) {
        self.send(Job::ClearIcons);
    }

    /// Book-keeping for a result from the worker.
    pub fn received(&self, done: &Done) {
        match done {
            Done::Icon { key, .. } => self.icons.borrow_mut().retain(|k| k != key),
            Done::Label { key, .. } => self.labels.borrow_mut().retain(|k| k != key),
            Done::IconsReloaded | Done::Pill { .. } => {}
        }
    }
}

fn run(inbox: mpsc::Receiver<Job>, done: channel::Sender<Done>) {
    // Loading the system fonts takes a while: do it when first needed, or
    // as soon as there is nothing else to do, so icons don't wait for it.
    let mut text: Option<Text> = None;
    let mut icons = Icons::new();
    // Letter tiles for apps without an icon, by icon key and font.
    let mut letters: HashMap<(icons::Key, String), Arc<Image>> = HashMap::new();
    loop {
        let job = match inbox.try_recv() {
            Ok(job) => job,
            Err(mpsc::TryRecvError::Disconnected) => return,
            Err(mpsc::TryRecvError::Empty) => {
                text.get_or_insert_with(Text::new);
                match inbox.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            }
        };
        let result = match job {
            Job::Icon { key, font } => {
                icons.load_entries();
                // Decoded (or lettered) without the shadow, which is drawn
                // on top.
                let plain = key.plain();
                let image = match icons.get(&plain) {
                    Some(image) => image,
                    None => letters
                        .entry((plain, font))
                        .or_insert_with_key(|(key, font)| {
                            let text = text.get_or_insert_with(Text::new);
                            Arc::new(text.letter(&key.app_id, key.size, font))
                        })
                        .clone(),
                };
                let image = match &key.shadow {
                    Some(s) => Arc::new(crate::shadow::icon(&image, s, key.scale)),
                    None => image,
                };
                Done::Icon { key, image }
            }
            Job::Label(key) => {
                let name = icons.name(&key.app_id);
                let pill = text.get_or_insert_with(Text::new).pill(
                    &name,
                    &key.style,
                    key.max_width,
                    key.scale,
                );
                Done::Label { key, pill }
            }
            Job::Pill { tag, request } => {
                let pill = text.get_or_insert_with(Text::new).pill(
                    &request.text,
                    &request.style,
                    request.max_width,
                    request.scale,
                );
                Done::Pill { tag, request, pill }
            }
            Job::Refresh => {
                if !icons.refresh() {
                    continue;
                }
                letters.clear();
                Done::IconsReloaded
            }
            Job::ClearIcons => {
                icons.clear();
                letters.clear();
                continue;
            }
        };
        if done.send(result).is_err() {
            return;
        }
    }
}
