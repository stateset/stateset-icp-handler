//! Server-Sent Events stream of transaction / subscription / peer-quote
//! events, scoped to the calling tenant.

use axum::{extract::State, http::HeaderMap};

use crate::errors::ApiError;
use crate::AppState;

use super::resolve_tenant;

#[utoipa::path(
    get,
    path = "/icp/v1/events:stream",
    tag = "ICP Core",
    responses(
        (status = 200, description = "Server-Sent Events stream of `transaction.*`, `subscription.*`, and `peer_quote.*` events.", content_type = "text/event-stream"),
    ),
)]
pub async fn sse_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<
    axum::response::Sse<
        impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let tenant_id = tenant.tenant_id.clone();
    let service = state.service.clone();
    let rx = state.service.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |evt| match evt {
        Ok(e) => {
            if !service.event_belongs_to_tenant(&e, &tenant_id) {
                return None;
            }
            let data = serde_json::to_string(&e).unwrap_or_default();
            Some(Ok(axum::response::sse::Event::default()
                .id(e.id)
                .event(e.r#type)
                .data(data)))
        }
        Err(_) => None,
    });
    Ok(axum::response::Sse::new(stream))
}
