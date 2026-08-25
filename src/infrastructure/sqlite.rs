//! Durable SQLite adapter implementing every repository port.
//!
//! One connection guarded by a std mutex; all calls run on the Tokio blocking
//! pool because rusqlite is synchronous. Writes that must be atomic (user +
//! session + refresh token, refresh rotation) run inside transactions.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, Transaction, params};
use tokio::task::spawn_blocking;
use uuid::Uuid;

use crate::domain::entities::{
    Application, ApplicationStatus, Job, MatchResult, RefreshToken, Resume, Role, Session, User,
};
use crate::domain::errors::DomainError;
use crate::domain::repositories::{
    ApplicationRepository, JobRepository, MatchResultRepository, ResumeRepository, UserRepository,
};

// Provenance note: resumes/jobs embeddings lack a stored `embedding_model` provenance
// column. A future v2 migration should add `embedding_model TEXT` to both tables,
// populated from `AppConfig::embedding_model` at write time, with a bounded
// re-embedding plan for existing rows. Deferred in this slice to avoid a risky
// migration (requires defaults/backfill and would touch restart durability).
const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        email TEXT NOT NULL UNIQUE,
        password_hash TEXT NOT NULL,
        role TEXT NOT NULL,
        created_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        current_refresh_token_id TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_rotated_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        revoked_at INTEGER
    )",
    "CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id)",
    "CREATE TABLE IF NOT EXISTS refresh_tokens (
        verifier TEXT PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        session_id TEXT NOT NULL,
        issued_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used_at INTEGER,
        revoked_at INTEGER,
        replaced_by TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_refresh_tokens_session ON refresh_tokens(session_id)",
    "CREATE TABLE IF NOT EXISTS resumes (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL,
        title TEXT,
        raw_text TEXT NOT NULL,
        skills_json TEXT NOT NULL,
        embedding BLOB NOT NULL,
        created_at INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_resumes_user ON resumes(user_id)",
    "CREATE TABLE IF NOT EXISTS jobs (
        id TEXT PRIMARY KEY,
        owner_id TEXT NOT NULL,
        title TEXT NOT NULL,
        description TEXT NOT NULL,
        skills_json TEXT NOT NULL,
        embedding BLOB NOT NULL,
        created_at INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at, id)",
    "CREATE TABLE IF NOT EXISTS applications (
        id TEXT PRIMARY KEY,
        candidate_id TEXT NOT NULL,
        resume_id TEXT NOT NULL,
        job_id TEXT NOT NULL,
        status TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
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
        created_at INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_match_results_candidate ON match_results(candidate_id, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_match_results_recruiter ON match_results(recruiter_id, created_at)",
];

#[derive(Clone)]
pub struct SqliteRepositories {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRepositories {
    pub fn open(path: &Path) -> Result<Self, DomainError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                tracing::error!(%error, ?parent, "could not create database directory");
                DomainError::Internal("database directory is not usable".to_owned())
            })?;
        }
        let conn = Connection::open(path).map_err(|error| fatal_open(error, path))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| fatal_open(error, path))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| fatal_open(error, path))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|error| fatal_open(error, path))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

fn fatal_open(error: rusqlite::Error, path: &Path) -> DomainError {
    tracing::error!(%error, ?path, "could not open sqlite database");
    DomainError::Internal("database is unavailable".to_owned())
}

fn storage_error(error: rusqlite::Error) -> DomainError {
    match error.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => DomainError::Conflict,
        Some(ErrorCode::DatabaseBusy | ErrorCode::OperationInterrupted | ErrorCode::DiskFull) => {
            tracing::error!(%error, "sqlite dependency unavailable");
            DomainError::DependencyUnavailable("database is busy".to_owned())
        }
        _ => {
            tracing::error!(%error, "sqlite storage failure");
            DomainError::Internal("storage operation failed".to_owned())
        }
    }
}

fn internal(message: &'static str) -> DomainError {
    DomainError::Internal(message.to_owned())
}

fn migrate(conn: &Connection) -> Result<(), DomainError> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(storage_error)?;
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
    let tx = conn.unchecked_transaction().map_err(storage_error)?;
    for statement in SCHEMA_V1 {
        if let Err(error) = tx.execute_batch(statement) {
            let _ = tx.rollback();
            tracing::error!(%error, "sqlite migration failed");
            return Err(internal("database migration failed"));
        }
    }
    if let Err(error) = tx.pragma_update(None, "user_version", SCHEMA_VERSION) {
        let _ = tx.rollback();
        tracing::error!(%error, "sqlite migration failed");
        return Err(internal("database migration failed"));
    }
    tx.commit().map_err(storage_error)
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

