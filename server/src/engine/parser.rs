use crate::config::QueryLimits;
use crate::engine::lexer::{Token, TokenKind, tokenize};
use crate::engine::normalize::{WildcardMatcher, has_wildcard, lucene_normalize, lucene_terms};
use crate::engine::regex::LuceneRegex;
use crate::engine::{QueryError, SortDirection, SortRule};

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub filter: QueryNode,
    pub sort: Vec<SortRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occur {
    Must,
    Should,
    MustNot,
}

#[derive(Debug, Clone)]
pub struct BooleanClause {
    pub occur: Occur,
    pub query: QueryNode,
}

#[derive(Debug, Clone)]
pub struct QueryNode {
    pub kind: QueryKind,
    pub boost: f64,
}

#[derive(Debug, Clone)]
pub enum QueryKind {
    MatchAll,
    Boolean(Vec<BooleanClause>),
    Term {
        field: String,
        normalized: String,
        query_terms: Vec<String>,
        fuzzy_edits: Option<usize>,
        wildcard: Option<WildcardMatcher>,
        raw: String,
    },
    Phrase {
        field: String,
        terms: Vec<String>,
        proximity: Option<usize>,
        raw: String,
    },
    Range {
        field: String,
        lower: Option<String>,
        upper: Option<String>,
        include_lower: bool,
        include_upper: bool,
    },
    Regex {
        field: String,
        pattern: String,
        compiled: LuceneRegex,
    },
}

const ID_FIELDS: &[&str] = &["id", "itemid", "objectid", "containsid"];
const MAX_SORT_KEYS: usize = 4;

const NON_SORTABLE_FIELDS: &[&str] = &[
    "types",
    "skills",
    "roles",
    "combatstyle",
    "armourrole",
    "family",
    "form",
    "set",
    "subtype",
    "weapontype",
    "reclaimsources",
    "storagelocations",
];

fn canonical_sort_field(raw: &str) -> Result<(String, SortDirection), QueryError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(QueryError::InvalidSyntax(
            "RuneQL sort requires a field name.".to_string(),
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    if ID_FIELDS.contains(&lower.as_str()) {
        return Err(QueryError::InvalidSyntax(
            "RuneQL item IDs are unsupported; search by display name and persist config names."
                .to_string(),
        ));
    }
    if lower == "equiprequirements" {
        return Err(QueryError::InvalidSyntax(
            "RuneQL cannot sort by 'equipRequirements'; sort by 'tier' or a numeric equip level such as 'equipDefence'."
                .to_string(),
        ));
    }
    if NON_SORTABLE_FIELDS.contains(&lower.as_str()) {
        return Err(QueryError::InvalidSyntax(format!(
            "RuneQL cannot sort by multi-value field '{trimmed}'."
        )));
    }
    let canonical = match lower.as_str() {
        "score" | "relevance" => "score",
        "name" | "displayname" => "name",
        "configname" => "configName",
        "qty" | "quantity" | "count" => "qty",
        "geprice" | "price" | "ge" => "gePrice",
        "stackvalue" | "totalvalue" | "totalgevalue" => "stackValue",
        "basevalue" => "baseValue",
        "highalch" | "alch" => "highAlch",
        "buylimit" => "buyLimit",
        "healamount" | "heal" => "healAmount",
        "tier" => "tier",
        "dose" => "dose",
        "category" => "category",
        "containertype" => "containerType",
        "slot" => "slot",
        "augmentable" => "augmentable",
        "augmented" => "augmented",
        "degradable" => "degradable",
        "getradeable" => "geTradeable",
        "tradeable" => "tradeable",
        "members" => "members",
        "reclaimable" => "reclaimable",
        "equipagility" => "equipAgility",
        "equipattack" => "equipAttack",
        "equipconstitution" => "equipConstitution",
        "equipdefence" => "equipDefence",
        "equipdungeoneering" => "equipDungeoneering",
        "equipmagic" => "equipMagic",
        "equipnecromancy" => "equipNecromancy",
        "equipprayer" => "equipPrayer",
        "equipranged" => "equipRanged",
        "equipsmithing" => "equipSmithing",
        "equipstrength" => "equipStrength",
        _ => trimmed,
    };
    let default_dir = match canonical {
        "name" | "configName" | "category" | "containerType" | "slot" => SortDirection::Asc,
        _ => SortDirection::Desc,
    };
    Ok((canonical.to_string(), default_dir))
}

fn parse_sort_direction(raw: &str, pos: usize) -> Result<SortDirection, QueryError> {
    match raw.to_ascii_lowercase().as_str() {
        "asc" | "ascending" => Ok(SortDirection::Asc),
        "desc" | "descending" => Ok(SortDirection::Desc),
        _ => Err(QueryError::InvalidSyntax(format!(
            "RuneQL sort direction must be ascending, descending, asc, or desc at character {pos}."
        ))),
    }
}

struct Parser<'a> {
    tokens: Vec<Token>,
    index: usize,
    clause_count: usize,
    expensive_count: usize,
    limits: &'a QueryLimits,
}

