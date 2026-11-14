//! Non-secret OAuth identity of a Claude config dir (`.claude.json`) + same-account grouping.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/claude/identity.rs
//! Deps:    serde / serde_json
//! Tested:  inline `#[cfg(test)]` — parse happy/stub/malformed + the grouping helpers
//!
//! Key responsibilities:
//! - `read_identity`: the `oauthAccount.{emailAddress,accountUuid}` pair a dir is logged in as.
//! - `collision_groups` / `duplicate_of`: which configured accounts are the SAME Anthropic account
//!   (same `accountUuid` ⇒ one shared quota rendered N times with byte-identical limit bars — the
//!   2026-08-26 eletrixa/rob7 mystery). Shared by `tok doctor` and the TUI title tag.
//!
//! Design constraints:
//! - Reads `.claude.json` only — never `.credentials.json` (that is `creds.rs`'s redact-everything
//!   contract). Nothing beyond email + uuid is ever surfaced.
//! - Grouping key is `accountUuid` alone: same account ⇒ same quota. Org-level grouping is NOT a
//!   quota statement (two accounts in one org bill separately), so it is deliberately not used.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Deserialize;

use crate::domain::{Account, Provider};

/// The identity a Claude config dir is logged in as (from `.claude.json`, non-secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthIdentity {
    pub email: Option<String>,
    pub account_uuid: String,
}

#[derive(Deserialize)]
struct ClaudeJson {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<OauthAccount>,
}

#[derive(Deserialize)]
struct OauthAccount {
    #[serde(rename = "emailAddress")]
    email: Option<String>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
}

/// Pure: `None` on malformed JSON, no `oauthAccount` (a never-logged-in stub), or an empty uuid.
pub fn parse_identity(bytes: &[u8]) -> Option<OauthIdentity> {
    let oauth = serde_json::from_slice::<ClaudeJson>(bytes)
        .ok()?
        .oauth_account?;
    let account_uuid = oauth.account_uuid.filter(|u| !u.is_empty())?;
    Some(OauthIdentity {
        email: oauth.email,
        account_uuid,
    })
}

/// Read `<config_dir>/.claude.json`; a missing/unreadable file is simply `None` (stub dir).
pub fn read_identity(config_dir: &Path) -> Option<OauthIdentity> {
    parse_identity(&std::fs::read(config_dir.join(".claude.json")).ok()?)
}

/// Every ACTIVE Claude account with a readable identity, in config order — the one shared filter
/// both `tok doctor` and the TUI feed into the grouping helpers below. Inactive accounts are out:
/// `active = false` is doctor's own advice for a duplicate, so it must clear the warning, and a
/// live row must never be tagged as a copy of a retired one.
pub fn resolve_identities(accounts: &[Account]) -> Vec<(String, OauthIdentity)> {
    accounts
        .iter()
        .filter(|a| a.active && a.provider == Provider::Claude)
        .filter_map(|a| read_identity(a.config_dir.as_deref()?).map(|id| (a.id.clone(), id)))
        .collect()
}

/// Pure: groups of ≥2 account ids sharing one `accountUuid`, each labelled by the group's email
/// (uuid prefix when absent). Ids keep config order within a group; groups sort by uuid.
pub fn collision_groups(resolved: &[(String, OauthIdentity)]) -> Vec<(String, Vec<String>)> {
    let mut by_uuid: BTreeMap<&str, (Option<&str>, Vec<String>)> = BTreeMap::new();
    for (id, identity) in resolved {
        let entry = by_uuid.entry(&identity.account_uuid).or_default();
        entry.0 = entry.0.or(identity.email.as_deref());
        entry.1.push(id.clone());
    }
    by_uuid
        .into_iter()
        .filter(|(_, (_, ids))| ids.len() >= 2)
        .map(|(uuid, (email, ids))| {
            let label = email.map_or_else(|| uuid.chars().take(8).collect(), str::to_string);
            (label, ids)
        })
        .collect()
}

/// Pure: for every account that duplicates an earlier-configured one, `id → that first id`.
pub fn duplicate_of(resolved: &[(String, OauthIdentity)]) -> HashMap<String, String> {
    collision_groups(resolved)
        .into_iter()
        .flat_map(|(_, ids)| {
            let first = ids[0].clone();
            ids.into_iter().skip(1).map(move |id| (id, first.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_identity_reads_email_and_uuid_or_none() {
        let ok = br#"{"oauthAccount":{"emailAddress":"a@b.c","accountUuid":"u-1","organizationUuid":"o"}}"#;
        assert_eq!(
            parse_identity(ok),
            Some(OauthIdentity {
                email: Some("a@b.c".into()),
                account_uuid: "u-1".into()
            })
        );
        assert_eq!(parse_identity(br#"{"numStartups":3}"#), None, "stub dir");
        assert_eq!(parse_identity(b"{not json"), None);
        assert_eq!(
            parse_identity(br#"{"oauthAccount":{"accountUuid":""}}"#),
            None
        );
    }

    #[test]
    fn collision_groups_flags_only_shared_account_uuids() {
        let id = |email: &str, uuid: &str| OauthIdentity {
            email: Some(email.to_string()),
            account_uuid: uuid.to_string(),
        };
        let resolved = vec![
            ("eletrixa".to_string(), id("someone@example.com", "uuid-a")),
            ("rob7".to_string(), id("someone@example.com", "uuid-a")),
            ("solo".to_string(), id("someone@example.com", "uuid-b")),
        ];
        assert_eq!(
            collision_groups(&resolved),
            vec![(
                "someone@example.com".to_string(),
                vec!["eletrixa".to_string(), "rob7".to_string()]
            )]
        );
        let dup = duplicate_of(&resolved);
        assert_eq!(dup.get("rob7").map(String::as_str), Some("eletrixa"));
        assert!(!dup.contains_key("eletrixa") && !dup.contains_key("solo"));
    }
}
