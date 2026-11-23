//! LSP を使わず、コミット内の宣言・使用候補を探す。
//! 同名の別シンボル、複数行の構文、動的参照は完全には区別できない。
use std::collections::HashSet;

use anyhow::{bail, Result};

use super::all_files::{self, ContentResults};
use super::git::Repository;
use super::references;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SymbolMode {
    Definitions,
    Usages,
}

pub(super) fn find(
    repository: &Repository,
    commit: &str,
    from_path: &str,
    symbol: &str,
    mode: SymbolMode,
) -> Result<ContentResults> {
    if !valid_identifier(symbol) {
        bail!("検索する識別子が不正です（文字・数字・_・$ を使用できます）");
    }
    let commit = repository.resolve(commit)?;
    // ReferenceIndex::load は全ソースを読むため使わず、JS/TS の相対 import がある場合だけ解決する。
    // 大きい・削除済みファイルで import 文を取得できなくても、候補の検索は継続する。
    let binding = if is_js(from_path) {
        repository
            .file_text(&commit, from_path)
            .ok()
            .filter(|text| text.contains("import"))
            .and_then(|text| {
                let files: HashSet<_> = all_files::list_paths(repository, &commit)
                    .ok()?
                    .into_iter()
                    .collect();
                references::symbol_import(from_path, &text, symbol, &files)
            })
    } else {
        None
    }
    .filter(|binding| valid_identifier(&binding.0));
    let name = binding.as_ref().map_or(symbol, |b| b.0.as_str());
    let target = binding.as_ref().map(|b| b.1.as_str());
    let default_import = binding.as_ref().is_some_and(|b| b.2);
    let paths = language_paths(from_path);
    let mut names = vec![name];
    if mode == SymbolMode::Usages && name != symbol {
        names.push(symbol);
    }
    let names_pattern = names
        .iter()
        .map(|n| n.replace('$', "\\$"))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!("(^|[^[:alnum:]_$])({names_pattern})([^[:alnum:]_$]|$)");
    let keep = |path: &str, text: &str| {
        let code = code_only(text, path);
        names.iter().any(|name| {
            occurrences(&code, name).any(|offset| {
                let declaration = is_definition(path, &code, name, offset);
                match mode {
                    SymbolMode::Definitions => declaration,
                    SymbolMode::Usages => !declaration && !is_import(&code),
                }
            })
        })
    };
    // パス順で先に出る同名の定義が上限を埋めても、import 先・現在ファイルは残す。
    let mut result = ContentResults {
        hits: Vec::new(),
        truncated: false,
    };
    if mode == SymbolMode::Definitions {
        for path in [target, Some(from_path)].into_iter().flatten() {
            let scoped = all_files::grep_bounded(
                repository,
                &commit,
                &pattern,
                &["-E"],
                &[format!(":(literal){path}")],
                false,
                &keep,
            )?;
            result.truncated |= scoped.truncated;
            result.hits.extend(scoped.hits);
        }
    }
    let broad =
        all_files::grep_bounded(repository, &commit, &pattern, &["-E"], &paths, false, keep)?;
    result.truncated |= broad.truncated;
    result.hits.extend(broad.hits);
    // default import のローカル名と export 元の名前は一致しない場合がある。
    // 対象1ファイルだけに限定した bounded grep を追加し、無名 default も候補に含める。
    if default_import && mode == SymbolMode::Definitions {
        if let Some(target) = target {
            let defaults = all_files::grep_bounded(
                repository,
                &commit,
                "export[[:space:]]+default",
                &["-E"],
                &[format!(":(literal){target}")],
                false,
                |path, text| code_only(text, path).contains("export default"),
            )?;
            result.truncated |= defaults.truncated;
            result.hits.extend(defaults.hits);
        }
    }
    result.hits.sort_by(|a, b| {
        let rank = |path: &str| {
            if Some(path) == target {
                0
            } else if path == from_path {
                1
            } else if parent(path) == parent(from_path) {
                2
            } else {
                3
            }
        };
        (rank(&a.path), &a.path, a.line).cmp(&(rank(&b.path), &b.path, b.line))
    });
    result
        .hits
        .dedup_by(|a, b| a.path == b.path && a.line == b.line);
    if result.hits.len() > 500 {
        result.hits.truncate(500);
        result.truncated = true;
    }
    Ok(result)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}
fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || matches!(c as u32, 0x300..=0x36f)
}
pub(super) fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(ident_char)
}
/// Picker 用。検索と同じ字句規則を使い、元テキストの UTF-8 バイト範囲を返す。
pub(super) fn pickable_identifiers(text: &str, path: &str) -> Vec<std::ops::Range<usize>> {
    let code = code_only(text, path);
    let mut ranges = Vec::new();
    let mut start = None;
    for (offset, ch) in code
        .char_indices()
        .chain(std::iter::once((code.len(), ' ')))
    {
        if ident_char(ch) {
            start.get_or_insert(offset);
        } else if let Some(begin) = start.take() {
            let name = &code[begin..offset];
            // Keywords can also be valid property names, such as map.delete().
            let member = code[..begin].trim_end().ends_with('.');
            if valid_identifier(name) && (member || !is_keyword(name)) {
                ranges.push(begin..offset);
            }
        }
    }
    ranges
}

fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "crate"
            | "default"
            | "def"
            | "defer"
            | "delete"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "extern"
            | "false"
            | "finally"
            | "fn"
            | "for"
            | "from"
            | "func"
            | "function"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "interface"
            | "is"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "namespace"
            | "new"
            | "nil"
            | "null"
            | "package"
            | "pass"
            | "private"
            | "protected"
            | "pub"
            | "public"
            | "raise"
            | "ref"
            | "return"
            | "static"
            | "struct"
            | "super"
            | "switch"
            | "throw"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typedef"
            | "typeof"
            | "union"
            | "unsafe"
            | "use"
            | "var"
            | "void"
            | "where"
            | "while"
            | "with"
            | "yield"
    )
}

fn occurrences<'a>(code: &'a str, symbol: &'a str) -> impl Iterator<Item = usize> + 'a {
    code.match_indices(symbol).filter_map(move |(offset, _)| {
        let before = code[..offset].chars().next_back();
        let after = code[offset + symbol.len()..].chars().next();
        (!before.is_some_and(ident_char) && !after.is_some_and(ident_char)).then_some(offset)
    })
}
fn extension(path: &str) -> String {
    path.rsplit_once('.')
        .map_or("", |(_, ext)| ext)
        .to_ascii_lowercase()
}
fn is_js(path: &str) -> bool {
    matches!(
        path.rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .as_deref(),
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    )
}
fn language_paths(path: &str) -> Vec<String> {
    let ext = path
        .rsplit_once('.')
        .map_or("", |(_, ext)| ext)
        .to_ascii_lowercase();
    let extensions: &[&str] = match ext.as_str() {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
        }
        "dart" => &["dart"],
        "rs" => &["rs"],
        "py" | "pyi" | "pyw" => &["py", "pyi", "pyw"],
        "go" => &["go"],
        _ => return vec![".".into()],
    };
    extensions
        .iter()
        .map(|ext| format!(":(icase,glob)**/*.{ext}"))
        .collect()
}
fn is_import(code: &str) -> bool {
    let text = code.trim_start();
    text.starts_with("import ")
        || text.starts_with("use ")
        || text.starts_with("from ")
        || (text.starts_with("export ") && text.contains(" from "))
}

fn is_definition(path: &str, code: &str, symbol: &str, offset: usize) -> bool {
    let before = code[..offset].trim_end();
    let after = code[offset + symbol.len()..].trim_start();
    let previous = before
        .rsplit(|c: char| !ident_char(c) && c != '!')
        .next()
        .unwrap_or("");
    // Desktop review_jump::definition_pattern と同じ主要宣言。
    if [
        "function",
        "const",
        "let",
        "var",
        "class",
        "interface",
        "type",
        "enum",
        "namespace",
        "fn",
        "struct",
        "trait",
        "mod",
        "def",
        "func",
        "static",
        "macro_rules!",
        "mixin",
        "extension",
        "typedef",
    ]
    .contains(&previous)
    {
        return true;
    }
    if before.ends_with("function*") || before.ends_with("function *") {
        return true;
    }
    if extension(path) == "dart" {
        // Desktop の dart_member_pattern と is_dart_statement の規則を構文トークンで適用。
        if [
            "return", "await", "yield", "throw", "new", "else", "case", "in", "is", "as", "assert",
        ]
        .contains(&previous)
        {
            return false;
        }
        if previous == "get" || previous == "set" {
            return true;
        }
        let prefix = before.trim_start();
        let typed = prefix
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && prefix
                .chars()
                .all(|c| ident_char(c) || c.is_whitespace() || "<>,?.[]".contains(c));
        if typed
            && (after.starts_with('(')
                || after.starts_with('<')
                || after.starts_with(';')
                || (after.starts_with('=') && !after.starts_with("==")))
        {
            return true;
        }
    }
    false
}

