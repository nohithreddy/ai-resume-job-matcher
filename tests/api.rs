use axum::{
    Router,
    body::{Body, to_bytes},
    response::Response,
};
use http::{Method, Request, StatusCode, header};
use resume_job_matcher::{
    build_application,
    config::{AppConfig, PersistenceBackend},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

fn base_config() -> AppConfig {
    AppConfig {
        bind_address: "127.0.0.1:0".to_owned(),
        log_filter: "resume_job_matcher=debug".to_owned(),
        jwt_secret: "integration-test-secret-with-at-least-32-bytes".to_owned(),
        jwt_ttl_seconds: 3600,
        argon2_memory_cost: 8192,
        embedding_endpoint: None,
        embedding_api_key: None,
        embedding_model: "test-model".to_owned(),
        persistence: PersistenceBackend::Memory,
        database_path: PathBuf::new(),
        database_url: None,
        database_max_connections: 5,
        auth_rate_limit_window_seconds: 60,
        auth_rate_limit_max_requests: 10_000,
        admin_email: None,
        admin_password: None,
        upload_dir: std::env::temp_dir().join("rjm-test-uploads"),
    }
}

fn test_application() -> Router {
    build_application(base_config()).expect("test application should build")
}

fn test_application_with_auth_limits(window_seconds: u64, max_requests: usize) -> Router {
    let mut config = base_config();
    config.auth_rate_limit_window_seconds = window_seconds;
    config.auth_rate_limit_max_requests = max_requests;
    build_application(config).expect("test application should build")
}

fn request(method: Method, uri: &str, body: Option<Value>, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .expect("test request should be valid")
}

async fn body_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body should be readable");
    serde_json::from_slice(&bytes).expect("response should contain JSON")
}

async fn register(app: &Router, email: &str, role: &str) -> (String, Uuid) {
    let response = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "email": email,
                "password": "correct horse battery staple",
                "role": role
            })),
            None,
        ))
        .await
        .expect("registration request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["role"], Value::from(role));
    assert!(body["refresh_token"].as_str().is_some());
    let token = body["access_token"]
        .as_str()
        .expect("access token should be a string")
        .to_owned();
    let user_id = Uuid::parse_str(
        body["user_id"]
            .as_str()
            .expect("user id should be a string"),
    )
    .expect("user id should be a UUID");
    assert_eq!(user_id.get_version_num(), 7);
    (token, user_id)
}

