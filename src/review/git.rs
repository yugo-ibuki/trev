//! コミットを直接読む。作業ツリーやリモートには変更を加えない。
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const MAX_OUTPUT: usize = 8_000_000;
const MAX_SOURCE: usize = 1_000_000;

#[derive(Debug, Clone)]
pub(super) struct Repository {
    pub(super) root: PathBuf,
    pub(super) identity: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct RefChoice {
    pub(super) label: String,
    pub(super) revision: String,
    pub(super) kind: &'static str,
}

#[derive(Debug, Clone)]
pub(super) struct Comparison {
    pub(super) base: String,
    pub(super) head: String,
    pub(super) base_key: String,
    pub(super) head_key: String,
    pub(super) merge_base: String,
    pub(super) head_commit: String,
    pub(super) files: Vec<FileDiff>,
}

#[derive(Debug, Clone)]
pub(super) struct FileDiff {
    pub(super) old_path: String,
    pub(super) new_path: String,
    pub(super) status: char,
    pub(super) digest: String,
    pub(super) lines: Vec<DiffLine>,
}

impl FileDiff {
    pub(super) fn path(&self) -> &str {
        if self.status == 'D' {
            &self.old_path
        } else {
            &self.new_path
        }
    }

    pub(super) fn source<'a>(&'a self, comparison: &'a Comparison) -> (&'a str, &'a str) {
        if self.status == 'D' {
            (&comparison.merge_base, &self.old_path)
        } else {
            (&comparison.head_commit, &self.new_path)
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct DiffLine {
    pub(super) kind: LineKind,
    pub(super) text: String,
    pub(super) old_line: Option<u32>,
    pub(super) new_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LineKind {
    Header,
    Context,
    Added,
    Removed,
}

impl Repository {
    pub(super) fn discover(path: &Path) -> Result<Self> {
        let root = git_at(path, &["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(text(&root)?.strip_suffix('\n').unwrap_or(text(&root)?))
            .canonicalize()
            .context("リポジトリの場所を確認できません")?;
        let common = git_at(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let identity = PathBuf::from(text(&common)?.strip_suffix('\n').unwrap_or(text(&common)?))
            .canonicalize()
            .context("共通 Git ディレクトリを確認できません")?;
        Ok(Self { root, identity })
    }

    pub(super) fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        git_at(&self.root, args)
    }

    pub(super) fn resolve(&self, revision: &str) -> Result<String> {
        let revision = format!("{revision}^{{commit}}");
        Ok(
            text(&self.git_bytes(&["rev-parse", "--verify", "--end-of-options", &revision])?)?
                .trim()
                .to_owned(),
        )
    }

    // ブランチ名・HEAD・完全修飾名は同じ保存キーにする。相対指定と直接の SHA は解決済みコミットを使う。
    fn revision_key(&self, revision: &str, commit: &str) -> Result<String> {
        let bytes = self.git_bytes(&[
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--end-of-options",
            revision,
        ])?;
        let name = text(&bytes)?.trim();
        Ok(if name.starts_with("refs/") && !name.contains('\n') {
            name.to_owned()
        } else {
            commit.to_owned()
        })
    }

    pub(super) fn default_base(&self) -> Result<String> {
        let head = self
            .resolve("HEAD")
            .context("HEAD にコミットがありません。先にコミットを作成してください")?;
        let mut candidates = Vec::new();
        if let Ok(bytes) = self.git_bytes(&["symbolic-ref", "refs/remotes/origin/HEAD"]) {
            candidates.push(text(&bytes)?.trim().to_owned());
        }
        candidates.extend(
            [
                "refs/remotes/origin/main",
                "refs/heads/main",
                "refs/heads/master",
            ]
            .map(str::to_owned),
        );
        for candidate in candidates {
            if self.resolve(&candidate).is_ok_and(|commit| commit != head) {
                return Ok(candidate);
            }
        }
        self.resolve("HEAD^").context("比較元を選べません。--base で既存のコミットまたはブランチを指定してください（最初のコミットだけでは比較できません）")?;
        Ok("HEAD^".into())
    }

    pub(super) fn refs(&self) -> Result<Vec<RefChoice>> {
        let mut choices = Vec::new();
        let worktrees = self.git_bytes(&["worktree", "list", "--porcelain", "-z"])?;
        let mut path = String::new();
        let mut commit = String::new();
        let mut branch = String::new();
        for field in worktrees.split(|byte| *byte == 0) {
            let field = text(field)?;
            if let Some(value) = field.strip_prefix("worktree ") {
                path = value.into();
            } else if let Some(value) = field.strip_prefix("HEAD ") {
                commit = value.into();
            } else if let Some(value) = field.strip_prefix("branch ") {
                branch = value.into();
            } else if field.is_empty() && !path.is_empty() {
                if !commit.is_empty() && !commit.bytes().all(|byte| byte == b'0') {
                    let label = if branch.is_empty() {
                        "detached"
                    } else {
                        branch.trim_start_matches("refs/heads/")
                    };
                    choices.push(RefChoice {
                        label: format!("{path} ({label}・コミット済み)"),
                        revision: if branch.is_empty() {
                            commit.clone()
                        } else {
                            branch.clone()
                        },
                        kind: "worktree",
                    });
                }
                path.clear();
                commit.clear();
                branch.clear();
            }
        }
        let refs = self.git_bytes(&[
            "for-each-ref",
            "--format=%(refname)",
            "--sort=-committerdate",
            "refs/heads",
            "refs/remotes/origin",
        ])?;
        for reference in text(&refs)?.lines() {
            let (label, kind) = if let Some(label) = reference.strip_prefix("refs/heads/") {
                (label, "local")
            } else if let Some(label) = reference.strip_prefix("refs/remotes/") {
                if label == "origin/HEAD" {
                    continue;
                }
                (label, "origin")
            } else {
                continue;
            };
            choices.push(RefChoice {
                label: label.into(),
                revision: reference.into(),
                kind,
            });
        }
        Ok(choices)
    }

    pub(super) fn compare(&self, base: &str, head: &str) -> Result<Comparison> {
        let base_commit = self
            .resolve(base)
            .context("比較元をコミットとして解決できません")?;
        let head_commit = self
            .resolve(head)
            .context("比較先をコミットとして解決できません")?;
        let merge_base = text(&self.git_bytes(&["merge-base", &base_commit, &head_commit])?)?
            .trim()
            .to_owned();
        // Desktop の比較方式を再利用する。パスの区切りだけは曖昧な diff ヘッダーではなく NUL 区切りで読む。
        let common = [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--ignore-submodules=none",
            "--submodule=short",
            "--inter-hunk-context=0",
            "-M",
            "--diff-algorithm=myers",
            "--no-indent-heuristic",
        ];
        let mut args = common.to_vec();
        args.extend([
            "--raw",
            "-z",
            "--no-abbrev",
            &merge_base,
            &head_commit,
            "--",
        ]);
        let raw = self.git_bytes(&args)?;
        let entries = raw_entries(&raw)?;
        let mut args = common.to_vec();
        args.extend([
            "--patch",
            "--full-index",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--unified=3",
            &merge_base,
            &head_commit,
            "--",
        ]);
        let patch = self.git_bytes(&args)?;
        let patch = text(&patch)?;
        let mut patches = Vec::new();
        let mut current = String::new();
        for line in patch.split_inclusive('\n') {
            if line.starts_with("diff --git ") && !current.is_empty() {
                patches.push(std::mem::take(&mut current));
            }
            current.push_str(line);
        }
        if !current.is_empty() {
            patches.push(current);
        }
        if patches.len() != entries.len() {
            bail!("Git のファイル一覧と差分が一致しません。差分設定を確認してください");
        }
        let mut files = Vec::new();
        for ((old_path, new_path, status, metadata), patch) in entries.into_iter().zip(patches) {
            let digest = digest(&[
                metadata.as_bytes(),
                old_path.as_bytes(),
                new_path.as_bytes(),
                patch.as_bytes(),
            ]);
            files.push(FileDiff {
                old_path,
                new_path,
                status,
                digest,
                lines: parse_lines(&patch)?,
            });
        }
        Ok(Comparison {
            base: base.into(),
            head: head.into(),
            base_key: self.revision_key(base, &base_commit)?,
            head_key: self.revision_key(head, &head_commit)?,
            merge_base,
            head_commit,
            files,
        })
    }

    pub(super) fn file_text(&self, commit: &str, path: &str) -> Result<String> {
        let commit = self.resolve(commit)?;
        let object = format!("{commit}:{path}");
        let size = self.git_bytes(&["cat-file", "-s", &object])?;
        let size: usize = text(&size)?
            .trim()
            .parse()
            .context("ファイルサイズを確認できません")?;
        if size > MAX_SOURCE {
            bail!("ファイルが大きすぎます（表示上限 1 MB）");
        }
        let bytes = self.git_bytes(&["cat-file", "blob", &object])?;
        if bytes.contains(&0) {
            bail!("バイナリファイルはテキスト表示できません");
        }
        Ok(text(&bytes)?.to_owned())
    }
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("UTF-8 でないファイル名または内容は表示できません")
}

pub(super) fn git_command(path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "core.quotepath=false",
            "-c",
            "color.ui=false",
            "-c",
            "diff.suppressBlankEmpty=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn git_at(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = git_command(path)
        .args(args)
        .spawn()
        .context("git を起動できません")?;
    let stderr = child
        .stderr
        .take()
        .context("git のエラー出力を取得できません")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(16_384).read_to_end(&mut bytes);
        bytes
    });
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .context("git の出力を取得できません")?
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > MAX_OUTPUT {
        let _ = child.kill();
    }
    let status = child.wait().context("git の終了を確認できません")?;
    let stderr = errors.join().unwrap_or_default();
    read.context("git の出力を読めません")?;
    if bytes.len() > MAX_OUTPUT {
        bail!("Git の出力が大きすぎます（上限 8 MB）。比較範囲を狭めてください");
    }
    if !status.success() {
        bail!(
            "git {} が失敗しました: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(bytes)
}

type RawEntry = (String, String, char, String);
fn raw_entries(raw: &[u8]) -> Result<Vec<RawEntry>> {
    let mut fields = raw
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(metadata) = fields.next() {
        let metadata = text(metadata)?;
        let status = metadata
            .split_whitespace()
            .last()
            .and_then(|value| value.chars().next())
            .context("Git の変更種別を読めません")?;
        let old_path = text(fields.next().context("Git のファイル名がありません")?)?.to_owned();
        let new_path = if status == 'R' || status == 'C' {
            text(fields.next().context("Git の移動先がありません")?)?.to_owned()
        } else {
            old_path.clone()
        };
        entries.push((old_path, new_path, status, metadata.into()));
    }
    Ok(entries)
}

fn parse_lines(patch: &str) -> Result<Vec<DiffLine>> {
    let mut lines = Vec::new();
    let (mut old, mut new) = (0, 0);
    let mut in_hunk = false;
    for line in patch.lines() {
        if !in_hunk
            && ["diff --git ", "index ", "--- ", "+++ "]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        {
            continue;
        }
        if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            parts.next();
            old = hunk_start(parts.next())?;
            new = hunk_start(parts.next())?;
            in_hunk = true;
            lines.push(DiffLine {
                kind: LineKind::Header,
                text: line.into(),
                old_line: None,
                new_line: None,
            });
            continue;
        }
        let (kind, body, old_line, new_line) = if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => {
                    let n = new;
                    new += 1;
                    (LineKind::Added, &line[1..], None, Some(n))
                }
                Some(b'-') => {
                    let n = old;
                    old += 1;
                    (LineKind::Removed, &line[1..], Some(n), None)
                }
                Some(b' ') => {
                    let (o, n) = (old, new);
                    old += 1;
                    new += 1;
                    (LineKind::Context, &line[1..], Some(o), Some(n))
                }
                _ => (LineKind::Header, line, None, None),
            }
        } else {
            (LineKind::Header, line, None, None)
        };
        lines.push(DiffLine {
            kind,
            text: body.into(),
            old_line,
            new_line,
        });
    }
    Ok(lines)
}

fn hunk_start(part: Option<&str>) -> Result<u32> {
    let part = part.context("差分の行番号がありません")?;
    part.get(1..)
        .and_then(|value| value.split(',').next())
        .context("差分の行番号を読めません")?
        .parse()
        .context("差分の行番号を読めません")
}

// デスクトップ版の FNV-1a を再利用。コミット ID は含めず、同じ内容を再比較しても既読状態を維持する。
fn digest(parts: &[&[u8]]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "cli-review-git-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            let fixture = Self(path);
            fixture.git(&["init", "-b", "main"]);
            fixture.git(&["config", "user.name", "Review Test"]);
            fixture.git(&["config", "user.email", "review@example.invalid"]);
            fixture.git(&["config", "commit.gpgsign", "false"]);
            fixture
        }
        fn git(&self, args: &[&str]) -> String {
            String::from_utf8(git_at(&self.0, args).unwrap())
                .unwrap()
                .trim()
                .to_owned()
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            std::fs::write(self.0.join(path), bytes).unwrap();
        }
        fn commit(&self) {
            self.git(&["add", "--all"]);
            self.git(&["commit", "-m", "fixture"]);
        }
        fn repo(&self) -> Repository {
            Repository::discover(&self.0).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn display_omits_file_metadata_and_preserves_hunk_markers() {
        let patch = "diff --git a/file b/file\nold mode 100644\nnew mode 100755\nindex abc..def\n--- a/file\n+++ b/file\n@@ -2 +2 @@\n--- content\n+++ content\n";
        let lines = parse_lines(patch).unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].text, "old mode 100644");
        assert_eq!(lines[1].text, "new mode 100755");
        assert_eq!(lines[2].text, "@@ -2 +2 @@");
        assert_eq!(lines[3].kind, LineKind::Removed);
        assert_eq!(lines[3].text, "-- content");
        assert_eq!(lines[3].old_line, Some(2));
        assert_eq!(lines[4].kind, LineKind::Added);
        assert_eq!(lines[4].text, "++ content");
        assert_eq!(lines[4].new_line, Some(2));
    }

