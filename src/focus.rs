//! The `mru-event-stream`: a command the daemon runs with `sh -c` for its
//! whole lifetime, printing the ID of each window that gains focus, one per
//! line. Each line moves that window to the front of `App::mru`.
//!
//! The command runs in its own process group, so stopping it also stops the
//! rest of a pipeline. If it exits, it is restarted with backoff (1 s,
//! doubling to 30 s, reset once it ran for a minute); the order learned so
//! far stays. Its stderr goes to the daemon's, prefixed.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};

use crate::app::App;
use crate::mru;

/// Longer lines are not window IDs; they are skipped.
const MAX_LINE: usize = 1 << 16;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A stream that ran this long starts over at `FIRST_BACKOFF`.
const STABLE: Duration = Duration::from_secs(60);

const PREFIX: &str = "wl-switcher: mru-event-stream";

type Handle = LoopHandle<'static, App>;

#[derive(Default)]
pub struct FocusStream {
    /// The command from the config; None: no stream.
    command: Option<String>,
    running: Option<Running>,
    /// The pending restart after the stream exited.
    restart: Option<RegistrationToken>,
    /// The last wait before a restart; zero until the stream first exits.
    backoff: Duration,
}

impl FocusStream {
    /// Run `command` (from the config): start it, restart it if it
    /// changed, or stop it if None. Nothing happens if it's the same.
    pub fn configure(&mut self, command: Option<&str>, handle: &Handle) {
        let command = command.map(str::trim).filter(|c| !c.is_empty());
        if command == self.command.as_deref() {
            return;
        }
        if self.command.is_some() {
            eprintln!("{PREFIX}: the config changed; restarting");
        }
        self.stop(handle);
        self.command = command.map(Into::into);
        self.backoff = Duration::ZERO;
        self.start(handle);
    }

    fn stop(&mut self, handle: &Handle) {
        if let Some(token) = self.restart.take() {
            handle.remove(token);
        }
        if let Some(mut running) = self.running.take()
            && let Some(token) = running.stdout.take()
        {
            handle.remove(token);
        }
        // Dropping it killed it.
    }

    fn start(&mut self, handle: &Handle) {
        let Some(command) = &self.command else {
            return;
        };
        match Running::spawn(command, handle) {
            Ok(running) => self.running = Some(running),
            Err(e) => {
                eprintln!("{PREFIX}: failed to start: {e}");
                self.schedule_restart(Duration::ZERO, handle);
            }
        }
    }

    /// The stream's output ended (its source removes itself).
    fn exited(&mut self, handle: &Handle) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        let ran = running.started.elapsed();
        match running.stop().and_then(|s| s.code()) {
            Some(code) => eprintln!("{PREFIX}: exited with status {code}"),
            None => eprintln!("{PREFIX}: closed its output"),
        }
        self.schedule_restart(ran, handle);
    }

    fn schedule_restart(&mut self, ran: Duration, handle: &Handle) {
        self.backoff = backoff(self.backoff, ran);
        eprintln!("{PREFIX}: restarting in {} s", self.backoff.as_secs());
        let timer = Timer::from_duration(self.backoff);
        let token = handle.insert_source(timer, |_, _, app| {
            app.focus_stream.restart = None;
            app.focus_stream.start(&app.loop_handle);
            TimeoutAction::Drop
        });
        match token {
            Ok(token) => self.restart = Some(token),
            Err(e) => eprintln!("{PREFIX}: cannot restart: {e}"),
        }
    }
}

/// The wait before restarting a stream that ran for `ran`, after waiting
/// `prev` the last time (zero: never).
fn backoff(prev: Duration, ran: Duration) -> Duration {
    if prev.is_zero() || ran >= STABLE {
        FIRST_BACKOFF
    } else {
        (prev * 2).min(MAX_BACKOFF)
    }
}

/// The running command. Dropping it kills its process group.
struct Running {
    child: Child,
    started: Instant,
    /// The stdout source, once registered.
    stdout: Option<RegistrationToken>,
    /// Set once reaped.
    status: Option<ExitStatus>,
}