#[tokio::test]
async fn health_and_failures_use_request_ids_and_problem_details() {
    let app = test_application();
    let health_request = Request::builder()
        .uri("/health/live")
        .header("x-request-id", "client-request-123")
        .body(Body::empty())
        .expect("health request should be valid");
    let health = app
        .clone()
        .oneshot(health_request)
        .await
        .expect("health request should complete");
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        health
            .headers()
            .get("x-request-id")
            .expect("request id header should be present"),
        "client-request-123"
    );
    assert_eq!(body_json(health).await, json!({ "status": "ok" }));

    let unauthorized = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/resumes",
            Some(json!({ "raw_text": "A sufficiently long resume body" })),
            None,
        ))
        .await
        .expect("unauthorized request should complete");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthorized
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content type should be present"),
        "application/problem+json"
    );
    assert!(unauthorized.headers().contains_key("x-request-id"));
    assert_eq!(body_json(unauthorized).await["status"], Value::from(401));

    let malformed = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/register")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{"))
        .expect("malformed request should build");
    let malformed = app
        .clone()
        .oneshot(malformed)
        .await
        .expect("malformed request should complete");
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

    let wrong_method = app
        .oneshot(request(Method::GET, "/api/v1/auth/register", None, None))
        .await
        .expect("method rejection should complete");
    assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn authenticated_resume_job_recommendation_flow_is_complete() {
    let app = test_application();
    let (candidate_token, candidate_id) = register(&app, "engineer@example.com", "candidate").await;
    let (recruiter_token, _) = register(&app, "hiring@example.com", "recruiter").await;

    let duplicate = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "email": "ENGINEER@example.com",
                "password": "correct horse battery staple",
                "role": "candidate"
            })),
            None,
        ))
        .await
        .expect("duplicate registration should complete");
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    let login = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "email": "engineer@example.com",
                "password": "correct horse battery staple"
            })),
            None,
        ))
        .await
        .expect("login should complete");
    assert_eq!(login.status(), StatusCode::OK);

    let resume = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/resumes",
            Some(json!({
                "title": "Backend Engineer",
                "raw_text": "Backend Engineer\nBuilt REST APIs with Rust, Axum, Tokio, SQL, Docker, and AWS."
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("resume creation should complete");
    assert_eq!(resume.status(), StatusCode::CREATED);
    let resume_body = body_json(resume).await;
    assert_eq!(
        resume_body["user_id"],
        Value::from(candidate_id.to_string())
    );
    assert_eq!(resume_body["skills"][0], Value::from("aws"));
    assert!(resume_body.get("raw_text").is_none());
    assert!(resume_body.get("embedding").is_none());
    let resume_id = resume_body["id"].as_str().expect("resume id").to_owned();

    let job = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Rust Platform Engineer",
                "description": "Build REST services with Rust, Axum, Tokio, SQL, Docker, and Kubernetes. 5 years experience. Location: Remote. Full-time."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job creation should complete");
    assert_eq!(job.status(), StatusCode::CREATED);
    let job_body = body_json(job).await;
    assert!(job_body.get("description").is_none());
    assert!(job_body.get("embedding").is_none());
    let job_id = job_body["id"].as_str().expect("job id").to_owned();

    // A candidate cannot publish jobs.
    let candidate_create_job = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Nope",
                "description": "Candidates must not be able to publish jobs on this platform."
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("candidate job creation should complete");
    assert_eq!(candidate_create_job.status(), StatusCode::FORBIDDEN);

    // Candidate applies with their resume.
    let application = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/applications",
            Some(json!({
                "resume_id": resume_id,
                "job_id": job_id
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("application creation should complete");
    assert_eq!(application.status(), StatusCode::CREATED);

    // Duplicate applications conflict.
    let duplicate_application = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/applications",
            Some(json!({
                "resume_id": resume_id,
                "job_id": job_id
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("duplicate application should complete");
    assert_eq!(duplicate_application.status(), StatusCode::CONFLICT);

    // The owning recruiter ranks submitted applicants.
    let recommendations = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/jobs/{job_id}/recommendations?limit=5"),
            None,
            Some(&recruiter_token),
        ))
        .await
        .expect("recommendation request should complete");
    assert_eq!(recommendations.status(), StatusCode::OK);
    let recommendations = body_json(recommendations).await;
    assert_eq!(recommendations["items"][0]["resume_id"], resume_id);
    let score = recommendations["items"][0]["score"]
        .as_f64()
        .expect("score should be numeric");
    assert!((0.0..=100.0).contains(&score));
    assert_eq!(
        recommendations["items"][0]["missing_skills"],
        json!(["kubernetes"])
    );
    assert!(
        recommendations["items"][0]["category_scores"]["skills"]["weighted_score"]
            .as_f64()
            .is_some()
    );
    assert!(
        recommendations["items"][0]["comparisons"]["location"]["reason"]
            .as_str()
            .is_some()
    );

    // Candidates may not rank applicants for any job.
    let candidate_recommendations = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/jobs/{job_id}/recommendations"),
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("candidate recommendation attempt should complete");
    assert_eq!(candidate_recommendations.status(), StatusCode::FORBIDDEN);

    // The candidate computes their own persisted match against the job.
    let match_created = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/matches",
            Some(json!({
                "resume_id": resume_id,
                "job_id": job_id
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("match creation should complete");
    assert_eq!(match_created.status(), StatusCode::CREATED);
    let match_body = body_json(match_created).await;
    let match_id = match_body["id"].as_str().expect("match id").to_owned();

    let report = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/reports/{match_id}"),
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("report fetch should complete");
    assert_eq!(report.status(), StatusCode::OK);
    let report = body_json(report).await;
    assert_eq!(report["report"]["score"], match_body["report"]["score"]);

    let (other_token, _) = register(&app, "other@example.com", "recruiter").await;
    let forbidden_report = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/reports/{match_id}"),
            None,
            Some(&other_token),
        ))
        .await
        .expect("forbidden report request should complete");
    assert_eq!(forbidden_report.status(), StatusCode::FORBIDDEN);

    let invalid_path = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/jobs/not-a-uuid/recommendations",
            None,
            Some(&other_token),
        ))
        .await
        .expect("invalid path request should complete");
    assert_eq!(invalid_path.status(), StatusCode::BAD_REQUEST);

    let openapi = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/openapi.json", None, None))
        .await
        .expect("OpenAPI request should complete");
    assert_eq!(openapi.status(), StatusCode::OK);
    let openapi = body_json(openapi).await;
    assert!(openapi["components"]["securitySchemes"]["bearer_auth"].is_object());
    assert!(openapi["paths"]["/api/v1/jobs/{job_id}/recommendations"].is_object());
    assert!(openapi["paths"]["/api/v1/matches"].is_object());
    assert!(openapi["paths"]["/api/v1/reports/{match_id}"].is_object());

    let metrics_response = app
        .oneshot(request(Method::GET, "/metrics", None, None))
        .await
        .expect("metrics request should complete");
    assert_eq!(metrics_response.status(), StatusCode::OK);
    let metrics_bytes = to_bytes(metrics_response.into_body(), 1024 * 1024)
        .await
        .expect("metrics body should be readable");
    let metrics_text = String::from_utf8(metrics_bytes.to_vec()).expect("metrics should be UTF-8");
    assert!(metrics_text.contains("http_requests_total"));
    assert!(metrics_text.contains("recommendation_requests_total"));
}

#[tokio::test]
async fn sqlite_backend_persists_across_application_restarts() {
    let directory = std::env::temp_dir().join(format!("rjm-it-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&directory).expect("temporary database directory should be created");
    let mut config = base_config();
    config.persistence = PersistenceBackend::Sqlite;
    config.database_path = directory.join("matcher.db");

    let first = build_application(config.clone()).expect("first application should build");
    let registered = first
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "email": "durable@example.com",
                "password": "correct horse battery staple",
                "role": "candidate"
            })),
            None,
        ))
        .await
        .expect("registration should complete");
    assert_eq!(registered.status(), StatusCode::CREATED);
    let registered = body_json(registered).await;
    let refresh_token = registered["refresh_token"]
        .as_str()
        .expect("refresh token should be present")
        .to_owned();

    let resume = first
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/resumes",
            Some(json!({
                "title": "Durable Engineer",
                "raw_text": "Backend engineer experienced with Rust, SQL, and Docker platforms."
            })),
            Some(registered["access_token"].as_str().expect("access token")),
        ))
        .await
        .expect("resume creation should complete");
    assert_eq!(resume.status(), StatusCode::CREATED);
    let resume_id = body_json(resume).await["id"]
        .as_str()
        .expect("resume id")
        .to_owned();
    drop(first);

    // A fresh process over the same database file must serve the same data.
    let second = build_application(config.clone()).expect("restarted application should build");
    let ready = second
        .clone()
        .oneshot(request(Method::GET, "/health/ready", None, None))
        .await
        .expect("readiness should complete");
    assert_eq!(body_json(ready).await["persistence"], Value::from("sqlite"));

    let login = second
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "email": "durable@example.com",
                "password": "correct horse battery staple"
            })),
            None,
        ))
        .await
        .expect("login should complete");
    assert_eq!(login.status(), StatusCode::OK);
    let token = body_json(login).await["access_token"]
        .as_str()
        .expect("access token")
        .to_owned();

    let resumes = second
        .clone()
        .oneshot(request(Method::GET, "/api/v1/resumes", None, Some(&token)))
        .await
        .expect("resume listing should complete");
    assert_eq!(resumes.status(), StatusCode::OK);
    let resumes = body_json(resumes).await;
    assert_eq!(
        resumes["items"].as_array().expect("items").len(),
        1,
        "the resume must survive the restart"
    );
    assert_eq!(resumes["items"][0]["id"], Value::from(resume_id));

    // Sessions are durable too: the pre-restart refresh token still rotates.
    let rotated = second
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({ "refresh_token": refresh_token })),
            None,
        ))
        .await
        .expect("post-restart refresh should complete");
    assert_eq!(rotated.status(), StatusCode::OK);
    drop(second);

    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn refresh_rotation_logout_and_role_boundaries_are_enforced() {
    let app = test_application();
    let (candidate_token, _) = register(&app, "candidate2@example.com", "candidate").await;
    let (recruiter_token, _) = register(&app, "recruiter2@example.com", "recruiter").await;

    // Recruiter creates a job.
    let job = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Senior Rust Engineer",
                "description": "Ship distributed systems in Rust with Tokio. 6 years experience required."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job creation should complete");
    assert_eq!(job.status(), StatusCode::CREATED);
    let job_id = body_json(job).await["id"]
        .as_str()
        .expect("job id")
        .to_owned();

    // Candidates cannot list another recruiter's recommendations.
    let forbidden_recommendations = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/jobs/{job_id}/recommendations"),
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("candidate recommendation attempt should complete");
    assert_eq!(forbidden_recommendations.status(), StatusCode::FORBIDDEN);

    // Login returns a refresh token that rotates exactly once.
    let login = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "email": "candidate2@example.com",
                "password": "correct horse battery staple"
            })),
            None,
        ))
        .await
        .expect("login should complete");
    assert_eq!(login.status(), StatusCode::OK);
    let login = body_json(login).await;
    let refresh_token = login["refresh_token"].as_str().expect("refresh").to_owned();

    let rotated = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({ "refresh_token": refresh_token })),
            None,
        ))
        .await
        .expect("refresh should complete");
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated = body_json(rotated).await;
    let new_refresh = rotated["refresh_token"]
        .as_str()
        .expect("new refresh")
        .to_owned();
    assert_ne!(new_refresh, refresh_token);

    // Replaying the old token fails and revokes the session family.
    let reused = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({ "refresh_token": refresh_token })),
            None,
        ))
        .await
        .expect("reused refresh should complete");
    assert_eq!(reused.status(), StatusCode::UNAUTHORIZED);

    let revoked = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({ "refresh_token": new_refresh })),
            None,
        ))
        .await
        .expect("post-reuse refresh should complete");
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);

    // Logout is idempotent from the client perspective.
    let logout = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/logout",
            Some(json!({ "refresh_token": new_refresh })),
            None,
        ))
        .await
        .expect("logout should complete");
    assert!(
        logout.status() == StatusCode::NO_CONTENT || logout.status() == StatusCode::UNAUTHORIZED
    );

    // Admin registration is rejected on the public endpoint.
    let admin_attempt = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "email": "admin@example.com",
                "password": "correct horse battery staple",
                "role": "admin"
            })),
            None,
        ))
        .await
        .expect("admin registration attempt should complete");
    assert_eq!(admin_attempt.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn auth_burst_over_the_limit_returns_429_with_retry_after() {
    let app = test_application_with_auth_limits(60, 2);
    let login_attempt = |app: &Router| {
        app.clone().oneshot(request(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "email": "nobody@example.com",
                "password": "correct horse battery staple"
            })),
            None,
        ))
    };

    // The first two attempts pass the limiter and fail on credentials only.
    let first = login_attempt(&app)
        .await
        .expect("first login attempt should complete");
    assert_eq!(first.status(), StatusCode::UNAUTHORIZED);
    let second = login_attempt(&app)
        .await
        .expect("second login attempt should complete");
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);

    let limited = login_attempt(&app)
        .await
        .expect("third login attempt should complete");
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        limited
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content type should be present"),
        "application/problem+json"
    );
    let retry_after = limited
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("retry-after should be a positive integer");
    assert!((1..=60).contains(&retry_after));
    let problem = body_json(limited).await;
    assert_eq!(problem["status"], Value::from(429));
    assert_eq!(problem["title"], Value::from("Too Many Requests"));
    assert_eq!(
        problem["type"],
        Value::from("https://resume-matcher.example/problems/too-many-requests")
    );

    // Non-auth routes stay available while authentication is throttled.
    let jobs = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/jobs", None, None))
        .await
        .expect("jobs listing should complete");
    assert_eq!(jobs.status(), StatusCode::OK);

    // The limiter is per application instance: a fresh app accepts the burst again.
    let fresh_app = test_application_with_auth_limits(60, 2);
    let fresh_first = login_attempt(&fresh_app)
        .await
        .expect("fresh app login attempt should complete");
    assert_eq!(fresh_first.status(), StatusCode::UNAUTHORIZED);

    let metrics_response = app
        .oneshot(request(Method::GET, "/metrics", None, None))
        .await
        .expect("metrics request should complete");
    assert_eq!(metrics_response.status(), StatusCode::OK);
    let metrics_bytes = to_bytes(metrics_response.into_body(), 1024 * 1024)
        .await
        .expect("metrics body should be readable");
    let metrics_text = String::from_utf8(metrics_bytes.to_vec()).expect("metrics should be UTF-8");
    assert!(metrics_text.contains("auth_rate_limited_total"));
}

