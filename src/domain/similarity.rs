use std::collections::BTreeSet;

use uuid::Uuid;

use super::{
    entities::{
        AttributeComparison, CategoryScore, CategoryScores, ComparisonOutcome, ContextComparisons,
        EducationLevel, Job, Recommendation, Resume,
    },
    errors::DomainError,
    ports::{ParsedJob, ParsedResume, SimilarityScorer},
};

pub const SKILLS_WEIGHT: u8 = 40;
pub const EXPERIENCE_WEIGHT: u8 = 20;
pub const EDUCATION_WEIGHT: u8 = 10;
pub const SEMANTIC_SIMILARITY_WEIGHT: u8 = 20;
pub const CERTIFICATIONS_WEIGHT: u8 = 5;
pub const KEYWORDS_WEIGHT: u8 = 5;
pub const TOTAL_WEIGHT: u8 = SKILLS_WEIGHT
    + EXPERIENCE_WEIGHT
    + EDUCATION_WEIGHT
    + SEMANTIC_SIMILARITY_WEIGHT
    + CERTIFICATIONS_WEIGHT
    + KEYWORDS_WEIGHT;

#[derive(Clone, Copy, Debug, Default)]
pub struct CosineSimilarity;

impl SimilarityScorer for CosineSimilarity {
    fn similarity(&self, left: &[f32], right: &[f32]) -> Result<f32, DomainError> {
        cosine_similarity(left, right)
    }
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32, DomainError> {
    if left.len() != right.len() {
        return Err(DomainError::EmbeddingDimensionMismatch);
    }
    if left.iter().chain(right).any(|value| !value.is_finite()) {
        return Err(DomainError::InvalidEmbedding);
    }
    if left.is_empty() {
        return Ok(0.0);
    }

    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (left_value, right_value) in left.iter().zip(right) {
        let left_value = f64::from(*left_value);
        let right_value = f64::from(*right_value);
        dot += left_value * right_value;
        left_norm += left_value * left_value;
        right_norm += right_value * right_value;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return Ok(0.0);
    }
    Ok((dot / (left_norm.sqrt() * right_norm.sqrt())).clamp(-1.0, 1.0) as f32)
}

/// Build the deterministic profile used by both parser adapters and matching.
pub fn parse_resume_profile(raw_text: &str, supplied_title: Option<&str>) -> ParsedResume {
    let title = supplied_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| first_non_empty_line(raw_text));
    let role = extract_labeled_value(raw_text, &["role", "position", "target role"])
        .or_else(|| title.clone());

    ParsedResume {
        title,
        role,
        skills: extract_skills(raw_text),
        experience_years: extract_experience_years(raw_text),
        education: extract_education(raw_text),
        certifications: extract_certifications(raw_text),
        keywords: extract_keywords(raw_text),
        location: extract_location(raw_text),
        availability: extract_availability(raw_text),
    }
}

pub fn parse_job_profile(title: &str, description: &str) -> ParsedJob {
    let title = title.trim();
    let combined = format!("{title}\n{description}");
    ParsedJob {
        role: (!title.is_empty()).then(|| title.to_owned()),
        skills: extract_skills(&combined),
        minimum_experience_years: extract_experience_years(description),
        minimum_education: extract_education(description),
        required_certifications: extract_certifications(description),
        keywords: extract_keywords(&combined),
        location: extract_location(&combined),
        availability: extract_availability(&combined),
    }
}

/// Rebuild a profile from the stored entity so old stored records remain matchable
/// even though ingestion currently persists only the original public fields.
pub fn profile_for_resume(resume: &Resume) -> ParsedResume {
    let mut profile = parse_resume_profile(&resume.raw_text, resume.title.as_deref());
    profile.skills = merge_terms(&profile.skills, &resume.skills);
    profile
}

pub fn profile_for_job(job: &Job) -> ParsedJob {
    let mut profile = parse_job_profile(&job.title, &job.description);
    profile.skills = merge_terms(&profile.skills, &job.skills);
    profile
}

