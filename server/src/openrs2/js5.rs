use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, bail};
use bzip2_rs::DecoderReader as Bz2Decoder;
use flate2::read::GzDecoder;

/// Decompresses a JS5 container (compression type 0 = raw, 1 = bzip2, 2 = gzip, 3 = lzma).
pub fn decompress_container(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 5 {
        bail!("JS5 container too short ({} bytes)", data.len());
    }
    let compression = data[0];
    let compressed_len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;

    match compression {
        0 => {
            let end = 5 + compressed_len;
            if data.len() < end {
                bail!("Truncated uncompressed JS5 container: {} < {}", data.len(), end);
            }
            Ok(data[5..end].to_vec())
        }
        1 => {
            if data.len() < 9 + compressed_len {
                bail!("Truncated bzip2 JS5 container");
            }
            let uncompressed_len =
                u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
            let mut prefixed = Vec::with_capacity(4 + compressed_len);
            prefixed.extend_from_slice(b"BZh1");
            prefixed.extend_from_slice(&data[9..9 + compressed_len]);
            let mut decoder = Bz2Decoder::new(&prefixed[..]);
            let mut out = Vec::with_capacity(uncompressed_len);
            decoder
                .read_to_end(&mut out)
                .context("bzip2 decompression failed")?;
            Ok(out)
        }
        2 => {
            if data.len() < 9 + compressed_len {
                bail!("Truncated gzip JS5 container");
            }
            let uncompressed_len =
                u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
            let mut decoder = GzDecoder::new(&data[9..9 + compressed_len]);
            let mut out = Vec::with_capacity(uncompressed_len);
            decoder
                .read_to_end(&mut out)
                .context("gzip decompression failed")?;
            Ok(out)
        }
        3 => {
            if data.len() < 9 + compressed_len {
                bail!("Truncated lzma JS5 container");
            }
            let uncompressed_len =
                u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
            let payload = &data[9..9 + compressed_len];
            // JS5 LZMA stores 5-byte props + raw stream without the 8-byte uncompressed size header.
            if payload.len() < 5 {
                bail!("LZMA JS5 payload shorter than 5-byte properties");
            }
            let mut headered = Vec::with_capacity(payload.len() + 8);
            headered.extend_from_slice(&payload[..5]);
            headered.extend_from_slice(&(uncompressed_len as u64).to_le_bytes());
            headered.extend_from_slice(&payload[5..]);
            let mut out = Vec::with_capacity(uncompressed_len);
            let mut cursor = std::io::Cursor::new(&headered);
            lzma_rs::lzma_decompress(&mut cursor, &mut out)
                .context("lzma decompression failed")?;
            Ok(out)
        }
        other => bail!("Unsupported JS5 compression type {other}"),
    }
}

/// Parsed JS5 reference table (supports formats 5, 6, and RS3 NXT format 7).
#[derive(Debug, Clone)]
pub struct ReferenceTable {
    #[allow(dead_code)]
    pub format: u8,
    pub revision: u32,
    #[allow(dead_code)]
    pub flags: u8,
    pub groups: BTreeMap<u32, Vec<u32>>,
}

impl ReferenceTable {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(buf);
        let format = cursor.read_u8()?;
        let revision = if format >= 6 { cursor.read_u32()? } else { 0 };
        let flags = cursor.read_u8()?;

        let read_id = |c: &mut Cursor<'_>| -> Result<u32> {
            if format >= 7 {
                c.read_big_smart_u32()
            } else {
                Ok(c.read_u16()? as u32)
            }
        };

        let group_count = read_id(&mut cursor)? as usize;
        let mut group_ids = Vec::with_capacity(group_count);
        let mut acc = 0u32;
        for _ in 0..group_count {
            acc = acc
                .checked_add(read_id(&mut cursor)?)
                .context("group id overflow")?;
            group_ids.push(acc);
        }

        // Optional group name hashes (0x01)
        if flags & 0x01 != 0 {
            cursor.skip(4 * group_count)?;
        }
        // Group CRCs (always present)
        cursor.skip(4 * group_count)?;
        // Uncompressed CRC/hash (0x08)
        if flags & 0x08 != 0 {
            cursor.skip(4 * group_count)?;
        }
        // Whirlpool digests (0x02)
        if flags & 0x02 != 0 {
            cursor.skip(64 * group_count)?;
        }
        // Uncompressed + compressed sizes (0x04)
        if flags & 0x04 != 0 {
            cursor.skip(8 * group_count)?;
        }
        // Group versions (always present)
        cursor.skip(4 * group_count)?;

        let mut file_counts = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            file_counts.push(read_id(&mut cursor)? as usize);
        }

        let mut groups = BTreeMap::new();
        for (gid, fc) in group_ids.into_iter().zip(file_counts.into_iter()) {
            let mut file_ids = Vec::with_capacity(fc);
            let mut facc = 0u32;
            for _ in 0..fc {
                facc = facc
                    .checked_add(read_id(&mut cursor)?)
                    .context("file id overflow")?;
                file_ids.push(facc);
            }
            groups.insert(gid, file_ids);
        }

        Ok(Self {
            format,
            revision,
            flags,
            groups,
        })
    }
}

