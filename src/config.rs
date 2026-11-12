//! Settings from the config file (TOML).
//!
//! Looked up at `$XDG_CONFIG_HOME/wl-switcher/config.toml` (default
//! `~/.config/wl-switcher/config.toml`) unless `--config` names a file. A
//! missing default file means the defaults; every key is optional.
//!
//! Colors live in `[appearance]`; `[appearance.dark]` and
//! `[appearance.light]` override its keys in the desktop's dark or light
//! mode (or the one `mode` forces).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Deserializer, de::Error as _};

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Which modifier's release picks.
    pub modifier: Modifier,
    /// Milliseconds before anything is visible, so a quick tap shows
    /// nothing.
    #[serde(deserialize_with = "duration")]
    pub show_delay: u32,
    /// App icon size in the bar.
    #[serde(deserialize_with = "icon_size")]
    pub icon_size: u32,
    /// Space between icons.
    #[serde(deserialize_with = "px")]
    pub icon_gap: u32,
    /// Vertical center of the bar, as a fraction of the output height.
    #[serde(deserialize_with = "unit")]
    pub bar_position: f64,
    /// Widest the bar gets, as a fraction of the output width.
    #[serde(deserialize_with = "fraction")]
    pub max_width: f64,
    /// The selected app's name under its icon.
    pub labels: bool,
    /// Dots under the icons of apps with several windows.
    pub window_dots: bool,
    /// Window previews in the strip.
    pub previews: Previews,
    /// Strip card height, as a fraction of the output height.
    #[serde(deserialize_with = "fraction")]
    pub strip_card_height: f64,
    /// Widest a strip card gets, as a fraction of the output width.
    #[serde(deserialize_with = "fraction")]
    pub strip_max_width: f64,
    /// Zoom factor of the selected card; 1 disables zooming.
    #[serde(deserialize_with = "zoom")]
    pub zoom: f64,
    /// Spring response for movement, in milliseconds.
    #[serde(deserialize_with = "response")]
    pub response: u32,
    /// Spring response for the apps/windows morph, in milliseconds.
    #[serde(deserialize_with = "response")]
    pub morph_response: u32,
    /// Alpha of a full-screen black tint behind the switcher; 0: none.
    #[serde(deserialize_with = "unit")]
    pub dim: f64,
    /// Font family for labels and titles (and letter icons).
    pub font: String,
    /// Label and title font size.
    #[serde(deserialize_with = "font_size")]
    pub font_size: u32,
    /// Command (run with `sh -c`) that prints the ID of each window that
    /// gains focus, one per line.
    pub mru_event_stream: Option<String>,
    pub appearance: Appearance,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            modifier: Modifier::default(),
            show_delay: 100,
            icon_size: 96,
            icon_gap: 16,
            bar_position: 0.5,
            max_width: 0.9,
            labels: true,
            window_dots: true,
            previews: Previews::default(),
            strip_card_height: 0.22,
            strip_max_width: 0.5,
            zoom: 1.09,
            response: 180,
            morph_response: 260,
            dim: 0.0,
            font: "sans-serif".into(),
            font_size: 14,
            mru_event_stream: None,
            appearance: Appearance::default(),
        }
    }
}

/// The modifier whose release picks.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Modifier {
    /// Whichever non-Shift modifiers are held when the switcher opens.
    #[default]
    Auto,
    Alt,
    Super,
    Ctrl,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Previews {
    /// Live while the strip is open.
    #[default]
    Live,
    /// One frame per window.
    Static,
    /// Icons and titles only.
    Off,
}

