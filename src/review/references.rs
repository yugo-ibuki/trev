//! コミット内の静的なファイル依存と、読み込んだ名前の字句上の使用候補を読む。
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

use super::git::Repository;
use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub(super) struct FileReference {
    pub(super) path: String,
    pub(super) line: u32,
    pub(super) text: String,
}

pub(super) struct FileReferences {
    pub(super) incoming: Vec<FileReference>,
    pub(super) outgoing: Vec<FileReference>,
    pub(super) note: Option<String>,
}

pub(super) struct ReferenceIndex {
    incoming: HashMap<String, Vec<FileReference>>,
    outgoing: HashMap<String, Vec<FileReference>>,
    note: Option<String>,
}

const MAX_FILE: usize = 512 * 1024;
const MAX_TOTAL: usize = 32 * 1024 * 1024;

impl ReferenceIndex {
    pub(super) fn load(repo: &Repository, commit: &str) -> Result<Self> {
        let tree = repo.git_bytes(&["ls-tree", "-r", "-l", "-z", commit])?;
        let mut files = HashSet::new();
        let mut objects = BTreeMap::new();
        let mut skipped = 0;
        for entry in tree.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let head = std::str::from_utf8(&entry[..tab])?;
            let fields: Vec<_> = head.split_whitespace().collect();
            if fields.len() != 4 || fields[1] != "blob" || fields[0] == "120000" {
                continue;
            }
            let Ok(path) = std::str::from_utf8(&entry[tab + 1..]) else {
                skipped += 1;
                continue;
            };
            files.insert(path.to_owned());
            if supported(path) || path.ends_with("package.json") {
                let size = fields[3]
                    .parse::<usize>()
                    .context("参照解析のファイルサイズを読めません")?;
                if size > MAX_FILE {
                    skipped += 1;
                } else {
                    objects.insert(path.to_owned(), (fields[2].to_owned(), size));
                }
            }
        }
        let mut total = 0;
        objects.retain(|_, (_, size)| {
            if total + *size > MAX_TOTAL {
                skipped += 1;
                false
            } else {
                total += *size;
                true
            }
        });
        let texts = read_batch(repo, &objects)?;
        skipped += objects.len().saturating_sub(texts.len());
        let mut index = Self::from_texts(files, &texts);
        if skipped > 0 {
            index.note = Some(format!("一部のみ: {skipped} ファイルを省略（1ファイル512KiB・合計32MiB上限、非UTF-8ファイル/パス）。参照なしとは断定できません。"));
        }
        Ok(index)
    }

    fn from_texts(files: HashSet<String>, texts: &BTreeMap<String, String>) -> Self {
        let mut index = Self {
            incoming: HashMap::new(),
            outgoing: HashMap::new(),
            note: None,
        };
        let packages = packages(texts);
        for (path, text) in texts.iter().filter(|(p, _)| supported(p)) {
            let tokens = lex(text, path.ends_with(".rs"));
            let lines = text.lines().collect::<Vec<_>>();
            let usages = if path.ends_with(".rs") {
                HashMap::new()
            } else {
                js_usage_lines(&tokens)
            };
            let mut seen = HashSet::new();
            let mut seen_incoming = HashSet::new();
            for (destination, line, token_index) in dependencies(path, &tokens, &files, &packages) {
                let new_dependency = seen.insert((destination.clone(), line));
                let evidence = lines
                    .get(line.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(240)
                    .collect::<String>();
                let usage_lines = usages.get(&token_index).filter(|rows| !rows.is_empty());
                let fallback = [line];
                for &usage_line in usage_lines.map(Vec::as_slice).unwrap_or(&fallback) {
                    if !seen_incoming.insert((destination.clone(), usage_line)) {
                        continue;
                    }
                    let usage_text = lines
                        .get(usage_line.saturating_sub(1) as usize)
                        .copied()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(240)
                        .collect::<String>();
                    index
                        .incoming
                        .entry(destination.clone())
                        .or_default()
                        .push(FileReference {
                            path: path.clone(),
                            line: usage_line,
                            text: if usage_lines.is_some() {
                                usage_text
                            } else {
                                format!("読み込み: {usage_text}")
                            },
                        });
                }
                if new_dependency {
                    index
                        .outgoing
                        .entry(path.clone())
                        .or_default()
                        .push(FileReference {
                            path: destination,
                            line: 1,
                            text: format!("{path}:{line} {evidence}"),
                        });
                }
            }
        }
        for rows in index
            .incoming
            .values_mut()
            .chain(index.outgoing.values_mut())
        {
            rows.sort_by(|a, b| (&a.path, a.line, &a.text).cmp(&(&b.path, b.line, &b.text)));
        }
        index
    }

    pub(super) fn rows(&self, path: &str, tab: usize) -> &[FileReference] {
        let map = if tab == 0 {
            &self.incoming
        } else {
            &self.outgoing
        };
        map.get(path).map(Vec::as_slice).unwrap_or_default()
    }

    pub(super) fn for_file(&self, path: &str) -> FileReferences {
        FileReferences {
            incoming: self.incoming.get(path).cloned().unwrap_or_default(),
            outgoing: self.outgoing.get(path).cloned().unwrap_or_default(),
            note: self.note.clone(),
        }
    }
}

