//! Resume and job matching service.
//!
//! The crate is intentionally split into domain, application, infrastructure, and
//! HTTP interface modules. Persistence is resolved at startup through
//! `infrastructure::open_repositories`: SQLite by default, in-memory opt-in.

pub mod application;
pub mod config;
pub mod domain;
pub mod infrastructure;
pub mod interfaces;

use std::sync::Arc;

use application::{AdminService, AuthService, MatchingService, ResumeJobService};
use config::AppConfig;
use domain::CosineSimilarity;
use infrastructure::{
    ClamAvScanner, DeterministicEmbeddingProvider, DeterministicInterviewGenerator,
    DeterministicJobParser, DeterministicResumeParser, HttpEmbeddingProvider, PasswordHasher,
    SecurityService, StubTextExtractor, TemplateCoverLetterGenerator,
};
use interfaces::http::{
    AppState, AuthRateLimiter, BootstrapError, build_router, install_metrics_recorder,
};

/// Builds the application state and router used by the binary and integration tests.
pub fn build_application(config: AppConfig) -> Result<axum::Router, BootstrapError> {
    let persistence_label = infrastructure::RepositorySet::label(&config);
    let rate_limit_max_requests = config.auth_rate_limit_max_requests;
    let rate_limit_window_seconds = config.auth_rate_limit_window_seconds;
    let repositories =
        infrastructure::open_repositories(&config).map_err(BootstrapError::database)?;
    let metrics = install_metrics_recorder()?;
    let concrete_hasher = Arc::new(PasswordHasher::new(config.argon2_memory_cost));
    let dummy_password_hash = concrete_hasher
        .hash("timing-equalization-password")
        .map_err(BootstrapError::password_hasher)?;
    let hasher_for_seed: Arc<PasswordHasher> = Arc::clone(&concrete_hasher);
    let password_service: Arc<dyn domain::PasswordService> = concrete_hasher;
    let security = Arc::new(SecurityService::new(
        config.jwt_secret.clone(),
        config.jwt_ttl_seconds,
    ));
    let auth = Arc::new(AuthService::new(
        repositories.users.clone(),
        password_service,
        security,
        dummy_password_hash,
    ));
    let embeddings: Arc<dyn domain::EmbeddingProvider> = match &config.embedding_endpoint {
        Some(endpoint) => Arc::new(HttpEmbeddingProvider::new(
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(BootstrapError::embedding_client)?,
            endpoint.clone(),
            config.embedding_api_key.clone(),
            config.embedding_model.clone(),
        )),
        None => Arc::new(DeterministicEmbeddingProvider::default()),
    };
    let resume_jobs = Arc::new(ResumeJobService::with_upload_deps(
        repositories.resumes.clone(),
        repositories.jobs.clone(),
        repositories.applications.clone(),
        Arc::new(DeterministicResumeParser),
        Arc::new(DeterministicJobParser),
        embeddings,
        Arc::new(StubTextExtractor),
        Arc::new(ClamAvScanner::new()),
        config.upload_dir.clone(),
        Arc::new(DeterministicInterviewGenerator),
        Arc::new(TemplateCoverLetterGenerator),
    ));
    let matching = Arc::new(MatchingService::new(
        repositories.resumes.clone(),
        repositories.jobs.clone(),
        repositories.applications.clone(),
        repositories.matches.clone(),
        Arc::new(CosineSimilarity),
    ));
    let admin = Arc::new(AdminService::new(repositories.users.clone()));

    // Seed admin user if env vars are set – best-effort, runs in background thread to avoid blocking build
    if let (Some(admin_email), Some(admin_password)) =
        (config.admin_email.clone(), config.admin_password.clone())
    {
        let users_for_seed = repositories.users.clone();
        let hasher_for_seed = Arc::clone(&hasher_for_seed);
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => {
                    tracing::error!(%error, "failed to build admin seed runtime");
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(error) =
                    seed_admin_user(users_for_seed, hasher_for_seed, admin_email, admin_password)
                        .await
                {
                    tracing::error!(%error, "admin seeding failed");
                }
            });
        });
    }

    let state = AppState {
        config: Arc::new(config),
        auth,
        resume_jobs,
        matching,
        admin,
        idempotency: Arc::new(infrastructure::IdempotencyStore::default()),
        metrics,
        persistence_label,
        rate_limiter: Arc::new(AuthRateLimiter::new(
            u32::try_from(rate_limit_max_requests).unwrap_or(u32::MAX),
            rate_limit_window_seconds,
        )),
    };
    Ok(build_router(state))
}

async fn seed_admin_user(
    users: Arc<dyn domain::UserRepository>,
    hasher: Arc<PasswordHasher>,
    email: String,
    password: String,
) -> Result<(), String> {
    use domain::{Role, User};
    if users
        .find_by_email(&email)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        tracing::info!(%email, "admin user already exists, skipping seed");
        return Ok(());
    }
    let hash = hasher
        .hash(&password)
        .map_err(|e| format!("hash failed: {e}"))?;
    let user = User {
        id: uuid::Uuid::now_v7(),
        email: email.clone(),
        password_hash: hash,
        role: Role::Admin,
        created_at: chrono::Utc::now(),
    };
    users.create(user).await.map_err(|e| e.to_string())?;
    tracing::info!(%email, "admin user seeded");
    Ok(())
}
