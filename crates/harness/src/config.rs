use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Write as _},
    fs, io,
    path::Path,
};

use nexa_protocol::{ApiFormat, ReasoningEffort};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const SERVER_TOKEN_BYTES: usize = 32;

pub fn load_or_create_server_token(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref();
    match read_server_token(path) {
        Ok(token) => return Ok(token),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("server token path has no parent directory"))?;
    fs::create_dir_all(parent)?;

    let mut random = [0_u8; SERVER_TOKEN_BYTES];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let mut token = String::with_capacity(SERVER_TOKEN_BYTES * 2);
    for byte in random {
        write!(&mut token, "{byte:02x}").expect("writing to a string cannot fail");
    }

    let mut temporary = NamedTempFile::new_in(parent)?;
    set_private_permissions(temporary.as_file())?;
    {
        use std::io::Write;
        temporary.write_all(token.as_bytes())?;
    }
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {
            set_private_path_permissions(path)?;
            Ok(token)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read_server_token(path),
        Err(error) => Err(error.error),
    }
}

fn read_server_token(path: &Path) -> io::Result<String> {
    let token = fs::read_to_string(path)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "server token file is empty",
        ));
    }
    set_private_path_permissions(path)?;
    Ok(token.to_owned())
}

#[cfg(unix)]
fn set_private_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_path_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_path_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderFile {
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl ProviderFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ProviderConfigError> {
        let contents = fs::read_to_string(path).map_err(ProviderConfigError::Read)?;
        toml::from_str(&contents).map_err(ProviderConfigError::Parse)
    }

    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, ProviderConfigError> {
        match Self::load(path) {
            Ok(file) => Ok(file),
            Err(ProviderConfigError::Read(error)) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ProviderConfigError> {
        self.clone().validated_providers()?;
        let contents = toml::to_string_pretty(self).map_err(ProviderConfigError::Serialize)?;
        write_file(path.as_ref(), contents.as_bytes(), false).map_err(ProviderConfigError::Write)
    }

    pub(crate) fn validated_providers(
        self,
    ) -> Result<BTreeMap<String, ProviderConfig>, ProviderConfigError> {
        for (name, provider) in &self.providers {
            validate_provider(name, provider)?;
        }
        Ok(self.providers)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum AuthStyle {
    /// Send credentials as `Authorization: Bearer <key>` (default).
    #[default]
    #[serde(rename = "bearer")]
    Bearer,
    /// Send credentials as `x-api-key: <key>` (Anthropic-style gateways).
    #[serde(rename = "x-api-key")]
    XApiKey,
}

impl AuthStyle {
    pub fn alternate(self) -> Self {
        match self {
            Self::Bearer => Self::XApiKey,
            Self::XApiKey => Self::Bearer,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_format: ApiFormat,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub auth: AuthStyle,
}

/// One selectable model of a provider. Plain TOML strings remain valid for
/// models without reported capability details.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ModelEntry {
    Detailed {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_efforts: Option<Vec<ReasoningEffort>>,
    },
    Plain(String),
}

impl ModelEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Detailed { id, .. } | Self::Plain(id) => id,
        }
    }

    #[must_use]
    pub fn reasoning_efforts(&self) -> Option<&[ReasoningEffort]> {
        match self {
            Self::Detailed {
                reasoning_efforts, ..
            } => reasoning_efforts.as_deref(),
            Self::Plain(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CredentialFile {
    pub providers: BTreeMap<String, ProviderCredential>,
}

impl CredentialFile {
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, CredentialFileError> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(CredentialFileError::Read(error)),
        };
        toml::from_str(&contents).map_err(CredentialFileError::Parse)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CredentialFileError> {
        let contents = toml::to_string_pretty(self).map_err(CredentialFileError::Serialize)?;
        write_file(path.as_ref(), contents.as_bytes(), true).map_err(CredentialFileError::Write)
    }

    #[must_use]
    pub fn api_key(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .map(|credentials| credentials.api_key.as_str())
            .filter(|api_key| !api_key.trim().is_empty())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderCredential {
    pub api_key: String,
}

/// Client defaults persisted under `NEXA_HOME`, separate from the provider
/// registry. Missing or unreadable settings never block startup.
///
/// File shape:
/// ```toml
/// [models]
/// default = "provider-id/model-id"
/// default_reasoning_effort = "high"
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SettingsFile {
    #[serde(default)]
    pub models: ModelsSettings,
    #[serde(default)]
    pub desktop: DesktopSettings,
}

impl SettingsFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SettingsFileError> {
        let contents = fs::read_to_string(path).map_err(SettingsFileError::Read)?;
        toml::from_str(&contents).map_err(SettingsFileError::Parse)
    }

    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, SettingsFileError> {
        match Self::load(path) {
            Ok(file) => Ok(file),
            Err(SettingsFileError::Read(error)) if error.kind() == io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SettingsFileError> {
        let contents = toml::to_string_pretty(self).map_err(SettingsFileError::Serialize)?;
        write_file(path.as_ref(), contents.as_bytes(), false).map_err(SettingsFileError::Write)
    }
}

/// Desktop client state persisted across launches: the active workspace,
/// its default agent preset, and recently used workspaces for the project
/// picker.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DesktopSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_preset: Option<String>,
    /// Most-recently-used first, deduplicated, capped at
    /// [`DesktopSettings::MAX_RECENT_WORKSPACES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_workspaces: Vec<String>,
}

impl DesktopSettings {
    pub const MAX_RECENT_WORKSPACES: usize = 12;

    /// Moves `workspace` to the front of the recent list, deduplicating and
    /// capping it at [`Self::MAX_RECENT_WORKSPACES`].
    pub fn remember_workspace(&mut self, workspace: impl Into<String>) {
        let workspace = workspace.into();
        self.recent_workspaces
            .retain(|existing| existing != &workspace);
        self.recent_workspaces.insert(0, workspace);
        self.recent_workspaces.truncate(Self::MAX_RECENT_WORKSPACES);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelsSettings {
    /// Model to select on startup, written as `"provider-id/model-id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
}

/// A `provider/model` pair serialized as a single slash-separated string.
/// Provider IDs never contain slashes, so splitting on the first one is
/// unambiguous even when the model ID itself does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefaultModel {
    pub provider: String,
    pub model: String,
}

impl fmt::Display for DefaultModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

impl Serialize for DefaultModel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DefaultModel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let (provider, model) = value.split_once('/').ok_or_else(|| {
            serde::de::Error::custom(
                "expected \"provider-id/model-id\", for example \"xai/grok-code-fast-1\"",
            )
        })?;
        Ok(Self {
            provider: provider.trim().to_owned(),
            model: model.trim().to_owned(),
        })
    }
}

#[derive(Debug)]
pub enum SettingsFileError {
    Read(io::Error),
    Write(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for SettingsFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read settings: {error}"),
            Self::Write(error) => write!(formatter, "could not write settings: {error}"),
            Self::Parse(error) => write!(formatter, "invalid settings file: {error}"),
            Self::Serialize(error) => write!(formatter, "could not serialize settings: {error}"),
        }
    }
}

impl Error for SettingsFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub enum ProviderConfigError {
    Read(io::Error),
    Write(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    InvalidProvider { name: String, reason: String },
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read provider registry: {error}"),
            Self::Write(error) => write!(formatter, "could not write provider registry: {error}"),
            Self::Parse(error) => write!(formatter, "invalid provider registry: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "could not serialize provider registry: {error}")
            }
            Self::InvalidProvider { name, reason } => {
                write!(formatter, "provider {name:?} is invalid: {reason}")
            }
        }
    }
}

impl Error for ProviderConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::InvalidProvider { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum CredentialFileError {
    Read(io::Error),
    Write(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for CredentialFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read credentials: {error}"),
            Self::Write(error) => write!(formatter, "could not write credentials: {error}"),
            Self::Parse(error) => write!(formatter, "invalid credentials file: {error}"),
            Self::Serialize(error) => write!(formatter, "could not serialize credentials: {error}"),
        }
    }
}

impl Error for CredentialFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

fn validate_provider(name: &str, provider: &ProviderConfig) -> Result<(), ProviderConfigError> {
    if name.trim().is_empty() || provider.name.trim().is_empty() {
        return Err(ProviderConfigError::InvalidProvider {
            name: name.to_owned(),
            reason: "provider ID and name must not be empty".to_owned(),
        });
    }
    if provider.base_url.trim().is_empty() {
        return Err(ProviderConfigError::InvalidProvider {
            name: name.to_owned(),
            reason: "base_url must not be empty".to_owned(),
        });
    }
    if provider.models.is_empty()
        || provider
            .models
            .iter()
            .any(|model| model.id().trim().is_empty())
    {
        return Err(ProviderConfigError::InvalidProvider {
            name: name.to_owned(),
            reason: "models must contain at least one non-empty model ID".to_owned(),
        });
    }
    Ok(())
}

fn write_file(path: &Path, contents: &[u8], private: bool) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).and_then(|mut file| {
        use std::io::Write;
        file.write_all(contents)
    })?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, Barrier},
        thread,
    };

    use nexa_protocol::{ApiFormat, ReasoningEffort};
    use tempfile::tempdir;

    use super::{
        AuthStyle, CredentialFile, DefaultModel, DesktopSettings, ModelEntry, ModelsSettings,
        ProviderConfig, ProviderCredential, ProviderFile, SERVER_TOKEN_BYTES, SettingsFile,
        load_or_create_server_token,
    };

    #[test]
    fn concurrent_server_token_creation_returns_one_private_token() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("server.token");
        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    load_or_create_server_token(path).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let tokens = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(tokens[0], tokens[1]);
        assert_eq!(tokens[0].len(), SERVER_TOKEN_BYTES * 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn roundtrips_the_models_settings_block() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");

        assert!(
            SettingsFile::load_or_default(&path)
                .unwrap()
                .models
                .default
                .is_none()
        );

        SettingsFile {
            models: ModelsSettings {
                default: Some(DefaultModel {
                    provider: "local".to_owned(),
                    model: "grok-code-fast-1".to_owned(),
                }),
                default_reasoning_effort: Some(ReasoningEffort::XHigh),
            },
            desktop: DesktopSettings::default(),
        }
        .save(&path)
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("default = \"local/grok-code-fast-1\""));
        assert!(contents.contains("default_reasoning_effort = \"xhigh\""));

        let settings = SettingsFile::load_or_default(&path).unwrap();
        let default_model = settings.models.default.unwrap();
        assert_eq!(default_model.provider, "local");
        assert_eq!(default_model.model, "grok-code-fast-1");
        assert_eq!(
            settings.models.default_reasoning_effort,
            Some(ReasoningEffort::XHigh)
        );
    }

    #[test]
    fn roundtrips_the_desktop_settings_block() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");

        SettingsFile {
            models: ModelsSettings::default(),
            desktop: DesktopSettings {
                active_workspace: Some("/home/user/project".to_owned()),
                default_preset: Some("read-only".to_owned()),
                recent_workspaces: vec![
                    "/home/user/project".to_owned(),
                    "/home/user/other".to_owned(),
                ],
            },
        }
        .save(&path)
        .unwrap();

        let settings = SettingsFile::load_or_default(&path).unwrap();
        assert_eq!(
            settings.desktop.active_workspace.as_deref(),
            Some("/home/user/project")
        );
        assert_eq!(
            settings.desktop.default_preset.as_deref(),
            Some("read-only")
        );
        assert_eq!(
            settings.desktop.recent_workspaces,
            vec![
                "/home/user/project".to_owned(),
                "/home/user/other".to_owned()
            ]
        );
    }

    #[test]
    fn remember_workspace_moves_existing_entries_to_the_front_and_caps_the_list() {
        let mut desktop = DesktopSettings::default();
        for index in 0..DesktopSettings::MAX_RECENT_WORKSPACES {
            desktop.remember_workspace(format!("/workspace-{index}"));
        }
        assert_eq!(
            desktop.recent_workspaces.len(),
            DesktopSettings::MAX_RECENT_WORKSPACES
        );

        desktop.remember_workspace("/workspace-0");
        assert_eq!(desktop.recent_workspaces[0], "/workspace-0");
        assert_eq!(
            desktop.recent_workspaces.len(),
            DesktopSettings::MAX_RECENT_WORKSPACES
        );

        desktop.remember_workspace("/workspace-new");
        assert_eq!(desktop.recent_workspaces[0], "/workspace-new");
        assert_eq!(
            desktop.recent_workspaces.len(),
            DesktopSettings::MAX_RECENT_WORKSPACES
        );
    }

    #[test]
    fn rejects_default_model_entries_without_a_provider_prefix() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        fs::write(&path, "[models]\ndefault = \"grok-code-fast-1\"\n").unwrap();

        assert!(SettingsFile::load(&path).is_err());
    }

    #[test]
    fn reads_plain_string_model_entries_and_detailed_capability_entries() {
        let file: ProviderFile = toml::from_str(
            r#"
                [providers.one]
                name = "One"
                base_url = "https://one.example/v1"
                models = ["plain-model"]

                [providers.two]
                name = "Two"
                base_url = "https://two.example/v1"
                models = ["shared-model"]

                [providers.three]
                name = "Three"
                base_url = "https://three.example/v1"

                [[providers.three.models]]
                id = "thinking-model"
                reasoning_efforts = ["low", "xhigh"]
            "#,
        )
        .unwrap();
        file.clone().validated_providers().unwrap();

        assert_eq!(file.providers["one"].models[0].id(), "plain-model");
        assert_eq!(file.providers["one"].models[0].reasoning_efforts(), None);
        let three = &file.providers["three"].models;
        assert_eq!(
            three[0].reasoning_efforts(),
            Some([ReasoningEffort::Low, ReasoningEffort::XHigh].as_slice())
        );
    }

    #[test]
    fn allows_two_providers_to_offer_the_same_model() {
        let file: ProviderFile = toml::from_str(
            r#"
                [providers.one]
                name = "One"
                base_url = "https://one.example/v1"
                models = ["shared-model"]

                [providers.two]
                name = "Two"
                base_url = "https://two.example/v1"
                models = ["shared-model"]
            "#,
        )
        .unwrap();

        let providers = file.validated_providers().unwrap();
        assert!(
            providers["one"]
                .models
                .iter()
                .any(|model| model.id() == "shared-model")
        );
        assert!(
            providers["two"]
                .models
                .iter()
                .any(|model| model.id() == "shared-model")
        );
    }

    #[test]
    fn keeps_credentials_out_of_the_provider_registry() {
        let directory = tempdir().unwrap();
        let provider_path = directory.path().join("providers.toml");
        let credentials_path = directory.path().join("credentials.toml");
        ProviderFile {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                ProviderConfig {
                    name: "DeepSeek".to_owned(),
                    base_url: "https://api.example/v1".to_owned(),
                    api_format: ApiFormat::ChatCompletions,
                    models: vec![ModelEntry::Plain("deepseek-chat".to_owned())],
                    api_key_env: None,
                    auth: AuthStyle::Bearer,
                },
            )]),
        }
        .save(&provider_path)
        .unwrap();
        CredentialFile {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                ProviderCredential {
                    api_key: "secret-key".to_owned(),
                },
            )]),
        }
        .save(&credentials_path)
        .unwrap();

        assert!(
            !fs::read_to_string(provider_path)
                .unwrap()
                .contains("secret-key")
        );
        assert_eq!(
            CredentialFile::load_or_default(&credentials_path)
                .unwrap()
                .api_key("deepseek"),
            Some("secret-key")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(credentials_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
