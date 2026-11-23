use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::git::{Comparison, FileDiff};
use crate::paths;

const FILE_NAME: &str = "review-viewed.json";

#[derive(Default, Deserialize, Serialize)]
struct ViewedComparisons {
    comparisons: Vec<ViewedComparison>,
}

#[derive(Deserialize, Serialize)]
struct ViewedComparison {
    repository: PathBuf,
    base: String,
    head: String,
    files: BTreeMap<String, String>,
}

impl ViewedComparison {
    fn matches(&self, repository: &Path, comparison: &Comparison) -> bool {
        self.repository == repository
            && self.base == comparison.base_key
            && self.head == comparison.head_key
    }
}

pub(super) struct ViewedStore {
    repository: PathBuf,
    path: PathBuf,
    state: ViewedComparisons,
}

impl ViewedStore {
    pub(super) fn load(repository: PathBuf) -> Result<Self> {
        Self::load_at(repository, paths::data_dir().join(FILE_NAME))
    }

    pub(super) fn load_at(repository: PathBuf, path: PathBuf) -> Result<Self> {
        let state = paths::load_json(&path)?;
        Ok(Self {
            repository,
            path,
            state,
        })
    }

    pub(super) fn is_viewed(&self, comparison: &Comparison, file: &FileDiff) -> bool {
        self.state
            .comparisons
            .iter()
            .find(|row| row.matches(&self.repository, comparison))
            .and_then(|row| row.files.get(file.path()))
            .is_some_and(|digest| digest == &file.digest)
    }

    pub(super) fn set_viewed(
        &mut self,
        comparison: &Comparison,
        file: &FileDiff,
        viewed: bool,
    ) -> Result<()> {
        // セッションを複数開いても、更新のたびに最新の状態に1件だけ反映する。
        let _lock = paths::lock("review-viewed")?;
        let mut state: ViewedComparisons = paths::load_json(&self.path)?;
        if let Some(index) = state
            .comparisons
            .iter()
            .position(|row| row.matches(&self.repository, comparison))
        {
            let files = &mut state.comparisons[index].files;
            if viewed {
                files.insert(file.path().to_string(), file.digest.clone());
            } else {
                files.remove(file.path());
                if files.is_empty() {
                    state.comparisons.remove(index);
                }
            }
        } else if viewed {
            state.comparisons.push(ViewedComparison {
                repository: self.repository.clone(),
                base: comparison.base_key.clone(),
                head: comparison.head_key.clone(),
                files: BTreeMap::from([(file.path().to_string(), file.digest.clone())]),
            });
        }
        paths::save_json(&self.path, &state)?;
        self.state = state;
        Ok(())
    }
}
