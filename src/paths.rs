//! 確認済み履歴の保存先・JSON の読み書き・同時更新のロック。
//!
//! 既存の確認済み履歴を引き継ぐため、my-job-tool-cli と保存先を共有する。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub const DATA_DIR_ENV: &str = "MY_JOB_TOOL_DATA_DIR";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn data_dir() -> PathBuf {
    match std::env::var(DATA_DIR_ENV) {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => home().join(".local/share/my-job-tool-cli"),
    }
}

/// 同時更新のロックを置くディレクトリ。
pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

/// JSON を読む。ファイルが無ければ既定値で返す
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} を読めません", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("{} を読めません", path.display())),
    }
}

/// JSON を書く。途中で落ちても壊れないよう、一時ファイルに書いてから置き換える
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("{} を作れません", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("{} に書けません", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("{} に書けません", path.display()))?;
    Ok(())
}

// ---- ロック -------------------------------------------------------------------

/// 同じデータを書き換える処理を 2 つ同時に流さないためのロック。
/// 返したファイルを持っている間だけ有効で、プロセスが終われば（落ちても）OS が外す
pub fn lock(name: &str) -> Result<File> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("{} を作れません", dir.display()))?;
    let path = dir.join(format!("{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("{} を開けません", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("同じ処理（{name}）が別に実行中です。終わってからもう一度実行してください")
        }
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("{} をロックできません", path.display()))
        }
    }
}
