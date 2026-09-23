use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::ChatRequest;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Auto,
    Glm,
    OpenaiCompatible,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Auto => "auto",
            Self::Glm => "glm",
            Self::OpenaiCompatible => "openai_compatible",
        };
        f.write_str(value)
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "glm" => Ok(Self::Glm),
            "openai_compatible" => Ok(Self::OpenaiCompatible),
            _ => Err(format!("未知 LLM provider：{value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    High,
    #[default]
    Max,
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        };
        f.write_str(value)
    }
}

impl FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err(format!("未知 reasoning_effort：{value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedProvider {
    Glm,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Copy)]
pub struct RequestAdapter {
    provider: ResolvedProvider,
    reasoning_effort: ReasoningEffort,
}

impl RequestAdapter {
    pub fn new(provider: Provider, model: &str, reasoning_effort: ReasoningEffort) -> Self {
        let provider = match provider {
            Provider::Glm => ResolvedProvider::Glm,
            Provider::OpenaiCompatible => ResolvedProvider::OpenaiCompatible,
            Provider::Auto if is_glm_model(model) => ResolvedProvider::Glm,
            Provider::Auto => ResolvedProvider::OpenaiCompatible,
        };
        Self {
            provider,
            reasoning_effort,
        }
    }

    #[cfg(test)]
    pub fn resolved_provider(self) -> ResolvedProvider {
        self.provider
    }

    pub fn adapt(self, request: &mut ChatRequest) {
        request.reasoning_effort = match self.provider {
            ResolvedProvider::Glm => Some(self.reasoning_effort.to_string()),
            ResolvedProvider::OpenaiCompatible => None,
        };
    }
}

pub fn is_glm_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.match_indices("glm").any(|(start, _)| {
        let before = lower[..start].chars().next_back();
        let after = lower[start + 3..].chars().next();
        before.is_none_or(|ch| !ch.is_ascii_alphabetic())
            && after.is_none_or(|ch| !ch.is_ascii_alphabetic())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Message;

    fn request() -> ChatRequest {
        ChatRequest {
            model: "GLM-5.3-Flash".into(),
            messages: vec![Message {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            }],
            tools: Vec::new(),
            temperature: Some(0.7),
            max_tokens: Some(100),
            stream: None,
            stream_options: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn glm_adapter_serializes_top_level_reasoning_effort() {
        for (effort, expected) in [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Max, "max"),
        ] {
            let mut req = request();
            RequestAdapter::new(Provider::Glm, &req.model, effort).adapt(&mut req);
            let json = serde_json::to_value(req).expect("request serializes");
            assert_eq!(json["reasoning_effort"], expected);
        }
    }

    #[test]
    fn generic_adapter_does_not_send_glm_field() {
        let mut req = request();
        RequestAdapter::new(Provider::OpenaiCompatible, &req.model, ReasoningEffort::Low)
            .adapt(&mut req);
        let json = serde_json::to_value(req).expect("request serializes");
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn auto_resolves_only_explicit_glm_names() {
        assert!(is_glm_model("GLM-5.3-Flash"));
        assert!(is_glm_model("provider/GLM5"));
        assert!(is_glm_model("glm"));
        assert!(!is_glm_model("myglmish-model"));
        assert!(is_glm_model("myglmish-GLM-5"));
        assert!(!is_glm_model("gpt-4o"));
        assert_eq!(
            RequestAdapter::new(Provider::Auto, "glm-5", ReasoningEffort::Max).resolved_provider(),
            ResolvedProvider::Glm
        );
        assert_eq!(
            RequestAdapter::new(Provider::Auto, "deepseek-chat", ReasoningEffort::Max)
                .resolved_provider(),
            ResolvedProvider::OpenaiCompatible
        );
    }

    #[test]
    fn enums_reject_unknown_values() {
        assert!("bogus".parse::<Provider>().is_err());
        assert!("off".parse::<ReasoningEffort>().is_err());
    }
}