pub fn extract_skills(text: &str) -> Vec<String> {
    extract_vocabulary(text, KNOWN_SKILLS)
}

/// Build the weighted ATS report. Category scores are percentages; weighted scores
/// are percentage points. The sum of the weighted scores is always in 0..100.
pub fn build_weighted_recommendation(
    job_id: Uuid,
    resume_id: Uuid,
    semantic_similarity: f32,
    job: &ParsedJob,
    resume: &ParsedResume,
) -> Result<Recommendation, DomainError> {
    if !semantic_similarity.is_finite() {
        return Err(DomainError::InvalidEmbedding);
    }
    Ok(build_report(
        job_id,
        resume_id,
        semantic_similarity,
        job,
        resume,
    ))
}

/// Preserve the previous helper for callers that only provide skill lists.
pub fn build_recommendation(
    job_id: Uuid,
    resume_id: Uuid,
    semantic_similarity: f32,
    job_skills: &[String],
    resume_skills: &[String],
) -> Recommendation {
    let job = ParsedJob {
        skills: job_skills.to_vec(),
        ..ParsedJob::default()
    };
    let resume = ParsedResume {
        skills: resume_skills.to_vec(),
        ..ParsedResume::default()
    };
    let similarity = if semantic_similarity.is_finite() {
        semantic_similarity
    } else {
        0.0
    };
    build_report(job_id, resume_id, similarity, &job, &resume)
}

fn build_report(
    job_id: Uuid,
    resume_id: Uuid,
    semantic_similarity: f32,
    job: &ParsedJob,
    resume: &ParsedResume,
) -> Recommendation {
    let (skills_score, matched_skills, missing_skills) = ratio_score(&job.skills, &resume.skills);
    let (certifications_score, _, missing_certifications) =
        ratio_score(&job.required_certifications, &resume.certifications);
    let (keywords_score, _, missing_keywords) = ratio_score(&job.keywords, &resume.keywords);
    let (experience_score, experience_reasons) =
        score_experience(job.minimum_experience_years, resume.experience_years);
    let (education_score, education_reasons) =
        score_education(job.minimum_education, resume.education);
    let semantic_score = semantic_fit_score(semantic_similarity);

    let skill_reasons = skill_reasons(&job.skills, &matched_skills, &missing_skills);
    let certification_reasons =
        certification_reasons(&job.required_certifications, &missing_certifications);
    let keyword_reasons = keyword_reasons(&job.keywords, &missing_keywords);
    let semantic_reasons = vec![format!(
        "Embedding similarity contributes a {:.1}/100 semantic fit.",
        semantic_score
    )];

    let category_scores = CategoryScores {
        skills: category(skills_score, SKILLS_WEIGHT, skill_reasons.clone()),
        experience: category(
            experience_score,
            EXPERIENCE_WEIGHT,
            experience_reasons.clone(),
        ),
        education: category(education_score, EDUCATION_WEIGHT, education_reasons.clone()),
        semantic_similarity: category(
            semantic_score,
            SEMANTIC_SIMILARITY_WEIGHT,
            semantic_reasons.clone(),
        ),
        certifications: category(
            certifications_score,
            CERTIFICATIONS_WEIGHT,
            certification_reasons.clone(),
        ),
        keywords: category(keywords_score, KEYWORDS_WEIGHT, keyword_reasons.clone()),
    };

    let score = (category_scores.skills.weighted_score
        + category_scores.experience.weighted_score
        + category_scores.education.weighted_score
        + category_scores.semantic_similarity.weighted_score
        + category_scores.certifications.weighted_score
        + category_scores.keywords.weighted_score)
        .clamp(0.0, f32::from(TOTAL_WEIGHT));

    let comparisons = ContextComparisons {
        role: compare_attribute("role", resume.role.as_deref(), job.role.as_deref()),
        location: compare_attribute(
            "location",
            resume.location.as_deref(),
            job.location.as_deref(),
        ),
        availability: compare_attribute(
            "availability",
            resume.availability.as_deref(),
            job.availability.as_deref(),
        ),
    };

    let mut reasons = Vec::new();
    reasons.extend(skill_reasons);
    reasons.extend(experience_reasons);
    reasons.extend(education_reasons);
    reasons.extend(semantic_reasons);
    reasons.extend(certification_reasons);
    reasons.extend(keyword_reasons);
    reasons.push(comparisons.role.reason.clone());
    reasons.push(comparisons.location.reason.clone());
    reasons.push(comparisons.availability.reason.clone());

    let recommendations = recommendations(
        &missing_skills,
        job.minimum_experience_years,
        resume.experience_years,
        job.minimum_education,
        resume.education,
        &missing_certifications,
        &missing_keywords,
        semantic_score,
        &comparisons,
    );

    Recommendation {
        job_id,
        resume_id,
        score,
        matched_skills,
        missing_skills,
        category_scores,
        reasons,
        recommendations,
        comparisons,
    }
}

