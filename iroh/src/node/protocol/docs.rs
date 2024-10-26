use std::{ops::Deref, sync::Arc};

use anyhow::Result;
use futures_lite::future::Boxed as BoxedFuture;
use iroh_blobs::downloader::Downloader;
use iroh_docs::engine::{DefaultAuthorStorage, Engine};
use iroh_gossip::net::Gossip;
use iroh_net::{endpoint::Connecting, Endpoint};
use iroh_router::ProtocolHandler;

use crate::node::DocsStorage;

/// Wrapper around [`Engine`] so that we can implement our RPC methods directly.
#[derive(Debug, Clone)]
pub(crate) struct DocsProtocol(Engine);

impl DocsProtocol {
    pub async fn spawn<S: iroh_blobs::store::Store>(
        storage: DocsStorage,
        blobs_store: S,
        default_author_storage: DefaultAuthorStorage,
        endpoint: Endpoint,
        gossip: Gossip,
        downloader: Downloader,
    ) -> anyhow::Result<Option<Self>> {
        let docs_store = match storage {
            DocsStorage::Disabled => return Ok(None),
            DocsStorage::Memory => iroh_docs::store::fs::Store::memory(),
            DocsStorage::Persistent(path) => iroh_docs::store::fs::Store::persistent(path)?,
        };
        let engine = Engine::spawn(
            endpoint,
            gossip,
            docs_store,
            blobs_store,
            downloader,
            default_author_storage,
        )
        .await?;
        Ok(Some(DocsProtocol(engine)))
    }
}

impl Deref for DocsProtocol {
    type Target = Engine;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ProtocolHandler for DocsProtocol {
    fn accept(self: Arc<Self>, conn: Connecting) -> BoxedFuture<Result<()>> {
        Box::pin(async move { self.handle_connection(conn).await })
    }

    fn shutdown(self: Arc<Self>) -> BoxedFuture<()> {
        Box::pin(async move {
            let this: &Self = &self;
            if let Err(err) = this.shutdown().await {
                tracing::warn!("shutdown error: {:?}", err);
            }
        })
    }
}
