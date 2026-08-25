use argon2::{
    Argon2, PasswordHash, PasswordVerifier,
    password_hash::{PasswordHasher as _, SaltString},
};
use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::entities::{AccessTokenClaims, Role};
use crate::domain::{DomainError, PasswordService, TokenService};

const MAX_ACCESS_TTL_SECONDS: i64 = 15 * 60;
const DEFAULT_REFRESH_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
const JWT_ISSUER: &str = "resume-job-matcher";
const JWT_AUDIENCE: &str = "resume-job-matcher-api";
const REFRESH_SALT: &[u8] = b"resume-job-matcher refresh verifier v1";
const REFRESH_DIGEST_BYTES: usize = 32;
const REFRESH_TOKEN_BYTES: usize = 32;

#[derive(Clone)]
pub struct PasswordHasher {
    memory_cost: u32,
}

impl PasswordHasher {
    pub fn new(memory_cost: u32) -> Self {
        Self { memory_cost }
    }

    pub fn hash(&self, password: &str) -> Result<String, argon2::password_hash::Error> {
        let salt = SaltString::generate(&mut OsRng);
        let params = argon2::Params::new(self.memory_cost, 2, 1, Some(32))?;
        let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        Ok(argon2
            .hash_password(password.as_bytes(), &salt)?
            .to_string())
    }

    pub fn verify(
        &self,
        password: &str,
        encoded_hash: &str,
    ) -> Result<bool, argon2::password_hash::Error> {
        let parsed = PasswordHash::new(encoded_hash)?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    }
}

#[async_trait::async_trait]
impl PasswordService for PasswordHasher {
    async fn hash_password(&self, password: &str) -> Result<String, DomainError> {
        let hasher = self.clone();
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || hasher.hash(&password))
            .await
            .map_err(|error| {
                DomainError::Internal(format!("password hashing task failed: {error}"))
            })?
            .map_err(|error| DomainError::Internal(format!("password hashing failed: {error}")))
    }

    async fn verify_password(
        &self,
        password: &str,
        encoded_hash: &str,
    ) -> Result<bool, DomainError> {
        let hasher = self.clone();
        let password = password.to_owned();
        let encoded_hash = encoded_hash.to_owned();
        tokio::task::spawn_blocking(move || hasher.verify(&password, &encoded_hash))
            .await
            .map_err(|error| {
                DomainError::Internal(format!("password verification task failed: {error}"))
            })?
            .map_err(|error| DomainError::Internal(format!("password hash is invalid: {error}")))
    }
}

#[derive(Clone)]
pub struct SecurityService {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    signing_secret: Vec<u8>,
    ttl_seconds: i64,
    issuer: String,
    audience: String,
    refresh_ttl_seconds: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    role: Role,
    sid: String,
    jti: String,
    iss: String,
    aud: String,
    iat: i64,
    nbf: i64,
    exp: i64,
}

impl SecurityService {
    pub fn new(secret: String, ttl_seconds: i64) -> Self {
        Self::with_policy(
            secret,
            ttl_seconds,
            JWT_ISSUER.to_owned(),
            JWT_AUDIENCE.to_owned(),
            DEFAULT_REFRESH_TTL_SECONDS,
        )
    }

    pub fn with_policy(
        secret: String,
        ttl_seconds: i64,
        issuer: String,
        audience: String,
        refresh_ttl_seconds: i64,
    ) -> Self {
        let ttl_seconds = ttl_seconds.clamp(1, MAX_ACCESS_TTL_SECONDS);
        let refresh_ttl_seconds = refresh_ttl_seconds.max(1);
        Self {
            encoding_key: EncodingKey::from_secret(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            signing_secret: secret.into_bytes(),
            ttl_seconds,
            issuer,
            audience,
            refresh_ttl_seconds,
        }
    }

    pub fn issue(&self, user_id: Uuid) -> Result<(String, i64), jsonwebtoken::errors::Error> {
        self.issue_access(user_id, Role::default(), Uuid::now_v7())
    }

    pub fn issue_access(
        &self,
        user_id: Uuid,
        role: Role,
        session_id: Uuid,
    ) -> Result<(String, i64), jsonwebtoken::errors::Error> {
        if user_id.is_nil() || session_id.is_nil() {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }
        let now = Utc::now();
        let expiration = now
            .checked_add_signed(Duration::seconds(self.ttl_seconds))
            .ok_or_else(|| {
                jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken)
            })?;
        let claims = Claims {
            sub: user_id.to_string(),
            role,
            sid: session_id.to_string(),
            jti: Uuid::now_v7().to_string(),
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            exp: expiration.timestamp(),
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding_key)
            .map(|token| (token, self.ttl_seconds))
    }

