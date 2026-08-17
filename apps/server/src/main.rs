use std::{env, error::Error, net::Ipv4Addr, path::PathBuf, sync::Arc};

use nexa_harness::{CredentialFile, HarnessAgent, ProviderFile, ProviderRegistry, WorkspaceTools};
use nexa_runtime::LocalSession;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let port = env::var("PORT")
        .or_else(|_| env::var("NEXA_PORT"))
        .unwrap_or_else(|_| "4123".to_owned())
        .parse::<u16>()?;
    let nexa_home = env::var_os("NEXA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".nexa")))
        .ok_or("could not determine the user home directory; set NEXA_HOME")?;
    let sessions_directory = env::var_os("NEXA_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("sessions"));
    let provider_path = env::var_os("NEXA_PROVIDER_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("provider.toml"));
    let credentials_path = env::var_os("NEXA_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("credentials.toml"));
    let workspace = env::var_os("NEXA_WORKSPACE")
        .map(PathBuf::from)
        .map_or_else(env::current_dir, Ok)?;

    let registry = Arc::new(ProviderRegistry::with_credentials(
        ProviderFile::load(&provider_path)?,
        &CredentialFile::load_or_default(credentials_path)?,
    )?);
    let model_count = registry.model_count();
    let catalog = registry.catalog();
    let tools = Arc::new(WorkspaceTools::new(&workspace)?);
    let agent = Arc::new(HarnessAgent::new(registry, tools));
    let session =
        LocalSession::open_with_agent(sessions_directory.join("local.ndjson"), agent).await?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;

    println!(
        "Nexa runtime listening at http://{} with {model_count} available model(s)",
        listener.local_addr()?
    );
    nexa_server::serve_with_catalog(listener, session, catalog).await?;
    Ok(())
}
