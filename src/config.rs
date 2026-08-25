use std::env;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceBackend {
    Memory,
    Sqlite,
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
            auth_rate_limit_window_seconds: 60,
            auth_rate_limit_max_requests: 10_000,
            admin_email: None,
            admin_password: None,
            upload_dir: PathBuf::from("./data/uploads"),
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
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

        let persistence = match env::var("APP_PERSISTENCE") {
            Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
                "sqlite" => PersistenceBackend::Sqlite,
                "memory" => PersistenceBackend::Memory,
                other => {
                    return Err(ConfigError::Invalid {
                        name: "APP_PERSISTENCE".to_owned(),
                        source_message: format!(
                            "'{other}' is not a supported backend; use sqlite or memory"
                        ),
                    });
                }
            },
            Err(_) => PersistenceBackend::Sqlite,
        };
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
