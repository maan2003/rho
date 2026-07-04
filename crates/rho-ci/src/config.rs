// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `.config/selfci/ci.yaml` parsing — the subset rho consumes: the job
//! command and the mq base branch/merge mode. Parsing is deliberately
//! lenient (unknown fields like `clone-mode` or hook definitions are
//! ignored): repos share this file with stock selfci, which is the
//! validator; rho is a consumer and must not break as the format grows.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

/// Configuration directory path relative to the repo root.
pub const CONFIG_DIR_PATH: &[&str] = &[".config", "selfci"];

/// Main configuration file name.
pub const CONFIG_FILENAME: &str = "ci.yaml";

fn default_command_prefix() -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobConfig {
    pub command: String,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MergeMode {
    #[default]
    Rebase,
    Merge,
}

fn default_merge_mode() -> MergeMode {
    MergeMode::default()
}

#[derive(Debug, Clone, Deserialize)]
pub struct MQConfig {
    #[serde(rename = "base-branch")]
    pub base_branch: Option<String>,
    #[serde(
        rename = "merge-mode",
        alias = "merge-style",
        default = "default_merge_mode"
    )]
    pub merge_mode: MergeMode,
}

#[derive(Debug, Deserialize)]
pub struct SelfCIConfig {
    pub job: JobConfig,
    pub mq: Option<MQConfig>,
}

/// The config file path under a repo root.
pub fn config_path(root: &Path) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in CONFIG_DIR_PATH {
        path.push(segment);
    }
    path.join(CONFIG_FILENAME)
}

/// Parses ci.yaml content (as read from the repo's base revision).
pub fn parse_config(content: &str) -> anyhow::Result<SelfCIConfig> {
    serde_yaml::from_str(content).context("parse ci.yaml")
}

/// Reads `.config/selfci/ci.yaml` under `root`. `Ok(None)` means the repo
/// has no selfci config at all — callers distinguish "not set up" (refuse
/// with a hint) from a malformed file (error).
pub fn read_config(root: &Path) -> anyhow::Result<Option<SelfCIConfig>> {
    let config_path = config_path(root);
    let config_content = match std::fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", config_path.display()));
        }
    };
    let config = parse_config(&config_content)
        .with_context(|| format!("parse {}", config_path.display()))?;
    Ok(Some(config))
}