impl Running {
    fn spawn(command: &str, handle: &Handle) -> std::io::Result<Self> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // A backstop for a daemon killed by a signal; normally `stop` kills
        // the whole group.
        // SAFETY: prctl is async-signal-safe and touches no memory.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))
                .map_err(Into::into)
            });
        }
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().map(|s| File::from(OwnedFd::from(s)));
        let stderr = child.stderr.take().map(|s| File::from(OwnedFd::from(s)));
        // On error, dropping `running` kills the child.
        let mut running = Running {
            child,
            started: Instant::now(),
            stdout: None,
            status: None,
        };
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            return Err(std::io::Error::other("no pipes"));
        };
        running.stdout = Some(watch_stdout(stdout, handle)?);
        watch_stderr(stderr, handle);
        Ok(running)
    }

    /// Kill the process group and reap the command; its exit status.
    fn stop(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            // Before reaping, the group's ID can't have been reused.
            if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            self.status = self.child.wait().ok();
        }
        self.status
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read focus events: each ID moves its window to the front of the MRU
/// list. At the end of the output, the stream counts as exited.
fn watch_stdout(stdout: File, handle: &Handle) -> std::io::Result<RegistrationToken> {
    let mut lines = Lines::default();
    let source = Generic::new(stdout, Interest::READ, Mode::Level);
    handle
        .insert_source(source, move |_, stdout, app| {
            // Level-triggered: a single read per wakeup never blocks.
            let mut chunk = [0; 4096];
            match (&**stdout).read(&mut chunk) {
                Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
                Ok(0) | Err(_) => {
                    app.focus_stream.exited(&app.loop_handle);
                    return Ok(PostAction::Remove);
                }
                Ok(n) => lines.push(&chunk[..n], |line| {
                    if let Some(id) = mru::parse_line(line) {
                        app.mru.focus(id);
                    }
                }),
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Pass the stream's stderr on, prefixed. Ends by itself once the stream's
/// processes are gone.
fn watch_stderr(stderr: File, handle: &Handle) {
    let mut lines = Lines::default();
    let source = Generic::new(stderr, Interest::READ, Mode::Level);
    let result = handle.insert_source(source, move |_, stderr, _| {
        let mut chunk = [0; 4096];
        match (&**stderr).read(&mut chunk) {
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {}
            Ok(0) | Err(_) => return Ok(PostAction::Remove),
            Ok(n) => lines.push(&chunk[..n], |line| {
                let line = line.trim_end();
                if !line.is_empty() {
                    eprintln!("{PREFIX}: {line}");
                }
            }),
        }
        Ok(PostAction::Continue)
    });
    if let Err(e) = result {
        eprintln!("{PREFIX}: cannot read its stderr: {e}");
    }
}

/// Splits output into lines, keeping a partial line for the next chunk.
/// Lines longer than `MAX_LINE` are dropped whole.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    /// Inside an overlong line: skip to its end.
    skipping: bool,
}

impl Lines {
    fn push(&mut self, mut data: &[u8], mut each: impl FnMut(&str)) {
        while let Some(end) = data.iter().position(|&b| b == b'\n') {
            let skip = std::mem::take(&mut self.skipping) || self.buf.len() + end > MAX_LINE;
            if !skip {
                self.buf.extend_from_slice(&data[..end]);
                each(&String::from_utf8_lossy(&self.buf));
            }
            self.buf.clear();
            data = &data[end + 1..];
        }
        if !self.skipping {
            self.buf.extend_from_slice(data);
            if self.buf.len() > MAX_LINE {
                self.buf.clear();
                self.skipping = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Lines::default();
        let mut out = Vec::new();
        for chunk in chunks {
            lines.push(chunk, |l| out.push(l.to_string()));
        }
        out
    }

    #[test]
    fn partial_lines_are_kept() {
        assert_eq!(split(&[b"1\n2", b"3\n\n4"]), ["1", "23", ""]);
        assert_eq!(split(&[b"a", b"b", b"c\n"]), ["abc"]);
    }

    #[test]
    fn overlong_lines_are_dropped() {
        let long = vec![b'x'; MAX_LINE + 1];
        assert_eq!(split(&[b"1\n", &long, b"yy\n2\n"]), ["1", "2"]);
        let mut whole = long.clone();
        whole.extend_from_slice(b"\n3\n");
        assert_eq!(split(&[&whole]), ["3"]);
    }

    #[test]
    fn backoff_doubles_then_resets() {
        let s = Duration::from_secs;
        let quick = s(2);
        let mut wait = Duration::ZERO;
        let mut waits = Vec::new();
        for _ in 0..7 {
            wait = backoff(wait, quick);
            waits.push(wait.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff(wait, STABLE), s(1));
    }
}