/// Colors: the keys of `[appearance]`, and of its `dark` and `light`
/// tables, which override them.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Theme {
    /// The bar's background.
    #[serde(deserialize_with = "color")]
    pub panel_color: u32,
    /// Behind the selected icon.
    #[serde(deserialize_with = "color")]
    pub highlight_color: u32,
    /// The window dots under icons.
    #[serde(deserialize_with = "color")]
    pub dot_color: u32,
    #[serde(deserialize_with = "color")]
    pub label_color: u32,
    #[serde(deserialize_with = "color")]
    pub label_background: u32,
    /// Color of windows without a preview.
    #[serde(deserialize_with = "color")]
    pub placeholder: u32,
    /// Shadow color; alpha 0 disables shadows.
    #[serde(deserialize_with = "color")]
    pub shadow_color: u32,
    /// Shadow blur radius.
    #[serde(deserialize_with = "px")]
    pub shadow_softness: u32,
    /// How much the shadow box is grown (or shrunk, if negative).
    #[serde(deserialize_with = "signed_px")]
    pub shadow_spread: i32,
    /// How far the shadow is moved, `[x, y]`.
    #[serde(deserialize_with = "offset")]
    pub shadow_offset: [i32; 2],
    /// Icon theme (a directory name); default: gsettings, then hicolor.
    pub icon_theme: Option<String>,
    #[serde(deserialize_with = "color")]
    pub title_color: u32,
    #[serde(deserialize_with = "color")]
    pub title_background: u32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            panel_color: 0x262626e6,
            highlight_color: 0xffffff2e,
            dot_color: 0xffffffb3,
            label_color: 0xffffffff,
            label_background: 0x00000000,
            placeholder: 0x3a3a3aff,
            shadow_color: 0x00000080,
            shadow_softness: 30,
            shadow_spread: 0,
            shadow_offset: [0, 6],
            icon_theme: None,
            title_color: 0xffffffff,
            title_background: 0x000000b3,
        }
    }
}

/// Which of `[appearance.dark]` and `[appearance.light]` applies.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Follow the desktop's color scheme.
    #[default]
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    #[default]
    Dark,
    Light,
}

/// The `[appearance]` table, resolved for both schemes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Appearance {
    pub mode: Mode,
    /// `[appearance]` with `[appearance.dark]` over it.
    pub dark: Theme,
    /// `[appearance]` with `[appearance.light]` over it.
    pub light: Theme,
    /// The scheme in use; set by `apply`, not read from the file.
    pub scheme: Scheme,
}

impl Appearance {
    /// The theme of the scheme in use.
    pub fn theme(&self) -> &Theme {
        match self.scheme {
            Scheme::Dark => &self.dark,
            Scheme::Light => &self.light,
        }
    }

    /// Pick the scheme: `mode`, or with `auto`, the desktop's, which
    /// `desktop` gives (only asked if the themes differ).
    pub fn apply(&mut self, desktop: impl FnOnce() -> Scheme) {
        self.scheme = match self.mode {
            Mode::Dark => Scheme::Dark,
            Mode::Light => Scheme::Light,
            Mode::Auto if self.dark == self.light => Scheme::Dark,
            Mode::Auto => desktop(),
        };
    }
}

impl Scheme {
    pub fn new(prefers_dark: bool) -> Self {
        if prefers_dark {
            Scheme::Dark
        } else {
            Scheme::Light
        }
    }
}

impl<'de> Deserialize<'de> for Appearance {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut base = toml::Table::deserialize(d)?;
        let mode = match base.remove("mode") {
            Some(v) => v
                .try_into()
                .map_err(|e| D::Error::custom(format!("mode: {e}")))?,
            None => Mode::default(),
        };
        let mut overrides = |name: &str| match base.remove(name) {
            None => Ok(toml::Table::new()),
            Some(toml::Value::Table(t)) => Ok(t),
            Some(_) => Err(D::Error::custom(format!("{name}: expected a table"))),
        };
        let (dark, light) = (overrides("dark")?, overrides("light")?);
        // Report errors in the shared keys once, without a table name.
        Theme::deserialize(base.clone()).map_err(D::Error::custom)?;
        let theme = |name: &str, overrides: toml::Table| {
            let mut table = base.clone();
            table.extend(overrides);
            Theme::deserialize(table).map_err(|e| D::Error::custom(format!("{name}: {e}")))
        };
        Ok(Self {
            mode,
            dark: theme("dark", dark)?,
            light: theme("light", light)?,
            scheme: Scheme::default(),
        })
    }
}

/// Run `cmd` with `args`; its stdout if it succeeds.
fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8(out.stdout).ok()
}

