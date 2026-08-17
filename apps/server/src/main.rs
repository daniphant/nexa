use std::{env, error::Error, net::Ipv4Addr, path::PathBuf, sync::Arc};

use nexa_harness::{HarnessAgent, ProviderFile, ProviderRegistry, WorkspaceTools};
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
    let provider_path = env::var_os("NEXA_PROVIDER_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("provider.toml"));
    let workspace = env::var_os("NEXA_WORKSPACE")
        .map(PathBuf::from)
        .map_or_else(env::current_dir, Ok)?;

    let registry = Arc::new(ProviderRegistry::new(ProviderFile::load(&provider_path)?)?);
    let model_count = registry.model_count();
    let tools = Arc::new(WorkspaceTools::new(&workspace)?);
    let agent = Arc::new(HarnessAgent::new(registry, tools));
    let session = LocalSession::open_with_agent(data_directory.join("local.ndjson"), agent).await?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;

    println!(
        "Nexa runtime listening at http://{} with {model_count} available model(s)",
        listener.local_addr()?
    );
    nexa_server::serve(listener, session).await?;
    Ok(())
}