impl<'a> Parser<'a> {
    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn peek(&self, offset: usize) -> &Token {
        self.tokens
            .get(self.index + offset)
            .unwrap_or_else(|| self.tokens.last().unwrap())
    }

    fn consume(&mut self, expected: Option<TokenKind>) -> Option<Token> {
        if let Some(ref kind) = expected {
            if &self.current().kind != kind {
                return None;
            }
        }
        let tok = self.tokens[self.index].clone();
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
        Some(tok)
    }

    fn fail(&self, msg: &str, tok: Option<&Token>) -> QueryError {
        let t = tok.unwrap_or_else(|| self.current());
        QueryError::InvalidSyntax(format!("{msg} at character {}.", t.position))
    }

    fn bump_expensive(&mut self) -> Result<(), QueryError> {
        self.expensive_count += 1;
        if self.expensive_count > self.limits.max_expensive_clauses {
            return Err(QueryError::LimitExceeded(format!(
                "Query exceeds remote limit of {} fuzzy/wildcard/regex clauses",
                self.limits.max_expensive_clauses
            )));
        }
        Ok(())
    }

    fn apply_boost(&mut self, mut node: QueryNode) -> Result<QueryNode, QueryError> {
        if self.current().kind != TokenKind::Boost {
            return Ok(node);
        }
        let caret = self.consume(Some(TokenKind::Boost)).unwrap();
        let factor = self.current().clone();
        let valid_num = factor.kind == TokenKind::Term
            && factor.position == caret.position + 1
            && is_positive_decimal(&factor.value);
        if !valid_num {
            return Err(self.fail(
                "Lucene boosts require a positive numeric factor immediately after ^",
                Some(&factor),
            ));
        }
        let value: f64 = factor.value.parse().unwrap_or(0.0);
        if !value.is_finite() || value <= 0.0 {
            return Err(self.fail(
                "Lucene boost factors must be greater than zero",
                Some(&factor),
            ));
        }
        self.consume(None);
        node.boost = value;
        Ok(node)
    }

    fn parse_range_boundary(&mut self) -> Result<Option<String>, QueryError> {
        let cur = self.current().clone();
        if cur.kind != TokenKind::Term && cur.kind != TokenKind::Phrase {
            return Err(self.fail("RuneQL expected a range boundary", Some(&cur)));
        }
        self.consume(None);
        if cur.kind == TokenKind::Term && cur.value == "*" {
            return Ok(None);
        }
        Ok(Some(cur.value))
    }

    fn parse_range(&mut self, field: String) -> Result<QueryNode, QueryError> {
        let opening = self.consume(None).unwrap();
        let lower = self.parse_range_boundary()?;
        if self.consume(Some(TokenKind::To)).is_none() {
            return Err(self.fail("RuneQL ranges require an uppercase TO", None));
        }
        let upper = self.parse_range_boundary()?;
        let closing = self.current().clone();
        if closing.kind != TokenKind::RangeEndInclusive
            && closing.kind != TokenKind::RangeEndExclusive
        {
            return Err(self.fail("RuneQL found an unterminated range", Some(&closing)));
        }
        self.consume(None);
        Ok(QueryNode {
            kind: QueryKind::Range {
                field,
                lower,
                upper,
                include_lower: opening.kind == TokenKind::RangeInclusive,
                include_upper: closing.kind == TokenKind::RangeEndInclusive,
            },
            boost: 1.0,
        })
    }

