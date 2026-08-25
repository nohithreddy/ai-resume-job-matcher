use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

use crate::domain::entities::{AccessTokenClaims, RefreshToken, Role, Session};
use crate::domain::{PasswordService, TokenService, User, UserRepository};

use super::super::domain::errors::DomainError;

#[derive(Clone)]
pub struct AuthService {
    users: Arc<dyn UserRepository>,
    password_hasher: Arc<dyn PasswordService>,
    security: Arc<dyn TokenService>,
    dummy_password_hash: Arc<str>,
}

#[derive(Clone, Deserialize, Validate)]
pub struct RegisterInput {
    #[validate(email)]
    pub email: String,
    #[validate(length(min = 12, max = 128))]
    pub password: String,
    pub role: Role,
}

#[derive(Clone, Deserialize, Validate)]
pub struct LoginInput {
    #[validate(email)]
    pub email: String,
    #[validate(length(min = 1, max = 128))]
    pub password: String,
}

#[derive(Clone, Serialize)]
pub struct AuthOutput {
    pub user_id: Uuid,
    pub role: Role,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_expires_in: i64,
    pub session_id: Uuid,
}

impl AuthService {
    pub fn new(
        users: Arc<dyn UserRepository>,
        password_hasher: Arc<dyn PasswordService>,
        security: Arc<dyn TokenService>,
        dummy_password_hash: String,
    ) -> Self {
        Self {
            users,
            password_hasher,
            security,
            dummy_password_hash: dummy_password_hash.into(),
        }
    }

    pub async fn register(&self, input: RegisterInput) -> Result<AuthOutput, DomainError> {
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        if !matches!(input.role, Role::Candidate | Role::Recruiter) {
            return Err(DomainError::InvalidInput(
                "public registration supports candidate or recruiter roles only".to_owned(),
            ));
        }
        let email = normalize_email(&input.email);
        if self.users.find_by_email(&email).await?.is_some() {
            return Err(DomainError::Conflict);
        }
        let user = User {
            id: Uuid::now_v7(),
            email,
            password_hash: self.password_hasher.hash_password(&input.password).await?,
            role: input.role,
            created_at: Utc::now(),
        };
        let (user, session, refresh_token) = self.prepare_session(user).await?;
        let user = self
            .users
            .create_with_session(user, session.clone(), refresh_token.0)
            .await?;
        let output = self.auth_output(user, session, refresh_token.1)?;
        tracing::info!(user_id=%output.user_id, action="auth.register", session_id=%output.session_id, "audit");
        Ok(output)
    }

    pub async fn login(&self, input: LoginInput) -> Result<AuthOutput, DomainError> {
        input
            .validate()
            .map_err(|error| DomainError::InvalidInput(error.to_string()))?;
        let user = self
            .users
            .find_by_email(&normalize_email(&input.email))
            .await?;
        let hash = user
            .as_ref()
            .map_or(self.dummy_password_hash.as_ref(), |user| {
                user.password_hash.as_str()
            });
        let valid = self
            .password_hasher
            .verify_password(&input.password, hash)
            .await?;
        let Some(user) = user.filter(|_| valid) else {
            return Err(DomainError::Unauthorized);
        };
        let (user, session, refresh_token) = self.prepare_session(user).await?;
        self.users
            .create_session(session.clone(), refresh_token.0)
            .await?;
        let output = self.auth_output(user, session, refresh_token.1)?;
        tracing::info!(user_id=%output.user_id, action="auth.login", session_id=%output.session_id, "audit");
        Ok(output)
    }

    pub async fn authenticate(&self, token: &str) -> Result<Uuid, DomainError> {
        Ok(self.authenticate_claims(token).await?.user_id)
    }

    pub async fn authenticate_claims(&self, token: &str) -> Result<AccessTokenClaims, DomainError> {
        let claims = self.security.decode_access_token(token)?;
        let user = self
            .users
            .find_by_id(claims.user_id)
            .await?
            .ok_or(DomainError::Unauthorized)?;
        if user.role != claims.role {
            return Err(DomainError::Unauthorized);
        }
        let session = self
            .users
            .find_session(claims.session_id)
            .await?
            .ok_or(DomainError::Unauthorized)?;
        if session.user_id != claims.user_id || !session.is_active_at(Utc::now()) {
            return Err(DomainError::Unauthorized);
        }
        Ok(claims)
    }