fn optional_from_millis(value: Option<i64>) -> Result<Option<DateTime<Utc>>, DomainError> {
    value.map(from_millis).transpose()
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

fn row_user(row: &Row<'_>) -> Result<User, rusqlite::Error> {
    let id: String = row.get("id")?;
    let role_text: String = row.get("role")?;
    let created_at: i64 = row.get("created_at")?;
    let role = role_from_text(&role_text)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let created_at = from_millis(created_at)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    Ok(User {
        id: Uuid::parse_str(&id).map_err(|_| rusqlite::Error::InvalidColumnName(id))?,
        email: row.get("email")?,
        password_hash: row.get("password_hash")?,
        role,
        created_at,
    })
}

fn row_session(row: &Row<'_>) -> Result<Session, rusqlite::Error> {
    fn uuid_field(raw: String) -> Result<Uuid, rusqlite::Error> {
        Uuid::parse_str(&raw).map_err(|_| rusqlite::Error::InvalidColumnName(raw))
    }
    fn time_field(raw: i64) -> Result<DateTime<Utc>, rusqlite::Error> {
        from_millis(raw).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    }
    Ok(Session {
        id: uuid_field(row.get::<_, String>("id")?)?,
        user_id: uuid_field(row.get::<_, String>("user_id")?)?,
        current_refresh_token_id: uuid_field(row.get::<_, String>("current_refresh_token_id")?)?,
        created_at: time_field(row.get("created_at")?)?,
        last_rotated_at: time_field(row.get("last_rotated_at")?)?,
        expires_at: time_field(row.get("expires_at")?)?,
        revoked_at: optional_from_millis(row.get("revoked_at")?)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
    })
}

fn row_refresh_token(row: &Row<'_>) -> Result<RefreshToken, rusqlite::Error> {
    fn uuid_field(raw: String) -> Result<Uuid, rusqlite::Error> {
        Uuid::parse_str(&raw).map_err(|_| rusqlite::Error::InvalidColumnName(raw))
    }
    fn time_field(raw: i64) -> Result<DateTime<Utc>, rusqlite::Error> {
        from_millis(raw).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    }
    Ok(RefreshToken {
        verifier: row.get("verifier")?,
        id: uuid_field(row.get::<_, String>("id")?)?,
        session_id: uuid_field(row.get::<_, String>("session_id")?)?,
        issued_at: time_field(row.get("issued_at")?)?,
        expires_at: time_field(row.get("expires_at")?)?,
        used_at: optional_from_millis(row.get("used_at")?)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
        revoked_at: optional_from_millis(row.get("revoked_at")?)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
        replaced_by: row
            .get::<_, Option<String>>("replaced_by")?
            .map(uuid_field)
            .transpose()?,
    })
}

fn row_resume(row: &Row<'_>) -> Result<Resume, rusqlite::Error> {
    let decode = |error: DomainError| rusqlite::Error::ToSqlConversionFailure(Box::new(error));
    let skills_json: String = row.get("skills_json")?;
    let embedding: Vec<u8> = row.get("embedding")?;
    Ok(Resume {
        id: Uuid::parse_str(&row.get::<_, String>("id")?)
            .map_err(|_| rusqlite::Error::InvalidColumnName("resume id".into()))?,
        user_id: Uuid::parse_str(&row.get::<_, String>("user_id")?)
            .map_err(|_| rusqlite::Error::InvalidColumnName("resume user".into()))?,
        title: row.get("title")?,
        raw_text: row.get("raw_text")?,
        skills: skills_from_text(&skills_json).map_err(decode)?,
        embedding: embedding_from_blob(&embedding).map_err(decode)?,
        created_at: from_millis(row.get("created_at")?).map_err(decode)?,
    })
}

fn row_job(row: &Row<'_>) -> Result<Job, rusqlite::Error> {
    let decode = |error: DomainError| rusqlite::Error::ToSqlConversionFailure(Box::new(error));
    let skills_json: String = row.get("skills_json")?;
    let embedding: Vec<u8> = row.get("embedding")?;
    Ok(Job {
        id: Uuid::parse_str(&row.get::<_, String>("id")?)
            .map_err(|_| rusqlite::Error::InvalidColumnName("job id".into()))?,
        owner_id: Uuid::parse_str(&row.get::<_, String>("owner_id")?)
            .map_err(|_| rusqlite::Error::InvalidColumnName("job owner".into()))?,
        title: row.get("title")?,
        description: row.get("description")?,
        skills: skills_from_text(&skills_json).map_err(decode)?,
        embedding: embedding_from_blob(&embedding).map_err(decode)?,
        created_at: from_millis(row.get("created_at")?).map_err(decode)?,
    })
}

fn row_application(row: &Row<'_>) -> Result<Application, rusqlite::Error> {
    let decode = |error: DomainError| rusqlite::Error::ToSqlConversionFailure(Box::new(error));
    fn uuid_column(
        value: Result<String, rusqlite::Error>,
        label: &'static str,
    ) -> Result<Uuid, rusqlite::Error> {
        Uuid::parse_str(&value?).map_err(|_| rusqlite::Error::InvalidColumnName(label.into()))
    }
    let status_text: String = row.get("status")?;
    Ok(Application {
        id: uuid_column(row.get("id"), "application id")?,
        candidate_id: uuid_column(row.get("candidate_id"), "candidate")?,
        resume_id: uuid_column(row.get("resume_id"), "resume")?,
        job_id: uuid_column(row.get("job_id"), "job")?,
        status: status_from_text(&status_text).map_err(decode)?,
        created_at: from_millis(row.get("created_at")?).map_err(decode)?,
        updated_at: from_millis(row.get("updated_at")?).map_err(decode)?,
    })
}

fn row_match_result(row: &Row<'_>) -> Result<MatchResult, rusqlite::Error> {
    let decode = |error: DomainError| rusqlite::Error::ToSqlConversionFailure(Box::new(error));
    fn uuid_column(
        value: Result<String, rusqlite::Error>,
        label: &'static str,
    ) -> Result<Uuid, rusqlite::Error> {
        Uuid::parse_str(&value?).map_err(|_| rusqlite::Error::InvalidColumnName(label.into()))
    }
    let report_json: String = row.get("report_json")?;
    let report = serde_json::from_str(&report_json).map_err(|error| {
        tracing::error!(%error, "stored match report is corrupt");
        rusqlite::Error::ToSqlConversionFailure(Box::new(internal(
            "stored match report is corrupt",
        )))
    })?;
    Ok(MatchResult {
        id: uuid_column(row.get("id"), "match id")?,
        resume_id: uuid_column(row.get("resume_id"), "resume")?,
        job_id: uuid_column(row.get("job_id"), "job")?,
        candidate_id: uuid_column(row.get("candidate_id"), "candidate")?,
        recruiter_id: uuid_column(row.get("recruiter_id"), "recruiter")?,
        requested_by: uuid_column(row.get("requested_by"), "requester")?,
        report,
        created_at: from_millis(row.get("created_at")?).map_err(decode)?,
    })
}

fn revoke_family_tx(
    tx: &Transaction<'_>,
    session_id: Uuid,
    revoked_at: DateTime<Utc>,
) -> Result<(), DomainError> {
    let when = millis(revoked_at);
    let id_text = session_id.to_string();
    tx.execute(
        "UPDATE sessions SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
        params![when, id_text],
    )
    .map_err(storage_error)?;
    tx.execute(
        "UPDATE refresh_tokens SET revoked_at = ?1 WHERE session_id = ?2 AND revoked_at IS NULL",
        params![when, id_text],
    )
    .map_err(storage_error)?;
    Ok(())
}

#[async_trait]
impl UserRepository for SqliteRepositories {
    async fn create(&self, user: User) -> Result<User, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let changed = conn
                .execute(
                    "INSERT INTO users (id, email, password_hash, role, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        user.id.to_string(),
                        user.email,
                        user.password_hash,
                        role_to_text(user.role),
                        millis(user.created_at)
                    ],
                )
                .map_err(storage_error)?;
            if changed == 0 {
                return Err(internal("user insert wrote no rows"));
            }
            Ok(user)
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "blocking task join failed");
            internal("storage worker failed")
        })?
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
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            let inserted = tx
                .execute(
                    "INSERT INTO users (id, email, password_hash, role, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        user.id.to_string(),
                        user.email,
                        user.password_hash,
                        role_to_text(user.role),
                        millis(user.created_at)
                    ],
                )
                .map_err(storage_error)?;
            if inserted == 0 {
                let _ = tx.rollback();
                return Err(internal("user insert wrote no rows"));
            }
            tx.execute(
                "INSERT INTO sessions (id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    session.id.to_string(),
                    session.user_id.to_string(),
                    session.current_refresh_token_id.to_string(),
                    millis(session.created_at),
                    millis(session.last_rotated_at),
                    millis(session.expires_at)
                ],
            )
            .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO refresh_tokens (verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
                params![
                    refresh_token.verifier,
                    refresh_token.id.to_string(),
                    refresh_token.session_id.to_string(),
                    millis(refresh_token.issued_at),
                    millis(refresh_token.expires_at)
                ],
            )
            .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
            Ok(user)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>, DomainError> {
        let conn = self.conn.clone();
        let email = email.to_owned();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                "SELECT id, email, password_hash, role, created_at FROM users WHERE email = ?1",
                params![email],
                row_user,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                "SELECT id, email, password_hash, role, created_at FROM users WHERE id = ?1",
                params![id],
                row_user,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<User>, DomainError> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = conn
                .prepare(
                    "SELECT id, email, password_hash, role, created_at FROM users
                     ORDER BY created_at ASC, id ASC LIMIT ?1 OFFSET ?2",
                )
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![limit_i64, offset_i64], row_user)
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
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
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let known: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
                    params![session.user_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if !known {
                return Err(DomainError::NotFound);
            }
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            tx.execute(
                "INSERT INTO sessions (id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    session.id.to_string(),
                    session.user_id.to_string(),
                    session.current_refresh_token_id.to_string(),
                    millis(session.created_at),
                    millis(session.last_rotated_at),
                    millis(session.expires_at)
                ],
            )
            .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO refresh_tokens (verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
                params![
                    refresh_token.verifier,
                    refresh_token.id.to_string(),
                    refresh_token.session_id.to_string(),
                    millis(refresh_token.issued_at),
                    millis(refresh_token.expires_at)
                ],
            )
            .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
            Ok(session)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_session(&self, id: Uuid) -> Result<Option<Session>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                "SELECT id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_session,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn find_refresh_token_by_verifier(
        &self,
        verifier: &str,
    ) -> Result<Option<RefreshToken>, DomainError> {
        let conn = self.conn.clone();
        let verifier = verifier.to_owned();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                "SELECT verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by
                 FROM refresh_tokens WHERE verifier = ?1",
                params![verifier],
                row_refresh_token,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn rotate_refresh_token(
        &self,
        current_verifier: &str,
        replacement: RefreshToken,
        rotated_at: DateTime<Utc>,
    ) -> Result<Session, DomainError> {
        let conn = self.conn.clone();
        let current_verifier = current_verifier.to_owned();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            let current = tx
                .query_row(
                    "SELECT verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by
                     FROM refresh_tokens WHERE verifier = ?1",
                    params![current_verifier],
                    row_refresh_token,
                )
                .optional()
                .map_err(storage_error)?
                .ok_or(DomainError::Unauthorized)?;
            let mut session = tx
                .query_row(
                    "SELECT id, user_id, current_refresh_token_id, created_at, last_rotated_at, expires_at, revoked_at
                     FROM sessions WHERE id = ?1",
                    params![current.session_id.to_string()],
                    row_session,
                )
                .optional()
                .map_err(storage_error)?
                .ok_or(DomainError::Unauthorized)?;

            let reused = current.used_at.is_some()
                || current.revoked_at.is_some()
                || current.replaced_by.is_some()
                || session.current_refresh_token_id != current.id;
            if reused {
                revoke_family_tx(&tx, current.session_id, rotated_at)?;
                tx.commit().map_err(storage_error)?;
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
                "UPDATE refresh_tokens SET used_at = ?1, replaced_by = ?2 WHERE verifier = ?3",
                params![
                    millis(rotated_at),
                    replacement.id.to_string(),
                    current.verifier
                ],
            )
            .map_err(storage_error)?;
            tx.execute(
                "UPDATE sessions SET current_refresh_token_id = ?1, last_rotated_at = ?2 WHERE id = ?3",
                params![
                    replacement.id.to_string(),
                    millis(rotated_at),
                    session.id.to_string()
                ],
            )
            .map_err(storage_error)?;
            tx.execute(
                "INSERT INTO refresh_tokens (verifier, id, session_id, issued_at, expires_at, used_at, revoked_at, replaced_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
                params![
                    replacement.verifier,
                    replacement.id.to_string(),
                    replacement.session_id.to_string(),
                    millis(replacement.issued_at),
                    millis(replacement.expires_at)
                ],
            )
            .map_err(storage_error)?;
            session.current_refresh_token_id = replacement.id;
            session.last_rotated_at = rotated_at;
            tx.commit().map_err(storage_error)?;
            Ok(session)
        })
        .await
        .map_err(join_failure)?
    }

    async fn revoke_session(
        &self,
        session_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                    params![session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if !exists {
                return Err(DomainError::Unauthorized);
            }
            revoke_family_tx(&tx, session_id, revoked_at)?;
            tx.commit().map_err(storage_error)
        })
        .await
        .map_err(join_failure)?
    }

    async fn revoke_session_by_refresh_token(
        &self,
        verifier: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let conn = self.conn.clone();
        let verifier = verifier.to_owned();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            let session_id: Option<String> = tx
                .query_row(
                    "SELECT session_id FROM refresh_tokens WHERE verifier = ?1",
                    params![verifier],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_error)?;
            let Some(session_id) = session_id.and_then(|text| Uuid::parse_str(&text).ok()) else {
                return Err(DomainError::Unauthorized);
            };
            revoke_family_tx(&tx, session_id, revoked_at)?;
            tx.commit().map_err(storage_error)
        })
        .await
        .map_err(join_failure)?
    }

    async fn revoke_all_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<usize, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let tx = conn.unchecked_transaction().map_err(storage_error)?;
            let known: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
                    params![user_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if !known {
                return Err(DomainError::Unauthorized);
            }
            let ids = {
                let mut statement = tx
                    .prepare("SELECT id FROM sessions WHERE user_id = ?1 AND revoked_at IS NULL")
                    .map_err(storage_error)?;
                let rows = statement
                    .query_map(params![user_id.to_string()], |row| row.get::<_, String>(0))
                    .map_err(storage_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)?
            };
            for id_text in &ids {
                let session_id = Uuid::parse_str(id_text)
                    .map_err(|_| internal("stored session id is corrupt"))?;
                revoke_family_tx(&tx, session_id, revoked_at)?;
            }
            tx.commit().map_err(storage_error)?;
            Ok(ids.len())
        })
        .await
        .map_err(join_failure)?
    }
}