    fn parse_primary(
        &mut self,
        inherited_field: Option<&str>,
        depth: usize,
    ) -> Result<QueryNode, QueryError> {
        if depth > self.limits.max_nesting_depth {
            return Err(QueryError::LimitExceeded(format!(
                "Query nesting exceeds remote limit of {}",
                self.limits.max_nesting_depth
            )));
        }

        let mut field = inherited_field.map(|s| s.to_string());
        if self.current().kind == TokenKind::Term && self.peek(1).kind == TokenKind::Colon {
            let f = self.consume(None).unwrap();
            self.consume(Some(TokenKind::Colon));
            field = Some(f.value.to_ascii_lowercase());
        }

        if let Some(ref f) = field {
            if ID_FIELDS.contains(&f.as_str()) {
                return Err(QueryError::InvalidSyntax(
                    "RuneQL item IDs are unsupported; search by display name and persist config names."
                        .to_string(),
                ));
            }
        }

        let resolved_field = field.clone().unwrap_or_else(|| "_all".to_string());

        match self.current().kind {
            TokenKind::LParen => {
                self.consume(None);
                let grouped =
                    self.parse_query(field.as_deref(), Some(TokenKind::RParen), depth + 1, None)?;
                if self.consume(Some(TokenKind::RParen)).is_none() {
                    return Err(self.fail("RuneQL found an unterminated group", None));
                }
                self.apply_boost(grouped)
            }
            TokenKind::RangeInclusive | TokenKind::RangeExclusive => {
                let range = self.parse_range(resolved_field)?;
                self.apply_boost(range)
            }
            TokenKind::Regex => {
                let tok = self.consume(None).unwrap();
                self.bump_expensive()?;
                let compiled = LuceneRegex::compile(&tok.value, self.limits)?;
                self.apply_boost(QueryNode {
                    kind: QueryKind::Regex {
                        field: resolved_field,
                        pattern: tok.value,
                        compiled,
                    },
                    boost: 1.0,
                })
            }
            TokenKind::Phrase => {
                let tok = self.consume(None).unwrap();
                let mut proximity = None;
                if self.current().kind == TokenKind::Tilde {
                    let tilde = self.consume(Some(TokenKind::Tilde)).unwrap();
                    let dist = self.current().clone();
                    if dist.kind != TokenKind::Term
                        || dist.position != tilde.position + 1
                        || !dist.value.bytes().all(|b| b.is_ascii_digit())
                        || dist.value.is_empty()
                    {
                        return Err(self.fail(
                            "Lucene phrase proximity requires a non-negative integer",
                            Some(&dist),
                        ));
                    }
                    let d: usize = dist.value.parse().unwrap_or(usize::MAX);
                    if d > 64 {
                        return Err(QueryError::LimitExceeded(
                            "Phrase proximity slop cannot exceed 64".to_string(),
                        ));
                    }
                    proximity = Some(d);
                    self.consume(None);
                }
                let terms = lucene_terms(&tok.value);
                self.apply_boost(QueryNode {
                    kind: QueryKind::Phrase {
                        field: resolved_field,
                        terms,
                        proximity,
                        raw: tok.raw,
                    },
                    boost: 1.0,
                })
            }
            TokenKind::Term => {
                let tok = self.consume(None).unwrap();
                if field.as_deref() == Some("*") && tok.value == "*" {
                    return self.apply_boost(QueryNode {
                        kind: QueryKind::MatchAll,
                        boost: 1.0,
                    });
                }
                if (tok.raw.starts_with('*') || tok.raw.starts_with('?')) && tok.raw != "*" {
                    return Err(self.fail("Lucene terms cannot begin with a wildcard", Some(&tok)));
                }

                let mut fuzzy_edits = None;
                let wildcard_flag = has_wildcard(&tok.raw);
                if self.current().kind == TokenKind::Tilde {
                    if wildcard_flag {
                        return Err(
                            self.fail("Lucene fuzzy terms cannot contain wildcards", Some(&tok))
                        );
                    }
                    let tilde = self.consume(None).unwrap();
                    let mut edits = 2usize;
                    let dist = self.current().clone();
                    if dist.kind == TokenKind::Term && dist.position == tilde.position + 1 {
                        if !dist.value.is_empty() && dist.value.bytes().all(|b| b.is_ascii_digit()) {
                            edits = dist.value.parse().unwrap_or(99);
                            self.consume(None);
                        } else {
                            return Err(self.fail(
                                "Lucene fuzzy edit distance must be an integer between 0 and 2",
                                Some(&dist),
                            ));
                        }
                    }
                    if edits > 2 {
                        return Err(self.fail(
                            "Lucene fuzzy edit distance must be between 0 and 2",
                            Some(&dist),
                        ));
                    }
                    self.bump_expensive()?;
                    fuzzy_edits = Some(edits);
                }

                let wildcard = if wildcard_flag {
                    self.bump_expensive()?;
                    Some(WildcardMatcher::compile(&tok.raw))
                } else {
                    None
                };

                let normalized = lucene_normalize(&tok.value);
                let query_terms = lucene_terms(&tok.value);
                self.apply_boost(QueryNode {
                    kind: QueryKind::Term {
                        field: resolved_field,
                        normalized,
                        query_terms,
                        fuzzy_edits,
                        wildcard,
                        raw: tok.raw,
                    },
                    boost: 1.0,
                })
            }
            _ => Err(self.fail("RuneQL expected a term, phrase, range, or group", None)),
        }
    }

