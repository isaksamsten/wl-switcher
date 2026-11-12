//! wl-switcher: an Alt+Tab app switcher for Wayland that prints the picked
//! window's ID. See PLAN.md for the design.

mod anim;
mod app;
mod bar;
mod cache;
mod capture;
mod config;
mod daemon;
mod dmabuf;
mod focus;
mod globals;
mod icons;
mod input;
mod ipc;
mod mru;
mod options;
mod overlay;
mod pixels;
mod scheme;
mod shadow;
mod strip;
mod switcher;
mod text;
mod toplevel;
mod worker;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use crate::ipc::{Reply, Request};
use crate::options::Options;

// Exit codes: 0 picked (the ID is on stdout), 1 cancelled or an error.

fn main() -> ExitCode {
    let opts = match Options::parse_args() {
        Ok(opts) => opts,
        Err(code) => return code,
    };
    if opts.daemon {
        return match daemon::run(opts.config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("wl-switcher: {e}");
                ExitCode::FAILURE
            }
        };
    }
    client(&opts.request()).unwrap_or_else(|| {
        eprintln!(
            "wl-switcher: no daemon running; start `wl-switcher --daemon` with the compositor"
        );
        ExitCode::FAILURE
    })
}

/// Hand `req` to the daemon and relay its answer. None: no daemon is
/// running.
fn client(req: &Request) -> Option<ExitCode> {
    let path = ipc::socket_path()?;
    let mut stream = UnixStream::connect(path).ok()?;
    stream.write_all(req.encode().as_bytes()).ok()?;
    // Keep the connection open while waiting: closing it cancels.
    let mut lines = BufReader::new(stream).lines();
    loop {
        let reply = match lines.next() {
            Some(Ok(line)) => Reply::decode(&line),
            Some(Err(e)) => Err(e.to_string()),
            None => Err("connection closed".into()),
        };
        match reply {
            Ok(Reply::Warn(m)) => eprintln!("wl-switcher: {m}"),
            Ok(Reply::Picked(id)) => {
                println!("{id}");
                return Some(ExitCode::SUCCESS);
            }
            Ok(Reply::Cancelled) => return Some(ExitCode::FAILURE),
            Ok(Reply::Error(m)) => {
                eprintln!("wl-switcher: {m}");
                return Some(ExitCode::FAILURE);
            }
            Err(e) => {
                eprintln!("wl-switcher: daemon: {e}");
                return Some(ExitCode::FAILURE);
            }
        }
    }
}
