//! Colour palette.
//!
//! Returned by functions rather than stored as `const`s because `Hsla` is not
//! const-constructible from a hex literal in this GPUI version. Each call is a
//! couple of float conversions, once per element per frame.

use gpui_kit::{Hsla, rgb};

pub fn bg() -> Hsla {
    rgb(0x0d1117).into()
}
pub fn panel() -> Hsla {
    rgb(0x121821).into()
}
pub fn panel_alt() -> Hsla {
    rgb(0x161d27).into()
}
pub fn toolbar() -> Hsla {
    rgb(0x101620).into()
}
pub fn border() -> Hsla {
    rgb(0x222b38).into()
}
pub fn border_soft() -> Hsla {
    rgb(0x1b232e).into()
}
/// Three levels of emphasis, each with the contrast ratio it earns on `bg()`,
/// measured rather than guessed:
///
/// | token | colour | on `bg()` | on `panel()` | WCAG |
/// | --- | --- | --- | --- | --- |
/// | `text` | `#c9d1d9` | 12.3:1 | 11.5:1 | AAA |
/// | `text_dim` | `#8b97a8` | 6.4:1 | 6.0:1 | AA |
/// | `text_faint` | `#78859a` | 5.1:1 | 4.8:1 | AA |
///
/// The primary tone is GitHub Primer's dark-theme body colour rather than pure
/// white: at 14:1 a light grey starts to halo against a dark surface, and the
/// eye reads the halation as blur. The tertiary tone used to be `#5d6878`, which
/// measures 3.35:1 — under the 4.5:1 floor for body text, and it was being used
/// at 11 px.
pub fn text() -> Hsla {
    rgb(0xc9d1d9).into()
}
pub fn text_dim() -> Hsla {
    rgb(0x8b97a8).into()
}
pub fn text_faint() -> Hsla {
    rgb(0x78859a).into()
}
pub fn accent() -> Hsla {
    rgb(0x7aa2f7).into()
}
pub fn accent_soft() -> Hsla {
    let c: Hsla = rgb(0x7aa2f7).into();
    c.opacity(0.14)
}
pub fn green() -> Hsla {
    rgb(0x9ece6a).into()
}
pub fn amber() -> Hsla {
    rgb(0xe0af68).into()
}
pub fn red() -> Hsla {
    rgb(0xf7768e).into()
}
pub fn violet() -> Hsla {
    rgb(0xbb9af7).into()
}
pub fn selected() -> Hsla {
    rgb(0x1b2740).into()
}
pub fn hover() -> Hsla {
    rgb(0x18202c).into()
}
pub fn overlay() -> Hsla {
    let c: Hsla = rgb(0x0a0e14).into();
    c.opacity(0.96)
}

/// Colour for a health indicator.
pub fn health_color(h: artifeeds_core::model::Health) -> Hsla {
    use artifeeds_core::model::Health::*;
    match h {
        Ok => green(),
        NotModified => accent(),
        Fetching => violet(),
        Backoff => amber(),
        Error => red(),
        Disabled => text_faint(),
        Idle => text_dim(),
    }
}