    pub async fn authorize(&self, token: &str, required_role: Role) -> Result<Uuid, DomainError> {
        let claims = self.authenticate_claims(token).await?;
        if !claims.role.permits(required_role) {
            return Err(DomainError::Forbidden);
        }
        Ok(claims.user_id)
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<AuthOutput, DomainError> {
        let current_verifier = self.security.refresh_token_verifier(refresh_token)?;
        let now = Utc::now();
        let current = self
            .users
            .find_refresh_token_by_verifier(&current_verifier)
            .await?
            .ok_or(DomainError::Unauthorized)?;
        let session = self
            .users
            .find_session(current.session_id)
            .await?
            .ok_or(DomainError::Unauthorized)?;
        if !current.is_active_at(now) || !session.is_active_at(now) {
            if current.used_at.is_some() || current.replaced_by.is_some() {
                self.users.revoke_session(session.id, now).await?;
            }
            return Err(DomainError::Unauthorized);
        }
        let user = self
            .users
            .find_by_id(session.user_id)
            .await?
            .ok_or(DomainError::Unauthorized)?;
        let (replacement_raw, replacement) =
            self.build_refresh_token(session.id, now, Some(session.expires_at))?;
        let session = self
            .users
            .rotate_refresh_token(&current_verifier, replacement, now)
            .await?;
        let output = self.auth_output(user, session, replacement_raw)?;
        tracing::info!(user_id=%output.user_id, action="auth.refresh", session_id=%output.session_id, "audit");
        Ok(output)
    }

    pub async fn logout(&self, refresh_token: &str) -> Result<(), DomainError> {
        let verifier = self.security.refresh_token_verifier(refresh_token)?;
        let result = self
            .users
            .revoke_session_by_refresh_token(&verifier, Utc::now())
            .await;
        if result.is_ok() {
            tracing::info!(action = "auth.logout", "audit");
        }
        result
    }

    pub async fn revoke_session(&self, session_id: Uuid) -> Result<(), DomainError> {
        self.users.revoke_session(session_id, Utc::now()).await
    }

    pub async fn revoke_all_sessions(&self, user_id: Uuid) -> Result<usize, DomainError> {
        self.users.revoke_all_sessions(user_id, Utc::now()).await
    }

    async fn prepare_session(
        &self,
        user: User,
    ) -> Result<(User, Session, (RefreshToken, String)), DomainError> {
        let now = Utc::now();
        let session_id = Uuid::now_v7();
        let (raw, token) = self.build_refresh_token(session_id, now, None)?;
        let session = Session {
            id: session_id,
            user_id: user.id,
            current_refresh_token_id: token.id,
            created_at: now,
            last_rotated_at: now,
            expires_at: token.expires_at,
            revoked_at: None,
        };
        Ok((user, session, (token, raw)))
    }

    fn build_refresh_token(
        &self,
        session_id: Uuid,
        now: DateTime<Utc>,
        session_expires_at: Option<DateTime<Utc>>,
    ) -> Result<(String, RefreshToken), DomainError> {
        let raw = self.security.generate_refresh_token()?;
        let verifier = self.security.refresh_token_verifier(&raw)?;
        let candidate_expires_at = now
            .checked_add_signed(Duration::seconds(self.security.refresh_token_ttl_seconds()))
            .ok_or_else(|| DomainError::Internal("refresh token expiry overflow".to_owned()))?;
        let expires_at = session_expires_at.map_or(candidate_expires_at, |limit| {
            candidate_expires_at.min(limit)
        });
        Ok((
            raw,
            RefreshToken {
                id: Uuid::now_v7(),
                session_id,
                verifier,
                issued_at: now,
                expires_at,
                used_at: None,
                revoked_at: None,
                replaced_by: None,
            },
        ))
    }

    fn auth_output(
        &self,
        user: User,
        session: Session,
        refresh_token: String,
    ) -> Result<AuthOutput, DomainError> {
        let (access_token, expires_in) = self
            .security
            .issue_access_token(user.id, user.role, session.id)?;
        let refresh_expires_in = session
            .expires_at
            .timestamp()
            .saturating_sub(Utc::now().timestamp())
            .max(0);
        Ok(AuthOutput {
            user_id: user.id,
            role: user.role,
            access_token,
            refresh_token,
            token_type: "Bearer",
            expires_in,
            refresh_expires_in,
            session_id: session.id,
        })
    }
}

fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AuthService, RegisterInput};
    use crate::domain::entities::Role;
    use crate::infrastructure::{InMemoryRepositories, PasswordHasher, SecurityService};

    #[tokio::test]
    async fn refresh_rotation_and_reuse_revoke_the_session_family() {
        let repositories = InMemoryRepositories::new();
        let auth = AuthService::new(
            repositories.users,
            Arc::new(PasswordHasher::new(8 * 1024)),
            Arc::new(SecurityService::new(
                "a-secure-test-secret-that-is-long-enough".to_owned(),
                3600,
            )),
            PasswordHasher::new(8 * 1024)
                .hash("timing-equalization-password")
                .expect("dummy hash should build"),
        );
        let first = auth
            .register(RegisterInput {
                email: "user@example.com".to_owned(),
                password: "correct horse battery staple".to_owned(),
                role: Role::Candidate,
            })
            .await
            .expect("registration should succeed");
        assert_eq!(first.role, Role::Candidate);
        assert_eq!(first.expires_in, 900);
        assert_eq!(first.refresh_token.len(), 64);
        assert!(auth.authenticate(&first.access_token).await.is_ok());

        let rotated = auth
            .refresh(&first.refresh_token)
            .await
            .expect("refresh should rotate the token");
        assert_ne!(rotated.refresh_token, first.refresh_token);
        assert!(auth.authenticate(&rotated.access_token).await.is_ok());

        assert!(auth.refresh(&first.refresh_token).await.is_err());
        assert!(auth.authenticate(&rotated.access_token).await.is_err());
        assert!(auth.refresh(&rotated.refresh_token).await.is_err());
    }
}
