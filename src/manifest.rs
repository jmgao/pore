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

use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename = "manifest", deny_unknown_fields)]
pub struct Manifest {
  #[serde(rename = "remote")]
  pub remotes: Vec<Remote>,

  pub default: Option<Default>,

  #[serde(rename = "manifest-server")]
  pub manifest_server: Option<ManifestServer>,

  #[serde(rename = "project")]
  pub projects: Vec<Project>,

  #[serde(rename = "repo-hooks", default)]
  pub repohooks: RepoHooks,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Remote {
  pub name: String,
  pub alias: Option<String>,
  pub fetch: String,
  pub review: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Default {
  pub revision: Option<String>,
  pub remote: Option<String>,

  #[serde(rename = "sync-j")]
  pub syncj: Option<String>,

  #[serde(rename = "sync-c")]
  pub syncc: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManifestServer {
  pub url: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Project {
  pub name: String,
  pub path: Option<String>,
  pub remote: Option<String>,
  pub revision: Option<String>,
  pub groups: Option<String>,
  pub syncc: Option<String>,

  #[serde(rename = "clone-depth")]
  pub clone_depth: Option<String>,

  #[serde(rename = "linkfile", default)]
  pub linkfiles: Vec<Linkfile>,

  #[serde(rename = "copyfile", default)]
  pub copyfiles: Vec<Copyfile>,
}

impl Project {
  pub fn path(&self) -> String {
    self.path.clone().unwrap_or_else(|| self.name.clone())
  }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Copyfile {
  pub src: String,

  #[serde(rename = "dest")]
  pub dst: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Linkfile {
  pub src: String,

  #[serde(rename = "dest")]
  pub dst: String,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoHooks {
  #[serde(rename = "in-project")]
  pub in_project: Option<String>,

  #[serde(rename = "enabled-list")]
  pub enabled_list: Option<String>,
}

impl Manifest {
  pub fn parse(data: &[u8]) -> std::io::Result<Manifest> {
    match serde_xml_rs::from_reader(data) {
      Ok(manifest) => Ok(manifest),
      Err(err) => {
        let error = std::io::Error::new(std::io::ErrorKind::InvalidData, err.description());
        Err(error)
      }
    }
  }

  pub fn parse_file(path: &Path) -> std::io::Result<Manifest> {
    let data = std::fs::read(path)?;
    Manifest::parse(&data)
  }
}