/// Splits a decompressed multi-file JS5 group into its constituent files.
pub fn split_group(buf: &[u8], file_ids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    if file_ids.len() == 1 {
        return Ok(vec![(file_ids[0], buf.to_vec())]);
    }
    if buf.is_empty() {
        bail!("Empty multi-file JS5 group");
    }

    let chunks = *buf.last().unwrap() as usize;
    let n = file_ids.len();
    let table_bytes = chunks
        .checked_mul(n)
        .and_then(|v| v.checked_mul(4))
        .context("JS5 chunk table size overflow")?;
    if buf.len() < table_bytes + 1 {
        bail!(
            "JS5 group buffer ({}) smaller than chunk table ({})",
            buf.len(),
            table_bytes + 1
        );
    }

    let table_start = buf.len() - 1 - table_bytes;
    let mut chunk_sizes = vec![vec![0usize; n]; chunks];
    let mut total_sizes = vec![0usize; n];
    let mut pos = table_start;
    for chunk in &mut chunk_sizes {
        let mut acc: i32 = 0;
        for (i, slot) in chunk.iter_mut().enumerate() {
            let delta = i32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
            pos += 4;
            acc = acc.checked_add(delta).context("chunk size delta overflow")?;
            if acc < 0 {
                bail!("Negative JS5 chunk size ({acc})");
            }
            *slot = acc as usize;
            total_sizes[i] += *slot;
        }
    }

    let mut files: Vec<Vec<u8>> = total_sizes.into_iter().map(Vec::with_capacity).collect();
    let mut read_pos = 0usize;
    for chunk in &chunk_sizes {
        for (i, &sz) in chunk.iter().enumerate() {
            let end = read_pos + sz;
            if end > table_start {
                bail!(
                    "JS5 group file data overruns chunk table ({end} > {table_start})"
                );
            }
            files[i].extend_from_slice(&buf[read_pos..end]);
            read_pos = end;
        }
    }

    Ok(file_ids.iter().copied().zip(files).collect())
}

/// Parses an RS3 Archive 67 version-1 name table (e.g. group 36 = config.Obj, group 37 = config.Param).
///
/// Format:
/// - `u32 BE`: version (must be 1)
/// - `u32 BE`: entry count (`id` 0..count-1)
/// - `count * i32 BE`: byte offset into null-terminated UTF-8/CP1252 string pool (-1 = absent)
/// - remaining bytes: null-terminated string pool
pub fn parse_archive67_name_table(buf: &[u8]) -> Result<BTreeMap<u32, String>> {
    if buf.len() < 8 {
        bail!("Archive 67 name table too short");
    }
    let version = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let count = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if version != 1 {
        bail!("Unsupported Archive 67 name table version {version} (expected 1)");
    }
    let pool_start = 8usize
        .checked_add(count.checked_mul(4).context("table count overflow")?)
        .context("pool start overflow")?;
    if buf.len() < pool_start {
        bail!("Archive 67 table truncated before string pool");
    }

    let pool = &buf[pool_start..];
    let mut out = BTreeMap::new();
    for id in 0..count {
        let o = 8 + id * 4;
        let offset = i32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        if offset < 0 {
            continue;
        }
        let start = offset as usize;
        if start >= pool.len() {
            bail!("Archive 67 string offset {start} out of bounds ({})", pool.len());
        }
        let rel_end = pool[start..]
            .iter()
            .position(|&b| b == 0)
            .context("Unterminated string in Archive 67 pool")?;
        let text = String::from_utf8_lossy(&pool[start..start + rel_end]).into_owned();
        if !text.is_empty() {
            out.insert(id as u32, text);
        }
    }
    Ok(out)
}

