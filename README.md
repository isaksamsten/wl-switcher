# wl-switcher

An Alt+Tab switcher for Wayland. Apps appear as a row of icons; Alt+grave
(the key above Tab on a US keyboard) opens a vertical strip of an app's windows,
with live previews. Release Alt to switch. A quick tap switches without showing
the overlay.

Windows are grouped by their Wayland `app_id`. Switching to an app selects its
most recently focused window.

wl-switcher doesn't focus windows itself. It prints the selected window's
`ext-foreign-toplevel` identifier, and a shell script passes that to your
compositor's IPC.

## Requirements

The compositor must support:

- `ext-foreign-toplevel-list-v1`
- `wlr-layer-shell-unstable-v1`
- `viewporter`
- `ext-foreign-toplevel-image-capture-source-v1`
- `ext-image-copy-capture-v1`

The capture protocols are required even with previews turned off. This won't
work on every Wayland compositor.

`alpha-modifier-v1` enables fades; without it, the overlay appears and disappears
without fading. `keyboard-shortcuts-inhibit-unstable-v1` lets the switcher handle
repeated Alt+Tab presses rather than triggering the compositor's binding again.

The setup below uses niri. An umbriel focus-stream example is also included.
The focus command and stream must use IDs that match the compositor's
`ext-foreign-toplevel` identifiers; if they don't, your scripts need to translate
them.

## Build and install

Requires Rust 1.88 or newer, a C toolchain, pkg-config, and the development
packages for libgbm and libxkbcommon. Package names vary by distribution.

```sh
cargo build --release
install -Dm755 target/release/wl-switcher ~/.local/bin/wl-switcher
```

Make sure `~/.local/bin` is in the compositor's `PATH`.

## Setup (niri)

There are three parts: a daemon, a focus-event stream, and key bindings that
run a small wrapper. The examples use `jq` to read niri's JSON output.

### 1. Track focus order

Save this as `~/.config/wl-switcher/focus-stream.sh`:

```sh
#!/bin/sh
niri msg -j event-stream | jq --unbuffered -r '
  if .WindowFocusChanged then .WindowFocusChanged.id // empty
  elif .WindowsChanged then (.WindowsChanged.windows[] | select(.is_focused) | .id)
  else empty end'
```

Make it executable:

```sh
chmod +x ~/.config/wl-switcher/focus-stream.sh
```

Add this to `~/.config/wl-switcher/config.toml`:

```toml
mru-event-stream = "~/.config/wl-switcher/focus-stream.sh"
```

The daemon runs this command with `sh -c` and reads one focused window ID per
line. It keeps the most recently focused windows first and restarts the stream
if it exits. Printing the current window when the stream starts gives it a
useful initial order.

Without a stream, the switcher still works, but it doesn't know the focus order.
Windows then appear in the order supplied on the command line, or the order in
which the compositor announced them.

### 2. Start the daemon

Add to niri's `config.kdl`:

```kdl
spawn-at-startup "wl-switcher" "--daemon"
```

For the current session, start `wl-switcher --daemon` in a terminal. The daemon
is required; client commands won't start one automatically.

### 3. Bind the keys

Save this as `~/.local/bin/switch-app` and make it executable:

```sh
#!/bin/sh
id=$(wl-switcher "$@") && niri msg action focus-window --id "$id"
```

Add these to your niri `binds` block:

```kdl
Alt+Tab { spawn "switch-app" "next"; }
Alt+Shift+Tab { spawn "switch-app" "prev"; }
Alt+grave { spawn "switch-app" "next-window"; }
Alt+Shift+grave { spawn "switch-app" "prev-window"; }
```

Each binding opens the switcher. Once open, it handles the keyboard itself until
you release Alt or cancel.

## Controls

