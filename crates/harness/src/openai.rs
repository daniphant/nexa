use std::{collections::BTreeMap, env, time::Duration};

use futures_util::StreamExt;
use nexa_protocol::{
    ModelMessage, ModelRef, ModelSummary, ProviderSummary, ReasoningEffort, ToolCall,
    ToolDefinition,
};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    AuthStyle, CredentialFile, InferenceRequest, Provider, ProviderConfig, ProviderConfigError,
    ProviderEvent, ProviderFile,
};

pub struct ProviderRegistry {
    providers: BTreeMap<String, RegisteredProvider>,
}

impl ProviderRegistry {
    pub fn new(file: ProviderFile) -> Result<Self, ProviderConfigError> {
        Self::with_credentials(file, &CredentialFile::default())
    }

    pub fn with_credentials(
        file: ProviderFile,
        credentials: &CredentialFile,
    ) -> Result<Self, ProviderConfigError> {
        let providers = file
            .validated_providers()?
            .into_iter()
            .map(|(id, config)| {
                let api_key = credentials.api_key(&id).map(str::to_owned);
                (id, RegisteredProvider { config, api_key })
            })
            .collect();
        Ok(Self { providers })
    }

    #[must_use]
    pub fn model_count(&self) -> usize {
        self.providers
            .values()
            .map(|provider| provider.config.models.len())
            .sum()
    }

    #[must_use]
    pub fn catalog(&self) -> Vec<ProviderSummary> {
        self.providers
            .iter()
            .map(|(id, provider)| ProviderSummary {
                id: id.clone(),
                name: provider.config.name.clone(),
                api_format: provider.config.api_format,
                models: provider
                    .config
                    .models
                    .iter()
                    .map(|model| ModelSummary {
                        id: model.id().to_owned(),
                        reasoning_efforts: model
                            .reasoning_efforts()
                            .map(<[ReasoningEffort]>::to_vec)
                            .or_else(|| {
                                crate::inferred_reasoning_efforts(model.id())
                                    .map(<[ReasoningEffort]>::to_vec)
                            }),
                    })
                    .collect(),
            })
            .collect()
    }
}

struct RegisteredProvider {
    config: ProviderConfig,
    api_key: Option<String>,
}

impl Provider for ProviderRegistry {
    fn validate_model(&self, model: &ModelRef) -> Result<(), String> {
        let provider = self
            .providers
            .get(&model.provider)
            .ok_or_else(|| format!("unknown provider {:?}", model.provider))?;
        if !provider
            .config
            .models
            .iter()
            .any(|entry| entry.id() == model.id)
        {
            return Err(format!(
                "provider {:?} does not offer model {:?}",
                model.provider, model.id
            ));
        }
        Ok(())
    }

    fn stream(
        &self,
        request: InferenceRequest,
    ) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
        if let Err(error) = self.validate_model(&request.model) {
            return error_stream(error);
        }
        let Some(provider) = self.providers.get(&request.model.provider) else {
            return error_stream(format!("unknown provider {:?}", request.model.provider));
        };
        let api_key = match &provider.config.api_key_env {
            Some(variable) => match env::var(variable) {
                Ok(value) if !value.trim().is_empty() => Some(value),
                _ if provider.api_key.is_some() => provider.api_key.clone(),
                _ => {
                    return error_stream(format!(
                        "provider {:?} requires authentication through {variable}",
                        request.model.provider
                    ));
                }
            },
            None => provider.api_key.clone(),
        };
        OpenAiProvider::new(
            &provider.config.base_url,
            &request.model.id,
            api_key,
            provider.config.auth,
        )
        .stream(request)
    }
}

fn error_stream(error: String) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
    let (events, receiver) = mpsc::unbounded_channel();
    let _ = events.send(Err(error));
    receiver
}

/// Applies provider credentials to a request using the configured header
/// style: `Authorization: Bearer` or the Anthropic-style `x-api-key`.
fn apply_auth(request: RequestBuilder, api_key: Option<&str>, auth: AuthStyle) -> RequestBuilder {
    match api_key {
        Some(api_key) => match auth {
            AuthStyle::Bearer => request.bearer_auth(api_key),
            AuthStyle::XApiKey => request.header("x-api-key", api_key),
        },
        None => request,
    }
}

