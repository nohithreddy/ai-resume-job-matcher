use std::env;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceBackend {
    Memory,
    Sqlite,
    Postgres,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub bind_address: String,
    pub log_filter: String,
    pub jwt_secret: String,
    pub jwt_ttl_seconds: i64,
    pub argon2_memory_cost: u32,
    pub embedding_endpoint: Option<String>,
    pub embedding_api_key: Option<String>,
    pub embedding_model: String,
    pub persistence: PersistenceBackend,
    pub database_path: PathBuf,
    /// PostgreSQL connection string; required only for `PersistenceBackend::Postgres`.
    /// Never log or embed this value in error output.
    pub database_url: Option<String>,
    /// Maximum pooled connections when `APP_PERSISTENCE=postgres`.
    pub database_max_connections: u32,
    pub auth_rate_limit_window_seconds: u64,
    pub auth_rate_limit_max_requests: usize,
    pub admin_email: Option<String>,
    pub admin_password: Option<String>,
    pub upload_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0} must be set")]
    Missing(String),
    #[error("{name} is invalid: {source_message}")]
    Invalid {
        name: String,
        source_message: String,
    },
    #[error("APP_JWT_SECRET must be at least 32 bytes")]
    WeakJwtSecret,
}

impl AppConfig {
    /// Expected embedding dimensions for well-known models, if enforced.
    /// Unknown models accept any non-empty finite vector.
    pub fn embedding_expected_dimensions(&self) -> Option<usize> {
        match self.embedding_model.as_str() {
            "all-MiniLM-L6-v2" => Some(384),
            "text-embedding-3-small" => Some(1536),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            bind_address: "127.0.0.1:0".to_owned(),
            log_filter: "resume_job_matcher=info".to_owned(),
            jwt_secret: "a-test-secret-that-is-at-least-32-bytes-long".to_owned(),
            jwt_ttl_seconds: 3600,
            argon2_memory_cost: 8192,
            embedding_endpoint: None,
            embedding_api_key: None,
            embedding_model: "test-embedding-model".to_owned(),
            persistence: PersistenceBackend::Memory,
            database_path: PathBuf::new(),
            database_url: None,
            database_max_connections: 5,
            auth_rate_limit_window_seconds: 60,
            auth_rate_limit_max_requests: 10_000,
            admin_email: None,
            admin_password: None,
            upload_dir: PathBuf::from("./data/uploads"),
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        // Load .env if present (no-op in production where vars are injected).
        let _ = dotenvy::dotenv();
        let jwt_secret = env::var("APP_JWT_SECRET")
            .map_err(|_| ConfigError::Missing("APP_JWT_SECRET".to_owned()))?;
        if jwt_secret.len() < 32 {
            return Err(ConfigError::WeakJwtSecret);
        }

        let jwt_ttl_seconds = parse_optional("APP_JWT_TTL_SECONDS", 3600)?;
        if jwt_ttl_seconds <= 0 {
            return Err(ConfigError::Invalid {
                name: "APP_JWT_TTL_SECONDS".to_owned(),
                source_message: "must be greater than zero".to_owned(),
            });
        }
        let argon2_memory_cost = parse_optional("APP_ARGON2_MEMORY_COST", 19_456)?;
        if argon2_memory_cost < 8_192 {
            return Err(ConfigError::Invalid {
                name: "APP_ARGON2_MEMORY_COST".to_owned(),
                source_message: "must be at least 8192 KiB".to_owned(),
            });
        }
        let auth_rate_limit_window_seconds =
            parse_optional("APP_AUTH_RATE_LIMIT_WINDOW_SECONDS", 60)?;
        if auth_rate_limit_window_seconds == 0 {
            return Err(ConfigError::Invalid {
                name: "APP_AUTH_RATE_LIMIT_WINDOW_SECONDS".to_owned(),
                source_message: "must be greater than zero".to_owned(),
            });
        }
        let auth_rate_limit_max_requests = parse_optional("APP_AUTH_RATE_LIMIT_MAX_REQUESTS", 10)?;
        if auth_rate_limit_max_requests == 0 {
            return Err(ConfigError::Invalid {
                name: "APP_AUTH_RATE_LIMIT_MAX_REQUESTS".to_owned(),
                source_message: "must be greater than zero".to_owned(),
            });
        }

        let persistence = parse_backend(env::var("APP_PERSISTENCE").ok().as_deref())?;
        let database_url = env::var("APP_DATABASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        validate_postgres_url(if persistence == PersistenceBackend::Postgres {
            database_url.as_ref()
        } else {
            None
        })?;
        let database_max_connections = parse_optional("APP_DATABASE_MAX_CONNECTIONS", 5_u32)?;
        if valid_pool_size(database_max_connections).is_none() {
            return Err(ConfigError::Invalid {
                name: "APP_DATABASE_MAX_CONNECTIONS".to_owned(),
                source_message: "must be greater than zero".to_owned(),
            });
        }
        let database_path = PathBuf::from(optional("APP_DATABASE_PATH", "./data/matcher.db"));
        let admin_email = env::var("APP_ADMIN_EMAIL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_ascii_lowercase());
        let admin_password = env::var("APP_ADMIN_PASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        if admin_email.is_some() ^ admin_password.is_some() {
            return Err(ConfigError::Invalid {
                name: "APP_ADMIN_EMAIL / APP_ADMIN_PASSWORD".to_owned(),
                source_message: "both must be set together or neither".to_owned(),
            });
        }
        if let Some(password) = &admin_password
            && password.len() < 12
        {
            return Err(ConfigError::Invalid {
                name: "APP_ADMIN_PASSWORD".to_owned(),
                source_message: "must be at least 12 characters".to_owned(),
            });
        }
        let upload_dir = PathBuf::from(optional("APP_UPLOAD_DIR", "./data/uploads"));
        let embedding_model = optional("APP_EMBEDDING_MODEL", "text-embedding-3-small");
        match embedding_model.as_str() {
            "all-MiniLM-L6-v2" | "text-embedding-3-small" => {}
            _ => {
                tracing::warn!(
                    embedding_model = %embedding_model,
                    "unknown embedding model; any non-empty finite vector will be accepted"
                );
            }
        }

        Ok(Self {
            bind_address: optional("APP_BIND_ADDRESS", "127.0.0.1:3000"),
            log_filter: optional("APP_LOG_FILTER", "resume_job_matcher=info,tower_http=info"),
            jwt_secret,
            jwt_ttl_seconds,
            argon2_memory_cost,
            embedding_endpoint: env::var("APP_EMBEDDING_ENDPOINT")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            embedding_api_key: env::var("APP_EMBEDDING_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            embedding_model,
            persistence,
            database_path,
            database_url,
            database_max_connections,
            auth_rate_limit_window_seconds,
            auth_rate_limit_max_requests,
            admin_email,
            admin_password,
            upload_dir,
        })
    }
}

fn optional(name: &str, default: &str) -> String {
    match env::var(name) {
        Ok(value) => value,
        Err(_) => default.to_owned(),
    }
}

fn parse_backend(raw: Option<&str>) -> Result<PersistenceBackend, ConfigError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(PersistenceBackend::Sqlite),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "sqlite" => Ok(PersistenceBackend::Sqlite),
            "memory" => Ok(PersistenceBackend::Memory),
            "postgres" | "postgresql" => Ok(PersistenceBackend::Postgres),
            other => Err(ConfigError::Invalid {
                name: "APP_PERSISTENCE".to_owned(),
                source_message: format!(
                    "'{other}' is not a supported backend; use sqlite, postgres, or memory"
                ),
            }),
        },
    }
}

