use std::{
    env,
    error::Error,
    io::{self, Write},
    path::PathBuf,
};

use nexa_harness::{CredentialFile, ProviderConfig, ProviderCredential, ProviderFile};
use nexa_protocol::{AcceptedCommand, ApiFormat, Command, Event, ModelRef, ProviderSummary};
use reqwest::{Client, Response};

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
           nexa provider add  Configure an OpenAI-compatible provider\n  \
           nexa chat          Chat through the running Nexa runtime\n\n\
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
    let client = Client::new();
    let providers = client
        .get(endpoint(&server_url, "/providers"))
        .send()
        .await
        .map_err(|error| format!("could not reach the Nexa runtime at {server_url}: {error}"))?
        .error_for_status()?
        .json::<Vec<ProviderSummary>>()
        .await?;
    let (provider, model) = select_model(&providers)?;

    println!(
        "Using {} / {} ({})\nType /quit to leave.\n",
        provider.name, model, provider.id
    );

    loop {
        let message = prompt("you> ")?;
        let message = message.trim();
        if message == "/quit" || message == "/exit" {
            break;
        }
        if message.is_empty() {
            continue;
        }

        run_turn(&client, &server_url, &provider.id, &model, message).await?;
    }
    Ok(())
}

fn select_model(providers: &[ProviderSummary]) -> CliResult<(ProviderSummary, String)> {
    let choices = providers
        .iter()
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .map(move |model| (provider.clone(), model.clone()))
        })
        .collect::<Vec<_>>();

    if choices.is_empty() {
        return Err("the runtime has no configured models; run `nexa provider add` first".into());
    }
    if choices.len() == 1 {
        return Ok(choices[0].clone());
    }

    println!("Choose a model:");
    for (index, (provider, model)) in choices.iter().enumerate() {
        println!(
            "  {}. {} / {} ({})",
            index + 1,
            provider.name,
            model,
            provider.id
        );
    }
    loop {
        let selection = prompt_required("Selection")?;
        if let Ok(index) = selection.parse::<usize>()
            && let Some(choice) = index.checked_sub(1).and_then(|index| choices.get(index))
        {
            return Ok(choice.clone());
        }
        eprintln!("Choose a number from 1 to {}.", choices.len());
    }
}

async fn run_turn(
    client: &Client,
    server_url: &str,
    provider: &str,
    model: &str,
    text: &str,
) -> CliResult {
    let event_response = client
        .get(endpoint(server_url, "/events"))
        .send()
        .await?
        .error_for_status()?;
    let mut events = EventReader::new(event_response);

    let response = client
        .post(endpoint(server_url, "/commands"))
        .json(&Command::SendMessage {
            client_id: format!("cli-{}", std::process::id()),
            model: ModelRef {
                provider: provider.to_owned(),
                id: model.to_owned(),
            },
            text: text.to_owned(),
        })
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        return Err(format!("runtime rejected the message ({status}): {body}").into());
    }
    let accepted = response.json::<AcceptedCommand>().await?;

    print!("assistant> ");
    io::stdout().flush()?;
    let mut saw_delta = false;
    loop {
        let event = events.next().await?;
        if event.sequence() <= accepted.sequence {
            continue;
        }
        match event {
            Event::AssistantTextDelta { text, .. } => {
                saw_delta = true;
                print!("{text}");
                io::stdout().flush()?;
            }
            Event::AssistantMessage { text, .. } if !saw_delta => {
                print!("{text}");
                io::stdout().flush()?;
            }
            Event::ToolCallStarted { call, .. } => {
                println!("\n[tool: {}]", call.name);
                print!("assistant> ");
                io::stdout().flush()?;
            }
            Event::RunCompleted { .. } => {
                println!();
                return Ok(());
            }
            Event::RunFailed { error, .. } => {
                println!();
                return Err(io::Error::other(format!("agent run failed: {error}")).into());
            }
            Event::Message { .. }
            | Event::RunStarted { .. }
            | Event::AssistantMessage { .. }
            | Event::ToolCallCompleted { .. } => {}
        }
    }
}

struct EventReader {
    response: Response,
    buffer: String,
}

impl EventReader {
    fn new(response: Response) -> Self {
        Self {
            response,
            buffer: String::new(),
        }
    }

    async fn next(&mut self) -> CliResult<Event> {
        loop {
            if let Some(event) = take_event(&mut self.buffer)? {
                return Ok(event);
            }
            let chunk = self
                .response
                .chunk()
                .await?
                .ok_or("runtime event stream closed")?;
            self.buffer.push_str(std::str::from_utf8(&chunk)?);
        }
    }
}

fn take_event(buffer: &mut String) -> CliResult<Option<Event>> {
    let Some((boundary, separator_length)) = frame_boundary(buffer) else {
        return Ok(None);
    };
    let frame = buffer[..boundary].to_owned();
    buffer.drain(..boundary + separator_length);
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&data)?))
}

fn frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    buffer
        .find("\n\n")
        .map(|index| (index, 2))
        .or_else(|| buffer.find("\r\n\r\n").map(|index| (index, 4)))
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

fn endpoint(server_url: &str, path: &str) -> String {
    format!("{}{path}", server_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use nexa_protocol::Event;

    use super::{provider_id, take_event};

    #[test]
    fn derives_a_stable_provider_id() {
        assert_eq!(provider_id("My DeepSeek API"), "my-deepseek-api");
        assert_eq!(provider_id("  Local / Qwen  "), "local-qwen");
    }

    #[test]
    fn decodes_an_event_without_exposing_sse_metadata() {
        let mut buffer = concat!(
            "id: 3\n",
            "event: message\n",
            "data: {\"type\":\"run_completed\",\"sessionId\":\"local\",",
            "\"sequence\":3,\"runId\":\"run-1\",\"createdAtMs\":1}\n\n"
        )
        .to_owned();

        let event = take_event(&mut buffer).unwrap().unwrap();
        assert!(matches!(event, Event::RunCompleted { sequence: 3, .. }));
        assert!(buffer.is_empty());
    }
}
