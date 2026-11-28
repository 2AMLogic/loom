//! Hermetic git helpers shared by the in-crate tests that drive real
//! repositories. [`git_out`] scrubs the `GIT_DIR`-family variables, reads no
//! host git config (global or system), and fixes the author and committer
//! identity, so a test cannot inherit `~/.gitconfig`, `/etc/gitconfig` or a
//! hook's environment (#8170, #11116). A host `init.defaultBranch` is ignored
//! with the rest: pass `-b <branch>` to `git init` when the name matters.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

pub fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        // Hermetic even when run from inside a git hook environment.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_REFLOG_ACTION")
        // No host ~/.gitconfig or /etc/gitconfig (#8170). GIT_CONFIG_NOSYSTEM
        // is the portable belt to GIT_CONFIG_SYSTEM's braces (git >= 2.32).
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .output()
        .expect("git must spawn")
}

pub fn run(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    run(dir, &["add", "-A"]);
    run(dir, &["commit", "-q", "-m", msg]);
    run(dir, &["rev-parse", "HEAD"])
}

pub fn configure(dir: &Path) {
    run(dir, &["config", "user.email", "loom@example.com"]);
    run(dir, &["config", "user.name", "Loom Test"]);
    run(dir, &["config", "commit.gpgsign", "false"]);
}

/// Bare `origin` + an `author` clone that builds the stack + the `repo` clone
/// the pass runs in (which never checked out any of the stack's branches).
pub struct StackFixture {
    pub _tmp: TempDir,
    pub origin: PathBuf,
    pub author: PathBuf,
    pub repo: PathBuf,
}

impl StackFixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let author = tmp.path().join("author");
        let repo = tmp.path().join("repo");
        run(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(&author).unwrap();
        run(&author, &["init", "-q", "--initial-branch=main"]);
        configure(&author);
        commit_file(&author, "base.txt", "base\n", "base");
        run(&author, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&author, &["push", "-q", "origin", "main"]);
        Self {
            _tmp: tmp,
            origin,
            author,
            repo,
        }
    }

    /// Commit `file` on a new branch `name` off `from` in the author clone,
    /// push it, and record it as PR `pr`'s head (`refs/pull/<pr>/head`).
    pub fn branch(&self, name: &str, from: &str, pr: u32, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "-B", name, from]);
        let sha = commit_file(&self.author, file, body, &format!("{name}: {file}"));
        run(&self.author, &["push", "-q", "-f", "origin", &format!("{name}:{name}")]);
        run(
            &self.author,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{name}:refs/pull/{pr}/head"),
            ],
        );
        sha
    }

    /// Move origin's main forward with `file`.
    pub fn advance_main(&self, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "main"]);
        let sha = commit_file(&self.author, file, body, &format!("main: {file}"));
        run(&self.author, &["push", "-q", "origin", "main"]);
        sha
    }

    /// The clone the pass runs in, made after the stack exists.
    pub fn clone_repo(&self) {
        run(
            self.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                self.repo.to_str().unwrap(),
            ],
        );
        configure(&self.repo);
    }

    pub fn origin_rev(&self, rev: &str) -> String {
        run(&self.origin, &["rev-parse", rev])
    }

    pub fn origin_files(&self, rev: &str) -> Vec<String> {
        run(&self.origin, &["ls-tree", "--name-only", rev])
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn origin_count(&self, range: &str) -> usize {
        run(&self.origin, &["rev-list", "--count", range])
            .parse()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_test_support::EnvGuard;
    use serial_test::serial;

    fn host_config(file: &Path) -> EnvGuard {
        EnvGuard::set("GIT_CONFIG_GLOBAL", file)
            .and_set("GIT_CONFIG_SYSTEM", Some(file.as_os_str()))
    }

    #[test]
    #[serial]
    fn host_config_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("host-gitconfig");
        std::fs::write(&file, "[user]\n\tname = Host Leak\n").unwrap();
        let _env = host_config(&file);

        let out = git_out(tmp.path(), &["config", "--get", "user.name"]);

        assert!(!out.status.success(), "host user.name leaked: {:?}", out.stdout);
    }

    #[test]
    #[serial]
    fn host_init_default_branch_is_not_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("host-gitconfig");
        std::fs::write(&file, "[init]\n\tdefaultBranch = host-default\n").unwrap();
        let _env = host_config(&file);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        run(&repo, &["init", "-q"]);

        assert_ne!(run(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/host-default");
    }
}
