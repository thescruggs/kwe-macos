// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionPolicy {
    pub requested: Vec<String>,
    pub granted: Vec<String>,
}

impl PermissionPolicy {
    pub fn new(requested: &[String], granted: &[String]) -> Self {
        let requested = requested
            .iter()
            .filter(|value| is_known(value))
            .cloned()
            .collect::<Vec<_>>();
        let granted = granted
            .iter()
            .filter(|value| requested.iter().any(|item| item == *value))
            .cloned()
            .collect();
        Self { requested, granted }
    }

    pub fn allows(&self, permission: &str) -> bool {
        self.granted.iter().any(|value| value == permission)
    }
}

fn is_known(value: &str) -> bool {
    matches!(value, "network" | "pointer" | "audio")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grants_only_requested_known_permissions() {
        let policy = PermissionPolicy::new(
            &["audio".into(), "unknown".into()],
            &["audio".into(), "network".into()],
        );
        assert_eq!(policy.requested, ["audio"]);
        assert_eq!(policy.granted, ["audio"]);
        assert!(policy.allows("audio"));
        assert!(!policy.allows("network"));
    }
}
