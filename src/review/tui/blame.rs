//! Desktop review_blame.rs parser and age wording adapted to immutable terminal snapshots.
//! Existing symbol lookup resolves the same sides but has no line-number/cache API.
use super::*;
use std::sync::Arc;

/// 1 つのコミットの情報
#[derive(Debug, Clone, PartialEq)]
struct BlameCommit {
    /// 40 文字の完全な ID
    pub sha: String,
    pub author: String,
    /// author-time（epoch 秒）
    pub time: i64,
    /// メッセージの 1 行目
    pub summary: String,
}

/// ファイル全体の blame。行番号（1 始まり）からコミットを引く
#[derive(Debug, Clone, Default)]
struct Blame {
    /// 行ごとのコミット（添字は行番号 - 1）。同じコミットの行は 1 つの Arc を共有する
    lines: Vec<Option<Arc<BlameCommit>>>,
}

impl Blame {
    fn line(&self, line_no: u32) -> Option<&BlameCommit> {
        let index = (line_no as usize).checked_sub(1)?;
        self.lines.get(index)?.as_deref()
    }
}

/// 見出しの行（`<sha> <元の行番号> <いまの行番号> [<まとまりの行数>]`）から、コミットといまの行番号を取り出す。
/// 見出しでなければ None
fn parse_header(line: &str) -> Option<(String, u32)> {
    let mut parts = line.split(' ');
    let sha = parts.next()?;
    let is_sha = sha.len() >= 40 && sha.chars().all(|c| c.is_ascii_hexdigit());
    if !is_sha {
        return None;
    }
    // 元のファイルでの行番号は使わない
    parts.next()?;
    let line_no: u32 = parts.next()?.parse().ok()?;
    if line_no == 0 {
        return None;
    }
    Some((sha.to_string(), line_no))
}

