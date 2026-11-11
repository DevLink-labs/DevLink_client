//! S15P31D108-188 (FIND-3-01): validate `find_agent` input and decode the central
//! Agent recommendation `POST /api/v1/teams/{teamId}/agents/recommendations`.
//!
//! The central server computes the scores; the broker only checks the request
//! limits it shares with the server and turns the response into the local shape
//! (string IDs, known enum values). It never chooses a recipient or creates a task.
use crate::task_client::{CentralEnvelope, TaskError, central_error, numeric_id};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

/// Central `RecommendationScorePolicy.MAX_QUESTION_CODE_POINTS`.
pub const MAX_QUESTION_CODE_POINTS: usize = 1_000;
const MAX_CANDIDATES: usize = 5;
const MAX_RELATED_FILES: usize = 5;
/// Related-file sources this client understands. The server may add values (its
/// 4th MVP dependency evidence); consumers must ignore unknown ones.
const KNOWN_SOURCES: [&str; 2] = ["WORKING", "COMMIT"];

/// `Character.isWhitespace`, which the server's `String.strip` uses. Rust's
/// `str::trim` differs (it trims U+00A0, U+2007, U+202F and U+0085 but not
/// U+001C..U+001F), so the broker would otherwise count a different length.
fn java_whitespace(c: char) -> bool {
    matches!(c,
        '\t'..='\r' | '\u{1C}'..='\u{1F}' | ' ' | '\u{1680}' | '\u{2000}'..='\u{2006}'
        | '\u{2008}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{205F}' | '\u{3000}')
}

/// The question as the server will evaluate it, or `None` when it is blank or
/// longer than 1,000 code points after stripping.
pub fn question(value: &str) -> Option<&str> {
    let question = value.trim_matches(java_whitespace);
    let length = question.chars().count();
    (1..=MAX_QUESTION_CODE_POINTS)
        .contains(&length)
        .then_some(question)
}

/// Central `repoKey`: lowercase hex, 64 digits.
pub fn repo_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Request body. `repoKey` is a hint only and never narrows the central search.
pub fn request_body(question: &str, repo_key: Option<&str>) -> Value {
    let mut body = json!({ "question": question });
    if let Some(key) = repo_key {
        body["repoKey"] = json!(key);
    }
    body
}

/// Map the central reply. `requester` is the verified Agent, which the server
/// excludes from its candidates.
pub(crate) fn decode(
    status: u16,
    envelope: CentralEnvelope,
    requester: u64,
) -> Result<Value, TaskError> {
    if status != 200 || !envelope.success {
        // Read-only: no recommendation was stored, so this is a definite rejection.
        if status == 503 && envelope.code.as_deref() == Some("GIT_ROUTING_UNAVAILABLE") {
            return Err(TaskError::rejected("GIT_ROUTING_UNAVAILABLE", 503));
        }
        return Err(central_error(status, envelope.code.as_deref()));
    }
    let data: Recommendation = serde_json::from_value(envelope.data.ok_or_else(protocol_error)?)
        .map_err(|_| protocol_error())?;
    data.local(requester).ok_or_else(protocol_error)
}

