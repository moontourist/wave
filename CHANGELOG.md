# Changelog

All notable changes to wave are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and wave uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Window borders, built in (`[settings.ui.borders]`). Every window gets an outline that moves and resizes with it on the same animation frame. The focused window's outline is `active_color` and the rest are `inactive_color`; when focus moves they fade between the two, like Hyprland's. `inactive = false` shows only the focused window's outline. Each border costs about 16 KB.
- `scripts/stress.sh` opens a burst of Ghostty windows and measures how fast wave tiles them, its input lag and where its threads spend the time.

### Changed
- During a burst of new windows, only the first pops in and the rest slide into place, so each pop-in's preparation no longer delays the next window.
- The bsp layout splits a new window's tile along its longer side, as Hyprland's dwindle does. A wide tile puts the new window beside it and a tall one below, so a 32:9 display fits three windows side by side before stacking.

### Fixed
- A window that opens thin in one direction, such as a 48-point-tall Ghostty window opened while many are tiled, is tiled instead of skipped as a tooltip.
- Borders move in one commit per frame and run on their own thread, so with many windows they no longer slow window motion.
- Windows opened in quick succession tile in order, each splitting or following the one before it. macOS focuses none of them until the burst ends, which left every new window next to the one selected before it.

## [0.1.0] - 2026-11-18

wave's first release. It starts from [rift](https://github.com/acsandmann/rift) and adds these changes.

### Added
- Animations follow an easing curve (`animation_easing`) at the slowest display's refresh rate (`animation_fps = 0.0`).
- New windows fly into their tile and closed ones shrink away (`window_open_close_animation`).
- wave ships as `wave.app`, with its own `[~]` icon in Finder and the permission lists. `scripts/bundle.sh` builds and signs it.
- `scripts/audit.sh` checks a change against the code rules.

### Changed
- Renamed to wave: the binaries are `wave` and `wave-cli`, the config lives in `~/.config/wave/`, and the service is `com.moontourist.wave`.
- The overview takes its desktop background snapshot when it opens and frees it when it closes, and the service tells macOS to return freed large blocks right away (`MallocLargeCache=0`). wave idles at about 17 MB and returns to about 20 MB after the overview, down from 153 MB on a 7680×2160 display.

### Fixed
- `wave service restart` no longer leaves the service stopped when the service file has changed.
- No panics outside tests: unexpected input becomes an error or a logged fallback. Every `unsafe` block does one thing and documents why it's sound.

[Unreleased]: https://github.com/moontourist/wave/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/moontourist/wave/releases/tag/v0.1.0
