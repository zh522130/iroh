use std::{collections::BTreeMap, sync::Arc};

use anyhow::{anyhow, Result};
use futures_lite::future::Boxed as BoxedFuture;
use iroh_blobs::{
    downloader::{DownloadRequest, Downloader},
    get::{
        db::{DownloadProgress, GetState},
        Stats,
    },
    provider::EventSender,
    util::{
        local_pool::LocalPoolHandle,
        progress::{AsyncChannelProgressSender, ProgressSender},
        SetTagOption,
    },
    HashAndFormat, TempTag,
};
use iroh_net::{endpoint::Connecting, Endpoint, NodeAddr};
use iroh_router::ProtocolHandler;
use tracing::{debug, warn};

use crate::{
    client::blobs::DownloadMode,
    rpc_protocol::blobs::{BatchId, DownloadRequest as BlobDownloadRequest},
};

#[derive(Debug)]
pub(crate) struct BlobsProtocol<S> {
    rt: LocalPoolHandle,
    store: S,
    events: EventSender,
    downloader: Downloader,
    batches: tokio::sync::Mutex<BlobBatches>,
}

/// Name used for logging when new node addresses are added from gossip.
const BLOB_DOWNLOAD_SOURCE_NAME: &str = "blob_download";

/// Keeps track of all the currently active batch operations of the blobs api.
#[derive(Debug, Default)]
pub(crate) struct BlobBatches {
    /// Currently active batches
    batches: BTreeMap<BatchId, BlobBatch>,
    /// Used to generate new batch ids.
    max: u64,
}

/// A single batch of blob operations
#[derive(Debug, Default)]
struct BlobBatch {
    /// The tags in this batch.
    tags: BTreeMap<HashAndFormat, Vec<TempTag>>,
}

impl BlobBatches {
    /// Create a new unique batch id.
    pub(crate) fn create(&mut self) -> BatchId {
        let id = self.max;
        self.max += 1;
        BatchId(id)
    }

    /// Store a temp tag in a batch identified by a batch id.
    pub(crate) fn store(&mut self, batch: BatchId, tt: TempTag) {
        let entry = self.batches.entry(batch).or_default();
        entry.tags.entry(tt.hash_and_format()).or_default().push(tt);
    }

    /// Remove a tag from a batch.
    pub(crate) fn remove_one(&mut self, batch: BatchId, content: &HashAndFormat) -> Result<()> {
        if let Some(batch) = self.batches.get_mut(&batch) {
            if let Some(tags) = batch.tags.get_mut(content) {
                tags.pop();
                if tags.is_empty() {
                    batch.tags.remove(content);
                }
                return Ok(());
            }
        }
        // this can happen if we try to upgrade a tag from an expired batch
        anyhow::bail!("tag not found in batch");
    }

    /// Remove an entire batch.
    pub(crate) fn remove(&mut self, batch: BatchId) {
        self.batches.remove(&batch);
    }
}

impl<S: iroh_blobs::store::Store> BlobsProtocol<S> {
    pub(crate) fn new_with_events(
        store: S,
        rt: LocalPoolHandle,
        events: EventSender,
        downloader: Downloader,
    ) -> Self {
        Self {
            rt,
            store,
            events,
            downloader,
            batches: Default::default(),
        }
    }

    pub(crate) fn store(&self) -> &S {
        &self.store
    }

