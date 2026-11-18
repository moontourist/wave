# Plan: make wave its own

Five goals from Matt (2026-11-18), done in batches from easiest to hardest:

1. Rename everything to wave, so the codebase never says "rift".
2. Window borders built into wave, like JankyBorders (the sketchybar borders tool) but moving in step with wave's tiling animations.
3. Accordion on any tile (largest, so last).
4. Keep memory as low as possible throughout.
5. README with screenshots and its own roadmap.

Order: Phase 0 (setup) → 1 rename → 2 README → 3 borders → 4 accordion. Memory (goal 4) is a check inside every phase, not a phase of its own.

Status: Phases 0–1 done and 2.1 done. 1.7, 2.2 and 2.3 wait for one screen session with Matt; that session also runs the 20-window-move memory test from 0.2.

---

## How every item is checked

After **each item** (not each phase), before moving on:

1. **Audit check:** `scripts/audit.sh <base>` runs clippy with the audit's lints on the code changed since `<base>`. It reports only new findings on changed lines:
   - panics: `unwrap_used`, `expect_used`, `panic`, `unreachable`, `indexing_slicing`, `string_slice`
   - unsafe: `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`, `missing_safety_doc`
   - style: `as_conversions`, `wildcard_enum_match_arm`, `unreachable_pub`, `clone_on_ref_ptr`

   The item is done only when this prints nothing, or each finding has an `#[expect(..., reason = "...")]`.
2. **Tests:** `cargo test --workspace` passes.
3. **Format:** nightly rustfmt on the changed files only.
4. **Memory**, for items that change runtime behavior: `footprint` of the running wave process against the Phase 0 baseline, recorded in the phase's results below.

Each phase ends with one commit (no co-author line). Live on-screen checks happen only when Matt says it's a good time.

---

## Phase 0: Setup (small)

- [x] **0.1 Audit script.** Add `scripts/audit.sh`, which does the check above. It replaces the throwaway scripts used for the rift audit.
- [x] **0.2 Memory baseline.** Record wave's footprint at idle and after 20 window moves, with and without animations. These are the numbers later phases are held to.
- [x] **0.3 Remove rift's own documents:** `manifesto.md`, `roadmap.md`, the demo GIF and the menu screenshots. They describe rift, and Phase 2 replaces them.

### Phase 0 results

- `scripts/audit.sh` works in both modes. A deliberate `v[0] + x.unwrap()` was reported as `indexing_slicing` and `unwrap_used` on its line. `clippy.toml` lets tests panic.
- Workspace counts at the start (`--counts`), the yardstick for the rename:
  - `as_conversions` 384, `clone_on_ref_ptr` 247, `unreachable_pub` 197, `wildcard_enum_match_arm` 131
  - `undocumented_unsafe_blocks` 77 (all in tests), `unreachable` 9, `string_slice` 5, `indexing_slicing` 4, `unwrap_used` 4, `todo` 2, `expect_used` 1
- Memory, read passively from the running build (release build from 10:55, before the audit commits):
  - Settings: `animate`, auto fps, `ease_out_cubic`, open/close animation on.
  - Idle after 3h41m: **24 MB footprint** (49 MB RSS).
  - Peak: **115 MB**.
  - The 20-window-move run, with and without animations, needs the screen, so it moves to the Phase 2 screen session.

## Phase 1: Rename to wave (wide but mechanical)

Scope today: 1,147 mentions in 117 files, 22 file names, 4 crates, 2 programs.

- [x] **1.1 Crates and folders:**
  - `rift-wm` becomes `wave`, with the library as `wave_wm`.
  - `rift-client`, `rift-protocol` and `rift-config-derive` become `wave-client`, `wave-protocol` and `wave-config-derive`.
  - Folders and `Cargo.lock` follow.
- [x] **1.2 Programs:** `rift` becomes `wave`, and `rift-cli` becomes `wave-cli`, including the launchd service's lookup of its sibling binary.
- [x] **1.3 System identity:**
  - The bundle ID, Mach service name and launchd label change from `git.acsandmann.rift` to `com.moontourist.wave`. Matt can pick another.
  - `RIFT_*` environment variables become `WAVE_*`.
- [x] **1.4 Config:**
  - The config folder moves from `~/.config/rift/` to `~/.config/wave/`.
  - `rift.default.toml` becomes `wave.default.toml`.
  - Its comments drop the rift-docs links.
  - No migration code, since that would mention rift. Matt's own config is moved by hand in 1.7.
- [x] **1.5 Code and UI text:**
  - Types and functions, for example `RiftMachClient` becomes `WaveMachClient`.
  - Log text, error messages, the menu bar, the Settings window title, CLI help, tests and CI workflows.
- [x] **1.6 Final sweep:** `git grep -i rift` finds nothing outside the attribution that Apache 2.0 requires:
  - `LICENSE`: the copyright lines.
  - `NOTICE`.
  - The README's credits line.
  - Those stay, because removing them would break the license.
- [ ] **1.7 Switch Matt's machine over (needs Matt):**
  - Stop and uninstall the rift service.
  - Copy the config to `~/.config/wave/`.
  - Install the wave service.
  - Grant Accessibility and Screen Recording to the new binary, since macOS ties them to the program's identity.

Memory: no change expected; confirm after 1.7.

### Phase 1 results (1.1–1.6)

