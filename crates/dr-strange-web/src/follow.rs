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
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::FollowOptions;

/// Generous: a snapshot is a whole-database dump. The master streams it frame
/// by frame; this end still reads the whole body before restoring, since
/// `Database::restore` refuses a non-empty target and a half-applied
/// snapshot would be one.
const SNAPSHOT_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// How many replicated batches may wait between the socket and the apply
/// loop. Bounded, because the master writes at its own pace and this end
/// applies at its own: on a follower that falls behind, an unbounded queue
/// would hold every batch the master ever sent until memory ran out, when
/// the design's answer to falling behind is a full resync anyway. When the
/// queue is full the reader stops pulling from the socket, so the pressure
/// lands in the TCP window where the master's broadcast can lag the follower
/// out and force that resync — a bounded, visible failure. During the
/// snapshot pull, when nothing applies yet, the forwarder's
/// [`PULL_BACKLOG_OPS`] backlog sits between the two so an ordinary master's
/// pull-time commits do not fill this queue and lag the follower out before
/// it has even started.
pub const REPLICATION_QUEUE: usize = 1024;

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
    pub batches: mpsc::Receiver<ReplicatedBatch>,
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
    let (raw_tx, raw_rx) = mpsc::channel::<ReplicatedBatch>(REPLICATION_QUEUE);
    // The forwarder is the raw queue's only consumer from the first frame
    // on, so a busy master during the pull lands in its bounded backlog
    // rather than in a parked socket reader; see `forward_tail`.
    let (tail_tx, tail_rx) = mpsc::channel(REPLICATION_QUEUE);
    let (cutover_tx, cutover_rx) = oneshot::channel::<u64>();
    tokio::spawn(forward_tail(raw_rx, tail_tx, cutover_rx));
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => match postcard::from_bytes(&bytes) {
                    Ok(batch) => {
                        // Waits while the queue is full — see REPLICATION_QUEUE.
                        if raw_tx.send(batch).await.is_err() {
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

    // Tell the forwarder where the snapshot ends: batches at or below the
    // cutover are already in the restored image and are skipped; everything
    // past it is exactly the start of the live tail.
    let _ = cutover_tx.send(stats.seq);

    Ok(Bootstrapped {
        stats,
        batches: tail_rx,
    })
}

/// While the snapshot pull is in flight nothing reads the live tail, so
/// whatever the master commits meanwhile has to wait somewhere. The forwarder
/// holds it, bounded by this many ops in total (a batch costs its ops, not
/// its count) — the pull-time backlog an ordinary master produces fits, and
/// a master that outruns it stops being read, so the pressure lands in the
/// TCP window, its broadcast lags this follower out, and the CLI resyncs:
/// bounded memory, visible failure, no unbounded queue.
pub const PULL_BACKLOG_OPS: usize = 1 << 18;

/// Moves batches from the socket reader's `raw` queue to the `tail` the
/// apply loop reads, skipping those the snapshot already covers.
///
/// This is the raw queue's only consumer. Until `cutover` arrives (the
/// restored snapshot's sequence) it buffers what the reader delivers, up to
/// [`PULL_BACKLOG_OPS`], and reads nothing more past that; once the cutover
/// is known it hands the backlog past it to `tail` and forwards live from
/// then on, waiting on a full `tail` exactly as the reader waits on a full
/// `raw`. Nothing here ever sends into `tail` before someone reads it — the
/// earlier inline drain did, and with the reader refilling `raw` behind it
/// a backlog longer than the tail's capacity parked the bootstrap forever.
///
/// A dropped `cutover` (the bootstrap failed) ends the forwarder, closing
/// `tail`; a closed `raw` (the socket ended) still waits for the cutover so
/// a backlog past it is delivered before `tail` closes and the live tail
/// asks for a resync.
async fn forward_tail(
    mut raw: mpsc::Receiver<ReplicatedBatch>,
    tail: mpsc::Sender<ReplicatedBatch>,
    mut cutover: oneshot::Receiver<u64>,
) {
    let mut backlog: std::collections::VecDeque<ReplicatedBatch> =
        std::collections::VecDeque::new();
    let mut backlog_ops = 0usize;
    let mut raw_open = true;
    let floor = loop {
        tokio::select! {
            found = &mut cutover => match found {
                Ok(seq) => break seq,
                Err(_) => return,
            },
            next = raw.recv(), if raw_open && backlog_ops < PULL_BACKLOG_OPS => match next {
                Some(batch) => {
                    // An empty batch still counts one, so the bound is a
                    // bound whatever the master sends.
                    backlog_ops += batch.ops.len().max(1);
                    backlog.push_back(batch);
                }
                None => raw_open = false,
            },
        }
    };
    for batch in backlog {
        if batch.seq > floor && tail.send(batch).await.is_err() {
            return;
        }
    }
    while let Some(batch) = raw.recv().await {
        if batch.seq > floor && tail.send(batch).await.is_err() {
            return;
        }
    }
}

async fn fetch_snapshot(opts: &FollowOptions) -> anyhow::Result<Vec<u8>> {
    let url = snapshot_url(opts);
    let token = opts.token.clone();
    let net = opts.net.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        // Through an agent, not `ureq::get`: a bare call reads no proxy at
        // all, so a replica behind one could never bootstrap (issue #37).
        let parsed = url::Url::parse(&url).with_context(|| format!("parsing {url}"))?;
        let agent = net
            .apply(
                ureq::AgentBuilder::new(),
                crate::fetch::guard::destination(&parsed),
            )?
            .build();
        let mut req = agent.get(&url).timeout(SNAPSHOT_FETCH_TIMEOUT);
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
    mut batches: mpsc::Receiver<ReplicatedBatch>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The live tail reads from a bounded queue and, when the queue closes
    /// (the socket reader gave up), returns having asked for a resync —
    /// the ordinary end of a follow, not an error.
    #[tokio::test]
    async fn the_live_tail_ends_when_the_bounded_queue_closes() {
        let db = Arc::new(Database::in_memory().unwrap());
        let (tx, rx) = mpsc::channel::<ReplicatedBatch>(REPLICATION_QUEUE);
        let resync = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(Notify::new());
        drop(tx);
        run_live_tail(db, rx, resync.clone(), lost.clone()).await;
        assert!(resync.load(Ordering::Relaxed));
        // `notify_one` stores a permit, so a late waiter still wakes.
        tokio::time::timeout(Duration::from_secs(1), lost.notified())
            .await
            .expect("the loss is signalled");
    }

    /// More batches than the tail's capacity arrive before the cutover is
    /// known and nothing reads the tail meanwhile: the forwarder holds them,
    /// then delivers exactly those past the cutover once a reader appears.
    /// The inline drain this replaces parked on the tail's
    /// `REPLICATION_QUEUE + 1`th send under these inputs and never returned.
    #[tokio::test]
    async fn the_pull_time_backlog_is_delivered_past_the_cutover() {
        let (raw_tx, raw_rx) = mpsc::channel::<ReplicatedBatch>(REPLICATION_QUEUE);
        let (tail_tx, mut tail_rx) = mpsc::channel(REPLICATION_QUEUE);
        let (cutover_tx, cutover_rx) = oneshot::channel();
        let forwarder = tokio::spawn(forward_tail(raw_rx, tail_tx, cutover_rx));
        let total = 3 * REPLICATION_QUEUE as u64;
        for seq in 1..=total {
            // Awaited: the forwarder must keep the bounded raw queue moving
            // while the cutover is still unknown.
            tokio::time::timeout(
                Duration::from_secs(5),
                raw_tx.send(ReplicatedBatch {
                    seq,
                    ops: Vec::new(),
                }),
            )
            .await
            .expect("the raw queue keeps draining during the pull")
            .unwrap();
        }
        let cutover = REPLICATION_QUEUE as u64 + 7;
        cutover_tx.send(cutover).unwrap();
        let mut expect = cutover + 1;
        while expect <= total {
            let batch = tokio::time::timeout(Duration::from_secs(5), tail_rx.recv())
                .await
                .expect("the tail delivers the backlog")
                .expect("the tail is open");
            assert_eq!(batch.seq, expect);
            expect += 1;
        }
        // Live from here on, and the tail closes with the socket.
        raw_tx
            .send(ReplicatedBatch {
                seq: total + 1,
                ops: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(tail_rx.recv().await.unwrap().seq, total + 1);
        drop(raw_tx);
        assert!(tail_rx.recv().await.is_none());
        forwarder.await.unwrap();
    }

    /// A failed bootstrap drops the cutover sender; the forwarder ends and
    /// the tail closes rather than holding the socket reader's queue open.
    #[tokio::test]
    async fn a_dropped_cutover_ends_the_forwarder() {
        let (_raw_tx, raw_rx) = mpsc::channel::<ReplicatedBatch>(REPLICATION_QUEUE);
        let (tail_tx, mut tail_rx) = mpsc::channel(REPLICATION_QUEUE);
        let (cutover_tx, cutover_rx) = oneshot::channel::<u64>();
        let forwarder = tokio::spawn(forward_tail(raw_rx, tail_tx, cutover_rx));
        drop(cutover_tx);
        tokio::time::timeout(Duration::from_secs(5), forwarder)
            .await
            .expect("the forwarder ends")
            .unwrap();
        assert!(tail_rx.recv().await.is_none());
    }
}