fn category(score: f32, weight: u8, reasons: Vec<String>) -> CategoryScore {
    let score = score.clamp(0.0, 100.0);
    CategoryScore {
        score,
        weight,
        weighted_score: score * f32::from(weight) / 100.0,
        reasons,
    }
}

fn semantic_fit_score(similarity: f32) -> f32 {
    if !similarity.is_finite() {
        return 0.0;
    }
    // Cosine is [-1, 1], while ATS fit is a non-negative percentage.
    similarity.clamp(0.0, 1.0) * 100.0
}

fn ratio_score(required: &[String], candidate: &[String]) -> (f32, Vec<String>, Vec<String>) {
    let required_set = normalized_set(required);
    let candidate_set = normalized_set(candidate);
    if required_set.is_empty() {
        return (100.0, Vec::new(), Vec::new());
    }

    let matched = required_set
        .intersection(&candidate_set)
        .cloned()
        .collect::<Vec<_>>();
    let missing = required_set
        .difference(&candidate_set)
        .cloned()
        .collect::<Vec<_>>();
    let score = matched.len() as f32 / required_set.len() as f32 * 100.0;
    (score, matched, missing)
}

fn score_experience(required: Option<u16>, candidate: Option<u16>) -> (f32, Vec<String>) {
    match (required, candidate) {
        (None, _) | (Some(0), _) => (
            100.0,
            vec!["No unmet minimum experience requirement was detected.".to_owned()],
        ),
        (Some(required), Some(candidate)) if candidate >= required => (
            100.0,
            vec![format!(
                "Candidate experience ({candidate} years) meets the {required}-year requirement."
            )],
        ),
        (Some(required), Some(candidate)) => (
            (candidate as f32 / required as f32 * 100.0).clamp(0.0, 100.0),
            vec![format!(
                "Candidate experience ({candidate} years) is below the {required}-year requirement."
            )],
        ),
        (Some(required), None) => (
            0.0,
            vec![format!(
                "The job requests {required} years of experience, but no experience duration was detected."
            )],
        ),
    }
}

fn score_education(
    required: Option<EducationLevel>,
    candidate: Option<EducationLevel>,
) -> (f32, Vec<String>) {
    match (required, candidate) {
        (None, _) => (
            100.0,
            vec!["No minimum education requirement was detected.".to_owned()],
        ),
        (Some(required), Some(candidate)) if candidate.rank() >= required.rank() => (
            100.0,
            vec![format!(
                "Candidate education ({}) meets or exceeds the {} requirement.",
                candidate.label(),
                required.label()
            )],
        ),
        (Some(required), Some(candidate)) => (
            (candidate.rank() as f32 / required.rank() as f32 * 100.0).clamp(0.0, 100.0),
            vec![format!(
                "Candidate education ({}) is below the {} requirement.",
                candidate.label(),
                required.label()
            )],
        ),
        (Some(required), None) => (
            0.0,
            vec![format!(
                "The job requests {}, but no education level was detected.",
                required.label()
            )],
        ),
    }
}

