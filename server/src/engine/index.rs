use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use crate::config::QueryLimits;
use crate::engine::normalize::{
    WildcardMatcher, lucene_normalize, lucene_terms, match_phrase, match_proximity,
    normalize_display_name, osa_distance,
};
use crate::engine::parser::{Occur, QueryKind, QueryNode, canonicalize_query, parse_query};
use crate::engine::regex::LuceneRegex;
use crate::engine::{
    CatalogItem, CatalogSourceInfo, CategoryMeta, EvalBudget, EvalEntry, EvalMode, EvalResponse,
    FieldMeta, FieldValue, FieldValueSuggestion, QueryError, SearchHit, SortDirection, SortRule,
    format_number,
};

/// Compact dense bitset (~8 KB for 64,206 RS3 items) for nanosecond boolean set operations.
#[derive(Debug, Clone)]
pub struct BitSet {
    words: Vec<u64>,
    len_bits: usize,
}

impl BitSet {
    pub fn empty(len_bits: usize) -> Self {
        Self {
            words: vec![0u64; len_bits.div_ceil(64)],
            len_bits,
        }
    }

    pub fn full(len_bits: usize) -> Self {
        let mut bs = Self {
            words: vec![u64::MAX; len_bits.div_ceil(64)],
            len_bits,
        };
        bs.mask_tail();
        bs
    }

    #[inline]
    fn mask_tail(&mut self) {
        let rem = self.len_bits % 64;
        if rem != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
    }

    #[inline]
    pub fn set(&mut self, idx: usize) {
        self.words[idx >> 6] |= 1u64 << (idx & 63);
    }

    #[inline]
    pub fn contains(&self, idx: usize) -> bool {
        (self.words[idx >> 6] & (1u64 << (idx & 63))) != 0
    }

    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub fn and_inplace(&mut self, other: &BitSet) {
        for (a, &b) in self.words.iter_mut().zip(other.words.iter()) {
            *a &= b;
        }
    }

    pub fn and_not_inplace(&mut self, other: &BitSet) {
        for (a, &b) in self.words.iter_mut().zip(other.words.iter()) {
            *a &= !b;
        }
    }

    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            BitIter {
                word,
                base: wi << 6,
            }
        })
    }
}

struct BitIter {
    word: u64,
    base: usize,
}

impl Iterator for BitIter {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.word == 0 {
            return None;
        }
        let tz = self.word.trailing_zeros() as usize;
        self.word &= self.word - 1;
        Some(self.base + tz)
    }
}

const DENSE_PROMOTE_THRESHOLD: usize = 1024;

/// Hybrid inverted-index posting list: compact `Vec<u32>` for sparse terms (e.g. unique item names)
/// promoted to a dense `BitSet` only when cardinality crosses `DENSE_PROMOTE_THRESHOLD`.
/// Reduces snapshot RAM from ~430 MB to ~25-35 MB for 45k+ RS3 items.
#[derive(Debug)]
enum PostingList {
    Sparse(Vec<u32>),
    Dense(BitSet),
}

impl PostingList {
    #[inline]
    fn new() -> Self {
        Self::Sparse(Vec::new())
    }

    #[inline]
    fn insert(&mut self, doc_id: usize, total_docs: usize) {
        match self {
            Self::Sparse(docs) => {
                let d = doc_id as u32;
                if docs.last() == Some(&d) {
                    return;
                }
                if docs.len() + 1 >= DENSE_PROMOTE_THRESHOLD {
                    let mut bs = BitSet::empty(total_docs);
                    for &existing in docs.iter() {
                        bs.set(existing as usize);
                    }
                    bs.set(doc_id);
                    *self = Self::Dense(bs);
                } else {
                    docs.push(d);
                }
            }
            Self::Dense(bs) => bs.set(doc_id),
        }
    }

    fn finalize(&mut self) {
        if let Self::Sparse(docs) = self {
            docs.shrink_to_fit();
        }
    }

    #[inline]
    fn for_each_in_scope(&self, scope: &BitSet, mut f: impl FnMut(usize)) {
        match self {
            Self::Sparse(docs) => {
                for &d in docs {
                    let doc = d as usize;
                    if scope.contains(doc) {
                        f(doc);
                    }
                }
            }
            Self::Dense(bs) => {
                for (wi, (&a, &b)) in bs.words.iter().zip(scope.words.iter()).enumerate() {
                    let mut word = a & b;
                    let base = wi << 6;
                    while word != 0 {
                        let tz = word.trailing_zeros() as usize;
                        word &= word - 1;
                        f(base + tz);
                    }
                }
            }
        }
    }