    #[test]
    fn paths_hunks_and_committed_sources_are_exact() {
        let f = Fixture::new();
        f.write("元 名前.txt", b"first\nsecond\nthird\n");
        f.write("deleted.txt", b"removed\n");
        f.commit();
        f.git(&["checkout", "-b", "feature"]);
        f.git(&["mv", "元 名前.txt", "新 名前.txt"]);
        f.write("新 名前.txt", b"first\nchanged\nthird\n");
        f.git(&["rm", "deleted.txt"]);
        f.write("space file.txt", b"added\n");
        f.commit();
        // 作業ツリーの未コミット分は比較にもソース表示にも含めない。
        f.write("新 名前.txt", b"uncommitted\n");
        let repo = f.repo();
        let c = repo.compare("main", "feature").unwrap();
        assert_eq!(c.files.len(), 3);
        let renamed = c.files.iter().find(|file| file.status == 'R').unwrap();
        assert_eq!(renamed.old_path, "元 名前.txt");
        assert_eq!(renamed.path(), "新 名前.txt");
        let added = renamed
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Added)
            .unwrap();
        assert_eq!(
            (added.text.as_str(), added.old_line, added.new_line),
            ("changed", None, Some(2))
        );
        let (commit, path) = renamed.source(&c);
        assert_eq!(
            repo.file_text(commit, path).unwrap(),
            "first\nchanged\nthird\n"
        );
        let deleted = c.files.iter().find(|file| file.status == 'D').unwrap();
        let (commit, path) = deleted.source(&c);
        assert_eq!(repo.file_text(commit, path).unwrap(), "removed\n");
        assert!(repo.compare("--help", "HEAD").is_err());
    }