/// `org.freedesktop.appearance color-scheme`: 0 no preference, 1 dark, 2 light.
pub fn portal_prefers_dark() -> Option<bool> {
    let out = output(
        "gdbus",
        &[
            "call",
            "--session",
            "--timeout=1",
            "--dest=org.freedesktop.portal.Desktop",
            "--object-path=/org/freedesktop/portal/desktop",
            "--method=org.freedesktop.portal.Settings.ReadOne",
            "org.freedesktop.appearance",
            "color-scheme",
        ],
    )?;
    parse_portal(&out)
}

/// `(<uint32 1>,)`, or `(<<uint32 1>>,)` from the older `Read`; also the
/// `SettingChanged` lines of `gdbus monitor`.
pub fn parse_portal(out: &str) -> Option<bool> {
    let rest = &out[out.find("uint32 ")? + "uint32 ".len()..];
    let n: u32 = rest[..rest.find(|c: char| !c.is_ascii_digit())?]
        .parse()
        .ok()?;
    Some(n == 1)
}

pub fn gsettings_prefers_dark() -> Option<bool> {
    let out = output(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    )?;
    Some(parse_gsettings(&out))
}

/// `'prefer-dark'`, from `gsettings get`.
pub fn parse_gsettings(value: &str) -> bool {
    value.trim().trim_matches('\'') == "prefer-dark"
}

impl Config {
    /// The colors in use.
    pub fn theme(&self) -> &Theme {
        self.appearance.theme()
    }

    /// The shadow settings; None if shadows are disabled.
    pub fn shadow(&self) -> Option<crate::shadow::Settings> {
        let t = self.theme();
        (t.shadow_color & 0xff != 0).then(|| crate::shadow::Settings {
            color: t.shadow_color,
            softness: t.shadow_softness as f64,
            spread: t.shadow_spread,
            offset: (t.shadow_offset[0], t.shadow_offset[1]),
        })
    }

    /// The style of the app name label; None if labels are disabled.
    pub fn label_style(&self) -> Option<crate::text::Style> {
        self.labels.then(|| crate::text::Style {
            font: self.font.clone(),
            size: self.font_size as f64,
            color: self.theme().label_color,
            background: self.theme().label_background,
        })
    }

    /// The style of a window's title pill.
    pub fn title_style(&self) -> crate::text::Style {
        crate::text::Style {
            font: self.font.clone(),
            size: self.font_size as f64,
            color: self.theme().title_color,
            background: self.theme().title_background,
        }
    }

    /// The file `read` reads: `explicit`, or the default config file.
    pub fn path(explicit: Option<&Path>) -> Option<PathBuf> {
        explicit.map(Path::to_path_buf).or_else(default_path)
    }

    /// Read `explicit`, or the default config file if None (missing is
    /// fine). The scheme is left for the caller to `apply`.
    pub fn read(explicit: Option<&Path>) -> Result<Self, String> {
        let Some(path) = Self::path(explicit) else {
            return Ok(Self::default());
        };
        let required = explicit.is_some();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Self::parse(&text).map_err(|e| format!("{}: {}", path.display(), e.trim_end()))
    }

    fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}

/// `$XDG_CONFIG_HOME/wl-switcher/config.toml`, falling back to `~/.config`.
fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("wl-switcher").join("config.toml"))
}

/// RRGGBB or RRGGBBAA, optionally with a leading '#', as 0xRRGGBBAA.
fn parse_color(s: &str) -> Result<u32, String> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    let invalid = || format!("expected RRGGBB or RRGGBBAA, got '{s}'");
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let v = u32::from_str_radix(hex, 16).map_err(|_| invalid())?;
    match hex.len() {
        6 => Ok(v << 8 | 0xff),
        8 => Ok(v),
        _ => Err(invalid()),
    }
}

fn color<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    parse_color(&String::deserialize(d)?).map_err(D::Error::custom)
}

fn px<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = i64::deserialize(d)?;
    u32::try_from(v)
        .ok()
        .filter(|&v| v <= 100_000)
        .ok_or_else(|| D::Error::custom(format!("expected 0-100000, got {v}")))
}

