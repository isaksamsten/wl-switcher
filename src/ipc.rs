//! Client/daemon protocol over a Unix socket. Pure, unit tested.
//!
//! The framing is wl-expose's: both directions are lines of
//! `<word>[ <escaped payload>]`, where the payload escapes `\` as `\\` and
//! newlines as `\n`. The client sends a request terminated by `end`.
//!
//! Unlike wl-expose, a request while the switcher is open doesn't toggle it:
//! it is a key press. It is applied to the open switcher and answered with
//! `forwarded` right away. Only the client that opened the switcher waits for
//! the outcome (`picked`, `cancelled` or `error`), and closing its connection
//! cancels.

use std::path::PathBuf;

use crate::switcher::Step;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open with this step, or apply it to the open switcher.
    Step(Step),
    /// Close the open switcher without picking.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub command: Command,
    /// Ext toplevel identifiers of the windows to show; empty: all. A
    /// filter: the daemon orders them by focus (see mru.rs). Ignored while
    /// open.
    pub ids: Vec<String>,
    /// The output to show on, by name.
    pub output: Option<String>,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            command: Command::Step(Step::NextApp),
            ids: Vec::new(),
            output: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Warn(String),
    /// The switcher closed on this window.
    Picked(String),
    Cancelled,
    /// Applied to a switcher another client opened; that one gets the outcome.
    Forwarded,
    Error(String),
}

/// `$XDG_RUNTIME_DIR/wl-switcher-$WAYLAND_DISPLAY.sock`, so nested or
/// parallel compositors each get their own daemon.
pub fn socket_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    socket_path_in(dir, &display)
}

fn socket_path_in(dir: PathBuf, display: &str) -> Option<PathBuf> {
    // WAYLAND_DISPLAY may be an absolute path.
    let name = display.replace('/', "_");
    dir.is_absolute()
        .then(|| dir.join(format!("wl-switcher-{name}.sock")))
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            _ => return Err(format!("bad escape in '{s}'")),
        }
    }
    Ok(out)
}

fn line(word: &str, payload: Option<&str>) -> String {
    match payload {
        Some(p) => format!("{word} {}\n", escape(p)),
        None => format!("{word}\n"),
    }
}

fn split(line: &str) -> (&str, Option<&str>) {
    match line.split_once(' ') {
        Some((word, payload)) => (word, Some(payload)),
        None => (line, None),
    }
}

impl Command {
    pub fn name(self) -> &'static str {
        match self {
            Command::Step(Step::NextApp) => "next",
            Command::Step(Step::PrevApp) => "prev",
            Command::Step(Step::NextWindow) => "next-window",
            Command::Step(Step::PrevWindow) => "prev-window",
            Command::Cancel => "cancel",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "next" => Command::Step(Step::NextApp),
            "prev" => Command::Step(Step::PrevApp),
            "next-window" => Command::Step(Step::NextWindow),
            "prev-window" => Command::Step(Step::PrevWindow),
            "cancel" => Command::Cancel,
            _ => return None,
        })
    }
}

impl Request {
    pub fn encode(&self) -> String {
        let mut out = line("command", Some(self.command.name()));
        for id in &self.ids {
            out += &line("id", Some(id));
        }
        if let Some(output) = &self.output {
            out += &line("output", Some(output));
        }
        out += &line("end", None);
        out
    }

    /// Parse a request from the start of `buf`. `Ok(None)`: no `end` line yet.
    pub fn decode(buf: &[u8]) -> Result<Option<Self>, String> {
        let mut req = Request::default();
        let mut rest = buf;
        while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
            let text = std::str::from_utf8(&rest[..nl]).map_err(|_| "request is not UTF-8")?;
            rest = &rest[nl + 1..];
            let payload = |p: Option<&str>| unescape(p.ok_or(format!("'{text}': missing value"))?);
            match split(text) {
                ("end", None) => return Ok(Some(req)),
                ("command", p) => {
                    let name = payload(p)?;
                    req.command =
                        Command::from_name(&name).ok_or(format!("'{text}': unknown command"))?;
                }
                ("id", p) => req.ids.push(payload(p)?),
                ("output", p) => req.output = Some(payload(p)?),
                _ => return Err(format!("unknown request line '{text}'")),
            }
        }
        Ok(None)
    }
}