async fn blocking<T, F>(closure: F) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DomainError> + Send + 'static,
{
    spawn_blocking(closure).await.map_err(join_failure)?
}

fn join_failure(error: tokio::task::JoinError) -> DomainError {
    tracing::error!(%error, "blocking storage task join failed");
    internal("storage worker failed")
}

const RESUME_COLUMNS: &str = "id, user_id, title, raw_text, skills_json, embedding, created_at";
const JOB_COLUMNS: &str = "id, owner_id, title, description, skills_json, embedding, created_at";
const APPLICATION_COLUMNS: &str =
    "id, candidate_id, resume_id, job_id, status, created_at, updated_at";
const MATCH_COLUMNS: &str =
    "id, resume_id, job_id, candidate_id, recruiter_id, requested_by, report_json, created_at";

#[async_trait]
impl ResumeRepository for SqliteRepositories {
    async fn create(&self, resume: Resume) -> Result<Resume, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.execute(
                "INSERT INTO resumes (id, user_id, title, raw_text, skills_json, embedding, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    resume.id.to_string(),
                    resume.user_id.to_string(),
                    resume.title,
                    resume.raw_text,
                    skills_to_text(&resume.skills)?,
                    embedding_to_blob(&resume.embedding),
                    millis(resume.created_at)
                ],
            )
            .map_err(storage_error)?;
            Ok(resume)
        })
        .await
        .map_err(join_failure)?
    }

    async fn update(&self, resume: Resume) -> Result<Resume, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let changed = conn
                .execute(
                    "UPDATE resumes SET user_id = ?2, title = ?3, raw_text = ?4, skills_json = ?5,
                     embedding = ?6, created_at = ?7 WHERE id = ?1",
                    params![
                        resume.id.to_string(),
                        resume.user_id.to_string(),
                        resume.title,
                        resume.raw_text,
                        skills_to_text(&resume.skills)?,
                        embedding_to_blob(&resume.embedding),
                        millis(resume.created_at)
                    ],
                )
                .map_err(storage_error)?;
            if changed == 0 {
                return Err(DomainError::NotFound);
            }
            Ok(resume)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Resume>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                &format!("SELECT {RESUME_COLUMNS} FROM resumes WHERE id = ?1"),
                params![id],
                row_resume,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn find_by_ids(&self, ids: &[Uuid]) -> Result<Vec<Resume>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let chunks: Vec<Vec<String>> = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .chunks(500)
            .map(<[String]>::to_vec)
            .collect();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let mut found = Vec::new();
            for chunk in &chunks {
                let placeholders = chunk
                    .iter()
                    .enumerate()
                    .map(|(index, _)| format!("?{}", index + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql =
                    format!("SELECT {RESUME_COLUMNS} FROM resumes WHERE id IN ({placeholders})");
                let mut statement = conn.prepare(&sql).map_err(storage_error)?;
                let rows = statement
                    .query_map(rusqlite::params_from_iter(chunk.iter()), row_resume)
                    .map_err(storage_error)?;
                for row in rows {
                    found.push(row.map_err(storage_error)?);
                }
            }
            Ok(found)
        })
        .await
    }

    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<Resume>, DomainError> {
        let conn = self.conn.clone();
        let user_id = user_id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {RESUME_COLUMNS} FROM resumes WHERE user_id = ?1"
                ))
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![user_id], row_resume)
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }

    async fn list_by_user_paginated(
        &self,
        user_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Resume>, DomainError> {
        let conn = self.conn.clone();
        let user_id = user_id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {RESUME_COLUMNS} FROM resumes WHERE user_id = ?1 ORDER BY created_at DESC, id ASC LIMIT ?2 OFFSET ?3"
                ))
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![user_id, limit_i64, offset_i64], row_resume)
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }

    async fn delete(&self, id: Uuid) -> Result<(), DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let changed = conn
                .execute("DELETE FROM resumes WHERE id = ?1", params![id])
                .map_err(storage_error)?;
            if changed == 0 {
                return Err(DomainError::NotFound);
            }
            Ok(())
        })
        .await
        .map_err(join_failure)?
    }
}

