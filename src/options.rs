use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use crate::ipc::{Command, Request};
use crate::switcher::Step;

/// Switch between apps (Alt+Tab) and between an app's windows (Alt+`).
/// Bind each key combination to one invocation; the first opens the
/// switcher, later ones while it is open move the selection. Releasing the
/// modifier picks: the window's ID is printed (exit 0). Escape exits 1, and
/// an invocation that only moved an open switcher exits 2.
#[derive(Parser, Debug)]
#[command(version)]
pub struct Options {
    /// What this key press does
    #[arg(value_enum, required_unless_present_any = ["daemon", "list"])]
    pub action: Option<Action>,

    /// Ext toplevel identifiers in most-recently-used order, focused first
    /// (default: the daemon's own order)
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Print identifier<TAB>app_id<TAB>title per window
    #[arg(short, long, conflicts_with_all = ["action", "daemon"])]
    pub list: bool,

    /// Output to show on (a wl_output name, e.g. DP-1); default: the
    /// focused one
    #[arg(short, long, value_name = "NAME")]
    pub output: Option<String>,

    /// Config file (default: $XDG_CONFIG_HOME/wl-switcher/config.toml).
    /// Without --daemon, implies --no-daemon
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Run in the background, ready for later invocations
    #[arg(long, conflicts_with_all = ["action", "ids", "output", "no_daemon"])]
    pub daemon: bool,

    /// Show the switcher from this process even if a daemon is running
    #[arg(long)]
    pub no_daemon: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Action {
    /// Next app (Alt+Tab)
    Next,
    /// Previous app (Alt+Shift+Tab)
    Prev,
    /// Next window of the selected app (Alt+`)
    NextWindow,
    /// Previous window of the selected app (Alt+Shift+`)
    PrevWindow,
    /// Close the switcher without picking
    Cancel,
}

impl Action {
    pub fn command(self) -> Command {
        match self {
            Action::Next => Command::Step(Step::NextApp),
            Action::Prev => Command::Step(Step::PrevApp),
            Action::NextWindow => Command::Step(Step::NextWindow),
            Action::PrevWindow => Command::Step(Step::PrevWindow),
            Action::Cancel => Command::Cancel,
        }
    }
}

impl Options {
    /// Parse the command line. On `--help`, `--version` or an error, prints
    /// and returns the exit code (0 or 1).
    pub fn parse_args() -> Result<Self, ExitCode> {
        Self::try_parse().map_err(|e| {
            let _ = e.print();
            if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        })
    }

    pub fn request(&self) -> Request {
        Request {
            command: self.action.unwrap_or(Action::Next).command(),
            ids: self.ids.clone(),
            output: self.output.clone(),
        }
    }

    /// Hand the request to a running daemon (or an open one-shot switcher).
    pub fn use_daemon(&self) -> bool {
        !self.daemon && !self.list && !self.no_daemon && self.config.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Options, clap::Error> {
        Options::try_parse_from([&["wl-switcher"], args].concat())
    }

    #[test]
    fn cli() {
        Options::command().debug_assert();
        let o = parse(&["next-window", "-o", "DP-1", "3", "1"]).unwrap();
        assert_eq!(o.request().command, Command::Step(Step::NextWindow));
        assert_eq!(o.request().ids, ["3", "1"]);
        assert_eq!(o.request().output.as_deref(), Some("DP-1"));
        assert!(o.use_daemon());
        assert_eq!(
            parse(&["prev"]).unwrap().request().command,
            Command::Step(Step::PrevApp)
        );
        assert_eq!(
            parse(&["cancel"]).unwrap().request().command,
            Command::Cancel
        );
    }

    #[test]
    fn modes() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["sideways"]).is_err());
        assert!(parse(&["--daemon"]).unwrap().daemon);
        assert!(parse(&["--daemon", "next"]).is_err());
        assert!(parse(&["--daemon", "--no-daemon"]).is_err());
        assert!(!parse(&["--list"]).unwrap().use_daemon());
        assert!(parse(&["--list", "next"]).is_err());
        assert!(!parse(&["next", "--config", "c.toml"]).unwrap().use_daemon());
        assert!(!parse(&["next", "--no-daemon"]).unwrap().use_daemon());
    }
}
