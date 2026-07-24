//! Machine-profile resolution. Replaces the `$USER` `case` in the old `.zshenv`:
//! the active profile is chosen from an explicit override, else the first
//! matching rule (by username/hostname), else the configured default.

use crate::config::ProfileConfig;

/// Resolve the active profile, returning its name and a human-readable reason.
pub fn resolve(cfg: &ProfileConfig, override_name: Option<&str>) -> (String, String) {
    if let Some(name) = override_name {
        return (name.to_string(), "requested via --profile".to_string());
    }

    let username = current_username();
    let hostname = current_hostname();

    for rule in &cfg.rules {
        let user_ok = rule
            .username
            .as_ref()
            .map(|u| Some(u) == username.as_ref())
            .unwrap_or(true);
        let host_ok = rule
            .hostname
            .as_ref()
            .map(|h| Some(h) == hostname.as_ref())
            .unwrap_or(true);
        // A rule with no predicates never matches (would be ambiguous).
        let has_predicate = rule.username.is_some() || rule.hostname.is_some();
        if has_predicate && user_ok && host_ok {
            let mut why = Vec::new();
            if let Some(u) = &rule.username {
                why.push(format!("username={u}"));
            }
            if let Some(h) = &rule.hostname {
                why.push(format!("hostname={h}"));
            }
            return (rule.profile.clone(), format!("matched {}", why.join(", ")));
        }
    }

    let default = if cfg.default.is_empty() {
        "personal".to_string()
    } else {
        cfg.default.clone()
    };
    (default, "default profile".to_string())
}

fn current_username() -> Option<String> {
    std::env::var("USER").ok().filter(|s| !s.is_empty())
}

fn current_hostname() -> Option<String> {
    // Prefer the environment, fall back to `hostname(1)` (present on macOS).
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return Some(h);
        }
    }
    crate::proc::output("hostname", &["-s"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchRule;

    fn cfg(default: &str, rules: Vec<MatchRule>) -> ProfileConfig {
        ProfileConfig {
            default: default.to_string(),
            rules,
        }
    }

    #[test]
    fn override_wins() {
        let (name, _) = resolve(&cfg("personal", vec![]), Some("work"));
        assert_eq!(name, "work");
    }

    #[test]
    fn falls_back_to_default() {
        let (name, why) = resolve(&cfg("personal", vec![]), None);
        assert_eq!(name, "personal");
        assert!(why.contains("default"));
    }

    #[test]
    fn matches_on_username() {
        std::env::set_var("USER", "Artur.Aukhatov");
        let rules = vec![MatchRule {
            profile: "rabobank".into(),
            username: Some("Artur.Aukhatov".into()),
            hostname: None,
        }];
        let (name, _) = resolve(&cfg("personal", rules), None);
        assert_eq!(name, "rabobank");
    }
}
