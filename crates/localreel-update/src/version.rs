//! Version numbers the update path compares.
//!
//! Deliberately numeric-only: the question is "is there something newer to
//! offer", not a full semver range solve. A pre-release suffix sorts below its
//! base version, so `0.6.0-rc1` is older than `0.6.0`.

use std::cmp::Ordering;

/// Drop a leading `v`/`V` and surrounding whitespace: release tags carry one,
/// `Cargo.toml` does not.
pub fn normalize(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

struct Parsed {
    numbers: Vec<u64>,
    prerelease: Option<String>,
}

fn parse(version: &str) -> Parsed {
    let version = version.trim();
    let (core, rest) = match version.find(['-', '+']) {
        Some(index) => (&version[..index], Some(&version[index + 1..])),
        None => (version, None),
    };
    let numbers = core
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect();
    let prerelease = rest.filter(|_| version.contains('-')).map(str::to_owned);
    Parsed {
        numbers,
        prerelease,
    }
}

/// Compare two versions: `Greater` when `a` is newer than `b`.
pub fn compare(a: &str, b: &str) -> Ordering {
    let a = parse(&normalize(a));
    let b = parse(&normalize(b));
    let width = a.numbers.len().max(b.numbers.len());
    for index in 0..width {
        let left = a.numbers.get(index).copied().unwrap_or(0);
        let right = b.numbers.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    match (&a.prerelease, &b.prerelease) {
        (None, None) => Ordering::Equal,
        // A release outranks its own pre-releases.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    }
}

/// Whether `latest` is a newer release than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    compare(latest, current) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_release_tags() {
        assert_eq!(normalize("v0.5.6"), "0.5.6");
        assert_eq!(normalize(" V1.2.3 "), "1.2.3");
        assert_eq!(normalize("0.5.6"), "0.5.6");
    }

    #[test]
    fn compares_numerically_not_lexically() {
        assert!(is_newer("0.5.10", "0.5.9"));
        assert!(!is_newer("0.5.9", "0.5.10"));
        assert!(is_newer("0.6", "0.5.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.5.6", "0.5.6"));
    }

    #[test]
    fn missing_components_are_zero() {
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare("1.2.1", "1.2"), Ordering::Greater);
    }

    #[test]
    fn prerelease_sorts_below_its_release() {
        assert_eq!(compare("0.6.0-rc1", "0.6.0"), Ordering::Less);
        assert_eq!(compare("0.6.0", "0.6.0-rc1"), Ordering::Greater);
        assert_eq!(compare("0.6.0-rc1", "0.6.0-rc1"), Ordering::Equal);
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(compare("0.5.6+build7", "0.5.6"), Ordering::Equal);
    }
}
