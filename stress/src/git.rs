//! Identify the commit under test: full hash, tag (if one points at it),
//! commit date, and whether the working tree is dirty.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Serialize, Clone, Debug, Default)]
pub struct CommitInfo {
    pub commit: String,
    pub short: String,
    pub tags: Vec<String>,
    pub commit_date: String,
    pub subject: String,
    /// Only meaningful when the commit under test is the checked-out HEAD.
    pub dirty: bool,
}

impl CommitInfo {
    /// The tag when one exists (the first, sorted), otherwise the short hash.
    pub fn label(&self) -> String {
        self.tags
            .first()
            .cloned()
            .unwrap_or_else(|| self.short.clone())
    }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn describe(repo: &Path, rev: &str) -> Result<CommitInfo, String> {
    let commit = git(
        repo,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
    .ok_or_else(|| format!("cannot resolve {rev:?} in {}", repo.display()))?;
    let mut tags: Vec<String> = git(repo, &["tag", "--points-at", &commit])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .filter(|t| !t.is_empty())
        .collect();
    tags.sort();
    let head = git(repo, &["rev-parse", "HEAD"]).unwrap_or_default();
    let dirty = head == commit
        && git(repo, &["status", "--porcelain", "--untracked-files=no"])
            .map(|s| !s.is_empty())
            .unwrap_or(false);
    Ok(CommitInfo {
        short: commit.chars().take(12).collect(),
        commit_date: git(repo, &["log", "-1", "--format=%cI", &commit]).unwrap_or_default(),
        subject: git(repo, &["log", "-1", "--format=%s", &commit]).unwrap_or_default(),
        commit,
        tags,
        dirty,
    })
}
