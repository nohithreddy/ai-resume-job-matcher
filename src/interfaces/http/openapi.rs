use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use super::routes::*;
use super::{errors::ProblemDetails, routes};

#[derive(OpenApi)]
#[openapi(
    paths(
        routes::health_live,
        routes::health_ready,
        routes::register,
        routes::login,
        routes::refresh,
        routes::logout,
        routes::create_resume,
        routes::upload_resume,
        routes::list_resumes,
        routes::get_resume,
        routes::update_resume,
        routes::delete_resume,
        routes::cover_letter,
        routes::create_job,
        routes::list_jobs,
        routes::create_application,
        routes::list_applications,
        routes::recommendations,
        routes::interview_questions,
        routes::create_match,
        routes::list_matches,
        routes::get_report,
        routes::admin_list_users
    ),
    components(schemas(
        HealthResponse,
        ReadyResponse,
        RegisterRequest,
        LoginRequest,
        RefreshRequest,
        LogoutRequest,
        AuthResponse,
        CreateResumeRequest,
        CreateJobRequest,
        CreateApplicationRequest,
        CreateMatchRequest,
        ResumeResponse,
        ResumeList,
        JobResponse,
        JobList,
        ApplicationResponse,
        ApplicationList,
        RecommendationResponse,
        RecommendationList,
        MatchResultResponse,
        MatchResultList,
        CategoryScoreResponse,
        CategoryScoresResponse,
        AttributeComparisonResponse,
        ContextComparisonsResponse,
        UserResponse,
        UserList,
        InterviewQuestionsResponse,
        CoverLetterResponse,
        crate::domain::entities::Role,
        crate::domain::entities::ApplicationStatus,
        crate::domain::entities::ComparisonOutcome,
        ProblemDetails
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "health", description = "Service health"),
        (name = "auth", description = "Authentication and sessions"),
        (name = "matching", description = "Resumes, jobs, and ATS scoring"),
        (name = "applications", description = "Job applications"),
        (name = "reports", description = "Persisted match reports"),
        (name = "admin", description = "Administration")
    ),
    info(
        title = "Resume Job Matcher API",
        version = "0.1.0",
        description = "Authenticated resume ingestion, job ingestion, weighted ATS matching, applications, and explainable reports"
    )
)]
pub struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
        }
    }
}
