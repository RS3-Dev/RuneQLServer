mod config;
mod engine;
mod http;
mod openrs2;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::{Cli, Command, QueryLimits};
use crate::engine::index::CatalogSnapshot;
use crate::http::handlers::AppState;
use crate::openrs2::OpenRs2Client;
use crate::openrs2::synthesize::{default_categories, default_field_defs, synthesize_catalog};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let t0 = Instant::now();

    let overlay_dir = resolve_overlay_dir(&cli);
    let client = OpenRs2Client::new(cli.openrs2_url.clone(), cli.cache_dir.clone(), cli.concurrency)?;
    let raw = client.load_rs3_items(cli.cache_id).await?;

    let (items, revision, source) = synthesize_catalog(&raw, overlay_dir.as_deref())?;
    let snapshot = Arc::new(CatalogSnapshot::build(
        items,
        revision,
        source,
        default_categories(),
        default_field_defs(),
    ));

    let limits = QueryLimits {
        max_eval_results: cli.max_eval_results,
        ..QueryLimits::default()
    };

    info!(
        elapsed_ms = t0.elapsed().as_millis(),
        cache_id = snapshot.source.openrs2_cache_id,
        build = %snapshot.source.build,
        total_items = snapshot.source.total_items,
        eligible_items = snapshot.source.eligible_items,
        overlay = snapshot.source.overlay_applied,
        overlay_matched = snapshot.source.overlay_matched,
        revision = %&snapshot.revision[..16],
        cache_dir = %client.cache_dir().display(),
        "RuneQL in-memory index ready"
    );

    match cli.command.unwrap_or(Command::Serve) {
        Command::Build => {
            println!("OpenRS2 Cache ID : {}", snapshot.source.openrs2_cache_id);
            println!("RS3 Build        : {}", snapshot.source.build);
            println!(
                "Timestamp        : {}",
                snapshot.source.timestamp.as_deref().unwrap_or("unknown")
            );
            println!("Catalog Revision : {}", snapshot.revision);
            println!("Total Items      : {}", snapshot.source.total_items);
            println!("Eligible Items   : {}", snapshot.source.eligible_items);
            println!(
                "Curated Overlay  : {} ({} matched)",
                snapshot.source.overlay_applied, snapshot.source.overlay_matched
            );
            Ok(())
        }
        Command::Query { query, limit, all } => {
            let qt = Instant::now();
            let (hits, total, canonical, sort) =
                snapshot.search(&query, limit, 0, !all, &limits)?;
            let us = qt.elapsed().as_micros();
            println!("Query     : {query}");
            println!("Canonical : {canonical}");
            if !sort.is_empty() {
                let s: Vec<String> = sort
                    .iter()
                    .map(|r| format!("{}:{:?}", r.field, r.direction).to_ascii_lowercase())
                    .collect();
                println!("Sort      : {}", s.join(", "));
            }
            println!("Matches   : {total} (showing {}, evaluated in {us} µs)", hits.len());
            for hit in hits {
                let cat = hit
                    .item
                    .fields
                    .get("category")
                    .and_then(|v| v.as_strings().into_iter().next())
                    .unwrap_or_default();
                println!(
                    "  {:>6.1}  {:<42}  {:<32}  [id={}, cat={}]",
                    hit.score, hit.item.config_name, hit.item.display_name, hit.item.id, cat
                );
            }
            Ok(())
        }
        Command::ExportSnapshot { out } => {
            use flate2::{Compression, write::GzEncoder};
            use std::io::Write;
            if let Some(parent) = out.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let payload = engine::SerializedSnapshot {
                revision: snapshot.revision.clone(),
                source: snapshot.source.clone(),
                categories: snapshot.categories.clone(),
                field_defs: snapshot.field_defs.clone(),
                items: snapshot.items.clone(),
            };
            let raw_bytes = engine::snapshot_codec::encode_snapshot(&payload);
            let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(&raw_bytes)?;
            let gz_bytes = encoder.finish()?;
            std::fs::write(&out, &gz_bytes)?;
            println!(
                "Exported RQL1 snapshot to {} (raw={:.2} MB, gzip={:.2} MB, items={}, eligible={}, rev={})",
                out.display(),
                raw_bytes.len() as f64 / (1024.0 * 1024.0),
                gz_bytes.len() as f64 / (1024.0 * 1024.0),
                payload.source.total_items,
                payload.source.eligible_items,
                &payload.revision[..16]
            );
            Ok(())
        }
        Command::Serve => {
            let state = Arc::new(AppState {
                snapshot,
                limits,
                started_at: Instant::now(),
                requests_served: AtomicU64::new(0),
            });
            let router = http::build_router(state);
            let listener = TcpListener::bind(&cli.bind).await?;
            info!(
                "Listening on http://{} (open in browser for interactive workbench)",
                cli.bind
            );
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            Ok(())
        }
    }
}

fn resolve_overlay_dir(cli: &Cli) -> Option<PathBuf> {
    if cli.no_overlay {
        return None;
    }
    if let Some(ref custom) = cli.overlay_dir {
        return Some(custom.clone());
    }
    for rel in [
        "../overlay",
        "overlay",
        "../resources/catalog",
        "../../runeql/resources/catalog",
        "resources/catalog",
    ] {
        let candidate = PathBuf::from(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("Shutdown signal received, exiting cleanly");
}