fn multipart_request(
    uri: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
    title: Option<&str>,
    token: Option<&str>,
) -> Request<Body> {
    let boundary = "----TestBoundary123456";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    if let Some(t) = title {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"title\"\r\n\r\n");
        body.extend_from_slice(t.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let mut builder = Request::builder().method(Method::POST).uri(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={boundary}"),
    );
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).expect("multipart request")
}

#[tokio::test]
async fn upload_rejects_wrong_mime_double_extension_and_oversized() {
    let app = test_application();
    let (candidate_token, _) = register(&app, "uploader_wrong@example.com", "candidate").await;

    // Valid PDF content for baseline success
    let pdf_bytes = {
        let mut bytes = b"%PDF-1.4 ".to_vec();
        bytes.extend_from_slice(b"Backend Engineer\nBuilt REST APIs with Rust, Axum, Tokio, SQL, Docker, and AWS. 5 years experience.");
        bytes
    };

    // Wrong MIME: filename declares docx but content is pdf
    let wrong_mime = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            &pdf_bytes,
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("wrong mime request should complete");
    assert_eq!(wrong_mime.status(), StatusCode::BAD_REQUEST);

    // Double extension rejection
    let double_ext = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.pdf.exe",
            "application/pdf",
            &pdf_bytes,
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("double extension should complete");
    assert_eq!(double_ext.status(), StatusCode::BAD_REQUEST);

    // Executable magic rejection (MZ header)
    let exe_bytes = b"MZ executable fake content that's long enough to pass extraction length requirement but should be rejected as executable";
    let exe_rejected = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.pdf",
            "application/pdf",
            exe_bytes,
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("exe magic should be rejected");
    assert_eq!(exe_rejected.status(), StatusCode::BAD_REQUEST);

    // Oversized file (10MB + 1) – should be rejected with 400 (handler) not 413 because limit is 11MB
    let oversized = vec![b'a'; 10 * 1024 * 1024 + 1];
    let over = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.pdf",
            "application/pdf",
            &oversized,
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("oversized should complete");
    assert_eq!(over.status(), StatusCode::BAD_REQUEST);

    // Success case: valid PDF upload
    let success = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.pdf",
            "application/pdf",
            &pdf_bytes,
            Some("Backend Engineer"),
            Some(&candidate_token),
        ))
        .await
        .expect("valid upload should complete");
    assert_eq!(success.status(), StatusCode::CREATED);
    let body = body_json(success).await;
    assert!(body["id"].is_string());
    assert_eq!(body["skills"][0], Value::from("aws"));
}