fn supported(path: &str) -> bool {
    [
        ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts", ".rs",
    ]
    .iter()
    .any(|ext| path.ends_with(ext))
}

fn read_batch(
    repo: &Repository,
    objects: &BTreeMap<String, (String, usize)>,
) -> Result<BTreeMap<String, String>> {
    if objects.is_empty() {
        return Ok(BTreeMap::new());
    }
    // 既存 Reader は1ファイルずつ git を起動するため、大規模リポジトリでは batch で同じコミットを読む。
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("参照解析用の git を起動できません")?;
    let request = objects
        .values()
        .map(|(hash, _)| format!("{hash}\n"))
        .collect::<String>();
    let mut stdin = child.stdin.take().context("git の入力を開けません")?;
    // 入出力が同時に pipe の容量を超えても停止しないよう、書き込みを分離する。
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let output = child
        .wait_with_output()
        .context("参照解析の git 出力を読めません")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("参照解析の入力処理が停止しました"))??;
    if !output.status.success() {
        bail!(
            "参照解析の git が失敗しました: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut bytes = output.stdout.as_slice();
    let mut texts = BTreeMap::new();
    for (path, (_, size)) in objects {
        let end = bytes
            .iter()
            .position(|b| *b == b'\n')
            .context("参照解析のオブジェクトヘッダーが不正です")?;
        let header = std::str::from_utf8(&bytes[..end])?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[1] != "blob" || fields[2].parse::<usize>()? != *size {
            bail!("参照解析のオブジェクトが一致しません: {path}");
        }
        bytes = &bytes[end + 1..];
        if bytes.len() < size + 1 {
            bail!("参照解析のファイルが途中で切れています: {path}");
        }
        if let Ok(text) = std::str::from_utf8(&bytes[..*size]) {
            texts.insert(path.clone(), text.to_owned());
        }
        bytes = &bytes[size + 1..];
    }
    Ok(texts)
}

#[derive(Debug)]
struct Token {
    value: String,
    line: u32,
    quoted: bool,
}

/// コメントと文字列内部の import を除外する小さな字句解析。構文木や評価は使わない。
fn lex(text: &str, rust: bool) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < bytes.len() {
        let start_line = line;
        match bytes[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            quote @ (b'\'' | b'"' | b'`') => {
                // Rust の lifetime は引用符として消費しない。
                if rust
                    && quote == b'\''
                    && bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic())
                    && bytes.get(i + 2) != Some(&b'\'')
                {
                    i += 1;
                    continue;
                }
                i += 1;
                let start = i;
                let mut escaped = false;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    if bytes[i] == b'\\' {
                        escaped = true;
                        i += 1;
                    }
                    i += 1;
                }
                let end = i.min(bytes.len());
                let value = if quote != b'`' && !escaped {
                    String::from_utf8_lossy(&bytes[start..end]).into_owned()
                } else {
                    String::new()
                };
                out.push(Token {
                    value,
                    line: start_line,
                    quoted: true,
                });
                i = (i + 1).min(bytes.len());
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                out.push(Token {
                    value: String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                    line,
                    quoted: false,
                });
            }
            c => {
                out.push(Token {
                    value: (c as char).to_string(),
                    line,
                    quoted: false,
                });
                i += 1;
            }
        }
    }
    out
}