fn skill_reasons(required: &[String], matched: &[String], missing: &[String]) -> Vec<String> {
    if required.is_empty() {
        return vec!["No explicit skills were detected in the job requirements.".to_owned()];
    }
    let mut reasons = vec![format!(
        "Matched {} of {} required skills.",
        matched.len(),
        normalized_set(required).len()
    )];
    if !matched.is_empty() {
        reasons.push(format!("Matched skills: {}.", matched.join(", ")));
    }
    if !missing.is_empty() {
        reasons.push(format!("Missing skills: {}.", missing.join(", ")));
    }
    reasons
}

fn certification_reasons(required: &[String], missing: &[String]) -> Vec<String> {
    if required.is_empty() {
        return vec!["No explicit certification requirement was detected.".to_owned()];
    }
    let matched = normalized_set(required).len().saturating_sub(missing.len());
    let mut reasons = vec![format!(
        "Matched {matched} of {} required certifications.",
        normalized_set(required).len()
    )];
    if !missing.is_empty() {
        reasons.push(format!("Missing certifications: {}.", missing.join(", ")));
    }
    reasons
}

fn keyword_reasons(required: &[String], missing: &[String]) -> Vec<String> {
    if required.is_empty() {
        return vec!["No explicit matching keywords were detected.".to_owned()];
    }
    let matched = normalized_set(required).len().saturating_sub(missing.len());
    let mut reasons = vec![format!(
        "Matched {matched} of {} matching keywords.",
        normalized_set(required).len()
    )];
    if !missing.is_empty() {
        reasons.push(format!("Missing keywords: {}.", missing.join(", ")));
    }
    reasons
}

fn compare_attribute(
    label: &str,
    resume_value: Option<&str>,
    job_value: Option<&str>,
) -> AttributeComparison {
    let outcome = match (resume_value, job_value) {
        (None, None) => ComparisonOutcome::NotSpecified,
        (Some(_), None) => ComparisonOutcome::NotSpecified,
        (None, Some(_)) => ComparisonOutcome::Unknown,
        (Some(resume), Some(job)) if values_match(resume, job) => ComparisonOutcome::Match,
        (Some(resume), Some(job)) if label != "role" && values_overlap(resume, job) => {
            ComparisonOutcome::Partial
        }
        (Some(_), Some(_)) => ComparisonOutcome::Mismatch,
    };
    let reason = match outcome {
        ComparisonOutcome::Match => format!("{label} matches the job context."),
        ComparisonOutcome::Partial => format!("{label} partially matches the job context."),
        ComparisonOutcome::Mismatch => format!("{label} differs from the job context."),
        ComparisonOutcome::Unknown => {
            format!("Job {label} is specified, but no candidate {label} was detected.")
        }
        ComparisonOutcome::NotSpecified => {
            format!("{label} comparison is informational because it is not fully specified.")
        }
    };
    AttributeComparison {
        resume_value: resume_value.map(str::to_owned),
        job_value: job_value.map(str::to_owned),
        outcome,
        reason,
    }
}

fn values_match(left: &str, right: &str) -> bool {
    let left = normalize_value(left);
    let right = normalize_value(right);
    left == right || left.contains(&right) || right.contains(&left)
}

fn values_overlap(left: &str, right: &str) -> bool {
    let left = normalized_tokens(left);
    let right = normalized_tokens(right);
    left.intersection(&right)
        .any(|token| !ATTRIBUTE_STOP_WORDS.contains(&token.as_str()))
}