#[async_trait]
impl JobRepository for SqliteRepositories {
    async fn create(&self, job: Job) -> Result<Job, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.execute(
                "INSERT INTO jobs (id, owner_id, title, description, skills_json, embedding, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    job.id.to_string(),
                    job.owner_id.to_string(),
                    job.title,
                    job.description,
                    skills_to_text(&job.skills)?,
                    embedding_to_blob(&job.embedding),
                    millis(job.created_at)
                ],
            )
            .map_err(storage_error)?;
            Ok(job)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<Job>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                params![id],
                row_job,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn list(&self, offset: usize, limit: usize) -> Result<Vec<Job>, DomainError> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at ASC, id ASC LIMIT ?1 OFFSET ?2"
                ))
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![limit_i64, offset_i64], row_job)
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }

    async fn list_filtered(
        &self,
        offset: usize,
        limit: usize,
        filter: crate::domain::JobFilter,
    ) -> Result<Vec<Job>, DomainError> {
        let conn = self.conn.clone();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            // Build dynamic WHERE clauses with LIKE; use lower() for case-insensitive matching
            let mut conditions = Vec::new();
            let mut params_values: Vec<String> = Vec::new();
            if let Some(q) = filter.query.as_deref().filter(|s| !s.trim().is_empty()) {
                let pattern = format!("%{}%", q.to_ascii_lowercase());
                conditions.push("(lower(title) LIKE ? OR lower(description) LIKE ?)".to_owned());
                params_values.push(pattern.clone());
                params_values.push(pattern);
            }
            if let Some(skills) = filter.skills.as_deref().filter(|v| !v.is_empty()) {
                for skill in skills {
                    let pattern = format!("%{}%", skill.to_ascii_lowercase());
                    conditions.push("lower(skills_json) LIKE ?".to_owned());
                    params_values.push(pattern);
                }
            }
            if let Some(loc) = filter.location.as_deref().filter(|s| !s.trim().is_empty()) {
                let pattern = format!("%{}%", loc.to_ascii_lowercase());
                conditions.push("(lower(title) LIKE ? OR lower(description) LIKE ?)".to_owned());
                params_values.push(pattern.clone());
                params_values.push(pattern);
            }
            let where_clause = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };
            let sql = format!(
                "SELECT {JOB_COLUMNS} FROM jobs {where_clause} ORDER BY created_at ASC, id ASC LIMIT ? OFFSET ?"
            );
            let mut statement = conn.prepare(&sql).map_err(storage_error)?;
            let param_count = params_values.len();
            // Need to bind dynamically: create params list with limit and offset at end
            let all_params: Vec<&dyn rusqlite::ToSql> = params_values.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            drop(all_params);
            let mut bind_values: Vec<rusqlite::types::Value> = params_values
                .into_iter()
                .map(rusqlite::types::Value::Text)
                .collect();
            bind_values.push(rusqlite::types::Value::Integer(limit_i64));
            bind_values.push(rusqlite::types::Value::Integer(offset_i64));
            let rows = statement
                .query_map(rusqlite::params_from_iter(bind_values.iter()), row_job)
                .map_err(storage_error)?;
            let _ = param_count;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }
}

