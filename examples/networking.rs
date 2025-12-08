use crate::get_temp_dir;
use std::path::PathBuf;
use std::{future::Future, io::Write as _, sync::Arc};
use tracing_subscriber::{
    Layer, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};
use veilid_core::{
    RouteId, Target, VeilidAPI, VeilidAPIError, VeilidAPIResult, VeilidConfig,
    VeilidConfigProtectedStore, VeilidConfigTableStore, VeilidUpdate,
    api_startup_config, tools,
};

// send Messages via
pub struct MessageSender {
    routeid: RouteId,
}

// assume we have the routeid already
pub async fn new_message(input_message: String) -> anyhow::Result<()> {
    let exe_dir = get_temp_dir()?; // send random /tmp/ directory
    let config = get_config(exe_dir);

    let connect = "";
    let blob: Vec<u8> = data_encoding::BASE64.decode(connect.as_bytes())?;

    println!("Opening route and sending message");
    let _ = open_route(blob, config, input_message).await;

    Ok(())
}

pub fn get_config(director: PathBuf) -> VeilidConfig {
    VeilidConfig {
        program_name: "Veilid Private Routing Example".into(),
        protected_store: VeilidConfigProtectedStore {
            // IMPORTANT: don't do this in production
            // This avoids prompting for a password and is insecure
            always_use_insecure_storage: true,
            directory: director
                .join(".veilid/protected_store")
                .to_string_lossy()
                .to_string(),
            ..Default::default()
        },
        table_store: VeilidConfigTableStore {
            directory: director
                .join(".veilid/table_store")
                .to_string_lossy()
                .to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn veilid_api_scope<
    'a,
    F: Future<Output = Result<T, Box<dyn std::error::Error>>>,
    T,
>(
    update_callback: impl Fn(VeilidUpdate) + Send + Sync + 'static,
    veilid_config: VeilidConfig,
    scope: impl FnOnce(VeilidAPI) -> F + Send + Sync + 'a,
) -> Result<T, Box<dyn std::error::Error>> {
    // Startup Veilid node
    // Note: future is boxed due to its size and our aggressive clippy lints
    let veilid_api =
        Box::pin(api_startup_config(Arc::new(update_callback), veilid_config))
            .await?;

    // Attach to the network
    veilid_api.attach().await?;

    // Operate the Veilid node inside a scope
    let res = scope(veilid_api.clone()).await;

    // Clean shutdown
    veilid_api.shutdown().await;

    // Return result
    res
}

pub async fn create_route(
    mut done_recv: tokio::sync::mpsc::Receiver<()>,
    mut config: VeilidConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    // Use a namespace for the receiving side of the private route
    config.namespace = "recv".to_owned();

    // Run veilid node
    veilid_api_scope(update_callback, config, |veilid_api| async move {

        // Create a new private route endpoint
        let (route_id, route_blob) =
            try_again_loop(|| async { veilid_api.new_private_route().await }).await?;

        // Print the blob
        println!(
            "Route id created: {route_id}\nConnect with this private route blob:\ncargo run --example private-route-example -- --connect {}",
            data_encoding::BASE64.encode(&route_blob)
        );

        // Wait for enter key to exit the application
        // The VeilidUpdate for AppMessages will print received messages in the background
        println!("Press ctrl-c when you are finished.");
        let _ = done_recv.recv().await;

        Ok(())

    }).await
}

fn update_callback(update: VeilidUpdate) {
    match update {
        VeilidUpdate::Log(_veilid_log) => {}
        VeilidUpdate::AppMessage(veilid_app_message) => {
            let msg = String::from_utf8_lossy(veilid_app_message.message());
            println!("AppMessage received: {msg}");
        }
        VeilidUpdate::AppCall(_veilid_app_call) => {}
        VeilidUpdate::Attachment(_veilid_state_attachment) => {}
        VeilidUpdate::Network(_veilid_state_network) => {}
        VeilidUpdate::Config(_veilid_state_config) => {}
        VeilidUpdate::RouteChange(veilid_route_change) => {
            // XXX: If this happens, the route is dead, and a new one should be generated and
            // exchanged. This will no longer be necessary after DHT Route Autopublish is implemented
            println!("{veilid_route_change:?}");
        }
        VeilidUpdate::ValueChange(_veilid_value_change) => {}
        VeilidUpdate::Shutdown => {}
    }
}

pub async fn send_message(
    veildid: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let exe_dir = get_temp_dir()?;
    let config = VeilidConfig {
        program_name: "Veilid Private Routing Example".into(),
        protected_store: VeilidConfigProtectedStore {
            // IMPORTANT: don't do this in production
            // This avoids prompting for a password and is insecure
            always_use_insecure_storage: true,
            directory: exe_dir
                .join(".veilid/protected_store")
                .to_string_lossy()
                .to_string(),
            ..Default::default()
        },
        table_store: VeilidConfigTableStore {
            directory: exe_dir
                .join(".veilid/table_store")
                .to_string_lossy()
                .to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    Ok(())
}

pub async fn open_route(
    route_blob: Vec<u8>,
    mut config: VeilidConfig,
    input_message: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Use a namespace for the sending side of the private route
    config.namespace = "send".to_owned();
    // Run veilid node
    veilid_api_scope(update_callback, config, |veilid_api| async move {
        // Import the private route blob
        let route_id = try_again_loop(|| async {
            veilid_api.import_remote_private_route(route_blob.clone())
        })
        .await?;
        // Create a routing context to send with
        let rc = veilid_api.routing_context()?;
        //    Push one message
        try_again_loop(|| async {
            rc.app_message(
                Target::PrivateRoute(route_id),
                input_message.as_bytes().to_vec(),
            )
            .await
        })
        .await?;
        //      println!("message sent");
        Ok(())
    })
    .await
}

async fn try_again_loop<R, F: Future<Output = VeilidAPIResult<R>>>(
    f: impl Fn() -> F,
) -> VeilidAPIResult<R> {
    let mut waiting = false;
    loop {
        let res = f().await;
        match res {
            Ok(v) => {
                if waiting {
                    println!("ready.");
                }
                return Ok(v);
            }
            Err(VeilidAPIError::TryAgain { message: _ }) => {
                if !waiting {
                    print!("Waiting for network...");
                    waiting = true;
                } else {
                    print!(".");
                }
                let _ = std::io::stdout().flush();
                tools::sleep(1000).await;
            }
            // XXX: This should not be necessary, and VeilidAPIError::TryAgain should
            // be used in the case of 'allocated route failed to test'.
            Err(VeilidAPIError::Generic { message }) => {
                if waiting {
                    println!();
                    waiting = false;
                }
                println!("Error: {message}");
                tools::sleep(1000).await;
            }
            Err(e) => {
                if waiting {
                    println!();
                }
                return Err(e);
            }
        }
    }
}
