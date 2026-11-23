#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

use crate::engine::{
    CatalogItem, CatalogSourceInfo, CategoryMeta, FieldMeta, FieldValue, SerializedSnapshot,
};

const MAGIC: &[u8; 4] = b"RQL1";

/// Encodes a `SerializedSnapshot` into the compact interned `RQL1` binary format.
/// Reduces a 45k-item snapshot from ~15 MB JSON down to ~3-4 MB raw (~1.1 MB gzipped)
/// so Cloudflare Workers WASM cold starts finish in tens of milliseconds with minimal peak RAM.
pub fn encode_snapshot(snapshot: &SerializedSnapshot) -> Vec<u8> {
    let mut key_map: HashMap<&str, usize> = HashMap::new();
    let mut key_table: Vec<&str> = Vec::new();
    let mut val_map: HashMap<&str, usize> = HashMap::new();
    let mut val_table: Vec<&str> = Vec::new();

    for item in &snapshot.items {
        for (k, v) in &item.fields {
            intern(&mut key_map, &mut key_table, k.as_str());
            match v {
                FieldValue::Text(s) => {
                    intern(&mut val_map, &mut val_table, s.as_str());
                }
                FieldValue::List(vs) => {
                    for s in vs {
                        intern(&mut val_map, &mut val_table, s.as_str());
                    }
                }
                FieldValue::Number(_) | FieldValue::Bool(_) => {}
            }
        }
    }

    let mut out = Vec::with_capacity(snapshot.items.len() * 72);
    out.extend_from_slice(MAGIC);

    write_str(&mut out, &snapshot.revision);
    write_varint(&mut out, snapshot.source.openrs2_cache_id as u64);
    write_str(&mut out, &snapshot.source.build);
    write_opt_str(&mut out, snapshot.source.timestamp.as_deref());
    write_varint(&mut out, snapshot.source.archive19_revision as u64);
    out.push(u8::from(snapshot.source.overlay_applied));
    write_varint(&mut out, snapshot.source.overlay_matched as u64);
    write_varint(&mut out, snapshot.source.total_items as u64);
    write_varint(&mut out, snapshot.source.eligible_items as u64);

    write_varint(&mut out, snapshot.categories.len() as u64);
    for cat in &snapshot.categories {
        write_str(&mut out, &cat.key);
        write_str(&mut out, &cat.name);
    }

    write_varint(&mut out, snapshot.field_defs.len() as u64);
    for f in &snapshot.field_defs {
        write_str(&mut out, &f.key);
        write_str(&mut out, &f.label);
        write_str(&mut out, &f.field_type);
        out.push(u8::from(f.multiple));
    }

    write_varint(&mut out, key_table.len() as u64);
    for k in &key_table {
        write_str(&mut out, k);
    }

    write_varint(&mut out, val_table.len() as u64);
    for v in &val_table {
        write_str(&mut out, v);
    }

    write_varint(&mut out, snapshot.items.len() as u64);
    for item in &snapshot.items {
        write_varint(&mut out, item.id as u64);
        let flags = u8::from(item.bankable) | (u8::from(item.deleted) << 1);
        out.push(flags);
        write_str(&mut out, &item.config_name);
        write_str(&mut out, &item.display_name);
        write_str(&mut out, &item.normalized_name);
        write_varint(&mut out, item.fields.len() as u64);
        for (k, v) in &item.fields {
            let kidx = key_map[k.as_str()];
            write_varint(&mut out, kidx as u64);
            match v {
                FieldValue::Text(s) => {
                    out.push(0);
                    write_varint(&mut out, val_map[s.as_str()] as u64);
                }
                FieldValue::Number(n) => {
                    if n.fract() == 0.0 && n.abs() < 4_503_599_627_370_496.0 {
                        out.push(1);
                        write_zigzag(&mut out, *n as i64);
                    } else {
                        out.push(2);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
                FieldValue::Bool(false) => out.push(3),
                FieldValue::Bool(true) => out.push(4),
                FieldValue::List(vs) => {
                    out.push(5);
                    write_varint(&mut out, vs.len() as u64);
                    for s in vs {
                        write_varint(&mut out, val_map[s.as_str()] as u64);
                    }
                }
            }
        }
    }

    out
}

/// Decodes a `RQL1` binary payload back into a `SerializedSnapshot`.
pub fn decode_snapshot(bytes: &[u8]) -> Result<SerializedSnapshot, String> {
    if bytes.len() < 4 || &bytes[..4] != MAGIC {
        return Err("Invalid RuneQL snapshot magic header".to_string());
    }
    let mut r = Reader { bytes, pos: 4 };

    let revision = r.read_str()?;
    let openrs2_cache_id = r.read_varint()? as u32;
    let build = r.read_str()?;
    let timestamp = r.read_opt_str()?;
    let archive19_revision = r.read_varint()? as u32;
    let overlay_applied = r.read_u8()? != 0;
    let overlay_matched = r.read_varint()? as usize;
    let total_items = r.read_varint()? as usize;
    let eligible_items = r.read_varint()? as usize;

    let source = CatalogSourceInfo {
        openrs2_cache_id,
        build,
        timestamp,
        archive19_revision,
        overlay_applied,
        overlay_matched,
        total_items,
        eligible_items,
    };

    let cat_len = r.read_varint()? as usize;
    let mut categories = Vec::with_capacity(cat_len);
    for _ in 0..cat_len {
        let key = r.read_str()?;
        let name = r.read_str()?;
        categories.push(CategoryMeta { key, name });
    }

    let field_len = r.read_varint()? as usize;
    let mut field_defs = Vec::with_capacity(field_len);
    for _ in 0..field_len {
        let key = r.read_str()?;
        let label = r.read_str()?;
        let field_type = r.read_str()?;
        let multiple = r.read_u8()? != 0;
        field_defs.push(FieldMeta {
            key,
            label,
            field_type,
            multiple,
        });
    }

    let key_table_len = r.read_varint()? as usize;
    let mut key_table = Vec::with_capacity(key_table_len);
    for _ in 0..key_table_len {
        key_table.push(r.read_str()?);
    }

    let val_table_len = r.read_varint()? as usize;
    let mut val_table = Vec::with_capacity(val_table_len);
    for _ in 0..val_table_len {
        val_table.push(r.read_str()?);
    }

    let items_len = r.read_varint()? as usize;
    let mut items = Vec::with_capacity(items_len);
    for _ in 0..items_len {
        let id = r.read_varint()? as u32;
        let flags = r.read_u8()?;
        let bankable = (flags & 1) != 0;
        let deleted = (flags & 2) != 0;
        let config_name = r.read_str()?;
        let display_name = r.read_str()?;
        let normalized_name = r.read_str()?;
        let fc = r.read_varint()? as usize;
        let mut fields = BTreeMap::new();
        for _ in 0..fc {
            let kidx = r.read_varint()? as usize;
            let key = key_table
                .get(kidx)
                .ok_or_else(|| format!("Key index {kidx} out of bounds"))?
                .clone();
            let tag = r.read_u8()?;
            let val = match tag {
                0 => {
                    let vidx = r.read_varint()? as usize;
                    let s = val_table
                        .get(vidx)
                        .ok_or_else(|| format!("Value index {vidx} out of bounds"))?
                        .clone();
                    FieldValue::Text(s)
                }
                1 => FieldValue::Number(r.read_zigzag()? as f64),
                2 => FieldValue::Number(r.read_f64()?),
                3 => FieldValue::Bool(false),
                4 => FieldValue::Bool(true),
                5 => {
                    let l = r.read_varint()? as usize;
                    let mut vs = Vec::with_capacity(l);
                    for _ in 0..l {
                        let vidx = r.read_varint()? as usize;
                        let s = val_table
                            .get(vidx)
                            .ok_or_else(|| format!("Value index {vidx} out of bounds"))?
                            .clone();
                        vs.push(s);
                    }
                    FieldValue::List(vs)
                }
                other => return Err(format!("Unknown field tag {other}")),
            };
            fields.insert(key, val);
        }
        items.push(CatalogItem {
            id,
            config_name,
            display_name,
            normalized_name,
            bankable,
            deleted,
            fields,
        });
    }

    Ok(SerializedSnapshot {
        revision,
        source,
        categories,
        field_defs,
        items,
    })
}

#[inline]
fn intern<'a>(map: &mut HashMap<&'a str, usize>, table: &mut Vec<&'a str>, s: &'a str) -> usize {
    if let Some(&idx) = map.get(s) {
        idx
    } else {
        let idx = table.len();
        table.push(s);
        map.insert(s, idx);
        idx
    }
}

#[inline]
fn write_varint(out: &mut Vec<u8>, mut val: u64) {
    loop {
        let mut b = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            b |= 0x80;
            out.push(b);
        } else {
            out.push(b);
            break;
        }
    }
}

