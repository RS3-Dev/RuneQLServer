use std::collections::{HashMap, HashSet};

use crate::config::QueryLimits;
use crate::engine::QueryError;

/// Bounded, ReDoS-proof Lucene classic-10.3 automaton regex AST and matcher.
/// Supports `|`, `&` (intersection), `~` (complement), `@` (any string), `#` (empty),
/// `<lo-hi>` numeric intervals, `[a-z]`/`[^...]`, `"quoted"`, `?`, `*`, `+`, `{m,n}`.
#[derive(Debug, Clone)]
pub struct LuceneRegex {
    root: RegexNode,
    max_steps: usize,
}

#[derive(Debug, Clone)]
enum RegexNode {
    EmptyString,
    EmptyLanguage,
    Literal(u8),
    AnyChar,
    AnyString,
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
    Interval {
        lower: u64,
        upper: u64,
        digits: Option<usize>,
    },
    Concat(Vec<RegexNode>),
    Union(Vec<RegexNode>),
    Intersection(Box<RegexNode>, Box<RegexNode>),
    Complement(Box<RegexNode>),
    Repeat {
        child: Box<RegexNode>,
        min: usize,
        max: Option<usize>,
    },
}

struct Parser<'a> {
    pattern: &'a [u8],
    index: usize,
    max_depth: usize,
    max_repeat: usize,
    node_count: usize,
}

impl<'a> Parser<'a> {
    fn fail(&self, msg: &str) -> QueryError {
        QueryError::InvalidSyntax(format!(
            "{msg} at regular-expression character {}.",
            self.index + 1
        ))
    }

    fn current(&self) -> Option<u8> {
        self.pattern.get(self.index).copied()
    }

