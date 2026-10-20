//! Exhibit recipes in a project's `.ralphus.toml`: exhibits a reviewer should
//! always see when certain files change, independent of what any agent asks
//! for.
//!
//! ```toml
//! [[exhibit.recipe]]
//! name = "hda-flipbook"
//! kind = "gif"
//! describe = "flipbook frames 1-48 of tests/scenes/smoke.hip"
//! paths = ["otls/**", "*.hda"]
//! command = "hython tools/ralphus_flipbook.py --out $RALPHUS_EXHIBIT_DIR"
//! ```
//!
//! A recipe's `paths` are globs (`*` within a segment, `**` across segments)
//! relative to the directory of the `.ralphus.toml` that declares it, and the
//! recipe only applies to changed files inside that directory. For one
//! changed file, `.ralphus.toml` files are read from the file's own directory
//! up to the repository root; the nearest file wins for a recipe `name`, so a
//! subfolder refines (or overrides) a parent's recipe only for the files it
//! contains.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The kinds a recipe may ask for.
pub const RECIPE_KINDS: &[&str] = &["gif", "video", "screenshot", "output"];

/// One recipe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExhibitRecipe {
    /// Identity of the recipe; a nearer file's recipe replaces a parent's
    /// with the same name.
    pub name: String,
    /// One of [`RECIPE_KINDS`].
    pub kind: String,
    /// What to show.
    pub describe: String,
    /// Globs relative to the declaring file's directory; at least one.
    pub paths: Vec<String>,
    /// How to capture it, handed to the live-action agent as is.
    #[serde(default)]
    pub command: Option<String>,
}

/// A recipe that applies to a change, with the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRecipe {
    pub recipe: ExhibitRecipe,
    pub source: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    exhibit: Option<ExhibitSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExhibitSection {
    #[serde(default)]
    recipe: Vec<ExhibitRecipe>,
}

/// Parse the exhibit recipes out of one `.ralphus.toml`'s text. A file with no
/// `[[exhibit.recipe]]` yields an empty list; other sections are ignored.
///
/// # Errors
/// A message when the TOML is malformed, a recipe has an unknown key, an
/// empty or repeated `name`, an unknown `kind`, an empty `describe`, no
/// `paths` (or an empty one), or an empty `command`.
pub fn parse_exhibit_recipes(text: &str) -> Result<Vec<ExhibitRecipe>, String> {
    let file: ProjectFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let recipes = file.exhibit.map(|e| e.recipe).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for r in &recipes {
        if r.name.trim().is_empty() {
            return Err("an [[exhibit.recipe]] has an empty `name`".to_string());
        }
        if !seen.insert(r.name.clone()) {
            return Err(format!(
                "exhibit recipe \"{}\" appears more than once in this file",
                r.name
            ));
        }
        if !RECIPE_KINDS.contains(&r.kind.trim().to_ascii_lowercase().as_str()) {
            return Err(format!(
                "exhibit recipe \"{}\" has kind \"{}\"; expected one of {}",
                r.name,
                r.kind,
                RECIPE_KINDS.join(", ")
            ));
        }
        if r.describe.trim().is_empty() {
            return Err(format!(
                "exhibit recipe \"{}\" has an empty `describe`",
                r.name
            ));
        }
        if r.paths.is_empty() || r.paths.iter().any(|p| p.trim().is_empty()) {
            return Err(format!(
                "exhibit recipe \"{}\" needs at least one non-empty glob in `paths`",
                r.name
            ));
        }
        if r.command.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(format!(
                "exhibit recipe \"{}\" has an empty `command`",
                r.name
            ));
        }
    }
    Ok(recipes)
}

/// The recipes that apply to `changed` (forward-slash paths relative to
/// `repo_root`), each once, plus a warning per `.ralphus.toml` that could not
/// be read or parsed (it is skipped; the files above it still count).
#[must_use]
pub fn recipes_for_changes(
    repo_root: &Path,
    changed: &[&str],
) -> (Vec<MatchedRecipe>, Vec<String>) {
    let mut cache: BTreeMap<PathBuf, Option<Vec<ExhibitRecipe>>> = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut out: Vec<MatchedRecipe> = Vec::new();
    for path in changed {
        let path = path.trim_start_matches("./");
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            continue;
        }
        let mut taken: Vec<String> = Vec::new();
        // Directories from the file's own up to the root: depth n..=0.
        for depth in (0..segments.len()).rev() {
            let dir_rel = segments[..depth].join("/");
            let dir = if dir_rel.is_empty() {
                repo_root.to_path_buf()
            } else {
                repo_root.join(&dir_rel)
            };
            let file = dir.join(".ralphus.toml");
            let recipes = cache.entry(file.clone()).or_insert_with(|| {
                if !file.is_file() {
                    return None;
                }
                match std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|t| parse_exhibit_recipes(&t))
                {
                    Ok(r) => Some(r),
                    Err(e) => {
                        warnings.push(format!("{}: {e}", file.display()));
                        None
                    }
                }
            });
            let Some(recipes) = recipes else {
                continue;
            };
            let within = segments[depth..].join("/");
            for recipe in recipes.iter() {
                if taken.contains(&recipe.name) {
                    continue;
                }
                // The nearest file decides a name, whether or not it matches.
                taken.push(recipe.name.clone());
                let applies = recipe
                    .paths
                    .iter()
                    .any(|g| crate::project_hints::path_matches(g, &within));
                if applies
                    && !out
                        .iter()
                        .any(|m| m.source == file && m.recipe.name == recipe.name)
                {
                    out.push(MatchedRecipe {
                        recipe: recipe.clone(),
                        source: file.clone(),
                    });
                }
            }
        }
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ralphus-recipes-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ROOT: &str = r#"
[review]
default_resolver_agent = "claude-code"