/// One model reported by a provider's models listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Effort levels declared for this model, or `None` when the endpoint
    /// does not say. A declaration without recognizable levels resolves to
    /// [`ReasoningEffort::COMMON`].
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
}

/// Parameter names that mark reasoning support inside parameter-list fields.
const REASONING_PARAM_NAMES: [&str; 2] = ["reasoning_effort", "reasoning"];
/// Provider-specific parameter-list fields (OpenRouter, LiteLLM).
const SUPPORTED_PARAM_FIELDS: [&str; 2] = ["supported_parameters", "supported_openai_params"];
/// Boolean capability flags (LM Studio-style endpoints).
const REASONING_FLAG_FIELDS: [&str; 2] = ["reasoning", "supports_reasoning"];
/// Fields carrying explicit effort levels when an endpoint enumerates them.
const EFFORT_LIST_FIELDS: [&str; 2] = ["reasoning_efforts", "supported_reasoning_efforts"];

fn reported_reasoning_efforts(extra: &BTreeMap<String, Value>) -> Option<Vec<ReasoningEffort>> {
    for field in EFFORT_LIST_FIELDS {
        if let Some(Value::Array(levels)) = extra.get(field) {
            let parsed = levels
                .iter()
                .filter_map(Value::as_str)
                .filter_map(ReasoningEffort::parse)
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                // Canonical order regardless of how the endpoint listed them.
                return Some(
                    ReasoningEffort::ALL
                        .into_iter()
                        .filter(|level| parsed.contains(level))
                        .collect(),
                );
            }
        }
    }

    for field in SUPPORTED_PARAM_FIELDS {
        if let Some(Value::Array(parameters)) = extra.get(field)
            && parameters
                .iter()
                .any(|parameter| {
                    matches!(parameter.as_str(), Some(name) if REASONING_PARAM_NAMES.contains(&name))
                })
        {
            return Some(ReasoningEffort::COMMON.to_vec());
        }
    }

    for field in REASONING_FLAG_FIELDS {
        if extra.get(field).and_then(Value::as_bool) == Some(true) {
            return Some(ReasoningEffort::COMMON.to_vec());
        }
    }

    if let Some(Value::Object(capabilities)) = extra.get("capabilities")
        && ["reasoning", "thinking"]
            .iter()
            .any(|field| capabilities.get(*field).and_then(Value::as_bool) == Some(true))
    {
        return Some(ReasoningEffort::COMMON.to_vec());
    }

    None
}

/// Queries an OpenAI-compatible server's `GET {base_url}/models` endpoint and
/// returns its models in case-insensitive alphabetical order.
///
/// Capability extensions are picked up opportunistically: parameter lists such
/// as OpenRouter's `supported_parameters` or LiteLLM's
/// `supported_openai_params` mark reasoning support, and boolean fields like
/// `reasoning` do the same on LM Studio-style endpoints. Endpoints without any
/// extension yield `None`, meaning unknown rather than unsupported.
///
/// # Errors
///
/// Returns an error when the request fails, the response status is not
/// successful, or the body is not a recognizable model listing.
pub async fn discover_models(
    base_url: &str,
    api_key: Option<&str>,
    auth: AuthStyle,
) -> Result<Vec<ModelInfo>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let request = Client::new().get(&url).timeout(Duration::from_secs(15));
    let response = apply_auth(request, api_key, auth)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("could not read error response: {error}"));
        return Err(format!("model listing returned {status}: {body}"));
    }
    let listing: ModelListing = response
        .json()
        .await
        .map_err(|error| format!("invalid model listing response: {error}"))?;

    let mut models = listing.models();
    models.sort_by_key(|model| model.id.to_lowercase());
    models.dedup_by(|left, right| left.id.to_lowercase() == right.id.to_lowercase());
    Ok(models)
}

#[derive(Deserialize)]
struct ModelListing {
    #[serde(default, alias = "models")]
    data: Vec<ModelListingEntry>,
}