/// Big-endian binary reader helper shared across JS5 & item definition decoders.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        let next = self.pos.checked_add(n).context("cursor overflow")?;
        if next > self.buf.len() {
            bail!("Unexpected EOF skipping {n} bytes at {}/{}", self.pos, self.buf.len());
        }
        self.pos = next;
        Ok(())
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.buf.len() {
            bail!("Unexpected EOF reading u8 at {}", self.pos);
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        if self.pos + 2 > self.buf.len() {
            bail!("Unexpected EOF reading u16 at {}", self.pos);
        }
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u24(&mut self) -> Result<u32> {
        if self.pos + 3 > self.buf.len() {
            bail!("Unexpected EOF reading u24 at {}", self.pos);
        }
        let v = ((self.buf[self.pos] as u32) << 16)
            | ((self.buf[self.pos + 1] as u32) << 8)
            | (self.buf[self.pos + 2] as u32);
        self.pos += 3;
        Ok(v)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        if self.pos + 4 > self.buf.len() {
            bail!("Unexpected EOF reading u32 at {}", self.pos);
        }
        let v = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        if self.pos + 8 > self.buf.len() {
            bail!("Unexpected EOF reading i64 at {}", self.pos);
        }
        let v = i64::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
            self.buf[self.pos + 4],
            self.buf[self.pos + 5],
            self.buf[self.pos + 6],
            self.buf[self.pos + 7],
        ]);
        self.pos += 8;
        Ok(v)
    }

    pub fn read_big_smart_u32(&mut self) -> Result<u32> {
        if self.pos >= self.buf.len() {
            bail!("Unexpected EOF reading big_smart at {}", self.pos);
        }
        if self.buf[self.pos] & 0x80 != 0 {
            Ok(self.read_u32()? & 0x7FFF_FFFF)
        } else {
            Ok(self.read_u16()? as u32)
        }
    }

    pub fn read_big_smart_opt(&mut self) -> Result<Option<u32>> {
        if self.pos >= self.buf.len() {
            bail!("Unexpected EOF reading big_smart_opt at {}", self.pos);
        }
        if self.buf[self.pos] & 0x80 != 0 {
            Ok(Some(self.read_u32()? & 0x7FFF_FFFF))
        } else {
            let v = self.read_u16()?;
            if v == 0x7FFF {
                Ok(None)
            } else {
                Ok(Some(v as u32))
            }
        }
    }

    /// Reads a null-terminated Jagex CP-1252 string.
    pub fn read_jagex_string(&mut self) -> Result<String> {
        let start = self.pos;
        let rel_end = self.buf[start..]
            .iter()
            .position(|&b| b == 0)
            .context("Unterminated Jagex string")?;
        let slice = &self.buf[start..start + rel_end];
        self.pos = start + rel_end + 1;
        Ok(decode_cp1252(slice))
    }
}

fn decode_cp1252(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        return std::str::from_utf8(bytes).unwrap_or_default().to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        let ch = match b {
            0x80 => '\u{20AC}',
            0x82 => '\u{201A}',
            0x83 => '\u{0192}',
            0x84 => '\u{201E}',
            0x85 => '\u{2026}',
            0x86 => '\u{2020}',
            0x87 => '\u{2021}',
            0x88 => '\u{02C6}',
            0x89 => '\u{2030}',
            0x8A => '\u{0160}',
            0x8B => '\u{2039}',
            0x8C => '\u{0152}',
            0x8E => '\u{017D}',
            0x91 => '\u{2018}',
            0x92 => '\u{2019}',
            0x93 => '\u{201C}',
            0x94 => '\u{201D}',
            0x95 => '\u{2022}',
            0x96 => '\u{2013}',
            0x97 => '\u{2014}',
            0x98 => '\u{02DC}',
            0x99 => '\u{2122}',
            0x9A => '\u{0161}',
            0x9B => '\u{203A}',
            0x9C => '\u{0153}',
            0x9E => '\u{017E}',
            0x9F => '\u{0178}',
            other => other as char,
        };
        out.push(ch);
    }
    out
}
