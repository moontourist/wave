# [~] wave

![[~] wave: a tiling window manager for macOS](assets/logo.png)

A tiling window manager for macOS, with smooth animations and a codebase that stays up when something goes wrong.

## What it does
- **Many layouts.** Tiling (like i3), binary space partitioning (like bspwm), master-stack (like dwm), scrolling columns (like niri), stacks and floating windows.
- **Smooth motion.** Windows ease into place on the curve you pick (`animation_easing`, such as `ease_out_cubic`), at your slowest display's refresh rate (`animation_fps = 0.0`). With `window_open_close_animation`, new windows fly into their tile and closed ones shrink away with their neighbors.
- **No SIP changes.** Works with System Integrity Protection on, and with "Displays have separate Spaces" turned on.
- **Workspaces you can see.** A niri-style overview for dragging windows between workspaces, and trackpad swipes between them like native macOS.
- **Menu bar menu.** Switch workspaces and layouts, and save or restore layouts.
- **Focus follows the mouse,** with auto raise.
- **Live config.** Edit the TOML file and the change applies when you save.
- **Scriptable.** A Mach-port IPC lets other programs (sketchybar and others) query wave and react to its events.
- **Hard to crash.** Unexpected input becomes an error or a logged fallback, never a panic, and every `unsafe` block does one thing and says why it's sound.

## Install
wave builds from source for now (macOS only).

1. `cargo build --release && scripts/bundle.sh` builds `target/release/wave.app`. Move it to `/Applications`.
2. Copy `wave.default.toml` to `~/.config/wave/config.toml` and edit it to taste.
3. `/Applications/wave.app/Contents/MacOS/wave service install`, then the same with `service start`.
4. Give it Accessibility permission when macOS asks. The overview and the open and close animations also need Screen Recording.

To use `wave-cli` from anywhere, link it onto your PATH: `ln -s /Applications/wave.app/Contents/MacOS/wave-cli /usr/local/bin/wave-cli`. `wave service restart` picks up a new build.

## Configure
Every setting is in [`wave.default.toml`](wave.default.toml), with a comment explaining it. `wave-cli settings` opens the native Settings window.

## How it's built
Rust, using macOS's private SkyLight APIs for speed and Core Animation for the overlays.

| Path | What it is |
|---|---|
| `src/bin/wave.rs` | the window manager, and `wave service` to install and run it |
| `src/bin/wave-cli.rs` | the command line: query, execute, subscribe |
| `src/actor/` | one thread per job: the reactor (layout decisions), apps, animation, input |
| `src/layout_engine/` | the layouts |
| `src/sys/` | the macOS layer: AX, SkyLight, Mach ports, event taps |
| `src/ui/` | the menu bar, the workspace overview and Settings |
| `crates/wave-client/` | a Rust library for talking to wave over IPC |
| `assets/` | the banner above (`node assets/render.mjs` redraws it) and the app's Info.plist |

## Develop
- `cargo test --workspace` runs the tests.
- `scripts/audit.sh` checks a change against the code rules: no panics outside tests, documented single-operation `unsafe`, no unchecked casts. It prints nothing when the change is clean.
- `scripts/release.sh` builds the zip for a GitHub release: no local paths, no debug info, signed ad hoc.
- `scripts/bundle.sh` packs the release build into `wave.app` with its icon and signs it. With a local certificate, macOS keeps wave's Accessibility and Screen Recording permissions across rebuilds; without one, it asks again after each build. Make the certificate once: in Keychain Access, Certificate Assistant → Create a Certificate, name it `wave`, identity type Self Signed Root, certificate type Code Signing. The first signing asks for your login password: choose Always Allow.
- Format changed files with nightly rustfmt only: `rustfmt +nightly --edition 2024 <files>`. Stable `cargo fmt` skips the repo's unstable options and reformats everything.

## Roadmap
Done:
- [x] Spring animations like macOS's, at your display's refresh rate, with windows that pop in when they open and shrink away when they close
- [x] No panics outside tests, and every `unsafe` block documented and doing one thing
- [x] wave's own name, identity and config folder
- [x] Memory given back: about 17 MB idle and 20 MB after using the overview, down from 153 MB
- [x] Window borders, built in: an outline on every window that moves on the same animation frame as its window, fading between focused and unfocused colors as focus moves. Borders follow windows you drag or that move themselves, and sit under their window, so an overlapping window covers them
- [x] New windows pop into their tile like Hyprland's, with their border, while the neighbors slide aside

Next:
- [ ] **Fast with many windows.** Open a burst of windows and have them all tile quickly, with no input lag.
- [ ] **Accordion on any tile.** Fold a group of windows into one tile in any layout, not only the stack layout.

Every feature is held to a memory budget: wave idles at about 17 MB and returns there after the overview, moving windows adds nothing measurable, and new features are measured against that.

## Credits
wave is a modified version of [rift](https://github.com/acsandmann/rift) by acsandmann, which includes code from glide-wm by tmandry. Licensed under Apache 2.0: see [LICENSE](LICENSE) and [NOTICE](NOTICE).