impl ModelListing {
    fn models(self) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .map(|entry| match entry {
                ModelListingEntry::Identified { id, extra } => {
                    let reasoning_efforts = reported_reasoning_efforts(&extra).or_else(|| {
                        crate::inferred_reasoning_efforts(&id).map(<[ReasoningEffort]>::to_vec)
                    });
                    ModelInfo {
                        id,
                        reasoning_efforts,
                    }
                }
                ModelListingEntry::Named(id) => {
                    let reasoning_efforts =
                        crate::inferred_reasoning_efforts(&id).map(<[ReasoningEffort]>::to_vec);
                    ModelInfo {
                        id,
                        reasoning_efforts,
                    }
                }
            })
            .filter(|model| !model.id.trim().is_empty())
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ModelListingEntry {
    Identified {
        id: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    Named(String),
}

struct OpenAiProvider {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    auth: AuthStyle,
}

impl OpenAiProvider {
    #[must_use]
    fn new(
        base_url: &str,
        model: impl Into<String>,
        api_key: Option<String>,
        auth: AuthStyle,
    ) -> Self {
        Self {
            client: Client::new(),
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.into(),
            api_key,
            auth,
        }
    }
}

impl Provider for OpenAiProvider {
    fn stream(
        &self,
        request: InferenceRequest,
    ) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let auth = self.auth;
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let Err(error) =
                stream_response(client, endpoint, model, api_key, auth, request, &events).await
            {
                let _ = events.send(Err(error));
            }
        });
        drop(task);
        receiver
    }
}

async fn stream_response(
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    auth: AuthStyle,
    request: InferenceRequest,
    events: &mpsc::UnboundedSender<Result<ProviderEvent, String>>,
) -> Result<(), String> {
    let mut body = json!({
        "model": model,
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
        "tools": request.tools.iter().map(tool_json).collect::<Vec<_>>(),
        "stream": true,
    });
    if let Some(effort) = request.reasoning_effort {
        // Chat Completions spelling. LiteLLM proxies that classify the model as
        // OpenAI reject this unless the request lists it as allowed; xAI also
        // accepts the nested `reasoning.effort` object.
        body["reasoning_effort"] = json!(effort);
        body["reasoning"] = json!({ "effort": effort });
        body["allowed_openai_params"] = json!(["reasoning_effort"]);
    }
    let request = apply_auth(client.post(endpoint).json(&body), api_key.as_deref(), auth);

    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("could not read error response: {error}"));
        return Err(format!("provider returned {status}: {body}"));
    }

    let mut bytes = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut tool_calls = BTreeMap::<usize, PartialToolCall>::new();

    while let Some(chunk) = bytes.next().await {
        decoder.push(&chunk.map_err(|error| error.to_string())?);
        while let Some(payload) = decoder.next_payload()? {
            if handle_payload(&payload, &mut tool_calls, events)? {
                return Ok(());
            }
        }
    }

    for payload in decoder.finish()? {
        if handle_payload(&payload, &mut tool_calls, events)? {
            return Ok(());
        }
    }
    finish_tool_calls(tool_calls, events)?;
    events
        .send(Ok(ProviderEvent::Completed))
        .map_err(|_| "agent stopped receiving provider events".to_owned())
}

fn handle_payload(
    payload: &str,
    tool_calls: &mut BTreeMap<usize, PartialToolCall>,
    events: &mpsc::UnboundedSender<Result<ProviderEvent, String>>,
) -> Result<bool, String> {
    if payload == "[DONE]" {
        finish_tool_calls(std::mem::take(tool_calls), events)?;
        events
            .send(Ok(ProviderEvent::Completed))
            .map_err(|_| "agent stopped receiving provider events".to_owned())?;
        return Ok(true);
    }

    let chunk: StreamChunk = serde_json::from_str(payload)
        .map_err(|error| format!("invalid provider stream event: {error}"))?;
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            events
                .send(Ok(ProviderEvent::TextDelta(content)))
                .map_err(|_| "agent stopped receiving provider events".to_owned())?;
        }
        for delta in choice.delta.tool_calls.unwrap_or_default() {
            let pending = tool_calls.entry(delta.index).or_default();
            if let Some(id) = delta.id {
                pending.id.push_str(&id);
            }
            if let Some(function) = delta.function {
                if let Some(name) = function.name {
                    pending.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    pending.arguments.push_str(&arguments);
                }
            }
        }
    }
    Ok(false)
}

fn finish_tool_calls(
    tool_calls: BTreeMap<usize, PartialToolCall>,
    events: &mpsc::UnboundedSender<Result<ProviderEvent, String>>,
) -> Result<(), String> {
    for (_, call) in tool_calls {
        if call.id.is_empty() || call.name.is_empty() {
            return Err("provider returned an incomplete tool call".to_owned());
        }
        events
            .send(Ok(ProviderEvent::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            })))
            .map_err(|_| "agent stopped receiving provider events".to_owned())?;
    }
    Ok(())
}

