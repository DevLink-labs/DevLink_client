use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentPart {
    Frontend,
    Backend,
    Infra,
    #[default]
    Other,
}

impl AgentPart {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "FRONTEND" => Ok(Self::Frontend),
            "BACKEND" => Ok(Self::Backend),
            "INFRA" => Ok(Self::Infra),
            "OTHER" => Ok(Self::Other),
            _ => bail!("Choose FRONTEND, BACKEND, INFRA or OTHER"),
        }
    }
}

pub fn normalize_roles(roles: &[String]) -> Result<Vec<String>> {
    ensure!(roles.len() <= 10, "At most 10 role tags are allowed");
    let mut normalized = Vec::new();
    for role in roles {
        ensure!(
            !role.chars().all(|c| matches!(c,
                '\u{9}'..='\u{d}' | '\u{1c}'..='\u{20}' | '\u{1680}' |
                '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200a}' |
                '\u{2028}' | '\u{2029}' | '\u{205f}' | '\u{3000}'))
                && role.encode_utf16().count() <= 64,
            "Each role tag must contain text and be at most 64 UTF-16 units"
        );
        // 서버의 String.trim()과 동일한 공백 처리
        let tag = role.trim_matches(|c| c <= '\u{20}').to_owned();
        if !normalized.contains(&tag) {
            normalized.push(tag);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_server_utf16_limits_and_normalization_order() {
        let boundary = "😀".repeat(32);
        assert_eq!(
            normalize_roles(std::slice::from_ref(&boundary)).unwrap(),
            vec![boundary]
        );
        assert!(normalize_roles(&["😀".repeat(33)]).is_err());
        assert!(normalize_roles(&[format!(" {}", "a".repeat(64))]).is_err());
        assert!(normalize_roles(&vec!["API".into(); 11]).is_err());
        assert!(normalize_roles(&["\u{3000}".into()]).is_err());
        assert_eq!(
            normalize_roles(&[" API ".into(), "API".into(), "인증".into()]).unwrap(),
            vec!["API", "인증"]
        );
        assert_eq!(
            normalize_roles(&vec!["API".into(); 10]).unwrap(),
            vec!["API"]
        );
        assert!(normalize_roles(&[]).unwrap().is_empty());
    }
}
