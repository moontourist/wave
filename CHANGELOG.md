# Changelog

All notable changes to wave are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and wave uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
