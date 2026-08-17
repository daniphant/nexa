use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::{FromRequest, Request, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use nexa_protocol::Command;
use nexa_runtime::LocalSession;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

pub async fn serve(listener: TcpListener, session: LocalSession) -> std::io::Result<()> {
    axum::serve(listener, router(session)).await
}

fn router(session: LocalSession) -> Router {
    Router::new()
        .route("/commands", post(accept_command))
        .route("/events", get(event_stream))
        .with_state(session)
}

async fn accept_command(State(session): State<LocalSession>, request: Request) -> Response {
    let Json(command) = match Json::<Command>::from_request(request, &()).await {
        Ok(command) => command,
        Err(rejection) => return invalid_json(rejection),
    };

    match command {
        Command::SendMessage { client_id, text } => {
            let client_id = client_id.trim();
            let text = text.trim();
            if client_id.is_empty() || text.is_empty() {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "clientId and text must not be empty",
                );
            }

            match session.append_message(client_id, text).await {
                Ok(event) => (
                    StatusCode::CREATED,
                    Json(AcceptedCommand {
                        accepted: true,
                        sequence: event.sequence(),
                    }),
                )
                    .into_response(),
                Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
            }
        }
    }
}

async fn event_stream(State(session): State<LocalSession>) -> Response {
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
#[serde(rename_all = "camelCase")]
struct AcceptedCommand {
    accepted: bool,
    sequence: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}
