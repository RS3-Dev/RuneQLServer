/// Strips HTML-like `<...>` tags from RS3 item names (e.g. `<col=...>`) and lowercases.
pub fn strip_tags_lower(input: &str, replace_with_space: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        if ch == '<' {
            in_tag = true;
            if replace_with_space {
                out.push(' ');
            }
        } else if ch == '>' && in_tag {
            in_tag = false;
        } else if !in_tag {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

/// Matches `ClientIndex.Normalize` in `resources/lib/catalog/client_index.lua`:
/// lowercase, strip `<[^>]*>` -> space, replace non-`[a-z0-9]` with space, collapse & trim.
pub fn normalize_display_name(input: &str) -> String {
    let stripped = strip_tags_lower(input, true);
    let mut out = String::with_capacity(stripped.len());
    let mut last_space = true;
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Matches `LuceneMatch.Normalize` in `resources/lib/text/lucene_match.lua`:
/// lowercase, remove `<[^>]*>`, trim whitespace.
pub fn lucene_normalize(input: &str) -> String {
    strip_tags_lower(input, false).trim().to_string()
}

/// Matches `terms(value)` in `resources/lib/text/lucene_match.lua`:
/// extracts `[a-z0-9]+` tokens; falls back to `[normalized]` if non-empty with no alphanumerics.
pub fn lucene_terms(input: &str) -> Vec<String> {
    let normalized = lucene_normalize(input);
    let mut result = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in normalized.char_indices() {
        if ch.is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            result.push(normalized[s..idx].to_string());
        }
    }
    if let Some(s) = start {
        result.push(normalized[s..].to_string());
    }
    if result.is_empty() && !normalized.is_empty() {
        result.push(normalized);
    }
    result
}

/// Optimal String Alignment (Damerau-Levenshtein) distance with early exit when `> maximum`.
/// Matches `osaDistance` in `resources/lib/text/lucene_match.lua`.
pub fn osa_distance(left: &str, right: &str, maximum: usize) -> usize {
    let a = left.as_bytes();
    let b = right.as_bytes();
    let la = a.len();
    let lb = b.len();
    if la.abs_diff(lb) > maximum {
        return maximum + 1;
    }
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }

    let mut prev_prev = vec![0usize; lb + 1];
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr = vec![0usize; lb + 1];

    for i in 1..=la {
        curr[0] = i;
        let mut row_min = i;
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut val = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                val = val.min(prev_prev[j - 2] + 1);
            }
            curr[j] = val;
            if val < row_min {
                row_min = val;
            }
        }
        if row_min > maximum {
            return maximum + 1;
        }
        std::mem::swap(&mut prev_prev, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[lb]
}

/// Checks if a raw Lucene term contains an unescaped `*` or `?`.
pub fn has_wildcard(raw: &str) -> bool {
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '*' || ch == '?' {
            return true;
        }
    }
    false
}

/// Compiled Lucene wildcard pattern (`*` = any 0+ chars, `?` = any 1 char), anchored to full token.
#[derive(Debug, Clone)]
pub struct WildcardMatcher {
    tokens: Vec<WildcardTok>,
    /// Literal prefix before the first `*` or `?`, enabling binary-search narrowing on sorted dictionaries.
    pub literal_prefix: String,
}

#[derive(Debug, Clone, Copy)]
enum WildcardTok {
    Literal(u8),
    AnyOne,
    AnyMany,
}