impl Reply {
    pub fn encode(&self) -> String {
        match self {
            Reply::Warn(m) => line("warn", Some(m)),
            Reply::Picked(id) => line("picked", Some(id)),
            Reply::Cancelled => line("cancelled", None),
            Reply::Forwarded => line("forwarded", None),
            Reply::Error(m) => line("error", Some(m)),
        }
    }

    /// Parse one line, without its newline.
    pub fn decode(text: &str) -> Result<Self, String> {
        let payload = |p: Option<&str>| unescape(p.ok_or(format!("'{text}': missing value"))?);
        Ok(match split(text) {
            ("warn", p) => Reply::Warn(payload(p)?),
            ("picked", p) => Reply::Picked(payload(p)?),
            ("cancelled", None) => Reply::Cancelled,
            ("forwarded", None) => Reply::Forwarded,
            ("error", p) => Reply::Error(payload(p)?),
            _ => return Err(format!("unknown reply '{text}'")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        for s in [
            "",
            "plain",
            "a b",
            "back\\slash",
            "new\nline",
            "\\n literal",
        ] {
            assert_eq!(unescape(&escape(s)).as_deref(), Ok(s));
            assert!(!escape(s).contains('\n'));
        }
        assert!(unescape("trailing\\").is_err());
        assert!(unescape("\\t").is_err());
    }

    #[test]
    fn commands() {
        for c in [
            Command::Step(Step::NextApp),
            Command::Step(Step::PrevApp),
            Command::Step(Step::NextWindow),
            Command::Step(Step::PrevWindow),
            Command::Cancel,
        ] {
            assert_eq!(Command::from_name(c.name()), Some(c));
        }
        assert_eq!(Command::from_name("toggle"), None);
    }

    #[test]
    fn request_roundtrip() {
        let req = Request {
            command: Command::Step(Step::PrevWindow),
            ids: vec!["1".into(), "x y".into(), "a\nb".into()],
            output: Some("DP-1".into()),
        };
        assert_eq!(Request::decode(req.encode().as_bytes()), Ok(Some(req)));
        let default = Request::default();
        assert_eq!(default.encode(), "command next\nend\n");
        assert_eq!(
            Request::decode(default.encode().as_bytes()),
            Ok(Some(default))
        );
    }

    #[test]
    fn request_incomplete_or_bad() {
        assert_eq!(Request::decode(b""), Ok(None));
        assert_eq!(Request::decode(b"id 1\nen"), Ok(None));
        assert!(Request::decode(b"bogus\nend\n").is_err());
        assert!(Request::decode(b"id\nend\n").is_err());
        assert!(Request::decode(b"command sideways\nend\n").is_err());
        assert!(Request::decode(b"\xff\nend\n").is_err());
    }

    #[test]
    fn reply_roundtrip() {
        for r in [
            Reply::Warn("no window with ID '3'".into()),
            Reply::Picked("a\\b".into()),
            Reply::Cancelled,
            Reply::Forwarded,
            Reply::Error("no windows to show".into()),
        ] {
            let enc = r.encode();
            assert_eq!(Reply::decode(enc.strip_suffix('\n').unwrap()), Ok(r));
        }
        assert!(Reply::decode("picked").is_err());
        assert!(Reply::decode("hello").is_err());
    }

    #[test]
    fn paths() {
        let p = |dir: &str, d: &str| socket_path_in(PathBuf::from(dir), d);
        assert_eq!(
            p("/run/user/1000", "wayland-1"),
            Some("/run/user/1000/wl-switcher-wayland-1.sock".into())
        );
        assert_eq!(
            p("/run", "/tmp/w"),
            Some("/run/wl-switcher-_tmp_w.sock".into())
        );
        assert_eq!(p("relative", "wayland-0"), None);
    }
}