    fn intersect_into(&self, target: &mut BitSet) {
        match self {
            Self::Dense(bs) => target.and_inplace(bs),
            Self::Sparse(docs) => {
                let mut next = BitSet::empty(target.len_bits);
                for &d in docs {
                    let doc = d as usize;
                    if target.contains(doc) {
                        next.set(doc);
                    }
                }
                *target = next;
            }
        }
    }
}

#[derive(Debug)]
struct FieldIndex {
    /// Temporary builder map during index construction (drained into `tokens` in `finalize()`).
    token_postings: HashMap<String, PostingList>,
    /// Temporary builder map during index construction (drained into `scalars` in `finalize()`).
    scalar_postings: HashMap<String, PostingList>,
    /// Sorted `(token, posting_list)` for O(log V) exact lookup and zero-hash prefix/wildcard/fuzzy/regex scans.
    tokens: Vec<(String, PostingList)>,
    /// Sorted `(scalar, posting_list)` for O(log V) exact number/bool lookups.
    scalars: Vec<(String, PostingList)>,
    /// Sorted `(numeric_value, doc_id)` column for O(log N) numeric ranges.
    numeric_column: Vec<(f64, u32)>,
    /// Sorted `(normalized_text, doc_id)` column for lexicographic string ranges on non-name text fields.
    text_column: Vec<(String, u32)>,
}

impl FieldIndex {
    fn new() -> Self {
        Self {
            token_postings: HashMap::new(),
            scalar_postings: HashMap::new(),
            tokens: Vec::new(),
            scalars: Vec::new(),
            numeric_column: Vec::new(),
            text_column: Vec::new(),
        }
    }

    fn finalize(&mut self) {
        let mut tokens: Vec<(String, PostingList)> = self
            .token_postings
            .drain()
            .map(|(k, mut p)| {
                p.finalize();
                (k, p)
            })
            .collect();
        self.token_postings = HashMap::new();
        tokens.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        tokens.shrink_to_fit();
        self.tokens = tokens;

        let mut scalars: Vec<(String, PostingList)> = self
            .scalar_postings
            .drain()
            .map(|(k, mut p)| {
                p.finalize();
                (k, p)
            })
            .collect();
        self.scalar_postings = HashMap::new();
        scalars.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        scalars.shrink_to_fit();
        self.scalars = scalars;

        self.numeric_column.sort_unstable_by(|a, b| {
            a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1))
        });
        self.numeric_column.shrink_to_fit();

        self.text_column.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
        });
        self.text_column.shrink_to_fit();
    }

    #[inline]
    fn get_token(&self, token: &str) -> Option<&PostingList> {
        self.tokens
            .binary_search_by(|(k, _)| k.as_str().cmp(token))
            .ok()
            .map(|i| &self.tokens[i].1)
    }

    #[inline]
    fn get_scalar(&self, val: &str) -> Option<&PostingList> {
        self.scalars
            .binary_search_by(|(k, _)| k.as_str().cmp(val))
            .ok()
            .map(|i| &self.scalars[i].1)
    }
}

/// Immutable, thread-safe in-memory RuneQL catalog snapshot and inverted index.
pub struct CatalogSnapshot {
    pub revision: String,
    pub source: CatalogSourceInfo,
    pub categories: Vec<CategoryMeta>,
    pub field_defs: Vec<FieldMeta>,
    pub items: Vec<CatalogItem>,
    /// Doc indices sorted by `items[idx].config_name` (178 KB for 45k items, zero string clones).
    config_order: Vec<u32>,
    eligible_mask: BitSet,
    all_mask: BitSet,
    fields: HashMap<String, FieldIndex>,
    discovery_values: BTreeMap<String, Vec<FieldValueSuggestion>>,
}

