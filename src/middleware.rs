//! Cross-cutting HTTP middleware.

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};

use crate::constants::{headers, ICP_VERSION};

/// Stamps `ICP-Version` on every response.
pub async fn icp_version_middleware(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(headers::ICP_VERSION, HeaderValue::from_static(ICP_VERSION));
    res
}