- 995 replacements in 108 files, plus file and folder renames. The package is `wave-wm` (library `wave_wm`); the programs are `wave` and `wave-cli`.
- Mach service, launchd label and bundle ID are all `com.moontourist.wave`. The server's name prefix moved from `git.` to `com.`, so the server and `wave-client` agree.
- Data folders: config in `~/.config/wave/`, layouts and the master file in `~/.wave/`.
- Links: GitHub, docs and release notes point to `moontourist/wave` (the docs link goes to the README). The update check reads wave's releases.
- Removed:
  - The Matrix and sponsor links from the menu bar, Settings and its search.
  - The `release`, `docs` and `funding` GitHub files, which published to acsandmann's Homebrew tap and docs site. `rust.yml` (CI) and `tag.yml` stay.
- The banner's "built on rift" line is gone.
- Audit: `--counts` matches Phase 0 exactly, so no new findings. All 952 tests pass.
- Sweep: `git grep -i rift` finds only `LICENSE`, `NOTICE` and the README credits line. "drift" is untouched.
- Left for 1.7, on Matt's machine:
  - `launchctl bootout gui/$UID/git.acsandmann.rift`, then delete `~/Library/LaunchAgents/git.acsandmann.rift.plist`.
  - Copy `~/.config/rift` to `~/.config/wave`, and `~/.rift` to `~/.wave`.
  - Build, then `wave-cli service install` and `start`.
  - Re-grant Accessibility and Screen Recording.

## Phase 2: README and roadmap (small, needs Matt's screen once)

- [x] **2.1 Roadmap:** a "Roadmap" section in the README with done / in progress / next, starting with this plan's phases. It's kept current at the end of every phase.
- [ ] **2.2 Screenshots:**
  - A tiled workspace, the workspace overview, the menu bar menu, and a window opening mid-animation.
  - Taken with `screencapture` when Matt says it's a good time. They're saved as PNGs in `assets/screenshots/`, cropped and kept small.
- [ ] **2.3 README pass:** add the screenshots and the roadmap, and recheck every command against the renamed programs.

Phases 3 and 4 each end by adding their own screenshot and updating the roadmap.

## Phase 3: Window borders (large)

### How JankyBorders does it, and what wave does differently

JankyBorders is a separate program. It creates one SkyLight window per app window and draws the outline into it. It listens to WindowServer events, so the border moves after the window has moved. That's why it trails behind during animations.

wave already knows every window's frame on every animation tick, so it can move the border on the same tick. wave also already has the tools: the `cgs` crate's `CgsWindow` and `LayerTransaction` (SkyLight overlay windows with Core Animation layers) are what the open/close animation in `pop_in.rs` uses.

### Memory approach

- Draw the outline with a `CALayer`'s own `borderWidth`, `borderColor` and `cornerRadius`. Core Animation draws these without a bitmap, so a border costs about the same whatever the window size. Never draw into an image the size of the window.
- Keep a small pool of border windows and reuse them, so windows opening and closing don't allocate each time.
- Don't allocate anything on each animation frame: a tick only changes frames inside one transaction.
- The budget is set from the 3.1 prototype's measurement, then held for 3.2–3.5.

### Items

- [ ] **3.1 Prototype, focused window only:**
  - One border window, ordered just below the focused window, with a fixed color and width.
  - Moves on focus change.
  - Measure memory here and set the budget.
- [ ] **3.2 Config:** a `[settings.ui.borders]` section in `wave.default.toml`:
  - `enabled`
  - `width`
  - `active_color` and `inactive_color`, defaulting to Gruvbox
  - `radius`, matching macOS's window corners
  - `inactive`: whether unfocused windows get borders
  - All of it hot-reloads like the rest of the config.
- [ ] **3.3 All managed windows:**
  - Borders for every tiled and floating window on the visible workspaces, ordered relative to each window so overlapping floating windows layer correctly.
  - Hidden with the window on minimize, fullscreen, hide and workspace switch.
- [ ] **3.4 In step with animations:**
  - The animation thread moves each border to the same frame it sends that window, in the same tick.
  - The open/close animation scales and fades the border with its overlay, using the same easing curve and duration.
  - Focus changes fade between colors over `animation_duration`.
- [ ] **3.5 Catch moves wave didn't make:**
  - Track WindowServer move and resize events, for windows the user drags or apps resize themselves, so the border corrects within a frame.
  - Drag and stack previews stay on top of borders.
- [ ] **3.6 Results:** memory against the budget, a screenshot, and the roadmap update.

## Phase 4: Accordion on any tile (largest)

### What exists today

The default ("traditional") layout already lets any container be stacked with `toggle_stack`. Stacked windows overlap with `stack_offset` pixels of each one showing, and the "stack" layout is the traditional layout with everything in one stack. BSP and master-stack have no per-tile stacking; scrolling has column stacks.

### First, a short design check with Matt

- Should every layout support it: BSP, master-stack, scrolling and floating?
- Does "accordion" mean today's overlapping stack, or a true accordion? In a true accordion the focused window opens to most of the tile and the others shrink to slim strips.
- Which keys toggle it, and how does focus move inside one?
- How does it interact with borders: one border around the whole accordion, or one per window?

### Likely items (final list after the design check)

- [ ] **4.1 One accordion model** in the layout engine that any layout can attach to a tile, so it isn't built separately for each layout.
- [ ] **4.2** Traditional and stack layouts move onto it, with their behavior unchanged and pinned by the existing tests first.
- [ ] **4.3** BSP, then master-stack, then scrolling.
- [ ] **4.4** Animations: opening, closing and moving focus within an accordion, using the same easing.
- [ ] **4.5** Borders and native tabs (`stack_line`) on accordion tiles.
- [ ] **4.6 Results:** memory, a screenshot, and the roadmap update.

Memory: an accordion is just layout data (a few window IDs per tile), so it should add nothing measurable.
