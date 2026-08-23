use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{FromRequest, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use nexa_protocol::{AcceptedCommand, Command, PresetSummary, ProviderSummary, WorkspaceRequest};
use nexa_runtime::{RegistryError, SessionError, SessionRegistry};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

pub async fn serve(
    listener: TcpListener,
    sessions: SessionRegistry,
    auth_token: String,
) -> std::io::Result<()> {
    serve_with_catalog(listener, sessions, Vec::new(), Vec::new(), auth_token).await
}

pub async fn serve_with_catalog(
    listener: TcpListener,
    sessions: SessionRegistry,
    providers: Vec<ProviderSummary>,
    presets: Vec<(String, Vec<String>)>,
    auth_token: String,
) -> std::io::Result<()> {
    axum::serve(listener, router(sessions, providers, presets, auth_token)).await
}

#[derive(Clone)]
struct AppState {
    sessions: SessionRegistry,
    providers: Arc<[ProviderSummary]>,
    /// Loaded agent presets: name -> tool allow-list (empty = every tool).
    presets: Arc<[(String, Vec<String>)]>,
    auth_token: Arc<str>,
}

fn resolve_preset(
    state: &AppState,
    preset: &Option<String>,
) -> Result<Option<Vec<String>>, StatusCode> {
    let Some(name) = preset else {
        return Ok(None);
    };
    if name.is_empty() {
        return Ok(None);
    }
    match state
        .presets
        .iter()
        .find(|(preset_name, _)| preset_name == name)
    {
        Some((_, tools)) => Ok(Some(tools.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

fn preset_summaries(state: &AppState) -> Vec<PresetSummary> {
    state
        .presets
        .iter()
        .map(|(name, tools)| PresetSummary {
            name: name.clone(),
            description: String::new(),
            tools: tools.clone(),
        })
        .collect()
}

fn router(
    sessions: SessionRegistry,
    providers: Vec<ProviderSummary>,
    presets: Vec<(String, Vec<String>)>,
    auth_token: String,
) -> Router {
    Router::new()
        .route("/commands", post(accept_command))
        .route("/presets", get(preset_catalog))
        .route("/providers", get(provider_catalog))
        .route("/sessions/create", post(create_session))
        .route("/sessions/list", post(list_sessions))
        .route("/sessions/{session_id}/events", get(event_stream))
        .with_state(AppState {
            sessions,
            providers: providers.into(),
            presets: presets.into(),
            auth_token: Arc::from(auth_token),
        })
        .layer(axum::middleware::map_response(protocol_version_header))
}

async fn preset_catalog(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    (StatusCode::OK, Json(preset_summaries(&state))).into_response()
}

/// Advertises the wire-protocol revision so clients can detect stale servers.
async fn protocol_version_header(mut response: Response) -> Response {
    let value = nexa_protocol::PROTOCOL_VERSION.into();
    response.headers_mut().insert("x-nexa-protocol", value);
    response
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<WorkspaceRequest>,
) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    if request.workspace.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "workspace must not be empty");
    }
    match state.sessions.create_session(request.workspace).await {
        Ok(info) => (StatusCode::CREATED, Json(info)).into_response(),
        Err(error) => registry_error_response(error),
    }
}

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<WorkspaceRequest>,
) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    if request.workspace.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "workspace must not be empty");
    }
    match state.sessions.list_sessions(request.workspace).await {
        Ok(summaries) => (StatusCode::OK, Json(summaries)).into_response(),
        Err(error) => registry_error_response(error),
    }
}

async fn accept_command(State(state): State<AppState>, request: Request) -> Response {
    if !authenticated(request.headers(), &state) {
        return authentication_required();
    }
    let Json(command) = match Json::<Command>::from_request(request, &()).await {
        Ok(command) => command,
        Err(rejection) => return invalid_json(rejection),
    };

    match command {
        Command::SendMessage {
            session_id,
            client_id,
            model,
            preset,
            reasoning_effort,
            text,
        } => {
            let session_id = session_id.trim();
            let client_id = client_id.trim();
            let model = nexa_protocol::ModelRef {
                provider: model.provider.trim().to_owned(),
                id: model.id.trim().to_owned(),
            };
            let allowed_tools = match resolve_preset(&state, &preset) {
                Ok(tools) => tools,
                Err(_) => {
                    return error_response(
                        StatusCode::NOT_FOUND,
                        &format!(
                            "unknown agent preset {:?}",
                            preset.as_deref().unwrap_or_default()
                        ),
                    );
                }
            };
            if preset.as_ref().is_some_and(|name| name.is_empty()) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "preset must not be empty when provided",
                );
            }
            let text = text.trim();
            if session_id.is_empty()
                || client_id.is_empty()
                || model.provider.is_empty()
                || model.id.is_empty()
                || text.is_empty()
            {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "sessionId, clientId, model, and text must not be empty",
                );
            }

            let session = match state.sessions.session(session_id).await {
                Ok(session) => session,
                Err(error) => return registry_error_response(error),
            };
            match session
                .append_message(client_id, &model, reasoning_effort, allowed_tools, text)
                .await
            {
                Ok(event) => (
                    StatusCode::CREATED,
                    Json(AcceptedCommand {
                        accepted: true,
                        sequence: event.sequence(),
                    }),
                )
                    .into_response(),
                Err(SessionError::Busy) => {
                    error_response(StatusCode::CONFLICT, "the session agent is already running")
                }
                Err(SessionError::InvalidModel(error)) => {
                    error_response(StatusCode::BAD_REQUEST, &error)
                }
                Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
            }
        }
    }
}

async fn event_stream(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    let session = match state.sessions.session(&session_id).await {
        Ok(session) => session,
        Err(error) => return registry_error_response(error),
    };
    let receiver = match session.subscribe().await {
        Ok(receiver) => receiver,
        Err(error) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
    };

    let stream = UnboundedReceiverStream::new(receiver).map(|event| {
        let sequence = event.sequence();
        let event = match serde_json::to_string(&event) {
            Ok(data) => SseEvent::default()
                .id(sequence.to_string())
                .event("message")
                .data(data),
            Err(error) => SseEvent::default().event("error").data(error.to_string()),
        };
        Ok::<SseEvent, Infallible>(event)
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn registry_error_response(error: RegistryError) -> Response {
    let status = if error.is_not_found() {
        StatusCode::NOT_FOUND
    } else if error.is_invalid_request() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error_response(status, &error.to_string())
}

async fn provider_catalog(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    Json(state.providers.to_vec()).into_response()
}

fn authenticated(headers: &HeaderMap, state: &AppState) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == state.auth_token.as_ref())
}

fn authentication_required() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "local server authentication required",
    )
}

fn invalid_json(rejection: JsonRejection) -> Response {
    error_response(StatusCode::BAD_REQUEST, &rejection.body_text())
}

fn error_response(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: error.to_owned(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}