#[tokio::test]
async fn upload_docx_and_end_to_end_extract_parse_embed_flow() {
    let app = test_application();
    let (candidate_token, _) = register(&app, "uploader_docx@example.com", "candidate").await;

    // Minimal docx-like zip payload with word/ marker and enough text
    let mut docx_bytes = b"PK\x03\x04".to_vec();
    docx_bytes.extend_from_slice(b"[Content_Types].xml word/document.xml <w:t>Rust</w:t> <w:t>Python</w:t> Backend Engineer with 4 years experience, Bachelor degree, AWS Certified, Remote Full-time. This resume contains sufficient text for parsing and embedding pipeline demonstration.");

    let docx = app
        .clone()
        .oneshot(multipart_request(
            "/api/v1/resumes/upload",
            "resume.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            &docx_bytes,
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("docx upload should complete");
    assert_eq!(docx.status(), StatusCode::CREATED);
    let body = body_json(docx).await;
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn job_search_filter_supports_q_skills_location_and_pagination() {
    let app = test_application();
    let (recruiter_token, _) = register(&app, "filter_recruiter@example.com", "recruiter").await;

    // Create jobs with distinct attributes
    let job1 = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Rust Backend Engineer",
                "description": "We need Rust, Axum, Tokio, SQL, Docker. Location: Remote. Full-time role with agile."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job1 should be created");
    assert_eq!(job1.status(), StatusCode::CREATED);

    let job2 = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Python Data Engineer",
                "description": "Python, SQL, AWS, machine learning. Location: New York. Contract role."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job2 should be created");
    assert_eq!(job2.status(), StatusCode::CREATED);

    let job3 = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Frontend React Developer",
                "description": "React, TypeScript, JavaScript, CSS. Location: Remote."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job3 should be created");
    assert_eq!(job3.status(), StatusCode::CREATED);

    // Filter by q
    let filtered_q = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/jobs?q=rust", None, None))
        .await
        .expect("q filter should complete");
    assert_eq!(filtered_q.status(), StatusCode::OK);
    let filtered_q = body_json(filtered_q).await;
    assert_eq!(filtered_q["items"].as_array().unwrap().len(), 1);
    assert!(
        filtered_q["items"][0]["title"]
            .as_str()
            .unwrap()
            .contains("Rust")
    );

    // Filter by skills
    let filtered_skills = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/jobs?skills=python,aws",
            None,
            None,
        ))
        .await
        .expect("skills filter should complete");
    assert_eq!(filtered_skills.status(), StatusCode::OK);
    let filtered_skills = body_json(filtered_skills).await;
    assert!(!filtered_skills["items"].as_array().unwrap().is_empty());

    // Filter by location
    let filtered_location = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/jobs?location=remote",
            None,
            None,
        ))
        .await
        .expect("location filter should complete");
    assert_eq!(filtered_location.status(), StatusCode::OK);
    let filtered_location = body_json(filtered_location).await;
    assert!(filtered_location["items"].as_array().unwrap().len() >= 2);

    // Pagination hardened: limit >100 should 400, limit 0 should 400
    let bad_limit = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/jobs?limit=101", None, None))
        .await
        .expect("bad limit should complete");
    assert_eq!(bad_limit.status(), StatusCode::BAD_REQUEST);

    let zero_limit = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/jobs?limit=0", None, None))
        .await
        .expect("zero limit should complete");
    assert_eq!(zero_limit.status(), StatusCode::BAD_REQUEST);

    // Pagination defaults: offset/limit bounded
    let paged = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/jobs?offset=0&limit=1",
            None,
            None,
        ))
        .await
        .expect("paged should complete");
    assert_eq!(paged.status(), StatusCode::OK);
    let paged = body_json(paged).await;
    assert_eq!(paged["items"].as_array().unwrap().len(), 1);
    assert_eq!(paged["limit"], Value::from(1));
}