#[allow(clippy::too_many_arguments)]
fn recommendations(
    missing_skills: &[String],
    required_experience: Option<u16>,
    candidate_experience: Option<u16>,
    required_education: Option<EducationLevel>,
    candidate_education: Option<EducationLevel>,
    missing_certifications: &[String],
    missing_keywords: &[String],
    semantic_score: f32,
    comparisons: &ContextComparisons,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if !missing_skills.is_empty() {
        recommendations.push(format!(
            "Address missing skills: {}.",
            missing_skills.join(", ")
        ));
    }
    if let (Some(required), Some(candidate)) = (required_experience, candidate_experience) {
        if candidate < required {
            recommendations.push(format!(
                "Highlight relevant experience or close the {required}-year experience gap."
            ));
        }
    } else if required_experience.is_some() && candidate_experience.is_none() {
        recommendations.push("Add a clearly stated total years-of-experience value.".to_owned());
    }
    if required_education.is_some() && candidate_education.is_none() {
        recommendations.push("Add the highest completed education level.".to_owned());
    }
    if !missing_certifications.is_empty() {
        recommendations.push(format!(
            "Add or obtain relevant certifications: {}.",
            missing_certifications.join(", ")
        ));
    }
    if !missing_keywords.is_empty() {
        recommendations.push(format!(
            "Reflect relevant job language where accurate: {}.",
            missing_keywords.join(", ")
        ));
    }
    if semantic_score < 50.0 {
        recommendations.push(
            "Use clearer role-specific wording in the resume summary and experience sections."
                .to_owned(),
        );
    }
    if comparisons.role.outcome == ComparisonOutcome::Mismatch {
        recommendations
            .push("Clarify how the candidate's target role maps to this position.".to_owned());
    }
    if comparisons.location.outcome == ComparisonOutcome::Mismatch {
        recommendations
            .push("Confirm location or remote-work eligibility before applying.".to_owned());
    }
    if comparisons.availability.outcome == ComparisonOutcome::Mismatch {
        recommendations.push("Clarify availability and start-date constraints.".to_owned());
    }
    if recommendations.is_empty() {
        recommendations
            .push("No specific ATS gaps were detected from the available text.".to_owned());
    }
    recommendations
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn extract_labeled_value(text: &str, labels: &[&str]) -> Option<String> {
    for line in text.lines().map(str::trim) {
        let lower = line.to_ascii_lowercase();
        for label in labels {
            if lower.starts_with(label) {
                let Some(rest) = line.get(label.len()..) else {
                    continue;
                };
                let rest = rest.trim_start();
                if let Some(value) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('-')) {
                    let value = value
                        .trim()
                        .trim_matches(|character: char| matches!(character, ',' | ';' | '|'));
                    if !value.is_empty() {
                        return Some(value.to_owned());
                    }
                }
            }
        }
    }
    None
}

fn extract_experience_years(text: &str) -> Option<u16> {
    let tokens = normalized_tokens_in_order(text);
    let mut result = None;
    for window in tokens.windows(2) {
        let Ok(years) = window[0].parse::<u16>() else {
            continue;
        };
        if years <= 60 && matches!(window[1].as_str(), "year" | "years" | "yr" | "yrs") {
            result = Some(result.map_or(years, |current: u16| current.max(years)));
        }
    }
    result
}

fn extract_education(text: &str) -> Option<EducationLevel> {
    let levels = [
        (
            EducationLevel::Doctorate,
            &["doctorate", "doctoral", "phd"][..],
        ),
        (
            EducationLevel::Master,
            &["master's", "masters", "master degree", "mba", "ms degree"][..],
        ),
        (
            EducationLevel::Bachelor,
            &[
                "bachelor's",
                "bachelors",
                "bachelor degree",
                "bs degree",
                "ba degree",
            ][..],
        ),
        (
            EducationLevel::Associate,
            &["associate degree", "associates degree"][..],
        ),
        (
            EducationLevel::HighSchool,
            &["high school", "secondary school"][..],
        ),
    ];
    levels
        .iter()
        .find(|(_, phrases)| phrases.iter().any(|phrase| contains_term(text, phrase)))
        .map(|(level, _)| *level)
}

fn extract_certifications(text: &str) -> Vec<String> {
    extract_vocabulary(text, KNOWN_CERTIFICATIONS)
}

fn extract_keywords(text: &str) -> Vec<String> {
    extract_vocabulary(text, KNOWN_KEYWORDS)
}