    fn is_sort_start(&self) -> bool {
        self.current().kind == TokenKind::Term
            && self.current().value.eq_ignore_ascii_case("sort")
            && self.peek(1).kind == TokenKind::Colon
    }

    fn parse_sort_directive(&mut self, sort_rules: &mut Vec<SortRule>) -> Result<(), QueryError> {
        let sort_tok = self.consume(Some(TokenKind::Term)).unwrap();
        self.consume(Some(TokenKind::Colon));
        if self.current().kind != TokenKind::Term {
            return Err(self.fail("RuneQL sort requires a field name after sort:", Some(&sort_tok)));
        }

        loop {
            let field_tok = self.current().clone();
            if field_tok.kind != TokenKind::Term {
                return Err(self.fail("RuneQL sort expected a field name", Some(&field_tok)));
            }
            self.consume(None);

            let mut raw_field = field_tok.value.clone();
            if raw_field.contains(',') {
                let parts: Vec<&str> = raw_field.split(',').collect();
                for &part in &parts[..parts.len() - 1] {
                    let (field, direction) = canonical_sort_field(part)?;
                    sort_rules.push(SortRule { field, direction });
                }
                let last = *parts.last().unwrap();
                if last.is_empty() {
                    return Err(self.fail("RuneQL sort expected a field name after ','", Some(&field_tok)));
                }
                raw_field = last.to_string();
            }

            let mut explicit_dir: Option<SortDirection> = None;
            let mut trailing_field: Option<String> = None;
            if self.current().kind == TokenKind::Colon {
                self.consume(Some(TokenKind::Colon));
                let dir_tok = self.current().clone();
                if dir_tok.kind != TokenKind::Term {
                    return Err(self.fail(
                        "RuneQL sort expected ascending, descending, asc, or desc after ':'",
                        Some(&dir_tok),
                    ));
                }
                self.consume(None);
                if let Some((dir_part, rest)) = dir_tok.value.split_once(',') {
                    explicit_dir = Some(parse_sort_direction(dir_part, dir_tok.position)?);
                    trailing_field = Some(rest.to_string());
                } else {
                    explicit_dir = Some(parse_sort_direction(&dir_tok.value, dir_tok.position)?);
                }
            }

            let (field, default_dir) = canonical_sort_field(&raw_field)?;
            sort_rules.push(SortRule {
                field,
                direction: explicit_dir.unwrap_or(default_dir),
            });
            if sort_rules.len() > MAX_SORT_KEYS {
                return Err(QueryError::LimitExceeded(format!(
                    "RuneQL queries can specify at most {MAX_SORT_KEYS} sort keys"
                )));
            }

            if let Some(next_field) = trailing_field {
                if next_field.is_empty() {
                    return Err(self.fail("RuneQL sort expected a field name after ','", None));
                }
                self.tokens.insert(
                    self.index,
                    Token {
                        kind: TokenKind::Term,
                        raw: next_field.clone(),
                        value: next_field,
                        position: field_tok.position,
                    },
                );
            } else {
                break;
            }
        }
        Ok(())
    }

