use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "runeql-server",
    about = "In-memory RuneQL query server built from OpenRS2 RS3 client caches"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Address to bind the HTTP server to
    #[arg(long, default_value = "127.0.0.1:8787", global = true)]
    pub bind: String,

    /// Specific OpenRS2 cache ID to load (defaults to latest live English RS3 cache)
    #[arg(long, global = true)]
    pub cache_id: Option<u32>,

    /// OpenRS2 base URL
    #[arg(long, default_value = "https://archive.openrs2.org", global = true)]
    pub openrs2_url: String,

    /// Directory to cache downloaded OpenRS2 archives locally between runs
    #[arg(long, default_value = ".cache/openrs2", global = true)]
    pub cache_dir: PathBuf,

    /// Optional path to RuneQL's resources/catalog directory for curated attribute overlays
    #[arg(long, global = true)]
    pub overlay_dir: Option<PathBuf>,

    /// Disable automatic discovery of ../resources/catalog curated overlay
    #[arg(long, default_value_t = false, global = true)]
    pub no_overlay: bool,

    /// Max concurrent HTTP downloads when fetching OpenRS2 groups
    #[arg(long, default_value_t = 12, global = true)]
    pub concurrency: usize,

    /// Optional cap on matching configNames returned by /v1/eval (0 = unlimited)
    #[arg(long, default_value_t = 0, global = true)]
    pub max_eval_results: usize,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch/build the OpenRS2 catalog snapshot and start the HTTP server (default)
    Serve,
    /// Fetch/build the OpenRS2 catalog snapshot, print stats, and exit
    Build,
    /// Run a single RuneQL query locally against the OpenRS2 catalog and print results
    Query {
        /// RuneQL query string
        query: String,
        /// Maximum number of results to display
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Include non-bankable and deleted items
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Export a pre-synthesized gzipped catalog snapshot for Cloudflare Workers edge deployment
    ExportSnapshot {
        /// Output file path (e.g. ../worker/src/catalog.bin.gz)
        #[arg(long)]
        out: PathBuf,
    },
}

/// Hard safety limits enforced on every remote query to prevent CPU/memory DoS and ReDoS.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryLimits {
    pub max_query_bytes: usize,
    pub max_clauses: usize,
    pub max_nesting_depth: usize,
    pub max_expensive_clauses: usize,
    pub max_regex_length: usize,
    pub max_regex_depth: usize,
    pub max_regex_repeat: usize,
    pub max_regex_steps: usize,
    pub max_eval_steps: usize,
    pub max_eval_ms: u64,
    pub max_eval_results: usize,
    pub max_search_limit: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_query_bytes: 4096,
            max_clauses: 128,
            max_nesting_depth: 8,
            max_expensive_clauses: 6,
            max_regex_length: 128,
            max_regex_depth: 8,
            max_regex_repeat: 64,
            max_regex_steps: 25_000,
            max_eval_steps: 500_000,
            max_eval_ms: 50,
            max_eval_results: 0,
            max_search_limit: 200,
        }
    }
}

impl QueryLimits {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.max_eval_ms)
    }
}
