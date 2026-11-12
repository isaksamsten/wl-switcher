//! wl-switcher: an Alt+Tab app switcher for Wayland that prints the picked
//! window's ID. See PLAN.md for the design.

// Phase 0: the pure core is in place; the Wayland side is not yet.
#![allow(dead_code)]

mod anim;
mod ipc;
mod mru;
mod options;
mod switcher;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use crate::ipc::{Reply, Request};
use crate::options::Options;

/// Exit code of an invocation that only moved an open switcher.
const FORWARDED: u8 = 2;

fn main() -> ExitCode {
    let opts = match Options::parse_args() {
        Ok(opts) => opts,
        Err(code) => return code,
    };
    if opts.use_daemon()
        && let Some(code) = client(&opts.request())
    {
        return code;
    }
    eprintln!("wl-switcher: not implemented yet: needs a running daemon (see PLAN.md, phase 1)");
    ExitCode::FAILURE
}

/// Hand `req` to a running daemon and relay its answer. None: no daemon is
/// running (or it went away before taking the request).
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
            Ok(Reply::Forwarded) => return Some(ExitCode::from(FORWARDED)),
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
