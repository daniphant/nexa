use std::{collections::BTreeMap, env};

use futures_util::StreamExt;
use nexa_protocol::{ModelMessage, ModelRef, ProviderSummary, ToolCall, ToolDefinition};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    CredentialFile, InferenceRequest, Provider, ProviderConfig, ProviderConfigError, ProviderEvent,
    ProviderFile,
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
                models: provider.config.models.clone(),
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
        if !provider.config.models.contains(&model.id) {
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
        OpenAiProvider::new(&provider.config.base_url, &request.model.id, api_key).stream(request)
    }
}

fn error_stream(error: String) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
    let (events, receiver) = mpsc::unbounded_channel();
    let _ = events.send(Err(error));
    receiver
}

struct OpenAiProvider {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    #[must_use]
    fn new(base_url: &str, model: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: Client::new(),
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.into(),
            api_key,
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
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let Err(error) =
                stream_response(client, endpoint, model, api_key, request, &events).await
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
    request: InferenceRequest,
    events: &mpsc::UnboundedSender<Result<ProviderEvent, String>>,
) -> Result<(), String> {
    let body = json!({
        "model": model,
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
        "tools": request.tools.iter().map(tool_json).collect::<Vec<_>>(),
        "stream": true,
    });
    let mut request = client.post(endpoint).json(&body);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

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
        http::{HeaderMap, header},
        response::IntoResponse,
        routing::post,
    };
    use nexa_protocol::{ModelMessage, ModelRef, ToolDefinition};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use crate::{InferenceRequest, Provider, ProviderEvent};

    use super::{OpenAiProvider, SseDecoder};

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
        let provider = OpenAiProvider::new(&base_url, "test-model", Some("test-key".to_owned()));
        let mut stream = provider.stream(InferenceRequest {
            model: ModelRef {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
            },
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
