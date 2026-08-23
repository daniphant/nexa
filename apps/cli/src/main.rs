use std::{
    env,
    error::Error,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crossterm::{
    cursor::{Hide, MoveUp, Show},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, read},
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use nexa_client::NexaClient;
use nexa_harness::{
    AuthStyle, CredentialFile, DefaultModel, ModelEntry, ModelInfo, ModelsSettings, ProviderConfig,
    ProviderCredential, ProviderFile, discover_models, load_or_create_server_token,
};
use nexa_protocol::{ApiFormat, ModelRef, ReasoningEffort};

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
        [group, command] if group == "provider" && command == "add" => add_provider().await,
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
        return add_provider().await;
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
           NEXA_PORT             Default local server port (default: 4123)\n  \
           NEXA_HOME             Nexa state directory (default: ~/.nexa)\n  \
           NEXA_WORKSPACE        Workspace to open (default: current directory)\n  \
           NEXA_SERVER_TOKEN     Authentication token for an explicit server\n  \
           NEXA_SERVER_TOKEN_FILE Override the local server token path\n  \
           NEXA_PROVIDER_FILE    Override the provider registry path\n  \
           NEXA_CREDENTIALS_FILE Override the credential store path"
    );
}

async fn add_provider() -> CliResult {
    println!("Add OpenAI-compatible provider\n");
    let name = prompt_required("Name")?;
    let id = provider_id(&name);
    if id.is_empty() {
        return Err("name must contain at least one letter or number".into());
    }
    let base_url = prompt_required("Base URL (include /v1 when required)")?;
    let base_url = if base_url.contains("://") {
        base_url
    } else {
        println!("No URL scheme given; assuming http://{base_url}.");
        format!("http://{base_url}")
    };
    let api_key = rpassword::prompt_password("API key (leave blank for none): ")?
        .trim()
        .to_owned();

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

    let models =
        choose_models(&base_url, (!api_key.is_empty()).then_some(api_key.as_str())).await?;
    let (models, auth) = models;

    registry.providers.insert(
        id.clone(),
        ProviderConfig {
            name,
            base_url,
            api_format: ApiFormat::ChatCompletions,
            models,
            api_key_env: None,
            auth,
        },
    );
    registry.save(&provider_path)?;

    if !api_key.is_empty() {
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

async fn choose_models(
    base_url: &str,
    api_key: Option<&str>,
) -> CliResult<(Vec<ModelEntry>, AuthStyle)> {
    println!("\nFetching available models from {base_url}…");
    let mut auth = AuthStyle::Bearer;
    match discover_models(base_url, api_key, auth).await {
        Ok(models) if !models.is_empty() => select_models(&models).map(|picked| (picked, auth)),
        first_attempt => {
            // Some gateways only accept the Anthropic-style `x-api-key`
            // header; retry with it before giving up on discovery.
            if let Some(api_key) = api_key {
                auth = auth.alternate();
                if let Ok(models) = discover_models(base_url, Some(api_key), auth).await
                    && !models.is_empty()
                {
                    println!("Provider authenticated with the x-api-key header.");
                    return select_models(&models).map(|picked| (picked, auth));
                }
            }
            match first_attempt {
                Ok(_) => eprintln!("The provider listed no models; enter them manually."),
                Err(error) => eprintln!("Could not list models ({error}); enter them manually."),
            }
            manual_models().map(|models| (models, AuthStyle::Bearer))
        }
    }
}

fn manual_models() -> CliResult<Vec<ModelEntry>> {
    let value = prompt_required("Model ID (separate several with commas)")?;
    let models = value
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|model| ModelEntry::Plain(model.to_owned()))
        .collect::<Vec<_>>();
    if models.is_empty() {
        return Err("at least one model ID is required".into());
    }
    Ok(models)
}

const MODEL_VIEWPORT_ROWS: usize = 10;

fn select_models(models: &[ModelInfo]) -> CliResult<Vec<ModelEntry>> {
    if models.is_empty() {
        return Err("at least one model ID is required".into());
    }
    let mut stdout = io::stdout();
    let _raw_mode = RawModeGuard::enable()?;
    execute!(stdout, Hide)?;

    let mut checked = vec![false; models.len()];
    let mut cursor = 0usize;
    let mut notice: Option<&'static str> = None;
    loop {
        draw_model_picker(&mut stdout, models, &checked, cursor, notice)?;
        match read()? {
            Event::Key(event)
                if matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                match event.code {
                    KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err("provider setup cancelled".into());
                    }
                    KeyCode::Esc => return Err("provider setup cancelled".into()),
                    KeyCode::Down => {
                        notice = None;
                        cursor = (cursor + 1) % models.len();
                    }
                    KeyCode::Up => {
                        notice = None;
                        cursor = (cursor + models.len() - 1) % models.len();
                    }
                    KeyCode::Char(' ') => {
                        notice = None;
                        checked[cursor] = !checked[cursor];
                    }
                    KeyCode::Enter => {
                        if checked.iter().all(|is_checked| !is_checked) {
                            notice = Some("Check at least one model before continuing.");
                            continue;
                        }
                        queue!(stdout, Clear(ClearType::FromCursorDown))?;
                        stdout.flush()?;
                        return Ok(models
                            .iter()
                            .zip(checked)
                            .filter(|(_, is_checked)| *is_checked)
                            .map(|(model, _)| {
                                if let Some(efforts) = &model.reasoning_efforts {
                                    ModelEntry::Detailed {
                                        id: model.id.clone(),
                                        reasoning_efforts: Some(efforts.clone()),
                                    }
                                } else {
                                    ModelEntry::Plain(model.id.clone())
                                }
                            })
                            .collect());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn draw_model_picker(
    stdout: &mut io::Stdout,
    models: &[ModelInfo],
    checked: &[bool],
    cursor: usize,
    notice: Option<&str>,
) -> io::Result<()> {
    let selected_count = checked.iter().filter(|is_checked| **is_checked).count();
    let viewport_start = cursor
        .saturating_sub(MODEL_VIEWPORT_ROWS - 1)
        .min(models.len().saturating_sub(MODEL_VIEWPORT_ROWS));
    let viewport_end = (viewport_start + MODEL_VIEWPORT_ROWS).min(models.len());

    let mut rows = 0u16;
    let mut line = |stdout: &mut io::Stdout, text: &str| -> io::Result<()> {
        queue!(
            stdout,
            Print(text.to_owned()),
            Clear(ClearType::UntilNewLine),
            Print("\r\n")
        )?;
        rows += 1;
        Ok(())
    };

    line(
        stdout,
        &format!(
            "Enable models · {selected_count} of {} checked",
            models.len()
        ),
    )?;
    for index in viewport_start..viewport_end {
        let state = if checked[index] { "[x]" } else { "[ ]" };
        let pointer = if index == cursor { ">" } else { " " };
        line(stdout, &format!("{pointer} {state} {}", models[index].id))?;
    }
    line(stdout, "")?;
    let footer = notice.unwrap_or("↑/↓ move · space check · enter continue · esc cancel");
    line(stdout, footer)?;
    stdout.flush()?;
    queue!(stdout, MoveUp(rows))?;
    stdout.flush()?;
    Ok(())
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show);
    }
}

async fn chat() -> CliResult {
    let configured_server_url = env::var("NEXA_SERVER_URL").ok();
    let server_url = configured_server_url
        .clone()
        .unwrap_or_else(default_server_url);
    let mut client = NexaClient::new(server_url, format!("cli-{}", std::process::id()));
    if let Some(token) = server_token(configured_server_url.is_none())? {
        client = client.with_token(token);
    }
    if configured_server_url.is_none() {
        ensure_local_server(&client).await?;
    }
    let workspace = env::var_os("NEXA_WORKSPACE")
        .map(PathBuf::from)
        .map_or_else(env::current_dir, Ok)?;
    let workspace = workspace
        .to_str()
        .ok_or("the current workspace path must be UTF-8")?;
    // Lazy sessions: nothing persists until the first message is sent.

    let settings_path = settings_path()?;
    let settings = load_settings(&settings_path);
    let preferred_model = settings.models.default.map(|default| ModelRef {
        provider: default.provider,
        id: default.model,
    });
    let preferred_effort = settings.models.default_reasoning_effort;
    let desktop_settings = settings.desktop;

    let (selected_model, selected_effort) = nexa_tui::run(
        client,
        workspace.to_owned(),
        preferred_model.clone(),
        preferred_effort,
    )
    .await?;

    if preferred_model.as_ref() != Some(&selected_model) || preferred_effort != selected_effort {
        save_settings(
            &settings_path,
            desktop_settings,
            &selected_model,
            selected_effort,
        )?;
    }
    Ok(())
}

fn settings_path() -> CliResult<PathBuf> {
    Ok(nexa_home()?.join("settings.toml"))
}

/// Settings are convenience state; a broken file never blocks chatting.
fn load_settings(path: &Path) -> nexa_harness::SettingsFile {
    match nexa_harness::SettingsFile::load_or_default(path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "Ignoring unreadable settings file {}: {error}",
                path.display()
            );
            nexa_harness::SettingsFile::default()
        }
    }
}

/// Saves the chosen model/effort as this file's new defaults, preserving
/// `desktop` (the desktop client's own state) rather than overwriting it.
fn save_settings(
    path: &Path,
    desktop: nexa_harness::DesktopSettings,
    model: &ModelRef,
    effort: Option<ReasoningEffort>,
) -> CliResult<()> {
    nexa_harness::SettingsFile {
        models: ModelsSettings {
            default: Some(DefaultModel {
                provider: model.provider.clone(),
                model: model.id.clone(),
            }),
            default_reasoning_effort: effort,
        },
        desktop,
    }
    .save(path)?;
    Ok(())
}

fn default_server_url() -> String {
    let port = env::var("NEXA_PORT").unwrap_or_else(|_| "4123".to_owned());
    format!("http://127.0.0.1:{port}")
}

async fn ensure_local_server(client: &NexaClient) -> CliResult {
    let log_path = nexa_home()?.join("logs/server.log");
    let client = client.clone();
    nexa_harness::ensure_local_server(
        &log_path,
        move || {
            let client = client.clone();
            async move {
                match client.providers().await {
                    Ok(_) => Ok(nexa_harness::ServerProbe::Ready),
                    Err(error) if error.is_connect() => Ok(nexa_harness::ServerProbe::Connecting),
                    Err(error) => Err(nexa_harness::LocalServerError::Failed(error.to_string())),
                }
            }
        },
        || println!("Starting Nexa server…"),
    )
    .await?;
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

fn server_token(local_server: bool) -> CliResult<Option<String>> {
    match env::var("NEXA_SERVER_TOKEN") {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token)),
        Ok(_) => Err("NEXA_SERVER_TOKEN must not be empty".into()),
        Err(env::VarError::NotPresent) if local_server => {
            let path = match env::var_os("NEXA_SERVER_TOKEN_FILE") {
                Some(path) => PathBuf::from(path),
                None => nexa_home()?.join("server.token"),
            };
            Ok(Some(load_or_create_server_token(path)?))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
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
