//! Git の差分と参照をターミナルでレビューする。

mod paths;
mod review;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(name = "trev", version, about)]
struct Cli {
    /// ローカルリポジトリの場所（省略時は現在のディレクトリ）
    #[arg(long)]
    repo: Option<PathBuf>,
    /// 比較元のブランチまたはコミット
    #[arg(long)]
    base: Option<String>,
    /// レビュー対象のブランチまたはコミット（省略時は HEAD）
    #[arg(long)]
    head: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match review::run(
        cli.repo.as_deref(),
        cli.base.as_deref(),
        cli.head.as_deref(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
