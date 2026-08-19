use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Write as _},
    fs, io,
    path::Path,
};

use nexa_protocol::ApiFormat;
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_format: ApiFormat,
    pub models: Vec<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
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
    if provider.models.is_empty() || provider.models.iter().any(|model| model.trim().is_empty()) {
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

    use nexa_protocol::ApiFormat;
    use tempfile::tempdir;

    use super::{
        CredentialFile, ProviderConfig, ProviderCredential, ProviderFile, SERVER_TOKEN_BYTES,
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
        assert!(providers["one"].models.contains(&"shared-model".to_owned()));
        assert!(providers["two"].models.contains(&"shared-model".to_owned()));
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
                    models: vec!["deepseek-chat".to_owned()],
                    api_key_env: None,
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
