use async_trait::async_trait;

#[cfg(test)]
use crate::domain::similarity::extract_skills as extract_profile_skills;
use crate::domain::similarity::{parse_job_profile, parse_resume_profile};
use crate::domain::{DomainError, JobParser, ParsedJob, ParsedResume, ResumeParser};

/// Offline parser used for the first slice. It is deterministic and intentionally
/// conservative; an LLM/document parser can implement `ResumeParser` later.
pub struct DeterministicResumeParser;

pub struct DeterministicJobParser;

#[async_trait]
impl ResumeParser for DeterministicResumeParser {
    async fn parse(&self, raw_text: &str) -> Result<ParsedResume, DomainError> {
        if raw_text.trim().is_empty() {
            return Err(DomainError::InvalidInput("resume text is empty".to_owned()));
        }
        Ok(parse_resume_profile(raw_text, None))
    }
}

#[async_trait]
impl JobParser for DeterministicJobParser {
    async fn parse(&self, title: &str, description: &str) -> Result<ParsedJob, DomainError> {
        if title.trim().is_empty() || description.trim().is_empty() {
            return Err(DomainError::InvalidInput(
                "job title and description are required".to_owned(),
            ));
        }
        Ok(parse_job_profile(title, description))
    }
}

#[cfg(test)]
fn extract_skills(text: &str) -> Vec<String> {
    extract_profile_skills(text)
}

#[cfg(test)]
mod tests {
    use super::{DeterministicJobParser, DeterministicResumeParser, extract_skills};
    use crate::domain::{JobParser, ResumeParser};

    #[test]
    fn extraction_is_case_insensitive_and_sorted() {
        assert_eq!(
            extract_skills("Built APIs with Rust, SQL, and Docker."),
            vec!["docker", "rust", "sql"]
        );
    }

    #[test]
    fn does_not_match_skill_substrings() {
        assert!(extract_skills("A rusty toolbox").is_empty());
    }

    #[tokio::test]
    async fn resume_parser_extracts_structured_ats_fields() {
        let parsed = DeterministicResumeParser
            .parse(
                "Platform Engineer\nExperience: 7 years\nEducation: Master's degree\nLocation: Remote\nAvailability: Full-time\nAWS Certified",
            )
            .await;
        assert!(parsed.is_ok());
        let parsed = parsed.unwrap_or_default();
        assert_eq!(parsed.experience_years, Some(7));
        assert_eq!(
            parsed.education.as_ref().map(|level| level.label()),
            Some("master's degree")
        );
        assert_eq!(parsed.location.as_deref(), Some("Remote"));
        assert_eq!(parsed.availability.as_deref(), Some("Full-time"));
        assert_eq!(parsed.certifications, vec!["aws certified"]);
    }

    #[tokio::test]
    async fn job_parser_extracts_requirements_and_context() {
        let parsed = DeterministicJobParser
            .parse(
                "Backend Engineer",
                "5 years experience. Bachelor's degree. AWS Certified. Location: Remote. Full-time agile role.",
            )
            .await;
        assert!(parsed.is_ok());
        let parsed = parsed.unwrap_or_default();
        assert_eq!(parsed.minimum_experience_years, Some(5));
        assert!(parsed.minimum_education.is_some());
        assert_eq!(parsed.location.as_deref(), Some("Remote"));
        assert_eq!(parsed.availability.as_deref(), Some("Full-time"));
        assert_eq!(parsed.required_certifications, vec!["aws certified"]);
        assert_eq!(parsed.keywords, vec!["agile"]);
    }
}
