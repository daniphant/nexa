use std::{env, error::Error, net::Ipv4Addr, path::PathBuf};

use nexa_runtime::LocalSession;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let port = env::var("PORT")
        .or_else(|_| env::var("NEXA_PORT"))
        .unwrap_or_else(|_| "4123".to_owned())
        .parse::<u16>()?;
    let data_directory = env::var_os("NEXA_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));
    let session = LocalSession::open(data_directory.join("local.ndjson")).await?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;

    println!(
        "Nexa runtime listening at http://{}",
        listener.local_addr()?
    );
    nexa_server::serve(listener, session).await?;
    Ok(())
}