    #[test]
    fn merge_base_and_digests_ignore_unrelated_commits() {
        let f = Fixture::new();
        f.write("text", b"before\n");
        f.write("binary", &[0, 1]);
        f.commit();
        f.git(&["checkout", "-b", "feature"]);
        f.write("text", b"after\n");
        f.write("binary", &[0, 2]);
        f.commit();
        let repo = f.repo();
        let before = repo.compare("main", "HEAD").unwrap();
        f.write("unrelated", b"extra\n");
        f.commit();
        let after = repo.compare("main", "HEAD").unwrap();
        for file in &before.files {
            assert_eq!(
                file.digest,
                after
                    .files
                    .iter()
                    .find(|other| other.path() == file.path())
                    .unwrap()
                    .digest
            );
        }
        f.write("binary", &[0, 3]);
        f.commit();
        let changed = repo.compare("main", "HEAD").unwrap();
        assert_ne!(
            before
                .files
                .iter()
                .find(|file| file.path() == "binary")
                .unwrap()
                .digest,
            changed
                .files
                .iter()
                .find(|file| file.path() == "binary")
                .unwrap()
                .digest
        );
        assert!(repo.file_text("HEAD", "binary").is_err());
        f.git(&["checkout", "main"]);
        f.write("base-only", b"base advanced\n");
        f.commit();
        let diverged = repo.compare("main", "feature").unwrap();
        assert!(!diverged.files.iter().any(|file| file.path() == "base-only"));
    }

