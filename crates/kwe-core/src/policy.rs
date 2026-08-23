// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};

const MAX_RULES: usize = 128;
const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackAction {
    KeepRunning,
    Mute,
    Pause,
    Stop,
}

impl PlaybackAction {
    fn precedence(self) -> u8 {
        match self {
            Self::KeepRunning => 0,
            Self::Mute => 1,
            Self::Pause => 2,
            Self::Stop => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "trigger", rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyTrigger {
    Fullscreen,
    Maximized,
    SessionLocked,
    Idle,
    OnBattery,
    PowerSaver,
    FocusedApplication {
        desktop_id: String,
    },
    TimeWindow {
        days: u8,
        start_minute: u16,
        end_minute: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    pub id: String,
    pub trigger: PolicyTrigger,
    pub action: PlaybackAction,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicySnapshot {
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub session_locked: bool,
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub on_battery: bool,
    #[serde(default)]
    pub power_saver: bool,
    #[serde(default)]
    pub focused_application: Option<String>,
    #[serde(default)]
    pub local_weekday: Option<u8>,
    #[serde(default)]
    pub local_minute: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlaybackPolicy {
    pub default_action: PlaybackAction,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PolicyDecision {
    pub action: PlaybackAction,
    pub matched_rule_ids: Vec<String>,
}

impl PlaybackPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.rules.len() > MAX_RULES {
            return Err("playback policy rule count exceeds safety limit".into());
        }
        let mut ids = std::collections::HashSet::with_capacity(self.rules.len());
        for rule in &self.rules {
            if rule.id.is_empty() || rule.id.len() > MAX_ID_BYTES || !ids.insert(rule.id.as_str()) {
                return Err("playback policy rule identity is invalid".into());
            }
            if let PolicyTrigger::FocusedApplication { desktop_id } = &rule.trigger
                && !valid_application_id(desktop_id)
            {
                return Err("playback policy application identity is invalid".into());
            }
            if let PolicyTrigger::TimeWindow {
                days,
                start_minute,
                end_minute,
            } = rule.trigger
                && (days == 0
                    || days & !0x7f != 0
                    || start_minute >= 24 * 60
                    || end_minute >= 24 * 60
                    || start_minute == end_minute)
            {
                return Err("playback policy time window is invalid".into());
            }
        }
        Ok(())
    }

    pub fn evaluate(&self, snapshot: &PolicySnapshot) -> Result<PolicyDecision, String> {
        self.validate()?;
        if snapshot
            .focused_application
            .as_ref()
            .is_some_and(|id| !valid_application_id(id))
        {
            return Err("focused application identity is invalid".into());
        }
        if snapshot.local_weekday.is_some_and(|weekday| weekday >= 7)
            || snapshot
                .local_minute
                .is_some_and(|minute| minute >= 24 * 60)
            || snapshot.local_weekday.is_some() != snapshot.local_minute.is_some()
        {
            return Err("playback policy local time is invalid".into());
        }

        let mut matched_action = None;
        let mut matched_rule_ids = Vec::with_capacity(self.rules.len());
        for rule in &self.rules {
            if rule_matches(&rule.trigger, snapshot) {
                matched_rule_ids.push(rule.id.clone());
                if matched_action.is_none_or(|action: PlaybackAction| {
                    rule.action.precedence() > action.precedence()
                }) {
                    matched_action = Some(rule.action);
                }
            }
        }
        Ok(PolicyDecision {
            action: matched_action.unwrap_or(self.default_action),
            matched_rule_ids,
        })
    }
}

fn rule_matches(trigger: &PolicyTrigger, snapshot: &PolicySnapshot) -> bool {
    match trigger {
        PolicyTrigger::Fullscreen => snapshot.fullscreen,
        PolicyTrigger::Maximized => snapshot.maximized,
        PolicyTrigger::SessionLocked => snapshot.session_locked,
        PolicyTrigger::Idle => snapshot.idle,
        PolicyTrigger::OnBattery => snapshot.on_battery,
        PolicyTrigger::PowerSaver => snapshot.power_saver,
        PolicyTrigger::FocusedApplication { desktop_id } => snapshot
            .focused_application
            .as_ref()
            .is_some_and(|focused| focused.eq_ignore_ascii_case(desktop_id)),
        PolicyTrigger::TimeWindow {
            days,
            start_minute,
            end_minute,
        } => match (snapshot.local_weekday, snapshot.local_minute) {
            (Some(weekday), Some(minute)) if start_minute < end_minute => {
                day_selected(*days, weekday) && minute >= *start_minute && minute < *end_minute
            }
            (Some(weekday), Some(minute)) => {
                (minute >= *start_minute && day_selected(*days, weekday))
                    || (minute < *end_minute && day_selected(*days, (weekday + 6) % 7))
            }
            _ => false,
        },
    }
}

fn day_selected(days: u8, weekday: u8) -> bool {
    days & (1 << weekday) != 0
}

fn valid_application_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, trigger: PolicyTrigger, action: PlaybackAction) -> PolicyRule {
        PolicyRule {
            id: id.into(),
            trigger,
            action,
        }
    }

    #[test]
    fn resolves_conflicts_with_deterministic_safety_precedence() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![
                rule("battery", PolicyTrigger::OnBattery, PlaybackAction::Mute),
                rule(
                    "fullscreen",
                    PolicyTrigger::Fullscreen,
                    PlaybackAction::Pause,
                ),
                rule("locked", PolicyTrigger::SessionLocked, PlaybackAction::Stop),
            ],
        };
        let decision = policy
            .evaluate(&PolicySnapshot {
                fullscreen: true,
                session_locked: true,
                on_battery: true,
                ..PolicySnapshot::default()
            })
            .unwrap();
        assert_eq!(decision.action, PlaybackAction::Stop);
        assert_eq!(
            decision.matched_rule_ids,
            ["battery", "fullscreen", "locked"]
        );
    }

    #[test]
    fn application_matching_is_exact_and_ascii_case_insensitive() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![rule(
                "game",
                PolicyTrigger::FocusedApplication {
                    desktop_id: "org.example.Game".into(),
                },
                PlaybackAction::Pause,
            )],
        };
        assert_eq!(
            policy
                .evaluate(&PolicySnapshot {
                    focused_application: Some("ORG.EXAMPLE.GAME".into()),
                    ..PolicySnapshot::default()
                })
                .unwrap()
                .action,
            PlaybackAction::Pause
        );
        assert_eq!(
            policy
                .evaluate(&PolicySnapshot {
                    focused_application: Some("org.example.Game.demo".into()),
                    ..PolicySnapshot::default()
                })
                .unwrap()
                .action,
            PlaybackAction::KeepRunning
        );
    }

    #[test]
    fn invalid_and_unbounded_rules_fail_closed() {
        let duplicate = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![
                rule("same", PolicyTrigger::Idle, PlaybackAction::Pause),
                rule("same", PolicyTrigger::OnBattery, PlaybackAction::Mute),
            ],
        };
        assert!(duplicate.validate().is_err());

        let unbounded = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: (0..=MAX_RULES)
                .map(|index| {
                    rule(
                        &format!("rule-{index}"),
                        PolicyTrigger::Idle,
                        PlaybackAction::Pause,
                    )
                })
                .collect(),
        };
        assert!(unbounded.validate().is_err());

        let unknown_action = r#"{"default_action":"execute","rules":[]}"#;
        assert!(serde_json::from_str::<PlaybackPolicy>(unknown_action).is_err());
    }

    #[test]
    fn inactive_rules_return_the_configured_default() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::Mute,
            rules: vec![rule(
                "locked",
                PolicyTrigger::SessionLocked,
                PlaybackAction::Stop,
            )],
        };
        let decision = policy.evaluate(&PolicySnapshot::default()).unwrap();
        assert_eq!(decision.action, PlaybackAction::Mute);
        assert!(decision.matched_rule_ids.is_empty());
    }

    #[test]
    fn a_matching_rule_replaces_the_default_before_conflict_resolution() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::Stop,
            rules: vec![rule(
                "active-session",
                PolicyTrigger::FocusedApplication {
                    desktop_id: "org.example.Editor".into(),
                },
                PlaybackAction::KeepRunning,
            )],
        };
        let decision = policy
            .evaluate(&PolicySnapshot {
                focused_application: Some("org.example.Editor".into()),
                ..PolicySnapshot::default()
            })
            .unwrap();
        assert_eq!(decision.action, PlaybackAction::KeepRunning);
    }

    #[test]
    fn time_windows_are_bounded_and_cross_midnight_from_the_start_day() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![rule(
                "weeknight",
                PolicyTrigger::TimeWindow {
                    days: 1 << 0,
                    start_minute: 22 * 60,
                    end_minute: 2 * 60,
                },
                PlaybackAction::Pause,
            )],
        };
        for (weekday, minute, expected) in [
            (0, 22 * 60, PlaybackAction::Pause),
            (1, 60, PlaybackAction::Pause),
            (1, 2 * 60, PlaybackAction::KeepRunning),
            (0, 12 * 60, PlaybackAction::KeepRunning),
        ] {
            assert_eq!(
                policy
                    .evaluate(&PolicySnapshot {
                        local_weekday: Some(weekday),
                        local_minute: Some(minute),
                        ..PolicySnapshot::default()
                    })
                    .unwrap()
                    .action,
                expected
            );
        }
    }

    #[test]
    fn missing_or_invalid_clock_data_fails_closed() {
        let policy = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![rule(
                "workday",
                PolicyTrigger::TimeWindow {
                    days: 0b0001_1111,
                    start_minute: 9 * 60,
                    end_minute: 17 * 60,
                },
                PlaybackAction::Mute,
            )],
        };
        assert_eq!(
            policy.evaluate(&PolicySnapshot::default()).unwrap().action,
            PlaybackAction::KeepRunning
        );
        assert!(
            policy
                .evaluate(&PolicySnapshot {
                    local_weekday: Some(7),
                    local_minute: Some(0),
                    ..PolicySnapshot::default()
                })
                .is_err()
        );
        assert!(
            policy
                .evaluate(&PolicySnapshot {
                    local_weekday: Some(0),
                    local_minute: None,
                    ..PolicySnapshot::default()
                })
                .is_err()
        );

        let invalid = PlaybackPolicy {
            default_action: PlaybackAction::KeepRunning,
            rules: vec![rule(
                "empty",
                PolicyTrigger::TimeWindow {
                    days: 0,
                    start_minute: 60,
                    end_minute: 60,
                },
                PlaybackAction::Stop,
            )],
        };
        assert!(invalid.validate().is_err());
    }
}