impl WildcardMatcher {
    pub fn compile(raw: &str) -> Self {
        let lower = raw.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let mut tokens = Vec::with_capacity(bytes.len());
        let mut literal_prefix = String::new();
        let mut in_prefix = true;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' && i + 1 < bytes.len() {
                i += 1;
                let lit = bytes[i];
                tokens.push(WildcardTok::Literal(lit));
                if in_prefix {
                    literal_prefix.push(lit as char);
                }
            } else if b == b'*' {
                in_prefix = false;
                if !matches!(tokens.last(), Some(WildcardTok::AnyMany)) {
                    tokens.push(WildcardTok::AnyMany);
                }
            } else if b == b'?' {
                in_prefix = false;
                tokens.push(WildcardTok::AnyOne);
            } else {
                tokens.push(WildcardTok::Literal(b));
                if in_prefix {
                    literal_prefix.push(b as char);
                }
            }
            i += 1;
        }
        Self {
            tokens,
            literal_prefix,
        }
    }

    /// Linear-time two-pointer wildcard matcher (zero exponential backtracking!).
    pub fn matches(&self, candidate: &str) -> bool {
        let s = candidate.as_bytes();
        let p = &self.tokens;
        let (mut si, mut pi) = (0usize, 0usize);
        let mut star_pi: Option<usize> = None;
        let mut star_si: usize = 0;

        while si < s.len() {
            if pi < p.len() {
                match p[pi] {
                    WildcardTok::Literal(c) if c == s[si] => {
                        si += 1;
                        pi += 1;
                        continue;
                    }
                    WildcardTok::AnyOne => {
                        si += 1;
                        pi += 1;
                        continue;
                    }
                    WildcardTok::AnyMany => {
                        star_pi = Some(pi);
                        star_si = si;
                        pi += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            if let Some(sp) = star_pi {
                star_si += 1;
                si = star_si;
                pi = sp + 1;
            } else {
                return false;
            }
        }
        while pi < p.len() && matches!(p[pi], WildcardTok::AnyMany) {
            pi += 1;
        }
        pi == p.len()
    }
}

/// Exact phrase match against an item's analyzed tokens.
pub fn match_phrase(wanted: &[String], available: &[String]) -> Option<f64> {
    if wanted.is_empty() || available.len() < wanted.len() {
        return None;
    }
    for start in 0..=(available.len() - wanted.len()) {
        if available[start..start + wanted.len()] == *wanted {
            return Some(120.0 + wanted.len() as f64);
        }
    }
    None
}

/// Proximity phrase match matching `phraseSlop` in `resources/lib/text/lucene_match.lua`.
pub fn match_proximity(wanted: &[String], available: &[String], max_slop: usize) -> Option<f64> {
    if wanted.is_empty() {
        return None;
    }
    let mut candidates: Vec<Vec<usize>> = Vec::with_capacity(wanted.len());
    for word in wanted {
        let positions: Vec<usize> = available
            .iter()
            .enumerate()
            .filter_map(|(idx, cand)| if cand == word { Some(idx + 1) } else { None })
            .collect();
        if positions.is_empty() {
            return None;
        }
        candidates.push(positions);
    }

    let mut chosen = vec![0usize; wanted.len()];
    let mut used = vec![false; available.len() + 1];
    let mut best: Option<usize> = None;
    let mut explored = 0usize;

    fn visit(
        index: usize,
        candidates: &[Vec<usize>],
        chosen: &mut [usize],
        used: &mut [bool],
        best: &mut Option<usize>,
        explored: &mut usize,
    ) {
        if *best == Some(0) || *explored > 20_000 {
            return;
        }
        if index == candidates.len() {
            *explored += 1;
            let mut offsets: Vec<isize> = chosen
                .iter()
                .enumerate()
                .map(|(wi, &pos)| pos as isize - (wi as isize + 1))
                .collect();
            offsets.sort_unstable();
            let origin = offsets[(offsets.len() - 1) / 2];
            let cost: usize = offsets.iter().map(|&o| (o - origin).unsigned_abs()).sum();
            if best.is_none_or(|b| cost < b) {
                *best = Some(cost);
            }
            return;
        }
        for &pos in &candidates[index] {
            if !used[pos] {
                used[pos] = true;
                chosen[index] = pos;
                visit(index + 1, candidates, chosen, used, best, explored);
                used[pos] = false;
            }
        }
    }

    visit(0, &candidates, &mut chosen, &mut used, &mut best, &mut explored);
    let slop = best?;
    if slop > max_slop {
        return None;
    }
    Some(115.0 + wanted.len() as f64 - slop as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalization_matches_lua() {
        assert_eq!(
            normalize_display_name("Adamant bolts (p++)"),
            "adamant bolts p"
        );
        assert_eq!(
            normalize_display_name("<col=ff0000>Rune platebody + 5</col>"),
            "rune platebody 5"
        );
        assert_eq!(
            lucene_terms("Akrisae's robe top (worn)"),
            vec!["akrisae", "s", "robe", "top", "worn"]
        );
    }

    #[test]
    fn test_osa_and_wildcard() {
        assert_eq!(osa_distance("shark", "shrak", 2), 1);
        assert_eq!(osa_distance("potion", "potoin", 2), 1);
        let w = WildcardMatcher::compile("plate*y");
        assert_eq!(w.literal_prefix, "plate");
        assert!(w.matches("platebody"));
        assert!(!w.matches("platelegs"));
    }
}