| Key                             | Action                                                    |
| ------------------------------- | --------------------------------------------------------- |
| Alt+Tab                         | Open on the previous app, then cycle forward              |
| Alt+Shift+Tab                   | Open at the other end of the app row, then cycle backward |
| Alt+grave                       | Open on the front app's next window                       |
| Alt+Shift+grave                 | Open on the front app's last window                       |
| Tab / Shift+Tab, while open     | Next / previous app; close the window strip               |
| Grave / Shift+grave, while open | Expand the selected app and cycle its windows             |
| Arrow keys                      | Move the selection                                        |
| Release Alt                     | Pick the selection                                        |
| Enter or click                  | Pick                                                      |
| Escape                          | Cancel                                                    |

You can also move the selection with the pointer. Inside the switcher, the grave
key is matched by its physical keycode, so Shift and keyboard layout don't change
which key cycles windows.

## Command line

```sh
wl-switcher next                    # All windows
wl-switcher prev -o DP-1             # Show on a particular output
wl-switcher next-window 4 7 9        # Only these window IDs
wl-switcher --daemon --config /path/to/config.toml
```

IDs only restrict which windows are shown.

A successful pick prints one ID to stdout and exits with status 0. Cancellation,
a missing daemon, or an error exits with status 1.

## Configuration

The default file is `$XDG_CONFIG_HOME/wl-switcher/config.toml`, or
`~/.config/wl-switcher/config.toml`. Every setting is optional. Changes are read
on the next request; an invalid file leaves the previous configuration in use.

These are the defaults:

```toml
modifier = "auto"          # auto, alt, super, ctrl
show-delay = 100            # Milliseconds; a quicker tap never shows the overlay
icon-size = 96
icon-gap = 16
bar-position = 0.5         # Fraction of output height
max-width = 0.9            # Fraction of output width
labels = true
window-dots = true
previews = "live"          # live, static (one frame), off (icons and titles)
strip-card-height = 0.22   # Fraction of output height
strip-max-width = 0.5      # Fraction of output width
zoom = 1.09                # Selected window's scale; 1 disables zoom
response = 180             # Movement spring response, milliseconds
morph-response = 260       # App-to-window transition response, milliseconds
dim = 0.0                  # Black tint behind the overlay, 0–1
font = "sans-serif"
font-size = 14
```

With `modifier = "auto"`, releasing all non-Shift modifiers held when the
switcher opened picks the selection. Set a specific modifier if you need to
avoid that detection, for example `modifier = "super"` for Super-based bindings.

Colors and shadows go in `[appearance]`. Colors accept `RRGGBB` or `RRGGBBAA`,
with an optional `#`; alpha comes last. For example:

```toml
[appearance]
mode = "auto"             # auto, dark, light
panel-color = "#262626e6"
highlight-color = "#ffffff2e"
label-color = "#ffffff"
shadow-color = "#00000080" # Use 00000000 to disable shadows
shadow-softness = 30
shadow-spread = 0
shadow-offset = [0, 6]
icon-shadow-color = "#00000059" # Follows the icon's shape; 00000000 disables
icon-shadow-softness = 6
icon-shadow-offset = [0, 2]
# icon-theme = "Papirus"

[appearance.light]
panel-color = "#eeeeeee6"
highlight-color = "#0000002e"
label-color = "#202020"
dot-color = "#202020b3"
```

`[appearance.dark]` and `[appearance.light]` override the shared appearance
settings. Auto mode follows the desktop's color-scheme preference. Other
appearance keys are `dot-color`, `label-background`, `placeholder`, `title-color`
and `title-background`. See [`src/config.rs`](src/config.rs) for all defaults.

## Other compositors

Keep the same wrapper structure, but replace the focus command with your
compositor's equivalent. The stream is just a command that prints focused window
IDs, one per line. For umbriel, for example:

```sh
#!/bin/sh
umbriel subscribe windows | jq --unbuffered -r '.data[] | select(.active) | .id'
```

Repeated IDs are harmless. The daemon forgets closed windows and retains its
focus history if the stream restarts.

## Development

```sh
cargo test
```