    fn parse_query(
        &mut self,
        inherited_field: Option<&str>,
        stop_kind: Option<TokenKind>,
        depth: usize,
        mut sort_rules: Option<&mut Vec<SortRule>>,
    ) -> Result<QueryNode, QueryError> {
        let mut clauses: Vec<BooleanClause> = Vec::new();
        let mut expecting_clause = true;
        let mut conjunction: Option<TokenKind> = None;
        let mut just_parsed_sort = false;
        let allow_sort = sort_rules.is_some() && inherited_field.is_none() && stop_kind.is_none();

        while self.current().kind != TokenKind::Eof
            && stop_kind.as_ref() != Some(&self.current().kind)
        {
            if self.current().kind == TokenKind::RParen && stop_kind != Some(TokenKind::RParen) {
                return Err(self.fail("RuneQL found an unexpected closing parenthesis", None));
            }

            if self.is_sort_start() {
                if !allow_sort {
                    return Err(self.fail(
                        "RuneQL sort directives are only allowed at the top level of a query",
                        None,
                    ));
                }
                if conjunction.is_some() {
                    return Err(self.fail("RuneQL sort directives cannot follow AND or OR", None));
                }
                self.parse_sort_directive(sort_rules.as_deref_mut().unwrap())?;
                just_parsed_sort = true;
                continue;
            }

            if !expecting_clause
                && matches!(self.current().kind, TokenKind::And | TokenKind::Or)
            {
                if just_parsed_sort {
                    return Err(
                        self.fail("RuneQL sort directives cannot be joined with AND or OR", None)
                    );
                }
                conjunction = Some(self.current().kind.clone());
                self.consume(None);
                expecting_clause = true;
                if self.is_sort_start() {
                    return Err(
                        self.fail("RuneQL sort directives cannot be joined with AND or OR", None)
                    );
                }
                continue;
            } else if !expecting_clause {
                conjunction = None;
            } else if matches!(self.current().kind, TokenKind::And | TokenKind::Or) {
                return Err(self.fail("RuneQL found a Boolean operator without a left clause", None));
            }

            just_parsed_sort = false;
            let mut modifier: Option<Occur> = None;
            match self.current().kind {
                TokenKind::Plus => {
                    modifier = Some(Occur::Must);
                    self.consume(None);
                }
                TokenKind::Minus | TokenKind::Not => {
                    modifier = Some(Occur::MustNot);
                    self.consume(None);
                }
                _ => {}
            }

            if self.is_sort_start() {
                return Err(self.fail(
                    "RuneQL sort directives cannot be negated or marked required",
                    None,
                ));
            }

            let query = self.parse_primary(inherited_field, depth)?;
            if conjunction == Some(TokenKind::And) {
                if let Some(prev) = clauses.last_mut() {
                    if prev.occur != Occur::MustNot {
                        prev.occur = Occur::Must;
                    }
                }
            }
            let occur = match modifier {
                Some(Occur::MustNot) => Occur::MustNot,
                Some(Occur::Must) => Occur::Must,
                _ if conjunction == Some(TokenKind::And) => Occur::Must,
                _ => Occur::Should,
            };
            clauses.push(BooleanClause { occur, query });
            self.clause_count += 1;
            if self.clause_count > self.limits.max_clauses {
                return Err(QueryError::LimitExceeded(format!(
                    "Query exceeds remote limit of {} clauses",
                    self.limits.max_clauses
                )));
            }
            expecting_clause = false;
            conjunction = None;
        }

        if conjunction.is_some() {
            return Err(self.fail("RuneQL expected a clause after the Boolean operator", None));
        }
        if clauses.is_empty() {
            if allow_sort && sort_rules.as_ref().is_some_and(|r| !r.is_empty()) {
                return Ok(QueryNode {
                    kind: QueryKind::MatchAll,
                    boost: 1.0,
                });
            }
            return Err(self.fail("RuneQL queries cannot be empty", None));
        }
        if expecting_clause {
            return Err(self.fail("RuneQL expected a clause after the Boolean operator", None));
        }

        Ok(QueryNode {
            kind: QueryKind::Boolean(clauses),
            boost: 1.0,
        })
    }
}

fn is_positive_decimal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            seen_digit = true;
        } else if b == b'.' && !seen_dot {
            seen_dot = true;
        } else {
            return false;
        }
    }
    seen_digit
}

pub fn parse_query(query: &str, limits: &QueryLimits) -> Result<ParsedQuery, QueryError> {
    if query.len() > limits.max_query_bytes {
        return Err(QueryError::LimitExceeded(format!(
            "RuneQL remote queries can contain at most {} bytes.",
            limits.max_query_bytes
        )));
    }
    let tokens = tokenize(query)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        clause_count: 0,
        expensive_count: 0,
        limits,
    };
    let mut sort = Vec::new();
    let filter = parser.parse_query(None, None, 0, Some(&mut sort))?;
    if parser.current().kind != TokenKind::Eof {
        return Err(parser.fail("RuneQL could not parse the remainder of this query", None));
    }
    Ok(ParsedQuery { filter, sort })
}

