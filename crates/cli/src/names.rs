//! Names read from chain data, as chibi shows them.

/// Whether `name` prints as itself. ENS names are chain data anyone can set, so a name with a
/// control character, a line or paragraph separator, a default-ignorable code point (which
/// renders as nothing, so the name could pass for another), or a private-use character is
/// dropped rather than shown.
pub fn printable(name: &str) -> bool {
    name.chars().all(|c| {
        !c.is_control()
            && (c == ' ' || !c.is_whitespace())
            && !matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..)
            && !DEFAULT_IGNORABLE
                .iter()
                .any(|&(first, last)| (first..=last).contains(&c))
    })
}

/// Default_Ignorable_Code_Point ranges from Unicode 16.0 DerivedCoreProperties.txt, adjacent
/// ranges merged, less U+200D ZERO WIDTH JOINER and U+FE0F VARIATION SELECTOR-16, which emoji
/// names need.
const DEFAULT_IGNORABLE: &[(char, char)] = &[
    ('\u{ad}', '\u{ad}'),
    ('\u{34f}', '\u{34f}'),
    ('\u{61c}', '\u{61c}'),
    ('\u{115f}', '\u{1160}'),
    ('\u{17b4}', '\u{17b5}'),
    ('\u{180b}', '\u{180f}'),
    ('\u{200b}', '\u{200c}'),
    ('\u{200e}', '\u{200f}'),
    ('\u{202a}', '\u{202e}'),
    ('\u{2060}', '\u{206f}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{fe00}', '\u{fe0e}'),
    ('\u{feff}', '\u{feff}'),
    ('\u{ffa0}', '\u{ffa0}'),
    ('\u{fff0}', '\u{fff8}'),
    ('\u{1bca0}', '\u{1bca3}'),
    ('\u{1d173}', '\u{1d17a}'),
    ('\u{e0000}', '\u{e0fff}'),
];
