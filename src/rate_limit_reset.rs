use anyhow::{Result, anyhow};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RateLimitResetCredit {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub granted_at: Option<i64>,
    pub expires_at: Option<i64>,
}

/// Chooses the credit that is most urgent to use: earliest expiry first, then
/// oldest grant, then ID for a stable final tie-break.
pub(crate) fn select_best_rate_limit_reset_credit(usage: &Value) -> Result<RateLimitResetCredit> {
    let mut credits = usage["rateLimitResetCredits"]["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|credit| credit["status"].as_str() == Some("available"))
        .filter_map(|credit| {
            let id = credit["id"].as_str()?.trim();
            (!id.is_empty()).then(|| RateLimitResetCredit {
                id: id.to_string(),
                title: optional_nonempty_string(&credit["title"]),
                description: optional_nonempty_string(&credit["description"]),
                granted_at: credit["grantedAt"].as_i64(),
                expires_at: credit["expiresAt"].as_i64(),
            })
        })
        .collect::<Vec<_>>();

    credits.sort_by(|left, right| {
        left.expires_at
            .unwrap_or(i64::MAX)
            .cmp(&right.expires_at.unwrap_or(i64::MAX))
            .then_with(|| {
                left.granted_at
                    .unwrap_or(i64::MAX)
                    .cmp(&right.granted_at.unwrap_or(i64::MAX))
            })
            .then_with(|| left.id.cmp(&right.id))
    });

    credits
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no selectable rate-limit reset credits are available"))
}

fn optional_nonempty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::select_best_rate_limit_reset_credit;

    #[test]
    fn selects_earliest_expiring_available_credit_with_stable_ties() {
        let usage = json!({
            "rateLimitResetCredits": {
                "credits": [
                    { "id": "never", "status": "available", "grantedAt": 1 },
                    { "id": "later", "status": "available", "grantedAt": 1, "expiresAt": 30 },
                    { "id": "newer", "status": "available", "grantedAt": 20, "expiresAt": 10 },
                    { "id": "older-b", "status": "available", "grantedAt": 10, "expiresAt": 10 },
                    { "id": "older-a", "status": "available", "grantedAt": 10, "expiresAt": 10 },
                    { "id": "redeemed", "status": "redeemed", "expiresAt": 1 }
                ]
            }
        });

        assert_eq!(
            select_best_rate_limit_reset_credit(&usage).unwrap().id,
            "older-a"
        );
    }

    #[test]
    fn refuses_when_the_server_does_not_supply_selectable_credit_details() {
        let usage = json!({ "rateLimitResetCredits": { "availableCount": 2 } });

        assert_eq!(
            select_best_rate_limit_reset_credit(&usage)
                .unwrap_err()
                .to_string(),
            "no selectable rate-limit reset credits are available"
        );
    }
}
