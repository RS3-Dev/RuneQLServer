pub mod item_def;
pub mod js5;
pub mod synthesize;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::openrs2::item_def::ItemDef;
use crate::openrs2::js5::{
    ReferenceTable, decompress_container, parse_archive67_name_table, split_group,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenRs2Build {
    pub major: Option<u32>,
    pub minor: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenRs2CacheMeta {
    pub id: u32,
    pub scope: String,
    pub game: String,
    pub environment: String,
    pub language: Option<String>,
    pub builds: Vec<OpenRs2Build>,
    pub timestamp: Option<String>,
    pub valid_groups: Option<u64>,
    pub groups: Option<u64>,
}

impl OpenRs2CacheMeta {
    pub fn build_label(&self) -> String {
        self.builds
            .first()
            .map(|b| match (b.major, b.minor) {
                (Some(maj), Some(min)) => format!("{maj}.{min}"),
                (Some(maj), None) => format!("{maj}"),
                _ => "unknown".to_string(),
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Raw decoded RS3 data pulled from OpenRS2 (Archive 19 + Archive 67 groups 36 & 37).
pub struct RawRs3CacheData {
    pub meta: OpenRs2CacheMeta,
    pub archive19_revision: u32,
    pub items: BTreeMap<u32, ItemDef>,
    pub obj_config_names: BTreeMap<u32, String>,
    pub param_names: BTreeMap<u32, String>,
}

pub struct OpenRs2Client {
    http: Client,
    base_url: String,
    cache_dir: PathBuf,
    concurrency: usize,
}

impl OpenRs2Client {
    pub fn new(base_url: String, cache_dir: PathBuf, concurrency: usize) -> Result<Self> {
        let http = Client::builder()
            .user_agent("RuneQL-Server/0.1 (plugins@rs3.dev)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to initialize HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            cache_dir,
            concurrency: concurrency.max(1).min(32),
        })
    }

    /// Resolves metadata for either a specific cache ID or the latest live English RS3 cache.
    pub async fn resolve_cache_meta(&self, requested_id: Option<u32>) -> Result<OpenRs2CacheMeta> {
        let meta_path = self.cache_dir.join("caches.json");
        let body = match self.fetch_caches_json().await {
            Ok(bytes) => {
                if let Some(parent) = meta_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                let _ = tokio::fs::write(&meta_path, &bytes).await;
                bytes
            }
            Err(err) => {
                if meta_path.exists() {
                    info!("OpenRS2 unreachable ({err}); falling back to cached caches.json");
                    tokio::fs::read(&meta_path)
                        .await
                        .context("Failed reading cached caches.json")?
                } else {
                    return Err(err);
                }
            }
        };

        let caches: Vec<OpenRs2CacheMeta> =
            serde_json::from_slice(&body).context("Failed to parse OpenRS2 caches.json")?;

        if let Some(id) = requested_id {
            return caches
                .into_iter()
                .find(|c| c.id == id && c.scope == "runescape")
                .ok_or_else(|| anyhow!("OpenRS2 cache id {id} (scope=runescape) not found"));
        }

        let mut candidates: Vec<OpenRs2CacheMeta> = caches
            .into_iter()
            .filter(|c| {
                c.scope == "runescape"
                    && c.game == "runescape"
                    && c.environment == "live"
                    && c.language.as_deref() == Some("en")
                    && c.valid_groups.unwrap_or(0) > 100_000
            })
            .collect();

        candidates.sort_by(|a, b| {
            b.timestamp
                .as_deref()
                .unwrap_or("")
                .cmp(a.timestamp.as_deref().unwrap_or(""))
                .then_with(|| b.id.cmp(&a.id))
        });

        candidates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No live English RS3 cache found in OpenRS2 caches.json"))
    }

    async fn fetch_caches_json(&self) -> Result<Vec<u8>> {
        let url = format!("{}/caches.json", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        if !resp.status().is_success() {
            bail!("GET {url} returned HTTP {}", resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Downloads (or reads from disk cache) a single JS5 group from OpenRS2.
    async fn fetch_group_cached(
        &self,
        scope: &str,
        cache_id: u32,
        archive: u32,
        group: u32,
    ) -> Result<Vec<u8>> {
        let local_path = self
            .cache_dir
            .join(format!("{scope}-{cache_id}"))
            .join(format!("a{archive}"))
            .join(format!("g{group}.dat"));

        if let Ok(existing) = tokio::fs::read(&local_path).await {
            return Ok(existing);
        }

        let url = format!(
            "{}/caches/{scope}/{cache_id}/archives/{archive}/groups/{group}.dat",
            self.base_url
        );

        let mut last_err = None;
        for attempt in 0..3 {
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let bytes = resp.bytes().await?.to_vec();
                    if let Some(parent) = local_path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let _ = tokio::fs::write(&local_path, &bytes).await;
                    return Ok(bytes);
                }
                Ok(resp) => {
                    last_err = Some(anyhow!("GET {url} returned HTTP {}", resp.status()));
                }
                Err(e) => {
                    last_err = Some(anyhow!("GET {url} error: {e}"));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250 * (attempt + 1))).await;
        }
        Err(last_err.unwrap_or_else(|| anyhow!("Failed fetching {url}")))
    }

    /// Loads Archive 19 items + Archive 67 object/param config names for the selected cache.
    pub async fn load_rs3_items(&self, requested_id: Option<u32>) -> Result<RawRs3CacheData> {
        let meta = self.resolve_cache_meta(requested_id).await?;
        let scope = meta.scope.clone();
        let cid = meta.id;

        info!(
            cache_id = cid,
            build = %meta.build_label(),
            timestamp = %meta.timestamp.as_deref().unwrap_or("unknown"),
            "Loading RS3 cache from OpenRS2 (cached under {})",
            self.cache_dir.display()
        );

        // Fetch reference table for Archive 19 + Archive 67 obj & param name tables concurrently.
        let (rt19_raw, obj_names_raw, param_names_raw) = tokio::try_join!(
            self.fetch_group_cached(&scope, cid, 255, 19),
            self.fetch_group_cached(&scope, cid, 67, 36),
            self.fetch_group_cached(&scope, cid, 67, 37),
        )?;

        let rt19 = ReferenceTable::parse(&decompress_container(&rt19_raw)?)
            .context("Failed parsing Archive 19 reference table")?;
        let obj_config_names = parse_archive67_name_table(&decompress_container(&obj_names_raw)?)
            .context("Failed parsing Archive 67 group 36 (config.Obj names)")?;
        let param_names = parse_archive67_name_table(&decompress_container(&param_names_raw)?)
            .context("Failed parsing Archive 67 group 37 (config.Param names)")?;

        info!(
            groups = rt19.groups.len(),
            obj_names = obj_config_names.len(),
            param_names = param_names.len(),
            "Fetched Archive 19 reference table and Archive 67 config name tables"
        );

        let client = Arc::new(self.clone_shallow());
        let scope_arc = Arc::new(scope);
        let group_entries: Vec<(u32, Vec<u32>)> = rt19.groups.into_iter().collect();

        let decoded_groups: Vec<Vec<ItemDef>> = stream::iter(group_entries)
            .map(|(gid, file_ids)| {
                let client = Arc::clone(&client);
                let scope = Arc::clone(&scope_arc);
                async move {
                    let raw = client.fetch_group_cached(&scope, cid, 19, gid).await?;
                    let decompressed = decompress_container(&raw)
                        .with_context(|| format!("Decompressing Archive 19 group {gid}"))?;
                    let files = split_group(&decompressed, &file_ids)
                        .with_context(|| format!("Splitting Archive 19 group {gid}"))?;
                    let mut group_items = Vec::with_capacity(files.len());
                    for (fid, bytes) in files {
                        let item_id = (gid << 8) | fid;
                        let def = ItemDef::decode(item_id, &bytes)
                            .with_context(|| format!("Decoding item {item_id}"))?;
                        group_items.push(def);
                    }
                    Ok::<Vec<ItemDef>, anyhow::Error>(group_items)
                }
            })
            .buffer_unordered(self.concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        let mut items = BTreeMap::new();
        for group in decoded_groups {
            for def in group {
                items.insert(def.id, def);
            }
        }

        info!(decoded_items = items.len(), "Decoded all RS3 item definitions");

        Ok(RawRs3CacheData {
            meta,
            archive19_revision: rt19.revision,
            items,
            obj_config_names,
            param_names,
        })
    }

    fn clone_shallow(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            cache_dir: self.cache_dir.clone(),
            concurrency: self.concurrency,
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}