/// Enforces that a Postgres deployment has a usable connection string. The
/// error is generic on purpose: the URL (and its embedded credentials) must
/// never be echoed back through `Display` or logs.
fn validate_postgres_url(database_url: Option<&String>) -> Result<(), ConfigError> {
    match database_url {
        Some(url) if !url.trim().is_empty() => Ok(()),
        _ => Err(ConfigError::Missing("APP_DATABASE_URL".to_owned())),
    }
}

fn valid_pool_size(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

fn parse_optional<T>(name: &str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value.parse::<T>().map_err(|source| ConfigError::Invalid {
            name: name.to_owned(),
            source_message: source.to_string(),
        }),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_sqlite_when_unset() {
        assert_eq!(
            parse_backend(None).expect("unset should parse"),
            PersistenceBackend::Sqlite
        );
        assert_eq!(
            parse_backend(Some("")).expect("empty should parse"),
            PersistenceBackend::Sqlite
        );
    }

    #[test]
    fn backend_accepts_all_three_backends_case_insensitively() {
        for (raw, expected) in [
            ("sqlite", PersistenceBackend::Sqlite),
            (" SQLite ", PersistenceBackend::Sqlite),
            ("memory", PersistenceBackend::Memory),
            ("MEMORY", PersistenceBackend::Memory),
            ("postgres", PersistenceBackend::Postgres),
            ("PostgreSQL", PersistenceBackend::Postgres),
            (" postgresql ", PersistenceBackend::Postgres),
        ] {
            assert_eq!(
                parse_backend(Some(raw)).expect("known backend should parse"),
                expected,
                "input {raw:?}"
            );
        }
    }

    #[test]
    fn backend_rejects_unknown_values_without_echoing_them_into_guidance() {
        let error = parse_backend(Some("oracle")).expect_err("unknown must fail");
        assert!(matches!(error, ConfigError::Invalid { .. }));
        assert!(error.to_string().contains("sqlite, postgres, or memory"));
    }

    #[test]
    fn postgres_requires_a_database_url() {
        // Mirrors the from_env rule: Postgres without APP_DATABASE_URL is a
        // configuration error reported generically, never with the URL echoed.
        let error = validate_postgres_url(None).expect_err("missing url must fail");
        match &error {
            ConfigError::Missing(name) => assert_eq!(name, "APP_DATABASE_URL"),
            other => panic!("expected Missing, got {other:?}"),
        }
        assert!(!error.to_string().contains("postgres"));
        assert!(validate_postgres_url(Some(&String::new())).is_err());
        let url = "postgresql://user:password@host/db?sslmode=require".to_owned();
        validate_postgres_url(Some(&url)).expect("present url should pass");
    }

    #[test]
    fn pool_size_must_be_positive() {
        assert!(valid_pool_size(0).is_none());
        assert_eq!(valid_pool_size(1), Some(1_u32));
        assert_eq!(valid_pool_size(u32::MAX), Some(u32::MAX));
    }
}
