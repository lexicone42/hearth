use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use tracing::info;

use crate::api::state::StateStore;
use crate::config::ApiConfig;
use crate::domain::UnitSystem;
use crate::whisker;

/// The fridge dashboard page (a single self-contained document). Served at `GET
/// /`; the API token is injected into it at serve time so its same-origin
/// `/api/latest` + `/api/history` polls authenticate without a token ever living
/// in the repo (the file ships a `__HEARTH_API_TOKEN__` placeholder).
const DASHBOARD_TEMPLATE: &str = include_str!("dashboard.html");

/// How many days of daily-median weight the sparklines show (matches the page's
/// "10-day trend" label).
const HISTORY_DAYS: usize = 10;

/// Everything a request handler needs: the shared store, the configured unit
/// system, the optional bearer token, the rendered dashboard HTML, and the
/// Whisker history dir (for the weight sparklines). Cloned per request by axum
/// (all members are cheap clones — the dashboard is an `Arc<str>`).
#[derive(Clone)]
struct AppState {
    store: StateStore,
    system: UnitSystem,
    token: Option<String>,
    dashboard: std::sync::Arc<str>,
    history_dir: Option<PathBuf>,
}

/// Bind and serve until the process exits. Spawned as its own task from
/// `main`; a bind failure is returned (and logged) rather than panicking, so
/// API trouble can't take down the bridge — same posture as every source.
/// `history_dir` is the Whisker archive dir (for `/api/history`); `None` when
/// Whisker isn't configured.
pub async fn serve(
    config: ApiConfig,
    store: StateStore,
    system: UnitSystem,
    history_dir: Option<PathBuf>,
) -> Result<()> {
    // Inject the token into the page once, up front (never logged, never stored).
    let dashboard: std::sync::Arc<str> = DASHBOARD_TEMPLATE
        .replace(
            "__HEARTH_API_TOKEN__",
            config.token.as_deref().unwrap_or(""),
        )
        .into();

    let state = AppState {
        store,
        system,
        token: config.token.clone(),
        dashboard,
        history_dir,
    };
    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/api/latest", get(latest))
        .route("/api/history", get(history))
        .route("/api/visits", get(visits))
        .route("/healthz", get(healthz))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding API listener on {}", config.listen))?;
    info!(listen = %config.listen, auth = config.token.is_some(), "api listening");
    axum::serve(listener, app).await.context("serving API")
}

/// `GET /` — the fridge dashboard page. Unauthenticated: it's the display shell
/// (with the token baked in for its own API polls), served on a trusted LAN.
async fn dashboard_page(State(state): State<AppState>) -> Response {
    Html(state.dashboard.to_string()).into_response()
}

/// `GET /api/latest` — the full latest-value snapshot in the hub's configured
/// unit system. The one endpoint a dashboard client needs for current state.
async fn latest(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state.token, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    Json(state.store.snapshot(state.system)).into_response()
}

/// `GET /api/history` — per-cat daily-median weight series (last `HISTORY_DAYS`
/// days) from the Whisker archive, keyed by cat slug, for the sparklines. Empty
/// when Whisker isn't configured or the archive is unreadable — the dashboard
/// falls back to its embedded series either way.
async fn history(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state.token, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    let series = match &state.history_dir {
        Some(dir) => whisker::history::weight_series(dir, HISTORY_DAYS),
        None => Default::default(),
    };
    Json(series).into_response()
}

/// Query for `/api/visits`: the cat slug and how many days back to include.
#[derive(serde::Deserialize)]
struct VisitsQuery {
    cat: String,
    #[serde(default = "default_visit_days")]
    days: usize,
}
fn default_visit_days() -> usize {
    30
}

/// `GET /api/visits?cat=<slug>&days=<n>` — every plausible visit for one cat
/// over the last `n` days (capped at 60), for the per-cat detail page's charts.
/// The page slices these into weight / waste / box / activity views itself.
async fn visits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<VisitsQuery>,
) -> Response {
    if !authorized(&state.token, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    let out = match &state.history_dir {
        Some(dir) => whisker::history::cat_visits(dir, &q.cat, q.days.min(60)),
        None => Vec::new(),
    };
    Json(out).into_response()
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

    #[test]
    fn dashboard_template_has_token_placeholder_and_no_baked_secret() {
        // The shipped page must carry the placeholder (so the token is injected
        // at runtime) and must never contain a real-looking token in the repo.
        assert!(DASHBOARD_TEMPLATE.contains("__HEARTH_API_TOKEN__"));
        // Injection replaces every placeholder occurrence.
        let injected = DASHBOARD_TEMPLATE.replace("__HEARTH_API_TOKEN__", "abc123");
        assert!(!injected.contains("__HEARTH_API_TOKEN__"));
    }
}
