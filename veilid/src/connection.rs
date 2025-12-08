use futures::channel::mpsc;
use futures::{Sink, Stream};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Mutex;
use tokio::time::Duration;
use veilid_core::{
    RouteId, Target, VeilidAPI, VeilidAPIError, VeilidAPIResult, VeilidConfig,
    VeilidConfigProtectedStore, VeilidConfigTableStore, VeilidUpdate,
};

#[derive(Debug, Clone)]
pub struct Config {
    pub route_blob: Vec<u8>,
    pub data_dir: PathBuf,
}

pub struct Connection {
    api: Arc<Mutex<Option<VeilidAPI>>>,
    route_id: Arc<Mutex<Option<RouteId>>>,
    message_sender: mpsc::UnboundedSender<String>,
    message_receiver: mpsc::UnboundedReceiver<String>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Connection {
    pub async fn new(
        config: Config,
        _codec: crate::Codec,
    ) -> Result<Self, Error> {
        let (message_tx, message_rx) = mpsc::unbounded();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let api_arc = Arc::new(Mutex::new(None));
        let route_id_arc = Arc::new(Mutex::new(None));

        let api_clone = api_arc.clone();
        let route_id_clone = route_id_arc.clone();
        let message_tx_clone = message_tx.clone();

        // Spawn the veilid node in a background task
        let route_blob = config.route_blob.clone();
        let data_dir = config.data_dir.clone();
        tokio::spawn(async move {
            let veilid_config = VeilidConfig {
                program_name: "Halloy Veilid Client".into(),
                protected_store: VeilidConfigProtectedStore {
                    always_use_insecure_storage: true,
                    directory: data_dir
                        .join(".veilid/protected_store")
                        .to_string_lossy()
                        .to_string(),
                    ..Default::default()
                },
                table_store: VeilidConfigTableStore {
                    directory: data_dir
                        .join(".veilid/table_store")
                        .to_string_lossy()
                        .to_string(),
                    ..Default::default()
                },
                namespace: "halloy".to_owned(),
                ..Default::default()
            };

            let update_callback = move |update: VeilidUpdate| match update {
                VeilidUpdate::AppMessage(msg) => {
                    let message = String::from_utf8_lossy(msg.message());
                    let _ =
                        message_tx_clone.unbounded_send(message.to_string());
                }
                _ => {}
            };

            // Startup Veilid node
            let veilid_api = match veilid_core::api_startup_config(
                Arc::new(update_callback),
                veilid_config,
            )
            .await
            {
                Ok(api) => api,
                Err(e) => {
                    log::error!("Failed to start veilid node: {e:?}");
                    return;
                }
            };

            // Attach to the network
            if let Err(e) = veilid_api.attach().await {
                log::error!("Failed to attach veilid node: {e:?}");
                return;
            }

            // Import the private route blob
            let route_id = loop {
                match veilid_api
                    .import_remote_private_route(route_blob.clone())
//                    .await
                {
                    Ok(rid) => break rid,
                    Err(VeilidAPIError::TryAgain { .. }) => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        //                        veilid_tools::sleep(1000).await;
                        continue;
                    }
                    Err(e) => {
                        log::error!("Failed to import route: {e:?}");
                        return;
                    }
                }
            };

            *api_clone.lock().await = Some(veilid_api.clone());
            *route_id_clone.lock().await = Some(route_id);

            // Wait for shutdown signal
            let _ = shutdown_rx.await;
            veilid_api.shutdown().await;
        });

        Ok(Self {
            api: api_arc,
            route_id: route_id_arc,
            message_sender: message_tx,
            message_receiver: message_rx,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    pub async fn shutdown(mut self) -> Result<(), Error> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }

    pub async fn send_message(&mut self, message: String) -> Result<(), Error> {
        let api = self.api.lock().await;
        let route_id = self.route_id.lock().await;

        if let (Some(api), Some(route_id)) = (api.as_ref(), route_id.as_ref()) {
            let rc = api.routing_context().map_err(Error::VeilidApi)?;
            loop {
                match rc
                    .app_message(
                        Target::PrivateRoute(*route_id),
                        message.as_bytes().to_vec(),
                    )
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(VeilidAPIError::TryAgain { .. }) => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        //                        veilid_tools::sleep(1000).await;
                        continue;
                    }
                    Err(e) => return Err(Error::VeilidApi(e)),
                }
            }
        } else {
            Err(Error::NotConnected)
        }
    }
}

impl Stream for Connection {
    type Item = Result<String, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.message_receiver)
            .poll_next(cx)
            .map(|opt| opt.map(|msg| Ok(msg)))
    }
}

impl Sink<String> for Connection {
    type Error = Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: String,
    ) -> Result<(), Self::Error> {
        // We can't use async in start_send, so we'll need to handle this differently
        // For now, we'll store it and send it in poll_flush
        // This is a limitation - we might need to refactor this
        let rt = tokio::runtime::Handle::current();
        rt.block_on(self.send_message(item))?;
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        // Shutdown will be handled by the Drop impl or explicit shutdown
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("veilid api error: {0:?}")]
    VeilidApi(VeilidAPIError),
    #[error("not connected")]
    NotConnected,
}