fn protocol_error() -> TaskError {
    TaskError::unknown("CENTRAL_PROTOCOL_ERROR")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recommendation {
    status: String,
    git_coverage: Coverage,
    evaluated_at: String,
    candidates: Vec<Candidate>,
    insufficient_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coverage {
    state: String,
    reason: Option<String>,
    working_state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    agent_id: u64,
    name: String,
    part: Option<String>,
    connection_status: String,
    score: f64,
    scores: Scores,
    matched_roles: Vec<String>,
    related_files: Vec<RelatedFile>,
    latest_commit_at: Option<String>,
    working_observed_at: Option<String>,
    insufficient_reason: Option<String>,
}

#[derive(Deserialize)]
struct Scores {
    role: f64,
    git: f64,
    working: f64,
    frequency: f64,
    recency: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelatedFile {
    repo_id: u64,
    relative_path: String,
    sources: Vec<String>,
    observed_at: Option<String>,
}

fn score(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

impl Recommendation {
    fn local(self, requester: u64) -> Option<Value> {
        let coverage = &self.git_coverage;
        let coverage_ok = matches!(
            (coverage.state.as_str(), coverage.reason.as_deref()),
            ("AVAILABLE", None)
                | (
                    "NOT_COLLECTED",
                    Some("NO_TEAM_REPOSITORIES" | "NO_RETAINED_OBSERVATIONS")
                )
        );
        if !coverage_ok
            || !matches!(
                coverage.working_state.as_str(),
                "FRESH" | "STALE" | "NOT_COLLECTED"
            )
            || self.evaluated_at.trim().is_empty()
            || self.candidates.len() > MAX_CANDIDATES
        {
            return None;
        }
        // Candidate-level reason must agree with the overall status.
        let candidate_reason = match (
            self.status.as_str(),
            self.insufficient_reason.as_deref(),
            self.candidates.len(),
        ) {
            ("MATCHED", None, 1) => None,
            ("MULTIPLE_CANDIDATES", None, 2..) => None,
            ("INSUFFICIENT_EVIDENCE", Some("NO_ELIGIBLE_AGENTS"), 0) => None,
            ("INSUFFICIENT_EVIDENCE", Some("NO_MATCHING_EVIDENCE"), 1..) => {
                Some("NO_MATCHING_EVIDENCE")
            }
            _ => return None,
        };
        let mut seen = HashSet::new();
        let mut candidates = Vec::with_capacity(self.candidates.len());
        for candidate in self.candidates {
            if numeric_id(&candidate.agent_id.to_string()).is_none()
                || candidate.agent_id == requester
                || !seen.insert(candidate.agent_id)
                || candidate.insufficient_reason.as_deref() != candidate_reason
            {
                return None;
            }
            candidates.push(candidate.local()?);
        }
        Some(json!({
            "status": self.status,
            "evaluatedAt": self.evaluated_at,
            "insufficientReason": self.insufficient_reason,
            "gitCoverage": {
                "state": coverage.state,
                "reason": coverage.reason,
                "workingState": coverage.working_state,
            },
            "candidates": candidates,
        }))
    }
}

impl Candidate {
    fn local(self) -> Option<Value> {
        let scores = &self.scores;
        // Reject only empty strings: the server accepts names that are blank to Rust (e.g.
        // NBSP), and one such teammate must not break find_agent for the whole team.
        if self.name.is_empty()
            || self.part.as_deref().is_some_and(str::is_empty)
            || !matches!(self.connection_status.as_str(), "ONLINE" | "OFFLINE")
            || ![
                self.score,
                scores.role,
                scores.git,
                scores.working,
                scores.frequency,
                scores.recency,
            ]
            .into_iter()
            .all(score)
            || self.matched_roles.iter().any(String::is_empty)
            || self.related_files.len() > MAX_RELATED_FILES
        {
            return None;
        }
        let mut files = Vec::with_capacity(self.related_files.len());
        for file in self.related_files {
            if numeric_id(&file.repo_id.to_string()).is_none() || file.relative_path.is_empty() {
                return None;
            }
            let sources: Vec<String> = file
                .sources
                .into_iter()
                .filter(|source| KNOWN_SOURCES.contains(&source.as_str()))
                .collect();
            files.push(json!({
                "repoId": file.repo_id.to_string(),
                "relativePath": file.relative_path,
                "sources": sources,
                "observedAt": file.observed_at,
            }));
        }
        Some(json!({
            "agentId": self.agent_id.to_string(),
            "name": self.name,
            "part": self.part,
            "connectionStatus": self.connection_status,
            "score": self.score,
            "scores": {
                "role": scores.role,
                "git": scores.git,
                "working": scores.working,
                "frequency": scores.frequency,
                "recency": scores.recency,
            },
            "matchedRoles": self.matched_roles,
            "relatedFiles": files,
            "latestCommitAt": self.latest_commit_at,
            "workingObservedAt": self.working_observed_at,
            "insufficientReason": self.insufficient_reason,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(data: Value) -> CentralEnvelope {
        serde_json::from_value(json!({"success": true, "data": data})).unwrap()
    }

    fn candidate(id: u64) -> Value {
        json!({"agentId": id, "name": "백엔드 Agent", "part": "BACKEND", "connectionStatus": "ONLINE",
            "score": 41.5, "scores": {"role": 20.0, "git": 21.5, "working": 15.0, "frequency": 3.0, "recency": 3.5},
            "matchedRoles": ["로그인"], "relatedFiles": [{"repoId": 7, "relativePath": "src/Login.java",
                "sources": ["WORKING", "DEPENDENCY"], "observedAt": "2026-11-09T01:00:00Z"}],
            "latestCommitAt": "2026-11-08T10:00:00Z", "workingObservedAt": null, "insufficientReason": null})
    }

    fn matched(candidates: Vec<Value>, status: &str, reason: Value) -> Value {
        json!({"status": status, "gitCoverage": {"state": "AVAILABLE", "reason": null, "workingState": "FRESH"},
            "evaluatedAt": "2026-11-09T02:00:00Z", "candidates": candidates, "insufficientReason": reason})
    }

    #[test]
    fn question_uses_the_server_strip_and_code_point_limit() {
        assert_eq!(question("  로그인 오류 \n"), Some("로그인 오류"));
        assert_eq!(question("\u{1C}x\u{3000}"), Some("x"));
        // NBSP is not Java whitespace: it stays and counts.
        assert_eq!(question("\u{A0}"), Some("\u{A0}"));
        assert_eq!(question(" \t\n"), None);
        assert!(question(&"가".repeat(1_000)).is_some());
        assert!(question(&format!(" {} ", "😀".repeat(1_000))).is_some());
        assert!(question(&"가".repeat(1_001)).is_none());
    }

    #[test]
    fn repo_key_is_lowercase_hex_64() {
        assert!(repo_key(&"a1".repeat(32)));
        assert!(!repo_key(&"A1".repeat(32)));
        assert!(!repo_key(&"a".repeat(63)));
        assert!(!repo_key(&"g".repeat(64)));
    }

    #[test]
    fn decodes_ids_as_strings_and_drops_unknown_sources() {
        let data = decode(
            200,
            envelope(matched(
                vec![candidate(i64::MAX as u64)],
                "MATCHED",
                Value::Null,
            )),
            12,
        )
        .unwrap();
        let first = &data["candidates"][0];
        assert_eq!(first["agentId"], "9223372036854775807");
        assert_eq!(first["relatedFiles"][0]["repoId"], "7");
        assert_eq!(first["relatedFiles"][0]["sources"], json!(["WORKING"]));
        assert_eq!(first["connectionStatus"], "ONLINE");
        assert_eq!(data["gitCoverage"]["workingState"], "FRESH");
    }

    #[test]
    fn insufficient_evidence_keeps_reference_candidates() {
        let mut reference = candidate(13);
        reference["insufficientReason"] = json!("NO_MATCHING_EVIDENCE");
        let data = matched(
            vec![reference],
            "INSUFFICIENT_EVIDENCE",
            json!("NO_MATCHING_EVIDENCE"),
        );
        assert!(decode(200, envelope(data), 12).is_ok());
        let none = matched(vec![], "INSUFFICIENT_EVIDENCE", json!("NO_ELIGIBLE_AGENTS"));
        assert_eq!(
            decode(200, envelope(none), 12).unwrap()["candidates"],
            json!([])
        );
    }

    #[test]
    fn rejects_inconsistent_or_unsafe_responses() {
        let bad = [
            matched(vec![candidate(12)], "MATCHED", Value::Null),
            matched(
                vec![candidate(13), candidate(13)],
                "MULTIPLE_CANDIDATES",
                Value::Null,
            ),
            matched(vec![candidate(13), candidate(14)], "MATCHED", Value::Null),
            matched(vec![candidate(13)], "MULTIPLE_CANDIDATES", Value::Null),
            matched(
                (13..19).map(candidate).collect(),
                "MULTIPLE_CANDIDATES",
                Value::Null,
            ),
            matched(
                vec![candidate(13)],
                "INSUFFICIENT_EVIDENCE",
                json!("NO_MATCHING_EVIDENCE"),
            ),
            matched(
                vec![candidate(13)],
                "INSUFFICIENT_EVIDENCE",
                json!("NO_ELIGIBLE_AGENTS"),
            ),
            matched(vec![candidate(13)], "UNKNOWN", Value::Null),
            matched(
                vec![candidate(9_223_372_036_854_775_808)],
                "MATCHED",
                Value::Null,
            ),
        ];
        for (index, data) in bad.into_iter().enumerate() {
            assert_eq!(
                decode(200, envelope(data), 12).unwrap_err().code,
                "CENTRAL_PROTOCOL_ERROR",
                "{index}"
            );
        }
        let mut negative = candidate(13);
        negative["scores"]["role"] = json!(-1.0);
        assert!(
            decode(
                200,
                envelope(matched(vec![negative], "MATCHED", Value::Null)),
                12
            )
            .is_err()
        );
        let mut nbsp = candidate(13);
        nbsp["name"] = json!("\u{A0}");
        assert!(
            decode(
                200,
                envelope(matched(vec![nbsp], "MATCHED", Value::Null)),
                12
            )
            .is_ok()
        );
        let mut empty = candidate(13);
        empty["name"] = json!("");
        assert!(
            decode(
                200,
                envelope(matched(vec![empty], "MATCHED", Value::Null)),
                12
            )
            .is_err()
        );
        let mut status = candidate(13);
        status["connectionStatus"] = json!("AWAY");
        assert!(
            decode(
                200,
                envelope(matched(vec![status], "MATCHED", Value::Null)),
                12
            )
            .is_err()
        );
        let mut coverage = matched(vec![candidate(13)], "MATCHED", Value::Null);
        coverage["gitCoverage"]["reason"] = json!("NO_TEAM_REPOSITORIES");
        assert!(decode(200, envelope(coverage), 12).is_err());
    }

    #[test]
    fn maps_central_errors_without_inventing_outcomes() {
        let failure = |code: &str| -> CentralEnvelope {
            serde_json::from_value(json!({"success": false, "code": code})).unwrap()
        };
        let routing = decode(503, failure("GIT_ROUTING_UNAVAILABLE"), 12).unwrap_err();
        assert_eq!(
            (routing.code, routing.status, routing.outcome),
            ("GIT_ROUTING_UNAVAILABLE", 503, "rejected")
        );
        let other = decode(503, failure("INTERNAL_ERROR"), 12).unwrap_err();
        assert_eq!(
            (other.code, other.outcome),
            ("CENTRAL_UNAVAILABLE", "unknown")
        );
        assert_eq!(
            decode(400, failure("VALIDATION_FAILED"), 12)
                .unwrap_err()
                .code,
            "VALIDATION_FAILED"
        );
    }
}
