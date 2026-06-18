use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptionsRequest {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, alias = "effort")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub speed: Option<RuntimeSpeed>,
}

impl RuntimeOptionsRequest {
    pub fn normalized(self) -> Result<Self, String> {
        let provider = trim_non_empty(self.provider);
        let model = trim_non_empty(self.model);
        let reasoning_effort = trim_non_empty(self.reasoning_effort)
            .map(|effort| effort.to_ascii_lowercase())
            .map(validate_reasoning_effort)
            .transpose()?;
        Ok(Self {
            provider,
            model,
            reasoning_effort,
            speed: self.speed,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.reasoning_effort.is_none()
            && self.speed.is_none()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(provider) = self.provider.as_deref() {
            parts.push(format!("provider={provider}"));
        }
        if let Some(model) = self.model.as_deref() {
            parts.push(format!("model={model}"));
        }
        if let Some(effort) = self.reasoning_effort.as_deref() {
            parts.push(format!("effort={effort}"));
        }
        if let Some(speed) = self.speed {
            parts.push(format!("speed={speed}"));
        }
        if parts.is_empty() {
            "no changes".into()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSpeed {
    Normal,
    Fast,
}

impl RuntimeSpeed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Fast => "fast",
        }
    }
}

impl fmt::Display for RuntimeSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptionsAck {
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptionsState {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub speed: Option<RuntimeSpeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOptionsCatalog {
    pub source: String,
    pub provider_switching: bool,
    pub current: RuntimeOptionsState,
    #[serde(default)]
    pub providers: Vec<RuntimeProviderOption>,
    #[serde(default)]
    pub models: Vec<RuntimeModelOption>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub speeds: Vec<RuntimeSpeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProviderOption {
    pub id: String,
    pub label: String,
    pub current: bool,
    #[serde(default)]
    pub authenticated: Option<bool>,
    #[serde(default)]
    pub models: Vec<RuntimeModelOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeModelOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub provider: Option<String>,
    pub current: bool,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub speeds: Vec<RuntimeSpeed>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOptionsCatalogRpc {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub fn all_reasoning_efforts() -> Vec<String> {
    REASONING_EFFORTS
        .iter()
        .map(|effort| (*effort).to_string())
        .collect()
}

pub fn dedupe_non_empty(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        let value = value.trim().to_string();
        if !value.is_empty() && !out.iter().any(|existing| existing == &value) {
            out.push(value);
        }
    }
    out
}

fn trim_non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_reasoning_effort(effort: String) -> Result<String, String> {
    if REASONING_EFFORTS.contains(&effort.as_str()) {
        Ok(effort)
    } else {
        Err(format!(
            "reasoning_effort must be one of {}",
            REASONING_EFFORTS.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_empty_strings_and_effort_alias() {
        let req: RuntimeOptionsRequest = serde_json::from_value(serde_json::json!({
            "provider": "  ",
            "model": " gpt-fixture ",
            "effort": " HIGH ",
            "speed": "fast",
        }))
        .unwrap();

        let req = req.normalized().unwrap();
        assert_eq!(req.provider, None);
        assert_eq!(req.model.as_deref(), Some("gpt-fixture"));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(req.speed, Some(RuntimeSpeed::Fast));
    }

    #[test]
    fn rejects_unknown_effort() {
        let req = RuntimeOptionsRequest {
            reasoning_effort: Some("maximum".into()),
            ..RuntimeOptionsRequest::default()
        };
        assert!(req.normalized().is_err());
    }
}