fn check_signed_px<E: serde::de::Error>(v: i64) -> Result<i32, E> {
    i32::try_from(v)
        .ok()
        .filter(|v| v.abs() <= 100_000)
        .ok_or_else(|| E::custom(format!("expected -100000-100000, got {v}")))
}

fn signed_px<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    check_signed_px(i64::deserialize(d)?)
}

fn offset<'de, D: Deserializer<'de>>(d: D) -> Result<[i32; 2], D::Error> {
    let [x, y] = <[i64; 2]>::deserialize(d)?;
    Ok([check_signed_px(x)?, check_signed_px(y)?])
}

fn bounded<'de, D: Deserializer<'de>>(d: D, min: u32, max: u32) -> Result<u32, D::Error> {
    let v = i64::deserialize(d)?;
    u32::try_from(v)
        .ok()
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| D::Error::custom(format!("expected {min}-{max}, got {v}")))
}

fn font_size<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    bounded(d, 1, 1000)
}

fn icon_size<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    bounded(d, 16, 512)
}

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    bounded(d, 0, 10_000)
}

fn response<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    bounded(d, 1, 10_000)
}

/// 0 to 1.
fn unit<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = f64::deserialize(d)?;
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(D::Error::custom(format!("expected 0-1, got {v}")))
    }
}

/// More than 0, at most 1.
fn fraction<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = f64::deserialize(d)?;
    if v > 0.0 && v <= 1.0 {
        Ok(v)
    } else {
        Err(D::Error::custom(format!(
            "expected more than 0, at most 1, got {v}"
        )))
    }
}