    pub fn issue_access_token(
        &self,
        user_id: Uuid,
        role: Role,
        session_id: Uuid,
    ) -> Result<(String, i64), DomainError> {
        self.issue_access(user_id, role, session_id)
            .map_err(|error| DomainError::Internal(format!("token issuance failed: {error}")))
    }

    pub fn decode_user_id(&self, token: &str) -> Result<Uuid, jsonwebtoken::errors::Error> {
        self.decode_claims(token).map(|claims| claims.user_id)
    }

    pub fn decode_claims(
        &self,
        token: &str,
    ) -> Result<AccessTokenClaims, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_issuer(std::slice::from_ref(&self.issuer));
        validation.set_audience(std::slice::from_ref(&self.audience));
        validation.set_required_spec_claims(&["exp", "iat", "nbf", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        let token_data = decode::<Claims>(token, &self.decoding_key, &validation)?;
        let claims = token_data.claims;
        let user_id = Uuid::parse_str(&claims.sub).map_err(|_| {
            jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidSubject)
        })?;
        if user_id.is_nil() {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidSubject,
            ));
        }

        let session_id = Uuid::parse_str(&claims.sid).map_err(|_| {
            jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken)
        })?;
        if session_id.is_nil() {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }
        let token_id = Uuid::parse_str(&claims.jti).map_err(|_| {
            jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken)
        })?;
        if token_id.is_nil() {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }
        let issued_at = timestamp(claims.iat)?;
        let not_before = timestamp(claims.nbf)?;
        let expires_at = timestamp(claims.exp)?;
        if claims.iat > claims.nbf || claims.nbf >= claims.exp {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }
        let Some(lifetime_seconds) = claims.exp.checked_sub(claims.iat) else {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        };
        if lifetime_seconds > MAX_ACCESS_TTL_SECONDS {
            return Err(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            ));
        }
        Ok(AccessTokenClaims {
            user_id,
            role: claims.role,
            session_id,
            token_id,
            issuer: claims.iss,
            audience: claims.aud,
            issued_at,
            not_before,
            expires_at,
        })
    }

    pub fn decode_access_token(&self, token: &str) -> Result<AccessTokenClaims, DomainError> {
        self.decode_claims(token)
            .map_err(|_| DomainError::Unauthorized)
    }

    pub fn generate_refresh_token(&self) -> Result<String, DomainError> {
        let mut bytes = [0_u8; REFRESH_TOKEN_BYTES];
        OsRng.try_fill_bytes(&mut bytes).map_err(|error| {
            DomainError::Internal(format!("refresh token generation failed: {error}"))
        })?;
        Ok(hex_encode(&bytes))
    }

    pub fn refresh_token_verifier(&self, token: &str) -> Result<String, DomainError> {
        if token.len() != REFRESH_TOKEN_BYTES * 2
            || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DomainError::Unauthorized);
        }
        let params =
            argon2::Params::new(1_024, 1, 1, Some(REFRESH_DIGEST_BYTES)).map_err(|error| {
                DomainError::Internal(format!("refresh verifier parameters failed: {error}"))
            })?;
        let argon2 = Argon2::new_with_secret(
            &self.signing_secret,
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            params,
        )
        .map_err(|error| {
            DomainError::Internal(format!("refresh verifier setup failed: {error}"))
        })?;
        let mut digest = [0_u8; REFRESH_DIGEST_BYTES];
        argon2
            .hash_password_into(token.as_bytes(), REFRESH_SALT, &mut digest)
            .map_err(|error| DomainError::Internal(format!("refresh verifier failed: {error}")))?;
        Ok(hex_encode(&digest))
    }

    pub fn refresh_token_digest(&self, token: &str) -> Result<String, DomainError> {
        self.refresh_token_verifier(token)
    }

    pub fn refresh_token_ttl_seconds(&self) -> i64 {
        self.refresh_ttl_seconds
    }
}

