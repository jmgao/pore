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
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use super::depot::Depot;

use failure::Error;
use failure::ResultExt;
use regex::Regex;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
  #[serde(default = "default_autosubmit")]
  pub autosubmit: bool,

  #[serde(default = "default_presubmit")]
  pub presubmit: bool,
  pub remotes: Vec<RemoteConfig>,
  pub depots: BTreeMap<String, DepotConfig>,
}

fn default_autosubmit() -> bool {
  false
}

fn default_presubmit() -> bool {
  false
}

fn default_project_renames() -> Vec<ProjectRename> {
  Vec::new()
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
  pub manifest: String,
  pub depot: String,

  #[serde(default = "default_branch")]
  pub default_branch: String,

  #[serde(default = "default_manifest_file")]
  pub default_manifest_file: String,

  #[serde(default = "default_project_renames")]
  pub project_renames: Vec<ProjectRename>,
}

fn default_branch() -> String {
  "master".into()
}

fn default_manifest_file() -> String {
  "default.xml".into()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DepotConfig {
  pub path: String,
}

impl Default for Config {
  fn default() -> Config {
    Config {
      autosubmit: default_autosubmit(),
      presubmit: default_presubmit(),
      remotes: vec![RemoteConfig {
        name: "aosp".into(),
        url: "https://android.googlesource.com/".into(),
        other_urls: Some(vec!["persistent-https://android.googlesource.com/".into()]),
        manifest: "platform/manifest".into(),
        depot: "android".into(),
        default_branch: default_branch(),
        default_manifest_file: default_manifest_file(),
        project_renames: default_project_renames(),
      }],
      depots: btreemap! {
        "android".into() => DepotConfig {
          path: "~/.pore/android".into(),
        },
      },
    }
  }
}

impl Config {
  pub fn from_path(path: &Path) -> Result<Config, Error> {
    let mut file = String::new();
    File::open(&path)?.read_to_string(&mut file)?;
    let config = toml::from_str(&file).context(format!("failed to parse config file {:?}", path))?;
    Ok(config)
  }

  fn expand_path(path: &str) -> Result<PathBuf, Error> {
    let path = shellexpand::full(path).context("shell expansion failed")?;
    Ok(path.into_owned().into())
  }

  pub fn find_remote(&self, remote_name: &str) -> Result<&RemoteConfig, Error> {
    for remote in &self.remotes {
      if remote.name == remote_name {
        return Ok(remote);
      }
    }

    Err(format_err!("unknown remote {}", remote_name))
  }

  pub fn find_depot(&self, depot: &str) -> Result<Depot, Error> {
    let depot_config = self
      .depots
      .get(depot)
      .ok_or_else(|| format_err!("unknown depot {}", depot))?;
    let path = Config::expand_path(&depot_config.path).context(format!("failed to expand path for depot {}", depot))?;

    Depot::new(depot.to_string(), path)
  }
}
