//! Durable PostgreSQL adapter (targeting managed providers such as Neon)
//! implementing every repository port.
//!
//! Connections come from a `deadpool-postgres` pool sized by
//! `APP_DATABASE_MAX_CONNECTIONS`. TLS always uses rustls with the webpki root
//! store because managed Postgres endpoints require `sslmode=require`.
//! Encodings deliberately mirror `sqlite.rs`: TEXT ids, BIGINT epoch-millis
//! timestamps, JSON-encoded skill lists and match reports, and little-endian
//! f32 embedding blobs. Every newly pooled connection re-checks the versioned
//! `schema_migrations` table before first use.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::{Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row, Transaction};
use uuid::Uuid;

use crate::domain::entities::{
    Application, ApplicationStatus, Job, MatchResult, RefreshToken, Resume, Role, Session, User,
};
use crate::domain::errors::DomainError;
use crate::domain::repositories::{
    ApplicationRepository, JobFilter, JobRepository, MatchResultRepository, ResumeRepository,
    UserRepository,
};

const SCHEMA_VERSION: i64 = 1;

const MIGRATION_TABLE_DDL: &str =
    "CREATE TABLE IF NOT EXISTS schema_migrations (version BIGINT PRIMARY KEY)";

const CURRENT_VERSION_SQL: &str =
    "SELECT COALESCE(MAX(version), 0)::BIGINT AS version FROM schema_migrations";

const VERSION_UPSERT_SQL: &str =
    "INSERT INTO schema_migrations (version) VALUES ($1) ON CONFLICT (version) DO NOTHING";

// Same logical schema as sqlite.rs SCHEMA_V1 translated to Postgres DDL:
// INTEGER epoch-millis becomes BIGINT, BLOB becomes BYTEA.
const SCHEMA_V1: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        email TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        role TEXT NOT NULL,
        created_at BIGINT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        current_refresh_token_id TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        last_rotated_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        revoked_at BIGINT
    )",
    "CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)",
    "CREATE TABLE IF NOT EXISTS refresh_tokens (
        verifier TEXT PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        session_id TEXT NOT NULL,
        issued_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        used_at BIGINT,
        revoked_at BIGINT,
        replaced_by TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_refresh_tokens_session ON refresh_tokens(session_id)",
    "CREATE TABLE IF NOT EXISTS resumes (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        title TEXT,
        raw_text TEXT NOT NULL,
        skills_json TEXT NOT NULL,
        embedding BYTEA NOT NULL,
        created_at BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_resumes_user ON resumes(user_id)",
    "CREATE TABLE IF NOT EXISTS jobs (
        id TEXT PRIMARY KEY,
        owner_id TEXT NOT NULL,
        title TEXT NOT NULL,
        description TEXT NOT NULL,
        skills_json TEXT NOT NULL,
        embedding BYTEA NOT NULL,
        created_at BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at, id)",
    "CREATE TABLE IF NOT EXISTS applications (
        id TEXT PRIMARY KEY,
        candidate_id TEXT NOT NULL,
        resume_id TEXT NOT NULL,
        job_id TEXT NOT NULL,
        status TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_applications_job ON applications(job_id, created_at, id)",
    "CREATE TABLE IF NOT EXISTS match_results (
        id TEXT PRIMARY KEY,
        resume_id TEXT NOT NULL,
        job_id TEXT NOT NULL,
        candidate_id TEXT NOT NULL,
        recruiter_id TEXT NOT NULL,
        requested_by TEXT NOT NULL,
        report_json TEXT NOT NULL,
        created_at BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_match_results_candidate ON match_results(candidate_id, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_match_results_recruiter ON match_results(recruiter_id, created_at)",
];

const RESUME_COLUMNS: &str = "id, user_id, title, raw_text, skills_json, embedding, created_at";
const JOB_COLUMNS: &str = "id, owner_id, title, description, skills_json, embedding, created_at";
const APPLICATION_COLUMNS: &str =
    "id, candidate_id, resume_id, job_id, status, created_at, updated_at";
const MATCH_COLUMNS: &str =
    "id, resume_id, job_id, candidate_id, recruiter_id, requested_by, report_json, created_at";

macro_rules! sql_params {
    ($($value:expr),* $(,)?) => {
        [$(&$value as &(dyn ToSql + Sync)),*]
    };
}

#[derive(Clone, Debug)]
pub struct PostgresRepositories {
    pool: Pool,
}

impl PostgresRepositories {
    /// Opens a pooled PostgreSQL backend, parses and validates the connection
    /// string eagerly, and migrates the schema before returning whenever the
    /// caller is not inside a current-thread runtime. The URL is never logged
    /// or embedded in any error message.
    pub fn open(url: &str, max_connections: u32) -> Result<Self, DomainError> {
        if max_connections == 0 {
            return Err(internal("database pool size must be positive"));
        }
        let pg_config: tokio_postgres::Config = url.parse().map_err(|error| {
            tracing::error!(%error, "postgres connection string is invalid");
            internal("database configuration is invalid")
        })?;
        let tls = make_tls_connector()?;
        let manager = Manager::from_config(
            pg_config,
            tls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(usize::try_from(max_connections).unwrap_or(usize::MAX))
            .post_create(migration_hook())
            .build()
            .map_err(|error| {
                tracing::error!(%error, "could not build postgres pool");
                internal("database is unavailable")
            })?;
        warm_up(&pool)?;
        Ok(Self { pool })
    }
}

/// Verifies connectivity and runs the initial migration when it is safe to
/// block. Inside a multi-thread runtime the caller hops into a blocking
/// region (`block_in_place`) so sibling workers keep driving spawned tasks;
/// with no ambient runtime a private one is used. Inside a current-thread
/// runtime blocking would stall the driver that the spawned connection task
/// depends on, so startup stays lazy there and the post-create hook
/// guarantees the schema before the first statement instead.
fn warm_up(pool: &Pool) -> Result<(), DomainError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            let owned = pool.clone();
            tokio::task::block_in_place(|| handle.block_on(probe(&owned)))
        }
        Ok(_) => Ok(()),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    tracing::error!(%error, "could not build postgres migration runtime");
                    internal("storage worker failed")
                })?;
            runtime.block_on(probe(pool))
        }
    }
}

