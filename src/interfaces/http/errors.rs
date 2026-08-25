use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    response::{IntoResponse, Response},
};
use http::{StatusCode, header};
use serde::Serialize;
use tracing::error;

use crate::domain::DomainError;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    problem_type: &'static str,
    title: &'static str,
    detail: String,
    retry_after_seconds: Option<u64>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub problem_type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
}

impl ApiError {
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "https://resume-matcher.example/problems/unauthorized",
            "Unauthorized",
            detail,
        )
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "https://resume-matcher.example/problems/forbidden",
            "Forbidden",
            detail,
        )
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "https://resume-matcher.example/problems/bad-request",
            "Bad Request",
            detail,
        )
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "https://resume-matcher.example/problems/not-found",
            "Not Found",
            detail,
        )
    }

    pub fn unsupported_media_type(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "https://resume-matcher.example/problems/unsupported-media-type",
            "Unsupported Media Type",
            detail,
        )
    }

    pub fn unprocessable_entity(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "https://resume-matcher.example/problems/validation-error",
            "Validation Error",
            detail,
        )
    }

    pub fn payload_too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "https://resume-matcher.example/problems/payload-too-large",
            "Payload Too Large",
            "The request body exceeds the 512 KiB limit.",
        )
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "https://resume-matcher.example/problems/method-not-allowed",
            "Method Not Allowed",
            "The HTTP method is not supported for this resource.",
        )
    }

    pub fn too_many_requests(retry_after_seconds: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            problem_type: "https://resume-matcher.example/problems/too-many-requests",
            title: "Too Many Requests",
            detail: "Too many requests from this address. Retry after the indicated delay."
                .to_owned(),
            retry_after_seconds: Some(retry_after_seconds.max(1)),
        }
    }

    pub fn service_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "https://resume-matcher.example/problems/dependency-unavailable",
            "Service Unavailable",
            detail,
        )
    }

    pub fn from_json(rejection: JsonRejection) -> Self {
        match rejection.status() {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                Self::unsupported_media_type("Content-Type must be application/json.")
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                Self::unprocessable_entity("The JSON body failed validation.")
            }
            StatusCode::PAYLOAD_TOO_LARGE => Self::payload_too_large(),
            _ => Self::bad_request("The request body is not valid JSON."),
        }
    }

    pub fn from_query(rejection: QueryRejection) -> Self {
        let _ = rejection;
        Self::bad_request("The query string is invalid.")
    }

    pub fn from_path(rejection: PathRejection) -> Self {
        let _ = rejection;
        Self::bad_request("A path parameter is invalid.")
    }

    fn new(
        status: StatusCode,
        problem_type: &'static str,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status,
            problem_type,
            title,
            detail: detail.into(),
            retry_after_seconds: None,
        }
    }
}

impl From<DomainError> for ApiError {
    fn from(error_value: DomainError) -> Self {
        match error_value {
            DomainError::NotFound => Self::not_found("The requested resource was not found."),
            DomainError::Conflict => Self::new(
                StatusCode::CONFLICT,
                "https://resume-matcher.example/problems/conflict",
                "Conflict",
                "The resource already exists.",
            ),
            DomainError::Unauthorized => Self::unauthorized("Valid credentials are required."),
            DomainError::Forbidden => Self::forbidden("You do not have access to this resource."),
            DomainError::InvalidInput(detail) => Self::bad_request(detail),
            DomainError::EmbeddingDimensionMismatch => {
                error!("stored embedding dimensions do not match");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "https://resume-matcher.example/problems/internal-error",
                    "Internal Server Error",
                    "The service could not complete the request.",
                )
            }
            DomainError::InvalidEmbedding => {
                error!("embedding provider returned an invalid vector");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "https://resume-matcher.example/problems/internal-error",
                    "Internal Server Error",
                    "The service could not complete the request.",
                )
            }
            DomainError::DependencyUnavailable(detail) => {
                error!(%detail, "dependency unavailable");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "https://resume-matcher.example/problems/dependency-unavailable",
                    "Service Unavailable",
                    "A required dependency is unavailable.",
                )
            }
            DomainError::Internal(detail) => {
                error!(%detail, "internal application error");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "https://resume-matcher.example/problems/internal-error",
                    "Internal Server Error",
                    "The service could not complete the request.",
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let problem = ProblemDetails {
            problem_type: self.problem_type.to_owned(),
            title: self.title.to_owned(),
            status: self.status.as_u16(),
            detail: self.detail,
        };
        let mut response = (self.status, Json(problem)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        if let Some(seconds) = self.retry_after_seconds
            && let Ok(value) = header::HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}