#[async_trait]
impl ApplicationRepository for SqliteRepositories {
    async fn create(&self, application: Application) -> Result<Application, DomainError> {
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let duplicate: bool = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM applications
                        WHERE job_id = ?1 AND resume_id = ?2 AND status = ?3
                     )",
                    params![
                        application.job_id.to_string(),
                        application.resume_id.to_string(),
                        status_to_text(application.status)
                    ],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if duplicate {
                return Err(DomainError::Conflict);
            }
            conn.execute(
                "INSERT INTO applications (id, candidate_id, resume_id, job_id, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    application.id.to_string(),
                    application.candidate_id.to_string(),
                    application.resume_id.to_string(),
                    application.job_id.to_string(),
                    status_to_text(application.status),
                    millis(application.created_at),
                    millis(application.updated_at)
                ],
            )
            .map_err(storage_error)?;
            Ok(application)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_by_job_and_resume(
        &self,
        job_id: Uuid,
        resume_id: Uuid,
    ) -> Result<Option<Application>, DomainError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        let resume_id = resume_id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                &format!(
                    "SELECT {APPLICATION_COLUMNS} FROM applications
                     WHERE job_id = ?1 AND resume_id = ?2
                     ORDER BY created_at ASC, id ASC LIMIT 1"
                ),
                params![job_id, resume_id],
                row_application,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn list_by_job(
        &self,
        job_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Application>, DomainError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {APPLICATION_COLUMNS} FROM applications WHERE job_id = ?1
                     ORDER BY created_at ASC, id ASC LIMIT ?2 OFFSET ?3"
                ))
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![job_id, limit_i64, offset_i64], row_application)
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }
}