async fn probe(pool: &Pool) -> Result<(), DomainError> {
    let mut client = pool.get().await.map_err(pool_unavailable)?;
    let tx = Client::transaction(&mut client)
        .await
        .map_err(storage_error)?;
    migrate_within_tx(&tx).await?;
    tx.commit().await.map_err(storage_error)
}

/// Runs on every freshly opened pooled connection so the logical schema is
/// always present before application statements reach the server.
fn migration_hook() -> Hook {
    Hook::async_fn(|client, _| {
        Box::pin(async move {
            let raw: &mut Client = client;
            let migration = async {
                let tx = raw.transaction().await.map_err(HookError::Backend)?;
                match migrate_within_tx(&tx).await {
                    Ok(()) => tx.commit().await.map_err(HookError::Backend)?,
                    Err(error) => {
                        tracing::error!(%error, "postgres schema check failed");
                        return Err(HookError::message("database schema check failed"));
                    }
                }
                Ok(())
            };
            migration.await
        })
    })
}

async fn migrate_within_tx(tx: &Transaction<'_>) -> Result<(), DomainError> {
    if let Err(error) = tx.batch_execute(MIGRATION_TABLE_DDL).await {
        tracing::error!(%error, "postgres migration failed");
        return Err(internal("database migration failed"));
    }
    let version = match tx.query_one(CURRENT_VERSION_SQL, &[]).await {
        Ok(row) => row
            .try_get::<_, i64>("version")
            .map_err(|_| corrupt_column("schema_migrations.version"))?,
        Err(error) => {
            tracing::error!(%error, "postgres migration failed");
            return Err(internal("database migration failed"));
        }
    };
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    if !(0..=SCHEMA_VERSION).contains(&version) {
        tracing::error!(
            version,
            expected = SCHEMA_VERSION,
            "unknown database schema"
        );
        return Err(internal("database schema is from a newer release"));
    }
    for statement in SCHEMA_V1 {
        if let Err(error) = tx.batch_execute(statement).await {
            tracing::error!(%error, "postgres migration failed");
            return Err(internal("database migration failed"));
        }
    }
    if let Err(error) = tx
        .execute(VERSION_UPSERT_SQL, &sql_params![SCHEMA_VERSION])
        .await
    {
        tracing::error!(%error, "postgres migration failed");
        return Err(internal("database migration failed"));
    }
    Ok(())
}

fn make_tls_connector() -> Result<tokio_postgres_rustls::MakeRustlsConnect, DomainError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| {
            tracing::error!(%error, "could not configure postgres TLS versions");
            internal("database TLS is unavailable")
        })?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

fn storage_error(error: tokio_postgres::Error) -> DomainError {
    if let Some(db_error) = error.as_db_error() {
        // SqlState is a struct of associated constants, not an enum.
        let code = db_error.code();
        if *code == SqlState::UNIQUE_VIOLATION || *code == SqlState::FOREIGN_KEY_VIOLATION {
            return DomainError::Conflict;
        }
    }
    tracing::error!(%error, "postgres storage failure");
    DomainError::Internal("storage operation failed".to_owned())
}

fn pool_unavailable(error: deadpool_postgres::PoolError) -> DomainError {
    match error {
        deadpool_postgres::PoolError::Timeout(timeout) => {
            tracing::error!(?timeout, "postgres pool timed out");
            DomainError::DependencyUnavailable("database is busy".to_owned())
        }
        other => {
            tracing::error!(%other, "postgres pool failure");
            DomainError::DependencyUnavailable("database is unavailable".to_owned())
        }
    }
}

fn internal(message: &'static str) -> DomainError {
    DomainError::Internal(message.to_owned())
}

fn corrupt_column(column: &str) -> DomainError {
    tracing::error!(column, "stored column could not be decoded");
    internal("stored record is corrupt")
}

fn millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis()
}

fn from_millis(value: i64) -> Result<DateTime<Utc>, DomainError> {
    DateTime::from_timestamp_millis(value).ok_or_else(|| {
        tracing::error!(value, "stored timestamp is out of range");
        internal("stored timestamp is corrupt")
    })
}

fn role_to_text(role: Role) -> &'static str {
    match role {
        Role::Candidate => "candidate",
        Role::Recruiter => "recruiter",
        Role::Admin => "admin",
    }
}

fn role_from_text(text: &str) -> Result<Role, DomainError> {
    match text {
        "candidate" => Ok(Role::Candidate),
        "recruiter" => Ok(Role::Recruiter),
        "admin" => Ok(Role::Admin),
        other => {
            tracing::error!(role = other, "unknown stored role");
            Err(internal("stored role is corrupt"))
        }
    }
}

fn status_to_text(status: ApplicationStatus) -> &'static str {
    match status {
        ApplicationStatus::Submitted => "submitted",
        ApplicationStatus::Withdrawn => "withdrawn",
    }
}

fn status_from_text(text: &str) -> Result<ApplicationStatus, DomainError> {
    match text {
        "submitted" => Ok(ApplicationStatus::Submitted),
        "withdrawn" => Ok(ApplicationStatus::Withdrawn),
        other => {
            tracing::error!(status = other, "unknown stored application status");
            Err(internal("stored application status is corrupt"))
        }
    }
}

fn skills_to_text(skills: &[String]) -> Result<String, DomainError> {
    serde_json::to_string(skills).map_err(|_| internal("could not serialize skill list"))
}

