use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use tracing::info;

use crate::api::state::StateStore;
use crate::config::ApiConfig;
use crate::domain::UnitSystem;

/// Everything a request handler needs: the shared store, the configured unit
/// system, and the optional bearer token. Cloned per request by axum (all
/// members are cheap clones).
#[derive(Clone)]
struct AppState {
    store: StateStore,
    system: UnitSystem,
    token: Option<String>,
}

/// Bind and serve until the process exits. Spawned as its own task from
/// `main`; a bind failure is returned (and logged) rather than panicking, so
/// API trouble can't take down the bridge — same posture as every source.
pub async fn serve(config: ApiConfig, store: StateStore, system: UnitSystem) -> Result<()> {
    let state = AppState {
        store,
        system,
        token: config.token.clone(),
    };
    let app = Router::new()
        .route("/api/latest", get(latest))
        .route("/healthz", get(healthz))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding API listener on {}", config.listen))?;
    info!(listen = %config.listen, auth = config.token.is_some(), "api listening");
    axum::serve(listener, app).await.context("serving API")
}

/// `GET /api/latest` — the full latest-value snapshot in the hub's configured
/// unit system. The one endpoint a dashboard client needs.
async fn latest(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state.token, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    Json(state.store.snapshot(state.system)).into_response()
}

/// `GET /healthz` — liveness only (no auth): the process is up and serving.
async fn healthz() -> &'static str {
    "ok"
}

/// When a token is configured, require `Authorization: Bearer <token>`.
/// LAN-grade — it keeps casual clients and housemates' port scans out, not
/// nation states; run it on a trusted network.
fn authorized(expected: &Option<String>, headers: &HeaderMap) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(auth: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = auth {
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn no_configured_token_allows_all() {
        assert!(authorized(&None, &headers_with(None)));
        assert!(authorized(&None, &headers_with(Some("Bearer whatever"))));
    }

    #[test]
    fn configured_token_requires_exact_bearer() {
        let expected = Some("s3cret".to_string());
        assert!(authorized(&expected, &headers_with(Some("Bearer s3cret"))));
        assert!(!authorized(&expected, &headers_with(Some("Bearer wrong"))));
        assert!(!authorized(&expected, &headers_with(Some("s3cret"))));
        assert!(!authorized(&expected, &headers_with(None)));
    }
}
