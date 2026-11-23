use std::collections::BTreeMap;

use crate::engine::{
    CatalogItem, EvalMode, EvalResponse, FieldValue, FieldValueSuggestion, SearchHit, format_number,
};

pub const PACKED_CONTENT_TYPE: &str = "text/x-runeql-packed; charset=utf-8";

pub fn escape_packed(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            '|' => out.push_str("%7C"),
            other => out.push(other),
        }
    }
    out
}

fn encode_field_value(fv: &FieldValue) -> String {
    match fv {
        FieldValue::Text(s) => format!("s{}", escape_packed(s)),
        FieldValue::Number(n) => format!("n{}", format_number(*n)),
        FieldValue::Bool(b) => if *b { "b1" } else { "b0" }.to_string(),
        FieldValue::List(vs) => {
            let mut out = String::from("a");
            for (i, item) in vs.iter().enumerate() {
                if i > 0 {
                    out.push('|');
                }
                out.push('s');
                out.push_str(&escape_packed(item));
            }
            out
        }
    }
}

pub fn encode_fields_row(fields: &BTreeMap<String, FieldValue>) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_field_value(v)))
        .collect::<Vec<_>>()
        .join("\t")
}

/// Encodes `/v1/eval` response for direct Lua parsing & verbatim `PersistentDB:SetString` caching.
/// Line 1 (header): `v1\t<rev>\t<include|exclude>\t<total>\t<scope_total>`
/// Lines 2+: `<CONFIG_NAME>\t<score>` (or `<CONFIG_NAME>` when `include_scores == false`)
pub fn encode_eval(resp: &EvalResponse, include_scores: bool) -> String {
    let mode = match resp.mode {
        EvalMode::Include => "include",
        EvalMode::Exclude => "exclude",
    };
    let mut out = format!(
        "v1\t{}\t{mode}\t{}\t{}\n",
        resp.revision, resp.total, resp.scope_total
    );
    for entry in &resp.entries {
        out.push_str(&entry.config_name);
        if include_scores {
            out.push('\t');
            out.push_str(&format_number(entry.score));
        }
        out.push('\n');
    }
    out
}

/// Encodes `/v1/search` response:
/// Line 1: `v1\t<rev>\t<total>\t<offset>`
/// Lines 2+: `<CONFIG_NAME>\t<score>\t<escaped_display_name>\t<normalized_name>[\t<packed_fields>]`
pub fn encode_search(
    revision: &str,
    hits: &[SearchHit<'_>],
    total: usize,
    offset: usize,
    include_fields: bool,
) -> String {
    let mut out = format!("v1\t{revision}\t{total}\t{offset}\n");
    for hit in hits {
        out.push_str(&hit.item.config_name);
        out.push('\t');
        out.push_str(&format_number(hit.score));
        out.push('\t');
        out.push_str(&escape_packed(&hit.item.display_name));
        out.push('\t');
        out.push_str(&hit.item.normalized_name);
        if include_fields {
            let attrs = encode_fields_row(&hit.item.fields);
            if !attrs.is_empty() {
                out.push('\t');
                out.push_str(&attrs);
            }
        }
        out.push('\n');
    }
    out
}

/// Encodes `/v1/find` response:
/// Line 1: `v1\t<rev>\t<total>`
/// Lines 2+: `<CONFIG_NAME>\t<escaped_display_name>\t<normalized_name>[\t<packed_fields>]`
pub fn encode_find(
    revision: &str,
    items: &[&CatalogItem],
    total: usize,
    include_fields: bool,
) -> String {
    let mut out = format!("v1\t{revision}\t{total}\n");
    for item in items {
        out.push_str(&item.config_name);
        out.push('\t');
        out.push_str(&escape_packed(&item.display_name));
        out.push('\t');
        out.push_str(&item.normalized_name);
        if include_fields {
            let attrs = encode_fields_row(&item.fields);
            if !attrs.is_empty() {
                out.push('\t');
                out.push_str(&attrs);
            }
        }
        out.push('\n');
    }
    out
}

pub fn encode_values(revision: &str, rows: &[&FieldValueSuggestion], total: usize) -> String {
    let mut out = format!("v1\t{revision}\t{total}\n");
    for row in rows {
        out.push_str(&encode_field_value(&row.value));
        out.push('\t');
        out.push_str(&escape_packed(&row.label));
        out.push('\t');
        out.push_str(&row.count.to_string());
        out.push('\n');
    }
    out
}

pub fn encode_item(revision: &str, item: &CatalogItem) -> String {
    let mut out = format!(
        "v1\t{revision}\t{}\t{}\t{}\t{}\t{}\n",
        item.config_name,
        escape_packed(&item.display_name),
        item.normalized_name,
        if item.bankable { 1 } else { 0 },
        if item.deleted { 1 } else { 0 },
    );
    out.push_str(&encode_fields_row(&item.fields));
    out.push('\n');
    out
}
