use std::{
    env,
    error::Error,
    io::{self, Write},
    path::PathBuf,
};

use nexa_client::NexaClient;
use nexa_harness::{CredentialFile, ProviderConfig, ProviderCredential, ProviderFile};
use nexa_protocol::ApiFormat;

type CliResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> CliResult {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => chat_or_setup().await,
        [command] if command == "chat" => chat_or_setup().await,
        [group, command] if group == "provider" && command == "add" => add_provider(),
        [flag] if flag == "--help" || flag == "-h" || flag == "help" => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            Err("unknown command".into())
        }
    }
}

async fn chat_or_setup() -> CliResult {
    if env::var_os("NEXA_SERVER_URL").is_none()
        && ProviderFile::load_or_default(provider_path()?)?
            .providers
            .is_empty()
    {
        println!("No providers are configured yet.\n");
        return add_provider();
    }
    chat().await
}

fn print_help() {
    println!(
        "Nexa CLI\n\n\
         Usage:\n  \
           nexa               Open the fullscreen terminal UI\n  \
           nexa provider add  Configure an OpenAI-compatible provider\n  \
           nexa chat          Open the fullscreen terminal UI\n\n\
         Environment:\n  \
           NEXA_SERVER_URL       Runtime URL (default: http://127.0.0.1:4123)\n  \
           NEXA_HOME             Nexa state directory (default: ~/.nexa)\n  \
           NEXA_PROVIDER_FILE    Override the provider registry path\n  \
           NEXA_CREDENTIALS_FILE Override the credential store path"
    );
}

fn add_provider() -> CliResult {
    println!("Add OpenAI-compatible provider\n");
    let name = prompt_required("Name")?;
    let id = provider_id(&name);
    if id.is_empty() {
        return Err("name must contain at least one letter or number".into());
    }
    let base_url = prompt_required("Base URL (include /v1 when required)")?;
    let model = prompt_required("Model ID")?;
    let api_key = rpassword::prompt_password("API key (leave blank for none): ")?;

    let provider_path = provider_path()?;
    let credentials_path = credentials_path()?;
    let mut registry = ProviderFile::load_or_default(&provider_path)?;
    if registry.providers.contains_key(&id) {
        return Err(format!(
            "provider {id:?} already exists in {}",
            provider_path.display()
        )
        .into());
    }

    registry.providers.insert(
        id.clone(),
        ProviderConfig {
            name,
            base_url,
            api_format: ApiFormat::ChatCompletions,
            models: vec![model],
            api_key_env: None,
        },
    );
    registry.save(&provider_path)?;

    if !api_key.trim().is_empty() {
        let mut credentials = CredentialFile::load_or_default(&credentials_path)?;
        credentials
            .providers
            .insert(id.clone(), ProviderCredential { api_key });
        credentials.save(&credentials_path)?;
    }

    println!(
        "\nAdded {id:?} using chat completions. Start or restart nexa-server, then run `nexa chat`."
    );
    Ok(())
}

async fn chat() -> CliResult {
    let server_url =
        env::var("NEXA_SERVER_URL").unwrap_or_else(|_| "http://127.0.0.1:4123".to_owned());
    let client = NexaClient::new(server_url, format!("cli-{}", std::process::id()));
    nexa_tui::run(client).await?;
    Ok(())
}

fn prompt_required(label: &str) -> io::Result<String> {
    loop {
        let value = prompt(&format!("{label}: "))?;
        if !value.trim().is_empty() {
            return Ok(value.trim().to_owned());
        }
        eprintln!("{label} must not be empty.");
    }
}

fn prompt(label: &str) -> io::Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

fn provider_id(name: &str) -> String {
    let mut id = String::new();
    let mut pending_separator = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !id.is_empty() {
                id.push('-');
            }
            id.push(character);
            pending_separator = false;
        } else if !id.is_empty() {
            pending_separator = true;
        }
    }
    id
}

fn provider_path() -> CliResult<PathBuf> {
    match env::var_os("NEXA_PROVIDER_FILE") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(nexa_home()?.join("provider.toml")),
    }
}

fn credentials_path() -> CliResult<PathBuf> {
    match env::var_os("NEXA_CREDENTIALS_FILE") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(nexa_home()?.join("credentials.toml")),
    }
}

fn nexa_home() -> CliResult<PathBuf> {
    env::var_os("NEXA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".nexa")))
        .ok_or_else(|| "could not determine the user home directory; set NEXA_HOME".into())
}

#[cfg(test)]
mod tests {
    use super::provider_id;

    #[test]
    fn derives_a_stable_provider_id() {
        assert_eq!(provider_id("My DeepSeek API"), "my-deepseek-api");
        assert_eq!(provider_id("  Local / Qwen  "), "local-qwen");
    }
}
