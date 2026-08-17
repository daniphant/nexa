use std::{collections::BTreeMap, error::Error, fmt, fs, io, path::Path};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderFile {
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl ProviderFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ProviderConfigError> {
        let contents = fs::read_to_string(path).map_err(ProviderConfigError::Read)?;
        toml::from_str(&contents).map_err(ProviderConfigError::Parse)
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

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug)]
pub enum ProviderConfigError {
    Read(io::Error),
    Parse(toml::de::Error),
    InvalidProvider { name: String, reason: String },
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read provider registry: {error}"),
            Self::Parse(error) => write!(formatter, "invalid provider registry: {error}"),
            Self::InvalidProvider { name, reason } => {
                write!(formatter, "provider {name:?} is invalid: {reason}")
            }
        }
    }
}

impl Error for ProviderConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::InvalidProvider { .. } => None,
        }
    }
}

fn validate_provider(name: &str, provider: &ProviderConfig) -> Result<(), ProviderConfigError> {
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

#[cfg(test)]
mod tests {
    use super::ProviderFile;

    #[test]
    fn allows_two_providers_to_offer_the_same_model() {
        let file: ProviderFile = toml::from_str(
            r#"
                [providers.one]
                base_url = "https://one.example/v1"
                models = ["shared-model"]

                [providers.two]
                base_url = "https://two.example/v1"
                models = ["shared-model"]
            "#,
        )
        .unwrap();

        let providers = file.validated_providers().unwrap();
        assert!(providers["one"].models.contains(&"shared-model".to_owned()));
        assert!(providers["two"].models.contains(&"shared-model".to_owned()));
    }
}
