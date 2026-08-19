use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{FromRequest, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use nexa_protocol::{AcceptedCommand, Command, OpenSessionRequest, ProviderSummary};
use nexa_runtime::{RegistryError, SessionError, SessionRegistry};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

pub async fn serve(
    listener: TcpListener,
    sessions: SessionRegistry,
    auth_token: String,
) -> std::io::Result<()> {
    serve_with_catalog(listener, sessions, Vec::new(), auth_token).await
}

pub async fn serve_with_catalog(
    listener: TcpListener,
    sessions: SessionRegistry,
    providers: Vec<ProviderSummary>,
    auth_token: String,
) -> std::io::Result<()> {
    axum::serve(listener, router(sessions, providers, auth_token)).await
}

#[derive(Clone)]
struct AppState {
    sessions: SessionRegistry,
    providers: Arc<[ProviderSummary]>,
    auth_token: Arc<str>,
}

fn router(
    sessions: SessionRegistry,
    providers: Vec<ProviderSummary>,
    auth_token: String,
) -> Router {
    Router::new()
        .route("/commands", post(accept_command))
        .route("/providers", get(provider_catalog))
        .route("/sessions/open", post(open_session))
        .route("/sessions/{session_id}/events", get(event_stream))
        .with_state(AppState {
            sessions,
            providers: providers.into(),
            auth_token: Arc::from(auth_token),
        })
}

async fn open_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OpenSessionRequest>,
) -> Response {
    if !authenticated(&headers, &state) {
        return authentication_required();
    }
    if request.workspace.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "workspace must not be empty");
    }
    match state.sessions.open_workspace(request.workspace).await {
        Ok(info) => (StatusCode::OK, Json(info)).into_response(),
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
            text,
        } => {
            let session_id = session_id.trim();
            let client_id = client_id.trim();
            let model = nexa_protocol::ModelRef {
                provider: model.provider.trim().to_owned(),
                id: model.id.trim().to_owned(),
            };
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
            match session.append_message(client_id, &model, text).await {
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
