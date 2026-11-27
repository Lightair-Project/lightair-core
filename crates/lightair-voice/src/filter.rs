//! What may be said aloud: local and fast, run on every piece before it reaches a voice. It drops
//! slurs (and, family friendly, swearing), personal data (emails, phone numbers, street
//! addresses), long numbers and spam, and makes text sayable (no markdown, emoji or links).

use std::sync::LazyLock;

use regex::Regex;

/// A short, conservative list of slurs and their common spellings, matched as whole words.
const SLURS: &[&str] = &[
    "n[i1]gg(?:a|er|uh)s?",
    "f[a@]gg?(?:ot)?s?",
    "k[i1]kes?",
    "sp[i1]cs?",
    "ch[i1]nks?",
    "tr[a@]nn(?:y|ies)",
    "r[e3]t[a@]rds?",
    "wetbacks?",
    "g[o0]oks?",
];

fn re(s: &str) -> Regex {
    Regex::new(s).expect("a valid pattern")
}

static SLUR: LazyLock<Regex> = LazyLock::new(|| re(&format!(r"(?i)\b(?:{})\b", SLURS.join("|"))));
static PROFANITY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(?:fuck\w*|shit\w*|cunt\w*|bitch\w*|dick(?:head)?s?|cock\w*|pussy|whore\w*|slut\w*|bastard\w*)\b")
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| re(r"[\w.+-]+@[\w-]+\.[\w.]+"));
static PHONE: LazyLock<Regex> = LazyLock::new(|| re(r"(?:\+?\d[\s.-]?){9,}"));
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b\d{1,5}\s+\w+(?:\s\w+)?\s(?:street|st|avenue|ave|road|rd|lane|ln|drive|dr|boulevard|blvd|court|ct)\b")
});
static LONG_NUMBER: LazyLock<Regex> = LazyLock::new(|| re(r"\d{7,}"));
static LINK: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(?:https?://|www\.)\S+"));
static BARE_DOMAIN: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b[\w-]+\.(?:com|net|org|io|gg|tv|ly|me|co|xyz|app|dev)\b(?:/\S*)?"));

/// Why something can't be said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Slur,
    Profanity,
    PersonalData,
    LongNumber,
    Spam,
}

impl Refused {
    pub fn reason(self) -> &'static str {
        match self {
            Refused::Slur => "a slur",
            Refused::Profanity => "swearing",
            Refused::PersonalData => "personal data",
            Refused::LongNumber => "a long number",
            Refused::Spam => "spam",
        }
    }
}

/// Whether `text` may be said: `family_friendly` also refuses swearing.
pub fn check(text: &str, family_friendly: bool) -> Result<(), Refused> {
    if SLUR.is_match(text) {
        return Err(Refused::Slur);
    }
    if family_friendly && PROFANITY.is_match(text) {
        return Err(Refused::Profanity);
    }
    if EMAIL.is_match(text) || PHONE.is_match(text) || ADDRESS.is_match(text) {
        return Err(Refused::PersonalData);
    }
    if LONG_NUMBER.is_match(text) {
        return Err(Refused::LongNumber);
    }
    let mut run = 0;
    let mut last = None;
    for c in text.chars() {
        run = if Some(c) == last { run + 1 } else { 1 };
        last = Some(c);
        if run >= 8 {
            return Err(Refused::Spam);
        }
    }
    Ok(())
}

/// Text made sayable: markdown marks, emoji and links out (a link becomes "a link").
pub fn speakable(text: &str) -> String {
    static CODE_BLOCK: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)```.*?```"));
    static CODE: LazyLock<Regex> = LazyLock::new(|| re(r"`([^`]*)`"));
    static BOLD: LazyLock<Regex> = LazyLock::new(|| re(r"\*\*?([^*]+)\*\*?"));
    static UNDER: LazyLock<Regex> = LazyLock::new(|| re(r"__?([^_]+)__?"));
    static HEADING: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^#+\s*"));
    static MD_LINK: LazyLock<Regex> = LazyLock::new(|| re(r"\[([^\]]+)\]\([^)]+\)"));
    static SPACE: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));
    let t = CODE_BLOCK.replace_all(text, " ");
    let t = CODE.replace_all(&t, "$1");
    let t = BOLD.replace_all(&t, "$1");
    let t = UNDER.replace_all(&t, "$1");
    let t = HEADING.replace_all(&t, "");
    let t = MD_LINK.replace_all(&t, "$1");
    let t = LINK.replace_all(&t, "a link");
    let t: String = t.chars().filter(|c| !is_emoji(*c)).collect();
    SPACE.replace_all(&t, " ").trim().to_string()
}

/// A viewer's text, ready to read aloud, or `None` when it shouldn't be: links and bare domains
/// are left out (never read), long numbers too, and anything [`check`] refuses isn't read at all.
/// At most `max_chars` characters (cut at a word).
pub fn viewer_text(text: &str, family_friendly: bool, max_chars: usize) -> Option<String> {
    let t = LINK.replace_all(text, " ");
    let t = BARE_DOMAIN.replace_all(&t, " ");
    let t = speakable(&t);
    if t.is_empty() || check(&t, family_friendly).is_err() {
        return None;
    }
    if t.chars().count() <= max_chars {
        return Some(t);
    }
    let cut: String = t.chars().take(max_chars).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a).trim_end_matches([',', ';', ':']).to_string();
    Some(cut)
}

fn is_emoji(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2300..=0x23FF | 0x2B00..=0x2BFF | 0xFE0F | 0x200D | 0xE0020..=0xE007F
    )
}
