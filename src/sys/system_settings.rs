//! The macOS settings wave follows: Reduce Transparency, Increase Contrast, light or dark
//! appearance, and the accent color. Not Reduce Motion: fluid motion is what wave is for.
//!
//! AppKit answers these on the main thread, so the main thread reads them at launch and whenever
//! macOS says one changed ([`refresh`]), and every other thread reads the latest copy
//! ([`current`]).

use std::cell::Cell;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSColor, NSColorSpace, NSWorkspace};
use parking_lot::Mutex;

use crate::common::config::parse_hex_color;

/// The border color value that follows macOS: the accent color for the focused window, the
/// separator color for the others.
pub const SYSTEM_COLOR: &str = "system";

/// The macOS settings wave follows, as last read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemSettings {
    pub reduce_transparency: bool,
    pub increase_contrast: bool,
    /// The accent color under the current appearance, as sRGB red, green, blue and alpha.
    pub accent: [f64; 4],
    /// The color macOS draws window edges and separators in, under the current appearance.
    pub separator: [f64; 4],
}

impl SystemSettings {
    /// Under Increase Contrast an unfocused outline is drawn at least this opaque.
    const CONTRAST_ALPHA: f64 = 0.6;
    /// Until the main thread first reads them, and in tests: no accessibility settings, macOS's
    /// default blue accent and a neutral gray separator.
    pub const FALLBACK: Self = Self {
        reduce_transparency: false,
        increase_contrast: false,
        accent: [0.0, 0.478, 1.0, 1.0],
        separator: [0.5, 0.5, 0.5, 0.35],
    };

    /// `color` from a border setting: a `#rrggbb[aa]` value, or [`SYSTEM_COLOR`], which takes
    /// `system`. `None` for anything else; config validation reports it.
    pub fn border_color(&self, color: &str, system: [f64; 4]) -> Option<[f64; 4]> {
        if color != SYSTEM_COLOR {
            return parse_hex_color(color);
        }
        let [r, g, b, a] = system;
        let a = if self.increase_contrast {
            a.max(Self::CONTRAST_ALPHA)
        } else {
            a
        };
        Some([r, g, b, a])
    }
}

static CURRENT: Mutex<SystemSettings> = Mutex::new(SystemSettings::FALLBACK);

/// The settings as last read by [`refresh`].
pub fn current() -> SystemSettings { *CURRENT.lock() }

/// Reads the settings again. Returns whether any changed.
pub fn refresh(mtm: MainThreadMarker) -> bool {
    let read = read(mtm);
    let mut current = CURRENT.lock();
    let changed = *current != read;
    if changed {
        tracing::info!(settings = ?read, "following macOS settings");
    }
    *current = read;
    changed
}

fn read(mtm: MainThreadMarker) -> SystemSettings {
    let workspace = NSWorkspace::sharedWorkspace();
    let fallback = SystemSettings::FALLBACK;
    // Accent and separator are dynamic colors: they resolve to concrete values only under an
    // appearance, so resolve them under the one wave's own windows get (light, dark, or a
    // high-contrast variant).
    let colors = Cell::new((fallback.accent, fallback.separator));
    let resolve = RcBlock::new(|| {
        colors.set((
            srgb(&NSColor::controlAccentColor()).unwrap_or(fallback.accent),
            srgb(&NSColor::separatorColor()).unwrap_or(fallback.separator),
        ));
    });
    NSApplication::sharedApplication(mtm)
        .effectiveAppearance()
        .performAsCurrentDrawingAppearance(&resolve);
    let (accent, separator) = colors.get();
    SystemSettings {
        reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
        increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
        accent,
        separator,
    }
}

fn srgb(color: &NSColor) -> Option<[f64; 4]> {
    let color = color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some([
        color.redComponent(),
        color.greenComponent(),
        color.blueComponent(),
        color.alphaComponent(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_border_color_follows_macos_and_a_hex_one_stays() {
        let system = SystemSettings::FALLBACK;
        let accent = system.accent;
        assert_eq!(system.border_color(SYSTEM_COLOR, accent), Some(accent));
        assert_eq!(
            system.border_color("#ff000080", accent),
            Some([1.0, 0.0, 0.0, 128. / 255.])
        );
        assert_eq!(system.border_color("blue", accent), None);

        let faint = [0.5, 0.5, 0.5, 0.1];
        let contrast = SystemSettings {
            increase_contrast: true,
            ..system
        };
        assert_eq!(
            contrast.border_color(SYSTEM_COLOR, faint),
            Some([0.5, 0.5, 0.5, 0.6])
        );
        assert_eq!(system.border_color(SYSTEM_COLOR, faint), Some(faint));
    }
}
