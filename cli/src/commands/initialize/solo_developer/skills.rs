//! The ralphus skills shipped with the CLI and the file-level install rules.
//! Which directory a harness reads skills from is the harness's own business
//! (`ralphus_runner::skills_install::SkillsHarness`); this module only knows
//! the skill contents and how to write one without clobbering a user's edits.

use std::io;
use std::path::{Path, PathBuf};

use ralphus_runner::skills_install::SkillsHarness;

/// One skill: installed as `<skills dir>/<name>/SKILL.md`.
pub(super) struct Skill {
    pub name: &'static str,
    pub contents: &'static str,
}

pub(super) const SKILLS: &[Skill] = &[
    Skill {
        name: "ralphus-submit",
        contents: include_str!("../../../../assets/skills/ralphus-submit/SKILL.md"),
    },
    Skill {
        name: "ralphus-feedback",
        contents: include_str!("../../../../assets/skills/ralphus-feedback/SKILL.md"),
    },
];

/// What [`install_into`] did with one skill.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// No file existed; it was written.
    Installed(PathBuf),
    /// The installed file already matches.
    Unchanged(PathBuf),
    /// A modified file exists and the caller declined to replace it.
    Skipped(PathBuf),
    /// A modified file was replaced; the old one was kept at `backup`.
    Replaced { path: PathBuf, backup: PathBuf },
}

/// Writes `skill` under `skills_dir`. An identical existing file is left
/// alone. A differing one is only replaced when `overwrite` says so, and the
/// previous contents are first copied to `SKILL.md.bak`.
pub(super) fn install_into(
    skills_dir: &Path,
    skill: &Skill,
    overwrite: &mut dyn FnMut(&Path) -> bool,
) -> io::Result<Outcome> {
    let path = skills_dir.join(skill.name).join("SKILL.md");
    let existing = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let Some(existing) = existing else {
        std::fs::create_dir_all(skills_dir.join(skill.name))?;
        std::fs::write(&path, skill.contents)?;
        return Ok(Outcome::Installed(path));
    };
    if existing == skill.contents.as_bytes() {
        return Ok(Outcome::Unchanged(path));
    }
    if !overwrite(&path) {
        return Ok(Outcome::Skipped(path));
    }
    let backup = path.with_file_name("SKILL.md.bak");
    std::fs::write(&backup, existing)?;
    std::fs::write(&path, skill.contents)?;
    Ok(Outcome::Replaced { path, backup })
}

/// Indices into `harnesses` named by a comma-separated `answer` (`all`,
/// `none`, or backend names); unknown names are returned in the second list.
pub(super) fn select_harnesses(
    answer: &str,
    harnesses: &[&dyn SkillsHarness],
) -> (Vec<usize>, Vec<String>) {
    let mut chosen = Vec::new();
    let mut unknown = Vec::new();
    for token in answer
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
    {
        match token.as_str() {
            "none" => return (Vec::new(), unknown),
            "all" => return ((0..harnesses.len()).collect(), unknown),
            _ => {
                let position = harnesses.iter().position(|harness| {
                    harness.backend_name() == token
                        || (token == "claude" && harness.backend_name() == "claude-code")
                });
                match position {
                    Some(index) if !chosen.contains(&index) => chosen.push(index),
                    Some(_) => {}
                    None => unknown.push(token),
                }
            }
        }
    }
    (chosen, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str);

    impl SkillsHarness for Fake {
        fn backend_name(&self) -> &'static str {
            self.0
        }
        fn display_name(&self) -> &'static str {
            self.0
        }
        fn skills_dir(&self) -> Option<PathBuf> {
            None
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-skills-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shipped_skills_have_frontmatter_and_no_personal_content() {
        for skill in SKILLS {
            assert!(
                skill
                    .contents
                    .starts_with(&format!("---\nname: {}\n", skill.name)),
                "{}",
                skill.name
            );
            for personal in ["staging", "gpt-5.6", "haiku", "C:\\", "/home/"] {
                assert!(!skill.contents.contains(personal), "{personal}");
            }
        }
    }

    #[test]
    fn installs_then_leaves_identical_file_alone() {
        let dir = temp_dir("fresh");
        let skill = &SKILLS[0];
        let first = install_into(&dir, skill, &mut |_| panic!("no prompt")).unwrap();
        assert!(matches!(first, Outcome::Installed(_)));
        let second = install_into(&dir, skill, &mut |_| panic!("no prompt")).unwrap();
        assert!(matches!(second, Outcome::Unchanged(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn modified_file_is_kept_unless_overwrite_is_granted() {
        let dir = temp_dir("modified");
        let skill = &SKILLS[1];
        let path = dir.join(skill.name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "my edits").unwrap();

        let skipped = install_into(&dir, skill, &mut |_| false).unwrap();
        assert!(matches!(skipped, Outcome::Skipped(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my edits");

        let replaced = install_into(&dir, skill, &mut |_| true).unwrap();
        assert!(matches!(replaced, Outcome::Replaced { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), skill.contents);
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("SKILL.md.bak")).unwrap(),
            "my edits"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selects_harnesses_by_name_all_and_none() {
        let (a, b) = (Fake("claude-code"), Fake("pi"));
        let harnesses: Vec<&dyn SkillsHarness> = vec![&a, &b];
        assert_eq!(select_harnesses("all", &harnesses).0, vec![0, 1]);
        assert!(select_harnesses("none", &harnesses).0.is_empty());
        let (chosen, unknown) = select_harnesses("claude, pi, pi, vim", &harnesses);
        assert_eq!(chosen, vec![0, 1]);
        assert_eq!(unknown, vec!["vim".to_string()]);
    }
}
