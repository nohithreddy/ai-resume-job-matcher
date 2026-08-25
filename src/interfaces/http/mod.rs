mod errors;
mod middleware;
mod openapi;
mod rate_limit;
mod routes;

use std::sync::{Arc, OnceLock};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware::{from_fn, from_fn_with_state},
    routing::{get, post},
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use thiserror::Error;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};

use axum::{
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use http::HeaderMap;

use crate::{
    application::{AdminService, AuthService, MatchingService, ResumeJobService},
    config::AppConfig,
    infrastructure::IdempotencyStore,
};

pub use errors::ApiError;
pub use middleware::RequestId;
pub use rate_limit::AuthRateLimiter;
use routes::{
    admin_list_users, cover_letter, create_application, create_job, create_match, create_resume,
    delete_resume, get_report, get_resume, health_live, health_ready, interview_questions,
    list_applications, list_jobs, list_matches, list_resumes, login, logout, metrics, not_found,
    openapi_json, recommendations, refresh, register, update_resume, upload_resume,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub auth: Arc<AuthService>,
    pub resume_jobs: Arc<ResumeJobService>,
    pub matching: Arc<MatchingService>,
    pub admin: Arc<AdminService>,
    pub idempotency: Arc<IdempotencyStore>,
    pub metrics: PrometheusHandle,
    pub persistence_label: &'static str,
    pub rate_limiter: Arc<AuthRateLimiter>,
}

#[derive(Clone, Debug, Error)]
#[error("{0}")]
pub struct BootstrapError(String);

impl BootstrapError {
    pub fn embedding_client(error: reqwest::Error) -> Self {
        Self(format!("failed to build embedding HTTP client: {error}"))
    }

    pub fn password_hasher(error: argon2::password_hash::Error) -> Self {
        Self(format!("failed to initialize password hashing: {error}"))
    }

    pub fn database(error: crate::domain::DomainError) -> Self {
        tracing::error!(%error, "database bootstrap failed");
        Self("database is unavailable".to_owned())
    }
}

pub fn install_metrics_recorder() -> Result<PrometheusHandle, BootstrapError> {
    static RECORDER: OnceLock<Result<PrometheusHandle, BootstrapError>> = OnceLock::new();
    RECORDER
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(|error| {
                    BootstrapError(format!("failed to install Prometheus recorder: {error}"))
                })
        })
        .clone()
}

pub fn build_router(state: AppState) -> Router {
    let auth_routes = Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
        .route("/logout", post(logout))
        .route_layer(from_fn_with_state(
            state.clone(),
            rate_limit::auth_rate_limit,
        ));

    Router::new()
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/metrics", get(metrics))
        .route("/api/v1/openapi.json", get(openapi_json))
        .route("/api-docs/openapi.json", get(openapi_json))
        .route("/swagger-ui", get(swagger_ui_redirect))
        .route("/swagger-ui/", get(serve_swagger_ui))
        .route("/swagger-ui/{*path}", get(serve_swagger_ui))
        .nest("/api/v1/auth", auth_routes)
        .route("/api/v1/resumes", get(list_resumes).post(create_resume))
        .route("/api/v1/resumes/upload", post(upload_resume))
        .route(
            "/api/v1/resumes/{resume_id}",
            get(get_resume).put(update_resume).delete(delete_resume),
        )
        .route(
            "/api/v1/resumes/{resume_id}/cover-letter",
            post(cover_letter),
        )
        .route("/api/v1/jobs", get(list_jobs).post(create_job))
        .route("/api/v1/applications", post(create_application))
        .route("/api/v1/jobs/{job_id}/applications", get(list_applications))
        .route(
            "/api/v1/jobs/{job_id}/recommendations",
            get(recommendations),
        )
        .route(
            "/api/v1/jobs/{job_id}/interview-questions",
            post(interview_questions),
        )
        .route("/api/v1/matches", get(list_matches).post(create_match))
        .route("/api/v1/reports/{match_id}", get(get_report))
        .route("/api/v1/admin/users", get(admin_list_users))
        .method_not_allowed_fallback(|| async { ApiError::method_not_allowed() })
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(11 * 1024 * 1024))
        .layer(CatchPanicLayer::custom(middleware::panic_response))
        .layer(TraceLayer::new_for_http())
        .layer(from_fn(middleware::request_metrics))
        .layer(from_fn(middleware::request_id))
        .with_state(state)
}

async fn swagger_ui_redirect() -> impl IntoResponse {
    (
        StatusCode::FOUND,
        [(header::LOCATION, HeaderValue::from_static("/swagger-ui/"))],
    )
}

async fn serve_swagger_ui(path: Option<axum::extract::Path<String>>) -> impl IntoResponse {
    let tail = path.as_ref().map(|p| p.0.as_str()).unwrap_or("");
    // Config points the UI at our alias endpoint; swagger-initializer.js will be templated on demand.
    let config = Arc::new(utoipa_swagger_ui::Config::from("/api-docs/openapi.json"));
    match utoipa_swagger_ui::serve(tail, config) {
        Ok(Some(file)) => {
            let content_type = file.content_type;
            let bytes = file.bytes;
            let mut headers = HeaderMap::new();
            if let Ok(value) = HeaderValue::from_str(&content_type) {
                headers.insert(header::CONTENT_TYPE, value);
            }
            // Allow the UI to be framed where needed and cache static assets briefly.
            headers.insert(
                HeaderName::from_static("cache-control"),
                HeaderValue::from_static("public, max-age=300"),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "Not found").into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to serve swagger ui");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to serve Swagger UI",
            )
                .into_response()
        }
    }
}
