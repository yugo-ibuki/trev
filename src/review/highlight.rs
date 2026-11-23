//! 文法による色付け。端末の ANSI パレットを使い、通常の文字色と行の地色は上書きしない。
use std::collections::BTreeMap;
use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use two_face::re_exports::syntect::{
    easy::HighlightLines,
    highlighting::{Color as SyntaxColor, StyleModifier, Theme, ThemeItem, ThemeSettings},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use super::git::{FileDiff, LineKind, Repository};

pub(super) type Spans = Vec<Span<'static>>;

const MAX_SOURCE: usize = 1_000_000;
const MAX_LINE: usize = 16_384;

fn extension(path: &str) -> Option<&'static str> {
    let filename = path.rsplit('/').next()?;
    let (_, extension) = filename.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => Some("ts"),
        "tsx" => Some("tsx"),
        "dart" => Some("dart"),
        "rs" => Some("rs"),
        "py" | "pyw" | "pyi" => Some("py"),
        _ => None,
    }
}

/// UI スレッドでも呼べる軽い判定。文法集はここでは読み込まない。
pub(super) fn supports(path: &str) -> bool {
    extension(path).is_some()
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    // Desktop の code_highlight と同じ文法集・拡張子別名を使う。
    // egui の LayoutJob は端末に描けないため、出力は ratatui の Span で作る。
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

// Syntect 内でだけ使う分類 ID。実際に描く色は terminal_style の ANSI 色。
fn marker(id: u8) -> SyntaxColor {
    SyntaxColor {
        r: id,
        g: 0,
        b: 0,
        a: 255,
    }
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| Theme {
        settings: ThemeSettings {
            foreground: Some(marker(0)),
            ..Default::default()
        },
        scopes: [
            ("keyword, storage", 1),
            ("string", 2),
            ("constant.numeric, constant.language", 3),
            ("entity.name.type, support.type, support.class", 4),
            ("entity.name.function, support.function", 5),
            ("comment", 6),
        ]
        .into_iter()
        .map(|(scope, id)| ThemeItem {
            scope: scope.parse().expect("固定の構文セレクタ"),
            style: StyleModifier {
                foreground: Some(marker(id)),
                ..Default::default()
            },
        })
        .collect(),
        ..Default::default()
    })
}

fn terminal_style(color: SyntaxColor) -> Style {
    let color = match color.r {
        1 => Color::Magenta,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Cyan,
        6 => Color::DarkGray,
        _ => return Style::default(),
    };
    Style::default().fg(color)
}

/// 全文を先頭から解析し、必要な行だけ渡す。途中で失敗したら以後は無色で残す。
fn visit(path: &str, text: &str, mut emit: impl FnMut(usize, Option<Spans>)) -> Option<()> {
    if text.len() > MAX_SOURCE || text.lines().any(|line| line.len() > MAX_LINE) {
        return None;
    }
    let syntax = syntaxes().find_syntax_by_extension(extension(path)?)?;
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut failed = false;
    for (index, line) in LinesWithEndings::from(text).enumerate() {
        let visible = line.strip_suffix('\n').unwrap_or(line);
        let visible = if line.ends_with('\n') {
            visible.strip_suffix('\r').unwrap_or(visible)
        } else {
            visible
        };
        let spans = if failed {
            None
        } else {
            match highlighter.highlight_line(line, syntaxes()) {
                Ok(parts) => {
                    let mut offset = 0;
                    Some(
                        parts
                            .into_iter()
                            .filter_map(|(style, part)| {
                                let start = offset;
                                offset += part.len();
                                let end = offset.min(visible.len());
                                (start < end).then(|| {
                                    Span::styled(
                                        line[start..end].to_owned(),
                                        terminal_style(style.foreground),
                                    )
                                })
                            })
                            .collect(),
                    )
                }
                Err(_) => {
                    failed = true;
                    None
                }
            }
        };
        emit(index, spans);
    }
    Some(())
}

pub(super) fn source(path: &str, text: &str) -> Option<Vec<Spans>> {
    if !supports(path) {
        return None;
    }
    let lines: Vec<_> = text.lines().collect();
    let mut output = Vec::with_capacity(lines.len());
    visit(path, text, |index, spans| {
        output.push(spans.unwrap_or_else(|| vec![Span::raw(lines[index].to_owned())]));
    })?;
    Some(output)
}