fn skills_from_text(text: &str) -> Result<Vec<String>, DomainError> {
    serde_json::from_str(text).map_err(|_| internal("stored skill list is corrupt"))
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn embedding_from_blob(blob: &[u8]) -> Result<Vec<f32>, DomainError> {
    if !blob.len().is_multiple_of(4) {
        tracing::error!(bytes = blob.len(), "stored embedding length is invalid");
        return Err(internal("stored embedding is corrupt"));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn text_column(row: &Row, name: &str) -> Result<String, DomainError> {
    row.try_get(name).map_err(|_| corrupt_column(name))
}

fn optional_text_column(row: &Row, name: &str) -> Result<Option<String>, DomainError> {
    row.try_get(name).map_err(|_| corrupt_column(name))
}

fn uuid_column(row: &Row, name: &str) -> Result<Uuid, DomainError> {
    let raw = text_column(row, name)?;
    Uuid::parse_str(&raw).map_err(|_| corrupt_column(name))
}

fn optional_uuid_column(row: &Row, name: &str) -> Result<Option<Uuid>, DomainError> {
    let raw = optional_text_column(row, name)?;
    raw.map(|value| Uuid::parse_str(&value).map_err(|_| corrupt_column(name)))
        .transpose()
}

fn time_column(row: &Row, name: &str) -> Result<DateTime<Utc>, DomainError> {
    let raw: i64 = row.try_get(name).map_err(|_| corrupt_column(name))?;
    from_millis(raw)
}

fn optional_time_column(row: &Row, name: &str) -> Result<Option<DateTime<Utc>>, DomainError> {
    let raw: Option<i64> = row.try_get(name).map_err(|_| corrupt_column(name))?;
    raw.map(from_millis).transpose()
}

fn bytes_column(row: &Row, name: &str) -> Result<Vec<u8>, DomainError> {
    row.try_get(name).map_err(|_| corrupt_column(name))
}

fn user_from_row(row: &Row) -> Result<User, DomainError> {
    let role_text = text_column(row, "role")?;
    Ok(User {
        id: uuid_column(row, "id")?,
        email: text_column(row, "email")?,
        password_hash: text_column(row, "password_hash")?,
        role: role_from_text(&role_text)?,
        created_at: time_column(row, "created_at")?,
    })
}

fn session_from_row(row: &Row) -> Result<Session, DomainError> {
    Ok(Session {
        id: uuid_column(row, "id")?,
        user_id: uuid_column(row, "user_id")?,
        current_refresh_token_id: uuid_column(row, "current_refresh_token_id")?,
        created_at: time_column(row, "created_at")?,
        last_rotated_at: time_column(row, "last_rotated_at")?,
        expires_at: time_column(row, "expires_at")?,
        revoked_at: optional_time_column(row, "revoked_at")?,
    })
}

fn refresh_token_from_row(row: &Row) -> Result<RefreshToken, DomainError> {
    Ok(RefreshToken {
        verifier: text_column(row, "verifier")?,
        id: uuid_column(row, "id")?,
        session_id: uuid_column(row, "session_id")?,
        issued_at: time_column(row, "issued_at")?,
        expires_at: time_column(row, "expires_at")?,
        used_at: optional_time_column(row, "used_at")?,
        revoked_at: optional_time_column(row, "revoked_at")?,
        replaced_by: optional_uuid_column(row, "replaced_by")?,
    })
}

fn resume_from_row(row: &Row) -> Result<Resume, DomainError> {
    let skills_json = text_column(row, "skills_json")?;
    let embedding = embedding_from_blob(&bytes_column(row, "embedding")?)?;
    Ok(Resume {
        id: uuid_column(row, "id")?,
        user_id: uuid_column(row, "user_id")?,
        title: optional_text_column(row, "title")?,
        raw_text: text_column(row, "raw_text")?,
        skills: skills_from_text(&skills_json)?,
        embedding,
        created_at: time_column(row, "created_at")?,
    })
}

fn job_from_row(row: &Row) -> Result<Job, DomainError> {
    let skills_json = text_column(row, "skills_json")?;
    let embedding = embedding_from_blob(&bytes_column(row, "embedding")?)?;
    Ok(Job {
        id: uuid_column(row, "id")?,
        owner_id: uuid_column(row, "owner_id")?,
        title: text_column(row, "title")?,
        description: text_column(row, "description")?,
        skills: skills_from_text(&skills_json)?,
        embedding,
        created_at: time_column(row, "created_at")?,
    })
}

fn application_from_row(row: &Row) -> Result<Application, DomainError> {
    let status_text = text_column(row, "status")?;
    Ok(Application {
        id: uuid_column(row, "id")?,
        candidate_id: uuid_column(row, "candidate_id")?,
        resume_id: uuid_column(row, "resume_id")?,
        job_id: uuid_column(row, "job_id")?,
        status: status_from_text(&status_text)?,
        created_at: time_column(row, "created_at")?,
        updated_at: time_column(row, "updated_at")?,
    })
}

fn match_result_from_row(row: &Row) -> Result<MatchResult, DomainError> {
    let report_json = text_column(row, "report_json")?;
    let report = serde_json::from_str(&report_json).map_err(|error| {
        tracing::error!(%error, "stored match report is corrupt");
        internal("stored match report is corrupt")
    })?;
    Ok(MatchResult {
        id: uuid_column(row, "id")?,
        resume_id: uuid_column(row, "resume_id")?,
        job_id: uuid_column(row, "job_id")?,
        candidate_id: uuid_column(row, "candidate_id")?,
        recruiter_id: uuid_column(row, "recruiter_id")?,
        requested_by: uuid_column(row, "requested_by")?,
        report,
        created_at: time_column(row, "created_at")?,
    })
}

async fn fetch_optional<T>(
    pool: &Pool,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    decode: fn(&Row) -> Result<T, DomainError>,
) -> Result<Option<T>, DomainError> {
    let client = pool.get().await.map_err(pool_unavailable)?;
    let row = client.query_opt(sql, params).await.map_err(storage_error)?;
    match row {
        Some(row) => Ok(Some(decode(&row)?)),
        None => Ok(None),
    }
}

async fn fetch_all<T>(
    pool: &Pool,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    decode: fn(&Row) -> Result<T, DomainError>,
) -> Result<Vec<T>, DomainError> {
    let client = pool.get().await.map_err(pool_unavailable)?;
    let rows = client.query(sql, params).await.map_err(storage_error)?;
    rows.iter().map(decode).collect()
}

async fn execute(
    pool: &Pool,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<u64, DomainError> {
    let client = pool.get().await.map_err(pool_unavailable)?;
    client.execute(sql, params).await.map_err(storage_error)
}

async fn revoke_family_tx(
    tx: &Transaction<'_>,
    session_id: Uuid,
    revoked_at: DateTime<Utc>,
) -> Result<(), DomainError> {
    let when = millis(revoked_at);
    let id_text = session_id.to_string();
    tx.execute(
        "UPDATE sessions SET revoked_at = $1 WHERE id = $2 AND revoked_at IS NULL",
        &sql_params![when, id_text],
    )
    .await
    .map_err(storage_error)?;
    tx.execute(
        "UPDATE refresh_tokens SET revoked_at = $1 WHERE session_id = $2 AND revoked_at IS NULL",
        &sql_params![when, id_text],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

const INSERT_USER_SQL: &str = "INSERT INTO users (id, email, password_hash, role, created_at)
     VALUES ($1, $2, $3, $4, $5)";
const INSERT_SESSION_SQL: &str =
    "INSERT INTO sessions (id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at)
     VALUES ($1, $2, $3, $4, $5, $6, NULL)";
const INSERT_REFRESH_TOKEN_SQL: &str =
    "INSERT INTO refresh_tokens (verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by)
     VALUES ($1, $2, $3, $4, $5, NULL, NULL, NULL)";
const SELECT_SESSION_COLUMNS: &str =
    "id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at";
const SELECT_REFRESH_TOKEN_COLUMNS: &str =
    "verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by";

#[async_trait]
impl UserRepository for PostgresRepositories {
    async fn create(&self, user: User) -> Result<User, DomainError> {
        execute(
            &self.pool,
            INSERT_USER_SQL,
            &sql_params![
                user.id.to_string(),
                user.email,
                user.password_hash,
                role_to_text(user.role),
                millis(user.created_at)
            ],
        )
        .await?;
        Ok(user)
    }

    async fn create_with_session(
        &self,
        user: User,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<User, DomainError> {
        if session.user_id != user.id
            || session.current_refresh_token_id != refresh_token.id
            || session.id != refresh_token.session_id
        {
            return Err(DomainError::InvalidInput(
                "session does not match the new user or token".to_owned(),
            ));
        }
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        tx.execute(
            INSERT_USER_SQL,
            &sql_params![
                user.id.to_string(),
                user.email,
                user.password_hash,
                role_to_text(user.role),
                millis(user.created_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            INSERT_SESSION_SQL,
            &sql_params![
                session.id.to_string(),
                session.user_id.to_string(),
                session.current_refresh_token_id.to_string(),
                millis(session.created_at),
                millis(session.last_rotated_at),
                millis(session.expires_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            INSERT_REFRESH_TOKEN_SQL,
            &sql_params![
                refresh_token.verifier,
                refresh_token.id.to_string(),
                refresh_token.session_id.to_string(),
                millis(refresh_token.issued_at),
                millis(refresh_token.expires_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(user)
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, DomainError> {
        fetch_optional(
            &self.pool,
            "SELECT id, email, password_hash, role, created_at FROM users WHERE email = $1",
            &sql_params![email],
            user_from_row,
        )
        .await
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, DomainError> {
        let id = id.to_string();
        fetch_optional(
            &self.pool,
            "SELECT id, email, password_hash, role, created_at FROM users WHERE id = $1",
            &sql_params![id],
            user_from_row,
        )
        .await
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<User>, DomainError> {
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        fetch_all(
            &self.pool,
            "SELECT id, email, password_hash, role, created_at FROM users
             ORDER BY created_at ASC, id ASC LIMIT $1 OFFSET $2",
            &sql_params![limit_i64, offset_i64],
            user_from_row,
        )
        .await
    }

    async fn create_session(
        &self,
        session: Session,
        refresh_token: RefreshToken,
    ) -> Result<Session, DomainError> {
        if session.current_refresh_token_id != refresh_token.id
            || session.id != refresh_token.session_id
        {
            return Err(DomainError::InvalidInput(
                "session does not match the refresh token".to_owned(),
            ));
        }
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let known: bool = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1) AS known",
                &sql_params![session.user_id.to_string()],
            )
            .await
            .map_err(storage_error)?
            .get("known");
        if !known {
            return Err(DomainError::NotFound);
        }
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        tx.execute(
            INSERT_SESSION_SQL,
            &sql_params![
                session.id.to_string(),
                session.user_id.to_string(),
                session.current_refresh_token_id.to_string(),
                millis(session.created_at),
                millis(session.last_rotated_at),
                millis(session.expires_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            INSERT_REFRESH_TOKEN_SQL,
            &sql_params![
                refresh_token.verifier,
                refresh_token.id.to_string(),
                refresh_token.session_id.to_string(),
                millis(refresh_token.issued_at),
                millis(refresh_token.expires_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(session)
    }

    async fn find_session(&self, id: Uuid) -> Result<Option<Session>, DomainError> {
        let id = id.to_string();
        fetch_optional(
            &self.pool,
            &format!("SELECT {SELECT_SESSION_COLUMNS} FROM sessions WHERE id = $1"),
            &sql_params![id],
            session_from_row,
        )
        .await
    }

    async fn find_refresh_token_by_verifier(
        &self,
        verifier: &str,
    ) -> Result<Option<RefreshToken>, DomainError> {
        fetch_optional(
            &self.pool,
            &format!(
                "SELECT {SELECT_REFRESH_TOKEN_COLUMNS} FROM refresh_tokens WHERE verifier = $1"
            ),
            &sql_params![verifier],
            refresh_token_from_row,
        )
        .await
    }

    async fn rotate_refresh_token(
        &self,
        current_verifier: &str,
        replacement: RefreshToken,
        rotated_at: DateTime<Utc>,
    ) -> Result<Session, DomainError> {
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        let current = tx
            .query_opt(
                &format!(
                    "SELECT {SELECT_REFRESH_TOKEN_COLUMNS} FROM refresh_tokens WHERE verifier = $1"
                ),
                &sql_params![current_verifier],
            )
            .await
            .map_err(storage_error)?
            .map(|row| refresh_token_from_row(&row))
            .transpose()?
            .ok_or(DomainError::Unauthorized)?;
        let mut session = tx
            .query_opt(
                &format!("SELECT {SELECT_SESSION_COLUMNS} FROM sessions WHERE id = $1"),
                &sql_params![current.session_id.to_string()],
            )
            .await
            .map_err(storage_error)?
            .map(|row| session_from_row(&row))
            .transpose()?
            .ok_or(DomainError::Unauthorized)?;

        let reused = current.used_at.is_some()
            || current.revoked_at.is_some()
            || current.replaced_by.is_some()
            || session.current_refresh_token_id != current.id;
        if reused {
            revoke_family_tx(&tx, current.session_id, rotated_at).await?;
            tx.commit().await.map_err(storage_error)?;
            return Err(DomainError::Unauthorized);
        }
        if !current.is_active_at(rotated_at) || !session.is_active_at(rotated_at) {
            return Err(DomainError::Unauthorized);
        }
        // Compare in the stored millisecond domain: freshly built datetimes
        // carry sub-millisecond precision that stored rows do not, so raw
        // DateTime comparisons would reject valid replacements.
        let rotated_ms = millis(rotated_at);
        let rep_issued_ms = millis(replacement.issued_at);
        let rep_expires_ms = millis(replacement.expires_at);
        let session_expires_ms = millis(session.expires_at);
        if replacement.session_id != session.id
            || rep_issued_ms < millis(current.issued_at)
            || rep_issued_ms > rotated_ms
            || rep_expires_ms <= rotated_ms
            || rep_expires_ms > session_expires_ms
        {
            return Err(DomainError::InvalidInput(
                "invalid refresh token replacement".to_owned(),
            ));
        }
        tx.execute(
            "UPDATE refresh_tokens SET used_at = $1, replaced_by = $2 WHERE verifier = $3",
            &sql_params![
                millis(rotated_at),
                replacement.id.to_string(),
                current.verifier
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "UPDATE sessions SET current_refresh_token_id = $1, last_rotated_at = $2 WHERE id = $3",
            &sql_params![
                replacement.id.to_string(),
                millis(rotated_at),
                session.id.to_string()
            ],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            INSERT_REFRESH_TOKEN_SQL,
            &sql_params![
                replacement.verifier,
                replacement.id.to_string(),
                replacement.session_id.to_string(),
                millis(replacement.issued_at),
                millis(replacement.expires_at)
            ],
        )
        .await
        .map_err(storage_error)?;
        session.current_refresh_token_id = replacement.id;
        session.last_rotated_at = rotated_at;
        tx.commit().await.map_err(storage_error)?;
        Ok(session)
    }

    async fn revoke_session(
        &self,
        session_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        let exists: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = $1) AS known",
                &sql_params![session_id.to_string()],
            )
            .await
            .map_err(storage_error)?
            .get("known");
        if !exists {
            return Err(DomainError::Unauthorized);
        }
        revoke_family_tx(&tx, session_id, revoked_at).await?;
        tx.commit().await.map_err(storage_error)
    }

    async fn revoke_session_by_refresh_token(
        &self,
        verifier: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        let session_id: Option<String> = tx
            .query_opt(
                "SELECT session_id FROM refresh_tokens WHERE verifier = $1",
                &sql_params![verifier],
            )
            .await
            .map_err(storage_error)?
            .map(|row| row.get("session_id"));
        let Some(session_id) = session_id.and_then(|text| Uuid::parse_str(&text).ok()) else {
            return Err(DomainError::Unauthorized);
        };
        revoke_family_tx(&tx, session_id, revoked_at).await?;
        tx.commit().await.map_err(storage_error)
    }

    async fn revoke_all_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<usize, DomainError> {
        let mut client = self.pool.get().await.map_err(pool_unavailable)?;
        let tx = Client::transaction(&mut client)
            .await
            .map_err(storage_error)?;
        let known: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1) AS known",
                &sql_params![user_id.to_string()],
            )
            .await
            .map_err(storage_error)?
            .get("known");
        if !known {
            return Err(DomainError::Unauthorized);
        }
        let ids: Vec<String> = tx
            .query(
                "SELECT id FROM sessions WHERE user_id = $1 AND revoked_at IS NULL",
                &sql_params![user_id.to_string()],
            )
            .await
            .map_err(storage_error)?
            .iter()
            .filter_map(|row| row.try_get::<_, String>(0).ok())
            .collect();
        for id_text in &ids {
            let session_id =
                Uuid::parse_str(id_text).map_err(|_| internal("stored session id is corrupt"))?;
            revoke_family_tx(&tx, session_id, revoked_at).await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(ids.len())
    }
}

impl PostgresRepositories {
    async fn resume_create_or_update(
        &self,
        resume: Resume,
        sql: &'static str,
    ) -> Result<Resume, DomainError> {
        let skills_json = skills_to_text(&resume.skills)?;
        let embedding = embedding_to_blob(&resume.embedding);
        let changed = execute(
            &self.pool,
            sql,
            &sql_params![
                resume.user_id.to_string(),
                resume.title,
                resume.raw_text,
                skills_json,
                embedding,
                millis(resume.created_at),
                resume.id.to_string()
            ],
        )
        .await?;
        if changed == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(resume)
    }
}

const UPSERT_RESUME_UPDATE_SQL: &str = "UPDATE resumes SET user_id = $1, title = $2, raw_text = $3,
     skills_json = $4, embedding = $5, created_at = $6 WHERE id = $7";
const INSERT_RESUME_SQL: &str =
    "INSERT INTO resumes (id, user_id, title, raw_text, skills_json, embedding, created_at)
     VALUES ($7, $1, $2, $3, $4, $5, $6)";

#[async_trait]
impl ResumeRepository for PostgresRepositories {
    async fn create(&self, resume: Resume) -> Result<Resume, DomainError> {
        self.resume_create_or_update(resume, INSERT_RESUME_SQL)
            .await
    }

    async fn update(&self, resume: Resume) -> Result<Resume, DomainError> {
        self.resume_create_or_update(resume, UPSERT_RESUME_UPDATE_SQL)
            .await
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Resume>, DomainError> {
        let id = id.to_string();
        fetch_optional(
            &self.pool,
            &format!("SELECT {RESUME_COLUMNS} FROM resumes WHERE id = $1"),
            &sql_params![id],
            resume_from_row,
        )
        .await
    }

    async fn find_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Resume>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let chunks: Vec<Vec<String>> = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .chunks(500)
            .map(<[String]>::to_vec)
            .collect();
        let client = self.pool.get().await.map_err(pool_unavailable)?;
        let mut found = Vec::new();
        for chunk in chunks {
            let placeholders = (1..=chunk.len())
                .map(|index| format!("${index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT {RESUME_COLUMNS} FROM resumes WHERE id IN ({placeholders})");
            let params: Vec<&(dyn ToSql + Sync)> = chunk
                .iter()
                .map(|value| value as &(dyn ToSql + Sync))
                .collect();
            let rows = client.query(&sql, &params).await.map_err(storage_error)?;
            found.extend(
                rows.iter()
                    .map(resume_from_row)
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(found)
    }

    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Resume>, DomainError> {
        let user_id = user_id.to_string();
        fetch_all(
            &self.pool,
            &format!("SELECT {RESUME_COLUMNS} FROM resumes WHERE user_id = $1"),
            &sql_params![user_id],
            resume_from_row,
        )
        .await
    }

    async fn list_by_user_paginated(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Resume>, DomainError> {
        let user_id = user_id.to_string();
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        fetch_all(
            &self.pool,
            &format!(
                "SELECT {RESUME_COLUMNS} FROM resumes WHERE user_id = $1
                 ORDER BY created_at DESC, id ASC LIMIT $2 OFFSET $3"
            ),
            &sql_params![user_id, limit_i64, offset_i64],
            resume_from_row,
        )
        .await
    }

    async fn delete(&self, id: Uuid) -> Result<(), DomainError> {
        let id = id.to_string();
        let changed = execute(
            &self.pool,
            "DELETE FROM resumes WHERE id = $1",
            &sql_params![id],
        )
        .await?;
        if changed == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}

#[async_trait]
impl JobRepository for PostgresRepositories {
    async fn create(&self, job: Job) -> Result<Job, DomainError> {
        execute(
            &self.pool,
            "INSERT INTO jobs (id, owner_id, title, description, skills_json, embedding, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &sql_params![
                job.id.to_string(),
                job.owner_id.to_string(),
                job.title,
                job.description,
                skills_to_text(&job.skills)?,
                embedding_to_blob(&job.embedding),
                millis(job.created_at)
            ],
        )
        .await?;
        Ok(job)
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Job>, DomainError> {
        let id = id.to_string();
        fetch_optional(
            &self.pool,
            &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = $1"),
            &sql_params![id],
            job_from_row,
        )
        .await
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<Job>, DomainError> {
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        fetch_all(
            &self.pool,
            &format!(
                "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at ASC, id ASC LIMIT $1 OFFSET $2"
            ),
            &sql_params![limit_i64, offset_i64],
            job_from_row,
        )
        .await
    }

    async fn list_filtered(
        &self,
        offset: usize,
        limit: usize,
        filter: JobFilter,
    ) -> Result<Vec<Job>, DomainError> {
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        // Dynamic WHERE clauses with lower()/LIKE mirror the SQLite adapter's
        // case-insensitive substring filtering.
        let mut conditions: Vec<String> = Vec::new();
        let mut values: Vec<String> = Vec::new();
        if let Some(query) = filter.query.as_deref().filter(|s| !s.trim().is_empty()) {
            let pattern = format!("%{}%", query.to_ascii_lowercase());
            let first = values.len() + 1;
            values.push(pattern.clone());
            conditions.push(format!(
                "(lower(title) LIKE ${first} OR lower(description) LIKE ${first})"
            ));
        }
        if let Some(skills) = filter.skills.as_deref().filter(|v| !v.is_empty()) {
            for skill in skills {
                let index = values.len() + 1;
                values.push(format!("%{}%", skill.to_ascii_lowercase()));
                conditions.push(format!("lower(skills_json) LIKE ${index}"));
            }
        }
        if let Some(location) = filter.location.as_deref().filter(|s| !s.trim().is_empty()) {
            let pattern = format!("%{}%", location.to_ascii_lowercase());
            let first = values.len() + 1;
            values.push(pattern);
            conditions.push(format!(
                "(lower(title) LIKE ${first} OR lower(description) LIKE ${first})"
            ));
        }
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let limit_index = values.len() + 1;
        let offset_index = values.len() + 2;
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM jobs {where_clause}
             ORDER BY created_at ASC, id ASC LIMIT ${limit_index} OFFSET ${offset_index}"
        );
        let mut params: Vec<&(dyn ToSql + Sync)> = values
            .iter()
            .map(|value| value as &(dyn ToSql + Sync))
            .collect();
        params.push(&limit_i64);
        params.push(&offset_i64);
        fetch_all(&self.pool, &sql, &params, job_from_row).await
    }
}

const INSERT_APPLICATION_SQL: &str =
    "INSERT INTO applications (id, candidate_id, resume_id, job_id, status, created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7)";

#[async_trait]
impl ApplicationRepository for PostgresRepositories {
    async fn create(&self, application: Application) -> Result<Application, DomainError> {
        let client = self.pool.get().await.map_err(pool_unavailable)?;
        let duplicate: bool = client
            .query_one(
                "SELECT EXISTS(
                     SELECT 1 FROM applications
                     WHERE job_id = $1 AND resume_id = $2 AND status = $3
                 ) AS duplicate",
                &sql_params![
                    application.job_id.to_string(),
                    application.resume_id.to_string(),
                    status_to_text(application.status)
                ],
            )
            .await
            .map_err(storage_error)?
            .get("duplicate");
        if duplicate {
            return Err(DomainError::Conflict);
        }
        client
            .execute(
                INSERT_APPLICATION_SQL,
                &sql_params![
                    application.id.to_string(),
                    application.candidate_id.to_string(),
                    application.resume_id.to_string(),
                    application.job_id.to_string(),
                    status_to_text(application.status),
                    millis(application.created_at),
                    millis(application.updated_at)
                ],
            )
            .await
            .map_err(storage_error)?;
        Ok(application)
    }

    async fn find_by_job_and_resume(
        &self,
        job_id: Uuid,
        resume_id: Uuid,
    ) -> Result<Option<Application>, DomainError> {
        let job_id = job_id.to_string();
        let resume_id = resume_id.to_string();
        fetch_optional(
            &self.pool,
            &format!(
                "SELECT {APPLICATION_COLUMNS} FROM applications
                 WHERE job_id = $1 AND resume_id = $2
                 ORDER BY created_at ASC, id ASC LIMIT 1"
            ),
            &sql_params![job_id, resume_id],
            application_from_row,
        )
        .await
    }

    async fn list_by_job(
        &self,
        job_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Application>, DomainError> {
        let job_id = job_id.to_string();
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        fetch_all(
            &self.pool,
            &format!(
                "SELECT {APPLICATION_COLUMNS} FROM applications WHERE job_id = $1
                 ORDER BY created_at ASC, id ASC LIMIT $2 OFFSET $3"
            ),
            &sql_params![job_id, limit_i64, offset_i64],
            application_from_row,
        )
        .await
    }
}

#[async_trait]
impl MatchResultRepository for PostgresRepositories {
    async fn create(&self, result: MatchResult) -> Result<MatchResult, DomainError> {
        let report_json = serde_json::to_string(&result.report)
            .map_err(|_| internal("could not serialize match report"))?;
        execute(
            &self.pool,
            "INSERT INTO match_results (id, resume_id, job_id, candidate_id, recruiter_id, requested_by, report_json, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &sql_params![
                result.id.to_string(),
                result.resume_id.to_string(),
                result.job_id.to_string(),
                result.candidate_id.to_string(),
                result.recruiter_id.to_string(),
                result.requested_by.to_string(),
                report_json,
                millis(result.created_at)
            ],
        )
        .await?;
        Ok(result)
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<MatchResult>, DomainError> {
        let id = id.to_string();
        fetch_optional(
            &self.pool,
            &format!("SELECT {MATCH_COLUMNS} FROM match_results WHERE id = $1"),
            &sql_params![id],
            match_result_from_row,
        )
        .await
    }

    async fn list_for_principal(
        &self,
        principal_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchResult>, DomainError> {
        let principal_id = principal_id.to_string();
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        fetch_all(
            &self.pool,
            &format!(
                "SELECT {MATCH_COLUMNS} FROM match_results
                 WHERE candidate_id = $1 OR recruiter_id = $1
                 ORDER BY created_at DESC, id ASC LIMIT $2 OFFSET $3"
            ),
            &sql_params![principal_id, limit_i64, offset_i64],
            match_result_from_row,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn schema_v1_translates_the_sqlite_logical_model() {
        let ddl = SCHEMA_V1.join("\n");
        for fragment in [
            "CREATE TABLE IF NOT EXISTS users",
            "email TEXT NOT NULL UNIQUE",
            "role TEXT NOT NULL",
            "created_at BIGINT NOT NULL",
            "revoked_at BIGINT",
            "replaced_by TEXT",
            "embedding BYTEA NOT NULL",
            "skills_json TEXT NOT NULL",
            "report_json TEXT NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_refresh_tokens_session ON refresh_tokens(session_id)",
            "CREATE INDEX IF NOT EXISTS idx_resumes_user ON resumes(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at, id)",
            "CREATE INDEX IF NOT EXISTS idx_applications_job ON applications(job_id, created_at, id)",
            "CREATE INDEX IF NOT EXISTS idx_match_results_candidate ON match_results(candidate_id, created_at)",
            "CREATE INDEX IF NOT EXISTS idx_match_results_recruiter ON match_results(recruiter_id, created_at)",
        ] {
            assert!(ddl.contains(fragment), "missing DDL fragment: {fragment}");
        }
        assert!(!ddl.contains("INTEGER"), "timestamps must use BIGINT");
        assert_eq!(
            SCHEMA_V1
                .iter()
                .filter(|statement| statement.contains("CREATE TABLE"))
                .count(),
            7,
            "seven tables expected: users, sessions, refresh_tokens, resumes, jobs, applications, match_results"
        );
    }

    #[test]
    fn migration_metadata_keeps_version_one_semantics() {
        assert_eq!(SCHEMA_VERSION, 1);
        assert!(MIGRATION_TABLE_DDL.contains("schema_migrations"));
        assert!(CURRENT_VERSION_SQL.contains("COALESCE(MAX(version), 0)"));
        assert!(VERSION_UPSERT_SQL.contains("VALUES ($1)"));
        assert!(VERSION_UPSERT_SQL.contains("ON CONFLICT (version) DO NOTHING"));
    }

    #[test]
    fn open_rejects_an_invalid_connection_string_without_echoing_it() {
        let secret = "supersecret-password";
        let malformed = format!("postgres://user:{secret}@host:99999/db?sslmode=require");
        let error =
            PostgresRepositories::open(&malformed, 5).expect_err("malformed URL must fail parsing");
        match &error {
            DomainError::Internal(message) => {
                assert_eq!(message, "database configuration is invalid");
            }
            other => panic!("expected generic Internal error, got {other:?}"),
        }
        let rendered = error.to_string();
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("host"));
        assert!(!rendered.contains("postgres://"));
    }

    #[test]
    fn open_rejects_a_non_positive_pool_size() {
        let error = PostgresRepositories::open(
            "postgresql://user:password@localhost/db?sslmode=require",
            0,
        )
        .expect_err("zero connections must fail validation");
        assert!(matches!(error, DomainError::Internal(_)));
    }

    #[test]
    fn encodings_match_the_sqlite_adapter() {
        assert_eq!(role_to_text(Role::Candidate), "candidate");
        assert_eq!(role_to_text(Role::Recruiter), "recruiter");
        assert_eq!(role_to_text(Role::Admin), "admin");
        for (text, role) in [
            ("candidate", Role::Candidate),
            ("recruiter", Role::Recruiter),
            ("admin", Role::Admin),
        ] {
            assert_eq!(role_from_text(text).expect("known role"), role);
        }
        for (text, status) in [
            ("submitted", ApplicationStatus::Submitted),
            ("withdrawn", ApplicationStatus::Withdrawn),
        ] {
            assert_eq!(status_from_text(text).expect("known status"), status);
            assert_eq!(status_to_text(status), text);
        }

        let skills = vec!["rust".to_owned(), "sql".to_owned()];
        let encoded = skills_to_text(&skills).expect("skills encode");
        assert_eq!(skills_from_text(&encoded).expect("skills decode"), skills);

        let embedding = vec![0.5_f32, -1.25, 0.0, f32::MAX];
        let blob = embedding_to_blob(&embedding);
        assert_eq!(blob.len(), embedding.len() * 4);
        let decoded = embedding_from_blob(&blob).expect("embedding decode");
        assert_eq!(decoded, embedding);

        let base = DateTime::from_timestamp_millis(1_234_567_890_123).expect("valid instant");
        assert_eq!(millis(base), 1_234_567_890_123);
        let with_submillis = base + Duration::nanoseconds(456_789);
        assert_eq!(millis(with_submillis), 1_234_567_890_123);
        assert_eq!(
            from_millis(millis(with_submillis)).expect("in range"),
            base,
            "sub-millisecond precision is intentionally truncated"
        );
        assert!(from_millis(i64::MIN).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a live PostgreSQL database; set APP_DATABASE_URL (e.g. Neon) to run"]
    async fn postgres_roundtrip_with_neon() {
        let _ = dotenvy::dotenv();
        let Some(url) = std::env::var("APP_DATABASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            println!("skipping postgres_roundtrip_with_neon: APP_DATABASE_URL is not set");
            return;
        };

        let repositories =
            PostgresRepositories::open(&url, 2).expect("postgres open and migrate should succeed");
        let marker = Uuid::now_v7().simple().to_string();
        let email = format!("pg-it-{marker}@example.com");

        let now = Utc::now();
        let user = User {
            id: Uuid::now_v7(),
            email,
            password_hash: "argon2id$live-test-hash".to_owned(),
            role: Role::Candidate,
            created_at: now,
        };
        UserRepository::create(&repositories, user.clone())
            .await
            .expect("user insert should succeed");
        let duplicate_rejected = matches!(
            UserRepository::create(&repositories, user.clone()).await,
            Err(DomainError::Conflict)
        );

        // A second, distinct user exercises the atomic register path.
        let registrant = User {
            id: Uuid::now_v7(),
            email: format!("pg-it-session-{marker}@example.com"),
            password_hash: "argon2id$live-test-hash".to_owned(),
            role: Role::Candidate,
            created_at: now,
        };

        let session = Session {
            id: Uuid::now_v7(),
            user_id: registrant.id,
            current_refresh_token_id: Uuid::now_v7(),
            created_at: now,
            last_rotated_at: now,
            expires_at: now + Duration::hours(1),
            revoked_at: None,
        };
        let first = RefreshToken {
            id: session.current_refresh_token_id,
            session_id: session.id,
            verifier: "live-digest-1".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        UserRepository::create_with_session(
            &repositories,
            registrant.clone(),
            session.clone(),
            first,
        )
        .await
        .expect("initial auth state should persist atomically");

        let replacement = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "live-digest-2".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        repositories
            .rotate_refresh_token("live-digest-1", replacement, now)
            .await
            .expect("rotation should succeed");

        let reuse = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "live-digest-3".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        let replay_rejected = matches!(
            repositories
                .rotate_refresh_token("live-digest-1", reuse, now)
                .await,
            Err(DomainError::Unauthorized)
        );

        let resumed =
            UserRepository::find_refresh_token_by_verifier(&repositories, "live-digest-2")
                .await
                .expect("replacement lookup")
                .expect("replacement token exists");
        let family_revoked = resumed.revoked_at.is_some();

        let resume = Resume {
            id: Uuid::now_v7(),
            user_id: user.id,
            title: Some("Live Test Engineer".to_owned()),
            raw_text: "Rust, SQL, and Postgres integration fixture.".to_owned(),
            skills: vec!["rust".to_owned(), "sql".to_owned()],
            embedding: vec![0.5_f32, -1.25, 0.0, f32::MAX],
            created_at: now,
        };
        let resume_ok = ResumeRepository::create(&repositories, resume.clone())
            .await
            .is_ok();

        let loaded = ResumeRepository::find_by_id(&repositories, resume.id)
            .await
            .expect("resume lookup")
            .expect("resume exists after roundtrip");
        let resume_roundtrip_ok = loaded.skills == resume.skills
            && loaded.raw_text == resume.raw_text
            && loaded.title == resume.title
            && loaded.embedding == resume.embedding;

        // Cleanup regardless of assertion outcomes so repeated runs stay clean.
        let client = repositories.pool.get().await.expect("cleanup connection");
        client
            .execute(
                "DELETE FROM resumes WHERE user_id = $1",
                &[&user.id.to_string()],
            )
            .await
            .expect("cleanup resumes");
        client
            .execute(
                "DELETE FROM refresh_tokens WHERE session_id = $1",
                &[&session.id.to_string()],
            )
            .await
            .expect("cleanup refresh tokens");
        client
            .execute(
                "DELETE FROM sessions WHERE user_id = $1",
                &[&registrant.id.to_string()],
            )
            .await
            .expect("cleanup sessions");
        for id in [&user.id, &registrant.id] {
            client
                .execute("DELETE FROM users WHERE id = $1", &[&id.to_string()])
                .await
                .expect("cleanup users");
        }

        assert!(duplicate_rejected, "duplicate email must be Conflict");
        assert!(replay_rejected, "refresh token reuse must be rejected");
        assert!(family_revoked, "reuse must revoke the whole token family");
        assert!(resume_ok, "resume insert should succeed");
        assert!(
            resume_roundtrip_ok,
            "skills, text, title, and LE f32 embedding must survive the roundtrip"
        );
    }
}
