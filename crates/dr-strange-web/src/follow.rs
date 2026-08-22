//! `serve --follow` (arch/01 §9): the client half of raw-WAL replication —
//! bootstrap from the master's `GET /snapshot`, then tail its `/ws/wal`.
//!
//! Every (re)connect does a full resync from scratch, by design: no
//! partial/bounded-WAL catch-up. The WebSocket subscribes *before* the
//! snapshot is pulled — the same ordering `/ws`'s `ChangeSet` feed already
//! uses for `plane.watch` — so nothing committed on the master during the
//! pull is ever missed: any batch that arrives before the snapshot's own
//! commit sequence is already reflected in it and is simply skipped.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use dr_strange_core::{Database, ReplicatedBatch, SnapshotStats};
use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::FollowOptions;

/// Generous: a snapshot is a whole-database dump, buffered fully in memory on
/// both ends (matching `/export`'s existing convention) before this project
/// needs chunked transfer.
const SNAPSHOT_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

fn token_query(token: &Option<String>) -> String {
    match token {
        Some(t) => format!(
            "?token={}",
            url::form_urlencoded::byte_serialize(t.as_bytes()).collect::<String>()
        ),
        None => String::new(),
    }
}

fn ws_url(opts: &FollowOptions) -> String {
    format!("{}/ws/wal{}", opts.upstream, token_query(&opts.token))
}

fn snapshot_url(opts: &FollowOptions) -> String {
    let http_base = opts
        .upstream
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    format!("{http_base}/snapshot")
}

/// The result of a successful bootstrap: the snapshot's cutover sequence, and
/// the still-open channel of subsequent live batches — reading from it *is*
/// the live tail; it closes when the WAL stream is lost.
pub struct Bootstrapped {
    pub stats: SnapshotStats,
    pub batches: mpsc::UnboundedReceiver<ReplicatedBatch>,
}

/// Connect to `opts.upstream`, pull a full snapshot into `db` (which must be
/// empty), and return the live tail past that snapshot's cutover.
pub async fn bootstrap(db: &Database, opts: &FollowOptions) -> anyhow::Result<Bootstrapped> {
    let (ws, _resp) = tokio_tungstenite::connect_async(ws_url(opts))
        .await
        .with_context(|| format!("connecting to {} for WAL replication", opts.upstream))?;
    // Subscribed now — every commit the master makes from this instant on is
    // captured in `raw_rx`, even the ones that land before the snapshot pull
    // below finishes. Read-only; the client never sends anything post-upgrade.
    let (_write, mut read) = ws.split();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<ReplicatedBatch>();
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => match postcard::from_bytes(&bytes) {
                    Ok(batch) => {
                        if raw_tx.send(batch).is_err() {
                            break; // nobody's consuming anymore
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "dropping malformed WAL replication message")
                    }
                },
                Ok(Message::Close(_)) => break,
                Ok(_) => {} // ping/pong/text — nothing else rides this socket
                Err(e) => {
                    tracing::warn!(error = %e, "WAL replication stream error");
                    break;
                }
            }
        }
        // `raw_tx` drops here, closing the channel — how the live-tail loop
        // in `server::run` learns the connection is gone.
    });

    let bytes = fetch_snapshot(opts).await?;
    // `block_in_place` (not `spawn_blocking`) so `db: &Database` needs no
    // `'static` bound — it runs inline on the current worker thread, which
    // is fine on the multi-thread runtime `dr_strange_web::serve` builds.
    let stats = tokio::task::block_in_place(|| db.restore(&mut bytes.as_slice()))
        .context("restoring the master's snapshot")?;

    // Drain whatever arrived during the pull: already-covered commits are
    // skipped, anything past the snapshot's cutover is exactly the start of
    // the live tail, so hand it straight through on the same channel.
    let (tail_tx, tail_rx) = mpsc::unbounded_channel();
    while let Ok(batch) = raw_rx.try_recv() {
        if batch.seq > stats.seq {
            let _ = tail_tx.send(batch);
        }
    }
    // Keep forwarding everything from here on.
    tokio::spawn(async move {
        while let Some(batch) = raw_rx.recv().await {
            if tail_tx.send(batch).is_err() {
                break;
            }
        }
    });

    Ok(Bootstrapped {
        stats,
        batches: tail_rx,
    })
}

async fn fetch_snapshot(opts: &FollowOptions) -> anyhow::Result<Vec<u8>> {
    let url = snapshot_url(opts);
    let token = opts.token.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let mut req = ureq::get(&url).timeout(SNAPSHOT_FETCH_TIMEOUT);
        if let Some(t) = &token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let mut buf = Vec::new();
        req.call()
            .with_context(|| format!("GET {url}"))?
            .into_reader()
            .read_to_end(&mut buf)?;
        Ok(buf)
    })
    .await?
}

/// Consume `batches` (a [`Bootstrapped::batches`]) forever, applying each to
/// `db` in order. Returns (rather than erroring) once the channel closes —
/// that's an ordinary "the stream was lost", not a bug — and sets
/// `resync_needed` + wakes `follow_lost` so `server::run` tears down and the
/// CLI's outer loop resyncs from scratch.
pub async fn run_live_tail(
    db: Arc<Database>,
    mut batches: mpsc::UnboundedReceiver<ReplicatedBatch>,
    resync_needed: Arc<AtomicBool>,
    follow_lost: Arc<Notify>,
) {
    while let Some(batch) = batches.recv().await {
        let db = db.clone();
        let seq = batch.seq;
        let applied = tokio::task::spawn_blocking(move || db.apply_replicated(batch)).await;
        match applied {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(seq, error = %e, "failed to apply a replicated batch; resyncing");
                break;
            }
            Err(e) => {
                tracing::error!(seq, error = %e, "apply_replicated task panicked; resyncing");
                break;
            }
        }
    }
    tracing::warn!("WAL replication stream ended; a full resync is needed");
    resync_needed.store(true, Ordering::Relaxed);
    follow_lost.notify_one();
}
