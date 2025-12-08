use std::{env::temp_dir, future::Future, io::Write as _, sync::Arc};
mod networking;
use networking::{open_route, get_config, create_route};
use std::path::PathBuf;
use tempfile::tempdir;
use veilid_core::*;

fn get_temp_dir() -> std::io::Result<PathBuf> {
    let temp_dir = tempdir()?; // Creates the directory in the OS's temp location (e.g., /tmp)
    let dir_path: PathBuf = temp_dir.into_path();
    Ok(dir_path)
}




#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Dialing Veilid..");
    let config = get_config(get_temp_dir()?);
    println!("All goood");
 //   println!("Spawning my own veilid node...");
 //   let _ = create_route(done_recv, config).await;
    println!("Opening route...");
/* connect to other node that i spawned:
warning: `veilid-core-examples-private-route` (example "private-route-example") generated 2 warnings (run `cargo fix --example "private-route-example"` to apply 1 suggestion)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.80s
     Running `/tmp/ve/third/veilid/target/debug/examples/private-route-example`
Veilid Private Routing Example
Waiting for network.........................ready.
Route id created: AQqE2rHU8qwxkL5fMo7101lxkBViPhIZEddHDKjtLkc
Connect with this private route blob:
cargo run --example private-route-example -- --connect ARAeUAECAQJRBAEBURgBAg8wRExWEAT/7sdvWj8/v1kDYXE+XfS/ULf8x31NITI+Sdjf66qhzlTBAAARBARBEAL/JVPBhu0kDCADmZ0m5USL70BUlzXIyWMvlkwvYhSUwwWHEQQDMQ3yAf9w0MAGY9qk4gnK1d4qtaaKLcFTETRXD7N27+N4ZB46mcJ9csfX/uxApBr5BiDV/J0IU3u/CS1+k5MjmdMaiMvWdNqRh8RvBLE/3E60qWAAZMQ/JkdnKkeM
Press ctrl-c when you are finished.
AppMessage received: yelllooow
*/
    let connect = "ARAeUAECAQJRBAEBURgBAg8wRExWEAT/7sdvWj8/v1kDYXE+XfS/ULf8x31NITI+Sdjf66qhzlTBAAARBARBEAL/JVPBhu0kDCADmZ0m5USL70BUlzXIyWMvlkwvYhSUwwWHEQQDMQ3yAf9w0MAGY9qk4gnK1d4qtaaKLcFTETRXD7N27+N4ZB46mcJ9csfX/uxApBr5BiDV/J0IU3u/CS1+k5MjmdMaiMvWdNqRh8RvBLE/3E60qWAAZMQ/JkdnKkeM";
    let blob: Vec<u8> = data_encoding::BASE64.decode(connect.as_bytes())?;
    let _ = open_route(blob, config, String::from("yelllooow")).await;

    Ok(())
}