struct JsBinding<'a> {
    local: &'a str,
    imported: &'a str,
    default: bool,
}

fn identifier(token: &Token) -> bool {
    !token.quoted
        && token
            .value
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_' || *c == b'$')
        && token
            .value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
}

/// Desktop の parse_clause と同じ束縛を、既存 lexer のトークンから取り出す。
/// 文字列ベースの parser はコメントを再び解釈するため、そのまま移植しない。
fn js_bindings(clause: &[Token]) -> Vec<JsBinding<'_>> {
    let mut out = Vec::new();
    let mut named = false;
    let mut i = usize::from(
        clause.first().is_some_and(|t| t.value == "type")
            && clause
                .get(1)
                .is_some_and(|t| identifier(t) || matches!(t.value.as_str(), "{" | "*")),
    );
    while i < clause.len() {
        let token = &clause[i];
        match token.value.as_str() {
            "{" => named = true,
            "}" => named = false,
            "*" if !named && clause.get(i + 1).is_some_and(|t| t.value == "as") => {
                if let Some(local) = clause.get(i + 2).filter(|t| identifier(t)) {
                    out.push(JsBinding {
                        local: &local.value,
                        imported: "*",
                        default: false,
                    });
                    i += 2;
                }
            }
            "," => {}
            _ if named && identifier(token) => {
                // `type Foo` は修飾子、`type as Local` の type は名前。
                if token.value == "type"
                    && clause
                        .get(i + 1)
                        .is_some_and(|t| identifier(t) && t.value != "as")
                {
                    i += 1;
                    continue;
                }
                if clause.get(i + 1).is_some_and(|t| t.value == "as") {
                    if let Some(local) = clause.get(i + 2).filter(|t| identifier(t)) {
                        out.push(JsBinding {
                            local: &local.value,
                            imported: &token.value,
                            default: false,
                        });
                        i += 2;
                    }
                } else {
                    out.push(JsBinding {
                        local: &token.value,
                        imported: &token.value,
                        default: false,
                    });
                }
            }
            _ if !named
                && identifier(token)
                && (i == 0 || (i == 1 && clause[0].value == "type")) =>
            {
                out.push(JsBinding {
                    local: &token.value,
                    imported: &token.value,
                    default: true,
                });
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn js_from(rest: &[Token]) -> Option<usize> {
    rest.iter()
        .take(256)
        .take_while(|t| t.value != ";" && t.value != "import" && t.value != "export")
        .position(|t| !t.quoted && t.value == "from")
        .filter(|&i| rest.get(i + 1).is_some_and(|t| t.quoted))
}

/// 1ファイルにつき束縛名の索引とトークン走査を1回ずつ行う。
/// スコープ解析は行わないため、同名のローカル変数は使用候補に含まれる。
fn js_usage_lines(tokens: &[Token]) -> HashMap<usize, Vec<u32>> {
    let mut bindings: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut ignored = vec![false; tokens.len()];
    let mut declaration_start = 0;
    for (i, token) in tokens.iter().enumerate() {
        if token.quoted {
            continue;
        }
        let rest = &tokens[i + 1..];
        if token.value == "import" || token.value == "export" {
            if let Some(from) = js_from(rest) {
                ignored[i..=i + from + 2].fill(true);
                if token.value == "import" {
                    for binding in js_bindings(&rest[..from]) {
                        bindings.entry(binding.local).or_default().push(i);
                    }
                }
            } else if token.value == "export" && rest.first().is_some_and(|t| t.value == "{") {
                if let Some(end) = rest.iter().position(|t| t.value == "}") {
                    ignored[i..=i + end + 1].fill(true);
                }
            }
        }
        if matches!(token.value.as_str(), "const" | "let" | "var" | ";") {
            declaration_start = i;
        }
        if token.value != "require"
            || i == 0
            || tokens[i - 1].value != "="
            || rest.len() < 3
            || rest[0].value != "("
            || !rest[1].quoted
            || rest[2].value != ")"
        {
            continue;
        }
        // const Name = require('...') / const { name: Local } = require('...').
        // 複雑な分割代入・既存変数への代入は読み込み行へフォールバックする。
        let declaration = &tokens[declaration_start..i - 1];
        if declaration
            .first()
            .is_none_or(|t| !matches!(t.value.as_str(), "const" | "let" | "var"))
        {
            continue;
        }
        let clause = &declaration[1..];
        let locals = if clause.len() == 1 && identifier(&clause[0]) {
            vec![clause[0].value.as_str()]
        } else if clause.first().is_some_and(|t| t.value == "{")
            && clause.last().is_some_and(|t| t.value == "}")
        {
            let mut locals = Vec::new();
            let mut valid = true;
            for part in clause[1..clause.len() - 1]
                .split(|t| t.value == ",")
                .filter(|p| !p.is_empty())
            {
                if part.len() == 1 && identifier(&part[0]) {
                    locals.push(part[0].value.as_str());
                } else if part.len() == 3
                    && identifier(&part[0])
                    && part[1].value == ":"
                    && identifier(&part[2])
                {
                    locals.push(part[2].value.as_str());
                } else {
                    valid = false;
                    break;
                }
            }
            if valid {
                locals
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        ignored[declaration_start..=i + 3].fill(true);
        for local in locals {
            bindings.entry(local).or_default().push(i);
        }
    }
    let mut usages: HashMap<usize, Vec<u32>> = HashMap::new();
    for (i, token) in tokens.iter().enumerate() {
        if ignored[i]
            || token.quoted
            || i.checked_sub(1).is_some_and(|p| tokens[p].value == ".")
            || i.checked_sub(1).is_some_and(|p| {
                matches!(
                    tokens[p].value.as_str(),
                    "const"
                        | "let"
                        | "var"
                        | "function"
                        | "class"
                        | "interface"
                        | "type"
                        | "enum"
                        | "namespace"
                )
            })
            || (tokens.get(i + 1).is_some_and(|t| t.value == ":")
                && i.checked_sub(1)
                    .is_some_and(|p| matches!(tokens[p].value.as_str(), "{" | ",")))
        {
            continue;
        }
        if let Some(imports) = bindings.get(token.value.as_str()) {
            for &import in imports {
                let rows = usages.entry(import).or_default();
                if rows.last() != Some(&token.line) {
                    rows.push(token.line);
                }
            }
        }
    }
    usages
}

fn dependencies(
    path: &str,
    tokens: &[Token],
    files: &HashSet<String>,
    packages: &[(String, String, serde_json::Value)],
) -> Vec<(String, u32, usize)> {
    let mut out = Vec::new();
    for (i, token) in tokens.iter().enumerate().filter(|(_, t)| !t.quoted) {
        let rest = &tokens[i + 1..];
        let destination = if path.ends_with(".rs") {
            if token.value == "mod" && rest.len() >= 2 && rest[1].value == ";" {
                let dir = rust_module_dir(path);
                join(&dir, &rest[0].value).and_then(|base| rust_file(files, &base))
            } else if token.value == "use" {
                rust_use(path, rest, files)
            } else {
                None
            }
        } else if token.value == "import" || token.value == "export" {
            if token.value == "import" && rest.first().is_some_and(|t| t.quoted) {
                resolve_js(path, &rest[0].value, files, packages)
            } else if rest.first().is_some_and(|t| t.value == "(") {
                None
            } else {
                js_from(rest).and_then(|n| resolve_js(path, &rest[n + 1].value, files, packages))
            }
        } else if token.value == "require"
            && i.checked_sub(1)
                .and_then(|n| tokens.get(n))
                .is_none_or(|t| t.value != ".")
            && rest.len() >= 3
            && rest[0].value == "("
            && rest[1].quoted
            && rest[2].value == ")"
        {
            resolve_js(path, &rest[1].value, files, packages)
        } else {
            None
        };
        if let Some(destination) = destination {
            out.push((destination, token.line, i));
        }
    }
    out
}

// review_jump.rs の normalize / find_file / source_base と同じ解決規則を snapshot に適用。
fn join(dir: &str, rest: &str) -> Option<String> {
    let joined = format!("{dir}/{rest}");
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}
fn find_js(files: &HashSet<String>, base: &str) -> Option<String> {
    let stem = [".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|ext| base.strip_suffix(ext));
    let bases = stem.into_iter().chain(std::iter::once(base));
    for base in bases {
        for ext in [
            "",
            ".ts",
            ".tsx",
            ".js",
            ".jsx",
            ".mjs",
            ".cjs",
            ".mts",
            ".cts",
            ".d.ts",
            "/index.ts",
            "/index.tsx",
            "/index.js",
            "/index.jsx",
        ] {
            let candidate = format!("{base}{ext}");
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}
/// 名前付き・default の静的な相対 import をシンボル候補の優先順位に使う。
/// 既存の字句解析とパス解決を共有し、プロジェクト全体の再解析は行わない。
pub(super) fn symbol_import(
    path: &str,
    text: &str,
    symbol: &str,
    files: &HashSet<String>,
) -> Option<(String, String, bool)> {
    let tokens = lex(text, false);
    for (index, token) in tokens.iter().enumerate() {
        if token.quoted || token.value != "import" {
            continue;
        }
        let rest = &tokens[index + 1..];
        let Some(from) = rest
            .iter()
            .take(256)
            .take_while(|t| t.value != ";" && t.value != "import")
            .position(|t| !t.quoted && t.value == "from")
        else {
            continue;
        };
        let Some(module) = rest
            .get(from + 1)
            .filter(|t| t.quoted && t.value.starts_with('.'))
        else {
            continue;
        };
        let Some(target) = find_js(files, &join(parent(path), &module.value)?) else {
            continue;
        };
        if let Some(binding) = js_bindings(&rest[..from])
            .into_iter()
            .find(|binding| binding.local == symbol && binding.imported != "*")
        {
            return Some((binding.imported.to_owned(), target, binding.default));
        }
    }
    None
}

fn first_export(value: &serde_json::Value) -> Option<&str> {
    value.as_str().or_else(|| {
        ["types", "import", "default", "require", "node"]
            .iter()
            .find_map(|k| value.get(k).and_then(first_export))
    })
}
fn packages(texts: &BTreeMap<String, String>) -> Vec<(String, String, serde_json::Value)> {
    // 依存の実体は checkout に依存せず、コミット内の package.json だけから解決する。
    let mut packages = texts
        .iter()
        .filter(|(p, _)| *p == "package.json" || p.ends_with("/package.json"))
        .filter_map(|(path, text)| {
            let json: serde_json::Value = serde_json::from_str(text).ok()?;
            let name = json["name"].as_str()?.to_owned();
            Some((name, parent(path).to_owned(), json))
        })
        .collect::<Vec<_>>();
    packages.sort_by_key(|(name, _, _)| std::cmp::Reverse(name.len()));
    packages
}
fn resolve_js(
    path: &str,
    module: &str,
    files: &HashSet<String>,
    packages: &[(String, String, serde_json::Value)],
) -> Option<String> {
    if module.starts_with('.') {
        return find_js(files, &join(parent(path), module)?);
    }
    for (name, dir, json) in packages {
        let sub = if module == name {
            ""
        } else if let Some(sub) = module.strip_prefix(&format!("{name}/")) {
            sub
        } else {
            continue;
        };
        let key = if sub.is_empty() {
            ".".to_owned()
        } else {
            format!("./{sub}")
        };
        let exports = &json["exports"];
        let target = exports
            .get(&key)
            .and_then(first_export)
            .or_else(|| {
                if sub.is_empty() {
                    first_export(exports)
                } else {
                    None
                }
            })
            .or_else(|| {
                if sub.is_empty() {
                    ["types", "typings", "module", "main"]
                        .iter()
                        .find_map(|k| json[*k].as_str())
                } else {
                    None
                }
            });
        if let Some(target) = target {
            let mut target = target.trim_start_matches("./").to_owned();
            for prefix in ["dist/", "build/", "lib/"] {
                if let Some(rest) = target.strip_prefix(prefix) {
                    target = format!("src/{rest}");
                    break;
                }
            }
            for ext in [
                ".d.ts", ".d.mts", ".d.cts", ".js", ".mjs", ".cjs", ".ts", ".tsx",
            ] {
                if let Some(stem) = target.strip_suffix(ext) {
                    target = stem.to_owned();
                    break;
                }
            }
            if let Some(file) = find_js(files, &join(dir, &target)?) {
                return Some(file);
            }
        }
        let rest = if sub.is_empty() { "index" } else { sub };
        return join(dir, &format!("src/{rest}"))
            .and_then(|base| find_js(files, &base))
            .or_else(|| join(dir, rest).and_then(|base| find_js(files, &base)));
    }
    None
}
fn rust_module_dir(path: &str) -> String {
    if ["mod.rs", "lib.rs", "main.rs"]
        .iter()
        .any(|name| path.rsplit('/').next() == Some(name))
    {
        parent(path).to_owned()
    } else {
        path.trim_end_matches(".rs").to_owned()
    }
}
fn rust_file(files: &HashSet<String>, base: &str) -> Option<String> {
    [format!("{base}.rs"), format!("{base}/mod.rs")]
        .into_iter()
        .find(|p| files.contains(p))
}
fn rust_use(path: &str, rest: &[Token], files: &HashSet<String>) -> Option<String> {
    let first = rest.first()?.value.as_str();
    let mut dir = match first {
        "self" => rust_module_dir(path),
        "super" => parent(&rust_module_dir(path)).to_owned(),
        "crate" => {
            let mut dir = parent(path);
            loop {
                if files.contains(&join(dir, "lib.rs")?) || files.contains(&join(dir, "main.rs")?) {
                    break;
                }
                if dir.is_empty() {
                    return None;
                }
                dir = parent(dir);
            }
            dir.to_owned()
        }
        _ => return None,
    };
    let mut destination = None;
    let mut n = 1;
    while n + 2 < rest.len() && rest[n].value == ":" && rest[n + 1].value == ":" {
        let name = &rest[n + 2];
        if name.quoted
            || !name
                .value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            break;
        }
        if name.value == "super" {
            dir = parent(&dir).to_owned();
        } else {
            dir = join(&dir, &name.value)?;
            if let Some(file) = rust_file(files, &dir) {
                destination = Some(file);
            }
        }
        n += 3;
    }
    destination
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(entries: &[(&str, &str)]) -> ReferenceIndex {
        let texts: BTreeMap<_, _> = entries
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect();
        ReferenceIndex::from_texts(texts.keys().cloned().collect(), &texts)
    }
    #[test]
    fn batch_reads_commit_instead_of_modified_worktree_and_reports_limits() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("cli-references-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(
            dir.join("main.ts"),
            format!("import './dep';\n/*{}*/", "x".repeat(100_000)),
        )
        .unwrap();
        std::fs::write(dir.join("dep.ts"), "export const value = 1;").unwrap();
        std::fs::write(dir.join("too-large.ts"), "x".repeat(MAX_FILE + 1)).unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        std::fs::write(dir.join("main.ts"), "import './other';").unwrap();
        let repo = Repository::discover(&dir).unwrap();
        let index = ReferenceIndex::load(&repo, "HEAD").unwrap();
        assert_eq!(index.for_file("dep.ts").incoming[0].path, "main.ts");
        let note = index.for_file("main.ts").note.unwrap();
        assert!(note.starts_with("一部のみ: 1 ファイルを省略"));
        assert!(!note.contains("静的参照のみ"));
    }
    #[test]
    fn js_references_are_file_level_and_keep_import_line() {
        let index = fixture(&[
            ("src/a.ts", "// import './fake'\nconst text = \"import './fake'\";\nimport {\n thing\n} from './b.js';\nexport * from './c';\nrequire('./b');\nimport('./fake');\nimport './c';\nimport '../../fake';"),
            ("src/b.ts", "export const thing = 1;"), ("src/c/index.ts", ""), ("src/fake.ts", ""), ("fake.ts", ""),
        ]);
        assert!(index.for_file("src/a.ts").note.is_none());
        let rows = index.for_file("src/a.ts").outgoing;
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.line == 1));
        let incoming = index.for_file("src/b.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![3, 7]
        );
        assert_eq!(incoming[0].path, "src/a.ts");
        assert!(index.for_file("src/fake.ts").incoming.is_empty());
        assert!(index.for_file("fake.ts").incoming.is_empty());
    }
    #[test]
    fn incoming_named_imports_point_to_each_usage_line_and_keep_outgoing_dependency() {
        let index = fixture(&[
            ("main.ts", "import { value, Other } from './dep';\nvalue(); value();\nconst next: Other = value;\nvalue();"),
            ("dep.ts", "export const value = 1;"),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(incoming[0].text, "value(); value();");
        assert_eq!(incoming[1].text, "const next: Other = value;");
        assert!(incoming.iter().all(|r| r.path == "main.ts"));
        let outgoing = index.for_file("main.ts").outgoing;
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].line, 1);
        assert_eq!(
            outgoing[0].text,
            "main.ts:1 import { value, Other } from './dep';"
        );
    }

    #[test]
    fn incoming_alias_default_namespace_and_type_bindings_use_local_names() {
        let index = fixture(&[
            ("main.tsx", "import Default, {\n original as Local, type Shape\n} from './dep';\nimport * as Namespace from './dep';\nLocal();\nconst view = <Default />;\nNamespace.original();\nconst shape: Shape = {};\noriginal();"),
            ("dep.ts", ""),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![5, 6, 7, 8]
        );
    }

    #[test]
    fn usage_candidates_ignore_comments_strings_properties_and_export_clauses() {
        let index = fixture(&[
            ("main.ts", "import { value as Local } from './dep';\n// Local();\n/* Local */\nconst text = 'Local';\nconst template = `Local`;\nobj.Local();\nconst object = { Local: 1 };\nexport { Local };\nexport { Local as renamed } from './other';\nLocal();"),
            ("dep.ts", ""), ("other.ts", ""),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].line, 10);
        assert_eq!(incoming[0].text, "Local();");
        assert_eq!(index.for_file("other.ts").incoming[0].line, 9);
    }

    #[test]
    fn unused_bindings_side_effects_and_reexports_keep_labeled_loading_evidence() {
        let index = fixture(&[
            ("main.ts", "import { unused } from './dep';\nimport './dep';\nexport * from './dep';\nrequire('./dep');"),
            ("dep.ts", ""),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert!(incoming.iter().all(|r| r.text.starts_with("読み込み: ")));
        assert_eq!(index.for_file("main.ts").outgoing.len(), 4);
    }

    #[test]
    fn require_namespace_and_simple_destructuring_use_local_bindings() {
        let index = fixture(&[
            ("main.cjs", "const Namespace = require('./dep');\nconst { original: Local, value } = require('./dep');\nNamespace.original();\nLocal(); value();\nobj.Local();\noriginal();"),
            ("dep.js", ""),
        ]);
        let incoming = index.for_file("dep.js").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(index.for_file("main.cjs").outgoing.len(), 2);
    }

    #[test]
    fn multiple_dependencies_on_one_line_do_not_share_binding_usages() {
        let index = fixture(&[
            (
                "main.ts",
                "import A from './a'; import B from './b';\nA();\nB();",
            ),
            ("a.ts", ""),
            ("b.ts", ""),
        ]);
        assert_eq!(index.for_file("a.ts").incoming[0].line, 2);
        assert_eq!(index.for_file("b.ts").incoming[0].line, 3);
    }

    #[test]
    fn ternary_operands_are_usages_and_obvious_declaration_names_are_not() {
        let index = fixture(&[
            ("main.ts", "import { Local } from './dep';\nconst Local = 1;\nlet Local;\nvar Local;\nfunction Local() {}\nclass Local {}\ninterface Local {}\ntype Local = string;\nenum Local {}\nconst result = flag ? Local : other;\nconst object = { Local: 1 };\nconst short = { Local };"),
            ("dep.ts", ""),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![10, 12]
        );
    }

    #[test]
    fn default_binding_named_type_keeps_its_usage_and_symbol_resolution() {
        let index = fixture(&[
            ("main.ts", "import type from './dep';\ntype();"),
            ("dep.ts", ""),
        ]);
        assert_eq!(index.for_file("dep.ts").incoming[0].line, 2);
        let files = ["dep.ts".to_owned()].into_iter().collect();
        assert_eq!(
            symbol_import("main.ts", "import type from './dep';", "type", &files),
            Some(("type".into(), "dep.ts".into(), true))
        );
    }

    #[test]
    fn same_line_imports_from_one_destination_keep_both_bindings_and_deduplicate_usages() {
        let index = fixture(&[
            (
                "main.ts",
                "import { A } from './dep'; import { B } from './dep';\nA(); B();\nB();",
            ),
            ("dep.ts", ""),
        ]);
        let incoming = index.for_file("dep.ts").incoming;
        assert_eq!(
            incoming.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(index.for_file("main.ts").outgoing.len(), 1);
    }

    #[test]
    fn shared_binding_parser_preserves_symbol_import_resolution() {
        let files = ["dep.ts".to_owned()].into_iter().collect();
        let text = "import Default, { original as Local, type Shape } from './dep';";
        assert_eq!(
            symbol_import("main.ts", text, "Local", &files),
            Some(("original".into(), "dep.ts".into(), false))
        );
        assert_eq!(
            symbol_import("main.ts", text, "Default", &files),
            Some(("Default".into(), "dep.ts".into(), true))
        );
        assert_eq!(symbol_import("main.ts", text, "original", &files), None);
        assert_eq!(
            symbol_import("main.ts", "import * as Ns from './dep';", "Ns", &files),
            None
        );
    }

    #[test]
    fn package_exports_prefer_source_snapshot() {
        let index = fixture(&[
            (
                "apps/main.ts",
                "import { x } from '@demo/core';\nimport '@demo/core/util';",
            ),
            (
                "packages/core/package.json",
                r#"{"name":"@demo/core","exports":{".":{"types":"./dist/index.d.ts"},"./util":"./dist/util.js"}}"#,
            ),
            ("packages/core/src/index.ts", ""),
            ("packages/core/src/util.ts", ""),
        ]);
        let rows = index.for_file("apps/main.ts").outgoing;
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["packages/core/src/index.ts", "packages/core/src/util.ts"]
        );
    }
    #[test]
    fn rust_mod_and_explicit_use_find_modules() {
        let index = fixture(&[
            (
                "crates/demo/src/lib.rs",
                "pub mod foo;\nmod nested;\nuse crate::foo::Thing;",
            ),
            ("crates/demo/src/foo.rs", "pub struct Thing;"),
            (
                "crates/demo/src/nested/mod.rs",
                "use super::foo::Thing;\nmod child;",
            ),
            (
                "crates/demo/src/nested/child.rs",
                "use crate::foo::{Thing};",
            ),
        ]);
        let incoming = index.for_file("crates/demo/src/foo.rs").incoming;
        assert_eq!(incoming.len(), 4);
        assert_eq!(
            index
                .for_file("crates/demo/src/nested/mod.rs")
                .outgoing
                .len(),
            2
        );
    }
    #[test]
    fn unresolved_external_and_dynamic_imports_are_not_claimed() {
        let index = fixture(&[("main.ts", "import React from 'react';\nimport(`./${name}`);\nconst p = require(variable);\nobj.require('./dep');"),("dep.ts", "")]);
        let refs = index.for_file("main.ts");
        assert!(refs.incoming.is_empty() && refs.outgoing.is_empty());
        assert!(refs.note.is_none());
    }
}
