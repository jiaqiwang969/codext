//! Account choice for a new worker (prototype `src/executor/codex/accounts.ts`, with the A1 rule of
//! design §6.6: the profile with the fewest running workers, at most `max_per_profile` each). A2
//! replaces the count with the quota data of `account/rateLimits/read`.

use std::collections::HashMap;

/// The profile to run a new worker under, or none while every profile is full.
pub fn choose(
    profiles: &[String],
    running: &HashMap<String, usize>,
    max_per_profile: usize,
) -> Option<String> {
    profiles
        .iter()
        .map(|profile| (profile, running.get(profile).copied().unwrap_or(0)))
        .filter(|(_, count)| *count < max_per_profile)
        .min_by_key(|(_, count)| *count)
        .map(|(profile, _)| profile.clone())
}