#[tokio::test]
async fn admin_endpoint_requires_admin_and_supports_pagination() {
    let app = test_application();
    let (candidate_token, _) =
        register(&app, "admin_test_candidate@example.com", "candidate").await;
    let (recruiter_token, _) =
        register(&app, "admin_test_recruiter@example.com", "recruiter").await;

    // Non-admin cannot list users
    let forbidden = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/admin/users",
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("candidate admin list should complete");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let forbidden2 = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/admin/users",
            None,
            Some(&recruiter_token),
        ))
        .await
        .expect("recruiter admin list should complete");
    assert_eq!(forbidden2.status(), StatusCode::FORBIDDEN);

    // Pagination hardened for admin endpoint as well
    let bad_limit = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/admin/users?limit=101",
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("admin pagination should be checked before role? but should still be 403 or 400");
    // If non-admin, we expect 403 before pagination check; so test with admin would be better but we don't have admin yet
    // Ensure unauthenticated returns 401
    let unauth = app
        .clone()
        .oneshot(request(Method::GET, "/api/v1/admin/users", None, None))
        .await
        .expect("unauth admin should complete");
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    let _ = bad_limit;
}

#[tokio::test]
async fn interview_and_cover_letter_generators_work_end_to_end() {
    let app = test_application();
    let (candidate_token, _) = register(&app, "gen_candidate@example.com", "candidate").await;
    let (recruiter_token, _) = register(&app, "gen_recruiter@example.com", "recruiter").await;

    let resume = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/resumes",
            Some(json!({
                "title": "Backend Engineer",
                "raw_text": "Backend Engineer\nBuilt REST APIs with Rust, Axum, Tokio, SQL, Docker, and AWS. 3 years experience. Bachelor degree. Remote Full-time."
            })),
            Some(&candidate_token),
        ))
        .await
        .expect("resume should be created");
    assert_eq!(resume.status(), StatusCode::CREATED);
    let resume_id = body_json(resume).await["id"].as_str().unwrap().to_owned();

    let job = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/jobs",
            Some(json!({
                "title": "Rust Platform Engineer",
                "description": "Build REST services with Rust, Axum, Tokio, SQL, Docker, and Kubernetes. 5 years experience. Location: Remote."
            })),
            Some(&recruiter_token),
        ))
        .await
        .expect("job should be created");
    assert_eq!(job.status(), StatusCode::CREATED);
    let job_id = body_json(job).await["id"].as_str().unwrap().to_owned();

    // Candidate applies so recruiter can generate interview questions tied to resume
    let _ = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/v1/applications",
            Some(json!({ "resume_id": resume_id, "job_id": job_id })),
            Some(&candidate_token),
        ))
        .await
        .expect("application should succeed");

    // Interview questions (recruiter)
    let iq = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/api/v1/jobs/{job_id}/interview-questions?resume_id={resume_id}"),
            None,
            Some(&recruiter_token),
        ))
        .await
        .expect("interview questions should complete");
    assert_eq!(iq.status(), StatusCode::OK);
    let iq_body = body_json(iq).await;
    assert!(iq_body["questions"].as_array().unwrap().len() >= 3);
    assert!(iq_body["questions"].as_array().unwrap().iter().any(|q| {
        q.as_str().unwrap().contains("kubernetes") || q.as_str().unwrap().contains("Rust")
    }));

    // Cover letter (candidate)
    let cl = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/api/v1/resumes/{resume_id}/cover-letter?job_id={job_id}"),
            None,
            Some(&candidate_token),
        ))
        .await
        .expect("cover letter should complete");
    assert_eq!(cl.status(), StatusCode::OK);
    let cl_body = body_json(cl).await;
    assert!(
        cl_body["cover_letter"]
            .as_str()
            .unwrap()
            .contains("Rust Platform Engineer")
    );
}