    pub(crate) async fn batches(&self) -> tokio::sync::MutexGuard<'_, BlobBatches> {
        self.batches.lock().await
    }

    pub(crate) async fn download(
        &self,
        endpoint: Endpoint,
        req: BlobDownloadRequest,
        progress: AsyncChannelProgressSender<DownloadProgress>,
    ) -> Result<()> {
        let BlobDownloadRequest {
            hash,
            format,
            nodes,
            tag,
            mode,
        } = req;
        let hash_and_format = HashAndFormat { hash, format };
        let temp_tag = self.store.temp_tag(hash_and_format);
        let stats = match mode {
            DownloadMode::Queued => {
                self.download_queued(endpoint, hash_and_format, nodes, progress.clone())
                    .await?
            }
            DownloadMode::Direct => {
                self.download_direct_from_nodes(endpoint, hash_and_format, nodes, progress.clone())
                    .await?
            }
        };

        progress.send(DownloadProgress::AllDone(stats)).await.ok();
        match tag {
            SetTagOption::Named(tag) => {
                self.store.set_tag(tag, Some(hash_and_format)).await?;
            }
            SetTagOption::Auto => {
                self.store.create_tag(hash_and_format).await?;
            }
        }
        drop(temp_tag);

        Ok(())
    }

    async fn download_queued(
        &self,
        endpoint: Endpoint,
        hash_and_format: HashAndFormat,
        nodes: Vec<NodeAddr>,
        progress: AsyncChannelProgressSender<DownloadProgress>,
    ) -> Result<Stats> {
        let mut node_ids = Vec::with_capacity(nodes.len());
        let mut any_added = false;
        for node in nodes {
            node_ids.push(node.node_id);
            if !node.info.is_empty() {
                endpoint.add_node_addr_with_source(node, BLOB_DOWNLOAD_SOURCE_NAME)?;
                any_added = true;
            }
        }
        let can_download = !node_ids.is_empty() && (any_added || endpoint.discovery().is_some());
        anyhow::ensure!(can_download, "no way to reach a node for download");
        let req = DownloadRequest::new(hash_and_format, node_ids).progress_sender(progress);
        let handle = self.downloader.queue(req).await;
        let stats = handle.await?;
        Ok(stats)
    }

    #[tracing::instrument("download_direct", skip_all, fields(hash=%hash_and_format.hash.fmt_short()))]
    async fn download_direct_from_nodes(
        &self,
        endpoint: Endpoint,
        hash_and_format: HashAndFormat,
        nodes: Vec<NodeAddr>,
        progress: AsyncChannelProgressSender<DownloadProgress>,
    ) -> Result<Stats> {
        let mut last_err = None;
        let mut remaining_nodes = nodes.len();
        let mut nodes_iter = nodes.into_iter();
        'outer: loop {
            match iroh_blobs::get::db::get_to_db_in_steps(
                self.store.clone(),
                hash_and_format,
                progress.clone(),
            )
            .await?
            {
                GetState::Complete(stats) => return Ok(stats),
                GetState::NeedsConn(needs_conn) => {
                    let (conn, node_id) = 'inner: loop {
                        match nodes_iter.next() {
                            None => break 'outer,
                            Some(node) => {
                                remaining_nodes -= 1;
                                let node_id = node.node_id;
                                if node_id == endpoint.node_id() {
                                    debug!(
                                        ?remaining_nodes,
                                        "skip node {} (it is the node id of ourselves)",
                                        node_id.fmt_short()
                                    );
                                    continue 'inner;
                                }
                                match endpoint.connect(node, iroh_blobs::protocol::ALPN).await {
                                    Ok(conn) => break 'inner (conn, node_id),
                                    Err(err) => {
                                        debug!(
                                            ?remaining_nodes,
                                            "failed to connect to {}: {err}",
                                            node_id.fmt_short()
                                        );
                                        continue 'inner;
                                    }
                                }
                            }
                        }
                    };
                    match needs_conn.proceed(conn).await {
                        Ok(stats) => return Ok(stats),
                        Err(err) => {
                            warn!(
                                ?remaining_nodes,
                                "failed to download from {}: {err}",
                                node_id.fmt_short()
                            );
                            last_err = Some(err);
                        }
                    }
                }
            }
        }
        match last_err {
            Some(err) => Err(err.into()),
            None => Err(anyhow!("No nodes to download from provided")),
        }
    }
}

impl<S: iroh_blobs::store::Store> ProtocolHandler for BlobsProtocol<S> {
    fn accept(self: Arc<Self>, conn: Connecting) -> BoxedFuture<Result<()>> {
        Box::pin(async move {
            iroh_blobs::provider::handle_connection(
                conn.await?,
                self.store.clone(),
                self.events.clone(),
                self.rt.clone(),
            )
            .await;
            Ok(())
        })
    }

    fn shutdown(self: Arc<Self>) -> BoxedFuture<()> {
        Box::pin(async move {
            self.store.shutdown().await;
        })
    }
}
