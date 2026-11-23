//! 比較先コミットの全ファイルを読み取り専用で探す。
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};

use anyhow::{bail, Context, Result};

use super::git::{git_command, Repository};

const MAX_PER_FILE: usize = 20;
const MAX_HITS: usize = 500;
const MAX_BYTES: u64 = 8_000_000;

#[derive(Debug, Clone)]
pub(super) struct ContentHit {
    pub(super) path: String,
    pub(super) line: u32,
    pub(super) text: String,
}

pub(super) struct ContentResults {
    pub(super) hits: Vec<ContentHit>,
    pub(super) truncated: bool,
}

pub(super) fn list_paths(repository: &Repository, commit: &str) -> Result<Vec<String>> {
    let commit = repository.resolve(commit)?;
    // デスクトップ版の改行区切りでは改行やタブを含む名前を保持できないため NUL 区切りで読む。
    let bytes = repository.git_bytes(&["ls-tree", "-rz", "--full-tree", &commit])?;
    let mut paths = Vec::new();
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .context("Git のファイル一覧を読めません")?;
        let metadata = std::str::from_utf8(&entry[..tab])?;
        if metadata.split_whitespace().nth(1) != Some("blob") {
            continue;
        }
        paths.push(
            std::str::from_utf8(&entry[tab + 1..])
                .context("UTF-8 でないファイル名は表示できません")?
                .to_owned(),
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(super) fn search_content(
    repository: &Repository,
    commit: &str,
    query: &str,
) -> Result<ContentResults> {
    let commit = repository.resolve(commit)?;
    if query.contains(['\n', '\0']) {
        bail!("検索語に改行や NUL 文字は使えません");
    }
    if query.is_empty() {
        return Ok(ContentResults {
            hits: Vec::new(),
            truncated: false,
        });
    }
    grep_bounded(
        repository,
        &commit,
        query,
        &["-F", "-i"],
        &[".".into()],
        true,
        |_, _| true,
    )
}

/// 既存の全ファイル検索とシンボル検索で出力・件数・バイト上限を共有する。
pub(super) fn grep_bounded(
    repository: &Repository,
    commit: &str,
    pattern: &str,
    flags: &[&str],
    paths: &[String],
    raw_limit: bool,
    mut keep: impl FnMut(&str, &str) -> bool,
) -> Result<ContentResults> {
    let max = (MAX_PER_FILE + 1).to_string();
    // 既存 git_bytes は出力上限を超えると全結果がエラーになる。
    // grep だけは順に読み、上限までの検索結果を残して子プロセスを回収する。
    let mut child = git_command(&repository.root)
        .arg("grep")
        .args(flags)
        .args(if raw_limit { vec!["-m", &max] } else { vec![] })
        .args([
            "-z",
            "-n",
            "-I",
            "--no-color",
            "--no-heading",
            "--no-column",
            "--no-break",
            "--no-textconv",
            "--full-name",
            "-e",
            pattern,
            commit,
            "--",
        ])
        .args(paths)
        .args([
            ":(exclude,glob)**/*.lock",
            ":(exclude,glob)**/package-lock.json",
            ":(exclude,glob)**/*.min.js",
            ":(exclude,glob)**/*.snap",
            ":(exclude,glob)**/*.map",
        ])
        .spawn()
        .context("git を起動できません")?;
    let stderr = child
        .stderr
        .take()
        .context("git のエラー出力を取得できません")?;
    let errors = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut saved = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let keep = count.min(16_384usize.saturating_sub(saved.len()));
            saved.extend_from_slice(&buffer[..keep]);
        }
        saved
    });
    let stdout = child.stdout.take().context("git の出力を取得できません")?;
    let mut reader = BufReader::new(stdout.take(MAX_BYTES));
    let result = read_hits(&mut reader, commit, &mut keep);
    let capped = reader.get_ref().limit() == 0;
    let stop = result.as_ref().map_or(true, |results| {
        results.hits.len() == MAX_HITS && results.truncated
    }) || capped;
    if stop {
        let _ = child.kill();
    }
    drop(reader);
    let status = child.wait().context("git の終了を確認できません");
    let stderr = errors.join().unwrap_or_default();
    let mut results = result?;
    let status = status?;
    results.truncated |= capped;
    if !stop && !status.success() && status.code() != Some(1) {
        bail!(
            "git grep が失敗しました: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(results)
}

fn read_hits(
    reader: &mut impl BufRead,
    commit: &str,
    keep: &mut impl FnMut(&str, &str) -> bool,
) -> Result<ContentResults> {
    let mut results = ContentResults {
        hits: Vec::new(),
        truncated: false,
    };
    let mut counts = HashMap::new();
    let prefix = format!("{commit}:");
    loop {
        let mut path = Vec::new();
        if reader.read_until(0, &mut path)? == 0 {
            break;
        }
        if path.pop() != Some(0) {
            results.truncated = true;
            break;
        }
        let path = std::str::from_utf8(&path)
            .context("UTF-8 でないファイル名は表示できません")?
            .strip_prefix(&prefix)
            .context("Git の検索ファイル名を読めません")?
            .to_owned();
        let mut line = Vec::new();
        reader.read_until(0, &mut line)?;
        if line.pop() != Some(0) {
            results.truncated = true;
            break;
        }
        let line = std::str::from_utf8(&line)?
            .parse()
            .context("Git の検索行番号を読めません")?;
        let mut text = Vec::new();
        reader.read_until(b'\n', &mut text)?;
        if text.last() == Some(&b'\n') {
            text.pop();
        }
        let text = match std::str::from_utf8(&text) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                results.truncated = true;
                std::str::from_utf8(&text[..error.valid_up_to()])?
            }
            Err(error) => return Err(error).context("UTF-8 でない検索結果は表示できません"),
        };
        if !keep(&path, text) {
            continue;
        }
        let count = counts.entry(path.clone()).or_insert(0);
        if *count == MAX_PER_FILE {
            results.truncated = true;
            continue;
        }
        if results.hits.len() == MAX_HITS {
            results.truncated = true;
            break;
        }
        *count += 1;
        let end = text
            .char_indices()
            .nth(300)
            .map_or(text.len(), |(index, _)| index);
        results.truncated |= end < text.len();
        results.hits.push(ContentHit {
            path,
            line,
            text: text[..end].to_owned(),
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cli-all-files-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            let fixture = Self(path);
            fixture.git(&["init", "-b", "main"]);
            fixture.git(&["config", "user.name", "Test"]);
            fixture.git(&["config", "user.email", "test@example.invalid"]);
            fixture.git(&["config", "commit.gpgsign", "false"]);
            fixture
        }
        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }
        fn write(&self, path: &str, contents: &[u8]) {
            let target = self.0.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, contents).unwrap();
        }
        fn commit(&self) -> String {
            self.git(&["add", "--all"]);
            self.git(&["commit", "-m", "fixture"]);
            self.git(&["rev-parse", "HEAD"])
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
    fn searches_unchanged_files_and_selected_snapshot_with_literal_case_insensitive_query() {
        let fixture = Fixture::new();
        fixture.write("unchanged.txt", b"Keep -Needle( here\n");
        fixture.write("changed.txt", b"old -Needle(\n");
        let old = fixture.commit();
        fixture.write("changed.txt", b"new value\n");
        let head = fixture.commit();
        fixture.write("unchanged.txt", b"uncommitted value\n");
        fixture.write("untracked.txt", b"-needle(\n");
        let repository = fixture.repo();
        let hits = search_content(&repository, &head, "-needle(").unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].path, "unchanged.txt");
        assert_eq!(hits.hits[0].line, 1);
        assert_eq!(hits.hits[0].text, "Keep -Needle( here");
        assert_eq!(
            search_content(&repository, &old, "-needle(")
                .unwrap()
                .hits
                .len(),
            2
        );
        assert_eq!(
            list_paths(&repository, &head).unwrap(),
            ["changed.txt", "unchanged.txt"]
        );
        assert!(search_content(&repository, &head, "absent")
            .unwrap()
            .hits
            .is_empty());
        assert!(search_content(&repository, "not-a-commit", "value").is_err());
        assert!(list_paths(&repository, "not-a-commit").is_err());
        assert!(search_content(&repository, &head, "\0").is_err());
        assert!(search_content(&repository, &head, "a\nb").is_err());
        assert!(search_content(&repository, &head, "")
            .unwrap()
            .hits
            .is_empty());
        fixture.git(&["config", "grep.column", "true"]);
        fixture.git(&["config", "grep.lineNumber", "false"]);
        assert_eq!(
            search_content(&repository, &head, "-needle(").unwrap().hits[0].line,
            1
        );
    }

    #[test]
    fn git_grep_failure_is_not_treated_as_an_empty_result() {
        let fixture = Fixture::new();
        fixture.write("a.txt", b"needle\n");
        let commit = fixture.commit();
        fixture.git(&["config", "grep.threads", "-1"]);
        let error = search_content(&fixture.repo(), &commit, "needle")
            .err()
            .unwrap();
        assert!(error.to_string().contains("git grep が失敗しました"));
    }

    #[test]
    fn preserves_nul_framed_filenames_and_filters_generated_and_binary_files() {
        let fixture = Fixture::new();
        let names = [
            " space : name.txt",
            "tab\tname.txt",
            "line\nname.txt",
            "-leading.txt",
        ];
        for name in names {
            fixture.write(name, b"needle: value\n");
        }
        for name in [
            "a.lock",
            "package-lock.json",
            "a.min.js",
            "a.snap",
            "a.map",
            "sub/a.lock",
            "sub/package-lock.json",
        ] {
            fixture.write(name, b"needle\n");
        }
        fixture.write("binary", b"needle\0binary");
        let commit = fixture.commit();
        let repository = fixture.repo();
        let paths = list_paths(&repository, &commit).unwrap();
        let hits = search_content(&repository, &commit, "needle").unwrap();
        assert_eq!(hits.hits.len(), names.len());
        for name in names {
            assert!(paths.iter().any(|path| path == name));
            assert!(hits.hits.iter().any(|hit| hit.path == name));
            assert_eq!(
                repository.file_text(&commit, name).unwrap(),
                "needle: value\n"
            );
        }
    }

    #[test]
    fn caps_each_file_and_total_results_without_losing_broad_matches() {
        let fixture = Fixture::new();
        fixture.write("many.txt", "needle\n".repeat(30).as_bytes());
        let commit = fixture.commit();
        let results = search_content(&fixture.repo(), &commit, "needle").unwrap();
        assert_eq!(results.hits.len(), MAX_PER_FILE);
        assert!(results.truncated);
        for index in 0..30 {
            fixture.write(&format!("{index:02}.txt"), "needle\n".repeat(20).as_bytes());
        }
        let commit = fixture.commit();
        let results = search_content(&fixture.repo(), &commit, "needle").unwrap();
        assert_eq!(results.hits.len(), MAX_HITS);
        assert!(results.truncated);
        assert!(results.hits.iter().all(|hit| hit.line <= 20));
    }

    #[test]
    fn bounds_snippets_by_unicode_characters() {
        let fixture = Fixture::new();
        fixture.write("a.txt", format!("needle{}\n", "あ".repeat(400)).as_bytes());
        let commit = fixture.commit();
        let results = search_content(&fixture.repo(), &commit, "needle").unwrap();
        assert_eq!(results.hits[0].text.chars().count(), 300);
        assert_eq!(results.hits[0].line, 1);
        assert!(results.truncated);
    }

    #[test]
    fn keeps_early_results_when_stdout_exceeds_byte_budget() {
        let fixture = Fixture::new();
        fixture.write("a.txt", b"needle\n");
        let long = format!("needle{}\n", "x".repeat(MAX_BYTES as usize + 10));
        fixture.write("z.txt", long.as_bytes());
        let commit = fixture.commit();
        let results = search_content(&fixture.repo(), &commit, "needle").unwrap();
        assert!(results.truncated);
        assert_eq!(results.hits[0].path, "a.txt");
    }
}