/// 行だけから確実に分かる引用文字列・コメントを空白にする。位置は UTF-8 のバイト数で保つ。
/// grep に現れない前行から続くコメントや template interpolation は判別しない。
fn code_only(text: &str, path: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/')
            || (bytes[i] == b'#' && matches!(extension(path).as_str(), "py" | "pyi" | "pyw"))
        {
            result[i..].fill(b' ');
            break;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i < bytes.len() && !(bytes[i - 1] == b'*' && bytes[i] == b'/') {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            result[start..i].fill(b' ');
            continue;
        }
        if matches!(bytes[i], b'\'' | b'"' | b'`') {
            // Rust lifetime は文字列開始として扱わない。
            if extension(path) == "rs" && bytes[i] == b'\'' && bytes.get(i + 2) != Some(&b'\'') {
                i += 1;
                continue;
            }
            let quote = bytes[i];
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            result[start..i].fill(b' ');
            continue;
        }
        i += 1;
    }
    String::from_utf8(result).expect("空白への置換は UTF-8 を保持する")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(entries: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cli-symbols-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let fixture = Self(dir);
            fixture.git(&["init", "-q"]);
            for (path, text) in entries {
                let dest = fixture.0.join(path);
                std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
                std::fs::write(dest, text).unwrap();
            }
            fixture.git(&["add", "."]);
            fixture.git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ]);
            fixture
        }
        fn git(&self, args: &[&str]) {
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
        }
        fn search(&self, path: &str, name: &str, mode: SymbolMode) -> ContentResults {
            find(
                &Repository::discover(&self.0).unwrap(),
                "HEAD",
                path,
                name,
                mode,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn picking_uses_byte_ranges_and_ignores_keywords_comments_and_strings() {
        let text = "const 名前 = 呼ぶ($値2, '隠す'); // 注釈";
        let ranges = pickable_identifiers(text, "main.ts");
        assert_eq!(
            ranges.iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(),
            ["名前", "呼ぶ", "$値2"]
        );
        for (path, text, expected) in [
            (
                "main.ts",
                "store.get(); it.set(); map.delete();",
                vec!["store", "get", "it", "set", "map", "delete"],
            ),
            ("lib.rs", "fn go() {}", vec!["go"]),
            ("main.py", "def select(): pass", vec!["select"]),
        ] {
            assert_eq!(
                pickable_identifiers(text, path)
                    .iter()
                    .map(|range| &text[range.clone()])
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let text = "let cafe\u{301} = 123; /* hidden */ cafe\u{301};";
        assert_eq!(
            pickable_identifiers(text, "lib.rs")
                .iter()
                .map(|r| &text[r.clone()])
                .collect::<Vec<_>>(),
            ["cafe\u{301}", "cafe\u{301}"]
        );
    }

    #[test]
    fn snapshot_definitions_and_usages_are_case_sensitive_whole_identifiers() {
        let fixture = Fixture::new(&[("src/main.ts", "export function $名前() {}\n$名前();\n$名前Long();\nother$名前();\n// $名前();\nconst s = '$名前()';\n"),
            ("src/other.tsx", "$名前();\n"), ("other.py", "def $名前(): pass\n")]);
        std::fs::write(fixture.0.join("src/main.ts"), "changed();").unwrap();
        let defs = fixture.search("src/main.ts", "$名前", SymbolMode::Definitions);
        assert_eq!(defs.hits.len(), 1);
        assert_eq!((&*defs.hits[0].path, defs.hits[0].line), ("src/main.ts", 1));
        let uses = fixture.search("src/main.ts", "$名前", SymbolMode::Usages);
        assert_eq!(
            uses.hits
                .iter()
                .map(|h| (&*h.path, h.line))
                .collect::<Vec<_>>(),
            [("src/main.ts", 2), ("src/other.tsx", 1)]
        );
        assert!(fixture
            .search("src/main.ts", "$名", SymbolMode::Usages)
            .hits
            .is_empty());
        assert!(find(
            &Repository::discover(&fixture.0).unwrap(),
            "HEAD",
            "src/main.ts",
            "-x|.*",
            SymbolMode::Usages
        )
        .is_err());
        assert!(!valid_identifier("1name"));
        assert!(!valid_identifier("foo\nbar"));
        assert!(valid_identifier("_$名前2"));
    }

    #[test]
    fn languages_keep_dart_methods_getters_and_exclude_statements() {
        let fixture = Fixture::new(&[("main.dart", "class Demo {\n  Future<void> run() {}\n  String get title => '';\n  void test() {\n    return run();\n    await run();\n    run();\n  }\n}\n"),
            ("a.ts", "function run() {}\nrun();\n"), ("a.py", "def run(): pass\nrun()\n")]);
        let defs = fixture.search("main.dart", "run", SymbolMode::Definitions);
        assert_eq!(defs.hits.len(), 1);
        assert_eq!(defs.hits[0].line, 2);
        let uses = fixture.search("main.dart", "run", SymbolMode::Usages);
        assert_eq!(
            uses.hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            [5, 6, 7]
        );
        assert_eq!(
            fixture
                .search("main.dart", "title", SymbolMode::Definitions)
                .hits[0]
                .line,
            3
        );
        assert_eq!(
            fixture.search("a.py", "run", SymbolMode::Definitions).hits[0].line,
            1
        );
        assert_eq!(
            fixture.search("a.py", "run", SymbolMode::Usages).hits[0].line,
            2
        );
    }

    #[test]
    fn rust_declarations_are_not_usage_rows() {
        let fixture = Fixture::new(&[("lib.rs", "pub struct Thing;\npub trait Action {}\npub fn run() {}\nfn main() { run(); let _x = Thing; }\n// run();\nlet s = \"run()\";\n" )]);
        for name in ["Thing", "Action", "run"] {
            assert_eq!(
                fixture
                    .search("lib.rs", name, SymbolMode::Definitions)
                    .hits
                    .len(),
                1
            );
        }
        assert_eq!(
            fixture.search("lib.rs", "run", SymbolMode::Usages).hits[0].line,
            4
        );
    }

    #[test]
    fn relative_ts_alias_and_default_import_rank_their_source() {
        let fixture = Fixture::new(&[("src/main.tsx", "import { original as Local } from './dep.js';\nimport Widget from './widget';\nLocal();\nconst view = <Widget />;\n"),
            ("aaa.ts", "export function original() {}\n"),
            ("src/dep.ts", "export function original() {}\n"),
            ("src/widget.tsx", "export default function ActualWidget() {}\n")]);
        let defs = fixture.search("src/main.tsx", "Local", SymbolMode::Definitions);
        assert_eq!(defs.hits[0].path, "src/dep.ts");
        assert_eq!(defs.hits.len(), 2);
        let uses = fixture.search("src/main.tsx", "Local", SymbolMode::Usages);
        assert_eq!(uses.hits.len(), 1);
        assert_eq!(uses.hits[0].line, 3);
        let defaults = fixture.search("src/main.tsx", "Widget", SymbolMode::Definitions);
        assert_eq!(defaults.hits[0].path, "src/widget.tsx");
    }

    #[test]
    fn preferred_source_survives_broad_candidate_limit_and_missing_source_is_optional() {
        let text = "export function needle() {}\n";
        let mut entries = (0..510)
            .map(|n| (format!("a/{n:03}.ts"), text))
            .collect::<Vec<_>>();
        entries.push(("z/source.ts".into(), text));
        let borrowed = entries
            .iter()
            .map(|(p, t)| (p.as_str(), *t))
            .collect::<Vec<_>>();
        let fixture = Fixture::new(&borrowed);
        let result = fixture.search("z/source.ts", "needle", SymbolMode::Definitions);
        assert!(result.truncated);
        assert_eq!(result.hits.len(), 500);
        assert_eq!(result.hits[0].path, "z/source.ts");
        let missing = fixture.search("deleted.ts", "needle", SymbolMode::Definitions);
        assert!(!missing.hits.is_empty());
    }

    #[test]
    fn comments_before_definitions_do_not_consume_accepted_hit_budget() {
        let text = format!("{}function needle() {{}}\n", "// needle\n".repeat(30));
        let fixture = Fixture::new(&[("main.ts", &text)]);
        let results = fixture.search("main.ts", "needle", SymbolMode::Definitions);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].line, 31);
        assert!(!results.truncated);
    }
}