[[exhibit.recipe]]
name = "board"
kind = "screenshot"
describe = "the review inspector"
paths = ["librarian/**"]

[[exhibit.recipe]]
name = "flipbook"
kind = "gif"
describe = "root flipbook"
paths = ["**/*.hda"]
"#;

    #[test]
    fn recipes_parse_and_validate() {
        assert_eq!(parse_exhibit_recipes(ROOT).unwrap().len(), 2);
        assert!(parse_exhibit_recipes("[review]\nx = 1").unwrap().is_empty());
        for (bad, why) in [
            (
                "[[exhibit.recipe]]\nname = \"\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = [\"a\"]",
                "empty `name`",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"hologram\"\ndescribe = \"d\"\npaths = [\"a\"]",
                "kind \"hologram\"",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \" \"\npaths = [\"a\"]",
                "empty `describe`",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = []",
                "at least one",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = [\"a\"]\ncommand = \"\"",
                "empty `command`",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = [\"a\"]\ncolour = 1",
                "unknown field",
            ),
            (
                "[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = [\"a\"]\n[[exhibit.recipe]]\nname = \"a\"\nkind = \"gif\"\ndescribe = \"d\"\npaths = [\"a\"]",
                "more than once",
            ),
        ] {
            let err = parse_exhibit_recipes(bad).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn a_subfolder_refines_a_recipe_only_for_its_own_files() {
        let t = Tree::new();
        t.write(".ralphus.toml", ROOT);
        // houdini/ overrides "flipbook" with its own scene and paths
        // relative to itself.
        t.write(
            "houdini/.ralphus.toml",
            "[[exhibit.recipe]]\nname = \"flipbook\"\nkind = \"gif\"\ndescribe = \"houdini flipbook\"\npaths = [\"otls/**\"]\ncommand = \"hython tools/flip.py\"\n",
        );
        let (got, warnings) =
            recipes_for_changes(&t.0, &["houdini/otls/scatter.hda", "maya/rig.hda"]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut names: Vec<(String, String)> = got
            .iter()
            .map(|m| (m.recipe.name.clone(), m.recipe.describe.clone()))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("flipbook".to_string(), "houdini flipbook".to_string()),
                ("flipbook".to_string(), "root flipbook".to_string()),
            ],
            "houdini's file wins inside houdini/, the root one still covers maya/"
        );
        let houdini = got
            .iter()
            .find(|m| m.recipe.describe == "houdini flipbook")
            .unwrap();
        assert_eq!(
            houdini.recipe.command.as_deref(),
            Some("hython tools/flip.py")
        );
    }

    #[test]
    fn a_nearer_recipe_that_does_not_match_still_hides_the_parents() {
        let t = Tree::new();
        t.write(".ralphus.toml", ROOT);
        t.write(
            "houdini/.ralphus.toml",
            "[[exhibit.recipe]]\nname = \"flipbook\"\nkind = \"gif\"\ndescribe = \"houdini flipbook\"\npaths = [\"otls/**\"]\n",
        );
        // houdini/scenes/x.hda matches the root glob, but houdini/ decides
        // "flipbook" for its own files and its glob does not match.
        let (got, _) = recipes_for_changes(&t.0, &["houdini/scenes/x.hda"]);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn unrelated_changes_match_nothing_and_a_broken_file_is_a_warning() {
        let t = Tree::new();
        t.write(".ralphus.toml", ROOT);
        t.write("docs/.ralphus.toml", "[[exhibit.recipe]]\nname = 1\n");
        let (got, warnings) = recipes_for_changes(&t.0, &["docs/readme.md"]);
        assert!(got.is_empty());
        assert_eq!(warnings.len(), 1);
        let (got, _) = recipes_for_changes(&t.0, &["librarian/assets/board.css"]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].recipe.name, "board");
    }
}
