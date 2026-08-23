use std::{
    env,
    error::Error,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
};

use nexa_harness::{
    Agent, CredentialFile, HarnessAgent, Provider, ProviderFile, ProviderRegistry, WorkspaceTools,
    load_or_create_server_token,
};
use nexa_protocol::PresetSummary;
use nexa_runtime::{AgentFactory, SessionRegistry};
use tokio::net::TcpListener;

struct NativeAgentFactory {
    providers: Arc<ProviderRegistry>,
}

impl AgentFactory for NativeAgentFactory {
    fn create(&self, workspace: &Path) -> Result<Option<Arc<dyn Agent>>, String> {
        let tools = WorkspaceTools::new(workspace).map_err(|error| error.to_string())?;
        let provider: Arc<dyn Provider> = self.providers.clone();
        let agent: Arc<dyn Agent> = Arc::new(HarnessAgent::new(provider, Arc::new(tools)));
        Ok(Some(agent))
    }
}

/// One loaded agent preset from `$NEXA_HOME/agent-presets/<name>.toml`.
#[derive(Clone)]
struct Preset {
    summary: PresetSummary,
    tools: Vec<String>,
}

impl Preset {
    fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            description: String,
            /// Tool allow-list. Empty or missing means every tool.
            #[serde(default)]
            tools: Vec<String>,
        }
        let raw = std::fs::read_to_string(path)?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let file: File = toml::from_str(&raw)?;
        Ok(Self {
            summary: PresetSummary {
                name,
                description: file.description,
                tools: file.tools.clone(),
            },
            tools: file.tools,
        })
    }
}

fn load_presets(directory: &Path) -> Result<Vec<Preset>, Box<dyn Error>> {
    let mut presets = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(presets),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            presets.push(Preset::load(&path)?);
        }
    }
    presets.sort_by(|left, right| left.summary.name.cmp(&right.summary.name));
    Ok(presets)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let port = env::var("NEXA_PORT")
        .or_else(|_| env::var("PORT"))
        .unwrap_or_else(|_| "4123".to_owned())
        .parse::<u16>()?;
    let nexa_home = env::var_os("NEXA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".nexa")))
        .ok_or("could not determine the user home directory; set NEXA_HOME")?;
    let sessions_directory = env::var_os("NEXA_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("sessions"));
    let presets_directory = env::var_os("NEXA_PRESETS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("agent-presets"));
    let provider_path = env::var_os("NEXA_PROVIDER_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("provider.toml"));
    let credentials_path = env::var_os("NEXA_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| nexa_home.join("credentials.toml"));
    let auth_token = match env::var("NEXA_SERVER_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token,
        Ok(_) => return Err("NEXA_SERVER_TOKEN must not be empty".into()),
        Err(env::VarError::NotPresent) => {
            let token_path = env::var_os("NEXA_SERVER_TOKEN_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| nexa_home.join("server.token"));
            load_or_create_server_token(token_path)?
        }
        Err(error) => return Err(error.into()),
    };
    let providers = Arc::new(ProviderRegistry::with_credentials(
        ProviderFile::load(&provider_path)?,
        &CredentialFile::load_or_default(credentials_path)?,
    )?);
    let model_count = providers.model_count();
    let catalog = providers.catalog();
    let sessions = SessionRegistry::new(
        sessions_directory,
        Arc::new(NativeAgentFactory { providers }),
    );
    let preset_list = load_presets(&presets_directory)?;
    let preset_count = preset_list.len();
    let presets: Vec<(String, Vec<String>)> = preset_list
        .into_iter()
        .map(|preset| (preset.summary.name, preset.tools))
        .collect();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;

    println!(
        "Nexa server listening at http://{} with {model_count} available model(s) and {preset_count} agent preset(s)",
        listener.local_addr()?
    );
    nexa_server::serve_with_catalog(listener, sessions, catalog, presets, auth_token).await?;
    Ok(())
}
