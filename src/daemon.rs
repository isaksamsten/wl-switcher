//! `--daemon`: keep the Wayland connection, window list, config and
//! rendered buffers ready, and show the switcher for clients on a Unix
//! socket (see ipc.rs).
//!
//! One show at a time: a request while the switcher is open is answered
//! with an error. A client that disconnects cancels its show.

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use smithay_client_toolkit::reexports::{
    calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, generic::Generic},
    calloop_wayland_source::WaylandSource,
};
use wayland_client::{Connection, Proxy, globals::registry_queue_init};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1;

use crate::app::{App, Outcome};
use crate::config::Config;
use crate::globals::Globals;
use crate::ipc::{self, Reply, Request};
use crate::scheme::DesktopScheme;

type Error = Box<dyn std::error::Error>;

/// Requests are tiny; anything bigger is not a client.
const MAX_REQUEST: usize = 1 << 20;

/// Client events, queued by the socket sources for the daemon loop.
pub enum Incoming {
    /// A complete request; `stream` is for the replies.
    Request {
        client: u64,
        stream: UnixStream,
        req: Request,
    },
    /// The client closed its connection.
    Gone(u64),
}

pub fn run(config: Option<PathBuf>) -> Result<(), Error> {
    let path = ipc::socket_path().ok_or("XDG_RUNTIME_DIR is not set (or not absolute)")?;
    let mut appearance = Appearance::load(config).map_err(|e| format!("config: {e}"))?;

    let conn = Connection::connect_to_env()
        .map_err(|e| format!("failed to connect to wayland display: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let toplevel_list = ExtForeignToplevelListV1::interface().name;
    if !globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == toplevel_list))
    {
        return Err(format!("missing global: {toplevel_list}").into());
    }
    // Globals::bind reports each missing global.
    let g = Rc::new(Globals::bind(&globals, &qh).ok_or("required globals missing")?);
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let mut app = App::new(&globals, g, &qh, event_loop.handle());
    // The first roundtrip delivers the handles, the second their state.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;

    appearance.refresh(&app.loop_handle, &mut |m| eprintln!("wl-switcher: {m}"));
    app.config = appearance.config.clone();
    // Start the worker now, so icons and labels are ready for the first show.
    app.prefetch();

    let listener = bind(&path)?;
    let _socket = SocketFile(path.clone());
    listener.set_nonblocking(true)?;
    let mut next_client = 0;
    event_loop.handle().insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        move |_, listener, app| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        next_client += 1;
                        if let Err(e) = watch_client(app, stream, next_client) {
                            eprintln!("wl-switcher: client: {e}");
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => {
                        eprintln!("wl-switcher: accept: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    )?;
    eprintln!("wl-switcher: daemon listening on {}", path.display());

    // The client of the current show.
    let mut active: Option<(u64, UnixStream)> = None;
    loop {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|e| format!("wayland connection error: {e}"))?;

        for incoming in std::mem::take(&mut app.inbox) {
            match incoming {
                Incoming::Request { stream, .. } if active.is_some() || app.overlay.is_some() => {
                    answer(stream, Reply::Error("the switcher is already open".into()));
                }
                Incoming::Request {
                    client,
                    stream,
                    req,
                } => {
                    let mut warn = |m: String| send(&stream, &Reply::Warn(m));
                    if appearance.refresh(&app.loop_handle, &mut warn) {
                        app.config = appearance.config.clone();
                        app.config_changed();
                    }
                    match app.show(&req, &mut warn) {
                        Ok(()) => active = Some((client, stream)),
                        Err(e) => {
                            app.end_show();
                            answer(stream, Reply::Error(e));
                        }
                    }
                }
                Incoming::Gone(client) => {
                    if active.as_ref().is_some_and(|(c, _)| *c == client) {
                        app.cancel();
                    }
                }
            }
        }

        if let Some(outcome) = app.outcome.take() {
            app.end_show();
            // Make sure the switcher is gone before the client acts on the
            // result, so the compositor focuses the window after it.
            conn.roundtrip()?;
            if let Some((_, stream)) = active.take() {
                answer(
                    stream,
                    match outcome {
                        Outcome::Picked(id) => Reply::Picked(id),
                        Outcome::Cancelled => Reply::Cancelled,
                    },
                );
            }
            app.after_show();
        }

        // Follow a theme switch now, not on the next show: that would have
        // to render everything first.
        if active.is_none() && appearance.desktop.take_changed() {
            let mut warn = |m: String| eprintln!("wl-switcher: {m}");
            if appearance.refresh(&app.loop_handle, &mut warn) {
                app.config = appearance.config.clone();
                app.config_changed();
                app.prefetch();
            }
        }
    }
}

/// Read the client's request, then watch for it hanging up.
fn watch_client(app: &mut App, stream: UnixStream, client: u64) -> std::io::Result<()> {
    stream.set_nonblocking(true)?;
    // Some(..) until the request is complete.
    let mut reply_stream = Some(stream.try_clone()?);
    let mut buf = Vec::new();
    let source = Generic::new(stream, Interest::READ, Mode::Level);
    let result = app
        .loop_handle
        .insert_source(source, move |_, stream, app| {
            let mut chunk = [0; 4096];
            loop {
                match (&**stream).read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) if reply_stream.is_some() => buf.extend_from_slice(&chunk[..n]),
                    Ok(_) => {} // nothing is expected after the request
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        if let Some(reply) = reply_stream.take() {
                            reply_stream = take_request(app, client, reply, &mut buf);
                        }
                        return Ok(PostAction::Continue);
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
                if buf.len() > MAX_REQUEST {
                    if let Some(reply) = reply_stream.take() {
                        answer(reply, Reply::Error("request too large".into()));
                    }
                    return Ok(PostAction::Remove);
                }
            }
            // End of stream (or an error): the client is gone.
            if reply_stream.is_none() {
                app.inbox.push(Incoming::Gone(client));
            }
            Ok(PostAction::Remove)
        });
    result
        .map(|_| ())
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Queue the request in `buf` if complete. Gives `reply` back if not.
fn take_request(
    app: &mut App,
    client: u64,
    reply: UnixStream,
    buf: &mut Vec<u8>,
) -> Option<UnixStream> {
    match Request::decode(buf) {
        Ok(Some(req)) => {
            buf.clear();
            app.inbox.push(Incoming::Request {
                client,
                stream: reply,
                req,
            });
            None
        }
        Ok(None) => Some(reply),
        Err(e) => {
            answer(reply, Reply::Error(format!("bad request: {e}")));
            None
        }
    }
}

/// Best effort: replies are tiny, and a client that went away needs none.
fn send(stream: &UnixStream, reply: &Reply) {
    let _ = (&*stream).write_all(reply.encode().as_bytes());
}

/// Send the final reply and close, which also ends the client's watch.
fn answer(stream: UnixStream, reply: Reply) {
    send(&stream, &reply);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Bind the socket, replacing a stale one left by a daemon that died.
fn bind(path: &Path) -> Result<UnixListener, Error> {
    match UnixListener::bind(path) {
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(format!("a daemon is already listening on {}", path.display()).into());
            }
            std::fs::remove_file(path)?;
            Ok(UnixListener::bind(path)?)
        }
        other => Ok(other?),
    }
}

/// Removes the socket on exit.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The config, reloaded when its file changes, and the desktop's color
/// scheme.
struct Appearance {
    /// `--config`, if given.
    explicit: Option<PathBuf>,
    config: Config,
    config_mtime: Option<SystemTime>,
    desktop: DesktopScheme,
}

impl Appearance {
    /// Read the config; `refresh` applies the scheme.
    fn load(explicit: Option<PathBuf>) -> Result<Self, String> {
        let config = Config::read(explicit.as_deref())?;
        let config_mtime = Config::path(explicit.as_deref()).as_deref().and_then(mtime);
        Ok(Self {
            explicit,
            config,
            config_mtime,
            desktop: DesktopScheme::default(),
        })
    }

    /// Reload the config if its file changed and apply the desktop's color
    /// scheme. Errors keep the previous config and are reported to `warn`.
    /// Returns whether the config (or the scheme in use) changed.
    fn refresh(&mut self, handle: &LoopHandle<'static, App>, warn: &mut dyn FnMut(String)) -> bool {
        let config_mtime = Config::path(self.explicit.as_deref())
            .as_deref()
            .and_then(mtime);
        let mut config = None;
        if config_mtime != self.config_mtime {
            match Config::read(self.explicit.as_deref()) {
                Ok(c) => {
                    config = Some(c);
                    self.config_mtime = config_mtime;
                }
                Err(e) => warn(format!("config: {e} (keeping the previous config)")),
            }
        }
        let mut config = config.unwrap_or_else(|| self.config.clone());
        // Only watches the desktop if the config cares.
        config
            .appearance
            .apply(|| self.desktop.get(handle, &mut *warn));
        let changed = config != self.config;
        self.config = config;
        changed
    }
}
