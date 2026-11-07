//! `!changelog`: a version's notes from `CHANGELOG.md`. The file is compiled in, so it
//! describes the build that's running rather than whatever is newest on GitHub.

const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// Versions listed when the one asked for isn't found.
const LISTED_VERSIONS: usize = 8;

struct Version<'a> {
    /// `0.16.0`
    number: &'a str,
    /// `2026-11-06`
    date: &'a str,
    /// The notes under the version's heading.
    body: &'a str,
}

/// The newest version's notes, or those of the version or date in `args`, which can
/// be a full version (`0.12.0`), a short one (`0.12`), or a day (`2026-11-01`).
pub fn reply(args: &str) -> String {
    let versions = versions(CHANGELOG);
    let wanted = args.trim().trim_start_matches(['v', 'V']);
    let found = if wanted.is_empty() {
        versions.first()
    } else {
        versions.iter().find(|version| {
            version.number == wanted || version.date == wanted || version.number.strip_suffix(".0") == Some(wanted)
        })
    };
    match found {
        Some(version) => format!("**{} - {}**\n\n{}", version.number, version.date, version.body),
        None => {
            let recent: Vec<String> =
                versions.iter().take(LISTED_VERSIONS).map(|version| format!("{} ({})", version.number, version.date)).collect();
            format!("There's no version {} in the changelog. Recent versions: {}.", args.trim(), recent.join(", "))
        }
    }
}

/// The newest version and its notes, as the build was made with them.
pub fn current() -> (&'static str, &'static str) {
    let newest = versions(CHANGELOG).into_iter().next().expect("CHANGELOG.md has a version, which a test checks");
    (newest.number, newest.body)
}

/// The changelog lines added since someone was last shown `seen`, a version and its
/// notes as they were then, newest first. Lines added to that same version later,
/// as happens when several deploys land on one day, count as new. Someone never
/// shown anything, or shown a version this changelog doesn't have, gets the newest
/// version's lines.
pub fn new_since(seen: Option<(&str, &str)>) -> Vec<&'static str> {
    new_lines(CHANGELOG, seen)
}

fn new_lines<'a>(changelog: &'a str, seen: Option<(&str, &str)>) -> Vec<&'a str> {
    let versions = versions(changelog);
    let entries = |body: &'a str| body.lines().filter(|line| line.starts_with("- ")).map(|line| &line[2..]);
    let Some((seen_version, seen_notes)) = seen else {
        return versions.first().map(|newest| entries(newest.body).collect()).unwrap_or_default();
    };
    let Some(seen_at) = versions.iter().position(|version| version.number == seen_version) else {
        return versions.first().map(|newest| entries(newest.body).collect()).unwrap_or_default();
    };
    let already: Vec<&str> = seen_notes.lines().collect();
    let mut lines: Vec<&str> = versions[..seen_at].iter().flat_map(|version| entries(version.body)).collect();
    lines.extend(entries(versions[seen_at].body).filter(|line| !already.contains(&format!("- {line}").as_str())));
    lines
}

/// Each `## <version> - <date>` section, newest first as the file lists them.
fn versions(changelog: &str) -> Vec<Version<'_>> {
    let mut versions = Vec::new();
    for section in changelog.split("\n## ").skip(1) {
        let (heading, body) = section.split_once('\n').unwrap_or((section, ""));
        let Some((number, date)) = heading.trim().split_once(" - ") else { continue };
        versions.push(Version { number: number.trim(), date: date.trim(), body: body.trim() });
    }
    versions
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Changelog\n\nIntro.\n\n## 0.2.0 - 2026-07-20\n\n### Added\n\n- Two\n\n## 0.1.0 - 2026-07-18\n\n### Added\n\n- One\n";

    #[test]
    fn reads_each_version() {
        let versions = versions(SAMPLE);
        let read: Vec<(&str, &str, &str)> = versions.iter().map(|version| (version.number, version.date, version.body)).collect();
        assert_eq!(read, [("0.2.0", "2026-07-20", "### Added\n\n- Two"), ("0.1.0", "2026-07-18", "### Added\n\n- One")]);
    }

    #[test]
    fn replies_with_the_newest_or_the_one_asked_for() {
        let newest = &versions(CHANGELOG)[0];
        assert!(reply("").starts_with(&format!("**{} - {}**\n\n", newest.number, newest.date)), "{}", reply(""));
        assert!(reply("0.1.0").contains("**0.1.0 - 2026-07-18**"));
        assert_eq!(reply("0.1"), reply("0.1.0"));
        assert_eq!(reply("v0.1.0"), reply("0.1.0"));
        assert_eq!(reply("2026-07-18"), reply("0.1.0"));
        let missing = reply("9.9");
        assert!(missing.starts_with("There's no version 9.9 in the changelog. Recent versions: "), "{missing}");
        assert!(missing.contains(&format!("{} ({})", newest.number, newest.date)), "{missing}");
    }

    #[test]
    fn finds_what_was_added_since_someone_was_last_told() {
        let changelog = "# Changelog\n\n## 0.3.0 - 2026-07-21\n\n### Added\n\n- Three\n\n### Changed\n\n- Three again\n\n\
                         ## 0.2.0 - 2026-07-20\n\n### Added\n\n- Two\n- Two, later that day\n\n## 0.1.0 - 2026-07-18\n\n- One\n";
        // Never told: the newest version.
        assert_eq!(new_lines(changelog, None), ["Three", "Three again"]);
        // Told about 0.2.0 before its second deploy that day.
        assert_eq!(new_lines(changelog, Some(("0.2.0", "### Added\n\n- Two"))), ["Three", "Three again", "Two, later that day"]);
        // Told about everything already.
        let newest = &versions(changelog)[0];
        assert!(new_lines(changelog, Some((newest.number, newest.body))).is_empty());
        // Told about a version this changelog doesn't have.
        assert_eq!(new_lines(changelog, Some(("9.9.9", ""))), ["Three", "Three again"]);
        assert_eq!(current().0, env!("CARGO_PKG_VERSION"));
    }

    /// The rules AGENTS.md sets for the changelog: the newest version is the crate's,
    /// and each version is one day, newest first, with notes.
    #[test]
    fn the_changelog_follows_its_rules() {
        let versions = versions(CHANGELOG);
        assert_eq!(versions[0].number, env!("CARGO_PKG_VERSION"), "the newest changelog version must match Cargo.toml");
        for pair in versions.windows(2) {
            assert!(pair[0].date > pair[1].date, "{} and {} must be on different days, newest first", pair[0].number, pair[1].number);
        }
        for version in &versions {
            let date = version.date.as_bytes();
            assert!(
                date.len() == 10 && date[4] == b'-' && date[7] == b'-' && version.date.replace('-', "").bytes().all(|b| b.is_ascii_digit()),
                "{} has a bad date {:?}",
                version.number,
                version.date
            );
            assert!(version.body.contains("\n- ") || version.body.starts_with("- "), "{} has no notes", version.number);
        }
    }
}
