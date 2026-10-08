// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 The Hyperlight Authors.
//! Measuring a second commit alongside the one checked out.
//!
//! A pull request is measured against where it branched from. Reading that
//! baseline from an earlier CI run compares two machines as much as two
//! commits, and the runners differ enough in processor and tenancy to show up
//! as a change. Measuring both here leaves only the commits between them.
//!
//! The baseline is built in a [git worktree], so the checkout in hand is never
//! moved and a run that fails leaves nothing to restore. Each benchmark binary
//! resolves its guests relative to its own source tree, so the baseline finds
//! the guests placed in the worktree and the current tree keeps its own.
//!
//! [git worktree]: https://git-scm.com/docs/git-worktree

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Where the baseline checkout goes, under the build directory so that it is
/// ignored by git and removed by `cargo clean`.
const WORKTREE_DIR: &str = "target/baseline-worktree";

/// The guest binaries a benchmark run loads, as the directory each is
/// published and read from.
const GUEST_DIRS: [(&str, &str); 2] = [
    ("rust", "src/tests/rust_guests/bin/release"),
    ("c", "src/tests/c_guests/bin/release"),
];

/// A checkout of another commit, removed when it goes out of scope.
pub(crate) struct Worktree {
    path: PathBuf,
    commit: String,
}

impl Worktree {
    /// Check `commit` out beside the current tree.
    pub(crate) fn add(commit: &str) -> Result<Self> {
        let path = PathBuf::from(WORKTREE_DIR);

        // A worktree left by an interrupted run holds the path, and one whose
        // directory went with `cargo clean` holds it without being there to
        // find. Pruning clears both rather than leaving them to accumulate.
        let _ = remove(&path);
        let _ = git(["worktree", "prune"]);

        let resolved = commit_of(commit)?;
        git(["worktree", "add", "--detach", WORKTREE_DIR, &resolved])
            .with_context(|| format!("Failed to check out {commit} to compare against"))?;

        Ok(Self {
            path,
            commit: resolved,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The commit it holds, as the revision that named it resolved to.
    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }

    /// Copy the guests of the commit this worktree holds into it.
    ///
    /// `source` holds one directory per guest language, named as
    /// [`GUEST_DIRS`] names them.
    pub(crate) fn place_guests(&self, source: &Path) -> Result<()> {
        for (name, destination) in GUEST_DIRS {
            let from = source.join(name);
            if !from.is_dir() {
                bail!(
                    "{} holds no `{name}` directory, so the baseline has no {name} guests to run",
                    source.display()
                );
            }
            copy_dir(&from, &self.path.join(destination))?;
        }
        Ok(())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // The run is over either way, so a worktree that will not go reports
        // itself rather than failing what already finished.
        if let Err(error) = remove(&self.path) {
            eprintln!("{error:#}");
        }
    }
}

fn remove(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    git([
        OsStr::new("worktree"),
        OsStr::new("remove"),
        OsStr::new("--force"),
        path.as_os_str(),
    ])
    .with_context(|| format!("Failed to remove the worktree at {}", path.display()))
}

fn git<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    read_git(args).map(|_| ())
}

fn read_git<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .args(args)
        .output()
        .context("Failed to run git")?;

    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The commit a revision names.
///
/// `git checkout` and `git diff` read `A...B` as the commit where the two
/// branched apart. `git worktree add` takes a reference rather than a
/// revision, so the naming is done here and it is handed a commit.
pub(crate) fn commit_of(revision: &str) -> Result<String> {
    if let Some((left, right)) = revision.split_once("...") {
        return read_git(["merge-base", left, right])
            .with_context(|| format!("Failed to find where {left} and {right} branched apart"));
    }

    read_git(["rev-parse", "--verify", &format!("{revision}^{{commit}}")])
        .with_context(|| format!("Failed to resolve {revision}"))
}

/// Replace `destination` with the contents of `source`.
fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)
            .with_context(|| format!("Failed to clear {}", destination.display()))?;
    }
    std::fs::create_dir_all(destination)
        .with_context(|| format!("Failed to create {}", destination.display()))?;

    for entry in
        std::fs::read_dir(source).with_context(|| format!("Failed to read {}", source.display()))?
    {
        let entry = entry.context("Failed to read a directory entry")?;
        let target = destination.join(entry.file_name());

        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copying_replaces_whatever_the_destination_held() {
        let root = std::env::temp_dir().join(format!("hl-baseline-{}", std::process::id()));
        let (source, destination) = (root.join("from"), root.join("to"));

        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("simpleguest"), b"new").unwrap();
        std::fs::write(source.join("nested").join("inner"), b"deep").unwrap();

        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("stale"), b"old").unwrap();

        copy_dir(&source, &destination).unwrap();

        assert_eq!(
            std::fs::read(destination.join("simpleguest")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(destination.join("nested").join("inner")).unwrap(),
            b"deep"
        );
        assert!(
            !destination.join("stale").exists(),
            "guests of another commit must not survive"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    /// `A...B` names where two commits branched apart, which is what a pull
    /// request's validation merge is read with.
    #[test]
    fn a_revision_names_one_commit() {
        let head = commit_of("HEAD").unwrap();
        assert_eq!(head.len(), 40, "{head}");
        assert_eq!(commit_of("HEAD^{commit}").unwrap(), head);

        // A merge of a commit with itself branched apart at that commit.
        assert_eq!(commit_of("HEAD...HEAD").unwrap(), head);
    }

    #[test]
    fn a_revision_that_names_nothing_is_reported() {
        let error = commit_of("not-a-revision").unwrap_err().to_string();
        assert!(error.contains("not-a-revision"), "{error}");
    }

    #[test]
    fn missing_guests_are_reported_against_the_directory_given() {
        let root = std::env::temp_dir().join(format!("hl-empty-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let worktree = Worktree {
            path: root.join("worktree"),
            commit: String::new(),
        };
        let error = worktree.place_guests(&root).unwrap_err().to_string();

        assert!(error.contains("rust"), "{error}");

        // Leaves nothing behind: the worktree was never created.
        std::mem::forget(worktree);
        std::fs::remove_dir_all(root).unwrap();
    }
}
