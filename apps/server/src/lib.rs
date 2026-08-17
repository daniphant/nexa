use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{FromRequest, Request, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use nexa_protocol::{AcceptedCommand, Command, ProviderSummary};
use nexa_runtime::{LocalSession, SessionError};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

pub async fn serve(listener: TcpListener, session: LocalSession) -> std::io::Result<()> {
    serve_with_catalog(listener, session, Vec::new()).await
}

pub async fn serve_with_catalog(
    listener: TcpListener,
    session: LocalSession,
    providers: Vec<ProviderSummary>,
) -> std::io::Result<()> {
    axum::serve(listener, router(session, providers)).await
}

#[derive(Clone)]
struct AppState {
    session: LocalSession,
    providers: Arc<[ProviderSummary]>,
}

fn router(session: LocalSession, providers: Vec<ProviderSummary>) -> Router {
    Router::new()
        .route("/commands", post(accept_command))
        .route("/events", get(event_stream))
        .route("/providers", get(provider_catalog))
        .with_state(AppState {
            session,
            providers: providers.into(),
        })
}

async fn accept_command(State(state): State<AppState>, request: Request) -> Response {
    let Json(command) = match Json::<Command>::from_request(request, &()).await {
        Ok(command) => command,
        Err(rejection) => return invalid_json(rejection),
    };

    match command {
        Command::SendMessage {
            client_id,
            model,
            text,
        } => {
            let client_id = client_id.trim();
            let model = nexa_protocol::ModelRef {
                provider: model.provider.trim().to_owned(),
                id: model.id.trim().to_owned(),
            };
            let text = text.trim();
            if client_id.is_empty()
                || model.provider.is_empty()
                || model.id.is_empty()
                || text.is_empty()
            {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "clientId, model, and text must not be empty",
                );
            }

            match state.session.append_message(client_id, &model, text).await {
                Ok(event) => (
                    StatusCode::CREATED,
                    Json(AcceptedCommand {
                        accepted: true,
                        sequence: event.sequence(),
                    }),
                )
                    .into_response(),
                Err(SessionError::Busy) => {
                    error_response(StatusCode::CONFLICT, "the local agent is already running")
                }
                Err(SessionError::InvalidModel(error)) => {
                    error_response(StatusCode::BAD_REQUEST, &error)
                }
                Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
            }
        }
    }
}

async fn event_stream(State(state): State<AppState>) -> Response {
    let receiver = match state.session.subscribe().await {
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

async fn provider_catalog(State(state): State<AppState>) -> Json<Vec<ProviderSummary>> {
    Json(state.providers.to_vec())
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