    #[test]
    fn worktrees_share_identity_and_select_committed_snapshots() {
        let f = Fixture::new();
        f.write("text", b"first\n");
        f.commit();
        let repo = f.repo();
        assert!(repo.default_base().is_err());
        f.write("text", b"second\n");
        f.commit();
        assert_eq!(repo.default_base().unwrap(), "HEAD^");
        let linked = f.0.join("linked");
        f.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
        assert_eq!(
            repo.identity,
            Repository::discover(&linked).unwrap().identity
        );
        let choices = repo.refs().unwrap();
        let worktrees: Vec<_> = choices
            .iter()
            .filter(|choice| choice.kind == "worktree")
            .collect();
        assert_eq!(worktrees.len(), 2);
        assert!(
            worktrees
                .iter()
                .all(|choice| repo.resolve(&choice.revision).unwrap()
                    == f.git(&["rev-parse", "HEAD"]))
        );
    }

    #[test]
    fn branch_alias_keys_are_stable_and_distinct() {
        let f = Fixture::new();
        f.write("text", b"before\n");
        f.commit();
        f.git(&["checkout", "-b", "feature"]);
        f.write("text", b"after\n");
        f.commit();
        let repo = f.repo();
        let initial = repo.compare("main", "HEAD").unwrap();
        for alias in ["feature", "refs/heads/feature"] {
            let compared = repo.compare("refs/heads/main", alias).unwrap();
            assert_eq!(initial.base_key, compared.base_key);
            assert_eq!(initial.head_key, compared.head_key);
        }
        f.write("other", b"advance\n");
        f.commit();
        assert_eq!(
            initial.head_key,
            repo.compare("main", "HEAD").unwrap().head_key
        );
        f.git(&["branch", "other"]);
        let switched = repo.compare("other", "main").unwrap();
        assert_ne!(initial.base_key, switched.base_key);
        assert_ne!(initial.head_key, switched.head_key);
        let relative = repo.compare("HEAD^", "HEAD").unwrap();
        assert_eq!(relative.base_key, repo.resolve("HEAD^").unwrap());
    }

    #[test]
    fn mode_only_and_large_source_are_explicit() {
        let f = Fixture::new();
        f.write("script", b"#!/bin/sh\n");
        f.commit();
        f.git(&["checkout", "-b", "feature"]);
        f.git(&["update-index", "--chmod=+x", "script"]);
        f.git(&["commit", "-m", "mode"]);
        let repo = f.repo();
        let comparison = repo.compare("main", "HEAD").unwrap();
        assert_eq!(comparison.files.len(), 1);
        assert!(comparison.files[0]
            .lines
            .iter()
            .any(|line| line.text.contains("new mode")));
        f.write("large", &vec![b'a'; MAX_SOURCE + 1]);
        f.commit();
        assert!(repo
            .file_text("HEAD", "large")
            .unwrap_err()
            .to_string()
            .contains("1 MB"));
    }
}
