pub mod index;
pub mod lexer;
pub mod normalize;
pub mod parser;
pub mod regex;
pub mod snapshot_codec;

use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::QueryLimits;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("{0}")]
    InvalidSyntax(String),
    #[error("{0}")]
    LimitExceeded(String),
    #[error("{0}")]
    BudgetExceeded(String),
    #[error("Result set too broad ({matched} items exceeds cap of {max}; narrow your query)")]
    ResultTooBroad { matched: usize, max: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    Number(f64),
    Bool(bool),
    List(Vec<String>),
}

impl FieldValue {
    pub fn as_strings(&self) -> Vec<String> {
        match self {
            FieldValue::Text(s) => vec![s.clone()],
            FieldValue::Number(n) => vec![format_number(*n)],
            FieldValue::Bool(b) => vec![if *b { "true" } else { "false" }.to_string()],
            FieldValue::List(vs) => vs.clone(),
        }
    }
}

pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogItem {
    pub id: u32,
    pub config_name: String,
    pub display_name: String,
    pub normalized_name: String,
    pub bankable: bool,
    pub deleted: bool,
    pub fields: BTreeMap<String, FieldValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryMeta {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldMeta {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub multiple: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldValueSuggestion {
    pub value: FieldValue,
    pub label: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSourceInfo {
    pub openrs2_cache_id: u32,
    pub build: String,
    pub timestamp: Option<String>,
    pub archive19_revision: u32,
    pub overlay_applied: bool,
    pub overlay_matched: usize,
    pub total_items: usize,
    pub eligible_items: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit<'a> {
    pub item: &'a CatalogItem,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SortRule {
    pub field: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalMode {
    Include,
    Exclude,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalEntry {
    pub config_name: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResponse {
    pub revision: String,
    pub canonical_query: String,
    pub sort: Vec<SortRule>,
    pub mode: EvalMode,
    pub total: usize,
    pub scope_total: usize,
    pub entries: Vec<EvalEntry>,
}

/// Serializable snapshot payload produced offline from OpenRS2 + overlay and loaded at edge cold start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedSnapshot {
    pub revision: String,
    pub source: CatalogSourceInfo,
    pub categories: Vec<CategoryMeta>,
    pub field_defs: Vec<FieldMeta>,
    pub items: Vec<CatalogItem>,
}

/// Step and wall-clock budget enforced during query evaluation.
pub struct EvalBudget {
    remaining_steps: usize,
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Instant,
    #[cfg(not(target_arch = "wasm32"))]
    max_ms: u64,
}

impl EvalBudget {
    pub fn new(limits: &QueryLimits) -> Self {
        Self {
            remaining_steps: limits.max_eval_steps,
            #[cfg(not(target_arch = "wasm32"))]
            deadline: Instant::now() + limits.timeout(),
            #[cfg(not(target_arch = "wasm32"))]
            max_ms: limits.max_eval_ms,
        }
    }

    #[inline]
    pub fn tick(&mut self, cost: usize) -> Result<(), QueryError> {
        if cost >= self.remaining_steps {
            return Err(QueryError::BudgetExceeded(
                "Query exceeded maximum evaluation step budget".to_string(),
            ));
        }
        self.remaining_steps -= cost;
        // Check wall clock periodically on native targets to keep overhead minimal.
        #[cfg(not(target_arch = "wasm32"))]
        if (self.remaining_steps & 0x3FF) == 0 && Instant::now() >= self.deadline {
            return Err(QueryError::BudgetExceeded(format!(
                "Query exceeded {}ms evaluation deadline",
                self.max_ms
            )));
        }
        Ok(())
    }
}
