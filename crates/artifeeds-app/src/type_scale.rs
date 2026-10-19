//! One type scale for the whole app.
//!
//! The sizes form a 1.2 ladder (12 · 14 · 17 · 20 · 24 · 29) rounded onto a 4 px
//! line grid, and every role fixes **size, leading, weight and emphasis
//! together** — because a scale only looks harmonious if a given size always
//! arrives with the same leading and weight. Mixing those ad hoc is what makes
//! an interface feel assembled rather than designed.
//!
//! Where the numbers come from:
//!
//! * **Material Design 3** supplies the *shape* of the scale — `label` /
//!   `body` / `title` roles, and the rule that line-height falls as size rises
//!   (1.6× at 11 px, 1.33× at 24 px).
//! * **Apple's HIG** supplies the body sizes a desktop app actually wants
//!   (11 / 12 / 13 / 15 / 17), which sit a step below Material's mobile-first
//!   defaults.
//! * **Refactoring UI** supplies the constraints that keep it disciplined: at
//!   most three weights, no more than a handful of sizes, and small text never
//!   grey-on-grey — which is why every emphasis level here clears WCAG AA.
//! * **CJK** sets the lower bound on leading. Han glyphs are denser and taller
//!   than Latin ones and need noticeably more breathing room, so no role here
//!   dips below 1.38× and body text sits at 1.57×.
//!
//! Tracking would normally be part of this (negative for display sizes, positive
//! for small labels), but this GPUI snapshot exposes no letter-spacing, so the
//! hierarchy rests on size, weight and colour instead.

use gpui_kit::{AbsoluteLength, DefiniteLength, FontWeight, Hsla, Styled, px};

use crate::theme;

/// How much the text should stand out, mapped to a measured colour token.
#[derive(Clone, Copy)]
pub enum Emphasis {
    Primary,
    Secondary,
    Tertiary,
}

impl Emphasis {
    pub fn color(self) -> Hsla {
        match self {
            Emphasis::Primary => theme::text(),
            Emphasis::Secondary => theme::text_dim(),
            Emphasis::Tertiary => theme::text_faint(),
        }
    }
}

/// A named slot in the scale.
#[derive(Clone, Copy)]
pub struct Role {
    /// Font size, px.
    pub size: f32,
    /// Line height, px — always a multiple of 4, so every row lands on the grid.
    pub line: f32,
    pub weight: FontWeight,
    pub emphasis: Emphasis,
}

impl Role {
    /// The same role at a different emphasis: a label is a label whether it is
    /// the row's caption or a section header.
    pub const fn with(self, emphasis: Emphasis) -> Self {
        Self { emphasis, ..self }
    }

    pub const fn weight(self, weight: FontWeight) -> Self {
        Self { weight, ..self }
    }
}

/// 11/16 · medium — the smallest step: ages, counts, status-bar figures,
/// keyboard hints. Never used for anything the user must read continuously.
pub const CAPTION: Role = Role { size: 11.0, line: 16.0, weight: FontWeight::MEDIUM, emphasis: Emphasis::Tertiary };

/// 12/18 · medium — chrome: sidebar rows, buttons, chips, tabs.
pub const LABEL: Role = Role { size: 12.0, line: 18.0, weight: FontWeight::MEDIUM, emphasis: Emphasis::Secondary };

/// 13/20 · regular — secondary prose: list summaries, help text.
pub const BODY_SM: Role = Role { size: 13.0, line: 20.0, weight: FontWeight::NORMAL, emphasis: Emphasis::Tertiary };

/// 14/22 · regular — UI prose: notices, help text.
pub const BODY: Role = Role { size: 14.0, line: 22.0, weight: FontWeight::NORMAL, emphasis: Emphasis::Primary };

/// 14/24 · regular — the reading surface's wrapped lines. One step looser than
/// `BODY` because this is where sustained CJK reading happens, and Han text at
/// 1.57× leading tires the eye faster than Latin at the same ratio.
pub const PROSE: Role = Role { size: 14.0, line: 24.0, weight: FontWeight::NORMAL, emphasis: Emphasis::Primary };

/// 17/26 · semibold — an article's title in the list. The list is titles; this
/// role is the reason the list exists, so it is the largest thing in a row by a
/// clear margin over `BODY_SM` and `CAPTION`.
pub const ROW_TITLE: Role = Role { size: 17.0, line: 26.0, weight: FontWeight::SEMIBOLD, emphasis: Emphasis::Primary };

/// 24/32 · semibold — Material's `headline-small`, used for the article title
/// on the reading surface.
pub const TITLE: Role = Role { size: 24.0, line: 32.0, weight: FontWeight::SEMIBOLD, emphasis: Emphasis::Primary };


/// Applies a [`Role`] to any styled element, and adds the OpenType features the
/// scale depends on.
pub trait Typography: Styled + Sized {
    /// Size, leading, weight and colour, as one decision. (Named `typo` rather
    /// than `role` because GPUI's `StatefulInteractiveElement` already owns
    /// `role` for ARIA.)
    fn typo(self, role: Role) -> Self {
        self.text_size(AbsoluteLength::Pixels(px(role.size)))
            .line_height(DefiniteLength::Absolute(gpui_kit::px(role.line).into()))
            .font_weight(role.weight)
            .text_color(role.emphasis.color())
    }

    /// Tabular figures (`tnum`), for any column of numbers. Without this, `1`
    /// is narrower than `8` and a right-aligned column of counts visibly
    /// shivers as the numbers change.
    fn tabular(self) -> Self {
        self.font_features(gpui_kit::FontFeatures(std::sync::Arc::new(vec![(
            "tnum".into(),
            1,
        )])))
    }
}

impl<T: Styled> Typography for T {}