fn zoom<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = f64::deserialize(d)?;
    if (1.0..=2.0).contains(&v) {
        Ok(v)
    } else {
        Err(D::Error::custom(format!("expected 1-2, got {v}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(parse_color("000000b3"), Ok(0x000000b3));
        assert_eq!(parse_color("#4c8dff"), Ok(0x4c8dffff));
        assert!(parse_color("+4c8dff").is_err());
        assert!(parse_color("4c8df").is_err());
        assert!(parse_color("4c8dffff00").is_err());
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::parse(""), Ok(Config::default()));
    }

    #[test]
    fn plan_example() {
        let c = Config::parse(r#"mru-event-stream = "~/.config/wl-switcher/focus-stream.sh""#);
        assert_eq!(
            c.unwrap().mru_event_stream.as_deref(),
            Some("~/.config/wl-switcher/focus-stream.sh")
        );
    }

    #[test]
    fn full() {
        let c = Config::parse(
            r##"
            modifier = "super"
            show-delay = 0
            icon-size = 64
            icon-gap = 8
            bar-position = 0.3
            max-width = 0.5
            labels = false
            window-dots = false
            previews = "static"
            strip-card-height = 0.3
            strip-max-width = 0.4
            zoom = 1
            response = 100
            morph-response = 300
            dim = 0.2
            font = "Inter"
            font-size = 12

            [appearance]
            mode = "light"
            panel-color = "#00000080"
            highlight-color = "ffffff40"
            dot-color = "ffffff"
            label-color = "eeeeee"
            label-background = "00000080"
            placeholder = "#222222"
            shadow-color = "ff000080"
            shadow-softness = 0
            shadow-spread = -3
            shadow-offset = [2, -4]
            icon-theme = "Papirus"
            title-color = "eeeeee"
            title-background = "00000080"
            "##,
        )
        .unwrap();
        assert_eq!(c.modifier, Modifier::Super);
        assert_eq!(c.show_delay, 0);
        assert_eq!((c.icon_size, c.icon_gap), (64, 8));
        assert_eq!((c.bar_position, c.max_width), (0.3, 0.5));
        assert!(!c.labels && c.label_style().is_none() && !c.window_dots);
        assert_eq!(c.previews, Previews::Static);
        assert_eq!((c.strip_card_height, c.strip_max_width), (0.3, 0.4));
        assert_eq!((c.zoom, c.response, c.morph_response), (1.0, 100, 300));
        assert_eq!(c.dim, 0.2);
        assert_eq!((c.font.as_str(), c.font_size), ("Inter", 12));
        assert_eq!(c.appearance.mode, Mode::Light);
        // Without overrides, both schemes are the same.
        assert_eq!(c.appearance.dark, c.appearance.light);
        let t = c.theme();
        assert_eq!(t.panel_color, 0x00000080);
        assert_eq!(t.highlight_color, 0xffffff40);
        assert_eq!(t.dot_color, 0xffffffff);
        assert_eq!(
            (t.label_color, t.label_background),
            (0xeeeeeeff, 0x00000080)
        );
        assert_eq!(t.placeholder, 0x222222ff);
        assert_eq!(t.shadow_color, 0xff000080);
        assert_eq!(t.shadow_softness, 0);
        assert_eq!(t.shadow_spread, -3);
        assert_eq!(t.shadow_offset, [2, -4]);
        assert_eq!(t.icon_theme.as_deref(), Some("Papirus"));
        assert_eq!(
            (t.title_color, t.title_background),
            (0xeeeeeeff, 0x00000080)
        );
    }

    #[test]
    fn schemes() {
        let mut c = Config::parse(
            r##"
            [appearance]
            panel-color = "101010"
            placeholder = "222222"

            [appearance.light]
            panel-color = "f0f0f0"
            "##,
        )
        .unwrap();
        let a = &c.appearance;
        assert_eq!(a.mode, Mode::Auto);
        assert_eq!(a.dark.panel_color, 0x101010ff);
        assert_eq!(a.light.panel_color, 0xf0f0f0ff);
        assert_eq!(
            (a.dark.placeholder, a.light.placeholder),
            (0x222222ff, 0x222222ff)
        );

        c.appearance.mode = Mode::Light;
        c.appearance.apply(|| unreachable!());
        assert_eq!(c.theme().panel_color, 0xf0f0f0ff);
        c.appearance.mode = Mode::Dark;
        c.appearance.apply(|| unreachable!());
        assert_eq!(c.theme().panel_color, 0x101010ff);
    }

    #[test]
    fn portal() {
        assert_eq!(parse_portal("(<uint32 1>,)\n"), Some(true));
        assert_eq!(parse_portal("(<<uint32 2>>,)\n"), Some(false));
        assert_eq!(parse_portal("(<uint32 0>,)\n"), Some(false));
        assert_eq!(parse_portal("()\n"), None);
    }

    #[test]
    fn invalid() {
        let err = |s: &str| Config::parse(s).is_err();
        assert!(err("icon-gap = -1"));
        assert!(err("icon-gap = 100001"));
        assert!(err("icon-size = 8"));
        assert!(err("icon-size = 2000"));
        assert!(err("modifier = \"hyper\""));
        assert!(err("previews = \"sometimes\""));
        assert!(err("bar-position = 1.5"));
        assert!(err("max-width = 0"));
        assert!(err("strip-card-height = 2"));
        assert!(err("dim = -0.1"));
        assert!(err("zoom = 0.9"));
        assert!(err("zoom = 3"));
        assert!(err("response = 0"));
        assert!(err("show-delay = 10001"));
        assert!(err("unknown = 1"));
        assert!(err("font-size = 0"));
        assert!(err("labels = 1"));
        // Appearance keys only go in [appearance].
        assert!(err("placeholder = \"222222\""));
        assert!(err("[appearance]\nicon-gap = 1"));
        assert!(err("[appearance]\nplaceholder = \"red\""));
        assert!(err("[appearance]\nshadow-spread = 100001"));
        assert!(err("[appearance]\nshadow-offset = [1]"));
        assert!(err("[appearance]\nshadow-softness = -1"));
        assert!(err("[appearance]\nmode = \"dim\""));
        assert!(err("[appearance]\ndark = 1"));
        assert!(err("[appearance.dark]\nplaceholder = \"red\""));
        assert!(err("[appearance.light]\nunknown = 1"));
        assert!(err("[appearance.light.dark]"));
        assert!(
            Config::parse("[appearance]\nshadow-color = \"00000000\"")
                .unwrap()
                .shadow()
                .is_none()
        );
    }
}