/// Produces a stable canonical query string for CDN and PersistentDB cache keys.
pub fn canonicalize_query(node: &QueryNode) -> String {
    let base = match &node.kind {
        QueryKind::MatchAll => "*:*".to_string(),
        QueryKind::Term {
            field,
            raw,
            fuzzy_edits,
            ..
        } => {
            let prefix = if field == "_all" {
                String::new()
            } else {
                format!("{field}:")
            };
            let suffix = fuzzy_edits.map(|d| format!("~{d}")).unwrap_or_default();
            format!("{prefix}{raw}{suffix}")
        }
        QueryKind::Phrase {
            field,
            raw,
            proximity,
            ..
        } => {
            let prefix = if field == "_all" {
                String::new()
            } else {
                format!("{field}:")
            };
            let suffix = proximity.map(|d| format!("~{d}")).unwrap_or_default();
            format!("{prefix}{raw}{suffix}")
        }
        QueryKind::Range {
            field,
            lower,
            upper,
            include_lower,
            include_upper,
        } => {
            let open = if *include_lower { '[' } else { '{' };
            let close = if *include_upper { ']' } else { '}' };
            let lo = lower.as_deref().unwrap_or("*");
            let hi = upper.as_deref().unwrap_or("*");
            let prefix = if field == "_all" {
                String::new()
            } else {
                format!("{field}:")
            };
            format!("{prefix}{open}{lo} TO {hi}{close}")
        }
        QueryKind::Regex { field, pattern, .. } => {
            let prefix = if field == "_all" {
                String::new()
            } else {
                format!("{field}:")
            };
            format!("{prefix}/{pattern}/")
        }
        QueryKind::Boolean(clauses) => {
            let parts: Vec<String> = clauses
                .iter()
                .map(|c| {
                    let inner = canonicalize_query(&c.query);
                    let wrapped = if matches!(c.query.kind, QueryKind::Boolean(_)) {
                        format!("({inner})")
                    } else {
                        inner
                    };
                    match c.occur {
                        Occur::Must => format!("+{wrapped}"),
                        Occur::MustNot => format!("-{wrapped}"),
                        Occur::Should => wrapped,
                    }
                })
                .collect();
            parts.join(" ")
        }
    };
    if (node.boost - 1.0).abs() > f64::EPSILON {
        format!("{base}^{}", node.boost)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_directives_and_canonical_key() {
        let lim = QueryLimits::default();
        let p = parse_query("gePrice:[2500 TO *] sort: gePrice : descending", &lim).unwrap();
        assert_eq!(canonicalize_query(&p.filter), "geprice:[2500 TO *]");
        assert_eq!(
            p.sort,
            vec![SortRule {
                field: "gePrice".to_string(),
                direction: SortDirection::Desc
            }]
        );

        let multi = parse_query(
            r#"category:"melee-equipment" sort:tier:descending,gePrice:desc,name:ascending"#,
            &lim,
        )
        .unwrap();
        assert_eq!(canonicalize_query(&multi.filter), r#"category:"melee-equipment""#);
        assert_eq!(multi.sort.len(), 3);
        assert_eq!(multi.sort[0].field, "tier");
        assert_eq!(multi.sort[0].direction, SortDirection::Desc);
        assert_eq!(multi.sort[1].field, "gePrice");
        assert_eq!(multi.sort[2].field, "name");
        assert_eq!(multi.sort[2].direction, SortDirection::Asc);

        // Sort-only query becomes MatchAll (*:*)
        let sort_only = parse_query("sort:qty:descending", &lim).unwrap();
        assert_eq!(canonicalize_query(&sort_only.filter), "*:*");
        assert_eq!(sort_only.sort[0].field, "qty");
        assert_eq!(sort_only.sort[0].direction, SortDirection::Desc);

        // Default directions
        let def_num = parse_query("sort:tier", &lim).unwrap();
        assert_eq!(def_num.sort[0].direction, SortDirection::Desc);
        let def_txt = parse_query("sort:name", &lim).unwrap();
        assert_eq!(def_txt.sort[0].direction, SortDirection::Asc);
    }

    #[test]
    fn test_sort_rejections() {
        let lim = QueryLimits::default();
        for bad in [
            "-sort:tier:desc",
            "+sort:tier:desc",
            "NOT sort:tier:desc",
            "category:food AND sort:tier:desc",
            "category:food sort:tier:desc AND category:potions",
            "(category:food sort:tier:desc)",
            "sort:tier:sideways",
            "sort:id:desc",
            "sort:skills:asc",
            "sort:equipRequirements:desc",
            "sort:",
            "sort:tier:desc sort:dose:desc sort:name:asc sort:qty:desc sort:gePrice:desc",
        ] {
            assert!(parse_query(bad, &lim).is_err(), "Expected error for {bad}");
        }
    }
}
