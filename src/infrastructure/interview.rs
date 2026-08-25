use crate::domain::InterviewQuestionGenerator;

pub struct DeterministicInterviewGenerator;

impl InterviewQuestionGenerator for DeterministicInterviewGenerator {
    fn generate(
        &self,
        job_title: &str,
        missing_skills: &[String],
        semantic_score: f32,
    ) -> Vec<String> {
        let mut questions = Vec::new();
        let role = if job_title.trim().is_empty() {
            "this role"
        } else {
            job_title.trim()
        };

        for skill in missing_skills.iter().take(5) {
            questions.push(format!(
                "Can you describe your experience with {skill} and how you would apply it in {role}?",
            ));
        }

        if questions.len() < 3 {
            questions.push(format!(
                "What interests you most about {role} and how does your background align with it?"
            ));
        }
        if semantic_score < 50.0 {
            questions.push(
                "How would you describe your approach to aligning resume language with job requirements?"
                    .to_owned(),
            );
        }
        questions.push(format!(
            "Tell us about a challenging project related to {role} and the outcome."
        ));
        questions.push(
            "How do you stay current with relevant technologies and best practices?".to_owned(),
        );

        // Deterministic dedup and cap at 8
        questions.truncate(8);
        questions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::InterviewQuestionGenerator;

    #[test]
    fn deterministic_questions_include_missing_skills() {
        let interview_gen = DeterministicInterviewGenerator;
        let qs = interview_gen.generate(
            "Backend Engineer",
            &["kubernetes".to_owned(), "aws".to_owned()],
            30.0,
        );
        assert!(qs.iter().any(|q| q.contains("kubernetes")));
        assert!(qs.len() >= 5 && qs.len() <= 8);
    }
}