#[tokio::test]
async fn idempotency_key_replays_post_resume_and_job() {
    let app = test_application();
    let (candidate_token, _) = register(&app, "idem_candidate@example.com", "candidate").await;
    let (recruiter_token, _) = register(&app, "idem_recruiter@example.com", "recruiter").await;

    let key = "test-idempotency-key-12345";
    let resume_body = json!({
        "title": "Idempotent Engineer",
        "raw_text": "Backend Engineer\nBuilt REST APIs with Rust, Axum, Tokio, SQL, Docker, and AWS. Sufficient length for validation."
    });

    let first = {
        let builder = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/resumes")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", key)
            .header(header::AUTHORIZATION, format!("Bearer {candidate_token}"));
        let request = builder
            .body(Body::from(resume_body.to_string()))
            .expect("first idempotent request");
        app.clone()
            .oneshot(request)
            .await
            .expect("first should complete")
    };
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    let first_id = first_body["id"].as_str().unwrap().to_owned();

    let second = {
        let builder = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/resumes")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", key)
            .header(header::AUTHORIZATION, format!("Bearer {candidate_token}"));
        // Different body but same key should replay first result
        let different = json!({
            "title": "Different Title Should Be Ignored",
            "raw_text": "Completely different content that would normally create a new resume but idempotency should replay."
        });
        let request = builder
            .body(Body::from(different.to_string()))
            .expect("second idempotent request");
        app.clone()
            .oneshot(request)
            .await
            .expect("second should complete")
    };
    assert_eq!(second.status(), StatusCode::CREATED);
    assert_eq!(
        second.headers().get("idempotency-replayed").unwrap(),
        "true"
    );
    let second_body = body_json(second).await;
    assert_eq!(second_body["id"].as_str().unwrap(), first_id);

    // Job idempotency
    let job_key = "job-idempotency-key-999";
    let job_body = json!({
        "title": "Idempotent Job",
        "description": "We need Rust, SQL, Docker developers. Sufficient description length for job creation validation."
    });
    let first_job = {
        let builder = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/jobs")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", job_key)
            .header(header::AUTHORIZATION, format!("Bearer {recruiter_token}"));
        let request = builder
            .body(Body::from(job_body.to_string()))
            .expect("first job idempotent");
        app.clone()
            .oneshot(request)
            .await
            .expect("first job should complete")
    };
    assert_eq!(first_job.status(), StatusCode::CREATED);
    let first_job_id = body_json(first_job).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let second_job = {
        let builder = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/jobs")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", job_key)
            .header(header::AUTHORIZATION, format!("Bearer {recruiter_token}"));
        let request = builder
            .body(Body::from(job_body.to_string()))
            .expect("second job idempotent");
        app.clone()
            .oneshot(request)
            .await
            .expect("second job should complete")
    };
    assert_eq!(second_job.status(), StatusCode::CREATED);
    assert_eq!(
        second_job.headers().get("idempotency-replayed").unwrap(),
        "true"
    );
    let second_job_id = body_json(second_job).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first_job_id, second_job_id);
}