#[async_trait]
impl MatchResultRepository for SqliteRepositories {
    async fn create(&self, result: MatchResult) -> Result<MatchResult, DomainError> {
        let report_json = serde_json::to_string(&result.report)
            .map_err(|_| internal("could not serialize match report"))?;
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.execute(
                "INSERT INTO match_results (id, resume_id, job_id, candidate_id, recruiter_id, requested_by, report_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
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
            .map_err(storage_error)?;
            Ok(result)
        })
        .await
        .map_err(join_failure)?
    }

    async fn find_by_id(&self, id: Uuid) -> Result<Option<MatchResult>, DomainError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            conn.query_row(
                &format!("SELECT {MATCH_COLUMNS} FROM match_results WHERE id = ?1"),
                params![id],
                row_match_result,
            )
            .optional()
            .map_err(storage_error)
        })
        .await
    }

    async fn list_for_principal(
        &self,
        principal_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MatchResult>, DomainError> {
        let conn = self.conn.clone();
        let principal_id = principal_id.to_string();
        blocking(move || {
            let conn = conn.lock().map_err(|_| internal("lock poisoned"))?;
            let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {MATCH_COLUMNS} FROM match_results
                     WHERE candidate_id = ?1 OR recruiter_id = ?1
                     ORDER BY created_at DESC, id ASC LIMIT ?2 OFFSET ?3"
                ))
                .map_err(storage_error)?;
            let rows = statement
                .query_map(
                    params![principal_id, limit_i64, offset_i64],
                    row_match_result,
                )
                .map_err(storage_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(storage_error)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn test_repository() -> (SqliteRepositories, PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = format!(
            "{}-{}",
            Uuid::now_v7().simple(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let path = std::env::temp_dir()
            .join("resume-job-matcher-tests")
            .join(format!("{unique}.db"));
        let repositories = SqliteRepositories::open(&path).expect("sqlite repository should open");
        (repositories, path)
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("db-wal"));
        let _ = fs::remove_file(path.with_extension("db-shm"));
    }

    fn sample_user(email: &str) -> User {
        User {
            id: Uuid::now_v7(),
            email: email.to_owned(),
            password_hash: "argon2id$test-hash".to_owned(),
            role: Role::Candidate,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn duplicate_email_is_rejected_as_conflict() {
        let (repository, path) = test_repository();
        let user = sample_user("dupe@example.com");
        UserRepository::create(&repository, user.clone())
            .await
            .expect("first insert should succeed");
        assert!(matches!(
            UserRepository::create(&repository, user).await,
            Err(DomainError::Conflict)
        ));
        cleanup(&path);
    }

    #[tokio::test]
    async fn users_and_sessions_survive_a_full_reopen() {
        let (repository, path) = test_repository();
        let user = sample_user("restart@example.com");
        UserRepository::create(&repository, user)
            .await
            .expect("user insert");

        // Reopen from disk as a fresh process would.
        let reopened = SqliteRepositories::open(&path).expect("reopen should succeed");
        let found = UserRepository::find_by_email(&reopened, "restart@example.com")
            .await
            .expect("lookup")
            .expect("user must survive reopen");
        assert_eq!(found.email, "restart@example.com");
        cleanup(&path);
    }

    #[tokio::test]
    async fn refresh_rotation_reuse_revokes_the_session_family() {
        let (repository, path) = test_repository();
        let now = Utc::now();
        let user = sample_user("rotation@example.com");
        let session = Session {
            id: Uuid::now_v7(),
            user_id: user.id,
            current_refresh_token_id: Uuid::now_v7(),
            created_at: now,
            last_rotated_at: now,
            expires_at: now + Duration::hours(1),
            revoked_at: None,
        };
        let first = RefreshToken {
            id: session.current_refresh_token_id,
            session_id: session.id,
            verifier: "digest-1".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        UserRepository::create_with_session(&repository, user, session.clone(), first)
            .await
            .expect("initial auth state");

        let replacement = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "digest-2".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        repository
            .rotate_refresh_token("digest-1", replacement, now)
            .await
            .expect("rotation should succeed");

        let reuse = RefreshToken {
            id: Uuid::now_v7(),
            session_id: session.id,
            verifier: "digest-3".to_owned(),
            issued_at: now,
            expires_at: session.expires_at,
            used_at: None,
            revoked_at: None,
            replaced_by: None,
        };
        assert!(matches!(
            repository
                .rotate_refresh_token("digest-1", reuse, now)
                .await,
            Err(DomainError::Unauthorized)
        ));
        let revoked_session = repository
            .find_session(session.id)
            .await
            .expect("session lookup")
            .expect("session exists");
        assert!(revoked_session.revoked_at.is_some());
        let revoked_replacement = repository
            .find_refresh_token_by_verifier("digest-2")
            .await
            .expect("token lookup")
            .expect("replacement exists");
        assert!(revoked_replacement.revoked_at.is_some());
        cleanup(&path);
    }

    #[tokio::test]
    async fn resumes_round_trip_skills_and_embeddings() {
        let (repository, path) = test_repository();
        let user = sample_user("resumes@example.com");
        UserRepository::create(&repository, user.clone())
            .await
            .expect("user insert");
        let resume = Resume {
            id: Uuid::now_v7(),
            user_id: user.id,
            title: Some("Engineer".to_owned()),
            raw_text: "Rust and SQL".to_owned(),
            skills: vec!["rust".to_owned(), "sql".to_owned()],
            embedding: vec![0.5_f32, -1.25, 0.0, f32::MAX],
            created_at: Utc::now(),
        };
        ResumeRepository::create(&repository, resume.clone())
            .await
            .expect("resume insert");
        let loaded = ResumeRepository::find_by_id(&repository, resume.id)
            .await
            .expect("lookup")
            .expect("resume exists");
        assert_eq!(loaded.skills, vec!["rust".to_owned(), "sql".to_owned()]);
        assert_eq!(loaded.embedding.len(), resume.embedding.len());
        assert_eq!(loaded.embedding[0], 0.5);
        assert_eq!(loaded.embedding[3], f32::MAX);
        assert!(loaded.raw_text == resume.raw_text);
        cleanup(&path);
    }
}
