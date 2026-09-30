//! Case-insensitive, order-preserving matching for names, paths and Korean
//! text. Every whitespace-separated query word must match. A Hangul
//! consonant in the query (ㄱ…ㅎ) also matches a syllable that starts with
//! it, and decomposed Hangul (as macOS stores file names) is composed first.

const SYLLABLE_BASE: u32 = 0xAC00;
const SYLLABLE_LAST: u32 = 0xD7A3;
/// Compatibility jamo for the 19 initial consonants, in syllable order.
const INITIALS: [char; 19] = [
    'ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ',
    'ㅌ', 'ㅍ', 'ㅎ',
];

/// Lowercase and compose conjoining Hangul jamo into syllables.
pub(crate) fn fold(text: &str) -> Vec<char> {
    let mut folded = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let code = ch as u32;
        if (0x1100..=0x1112).contains(&code)
            && let Some(&vowel) = chars.peek()
            && (0x1161..=0x1175).contains(&(vowel as u32))
        {
            chars.next();
            let mut syllable =
                SYLLABLE_BASE + ((code - 0x1100) * 21 + (vowel as u32 - 0x1161)) * 28;
            if let Some(&last) = chars.peek()
                && (0x11A8..=0x11C2).contains(&(last as u32))
            {
                chars.next();
                syllable += last as u32 - 0x11A7;
            }
            folded.extend(char::from_u32(syllable));
            continue;
        }
        folded.extend(ch.to_lowercase());
    }
    folded
}

fn initial(ch: char) -> Option<char> {
    let code = ch as u32;
    (SYLLABLE_BASE..=SYLLABLE_LAST)
        .contains(&code)
        .then(|| INITIALS[((code - SYLLABLE_BASE) / 588) as usize])
}

fn matches(query: char, text: char) -> bool {
    query == text || (INITIALS.contains(&query) && initial(text) == Some(query))
}

fn boundary(text: &[char], index: usize) -> bool {
    index == 0
        || matches!(
            text[index - 1],
            ' ' | '/' | '-' | '_' | '.' | ':' | '@' | '%' | '·' | '\t'
        )
}

/// Score one folded word against folded text: a contiguous run scores above
/// a scattered subsequence, and runs that start a word score higher.
fn word_score(word: &[char], text: &[char]) -> Option<u32> {
    if word.is_empty() {
        return Some(0);
    }
    let mut best: Option<u32> = None;
    // Each possible start of the first character; the tail is matched greedily.
    for start in (0..text.len()).filter(|&i| matches(word[0], text[i])) {
        let mut score: u32 = 16 + if boundary(text, start) { 24 } else { 0 };
        let mut previous = start;
        let mut matched = true;
        // A scattered match counts only inside one token, or when every
        // skipped-to character starts a word (an acronym such as "gtp").
        let mut one_token = true;
        let mut acronym = true;
        for &query in &word[1..] {
            let Some(offset) = text[previous + 1..].iter().position(|&c| matches(query, c)) else {
                matched = false;
                break;
            };
            let index = previous + 1 + offset;
            if offset > 0 {
                one_token &= !(previous + 1..=index).any(|i| boundary(text, i));
                acronym &= boundary(text, index);
            }
            score += if offset == 0 {
                12
            } else if boundary(text, index) {
                8
            } else {
                2
            };
            score = score.saturating_sub((offset as u32).min(8));
            previous = index;
        }
        if !matched {
            // Greedy positions from a later start are never earlier, so no
            // later start can match either.
            break;
        }
        if !one_token && !acronym {
            continue;
        }
        // Earlier matches win ties.
        let score = score.saturating_sub((start as u32).min(16));
        best = Some(best.map_or(score, |best| best.max(score)));
    }
    best
}

/// Match `query` against each field; the first field (the label) counts
/// double. `None` means some query word matched no field.
pub(crate) fn score(query: &str, fields: &[&str]) -> Option<u32> {
    let folded: Vec<Vec<char>> = fields.iter().map(|field| fold(field)).collect();
    let mut total = 0u32;
    for word in query.split_whitespace() {
        let word = fold(word);
        let best = folded
            .iter()
            .enumerate()
            .filter_map(|(index, field)| {
                word_score(&word, field).map(|score| if index == 0 { score * 2 } else { score })
            })
            .max()?;
        total = total.saturating_add(best);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_must_all_match_in_order_and_case_insensitively() {
        assert!(score("BUILD", &["builder"]).is_some());
        assert!(score("bdr", &["builder"]).is_some());
        assert!(score("rdb", &["builder"]).is_none());
        assert!(score("api codex", &["api-review", "codex · working"]).is_some());
        assert!(score("gtp", &["Go to pane"]).is_some());
        // Letters scattered across unrelated words do not match.
        assert!(score("bui", &["bash /Users/me/Documents/Projects"]).is_none());
        assert!(score("bui", &["Go to pane", "local builder"]).is_some());
        assert!(score("api gemini", &["api-review", "codex · working"]).is_none());
        assert_eq!(score("", &["anything"]), Some(0));
    }

    #[test]
    fn contiguous_and_word_start_matches_rank_higher() {
        let contiguous = score("rev", &["api-review"]).unwrap();
        let scattered = score("rev", &["river-event"]).unwrap();
        assert!(contiguous > scattered);
        let start = score("co", &["codex"]).unwrap();
        let middle = score("co", &["unicode"]).unwrap();
        assert!(start > middle);
        let label = score("api", &["api", "other"]).unwrap();
        let detail = score("api", &["other", "api"]).unwrap();
        assert!(label > detail);
    }

    #[test]
    fn korean_matches_syllables_initials_and_decomposed_paths() {
        assert!(score("개발", &["개발 서버"]).is_some());
        assert!(score("ㄱㅂ", &["개발 서버"]).is_some());
        assert!(score("ㄱㅅ", &["개발 서버"]).is_some());
        assert!(score("ㄴㅂ", &["개발 서버"]).is_none());
        // "문서" as macOS stores it: conjoining jamo.
        let decomposed = "/Users/me/\u{1106}\u{116E}\u{11AB}\u{1109}\u{1165}";
        assert!(score("문서", &[decomposed]).is_some());
        assert!(score("ㅁㅅ", &[decomposed]).is_some());
        assert_eq!(fold("\u{1112}\u{1161}\u{11AB}"), vec!['한']);
    }

    #[test]
    fn long_inputs_stay_bounded() {
        let text = "a".repeat(2048);
        let query = "a".repeat(64);
        assert!(score(&query, &[&text]).is_some());
        assert!(score(&format!("{query}b"), &[&text]).is_none());
    }
}
