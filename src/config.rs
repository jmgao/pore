/*
 * Copyright (C) 2019 Josh Gao
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;

use super::depot::Depot;

use anyhow::{Context as _, Error};
use regex::Regex;

const DEFAULT_CONFIG: &str = include_str!("../pore.toml");

fn default_update_check() -> bool {
  true
}

fn default_autosubmit() -> bool {
  false
}

fn default_presubmit() -> bool {
  false
}

fn default_upload_options() -> Vec<String> {
  Vec::new()
}

fn default_project_renames() -> Vec<ProjectRename> {
  Vec::new()
}

fn default_branch() -> String {
  "master".into()
}

fn default_manifest_file() -> String {
  "default.xml".into()
}

fn default_parallelism() -> HashMap<String, i32> {
  let mut result = HashMap::new();
  result.insert("status".into(), -16);
  result
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
  #[serde(default = "default_update_check")]
  pub update_check: bool,

  #[serde(default = "default_autosubmit")]
  pub autosubmit: bool,

  #[serde(default = "default_presubmit")]
  pub presubmit: bool,

  pub depots: BTreeMap<String, DepotConfig>,
  pub remotes: Vec<RemoteConfig>,
  pub manifests: Vec<ManifestConfig>,

  #[serde(default = "default_parallelism")]
  pub parallelism: HashMap<String, i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRename {
  #[serde(with = "serde_regex")]
  pub regex: Regex,
  pub replacement: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteConfig {
  pub name: String,
  pub url: String,
  pub other_urls: Option<Vec<String>>,
  pub depot: String,

  #[serde(default = "default_project_renames")]
  pub project_renames: Vec<ProjectRename>,

  #[serde(default = "default_upload_options")]
  pub default_upload_options: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestConfig {
  pub name: String,

  pub remote: String,
  pub project: String,

  #[serde(default = "default_branch")]
  pub default_branch: String,

  #[serde(default = "default_manifest_file")]
  pub default_manifest_file: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DepotConfig {
  pub path: String,
}

impl Default for Config {
  fn default() -> Config {
    toml::from_str(DEFAULT_CONFIG).expect("failed to parse embedded config")
  }
}

impl Config {
  pub fn default_string() -> &'static str {
    DEFAULT_CONFIG
  }

  pub fn from_path(path: &Path) -> Result<Config, Error> {
    let mut file = String::new();
    File::open(path)?.read_to_string(&mut file)?;
    let config = toml::from_str(&file).with_context(|| format!("failed to parse config file {:?}", path))?;
    Ok(config)
  }

  fn expand_path(path: &str) -> Result<PathBuf, Error> {
    let path = shellexpand::full(path).context("shell expansion failed")?;
    Ok(path.into_owned().into())
  }

  pub fn find_depot(&self, depot: &str) -> Result<Depot, Error> {
    let depot_config = self
      .depots
      .get(depot)
      .ok_or_else(|| format_err!("unknown depot {}", depot))?;
    let path =
      Config::expand_path(&depot_config.path).with_context(|| format!("failed to expand path for depot {}", depot))?;

    Depot::new(depot.to_string(), path)
  }

  pub fn find_remote(&self, remote_name: &str) -> Result<&RemoteConfig, Error> {
    for remote in &self.remotes {
      if remote.name == remote_name {
        return Ok(remote);
      }
    }

    Err(format_err!("unknown remote {}", remote_name))
  }

  pub fn find_manifest(&self, manifest_name: &str) -> Result<&ManifestConfig, Error> {
    for manifest in &self.manifests {
      if manifest.name == manifest_name {
        return Ok(manifest);
      }
    }

    Err(format_err!("unknown manifest {}", manifest_name))
  }
}