/// `git blame --porcelain` の出力を読む。コミットの見出し（author / author-time / summary）は
/// そのコミットが最初に出てくる行にしか付かないので、2 回目以降は前に読んだものを使う
fn parse_porcelain(text: &str) -> Blame {
    // コミットごとの情報。見出しの続きの行（author など）で埋めていく
    let mut commits: HashMap<String, BlameCommit> = HashMap::new();
    // 行番号と、その行のコミット
    let mut line_shas: Vec<(u32, String)> = Vec::new();
    // いま読んでいる見出しのコミットと行番号。行の中身（タブ始まり）を読んだら次の見出しを待つ
    let mut current: Option<(String, u32)> = None;

    for line in text.lines() {
        if line.starts_with('\t') {
            if let Some((sha, line_no)) = current.take() {
                line_shas.push((line_no, sha));
            }
            continue;
        }
        if current.is_none() {
            current = parse_header(line);
            if let Some((sha, _)) = &current {
                commits.entry(sha.clone()).or_insert_with(|| BlameCommit {
                    sha: sha.clone(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                });
            }
            continue;
        }
        let Some((sha, _)) = &current else {
            continue;
        };
        let Some(commit) = commits.get_mut(sha) else {
            continue;
        };
        // `author-mail` などは `author ` では始まらないので取り違えない
        if let Some(author) = line.strip_prefix("author ") {
            commit.author = author.to_string();
        } else if let Some(time) = line.strip_prefix("author-time ") {
            commit.time = time.trim().parse().unwrap_or(0);
        } else if let Some(summary) = line.strip_prefix("summary ") {
            commit.summary = summary.to_string();
        }
    }

    let mut shared: HashMap<String, Arc<BlameCommit>> = HashMap::new();
    for (sha, commit) in commits {
        shared.insert(sha, Arc::new(commit));
    }
    let mut lines: Vec<Option<Arc<BlameCommit>>> = Vec::new();
    for (line_no, sha) in line_shas {
        let Some(commit) = shared.get(&sha) else {
            continue;
        };
        let index = line_no as usize - 1;
        if lines.len() <= index {
            lines.resize(index + 1, None);
        }
        lines[index] = Some(Arc::clone(commit));
    }
    Blame { lines }
}

// ---- 表示する文言 ------------------------------------------------------------------

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// 「たった今 / 5 分前 / 3 時間前 / 5 日前 / 2 週間前 / 3 か月前 / 2 年前」。
/// 未来の時刻（時計のずれ）は「たった今」。月は 30 日、年は 365 日で数える
fn relative_time(now: i64, then: i64) -> String {
    let elapsed = now.saturating_sub(then);
    if elapsed < MINUTE {
        return "たった今".to_string();
    }
    if elapsed < HOUR {
        return format!("{} 分前", elapsed / MINUTE);
    }
    if elapsed < DAY {
        return format!("{} 時間前", elapsed / HOUR);
    }
    if elapsed < WEEK {
        return format!("{} 日前", elapsed / DAY);
    }
    if elapsed < MONTH {
        return format!("{} 週間前", elapsed / WEEK);
    }
    if elapsed < YEAR {
        return format!("{} か月前", elapsed / MONTH);
    }
    format!("{} 年前", elapsed / YEAR)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    commit: String,
    path: String,
}

#[derive(Default)]
pub(super) struct History {
    pub(super) enabled: bool,
    entries: VecDeque<(Key, Option<Blame>)>,
    job: Option<(Key, Job<Blame>)>,
}

fn load(repository: &Repository, key: &Key) -> Result<Blame> {
    let bytes = repository.git_bytes(&[
        "--no-lazy-fetch",
        "blame",
        "--porcelain",
        &key.commit,
        "--",
        &key.path,
    ])?;
    Ok(parse_porcelain(&String::from_utf8_lossy(&bytes)))
}

impl App {
    fn blame_target(&self) -> Option<(Key, u32)> {
        if self.focus != Focus::Diff || self.folder.is_some() {
            return None;
        }
        let (commit, path, line) = match &self.overlay {
            Overlay::None => {
                let file = self.display_file(self.current()?);
                let row = file.lines.get(self.cursor.position?.row)?;
                match row.kind {
                    LineKind::Header => return None,
                    LineKind::Removed => {
                        (&self.comparison.merge_base, &file.old_path, row.old_line?)
                    }
                    LineKind::Added | LineKind::Context => {
                        (&self.comparison.head_commit, &file.new_path, row.new_line?)
                    }
                }
            }
            Overlay::Source(source) => {
                let row = source.cursor.position?.row;
                source.lines.get(row)?;
                (
                    &source.commit,
                    &source.path,
                    u32::try_from(row).ok()?.checked_add(1)?,
                )
            }
            _ => return None,
        };
        Some((
            Key {
                commit: commit.clone(),
                path: path.clone(),
            },
            line,
        ))
    }

    pub(super) fn poll_blame(&mut self) -> bool {
        let mut changed = false;
        if let Some(result) = self
            .history
            .job
            .as_ref()
            .and_then(|(_, job)| job_result(job))
        {
            if let Some((key, _)) = self.history.job.take() {
                self.history.entries.push_back((key, result.ok()));
                while self.history.entries.len() > 6 {
                    self.history.entries.pop_front();
                }
                changed = true;
            }
        }
        changed
    }

    pub(super) fn start_blame(&mut self) {
        if !self.history.enabled || self.history.job.is_some() {
            return;
        }
        let Some((key, _)) = self.blame_target() else {
            return;
        };
        if self
            .history
            .entries
            .iter()
            .any(|(cached, _)| cached == &key)
        {
            return;
        }
        let repository = self.repository_copy();
        let worker_key = key.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load(&repository, &worker_key).map_err(|error| format!("{error:#}")));
        });
        self.history.job = Some((key, rx));
    }

    pub(super) fn blame_status(&self) -> Option<String> {
        if !self.history.enabled {
            return None;
        }
        let (key, line) = self.blame_target()?;
        let Some((_, blame)) = self
            .history
            .entries
            .iter()
            .find(|(cached, _)| cached == &key)
        else {
            return Some("行の履歴: 読み込み中…".into());
        };
        let Some(commit) = blame.as_ref().and_then(|blame| blame.line(line)) else {
            return Some("行の履歴: 取得できません".into());
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Some(format!(
            "行の履歴: {}, {} • {} ({})",
            clean(&commit.author),
            relative_time(now, commit.time),
            clean(&commit.summary),
            &commit.sha[..7.min(commit.sha.len())]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{app, press};
    use super::*;

    #[test]
    fn porcelain_reuses_commit_headers_and_ignores_source_shaped_metadata() {
        let sha = "a".repeat(40);
        let other = "b".repeat(40);
        let blame = parse_porcelain(&format!(
            "{sha} 9 1 2\nauthor 山田 太郎\nauthor-mail <ignored>\nauthor-time 12345\nsummary first change\nfilename file.rs\n\tauthor forged\n{sha} 10 2\n\tsecond\n{other} 2 3 1\nauthor Bob\nauthor-time 23456\nsummary next change\n\tthird\n"
        ));
        assert!(blame.line(0).is_none());
        assert!(blame.line(4).is_none());
        assert_eq!(blame.line(1).unwrap(), blame.line(2).unwrap());
        assert_eq!(blame.line(1).unwrap().author, "山田 太郎");
        assert_eq!(blame.line(1).unwrap().time, 12345);
        assert_eq!(blame.line(3).unwrap().sha, other);
        assert_eq!(blame.line(3).unwrap().summary, "next change");
        assert_eq!(relative_time(3600, 0), "1 時間前");
        assert_eq!(relative_time(0, 10), "たった今");
    }

    #[test]
    fn cursor_targets_old_new_and_source_snapshots_and_skips_non_code() {
        let (_fixture, mut app) = app();
        app.comparison.files[0].old_path = "before.rs".into();
        app.comparison.files[0].new_path = "after.rs".into();
        app.prepare_code_cursor();
        let (old, n) = app.blame_target().unwrap();
        assert_eq!(
            (old.commit.as_str(), old.path.as_str(), n),
            ("basecommit", "before.rs", 12)
        );
        press(&mut app, KeyCode::Char('j'));
        let (new, n) = app.blame_target().unwrap();
        assert_eq!(
            (new.commit.as_str(), new.path.as_str(), n),
            ("headcommit", "after.rs", 12)
        );
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.blame_target(), Some((new, 13)));
        app.comparison.files[0].lines[2].kind = LineKind::Header;
        assert!(app.blame_target().is_none());
        app.overlay = Overlay::Source(Source::new(
            "sourcecommit".into(),
            "source.rs".into(),
            2,
            "a\nb\nc".into(),
        ));
        app.prepare_code_cursor();
        let (source, n) = app.blame_target().unwrap();
        assert_eq!(
            (source.commit.as_str(), source.path.as_str(), n),
            ("sourcecommit", "source.rs", 2)
        );
        app.focus = Focus::Files;
        assert!(app.blame_target().is_none());
        app.focus = Focus::Diff;
        app.overlay = Overlay::Help(0);
        assert!(app.blame_target().is_none());
        app.overlay = Overlay::None;
        app.folder = Some("dir".into());
        assert!(app.blame_target().is_none());
    }

    #[test]
    fn toggle_and_status_never_reuse_another_snapshot_or_line() {
        let (_fixture, mut app) = app();
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('b'));
        assert!(app.history.enabled);
        let (key, _) = app.blame_target().unwrap();
        let sha = "a".repeat(40);
        let blame = parse_porcelain(&format!(
            "{sha} 1 12 1\nauthor Alice\nauthor-time 0\nsummary original\n\told\n"
        ));
        app.history.entries.push_back((key.clone(), Some(blame)));
        assert!(app.blame_status().unwrap().contains("Alice"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.blame_status().as_deref(), Some("行の履歴: 読み込み中…"));
        app.comparison.merge_base = "different".into();
        press(&mut app, KeyCode::Char('k'));
        assert!(!app.blame_status().unwrap().contains("Alice"));
        app.activate_mouse_control(MouseAction::History);
        assert!(!app.history.enabled);
        assert!(app.blame_status().is_none());
        app.activate_mouse_control(MouseAction::History);
        app.focus = Focus::Files;
        press(&mut app, KeyCode::Char('b'));
        assert!(matches!(app.overlay, Overlay::Branches(_)));
    }

    #[test]
    fn one_background_job_and_bounded_cache_keep_failures_without_retry() {
        let (_fixture, mut app) = app();
        app.history.enabled = true;
        app.prepare_code_cursor();
        let (key, _) = app.blame_target().unwrap();
        let (tx, rx) = mpsc::channel();
        app.history.job = Some((key.clone(), rx));
        press(&mut app, KeyCode::Char('j'));
        app.start_blame();
        assert_eq!(app.history.job.as_ref().unwrap().0, key);
        for n in 0..6 {
            app.history.entries.push_back((
                Key {
                    commit: n.to_string(),
                    path: "x".into(),
                },
                None,
            ));
        }
        tx.send(Err("failure".into())).unwrap();
        assert!(app.poll_blame());
        assert_eq!(app.history.entries.len(), 6);
        press(&mut app, KeyCode::Char('k'));
        app.start_blame();
        assert!(app.history.job.is_none());
        assert_eq!(
            app.blame_status().as_deref(),
            Some("行の履歴: 取得できません")
        );
    }

    #[test]
    fn real_git_attributes_immutable_old_and_renamed_head_with_dirty_worktree() {
        let (_fixture, app) = app();
        let repo = &app.repository;
        let git = |args: &[&str]| {
            String::from_utf8(repo.git_bytes(args).unwrap())
                .unwrap()
                .trim()
                .to_owned()
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Alice"]);
        git(&["config", "user.email", "alice@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        let old_path = "-old name.rs";
        let new_path = "new name.rs";
        std::fs::write(repo.root.join(old_path), "unchanged\nold value\n").unwrap();
        git(&["add", "--", old_path]);
        git(&["commit", "-m", "initial lines"]);
        let base = repo.resolve("HEAD").unwrap();
        git(&["mv", "--", old_path, new_path]);
        std::fs::write(repo.root.join(new_path), "unchanged\nnew value\n").unwrap();
        git(&["config", "user.name", "Bob"]);
        git(&["add", "--", new_path]);
        git(&["commit", "-m", "replace second line"]);
        let head = repo.resolve("HEAD").unwrap();
        std::fs::write(repo.root.join(new_path), "dirty worktree\n").unwrap();
        let old = load(
            repo,
            &Key {
                commit: base.clone(),
                path: old_path.into(),
            },
        )
        .unwrap();
        let new = load(
            repo,
            &Key {
                commit: head.clone(),
                path: new_path.into(),
            },
        )
        .unwrap();
        assert_eq!(old.line(2).unwrap().sha, base);
        assert_eq!(old.line(2).unwrap().author, "Alice");
        assert_eq!(new.line(1).unwrap().author, "Alice");
        assert_eq!(new.line(2).unwrap().sha, head);
        assert_eq!(new.line(2).unwrap().author, "Bob");
        assert_eq!(new.line(2).unwrap().summary, "replace second line");
    }
}