impl CatalogSnapshot {
    /// Builds an immutable indexed snapshot from synthesized catalog items.
    /// Items are sorted once in exact RuneQL tie-breaking order:
    /// `(normalizedName ASC, displayName ASC, configName ASC)`.
    #[allow(dead_code)]
    pub fn build(
        mut items: Vec<CatalogItem>,
        revision: String,
        source: CatalogSourceInfo,
        categories: Vec<CategoryMeta>,
        field_defs: Vec<FieldMeta>,
    ) -> Self {
        items.sort_by(|a, b| {
            a.normalized_name
                .cmp(&b.normalized_name)
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.config_name.cmp(&b.config_name))
        });
        Self::build_presorted(items, revision, source, categories, field_defs)
    }

    /// Builds an immutable indexed snapshot from items ALREADY sorted in canonical RuneQL order
    /// `(normalizedName ASC, displayName ASC, configName ASC)`. Used for fast edge cold starts.
    pub fn build_presorted(
        items: Vec<CatalogItem>,
        revision: String,
        mut source: CatalogSourceInfo,
        categories: Vec<CategoryMeta>,
        field_defs: Vec<FieldMeta>,
    ) -> Self {
        let n = items.len();
        let mut config_order: Vec<u32> = (0..n as u32).collect();
        config_order.sort_unstable_by(|&a, &b| {
            items[a as usize]
                .config_name
                .cmp(&items[b as usize].config_name)
        });
        let mut eligible_mask = BitSet::empty(n);
        let all_mask = BitSet::full(n);
        let mut fields: HashMap<String, FieldIndex> = HashMap::new();

        // Value frequency counters for `/v1/values` discovery suggestions.
        let mut value_counts: HashMap<String, HashMap<String, (FieldValue, usize)>> =
            HashMap::new();

        for (doc_id, item) in items.iter().enumerate() {
            let eligible = item.bankable && !item.deleted;
            if eligible {
                eligible_mask.set(doc_id);
            }

            // Index `name` once (`_all` aliases to `name`); `name` and `configname` ranges use
            // `items` and `config_order` directly so they skip redundant `text_column` storage.
            index_text_field(&mut fields, "name", doc_id, &item.display_name, n, false);
            index_text_field(&mut fields, "configname", doc_id, &item.config_name, n, false);

            for (raw_key, fval) in &item.fields {
                let fkey = raw_key.to_ascii_lowercase();
                let fidx = fields.entry(fkey).or_insert_with(FieldIndex::new);

                match fval {
                    FieldValue::Number(num) => {
                        let norm = format_number(*num);
                        fidx.scalar_postings
                            .entry(norm)
                            .or_insert_with(PostingList::new)
                            .insert(doc_id, n);
                        fidx.numeric_column.push((*num, doc_id as u32));
                    }
                    FieldValue::Bool(b) => {
                        let norm = if *b { "true" } else { "false" }.to_string();
                        fidx.scalar_postings
                            .entry(norm)
                            .or_insert_with(PostingList::new)
                            .insert(doc_id, n);
                    }
                    FieldValue::Text(text) => {
                        add_text_to_field_index(fidx, doc_id, text, n, true);
                    }
                    FieldValue::List(values) => {
                        for text in values {
                            add_text_to_field_index(fidx, doc_id, text, n, true);
                        }
                    }
                }

                // Track suggestion counts over eligible items (or all if none eligible).
                if eligible {
                    let counts = value_counts.entry(raw_key.clone()).or_default();
                    match fval {
                        FieldValue::List(vs) => {
                            for v in vs {
                                let entry = counts
                                    .entry(v.clone())
                                    .or_insert_with(|| (FieldValue::Text(v.clone()), 0));
                                entry.1 += 1;
                            }
                        }
                        other => {
                            let k = other.as_strings().into_iter().next().unwrap_or_default();
                            let entry = counts
                                .entry(k)
                                .or_insert_with(|| (other.clone(), 0));
                            entry.1 += 1;
                        }
                    }
                }
            }
        }

        for fidx in fields.values_mut() {
            fidx.finalize();
        }
        fields.shrink_to_fit();

        let category_labels: HashMap<&str, &str> = categories
            .iter()
            .map(|c| (c.key.as_str(), c.name.as_str()))
            .collect();

        let mut discovery_values = BTreeMap::new();
        for (field_key, map) in value_counts {
            let mut rows: Vec<FieldValueSuggestion> = map
                .into_values()
                .map(|(value, count)| {
                    let label = match &value {
                        FieldValue::Text(s) if field_key == "category" => category_labels
                            .get(s.as_str())
                            .map(|l| (*l).to_string())
                            .unwrap_or_else(|| title_case_slug(s)),
                        FieldValue::Text(s) => title_case_slug(s),
                        FieldValue::Number(num) => format_number(*num),
                        FieldValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
                        FieldValue::List(_) => String::new(),
                    };
                    FieldValueSuggestion {
                        value,
                        label,
                        count,
                    }
                })
                .collect();
            rows.sort_by(|a, b| {
                a.label
                    .to_ascii_lowercase()
                    .cmp(&b.label.to_ascii_lowercase())
                    .then_with(|| b.count.cmp(&a.count))
            });
            discovery_values.insert(field_key, rows);
        }

        source.total_items = n;
        source.eligible_items = eligible_mask.count_ones();

        Self {
            revision,
            source,
            categories,
            field_defs,
            items,
            config_order,
            eligible_mask,
            all_mask,
            fields,
            discovery_values,
        }
    }

    pub fn get(&self, config_name: &str) -> Option<&CatalogItem> {
        let key = config_name.trim().to_ascii_uppercase();
        self.config_order
            .binary_search_by(|&idx| self.items[idx as usize].config_name.as_str().cmp(&key))
            .ok()
            .map(|pos| &self.items[self.config_order[pos] as usize])
    }

    /// Matches `Catalog.Find` ranking:
    /// rank 1: exact normalized match; rank 2: prefix; rank 3: word boundary; rank 4: substring.
    pub fn find(
        &self,
        query: &str,
        limit: usize,
        eligible_only: bool,
    ) -> (Vec<&CatalogItem>, usize) {
        let needle = normalize_display_name(query);
        if needle.is_empty() {
            return (Vec::new(), 0);
        }
        let word_needle = format!(" {needle}");
        let cap = limit.max(1);
        let mut buckets: [Vec<&CatalogItem>; 4] = Default::default();
        let mut total = 0usize;

        for (idx, item) in self.items.iter().enumerate() {
            if eligible_only && !self.eligible_mask.contains(idx) {
                continue;
            }
            let Some(pos) = item.normalized_name.find(&needle) else {
                continue;
            };
            total += 1;
            let rank = if item.normalized_name == needle {
                0
            } else if pos == 0 {
                1
            } else if item.normalized_name.contains(&word_needle) {
                2
            } else {
                3
            };
            // `self.items` is already sorted deterministically, so appending preserves exact order!
            if buckets[rank].len() < cap {
                buckets[rank].push(item);
            }
        }

        let mut results = Vec::with_capacity(cap.min(total));
        for bucket in buckets {
            for item in bucket {
                results.push(item);
                if results.len() >= cap {
                    return (results, total);
                }
            }
        }
        (results, total)
    }

    pub fn values(
        &self,
        field_key: &str,
        filter_query: Option<&str>,
        limit: Option<usize>,
    ) -> Option<(Vec<&FieldValueSuggestion>, usize)> {
        let canonical = self
            .field_defs
            .iter()
            .find(|f| f.key.eq_ignore_ascii_case(field_key))
            .map(|f| f.key.as_str())?;

        let rows = match self.discovery_values.get(canonical) {
            Some(r) => r,
            None => return Some((Vec::new(), 0)),
        };

        let needle = filter_query.unwrap_or("").to_ascii_lowercase();
        let mut total = 0usize;
        let mut out = Vec::new();

        for row in rows {
            let val_str = row.value.as_strings().into_iter().next().unwrap_or_default();
            let matches = needle.is_empty()
                || val_str.to_ascii_lowercase().contains(&needle)
                || row.label.to_ascii_lowercase().contains(&needle);
            if matches {
                total += 1;
                if limit.is_none_or(|m| out.len() < m) {
                    out.push(row);
                }
            }
        }
        Some((out, total))
    }

    /// Evaluates a RuneQL Lucene query and returns paginated hits ordered by any `sort:` rules
    /// (NULLS LAST), then deterministic `(score DESC, normalizedName ASC, displayName ASC, configName ASC)`.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
        eligible_only: bool,
        limits: &QueryLimits,
    ) -> Result<(Vec<SearchHit<'_>>, usize, String, Vec<SortRule>), QueryError> {
        let parsed = parse_query(query, limits)?;
        let canonical = canonicalize_query(&parsed.filter);
        let mut budget = EvalBudget::new(limits);
        let scope = if eligible_only {
            &self.eligible_mask
        } else {
            &self.all_mask
        };

        let scored = self.eval_node(&parsed.filter, scope, &mut budget)?;
        let total = scored.len();

        let mut ranked: Vec<(usize, f64)> = scored.into_iter().collect();
        if parsed.sort.is_empty() {
            ranked.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
        } else {
            ranked.sort_by(|&(doc_a, score_a), &(doc_b, score_b)| {
                let item_a = &self.items[doc_a];
                let item_b = &self.items[doc_b];
                for rule in &parsed.sort {
                    let ord = compare_items_by_rule(item_a, score_a, item_b, score_b, rule);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                score_b
                    .partial_cmp(&score_a)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| doc_a.cmp(&doc_b))
            });
        }

        let cap = limit.clamp(1, limits.max_search_limit);
        let page = ranked
            .into_iter()
            .skip(offset)
            .take(cap)
            .map(|(doc_id, score)| SearchHit {
                item: &self.items[doc_id],
                score,
            })
            .collect();

        Ok((page, total, canonical, parsed.sort))
    }

    /// Evaluates a static catalog query into a compact `configName` set for Bank Tags / local plan evaluation.
    /// `sort:` directives are parsed and echoed back while stripped from the filter cache key so
    /// changing client-side sort order reuses the exact same cached result set.
    pub fn eval_set(
        &self,
        query: &str,
        eligible_only: bool,
        include_scores: bool,
        allow_complement: bool,
        limits: &QueryLimits,
    ) -> Result<EvalResponse, QueryError> {
        let parsed = parse_query(query, limits)?;
        let canonical = canonicalize_query(&parsed.filter);
        let mut budget = EvalBudget::new(limits);
        let scope = if eligible_only {
            &self.eligible_mask
        } else {
            &self.all_mask
        };

        let scored = self.eval_node(&parsed.filter, scope, &mut budget)?;
        let total = scored.len();
        let scope_total = scope.count_ones();

        // If >50% of scope matches and scores are uniform or not requested, complement encoding shrinks payload dramatically.
        let uniform_score = scored
            .values()
            .next()
            .copied()
            .filter(|&first| scored.values().all(|&s| (s - first).abs() < f64::EPSILON));

        let use_complement = allow_complement
            && total > scope_total / 2
            && (!include_scores || uniform_score.is_some());

        if use_complement {
            let excluded_count = scope_total.saturating_sub(total);
            if limits.max_eval_results > 0 && excluded_count > limits.max_eval_results {
                return Err(QueryError::ResultTooBroad {
                    matched: total,
                    max: limits.max_eval_results,
                });
            }
            let mut entries = Vec::with_capacity(excluded_count);
            for doc_id in scope.iter_ones() {
                if !scored.contains_key(&doc_id) {
                    entries.push(EvalEntry {
                        config_name: self.items[doc_id].config_name.clone(),
                        score: uniform_score.unwrap_or(1.0),
                    });
                }
            }
            return Ok(EvalResponse {
                revision: self.revision.clone(),
                canonical_query: canonical,
                sort: parsed.sort,
                mode: EvalMode::Exclude,
                total,
                scope_total,
                entries,
            });
        }

        if limits.max_eval_results > 0 && total > limits.max_eval_results {
            return Err(QueryError::ResultTooBroad {
                matched: total,
                max: limits.max_eval_results,
            });
        }

        let mut ranked: Vec<(usize, f64)> = scored.into_iter().collect();
        ranked.sort_by(|a, b| {
            if include_scores {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            } else {
                a.0.cmp(&b.0)
            }
        });

        let entries = ranked
            .into_iter()
            .map(|(doc_id, score)| EvalEntry {
                config_name: self.items[doc_id].config_name.clone(),
                score,
            })
            .collect();

        Ok(EvalResponse {
            revision: self.revision.clone(),
            canonical_query: canonical,
            sort: parsed.sort,
            mode: EvalMode::Include,
            total,
            scope_total,
            entries,
        })
    }

    fn eval_node(
        &self,
        node: &QueryNode,
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        budget.tick(1)?;
        let mut scores = match &node.kind {
            QueryKind::MatchAll => {
                let mut map = HashMap::with_capacity(scope.count_ones());
                for doc in scope.iter_ones() {
                    map.insert(doc, 1.0);
                }
                map
            }
            QueryKind::Term {
                field,
                normalized,
                query_terms,
                fuzzy_edits,
                wildcard,
                ..
            } => self.eval_term(
                field,
                normalized,
                query_terms,
                *fuzzy_edits,
                wildcard.as_ref(),
                scope,
                budget,
            )?,
            QueryKind::Phrase {
                field,
                terms,
                proximity,
                ..
            } => self.eval_phrase(field, terms, *proximity, scope, budget)?,
            QueryKind::Range {
                field,
                lower,
                upper,
                include_lower,
                include_upper,
            } => self.eval_range(
                field,
                lower.as_deref(),
                upper.as_deref(),
                *include_lower,
                *include_upper,
                scope,
                budget,
            )?,
            QueryKind::Regex {
                field, compiled, ..
            } => self.eval_regex(field, compiled, scope, budget)?,
            QueryKind::Boolean(clauses) => self.eval_boolean(clauses, scope, budget)?,
        };

        if (node.boost - 1.0).abs() > f64::EPSILON {
            for score in scores.values_mut() {
                *score *= node.boost;
            }
        }
        Ok(scores)
    }

    #[inline]
    fn get_field(&self, field: &str) -> Option<&FieldIndex> {
        let key = if field == "_all" { "name" } else { field };
        self.fields.get(key)
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_term(
        &self,
        field: &str,
        normalized: &str,
        query_terms: &[String],
        fuzzy_edits: Option<usize>,
        wildcard: Option<&WildcardMatcher>,
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        let Some(fidx) = self.get_field(field) else {
            return Ok(HashMap::new());
        };
        let mut out: HashMap<usize, f64> = HashMap::new();

        if let Some(max_edits) = fuzzy_edits {
            // Fuzzy only matches non-scalar tokens (matches lucene_match.lua).
            for (token, pl) in &fidx.tokens {
                budget.tick(1)?;
                let dist = osa_distance(normalized, token, max_edits);
                if dist <= max_edits && dist < normalized.len().min(token.len()) {
                    let score = 80.0 - (dist as f64) * 10.0;
                    pl.for_each_in_scope(scope, |doc| {
                        out.entry(doc)
                            .and_modify(|s| {
                                if score > *s {
                                    *s = score;
                                }
                            })
                            .or_insert(score);
                    });
                }
            }
        } else if let Some(matcher) = wildcard {
            // Narrow scan using literal prefix via binary search!
            let start_idx = if matcher.literal_prefix.is_empty() {
                0
            } else {
                fidx.tokens
                    .partition_point(|(t, _)| t.as_str() < matcher.literal_prefix.as_str())
            };
            for (token, pl) in &fidx.tokens[start_idx..] {
                if !matcher.literal_prefix.is_empty() && !token.starts_with(&matcher.literal_prefix)
                {
                    break;
                }
                budget.tick(1)?;
                if matcher.matches(token) {
                    pl.for_each_in_scope(scope, |doc| {
                        out.entry(doc).or_insert(70.0);
                    });
                }
            }
        } else {
            // Exact scalar match
            if let Some(pl) = fidx.get_scalar(normalized) {
                pl.for_each_in_scope(scope, |doc| {
                    out.insert(doc, 100.0);
                });
            }
            // Exact token match
            for qt in query_terms {
                if let Some(pl) = fidx.get_token(qt) {
                    pl.for_each_in_scope(scope, |doc| {
                        out.insert(doc, 100.0);
                    });
                }
            }
        }
        Ok(out)
    }

    fn eval_phrase(
        &self,
        field: &str,
        wanted: &[String],
        proximity: Option<usize>,
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let Some(fidx) = self.get_field(field) else {
            return Ok(HashMap::new());
        };

        // Intersect all constituent term postings first -> narrows 45k items to a tiny candidate set!
        let mut candidates = scope.clone();
        for w in wanted {
            match fidx.get_token(w) {
                Some(pl) => pl.intersect_into(&mut candidates),
                None => return Ok(HashMap::new()),
            }
            if candidates.is_empty() {
                return Ok(HashMap::new());
            }
        }

        let mut out = HashMap::new();
        for doc in candidates.iter_ones() {
            budget.tick(2)?;
            if let Some(score) = eval_phrase_on_item(&self.items[doc], field, wanted, proximity) {
                out.insert(doc, score);
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_range(
        &self,
        field: &str,
        lower: Option<&str>,
        upper: Option<&str>,
        include_lower: bool,
        include_upper: bool,
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        let Some(fidx) = self.get_field(field) else {
            return Ok(HashMap::new());
        };

        let lo_num = lower.and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite());
        let hi_num = upper.and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite());
        let lo_is_num = lower.is_none() || lo_num.is_some();
        let hi_is_num = upper.is_none() || hi_num.is_some();

        let mut out = HashMap::new();

        // Fast O(log N) binary-search path over sorted numeric column!
        if lo_is_num && hi_is_num && !fidx.numeric_column.is_empty() {
            let start = match lo_num {
                Some(lo) => fidx.numeric_column.partition_point(|&(v, _)| {
                    if include_lower { v < lo } else { v <= lo }
                }),
                None => 0,
            };
            let end = match hi_num {
                Some(hi) => fidx.numeric_column.partition_point(|&(v, _)| {
                    if include_upper { v <= hi } else { v < hi }
                }),
                None => fidx.numeric_column.len(),
            };
            if start < end {
                for &(_, doc_u32) in &fidx.numeric_column[start..end] {
                    let doc = doc_u32 as usize;
                    if scope.contains(doc) {
                        out.insert(doc, 90.0);
                    }
                }
            }
        }

        // Lexicographic text range fallback (for text fields or non-numeric boundaries).
        if !lo_is_num || !hi_is_num || fidx.numeric_column.is_empty() {
            if field == "_all" || field == "name" {
                let lo_txt = lower.map(normalize_display_name);
                let hi_txt = upper.map(normalize_display_name);
                let start = match lo_txt.as_deref() {
                    Some(lo) => self.items.partition_point(|it| {
                        if include_lower {
                            it.normalized_name.as_str() < lo
                        } else {
                            it.normalized_name.as_str() <= lo
                        }
                    }),
                    None => 0,
                };
                let end = match hi_txt.as_deref() {
                    Some(hi) => self.items.partition_point(|it| {
                        if include_upper {
                            it.normalized_name.as_str() <= hi
                        } else {
                            it.normalized_name.as_str() < hi
                        }
                    }),
                    None => self.items.len(),
                };
                for doc in start..end {
                    budget.tick(1)?;
                    if scope.contains(doc) {
                        out.insert(doc, 90.0);
                    }
                }
            } else if field == "configname" {
                let lo_txt = lower.map(|s| s.trim().to_ascii_uppercase());
                let hi_txt = upper.map(|s| s.trim().to_ascii_uppercase());
                let start = match lo_txt.as_deref() {
                    Some(lo) => self.config_order.partition_point(|&idx| {
                        let v = self.items[idx as usize].config_name.as_str();
                        if include_lower { v < lo } else { v <= lo }
                    }),
                    None => 0,
                };
                let end = match hi_txt.as_deref() {
                    Some(hi) => self.config_order.partition_point(|&idx| {
                        let v = self.items[idx as usize].config_name.as_str();
                        if include_upper { v <= hi } else { v < hi }
                    }),
                    None => self.config_order.len(),
                };
                for &idx in &self.config_order[start..end] {
                    budget.tick(1)?;
                    let doc = idx as usize;
                    if scope.contains(doc) {
                        out.insert(doc, 90.0);
                    }
                }
            } else {
                let lo_txt = lower.map(lucene_normalize);
                let hi_txt = upper.map(lucene_normalize);
                let start = match lo_txt.as_deref() {
                    Some(lo) => fidx.text_column.partition_point(|(v, _)| {
                        if include_lower {
                            v.as_str() < lo
                        } else {
                            v.as_str() <= lo
                        }
                    }),
                    None => 0,
                };
                let end = match hi_txt.as_deref() {
                    Some(hi) => fidx.text_column.partition_point(|(v, _)| {
                        if include_upper {
                            v.as_str() <= hi
                        } else {
                            v.as_str() < hi
                        }
                    }),
                    None => fidx.text_column.len(),
                };
                if start < end {
                    for (_, doc_u32) in &fidx.text_column[start..end] {
                        budget.tick(1)?;
                        let doc = *doc_u32 as usize;
                        if scope.contains(doc) {
                            out.insert(doc, 90.0);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn eval_regex(
        &self,
        field: &str,
        compiled: &LuceneRegex,
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        let Some(fidx) = self.get_field(field) else {
            return Ok(HashMap::new());
        };
        let mut out = HashMap::new();
        for (token, pl) in &fidx.tokens {
            budget.tick(2)?;
            if compiled.matches(token)? {
                pl.for_each_in_scope(scope, |doc| {
                    out.insert(doc, 75.0);
                });
            }
        }
        Ok(out)
    }

    fn eval_boolean(
        &self,
        clauses: &[crate::engine::parser::BooleanClause],
        scope: &BitSet,
        budget: &mut EvalBudget,
    ) -> Result<HashMap<usize, f64>, QueryError> {
        let mut musts = Vec::new();
        let mut shoulds = Vec::new();
        let mut must_nots = Vec::new();

        for c in clauses {
            match c.occur {
                Occur::Must => musts.push(&c.query),
                Occur::Should => shoulds.push(&c.query),
                Occur::MustNot => must_nots.push(&c.query),
            }
        }

        // Pure negative query (no MUST and no SHOULD) matches nothing, per Lucene & evaluator.lua.
        if musts.is_empty() && shoulds.is_empty() {
            return Ok(HashMap::new());
        }

        let mut active_scope = scope.clone();
        let mut scores: HashMap<usize, f64> = HashMap::new();

        // 1. Evaluate MUST clauses sequentially, shrinking `active_scope` after each one!
        if !musts.is_empty() {
            let first = self.eval_node(musts[0], &active_scope, budget)?;
            if first.is_empty() {
                return Ok(HashMap::new());
            }
            active_scope = BitSet::empty(self.items.len());
            for (&doc, &sc) in &first {
                active_scope.set(doc);
                scores.insert(doc, sc);
            }

            for clause in &musts[1..] {
                let next = self.eval_node(clause, &active_scope, budget)?;
                if next.is_empty() {
                    return Ok(HashMap::new());
                }
                let mut narrowed = BitSet::empty(self.items.len());
                let mut combined = HashMap::with_capacity(next.len());
                for (doc, sc) in next {
                    if let Some(&prev) = scores.get(&doc) {
                        narrowed.set(doc);
                        combined.insert(doc, prev + sc);
                    }
                }
                if combined.is_empty() {
                    return Ok(HashMap::new());
                }
                active_scope = narrowed;
                scores = combined;
            }
        }

        // 2. Subtract MUST_NOT clauses from `active_scope` & `scores`!
        for clause in must_nots {
            let excluded = self.eval_node(clause, &active_scope, budget)?;
            if !excluded.is_empty() {
                let mut ex_bs = BitSet::empty(self.items.len());
                for doc in excluded.into_keys() {
                    ex_bs.set(doc);
                    scores.remove(&doc);
                }
                active_scope.and_not_inplace(&ex_bs);
                if active_scope.is_empty() {
                    return Ok(HashMap::new());
                }
            }
        }

        // 3. Evaluate SHOULD clauses (boosting when MUSTs exist, or unioning when no MUSTs exist).
        if musts.is_empty() {
            for clause in shoulds {
                let matched = self.eval_node(clause, &active_scope, budget)?;
                for (doc, sc) in matched {
                    scores
                        .entry(doc)
                        .and_modify(|s| *s += sc)
                        .or_insert(sc);
                }
            }
        } else {
            for clause in shoulds {
                let matched = self.eval_node(clause, &active_scope, budget)?;
                for (doc, sc) in matched {
                    if let Some(existing) = scores.get_mut(&doc) {
                        *existing += sc;
                    }
                }
            }
        }

        Ok(scores)
    }
}

fn eval_phrase_on_item(
    item: &CatalogItem,
    field: &str,
    wanted: &[String],
    proximity: Option<usize>,
) -> Option<f64> {
    let score_text = |text: &str| -> Option<f64> {
        let tokens = lucene_terms(text);
        match proximity {
            Some(slop) => match_proximity(wanted, &tokens, slop),
            None => match_phrase(wanted, &tokens),
        }
    };

    if field == "_all" || field == "name" {
        return score_text(&item.display_name);
    }
    if field == "configname" {
        return score_text(&item.config_name);
    }
    let fv = item.fields.get(field).or_else(|| {
        item.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(field))
            .map(|(_, v)| v)
    })?;
    match fv {
        FieldValue::Text(s) => score_text(s),
        FieldValue::List(vs) => {
            let mut best: Option<f64> = None;
            for s in vs {
                if let Some(sc) = score_text(s) {
                    if best.is_none_or(|b| sc > b) {
                        best = Some(sc);
                    }
                }
            }
            best
        }
        _ => None,
    }
}

fn index_text_field(
    fields: &mut HashMap<String, FieldIndex>,
    field_key: &str,
    doc_id: usize,
    text: &str,
    total_docs: usize,
    store_text_column: bool,
) {
    let fidx = fields
        .entry(field_key.to_string())
        .or_insert_with(FieldIndex::new);
    add_text_to_field_index(fidx, doc_id, text, total_docs, store_text_column);
}

fn add_text_to_field_index(
    fidx: &mut FieldIndex,
    doc_id: usize,
    text: &str,
    total_docs: usize,
    store_text_column: bool,
) {
    if store_text_column {
        let norm = lucene_normalize(text);
        if norm.is_empty() {
            return;
        }
        fidx.text_column.push((norm, doc_id as u32));
    }
    let terms = lucene_terms(text);
    for t in terms {
        fidx.token_postings
            .entry(t)
            .or_insert_with(PostingList::new)
            .insert(doc_id, total_docs);
    }
}

fn compare_items_by_rule(
    a: &CatalogItem,
    score_a: f64,
    b: &CatalogItem,
    score_b: f64,
    rule: &SortRule,
) -> Ordering {
    let val_a = extract_sort_key(a, score_a, &rule.field);
    let val_b = extract_sort_key(b, score_b, &rule.field);
    match (val_a, val_b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // NULLS LAST
        (Some(_), None) => Ordering::Less,
        (Some(ka), Some(kb)) => {
            let ord = match (ka, kb) {
                (SortKey::Num(x), SortKey::Num(y)) => x.total_cmp(&y),
                (SortKey::Bool(x), SortKey::Bool(y)) => x.cmp(&y),
                (SortKey::Text(x), SortKey::Text(y)) => x.cmp(&y),
                (x, y) => x.as_text().cmp(&y.as_text()),
            };
            match rule.direction {
                SortDirection::Asc => ord,
                SortDirection::Desc => ord.reverse(),
            }
        }
    }
}

enum SortKey<'a> {
    Num(f64),
    Bool(bool),
    Text(&'a str),
}

impl<'a> SortKey<'a> {
    fn as_text(&self) -> String {
        match self {
            SortKey::Num(n) => format_number(*n),
            SortKey::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            SortKey::Text(s) => (*s).to_string(),
        }
    }
}

fn extract_sort_key<'a>(item: &'a CatalogItem, score: f64, field: &str) -> Option<SortKey<'a>> {
    match field {
        "score" => Some(SortKey::Num(score)),
        "name" => Some(SortKey::Text(item.normalized_name.as_str())),
        "configName" => Some(SortKey::Text(item.config_name.as_str())),
        other => {
            let fv = item.fields.get(other).or_else(|| {
                item.fields
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(other))
                    .map(|(_, v)| v)
            })?;
            match fv {
                FieldValue::Number(n) => Some(SortKey::Num(*n)),
                FieldValue::Bool(b) => Some(SortKey::Bool(*b)),
                FieldValue::Text(s) => Some(SortKey::Text(s.as_str())),
                FieldValue::List(vs) if vs.len() == 1 => Some(SortKey::Text(vs[0].as_str())),
                FieldValue::List(_) => None,
            }
        }
    }
}

fn title_case_slug(s: &str) -> String {
    s.split('-')
        .filter(|p| !p.is_empty())
        .enumerate()
        .map(|(i, part)| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if i == 0 => {
                    format!("{}{}", first.to_uppercase(), chars.as_str())
                }
                _ => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
