use std::sync::Arc;

use axum::{
    Json,
    extract::{FromRef, FromRequest, FromRequestParts, Multipart, Path, Query, Request, State},
    response::{IntoResponse, Response},
};
use http::{HeaderMap, StatusCode, header, request::Parts};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use crate::application::{
    AuthOutput, CreateJobInput, CreateResumeInput, LoginInput, RegisterInput,
};
use crate::domain::entities::{
    Application, ApplicationStatus, AttributeComparison, CategoryScore, CategoryScores,
    ComparisonOutcome, ContextComparisons, MatchResult,
};
use crate::domain::{Job, Recommendation, Resume, Role};

use super::{ApiError, AppState, openapi::ApiDoc};

const MAX_PAGE_LIMIT: usize = 100;
const DEFAULT_PAGE_LIMIT: usize = 25;

#[derive(Debug, Clone, Copy)]
pub struct AuthenticatedUser {
    pub user_id: Uuid,
    pub role: Role,
}

impl AuthenticatedUser {
    fn require(self, permitted: bool) -> Result<Self, ApiError> {
        if permitted {
            Ok(self)
        } else {
            Err(ApiError::forbidden("Your role does not allow this action."))
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    /// Optional role. Public registration accepts `candidate` (default) or `recruiter`.
    #[serde(default)]
    pub role: Role,
}

impl From<RegisterRequest> for RegisterInput {
    fn from(request: RegisterRequest) -> Self {
        Self {
            email: request.email,
            password: request.password,
            role: request.role,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

impl From<LoginRequest> for LoginInput {
    fn from(request: LoginRequest) -> Self {
        Self {
            email: request.email,
            password: request.password,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct LogoutRequest {
    pub refresh_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateResumeRequest {
    pub title: Option<String>,
    pub raw_text: String,
}

impl From<CreateResumeRequest> for CreateResumeInput {
    fn from(request: CreateResumeRequest) -> Self {
        Self {
            title: request.title,
            raw_text: request.raw_text,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct CreateJobRequest {
    pub title: String,
    pub description: String,
}

impl From<CreateJobRequest> for CreateJobInput {
    fn from(request: CreateJobRequest) -> Self {
        Self {
            title: request.title,
            description: request.description,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct CreateApplicationRequest {
    pub resume_id: Uuid,
    pub job_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateMatchRequest {
    pub resume_id: Uuid,
    pub job_id: Uuid,
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_page_limit")]
    limit: usize,
}

fn default_page_limit() -> usize {
    DEFAULT_PAGE_LIMIT
}

impl PageQuery {
    fn bounds(&self) -> Result<(usize, usize), ApiError> {
        if self.limit == 0 || self.limit > MAX_PAGE_LIMIT {
            return Err(ApiError::bad_request(format!(
                "limit must be between 1 and {MAX_PAGE_LIMIT}"
            )));
        }
        Ok((self.offset, self.limit))
    }
}

#[derive(Deserialize)]
pub struct RecommendationQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[derive(Deserialize, Debug)]
pub struct JobSearchQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_page_limit")]
    limit: usize,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    skills: Option<String>,
    #[serde(default)]
    location: Option<String>,
}

impl JobSearchQuery {
    fn bounds(&self) -> Result<(usize, usize), ApiError> {
        if self.limit == 0 || self.limit > MAX_PAGE_LIMIT {
            return Err(ApiError::bad_request(format!(
                "limit must be between 1 and {MAX_PAGE_LIMIT}"
            )));
        }
        Ok((self.offset, self.limit))
    }

    fn into_filter(self) -> crate::domain::JobFilter {
        let skills = self.skills.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                let values = trimmed
                    .split(',')
                    .map(|part| part.trim().to_owned())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>();
                if values.is_empty() {
                    None
                } else {
                    Some(values)
                }
            }
        });
        let q = self.q.and_then(|value| {
            let trimmed = value.trim().to_owned();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        let location = self.location.and_then(|value| {
            let trimmed = value.trim().to_owned();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        crate::domain::JobFilter {
            query: q,
            skills,
            location,
        }
    }
}

#[derive(Deserialize)]
pub struct CoverLetterQuery {
    pub job_id: Uuid,
}

#[derive(Deserialize)]
pub struct InterviewQuery {
    #[serde(default)]
    pub resume_id: Option<Uuid>,
}

pub struct ProblemJson<T>(pub T);

pub struct ProblemQuery<T>(pub T);

pub struct ProblemPath<T>(pub T);

impl<T, S> FromRequest<S> for ProblemJson<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(ApiError::from_json)
    }
}

impl<T, S> FromRequestParts<S> for ProblemQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(ApiError::from_query)
    }
}

impl<T, S> FromRequestParts<S> for ProblemPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(ApiError::from_path)
    }
}

impl FromRef<AppState> for Arc<crate::application::AuthService> {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

impl<S> FromRequestParts<S> for AuthenticatedUser
where
    Arc<crate::application::AuthService>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = Arc::<crate::application::AuthService>::from_ref(state);
        let authorization = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| ApiError::unauthorized("A Bearer access token is required."))?;
        let Some((scheme, token)) = authorization.split_once(' ') else {
            return Err(ApiError::unauthorized("A Bearer access token is required."));
        };
        let token = token.trim();
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.chars().any(|character| character.is_whitespace())
        {
            return Err(ApiError::unauthorized("A Bearer access token is required."));
        }
        let claims = auth
            .authenticate_claims(token)
            .await
            .map_err(ApiError::from)?;
        Ok(Self {
            user_id: claims.user_id,
            role: claims.role,
        })
    }
}

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: &'static str,
}

#[derive(Serialize, ToSchema)]
pub struct ReadyResponse {
    pub status: &'static str,
    pub persistence: &'static str,
    pub embeddings: &'static str,
}

#[derive(Serialize, ToSchema)]
pub struct AuthResponse {
    pub user_id: Uuid,
    pub role: Role,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub refresh_expires_in: i64,
    pub session_id: Uuid,
}

impl From<AuthOutput> for AuthResponse {
    fn from(output: AuthOutput) -> Self {
        Self {
            user_id: output.user_id,
            role: output.role,
            access_token: output.access_token,
            refresh_token: output.refresh_token,
            token_type: output.token_type.to_owned(),
            expires_in: output.expires_in,
            refresh_expires_in: output.refresh_expires_in,
            session_id: output.session_id,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CategoryScoreResponse {
    pub score: f32,
    pub weight: u8,
    pub weighted_score: f32,
    pub reasons: Vec<String>,
}

impl From<&CategoryScore> for CategoryScoreResponse {
    fn from(score: &CategoryScore) -> Self {
        Self {
            score: score.score,
            weight: score.weight,
            weighted_score: score.weighted_score,
            reasons: score.reasons.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CategoryScoresResponse {
    pub skills: CategoryScoreResponse,
    pub experience: CategoryScoreResponse,
    pub education: CategoryScoreResponse,
    pub semantic_similarity: CategoryScoreResponse,
    pub certifications: CategoryScoreResponse,
    pub keywords: CategoryScoreResponse,
}

impl From<&CategoryScores> for CategoryScoresResponse {
    fn from(scores: &CategoryScores) -> Self {
        Self {
            skills: CategoryScoreResponse::from(&scores.skills),
            experience: CategoryScoreResponse::from(&scores.experience),
            education: CategoryScoreResponse::from(&scores.education),
            semantic_similarity: CategoryScoreResponse::from(&scores.semantic_similarity),
            certifications: CategoryScoreResponse::from(&scores.certifications),
            keywords: CategoryScoreResponse::from(&scores.keywords),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct AttributeComparisonResponse {
    pub resume_value: Option<String>,
    pub job_value: Option<String>,
    pub outcome: ComparisonOutcome,
    pub reason: String,
}

impl From<&AttributeComparison> for AttributeComparisonResponse {
    fn from(comparison: &AttributeComparison) -> Self {
        Self {
            resume_value: comparison.resume_value.clone(),
            job_value: comparison.job_value.clone(),
            outcome: comparison.outcome,
            reason: comparison.reason.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ContextComparisonsResponse {
    pub role: AttributeComparisonResponse,
    pub location: AttributeComparisonResponse,
    pub availability: AttributeComparisonResponse,
}

impl From<&ContextComparisons> for ContextComparisonsResponse {
    fn from(comparisons: &ContextComparisons) -> Self {
        Self {
            role: AttributeComparisonResponse::from(&comparisons.role),
            location: AttributeComparisonResponse::from(&comparisons.location),
            availability: AttributeComparisonResponse::from(&comparisons.availability),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct RecommendationList {
    pub items: Vec<RecommendationResponse>,
}

#[derive(Serialize, ToSchema)]
pub struct RecommendationResponse {
    pub job_id: Uuid,
    pub resume_id: Uuid,
    pub score: f32,
    pub matched_skills: Vec<String>,
    pub missing_skills: Vec<String>,
    pub category_scores: CategoryScoresResponse,
    pub reasons: Vec<String>,
    pub recommendations: Vec<String>,
    pub comparisons: ContextComparisonsResponse,
}

impl From<Recommendation> for RecommendationResponse {
    fn from(recommendation: Recommendation) -> Self {
        Self {
            job_id: recommendation.job_id,
            resume_id: recommendation.resume_id,
            score: recommendation.score,
            matched_skills: recommendation.matched_skills,
            missing_skills: recommendation.missing_skills,
            category_scores: CategoryScoresResponse::from(&recommendation.category_scores),
            reasons: recommendation.reasons,
            recommendations: recommendation.recommendations,
            comparisons: ContextComparisonsResponse::from(&recommendation.comparisons),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ResumeList {
    pub items: Vec<ResumeResponse>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct ResumeResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: Option<String>,
    pub skills: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<Resume> for ResumeResponse {
    fn from(resume: Resume) -> Self {
        Self {
            id: resume.id,
            user_id: resume.user_id,
            title: resume.title,
            skills: resume.skills,
            created_at: resume.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct JobList {
    pub items: Vec<JobResponse>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct JobResponse {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub title: String,
    pub skills: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<Job> for JobResponse {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            owner_id: job.owner_id,
            title: job.title,
            skills: job.skills,
            created_at: job.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ApplicationResponse {
    pub id: Uuid,
    pub candidate_id: Uuid,
    pub resume_id: Uuid,
    pub job_id: Uuid,
    pub status: ApplicationStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<Application> for ApplicationResponse {
    fn from(application: Application) -> Self {
        Self {
            id: application.id,
            candidate_id: application.candidate_id,
            resume_id: application.resume_id,
            job_id: application.job_id,
            status: application.status,
            created_at: application.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ApplicationList {
    pub items: Vec<ApplicationResponse>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct MatchResultResponse {
    pub id: Uuid,
    pub resume_id: Uuid,
    pub job_id: Uuid,
    pub candidate_id: Uuid,
    pub recruiter_id: Uuid,
    pub requested_by: Uuid,
    pub report: RecommendationResponse,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<MatchResult> for MatchResultResponse {
    fn from(result: MatchResult) -> Self {
        Self {
            id: result.id,
            resume_id: result.resume_id,
            job_id: result.job_id,
            candidate_id: result.candidate_id,
            recruiter_id: result.recruiter_id,
            requested_by: result.requested_by,
            report: RecommendationResponse::from(result.report),
            created_at: result.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct MatchResultList {
    pub items: Vec<MatchResultResponse>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct UserResponse {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<crate::domain::User> for UserResponse {
    fn from(user: crate::domain::User) -> Self {
        Self {
            id: user.id,
            email: user.email,
            role: user.role,
            created_at: user.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct UserList {
    pub items: Vec<UserResponse>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct InterviewQuestionsResponse {
    pub job_id: Uuid,
    pub questions: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CoverLetterResponse {
    pub resume_id: Uuid,
    pub job_id: Uuid,
    pub cover_letter: String,
}

#[utoipa::path(
    get,
    path = "/health/live",
    tag = "health",
    responses((status = 200, body = HealthResponse))
)]
pub async fn health_live() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[utoipa::path(
    get,
    path = "/health/ready",
    tag = "health",
    responses(
        (status = 200, body = ReadyResponse),
        (status = 503, body = super::errors::ProblemDetails)
    )
)]
pub async fn health_ready(
    State(state): State<AppState>,
) -> Result<Json<ReadyResponse>, super::ApiError> {
    if state.config.persistence == crate::config::PersistenceBackend::Sqlite {
        let path = state.config.database_path.clone();
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::task::spawn_blocking(move || {
                let conn = rusqlite::Connection::open(&path)
                    .map_err(|error| format!("open failed: {error}"))?;
                conn.busy_timeout(std::time::Duration::from_millis(200))
                    .map_err(|error| format!("busy_timeout failed: {error}"))?;
                conn.query_row("SELECT 1", [], |row| row.get::<_, i32>(0))
                    .map(|_| ())
                    .map_err(|error| format!("query failed: {error}"))
            }),
        )
        .await;

        let ready = match probe {
            Ok(Ok(Ok(()))) => true,
            Ok(Ok(Err(detail))) => {
                tracing::error!(%detail, "readiness probe failed");
                false
            }
            Ok(Err(join_error)) => {
                tracing::error!(%join_error, "readiness probe join failed");
                false
            }
            Err(_) => {
                tracing::error!("readiness probe timed out");
                false
            }
        };

        if !ready {
            return Err(super::ApiError::service_unavailable(
                "Persistence is unavailable.",
            ));
        }
    }

    Ok(Json(ReadyResponse {
        status: "ready",
        persistence: state.persistence_label,
        embeddings: if state.config.embedding_endpoint.is_some() {
            "http"
        } else {
            "deterministic-local"
        },
    }))
}

pub async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/register",
    tag = "auth",
    request_body = RegisterRequest,
    responses(
        (status = 201, body = AuthResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 409, body = super::errors::ProblemDetails)
    )
)]
pub async fn register(
    State(state): State<AppState>,
    ProblemJson(input): ProblemJson<RegisterRequest>,
) -> Result<(StatusCode, Json<AuthResponse>), ApiError> {
    let output = state
        .auth
        .register(input.into())
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("auth_registrations_total").increment(1);
    Ok((StatusCode::CREATED, Json(AuthResponse::from(output))))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, body = AuthResponse),
        (status = 401, body = super::errors::ProblemDetails)
    )
)]
pub async fn login(
    State(state): State<AppState>,
    ProblemJson(input): ProblemJson<LoginRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let output = state
        .auth
        .login(input.into())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AuthResponse::from(output)))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/refresh",
    tag = "auth",
    request_body = RefreshRequest,
    responses(
        (status = 200, body = AuthResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails)
    )
)]
pub async fn refresh(
    State(state): State<AppState>,
    ProblemJson(input): ProblemJson<RefreshRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    if input.refresh_token.trim().is_empty() || input.refresh_token.len() > 256 {
        return Err(ApiError::bad_request("A refresh token is required."));
    }
    let output = state
        .auth
        .refresh(input.refresh_token.trim())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AuthResponse::from(output)))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    request_body = LogoutRequest,
    responses(
        (status = 204),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails)
    )
)]
pub async fn logout(
    State(state): State<AppState>,
    ProblemJson(input): ProblemJson<LogoutRequest>,
) -> Result<StatusCode, ApiError> {
    if input.refresh_token.trim().is_empty() || input.refresh_token.len() > 256 {
        return Err(ApiError::bad_request("A refresh token is required."));
    }
    state
        .auth
        .logout(input.refresh_token.trim())
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/resumes",
    tag = "matching",
    security(("bearer_auth" = [])),
    request_body = CreateResumeRequest,
    responses(
        (status = 201, body = ResumeResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails)
    )
)]
pub async fn create_resume(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    ProblemJson(input): ProblemJson<CreateResumeRequest>,
) -> Result<Response, ApiError> {
    user.require(user.role.can_manage_resumes())?;
    if let Some(key) = extract_idempotency_key(&headers)
        && let Some((status, body)) = state.idempotency.get(user.user_id, &key)
    {
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .map_err(|_| ApiError::bad_request("failed to build idempotent response"))?;
        response.headers_mut().insert(
            header::HeaderName::from_static("idempotency-replayed"),
            header::HeaderValue::from_static("true"),
        );
        return Ok(response);
    }
    let resume = state
        .resume_jobs
        .create_resume(user.user_id, input.into())
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("resumes_created_total").increment(1);
    let body = Json(ResumeResponse::from(resume));
    let json_body = serde_json::to_string(&body.0).unwrap_or_default();
    if let Some(key) = extract_idempotency_key(&headers) {
        state.idempotency.insert(
            user.user_id,
            key,
            StatusCode::CREATED.as_u16(),
            json_body.clone(),
        );
    }
    Ok((StatusCode::CREATED, body).into_response())
}

fn extract_idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .or_else(|| headers.get("Idempotency-Key"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && value.len() <= 128)
}

#[utoipa::path(
    get,
    path = "/api/v1/resumes",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("offset" = Option<usize>, Query), ("limit" = Option<usize>, Query)),
    responses(
        (status = 200, body = ResumeList),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails)
    )
)]
pub async fn list_resumes(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemQuery(query): ProblemQuery<PageQuery>,
) -> Result<Json<ResumeList>, ApiError> {
    user.require(user.role.can_manage_resumes())?;
    let (offset, limit) = query.bounds()?;
    let items = state
        .resume_jobs
        .list_resumes(user.user_id, offset, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ResumeList {
        items: items.into_iter().map(ResumeResponse::from).collect(),
        offset,
        limit,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/resumes/{resume_id}",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("resume_id" = Uuid, Path)),
    responses(
        (status = 200, body = ResumeResponse),
        (status = 401, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn get_resume(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(resume_id): ProblemPath<Uuid>,
) -> Result<Json<ResumeResponse>, ApiError> {
    user.require(user.role.can_manage_resumes())?;
    let resume = state
        .resume_jobs
        .get_resume(user.user_id, resume_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(resume.into()))
}

#[utoipa::path(
    put,
    path = "/api/v1/resumes/{resume_id}",
    tag = "matching",
    security(("bearer_auth" = [])),
    request_body = CreateResumeRequest,
    params(("resume_id" = Uuid, Path)),
    responses(
        (status = 200, body = ResumeResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn update_resume(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(resume_id): ProblemPath<Uuid>,
    ProblemJson(input): ProblemJson<CreateResumeRequest>,
) -> Result<Json<ResumeResponse>, ApiError> {
    user.require(user.role.can_manage_resumes())?;
    let resume = state
        .resume_jobs
        .update_resume(user.user_id, resume_id, input.into())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(resume.into()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/resumes/{resume_id}",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("resume_id" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn delete_resume(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(resume_id): ProblemPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    user.require(user.role.can_manage_resumes())?;
    state
        .resume_jobs
        .delete_resume(user.user_id, resume_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/jobs",
    tag = "matching",
    security(("bearer_auth" = [])),
    request_body = CreateJobRequest,
    responses(
        (status = 201, body = JobResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails)
    )
)]
pub async fn create_job(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    ProblemJson(input): ProblemJson<CreateJobRequest>,
) -> Result<Response, ApiError> {
    user.require(user.role.can_manage_jobs())?;
    if let Some(key) = extract_idempotency_key(&headers)
        && let Some((status, body)) = state.idempotency.get(user.user_id, &key)
    {
        let mut response = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .map_err(|_| ApiError::bad_request("failed to build idempotent response"))?;
        response.headers_mut().insert(
            header::HeaderName::from_static("idempotency-replayed"),
            header::HeaderValue::from_static("true"),
        );
        return Ok(response);
    }
    let job = state
        .resume_jobs
        .create_job(user.user_id, input.into())
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("jobs_created_total").increment(1);
    let body = Json(JobResponse::from(job));
    let json_body = serde_json::to_string(&body.0).unwrap_or_default();
    if let Some(key) = extract_idempotency_key(&headers) {
        state.idempotency.insert(
            user.user_id,
            key,
            StatusCode::CREATED.as_u16(),
            json_body.clone(),
        );
    }
    Ok((StatusCode::CREATED, body).into_response())
}

#[utoipa::path(
    get,
    path = "/api/v1/jobs",
    tag = "matching",
    params(
        ("offset" = Option<usize>, Query),
        ("limit" = Option<usize>, Query),
        ("q" = Option<String>, Query),
        ("skills" = Option<String>, Query),
        ("location" = Option<String>, Query)
    ),
    responses(
        (status = 200, body = JobList),
        (status = 400, body = super::errors::ProblemDetails)
    )
)]
pub async fn list_jobs(
    State(state): State<AppState>,
    ProblemQuery(query): ProblemQuery<JobSearchQuery>,
) -> Result<Json<JobList>, ApiError> {
    let (offset, limit) = query.bounds()?;
    let filter = query.into_filter();
    let has_filter = filter.query.is_some() || filter.skills.is_some() || filter.location.is_some();
    let items = if has_filter {
        state
            .resume_jobs
            .list_jobs_filtered(offset, limit, filter)
            .await
            .map_err(ApiError::from)?
    } else {
        state
            .resume_jobs
            .list_jobs(offset, limit)
            .await
            .map_err(ApiError::from)?
    };
    Ok(Json(JobList {
        items: items.into_iter().map(JobResponse::from).collect(),
        offset,
        limit,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/applications",
    tag = "applications",
    security(("bearer_auth" = [])),
    request_body = CreateApplicationRequest,
    responses(
        (status = 201, body = ApplicationResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 409, body = super::errors::ProblemDetails)
    )
)]
pub async fn create_application(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemJson(input): ProblemJson<CreateApplicationRequest>,
) -> Result<(StatusCode, Json<ApplicationResponse>), ApiError> {
    user.require(user.role.can_manage_resumes())?;
    let application = state
        .resume_jobs
        .apply_to_job(user.user_id, input.resume_id, input.job_id)
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("applications_created_total").increment(1);
    Ok((StatusCode::CREATED, Json(application.into())))
}

#[utoipa::path(
    get,
    path = "/api/v1/jobs/{job_id}/applications",
    tag = "applications",
    security(("bearer_auth" = [])),
    params(("job_id" = Uuid, Path), ("offset" = Option<usize>, Query), ("limit" = Option<usize>, Query)),
    responses(
        (status = 200, body = ApplicationList),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn list_applications(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(job_id): ProblemPath<Uuid>,
    ProblemQuery(query): ProblemQuery<PageQuery>,
) -> Result<Json<ApplicationList>, ApiError> {
    user.require(user.role.can_manage_jobs())?;
    let (offset, limit) = query.bounds()?;
    let items = state
        .resume_jobs
        .list_applications(user.user_id, job_id, offset, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApplicationList {
        items: items.into_iter().map(ApplicationResponse::from).collect(),
        offset,
        limit,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/jobs/{job_id}/recommendations",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(
        ("job_id" = Uuid, Path, description = "Job identifier"),
        ("limit" = Option<usize>, Query, description = "Maximum recommendations, from 1 to 100")
    ),
    responses(
        (status = 200, body = RecommendationList),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn recommendations(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(job_id): ProblemPath<Uuid>,
    ProblemQuery(query): ProblemQuery<RecommendationQuery>,
) -> Result<Json<RecommendationList>, ApiError> {
    user.require(user.role.can_manage_jobs())?;
    if !(1..=100).contains(&query.limit) {
        return Err(ApiError::bad_request("limit must be between 1 and 100"));
    }
    let items = state
        .matching
        .recommendations_for_job(job_id, user.user_id, query.limit)
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("recommendation_requests_total").increment(1);
    Ok(Json(RecommendationList {
        items: items
            .into_iter()
            .map(RecommendationResponse::from)
            .collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/matches",
    tag = "matching",
    security(("bearer_auth" = [])),
    request_body = CreateMatchRequest,
    responses(
        (status = 201, body = MatchResultResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn create_match(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemJson(input): ProblemJson<CreateMatchRequest>,
) -> Result<(StatusCode, Json<MatchResultResponse>), ApiError> {
    let result = state
        .matching
        .create_match(user.user_id, input.resume_id, input.job_id)
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("matches_created_total").increment(1);
    Ok((StatusCode::CREATED, Json(result.into())))
}

#[utoipa::path(
    get,
    path = "/api/v1/matches",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("offset" = Option<usize>, Query), ("limit" = Option<usize>, Query)),
    responses(
        (status = 200, body = MatchResultList),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails)
    )
)]
pub async fn list_matches(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemQuery(query): ProblemQuery<PageQuery>,
) -> Result<Json<MatchResultList>, ApiError> {
    let (offset, limit) = query.bounds()?;
    let items = state
        .matching
        .list_matches(user.user_id, offset, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(MatchResultList {
        items: items.into_iter().map(MatchResultResponse::from).collect(),
        offset,
        limit,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/reports/{match_id}",
    tag = "reports",
    security(("bearer_auth" = [])),
    params(("match_id" = Uuid, Path)),
    responses(
        (status = 200, body = MatchResultResponse),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn get_report(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(match_id): ProblemPath<Uuid>,
) -> Result<Json<MatchResultResponse>, ApiError> {
    let result = state
        .matching
        .get_report(user.user_id, match_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(result.into()))
}

#[utoipa::path(
    post,
    path = "/api/v1/resumes/upload",
    tag = "matching",
    security(("bearer_auth" = [])),
    responses(
        (status = 201, body = ResumeResponse),
        (status = 400, body = super::errors::ProblemDetails),
        (status = 401, body = super::errors::ProblemDetails),
        (status = 403, body = super::errors::ProblemDetails)
    )
)]
pub async fn upload_resume(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ResumeResponse>), ApiError> {
    user.require(user.role.can_manage_resumes())?;
    let mut filename: Option<String> = None;
    let mut declared_mime: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut title: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("invalid multipart: {error}")))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_owned());
                declared_mime = field.content_type().map(|mime| mime.to_string());
                let data = field.bytes().await.map_err(|error| {
                    ApiError::bad_request(format!("could not read file: {error}"))
                })?;
                if data.len() > crate::infrastructure::upload::MAX_UPLOAD_BYTES {
                    return Err(ApiError::bad_request(format!(
                        "file exceeds the {} byte limit",
                        crate::infrastructure::upload::MAX_UPLOAD_BYTES
                    )));
                }
                bytes = Some(data.to_vec());
            }
            "title" => {
                let text = field
                    .text()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("invalid title: {error}")))?;
                let trimmed = text.trim().to_owned();
                if !trimmed.is_empty() {
                    if trimmed.len() > 200 {
                        return Err(ApiError::bad_request(
                            "title must be at most 200 characters",
                        ));
                    }
                    title = Some(trimmed);
                }
            }
            _ => {}
        }
    }

    let filename = filename.ok_or_else(|| ApiError::bad_request("missing file field"))?;
    let bytes = bytes.ok_or_else(|| ApiError::bad_request("missing file data"))?;

    let resume = state
        .resume_jobs
        .create_resume_from_upload(
            user.user_id,
            &filename,
            declared_mime.as_deref(),
            bytes,
            title,
        )
        .await
        .map_err(ApiError::from)?;
    metrics::counter!("resumes_created_total").increment(1);
    metrics::counter!("resume_uploads_total").increment(1);
    Ok((StatusCode::CREATED, Json(resume.into())))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/users",
    tag = "admin",
    security(("bearer_auth" = [])),
    params(("offset" = Option<usize>, Query), ("limit" = Option<usize>, Query)),
    responses(
        (status = 200, body = UserList),
        (status = 403, body = super::errors::ProblemDetails)
    )
)]
pub async fn admin_list_users(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemQuery(query): ProblemQuery<PageQuery>,
) -> Result<Json<UserList>, ApiError> {
    if user.role != crate::domain::Role::Admin {
        return Err(ApiError::forbidden("Admin role required."));
    }
    let (offset, limit) = query.bounds()?;
    let users = state
        .admin
        .list_users(offset, limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(UserList {
        items: users.into_iter().map(UserResponse::from).collect(),
        offset,
        limit,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/jobs/{job_id}/interview-questions",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("job_id" = Uuid, Path)),
    responses(
        (status = 200, body = InterviewQuestionsResponse),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn interview_questions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(job_id): ProblemPath<Uuid>,
    ProblemQuery(query): ProblemQuery<InterviewQuery>,
) -> Result<Json<InterviewQuestionsResponse>, ApiError> {
    let questions = state
        .resume_jobs
        .generate_interview_questions(job_id, user.user_id, query.resume_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(InterviewQuestionsResponse { job_id, questions }))
}

#[utoipa::path(
    post,
    path = "/api/v1/resumes/{resume_id}/cover-letter",
    tag = "matching",
    security(("bearer_auth" = [])),
    params(("resume_id" = Uuid, Path), ("job_id" = Uuid, Query)),
    responses(
        (status = 200, body = CoverLetterResponse),
        (status = 403, body = super::errors::ProblemDetails),
        (status = 404, body = super::errors::ProblemDetails)
    )
)]
pub async fn cover_letter(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    ProblemPath(resume_id): ProblemPath<Uuid>,
    ProblemQuery(query): ProblemQuery<CoverLetterQuery>,
) -> Result<Json<CoverLetterResponse>, ApiError> {
    let letter = state
        .resume_jobs
        .generate_cover_letter(resume_id, query.job_id, user.user_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(CoverLetterResponse {
        resume_id,
        job_id: query.job_id,
        cover_letter: letter,
    }))
}

pub async fn not_found() -> ApiError {
    ApiError::not_found("No route matches the requested URI.")
}
