//! How the matcher reads a string: lowercase, without accents, one `char` per letter kept.
//!
//! "é" is found by "e", "ş" by "s", "ı" by "i" and "Rosé Pine" by "rose". The folding is a
//! small table of single-letter mappings — no new crate, and no mapping that changes a
//! string's length, so a highlighted position in the folded string is a position in the
//! original ([`Folded::origin`]). Marks combining a base letter (a decomposed "é" is "e" plus
//! U+0301) are dropped, and the letter keeps its place.

/// A string folded for matching, remembering where each kept letter came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Folded {
    chars: Vec<char>,
    origin: Vec<usize>,
}

impl Folded {
    pub(crate) fn new(text: &str) -> Folded {
        let mut chars = Vec::new();
        let mut origin = Vec::new();
        for (index, c) in text.chars().enumerate() {
            if let Some(folded) = fold_char(c) {
                chars.push(folded);
                origin.push(index);
            }
        }
        Folded { chars, origin }
    }

    pub(crate) fn chars(&self) -> &[char] {
        &self.chars
    }

    pub(crate) fn len(&self) -> usize {
        self.chars.len()
    }

    /// The position, in the original string, of the folded string's `index`th letter.
    pub(crate) fn origin(&self, index: usize) -> usize {
        self.origin.get(index).copied().unwrap_or(index)
    }

    /// Whether the folded string begins with `prefix` (already folded).
    pub(crate) fn starts_with(&self, prefix: &[char]) -> bool {
        self.chars.starts_with(prefix)
    }

    /// Where `needle` (already folded) first occurs.
    pub(crate) fn find(&self, needle: &[char]) -> Option<usize> {
        if needle.is_empty() || needle.len() > self.chars.len() {
            return None;
        }
        self.chars
            .windows(needle.len())
            .position(|window| window == needle)
    }
}

/// The folded form of one letter; `None` for a combining mark, which has no place of its own.
pub(crate) fn fold_char(c: char) -> Option<char> {
    if ('\u{0300}'..='\u{036f}').contains(&c) {
        return None;
    }
    // `İ` lowercases to `i` plus a combining dot: the first letter is the one kept.
    let lower = c.to_lowercase().next().unwrap_or(c);
    Some(match lower {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'æ' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'œ' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' | 'ß' => 's',
        'ţ' | 'ť' | 'ț' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    })
}

/// A query word folded into letters.
pub(crate) fn word(text: &str) -> Vec<char> {
    Folded::new(text).chars
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(text: &str) -> String {
        Folded::new(text).chars().iter().collect()
    }

    #[test]
    fn accents_and_case_fold_to_the_plain_letter() {
        assert_eq!(folded("Rosé Pine"), "rose pine");
        assert_eq!(folded("ŞIĞ çöl İstanbul ışık"), "sig col istanbul isik");
        assert_eq!(folded("Zoë Ångström"), "zoe angstrom");
    }

    #[test]
    fn a_decomposed_accent_keeps_the_letters_place() {
        // "e" + U+0301: the mark has no place of its own, the next letter keeps its origin.
        let decomposed = Folded::new("Ros\u{0065}\u{0301} Pine");
        assert_eq!(decomposed.chars().iter().collect::<String>(), "rose pine");
        assert_eq!(decomposed.origin(3), 3);
        assert_eq!(decomposed.origin(4), 5);
    }

    #[test]
    fn find_gives_the_first_occurrence() {
        let t = Folded::new("Split Right");
        assert_eq!(t.find(&word("rig")), Some(6));
        assert_eq!(t.find(&word("zzz")), None);
        assert_eq!(t.find(&[]), None);
    }
}