impl TokenService for SecurityService {
    fn issue(&self, user_id: Uuid) -> Result<(String, i64), DomainError> {
        SecurityService::issue(self, user_id)
            .map_err(|error| DomainError::Internal(format!("token issuance failed: {error}")))
    }

    fn decode_user_id(&self, token: &str) -> Result<Uuid, DomainError> {
        SecurityService::decode_user_id(self, token).map_err(|_| DomainError::Unauthorized)
    }

    fn issue_access_token(
        &self,
        user_id: Uuid,
        role: Role,
        session_id: Uuid,
    ) -> Result<(String, i64), DomainError> {
        self.issue_access(user_id, role, session_id)
            .map_err(|error| DomainError::Internal(format!("token issuance failed: {error}")))
    }

    fn decode_access_token(&self, token: &str) -> Result<AccessTokenClaims, DomainError> {
        self.decode_claims(token)
            .map_err(|_| DomainError::Unauthorized)
    }

    fn generate_refresh_token(&self) -> Result<String, DomainError> {
        SecurityService::generate_refresh_token(self)
    }

    fn refresh_token_verifier(&self, token: &str) -> Result<String, DomainError> {
        SecurityService::refresh_token_verifier(self, token)
    }

    fn refresh_token_ttl_seconds(&self) -> i64 {
        SecurityService::refresh_token_ttl_seconds(self)
    }
}

fn timestamp(value: i64) -> Result<DateTime<Utc>, jsonwebtoken::errors::Error> {
    DateTime::from_timestamp(value, 0).ok_or_else(|| {
        jsonwebtoken::errors::Error::from(jsonwebtoken::errors::ErrorKind::InvalidToken)
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{PasswordHasher, SecurityService};
    use crate::domain::TokenService;
    use crate::domain::entities::Role;
    use uuid::Uuid;

    #[test]
    fn argon2id_hashes_and_verifies_passwords() {
        let hasher = PasswordHasher::new(8 * 1024);
        let hash = hasher.hash("correct horse battery staple").expect("hash");
        assert!(
            hasher
                .verify("correct horse battery staple", &hash)
                .expect("verify")
        );
        assert!(!hasher.verify("wrong", &hash).expect("verify"));
        assert!(hash.contains("argon2id"));
    }

    #[test]
    fn jwt_round_trip_preserves_audit_claims() {
        let service =
            SecurityService::new("a-secure-test-secret-that-is-long-enough".to_owned(), 3600);
        let user_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let (token, expires_in) = service
            .issue_access(user_id, Role::Admin, session_id)
            .expect("issue");
        assert_eq!(expires_in, 900);
        let claims = service.decode_claims(&token).expect("decode");
        assert_eq!(claims.user_id, user_id);
        assert_eq!(claims.role, Role::Admin);
        assert_eq!(claims.session_id, session_id);
        assert!(!claims.token_id.is_nil());
        assert_eq!(claims.issuer, "resume-job-matcher");
        assert_eq!(claims.audience, "resume-job-matcher-api");
    }

    #[test]
    fn refresh_tokens_are_random_opaque_and_only_verifiers_are_deterministic() {
        let service =
            SecurityService::new("a-secure-test-secret-that-is-long-enough".to_owned(), 60);
        let first = service.generate_refresh_token().expect("token");
        let second = service.generate_refresh_token().expect("token");
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(
            service.refresh_token_verifier(&first).expect("digest"),
            service.refresh_token_verifier(&first).expect("digest")
        );
        assert_ne!(
            service.refresh_token_verifier(&first).expect("digest"),
            service.refresh_token_verifier(&second).expect("digest")
        );
        assert!(service.refresh_token_verifier("not-a-token").is_err());
    }

    #[test]
    fn token_service_compatibility_methods_use_full_claims() {
        let service =
            SecurityService::new("a-secure-test-secret-that-is-long-enough".to_owned(), 60);
        let user_id = Uuid::now_v7();
        let (token, _) = TokenService::issue(&service, user_id).expect("issue");
        assert_eq!(
            TokenService::decode_user_id(&service, &token).expect("decode"),
            user_id
        );
    }
}
