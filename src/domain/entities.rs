use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Candidate,
    Recruiter,
    Admin,
}

impl Role {
    pub const fn permits(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Admin, _)
                | (Self::Candidate, Self::Candidate)
                | (Self::Recruiter, Self::Recruiter)
        )
    }

    pub const fn can_manage_resumes(self) -> bool {
        matches!(self, Self::Candidate | Self::Admin)
    }

    pub const fn can_manage_jobs(self) -> bool {
        matches!(self, Self::Recruiter | Self::Admin)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    #[serde(default)]
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessTokenClaims {
    pub user_id: Uuid,
    pub role: Role,
    pub session_id: Uuid,
    pub token_id: Uuid,
    pub issuer: String,
    pub audience: String,
    pub issued_at: DateTime<Utc>,
    pub not_before: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub current_refresh_token_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_rotated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Session {
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.created_at <= now && self.expires_at > now
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshToken {
    pub id: Uuid,
    pub session_id: Uuid,
    /// A keyed Argon2id digest of the opaque token; the raw token is never stored.
    pub verifier: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub replaced_by: Option<Uuid>,
}

impl RefreshToken {
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.used_at.is_none()
            && self.revoked_at.is_none()
            && self.issued_at <= now
            && self.expires_at > now
    }

    pub fn digest(&self) -> &str {
        &self.verifier
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Resume {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: Option<String>,
    #[serde(skip_serializing)]
    pub raw_text: String,
    pub skills: Vec<String>,
    #[serde(skip_serializing)]
    pub embedding: Vec<f32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub title: String,
    #[serde(skip_serializing)]
    pub description: String,
    pub skills: Vec<String>,
    #[serde(skip_serializing)]
    pub embedding: Vec<f32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationStatus {
    Submitted,
    Withdrawn,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Application {
    pub id: Uuid,
    pub candidate_id: Uuid,
    pub resume_id: Uuid,
    pub job_id: Uuid,
    pub status: ApplicationStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EducationLevel {
    HighSchool,
    Associate,
    Bachelor,
    Master,
    Doctorate,
}

impl EducationLevel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::HighSchool => "high school",
            Self::Associate => "associate degree",
            Self::Bachelor => "bachelor's degree",
            Self::Master => "master's degree",
            Self::Doctorate => "doctorate",
        }
    }

    pub(crate) const fn rank(self) -> u8 {
        match self {
            Self::HighSchool => 1,
            Self::Associate => 2,
            Self::Bachelor => 3,
            Self::Master => 4,
            Self::Doctorate => 5,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CategoryScore {
    /// Unweighted category fit, normalized to 0..100.
    pub score: f32,
    /// Percentage points this category can contribute to the final score.
    pub weight: u8,
    /// Actual percentage points contributed to the final score.
    pub weighted_score: f32,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CategoryScores {
    pub skills: CategoryScore,
    pub experience: CategoryScore,
    pub education: CategoryScore,
    pub semantic_similarity: CategoryScore,
    pub certifications: CategoryScore,
    pub keywords: CategoryScore,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOutcome {
    Match,
    Partial,
    Mismatch,
    Unknown,
    NotSpecified,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttributeComparison {
    pub resume_value: Option<String>,
    pub job_value: Option<String>,
    pub outcome: ComparisonOutcome,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextComparisons {
    /// These comparisons are explanatory and do not add to the 100-point score.
    pub role: AttributeComparison,
    pub location: AttributeComparison,
    pub availability: AttributeComparison,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Recommendation {
    pub job_id: Uuid,
    pub resume_id: Uuid,
    /// Weighted ATS score normalized to 0..100.
    pub score: f32,
    pub matched_skills: Vec<String>,
    pub missing_skills: Vec<String>,
    pub category_scores: CategoryScores,
    pub reasons: Vec<String>,
    pub recommendations: Vec<String>,
    pub comparisons: ContextComparisons,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MatchResult {
    pub id: Uuid,
    pub resume_id: Uuid,
    pub job_id: Uuid,
    pub candidate_id: Uuid,
    pub recruiter_id: Uuid,
    pub requested_by: Uuid,
    pub report: Recommendation,
    pub created_at: DateTime<Utc>,
}
