//! Git の差分を確認して、ファイルごとの確認済み状態を記録する対話コマンド。

mod all_files;
mod git;
mod highlight;
mod references;
mod symbols;
mod tui;
mod viewed;

use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{bail, Context, Result};

use git::Repository;
use viewed::ViewedStore;

pub fn run(repo: Option<&Path>, base: Option<&str>, head: Option<&str>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("trev は対話型ターミナルで実行してください");
    }
    let cwd = std::env::current_dir().context("現在のディレクトリを取得できません")?;
    let repository = Repository::discover(repo.unwrap_or(&cwd))?;
    let base = match base {
        Some(base) => base.to_string(),
        None => repository.default_base()?,
    };
    let comparison = repository.compare(&base, head.unwrap_or("HEAD"))?;
    let viewed = ViewedStore::load(repository.identity.clone())?;
    tui::run(repository, comparison, viewed)
}