#[inline]
fn write_zigzag(out: &mut Vec<u8>, v: i64) {
    let encoded = ((v << 1) ^ (v >> 63)) as u64;
    write_varint(out, encoded);
}

#[inline]
fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

#[inline]
fn write_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(v) => {
            out.push(1);
            write_str(out, v);
        }
        None => out.push(0),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn read_u8(&mut self) -> Result<u8, String> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| "Unexpected EOF reading u8".to_string())?;
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    fn read_varint(&mut self) -> Result<u64, String> {
        let mut res = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.read_u8()?;
            res |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Ok(res);
            }
            shift += 7;
            if shift >= 64 {
                return Err("Varint overflow".to_string());
            }
        }
    }

    #[inline]
    fn read_zigzag(&mut self) -> Result<i64, String> {
        let u = self.read_varint()?;
        Ok(((u >> 1) as i64) ^ (-((u & 1) as i64)))
    }

    #[inline]
    fn read_f64(&mut self) -> Result<f64, String> {
        if self.pos + 8 > self.bytes.len() {
            return Err("Unexpected EOF reading f64".to_string());
        }
        let arr: [u8; 8] = self.bytes[self.pos..self.pos + 8].try_into().unwrap();
        self.pos += 8;
        Ok(f64::from_le_bytes(arr))
    }

    #[inline]
    fn read_str(&mut self) -> Result<String, String> {
        let len = self.read_varint()? as usize;
        if self.pos + len > self.bytes.len() {
            return Err("Unexpected EOF reading string".to_string());
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + len])
            .map_err(|e| format!("Invalid UTF-8: {e}"))?
            .to_string();
        self.pos += len;
        Ok(s)
    }

    #[inline]
    fn read_opt_str(&mut self) -> Result<Option<String>, String> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_str()?)),
            other => Err(format!("Invalid option tag {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_roundtrip() {
        let mut fields = BTreeMap::new();
        fields.insert("category".to_string(), FieldValue::Text("potions".to_string()));
        fields.insert("dose".to_string(), FieldValue::Number(4.0));
        fields.insert("ratio".to_string(), FieldValue::Number(1.25));
        fields.insert("tradeable".to_string(), FieldValue::Bool(true));
        fields.insert("deletedFlag".to_string(), FieldValue::Bool(false));
        fields.insert(
            "roles".to_string(),
            FieldValue::List(vec!["pvm".to_string(), "skilling".to_string()]),
        );

        let snap = SerializedSnapshot {
            revision: "abc123".to_string(),
            source: CatalogSourceInfo {
                openrs2_cache_id: 2450,
                build: "951.1".to_string(),
                timestamp: Some("2026-11-20T00:00:00Z".to_string()),
                archive19_revision: 123,
                overlay_applied: true,
                overlay_matched: 1,
                total_items: 1,
                eligible_items: 1,
            },
            categories: vec![CategoryMeta {
                key: "potions".to_string(),
                name: "Potions".to_string(),
            }],
            field_defs: vec![FieldMeta {
                key: "dose".to_string(),
                label: "Dose".to_string(),
                field_type: "number".to_string(),
                multiple: false,
            }],
            items: vec![CatalogItem {
                id: 15332,
                config_name: "OVERLOAD_4".to_string(),
                display_name: "Overload (4)".to_string(),
                normalized_name: "overload 4".to_string(),
                bankable: true,
                deleted: false,
                fields,
            }],
        };

        let encoded = encode_snapshot(&snap);
        let decoded = decode_snapshot(&encoded).unwrap();
        assert_eq!(decoded.revision, snap.revision);
        assert_eq!(decoded.items.len(), 1);
        assert_eq!(decoded.items[0].config_name, "OVERLOAD_4");
        assert_eq!(decoded.items[0].fields, snap.items[0].fields);
    }
}