fn extract_location(text: &str) -> Option<String> {
    extract_labeled_value(
        text,
        &[
            "location",
            "based in",
            "preferred location",
            "work location",
        ],
    )
    .or_else(|| {
        ["remote", "hybrid", "on-site", "onsite"]
            .iter()
            .find(|term| contains_term(text, term))
            .map(|term| {
                text.lines()
                    .map(str::trim)
                    .find(|line| contains_term(line, term))
                    .and_then(|line| {
                        line.split_whitespace()
                            .find(|word| normalize_value(word) == **term)
                            .map(trim_extracted_word)
                    })
                    .unwrap_or_else(|| (*term).to_owned())
            })
    })
}

fn extract_availability(text: &str) -> Option<String> {
    extract_labeled_value(
        text,
        &["availability", "available", "start date", "notice period"],
    )
    .or_else(|| {
        [
            "full-time",
            "full time",
            "part-time",
            "part time",
            "contract",
            "immediate start",
            "available immediately",
        ]
        .iter()
        .find(|term| contains_term(text, term))
        .map(|term| find_original_term(text, term).unwrap_or_else(|| (*term).to_owned()))
    })
}

fn extract_vocabulary(text: &str, vocabulary: &[&str]) -> Vec<String> {
    let mut values = vocabulary
        .iter()
        .filter(|term| contains_term(text, term))
        .map(|term| (*term).to_owned())
        .collect::<Vec<_>>();
    values.sort();
    values
}