    fn consume(&mut self, expected: Option<u8>) -> bool {
        if let Some(ch) = expected {
            if self.current() != Some(ch) {
                return false;
            }
        }
        if self.index < self.pattern.len() {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn count_node(&mut self) -> Result<(), QueryError> {
        self.node_count += 1;
        if self.node_count > 512 {
            return Err(QueryError::LimitExceeded(
                "Regular expression AST exceeds maximum complexity".to_string(),
            ));
        }
        Ok(())
    }

    fn parse_class(&mut self) -> Result<RegexNode, QueryError> {
        self.consume(Some(b'['));
        let negated = self.consume(Some(b'^'));
        let mut ranges = Vec::new();
        let mut first = true;
        while self.index < self.pattern.len() {
            if self.current() == Some(b']') && !first {
                self.consume(Some(b']'));
                self.count_node()?;
                return Ok(RegexNode::Class { negated, ranges });
            }
            first = false;
            let mut left = self.current().unwrap();
            if left == b'\\' {
                self.consume(None);
                left = self
                    .current()
                    .ok_or_else(|| self.fail("RuneQL found an incomplete regex class escape"))?;
            }
            self.consume(None);
            let mut right = left;
            if self.current() == Some(b'-')
                && self.pattern.get(self.index + 1).copied() != Some(b']')
            {
                self.consume(None);
                right = self.current().ok_or_else(|| {
                    self.fail("RuneQL found an incomplete regex character range")
                })?;
                if right == b'\\' {
                    self.consume(None);
                    right = self.current().ok_or_else(|| {
                        self.fail("RuneQL found an incomplete regex character range")
                    })?;
                }
                self.consume(None);
            }
            if left > right {
                return Err(self.fail("RuneQL found a reversed regex character range"));
            }
            ranges.push((left, right));
        }
        Err(self.fail("RuneQL found an unterminated regex character class"))
    }

    fn parse_quoted(&mut self) -> Result<RegexNode, QueryError> {
        self.consume(Some(b'"'));
        let mut nodes = Vec::new();
        while self.index < self.pattern.len() {
            let mut ch = self.current().unwrap();
            if ch == b'"' {
                self.consume(None);
                return Ok(sequence(nodes));
            }
            if ch == b'\\' {
                self.consume(None);
                ch = self
                    .current()
                    .ok_or_else(|| self.fail("RuneQL found an incomplete quoted regex escape"))?;
            }
            self.count_node()?;
            nodes.push(RegexNode::Literal(ch));
            self.consume(None);
        }
        Err(self.fail("RuneQL found an unterminated quoted regex string"))
    }

    fn parse_interval(&mut self) -> Result<RegexNode, QueryError> {
        let rest = &self.pattern[self.index..];
        let close = rest
            .iter()
            .position(|&b| b == b'>')
            .ok_or_else(|| self.fail("RuneQL regex named automata are unavailable without a provider"))?;
        let inner = std::str::from_utf8(&rest[1..close]).unwrap_or("");
        let (lo_s, hi_s) = inner
            .split_once('-')
            .ok_or_else(|| self.fail("RuneQL regex named automata are unavailable without a provider"))?;
        if lo_s.is_empty()
            || hi_s.is_empty()
            || !lo_s.bytes().all(|b| b.is_ascii_digit())
            || !hi_s.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(self.fail("RuneQL regex named automata are unavailable without a provider"));
        }
        let lower: u64 = lo_s
            .parse()
            .map_err(|_| self.fail("Invalid regex numeric interval"))?;
        let upper: u64 = hi_s
            .parse()
            .map_err(|_| self.fail("Invalid regex numeric interval"))?;
        if lower > upper {
            return Err(self.fail("RuneQL found a reversed regex numeric interval"));
        }
        self.index += close + 1;
        self.count_node()?;
        Ok(RegexNode::Interval {
            lower,
            upper,
            digits: if lo_s.len() == hi_s.len() {
                Some(lo_s.len())
            } else {
                None
            },
        })
    }

    fn parse_atom(&mut self, depth: usize) -> Result<RegexNode, QueryError> {
        if depth > self.max_depth {
            return Err(QueryError::LimitExceeded(format!(
                "RuneQL regex nesting exceeds limit of {}",
                self.max_depth
            )));
        }
        let ch = self
            .current()
            .ok_or_else(|| self.fail("RuneQL expected a regex expression"))?;
        match ch {
            b'(' => {
                self.consume(None);
                let node = self.parse_union(depth + 1)?;
                if !self.consume(Some(b')')) {
                    return Err(self.fail("RuneQL found an unterminated regex group"));
                }
                Ok(node)
            }
            b'[' => self.parse_class(),
            b'"' => self.parse_quoted(),
            b'<' => self.parse_interval(),
            b'.' => {
                self.consume(None);
                self.count_node()?;
                Ok(RegexNode::AnyChar)
            }
            b'@' => {
                self.consume(None);
                self.count_node()?;
                Ok(RegexNode::AnyString)
            }
            b'#' => {
                self.consume(None);
                self.count_node()?;
                Ok(RegexNode::EmptyLanguage)
            }
            b'~' => {
                self.consume(None);
                let child = self.parse_atom(depth + 1)?;
                self.count_node()?;
                Ok(RegexNode::Complement(Box::new(child)))
            }
            b'\\' => {
                self.consume(None);
                let esc = self
                    .current()
                    .ok_or_else(|| self.fail("RuneQL found an incomplete regex escape"))?;
                self.consume(None);
                self.count_node()?;
                Ok(RegexNode::Literal(esc))
            }
            b')' | b'|' | b'&' | b'?' | b'*' | b'+' | b'{' | b'}' => {
                Err(self.fail("RuneQL found an unexpected regex operator"))
            }
            other => {
                self.consume(None);
                self.count_node()?;
                Ok(RegexNode::Literal(other))
            }
        }
    }

    fn parse_repeat(&mut self, depth: usize) -> Result<RegexNode, QueryError> {
        let mut node = self.parse_atom(depth)?;
        while let Some(ch) = self.current() {
            let (min, max) = match ch {
                b'?' => {
                    self.consume(None);
                    (0, Some(1))
                }
                b'*' => {
                    self.consume(None);
                    (0, None)
                }
                b'+' => {
                    self.consume(None);
                    (1, None)
                }
                b'{' => {
                    let rest = &self.pattern[self.index..];
                    let close = rest
                        .iter()
                        .position(|&b| b == b'}')
                        .ok_or_else(|| self.fail("RuneQL found an invalid regex repetition"))?;
                    let inner = std::str::from_utf8(&rest[1..close]).unwrap_or("");
                    let (min, max) = if let Some((a, b)) = inner.split_once(',') {
                        if a.is_empty()
                            || !a.bytes().all(|c| c.is_ascii_digit())
                            || (!b.is_empty() && !b.bytes().all(|c| c.is_ascii_digit()))
                        {
                            return Err(self.fail("RuneQL found an invalid regex repetition"));
                        }
                        let lo: usize = a
                            .parse()
                            .map_err(|_| self.fail("RuneQL found an invalid regex repetition"))?;
                        let hi = if b.is_empty() {
                            None
                        } else {
                            Some(
                                b.parse()
                                    .map_err(|_| self.fail("RuneQL found an invalid regex repetition"))?,
                            )
                        };
                        (lo, hi)
                    } else {
                        if inner.is_empty() || !inner.bytes().all(|c| c.is_ascii_digit()) {
                            return Err(self.fail("RuneQL found an invalid regex repetition"));
                        }
                        let lo: usize = inner
                            .parse()
                            .map_err(|_| self.fail("RuneQL found an invalid regex repetition"))?;
                        (lo, Some(lo))
                    };
                    if min > self.max_repeat || max.is_some_and(|m| m > self.max_repeat) {
                        return Err(QueryError::LimitExceeded(format!(
                            "RuneQL regex repetitions cannot exceed {}",
                            self.max_repeat
                        )));
                    }
                    if max.is_some_and(|m| min > m) {
                        return Err(self.fail("RuneQL found a reversed regex repetition range"));
                    }
                    self.index += close + 1;
                    (min, max)
                }
                _ => break,
            };
            self.count_node()?;
            node = RegexNode::Repeat {
                child: Box::new(node),
                min,
                max,
            };
        }
        Ok(node)
    }

    fn parse_concat(&mut self, depth: usize) -> Result<RegexNode, QueryError> {
        let mut nodes = Vec::new();
        while let Some(ch) = self.current() {
            if matches!(ch, b')' | b'|' | b'&') {
                break;
            }
            nodes.push(self.parse_repeat(depth)?);
        }
        Ok(sequence(nodes))
    }

    fn parse_intersection(&mut self, depth: usize) -> Result<RegexNode, QueryError> {
        let mut node = self.parse_concat(depth)?;
        while self.consume(Some(b'&')) {
            let right = self.parse_concat(depth)?;
            self.count_node()?;
            node = RegexNode::Intersection(Box::new(node), Box::new(right));
        }
        Ok(node)
    }

    fn parse_union(&mut self, depth: usize) -> Result<RegexNode, QueryError> {
        let mut nodes = vec![self.parse_intersection(depth)?];
        while self.consume(Some(b'|')) {
            nodes.push(self.parse_intersection(depth)?);
        }
        if nodes.len() == 1 {
            Ok(nodes.pop().unwrap())
        } else {
            self.count_node()?;
            Ok(RegexNode::Union(nodes))
        }
    }
}

fn sequence(mut nodes: Vec<RegexNode>) -> RegexNode {
    match nodes.len() {
        0 => RegexNode::EmptyString,
        1 => nodes.pop().unwrap(),
        _ => RegexNode::Concat(nodes),
    }
}

impl LuceneRegex {
    pub fn compile(pattern: &str, limits: &QueryLimits) -> Result<Self, QueryError> {
        let lower = pattern.to_ascii_lowercase();
        if lower.is_empty() {
            return Err(QueryError::InvalidSyntax(
                "RuneQL regular expressions cannot be empty.".to_string(),
            ));
        }
        if lower.len() > limits.max_regex_length {
            return Err(QueryError::LimitExceeded(format!(
                "Regular expression exceeds remote limit of {} bytes",
                limits.max_regex_length
            )));
        }
        let mut parser = Parser {
            pattern: lower.as_bytes(),
            index: 0,
            max_depth: limits.max_regex_depth,
            max_repeat: limits.max_regex_repeat,
            node_count: 0,
        };
        let root = parser.parse_union(0)?;
        if parser.index < parser.pattern.len() {
            return Err(parser.fail("RuneQL could not parse the remainder of this regex"));
        }
        Ok(Self {
            root,
            max_steps: limits.max_regex_steps,
        })
    }