fn message_json(message: &ModelMessage) -> Value {
    match message {
        ModelMessage::User { content } => json!({
            "role": "user",
            "content": content,
        }),
        ModelMessage::Assistant {
            content,
            tool_calls,
        } => {
            let mut message = json!({
                "role": "assistant",
                "content": content,
            });
            if !tool_calls.is_empty() {
                message["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                },
                            })
                        })
                        .collect(),
                );
            }
            message
        }
        ModelMessage::Tool(result) => json!({
            "role": "tool",
            "tool_call_id": result.tool_call_id,
            "content": result.content,
        }),
    }
}

fn tool_json(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn next_payload(&mut self) -> Result<Option<String>, String> {
        let Some((boundary, separator_length)) = frame_boundary(&self.buffer) else {
            return Ok(None);
        };
        let frame = self.buffer.drain(..boundary).collect::<Vec<_>>();
        self.buffer.drain(..separator_length);
        parse_frame(&frame).map(Some)
    }

    fn finish(&mut self) -> Result<Vec<String>, String> {
        let mut payloads = Vec::new();
        while let Some(payload) = self.next_payload()? {
            payloads.push(payload);
        }
        if !self.buffer.is_empty() {
            payloads.push(parse_frame(&std::mem::take(&mut self.buffer))?);
        }
        Ok(payloads)
    }
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| (index, 4))
        })
}

