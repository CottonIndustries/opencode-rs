//! Configuration policies and managed-policy decisions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub action: String,
    pub resource: String,
    #[serde(default = "allow")]
    pub effect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn allow() -> String {
    "allow".into()
}

/// The last statement matching an action and resource wins.
pub fn decision<'a>(policies: &'a [Policy], action: &str, resource: &str) -> Option<&'a Policy> {
    policies
        .iter()
        .rev()
        .find(|policy| policy.action == "*" || policy.action == action)
        .filter(|policy| crate::util::wildcard(&policy.resource, resource))
}

pub fn blocked(policies: &[Policy], action: &str, resources: &[String]) -> Option<String> {
    for resource in resources {
        if let Some(policy) = decision(policies, action, resource) {
            if policy.effect == "deny" {
                return Some(
                    policy
                        .message
                        .clone()
                        .unwrap_or_else(|| format!("Blocked by configuration policy: {action} {resource}")),
                );
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_matching_policy_wins() {
        let policies = vec![
            Policy { action: "edit".into(), resource: "*".into(), effect: "allow".into(), message: None },
            Policy { action: "edit".into(), resource: "*.env".into(), effect: "deny".into(), message: Some("no".into()) },
        ];
        assert!(blocked(&policies, "edit", &["src/main.rs".into()]).is_none());
        assert_eq!(blocked(&policies, "edit", &[".env".into()]).as_deref(), Some("no"));
        assert!(blocked(&policies, "read", &[".env".into()]).is_none());
    }

    #[test]
    fn config_accepts_policy_documents() {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "policy": [{ "action": "bash", "resource": "rm *", "effect": "deny" }]
        }))
        .unwrap();
        assert_eq!(config.policies.len(), 1);
        assert!(blocked(&config.policies, "bash", &["rm -rf /".into()]).is_some());
    }
}