    /// Evaluates whether the regex matches the entire token, with a strict step cap.
    pub fn matches(&self, token: &str) -> Result<bool, QueryError> {
        let bytes = token.as_bytes();
        let mut memo: HashMap<(usize, usize), HashSet<usize>> = HashMap::new();
        let mut steps = self.max_steps;
        let endings = match_node(&self.root, bytes, 0, &mut memo, &mut steps)?;
        Ok(endings.contains(&bytes.len()))
    }
}

fn match_node(
    node: &RegexNode,
    text: &[u8],
    start: usize,
    memo: &mut HashMap<(usize, usize), HashSet<usize>>,
    steps: &mut usize,
) -> Result<HashSet<usize>, QueryError> {
    if *steps == 0 {
        return Err(QueryError::BudgetExceeded(
            "Regular expression exceeded maximum step budget".to_string(),
        ));
    }
    *steps -= 1;

    let key = (node as *const RegexNode as usize, start);
    if let Some(cached) = memo.get(&key) {
        return Ok(cached.clone());
    }
    // Insert empty set to break any zero-width recursion cycles safely.
    memo.insert(key, HashSet::new());

    let mut result = HashSet::new();
    match node {
        RegexNode::EmptyString => {
            result.insert(start);
        }
        RegexNode::EmptyLanguage => {}
        RegexNode::Literal(ch) => {
            if text.get(start) == Some(ch) {
                result.insert(start + 1);
            }
        }
        RegexNode::AnyChar => {
            if start < text.len() {
                result.insert(start + 1);
            }
        }
        RegexNode::AnyString => {
            for pos in start..=text.len() {
                result.insert(pos);
            }
        }
        RegexNode::Class { negated, ranges } => {
            if let Some(&b) = text.get(start) {
                let mut matched = ranges.iter().any(|&(lo, hi)| b >= lo && b <= hi);
                if *negated {
                    matched = !matched;
                }
                if matched {
                    result.insert(start + 1);
                }
            }
        }
        RegexNode::Interval {
            lower,
            upper,
            digits,
        } => {
            for finish in start..text.len() {
                let slice = &text[start..=finish];
                if !slice.iter().all(|b| b.is_ascii_digit()) {
                    break;
                }
                if digits.is_none_or(|d| slice.len() == d) {
                    if let Ok(val) = std::str::from_utf8(slice).unwrap_or("").parse::<u64>() {
                        if val >= *lower && val <= *upper {
                            result.insert(finish + 1);
                        }
                    }
                }
            }
        }
        RegexNode::Union(children) => {
            for child in children {
                result.extend(match_node(child, text, start, memo, steps)?);
            }
        }
        RegexNode::Intersection(left, right) => {
            let l = match_node(left, text, start, memo, steps)?;
            let r = match_node(right, text, start, memo, steps)?;
            for pos in l {
                if r.contains(&pos) {
                    result.insert(pos);
                }
            }
        }
        RegexNode::Complement(child) => {
            let excluded = match_node(child, text, start, memo, steps)?;
            for pos in start..=text.len() {
                if !excluded.contains(&pos) {
                    result.insert(pos);
                }
            }
        }
        RegexNode::Concat(children) => {
            let mut positions = HashSet::from([start]);
            for child in children {
                let mut next = HashSet::new();
                for pos in positions {
                    next.extend(match_node(child, text, pos, memo, steps)?);
                }
                positions = next;
                if positions.is_empty() {
                    break;
                }
            }
            result = positions;
        }
        RegexNode::Repeat { child, min, max } => {
            let mut positions = HashSet::from([start]);
            let limit = max.unwrap_or_else(|| text.len().saturating_sub(start) + 2 + *min);
            for count in 0..=limit {
                if count >= *min {
                    result.extend(positions.iter().copied());
                }
                if count == limit || positions.is_empty() {
                    break;
                }
                let mut next = HashSet::new();
                for &pos in &positions {
                    next.extend(match_node(child, text, pos, memo, steps)?);
                }
                let changed = next.iter().any(|p| !positions.contains(p));
                positions = next;
                if !changed && max.is_none() {
                    break;
                }
            }
        }
    }

    memo.insert(key, result.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lucene_regex_features() {
        let lim = QueryLimits::default();
        let re = LuceneRegex::compile("rune&.*e", &lim).unwrap();
        assert!(re.matches("rune").unwrap());
        assert!(!re.matches("runes").unwrap());

        let interval = LuceneRegex::compile("t<70-90>", &lim).unwrap();
        assert!(interval.matches("t80").unwrap());
        assert!(!interval.matches("t95").unwrap());
    }
}
