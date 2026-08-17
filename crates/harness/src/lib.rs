mod config;
mod openai;
mod tools;

use std::sync::Arc;

pub use config::{
    CredentialFile, CredentialFileError, ProviderConfig, ProviderConfigError, ProviderCredential,
    ProviderFile,
};
use nexa_protocol::{ModelMessage, ModelRef, ToolCall, ToolDefinition, ToolResult};
pub use openai::ProviderRegistry;
use tokio::sync::mpsc;
pub use tools::{WorkspaceTools, WorkspaceToolsError};

const MAX_TOOL_ROUNDS: usize = 16;

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub model: ModelRef,
    pub messages: Vec<ModelMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEvent {
    AssistantTextDelta(String),
    AssistantMessage {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolCallStarted(ToolCall),
    ToolCallCompleted(ToolResult),
    Completed,
    Failed(String),
}

pub trait Agent: Send + Sync {
    fn validate_model(&self, _model: &ModelRef) -> Result<(), String> {
        Ok(())
    }

    fn start(&self, request: AgentRequest) -> mpsc::UnboundedReceiver<AgentEvent>;
}

#[derive(Clone, Debug)]
pub struct InferenceRequest {
    pub model: ModelRef,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolCall(ToolCall),
    Completed,
}

pub trait Provider: Send + Sync {
    fn validate_model(&self, _model: &ModelRef) -> Result<(), String> {
        Ok(())
    }

    fn stream(
        &self,
        request: InferenceRequest,
    ) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>>;
}

pub trait ToolBridge: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn execute(&self, call: ToolCall) -> impl Future<Output = ToolResult> + Send
    where
        Self: Sized;
}

pub struct HarnessAgent<T> {
    provider: Arc<dyn Provider>,
    tools: Arc<T>,
}

impl<T> HarnessAgent<T>
where
    T: ToolBridge + 'static,
{
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<T>) -> Self {
        Self { provider, tools }
    }
}

impl<T> Agent for HarnessAgent<T>
where
    T: ToolBridge + 'static,
{
    fn validate_model(&self, model: &ModelRef) -> Result<(), String> {
        self.provider.validate_model(model)
    }

    fn start(&self, request: AgentRequest) -> mpsc::UnboundedReceiver<AgentEvent> {
        let provider = Arc::clone(&self.provider);
        let tools = Arc::clone(&self.tools);
        let (events, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            run_agent(provider, tools, request, events).await;
        });
        drop(task);
        receiver
    }
}

async fn run_agent<T>(
    provider: Arc<dyn Provider>,
    tools: Arc<T>,
    mut request: AgentRequest,
    events: mpsc::UnboundedSender<AgentEvent>,
) where
    T: ToolBridge,
{
    let definitions = tools.definitions();

    for _ in 0..MAX_TOOL_ROUNDS {
        let mut provider_events = provider.stream(InferenceRequest {
            model: request.model.clone(),
            messages: request.messages.clone(),
            tools: definitions.clone(),
        });
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut completed = false;

        while let Some(event) = provider_events.recv().await {
            match event {
                Ok(ProviderEvent::TextDelta(delta)) => {
                    text.push_str(&delta);
                    if events.send(AgentEvent::AssistantTextDelta(delta)).is_err() {
                        return;
                    }
                }
                Ok(ProviderEvent::ToolCall(call)) => tool_calls.push(call),
                Ok(ProviderEvent::Completed) => {
                    completed = true;
                    break;
                }
                Err(error) => {
                    let _ = events.send(AgentEvent::Failed(error));
                    return;
                }
            }
        }

        if !completed {
            let _ = events.send(AgentEvent::Failed(
                "provider stream ended before completing the response".to_owned(),
            ));
            return;
        }

        if events
            .send(AgentEvent::AssistantMessage {
                text: text.clone(),
                tool_calls: tool_calls.clone(),
            })
            .is_err()
        {
            return;
        }
        request.messages.push(ModelMessage::Assistant {
            content: text,
            tool_calls: tool_calls.clone(),
        });

        if tool_calls.is_empty() {
            let _ = events.send(AgentEvent::Completed);
            return;
        }

        for call in tool_calls {
            if events
                .send(AgentEvent::ToolCallStarted(call.clone()))
                .is_err()
            {
                return;
            }
            let result = tools.execute(call).await;
            request.messages.push(ModelMessage::Tool(result.clone()));
            if events.send(AgentEvent::ToolCallCompleted(result)).is_err() {
                return;
            }
        }
    }

    let _ = events.send(AgentEvent::Failed(format!(
        "agent exceeded the limit of {MAX_TOOL_ROUNDS} consecutive tool rounds"
    )));
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{Arc, Mutex},
    };

    use nexa_protocol::{ModelMessage, ModelRef, ToolCall};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    use super::{
        Agent, AgentEvent, AgentRequest, HarnessAgent, InferenceRequest, Provider, ProviderEvent,
        WorkspaceTools,
    };

    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<Result<ProviderEvent, String>>>>,
        requests: Arc<Mutex<Vec<InferenceRequest>>>,
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            request: InferenceRequest,
        ) -> mpsc::UnboundedReceiver<Result<ProviderEvent, String>> {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            let (events, receiver) = mpsc::unbounded_channel();
            for event in response {
                events.send(event).unwrap();
            }
            receiver
        }
    }

    #[tokio::test]
    async fn continues_inference_after_a_tool_result() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("notes.txt"),
            "hello from the workspace",
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(VecDeque::from([
                vec![
                    Ok(ProviderEvent::ToolCall(ToolCall {
                        id: "call-1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: r#"{"path":"notes.txt"}"#.to_owned(),
                    })),
                    Ok(ProviderEvent::Completed),
                ],
                vec![
                    Ok(ProviderEvent::TextDelta("I found it.".to_owned())),
                    Ok(ProviderEvent::Completed),
                ],
            ])),
            requests: Arc::clone(&requests),
        });
        let tools = Arc::new(WorkspaceTools::new(directory.path()).unwrap());
        let agent = HarnessAgent::new(provider, tools);
        let mut events = agent.start(AgentRequest {
            model: ModelRef {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
            },
            messages: vec![ModelMessage::User {
                content: "Read the notes".to_owned(),
            }],
        });

        let mut received = Vec::new();
        while let Some(event) = events.recv().await {
            let terminal = matches!(event, AgentEvent::Completed | AgentEvent::Failed(_));
            received.push(event);
            if terminal {
                break;
            }
        }

        assert!(received.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallCompleted(result)
                if !result.is_error && result.content.contains("hello from the workspace")
        )));
        assert!(received.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantMessage { text, tool_calls }
                if text == "I found it." && tool_calls.is_empty()
        )));
        assert!(matches!(received.last(), Some(AgentEvent::Completed)));

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            requests[1].messages.last(),
            Some(ModelMessage::Tool(result)) if !result.is_error
        ));
    }
}