fn parse_frame(frame: &[u8]) -> Result<String, String> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| format!("provider stream was not UTF-8: {error}"))?;
    let payload = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if payload.is_empty() {
        return Err("provider stream event contained no data".to_owned());
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::{get, post},
    };
    use nexa_protocol::{ModelMessage, ModelRef, ReasoningEffort, ToolDefinition};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use crate::{AuthStyle, InferenceRequest, Provider, ProviderEvent};

    use super::{ModelInfo, OpenAiProvider, SseDecoder, discover_models};

    #[test]
    fn decodes_split_crlf_frames() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: {\"choices\":[]}");
        assert!(decoder.next_payload().unwrap().is_none());
        decoder.push(b"\r\n\r\ndata: [DONE]\r\n\r\n");
        assert_eq!(
            decoder.next_payload().unwrap().as_deref(),
            Some("{\"choices\":[]}")
        );
        assert_eq!(decoder.next_payload().unwrap().as_deref(), Some("[DONE]"));
    }

    #[tokio::test]
    async fn translates_streamed_text_and_tool_calls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(mock_completion)),
            )
            .await
            .unwrap();
        });
        let provider = OpenAiProvider::new(
            &base_url,
            "test-model",
            Some("test-key".to_owned()),
            AuthStyle::Bearer,
        );
        let mut stream = provider.stream(InferenceRequest {
            model: ModelRef {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
            },
            reasoning_effort: None,
            messages: vec![ModelMessage::User {
                content: "read the notes".to_owned(),
            }],
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read a file".to_owned(),
                input_schema: json!({ "type": "object" }),
            }],
        });

        assert!(matches!(
            stream.recv().await.unwrap().unwrap(),
            ProviderEvent::TextDelta(text) if text == "checking"
        ));
        assert!(matches!(
            stream.recv().await.unwrap().unwrap(),
            ProviderEvent::ToolCall(call)
                if call.id == "call-1"
                    && call.name == "read_file"
                    && call.arguments == r#"{"path":"notes.txt"}"#
        ));
        assert!(matches!(
            stream.recv().await.unwrap().unwrap(),
            ProviderEvent::Completed
        ));
        server.abort();
    }

    #[tokio::test]
    async fn discovers_sorted_unique_model_ids() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/models", get(mock_models)),
            )
            .await
            .unwrap();
        });

        let models = discover_models(&base_url, Some("test-key"), AuthStyle::Bearer)
            .await
            .unwrap();
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "a-model".to_owned(),
                    reasoning_efforts: None,
                },
                ModelInfo {
                    id: "b-model".to_owned(),
                    reasoning_efforts: None,
                },
                ModelInfo {
                    id: "c-model".to_owned(),
                    reasoning_efforts: None,
                },
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn discovers_reasoning_capabilities_from_listing_extensions() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/models", get(mock_capability_models))
                    .route("/models", get(mock_flag_models)),
            )
            .await
            .unwrap();
        });

        // OpenRouter-style parameter lists.
        let models = discover_models(
            &format!("{base_url}/v1"),
            Some("test-key"),
            AuthStyle::Bearer,
        )
        .await
        .unwrap();
        // Sorted order: enumerated, glm-5, litellm-model, plain.
        assert_eq!(models[0].id, "enumerated");
        // Explicit level enumeration wins when present.
        assert_eq!(
            models[0].reasoning_efforts,
            Some(vec![ReasoningEffort::Low, ReasoningEffort::XHigh])
        );
        // OpenRouter-style parameter lists.
        assert_eq!(
            models[1].reasoning_efforts,
            Some(ReasoningEffort::COMMON.to_vec())
        );
        // LiteLLM-style parameter lists.
        assert_eq!(
            models[2].reasoning_efforts,
            Some(ReasoningEffort::COMMON.to_vec())
        );
        // Plain entries stay unknown.
        assert_eq!(models[3].id, "plain");
        assert_eq!(models[3].reasoning_efforts, None);

        // LM Studio-style boolean flags.
        let models = discover_models(&base_url, Some("test-key"), AuthStyle::XApiKey)
            .await
            .unwrap();
        assert_eq!(
            models[0].reasoning_efforts,
            Some(ReasoningEffort::COMMON.to_vec())
        );
        server.abort();
    }

    #[tokio::test]
    async fn discovers_with_an_x_api_key_header() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/models", get(expect_x_api_key)),
            )
            .await
            .unwrap();
        });

        let models = discover_models(&base_url, Some("test-key"), AuthStyle::XApiKey)
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "thinking-local");
        server.abort();
    }

    #[tokio::test]
    async fn surfaces_model_listing_failures() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/models", get(|| async { StatusCode::UNAUTHORIZED })),
            )
            .await
            .unwrap();
        });

        let error = discover_models(&base_url, None, AuthStyle::Bearer)
            .await
            .unwrap_err();
        assert!(error.contains("401"), "unexpected error: {error}");
        server.abort();
    }

    async fn expect_x_api_key(headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        assert!(headers.get(header::AUTHORIZATION).is_none());
        Json(json!({
            "data": [
                {
                    "id": "thinking-local",
                    "capabilities": {"vision": false, "reasoning": true},
                },
            ]
        }))
    }

    async fn mock_capability_models(headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(
            headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
        Json(json!({
            "data": [
                {
                    "id": "glm-5",
                    "supported_parameters": ["temperature", "reasoning_effort"],
                },
                {
                    "id": "litellm-model",
                    "supported_openai_params": ["reasoning_effort", "max_tokens"],
                },
                {
                    "id": "enumerated",
                    "supported_reasoning_efforts": ["xhigh", "low", "banana"],
                },
                {"id": "plain"},
            ]
        }))
    }

    async fn mock_flag_models(headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
        Json(json!({
            "models": [
                {"id": "flagged", "supports_reasoning": true},
            ]
        }))
    }

    async fn mock_models(headers: HeaderMap) -> impl IntoResponse {
        assert_eq!(
            headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
        Json(json!({
            "data": [
                {"id": "b-model"},
                {"id": ""},
                {"id": "a-model"},
                {"id": "c-model"},
                {"id": "a-model"},
            ]
        }))
    }

    #[tokio::test]
    async fn sends_reasoning_effort_when_requested() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/chat/completions", post(mock_reasoning_completion)),
            )
            .await
            .unwrap();
        });
        let provider = OpenAiProvider::new(&base_url, "test-model", None, AuthStyle::Bearer);
        let mut stream = provider.stream(InferenceRequest {
            model: ModelRef {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
            },
            reasoning_effort: Some(ReasoningEffort::XHigh),
            messages: vec![ModelMessage::User {
                content: "hi".to_owned(),
            }],
            tools: Vec::new(),
        });

        assert!(matches!(
            stream.recv().await.unwrap().unwrap(),
            ProviderEvent::Completed
        ));
        server.abort();
    }

    async fn mock_reasoning_completion(Json(body): Json<Value>) -> impl IntoResponse {
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["allowed_openai_params"], json!(["reasoning_effort"]));
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
        )
    }

    async fn mock_completion(headers: HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
        assert_eq!(
            headers.get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");

        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"checking\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"notes.txt\\\"}\"}}]}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
    }
}