pub(super) fn diff(
    repository: &Repository,
    merge_base: &str,
    head_commit: &str,
    file: &FileDiff,
) -> Vec<Option<Spans>> {
    let mut output = vec![None; file.lines.len()];
    for (removed, commit, path) in [
        (true, merge_base, file.old_path.as_str()),
        (false, head_commit, file.new_path.as_str()),
    ] {
        if !supports(path) {
            continue;
        }
        let mut needed: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, line) in file.lines.iter().enumerate() {
            let number = match (removed, line.kind) {
                (true, LineKind::Removed) => line.old_line,
                (false, LineKind::Added | LineKind::Context) => line.new_line,
                _ => None,
            };
            if let Some(number) = number.and_then(|n| n.checked_sub(1)) {
                needed.entry(number as usize).or_default().push(index);
            }
        }
        if needed.is_empty() {
            continue;
        }
        let Ok(text) = repository.file_text(commit, path) else {
            continue;
        };
        let _ = visit(path, &text, |number, spans| {
            if let Some(indices) = needed.get(&number) {
                for &index in indices {
                    // 色を付ける文字が差分本文と一致する場合だけ採用する。
                    if let Some(spans) = &spans {
                        let body: String = spans.iter().map(|span| span.content.as_ref()).collect();
                        if body == file.lines[index].text {
                            output[index] = Some(spans.clone());
                        }
                    }
                }
            }
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(spans: &Spans) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn real_grammars_color_keywords_strings_and_comments() {
        for (path, text) in [
            ("a.ts", "const name: string = \"猫\"; // comment"),
            ("a.dart", "final name = \"猫\"; // comment"),
            ("a.rs", "let name = \"猫\"; // comment"),
            ("a.py", "return \"猫\" # comment"),
        ] {
            let result = source(path, text).unwrap();
            assert_eq!(body(&result[0]), text);
            for color in [Color::Magenta, Color::Green, Color::DarkGray] {
                assert!(
                    result[0].iter().any(|span| span.style.fg == Some(color)),
                    "{path}: {color:?} {result:?}"
                );
            }
            assert!(result[0].iter().all(|span| span.style.bg.is_none()));
        }
    }

    #[test]
    fn aliases_and_case_preserve_typescript_and_python_grammars() {
        for path in [
            "a.MTS", "a.CTS", "a.TS", "a.TSX", "a.DART", "a.RS", "a.PYW", "a.PYI",
        ] {
            assert!(supports(path));
            assert!(source(path, "name = 1").is_some(), "{path}");
        }
        for path in ["a.txt", "folder.rs/file", "a"] {
            assert!(!supports(path));
        }
    }

    #[test]
    fn multiline_state_and_raw_utf8_control_text_are_preserved() {
        let text = "/* start\r\n猫\t\u{1b}[31m\r\n*/\nlet x = 1;\n";
        let result = source("a.rs", text).unwrap();
        assert_eq!(result.len(), text.lines().count());
        for (spans, line) in result.iter().zip(text.lines()) {
            assert_eq!(body(spans), line);
        }
        assert!(result[1]
            .iter()
            .all(|span| span.style.fg == Some(Color::DarkGray)));
        assert!(result[3]
            .iter()
            .any(|span| span.style.fg == Some(Color::Magenta)));
        assert_eq!(source("a.txt", text), None);
        assert_eq!(source("a.rs", &"x".repeat(MAX_LINE + 1)), None);
        assert_eq!(source("a.rs", &"x\n".repeat(MAX_SOURCE)), None);
    }

    #[test]
    fn diff_reads_both_snapshots_before_hunks_and_uses_each_filename() {
        use super::super::git::DiffLine;
        use std::{fs, process::Command};
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("cli-highlight-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-b", "main"]);
        fs::write(
            root.join("old.rs"),
            "/* outside hunk\n2\n3\n4\n5\ninside old\nshared context\n*/\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "old"]);
        let old = git(&["rev-parse", "HEAD"]);
        fs::remove_file(root.join("old.rs")).unwrap();
        fs::write(
            root.join("new.py"),
            "value = \"\"\"outside hunk\n2\n3\n4\n5\ninside new\nshared context\n\"\"\"\nreturn \"猫\"\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", "new"]);
        let head = git(&["rev-parse", "HEAD"]);
        let repo = Repository::discover(&root).unwrap();
        let line = |kind, text: &str, old_line, new_line| DiffLine {
            kind,
            text: text.into(),
            old_line,
            new_line,
        };
        let mut file = FileDiff {
            old_path: "old.rs".into(),
            new_path: "new.py".into(),
            status: 'R',
            digest: String::new(),
            lines: vec![
                line(LineKind::Header, "@@ -6,2 +6,3 @@", None, None),
                line(LineKind::Removed, "inside old", Some(6), None),
                line(LineKind::Added, "inside new", None, Some(6)),
                line(LineKind::Context, "shared context", Some(7), Some(7)),
                line(LineKind::Added, "return \"猫\"", None, Some(9)),
            ],
        };
        let result = diff(&repo, &old, &head, &file);
        assert!(result[0].is_none());
        assert!(result[1]
            .as_ref()
            .unwrap()
            .iter()
            .all(|span| span.style.fg == Some(Color::DarkGray)));
        assert!(result[2]
            .as_ref()
            .unwrap()
            .iter()
            .all(|span| span.style.fg == Some(Color::Green)));
        assert!(result[3]
            .as_ref()
            .unwrap()
            .iter()
            .all(|span| span.style.fg == Some(Color::Green)));
        assert!(result[4]
            .as_ref()
            .unwrap()
            .iter()
            .any(|span| span.style.fg == Some(Color::Magenta)));
        for (spans, line) in result.iter().zip(&file.lines) {
            if let Some(spans) = spans {
                assert_eq!(body(spans), line.text);
            }
        }
        // 削除後のパスは取得不能でも、削除行は比較元から色を付ける。
        file.status = 'D';
        file.new_path = "missing.rs".into();
        let result = diff(&repo, &old, &head, &file);
        assert!(result[1].is_some());
        assert!(result[2..].iter().all(Option::is_none));
        file.old_path = "missing.rs".into();
        assert!(diff(&repo, &old, &head, &file).iter().all(Option::is_none));
    }
}