fn merge_terms(left: &[String], right: &[String]) -> Vec<String> {
    normalized_set(left)
        .into_iter()
        .chain(normalized_set(right))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalized_set(values: &[String]) -> BTreeSet<String> {
    values
        .iter()
        .map(|value| normalize_value(value))
        .filter(|value| !value.is_empty())
        .collect()
}

fn normalized_tokens(value: &str) -> BTreeSet<String> {
    normalized_tokens_in_order(value).into_iter().collect()
}

fn normalized_tokens_in_order(value: &str) -> Vec<String> {
    value
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalize_value(value: &str) -> String {
    normalized_tokens_in_order(value).join(" ")
}

fn trim_extracted_word(value: &str) -> String {
    value
        .trim_matches(|character: char| !character.is_alphanumeric() && character != '-')
        .to_owned()
}

fn find_original_term(text: &str, term: &str) -> Option<String> {
    let term_token_count = normalized_tokens_in_order(term).len();
    text.lines().map(str::trim).find_map(|line| {
        let words = line.split_whitespace().collect::<Vec<_>>();
        if let Some(word) = words
            .iter()
            .find(|word| normalize_value(word) == normalize_value(term))
        {
            return Some(trim_extracted_word(word));
        }
        words.windows(term_token_count).find_map(|window| {
            let original = window.join(" ");
            (normalize_value(&original) == normalize_value(term)).then(|| {
                original
                    .trim_matches(|character: char| {
                        !character.is_alphanumeric() && character != '-' && character != ' '
                    })
                    .to_owned()
            })
        })
    })
}

fn contains_term(text: &str, term: &str) -> bool {
    let text_tokens = normalized_tokens_in_order(text);
    let term_tokens = normalized_tokens_in_order(term);
    !term_tokens.is_empty()
        && text_tokens
            .windows(term_tokens.len())
            .any(|window| window == term_tokens.as_slice())
}

const KNOWN_SKILLS: &[&str] = &[
    "rust",
    "typescript",
    "javascript",
    "python",
    "java",
    "go",
    "sql",
    "postgresql",
    "redis",
    "docker",
    "kubernetes",
    "aws",
    "azure",
    "gcp",
    "react",
    "vue",
    "angular",
    "axum",
    "tokio",
    "graphql",
    "rest",
    "machine learning",
    "natural language processing",
];

const KNOWN_CERTIFICATIONS: &[&str] = &[
    "aws certified",
    "azure certification",
    "google cloud certified",
    "certified kubernetes administrator",
    "certified kubernetes application developer",
    "cka",
    "ckad",
    "cissp",
    "cisa",
    "pmp",
    "ccna",
    "comptia security",
    "scrum master",
    "professional scrum master",
    "cpa",
    "cfa",
    "shrm",
];

const KNOWN_KEYWORDS: &[&str] = &[
    "agile",
    "scrum",
    "kanban",
    "ci cd",
    "microservices",
    "distributed systems",
    "leadership",
    "communication",
    "mentoring",
    "testing",
    "test automation",
    "security",
    "performance",
    "scalability",
    "customer facing",
    "stakeholder management",
    "problem solving",
    "documentation",
    "analytics",
    "compliance",
];

const ATTRIBUTE_STOP_WORDS: &[&str] = &["time", "work", "role", "position", "job", "the", "a"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_cosine_similarity() {
        let result = cosine_similarity(&[1.0, 0.0], &[1.0, 1.0]);
        assert!(result.is_ok());
        let score = result.unwrap_or_default();
        assert!((score - 0.70710677).abs() < 0.0001);
    }

    #[test]
    fn rejects_different_dimensions() {
        assert!(matches!(
            cosine_similarity(&[1.0], &[1.0, 2.0]),
            Err(DomainError::EmbeddingDimensionMismatch)
        ));
    }

    #[test]
    fn weighted_report_sums_to_one_hundred_points() {
        let job = ParsedJob {
            skills: vec!["rust".to_owned(), "sql".to_owned()],
            minimum_experience_years: Some(4),
            minimum_education: Some(EducationLevel::Bachelor),
            required_certifications: vec!["pmp".to_owned()],
            keywords: vec!["agile".to_owned()],
            role: Some("Backend Engineer".to_owned()),
            location: Some("Remote".to_owned()),
            availability: Some("full-time".to_owned()),
        };
        let resume = ParsedResume {
            title: Some("Backend Engineer".to_owned()),
            role: Some("Backend Engineer".to_owned()),
            skills: job.skills.clone(),
            experience_years: Some(5),
            education: Some(EducationLevel::Master),
            certifications: job.required_certifications.clone(),
            keywords: job.keywords.clone(),
            location: Some("Remote".to_owned()),
            availability: Some("full-time".to_owned()),
        };
        let result = build_weighted_recommendation(Uuid::nil(), Uuid::nil(), 1.0, &job, &resume);
        assert!(result.is_ok());
        if let Ok(report) = result {
            assert!((report.score - 100.0).abs() < f32::EPSILON);
            assert_eq!(report.category_scores.skills.weight, SKILLS_WEIGHT);
            assert_eq!(report.comparisons.role.outcome, ComparisonOutcome::Match);
        }

        let mut context_mismatch_resume = resume;
        context_mismatch_resume.role = Some("Data Analyst".to_owned());
        context_mismatch_resume.location = Some("New York".to_owned());
        context_mismatch_resume.availability = Some("part-time".to_owned());
        let mismatch_result = build_weighted_recommendation(
            Uuid::nil(),
            Uuid::nil(),
            1.0,
            &job,
            &context_mismatch_resume,
        );
        assert!(mismatch_result.is_ok());
        if let Ok(report) = mismatch_result {
            assert!((report.score - 100.0).abs() < f32::EPSILON);
            assert_eq!(report.comparisons.role.outcome, ComparisonOutcome::Mismatch);
            assert_eq!(
                report.comparisons.location.outcome,
                ComparisonOutcome::Mismatch
            );
            assert_eq!(
                report.comparisons.availability.outcome,
                ComparisonOutcome::Mismatch
            );
        }
    }

    #[test]
    fn profile_parser_extracts_structured_fields_deterministically() {
        let profile = parse_resume_profile(
            "Backend Engineer\nExperience: 6 years\nEducation: Bachelor's degree\nLocation: Remote\nAvailability: Immediate start\nAWS Certified",
            None,
        );
        assert_eq!(profile.role.as_deref(), Some("Backend Engineer"));
        assert_eq!(profile.experience_years, Some(6));
        assert_eq!(profile.education, Some(EducationLevel::Bachelor));
        assert_eq!(profile.location.as_deref(), Some("Remote"));
        assert_eq!(profile.availability.as_deref(), Some("Immediate start"));
        assert_eq!(profile.certifications, vec!["aws certified"]);
    }
}
