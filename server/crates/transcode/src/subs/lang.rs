//! ISO 639 language token normalisation for sidecar filenames (ADR-0010).
//! Small static table only — no ISO crate (Rule 4.4).

/// Maps a 2- or 3-letter language token to ISO 639-1 when known.
/// Returns `None` for unrecognised tokens (caller keeps an unlabelled track).
pub fn normalize_language(token: &str) -> Option<String> {
    let t = token.trim().to_ascii_lowercase();
    if t.len() == 2 {
        if KNOWN_639_1.iter().any(|c| *c == t) {
            return Some(t);
        }
        return None;
    }
    if t.len() == 3 {
        return THREE_TO_TWO
            .iter()
            .find(|(three, _)| *three == t)
            .map(|(_, two)| (*two).to_string());
    }
    None
}

const KNOWN_639_1: &[&str] = &[
    "aa", "ab", "ae", "af", "ak", "am", "an", "ar", "as", "av", "ay", "az", "ba", "be", "bg", "bh",
    "bi", "bm", "bn", "bo", "br", "bs", "ca", "ce", "ch", "co", "cr", "cs", "cu", "cv", "cy", "da",
    "de", "dv", "dz", "ee", "el", "en", "eo", "es", "et", "eu", "fa", "ff", "fi", "fj", "fo", "fr",
    "fy", "ga", "gd", "gl", "gn", "gu", "gv", "ha", "he", "hi", "ho", "hr", "ht", "hu", "hy", "hz",
    "ia", "id", "ie", "ig", "ii", "ik", "io", "is", "it", "iu", "ja", "jv", "ka", "kg", "ki", "kj",
    "kk", "kl", "km", "kn", "ko", "kr", "ks", "ku", "kv", "kw", "ky", "la", "lb", "lg", "li", "ln",
    "lo", "lt", "lu", "lv", "mg", "mh", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "na", "nb",
    "nd", "ne", "ng", "nl", "nn", "no", "nr", "nv", "ny", "oc", "oj", "om", "or", "os", "pa", "pi",
    "pl", "ps", "pt", "qu", "rm", "rn", "ro", "ru", "rw", "sa", "sc", "sd", "se", "sg", "si", "sk",
    "sl", "sm", "sn", "so", "sq", "sr", "ss", "st", "su", "sv", "sw", "ta", "te", "tg", "th", "ti",
    "tk", "tl", "tn", "to", "tr", "ts", "tt", "tw", "ty", "ug", "uk", "ur", "uz", "ve", "vi", "vo",
    "wa", "wo", "xh", "yi", "yo", "za", "zh", "zu",
];

/// Common bibliographic / terminology 639-2/B codes → 639-1.
const THREE_TO_TWO: &[(&str, &str)] = &[
    ("aar", "aa"),
    ("abk", "ab"),
    ("afr", "af"),
    ("amh", "am"),
    ("ara", "ar"),
    ("asm", "as"),
    ("aze", "az"),
    ("bel", "be"),
    ("ben", "bn"),
    ("bod", "bo"),
    ("bos", "bs"),
    ("bul", "bg"),
    ("cat", "ca"),
    ("ces", "cs"),
    ("cze", "cs"),
    ("dan", "da"),
    ("deu", "de"),
    ("ger", "de"),
    ("ell", "el"),
    ("gre", "el"),
    ("eng", "en"),
    ("spa", "es"),
    ("est", "et"),
    ("eus", "eu"),
    ("baq", "eu"),
    ("fas", "fa"),
    ("per", "fa"),
    ("fin", "fi"),
    ("fra", "fr"),
    ("fre", "fr"),
    ("gla", "gd"),
    ("gle", "ga"),
    ("guj", "gu"),
    ("heb", "he"),
    ("hin", "hi"),
    ("hrv", "hr"),
    ("hun", "hu"),
    ("hye", "hy"),
    ("arm", "hy"),
    ("ind", "id"),
    ("isl", "is"),
    ("ice", "is"),
    ("ita", "it"),
    ("jpn", "ja"),
    ("kat", "ka"),
    ("geo", "ka"),
    ("kor", "ko"),
    ("kur", "ku"),
    ("lat", "la"),
    ("lav", "lv"),
    ("lit", "lt"),
    ("mkd", "mk"),
    ("mac", "mk"),
    ("mal", "ml"),
    ("mar", "mr"),
    ("msa", "ms"),
    ("may", "ms"),
    ("nld", "nl"),
    ("dut", "nl"),
    ("nor", "no"),
    ("pol", "pl"),
    ("por", "pt"),
    ("ron", "ro"),
    ("rum", "ro"),
    ("rus", "ru"),
    ("slk", "sk"),
    ("slo", "sk"),
    ("slv", "sl"),
    ("srp", "sr"),
    ("swe", "sv"),
    ("tam", "ta"),
    ("tel", "te"),
    ("tha", "th"),
    ("tur", "tr"),
    ("ukr", "uk"),
    ("urd", "ur"),
    ("vie", "vi"),
    ("zho", "zh"),
    ("chi", "zh"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_and_three_letter() {
        assert_eq!(normalize_language("en").as_deref(), Some("en"));
        assert_eq!(normalize_language("ENG").as_deref(), Some("en"));
        assert_eq!(normalize_language("fre").as_deref(), Some("fr"));
        assert_eq!(normalize_language("deu").as_deref(), Some("de"));
    }

    #[test]
    fn unrecognised_is_none() {
        assert_eq!(normalize_language("xx"), None);
        assert_eq!(normalize_language("xyz"), None);
        assert_eq!(normalize_language("english"), None);
        assert_eq!(normalize_language("forced"), None);
    }
}
