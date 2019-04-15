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

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use failure::{Error, ResultExt};

mod parser;
mod serializer;

// The repo manifest format is described at
// https://gerrit.googlesource.com/git-repo/+/master/docs/manifest-format.md

#[derive(Default, Debug)]
pub struct Manifest {
  pub remotes: HashMap<String, Remote>,
  pub projects: BTreeMap<PathBuf, Project>,
  pub default: Option<Default>,
  pub manifest_server: Option<ManifestServer>,
  pub repo_hooks: Option<RepoHooks>,
}

#[derive(Default, Debug)]
pub struct Remote {
  pub name: String,
  pub alias: Option<String>,
  pub fetch: String,
  pub review: Option<String>,
}

#[derive(Default, Debug)]
pub struct Default {
  pub revision: Option<String>,
  pub remote: Option<String>,
  pub sync_j: Option<u32>,
  pub sync_c: Option<u32>,
}

#[derive(Debug)]
pub struct ManifestServer {
  pub url: String,
}

#[derive(Default, Debug)]
pub struct Project {
  pub name: String,
  pub path: Option<String>,
  pub remote: Option<String>,
  pub revision: Option<String>,

  pub dest_branch: Option<String>,
  pub groups: Option<Vec<String>>,

  pub sync_c: Option<u32>,
  pub clone_depth: Option<u32>,

  pub file_operations: Vec<FileOperation>,
}

impl Project {
  pub fn path(&self) -> String {
    self.path.clone().unwrap_or_else(|| self.name.clone())
  }
}

#[derive(Clone, Debug)]
pub enum FileOperation {
  LinkFile { src: String, dst: String },
  CopyFile { src: String, dst: String },
}

impl FileOperation {
  pub fn src(&self) -> &str {
    match self {
      FileOperation::LinkFile { src, .. } => src,
      FileOperation::CopyFile { src, .. } => src,
    }
  }

  pub fn dst(&self) -> &str {
    match self {
      FileOperation::LinkFile { dst, .. } => dst,
      FileOperation::CopyFile { dst, .. } => dst,
    }
  }
}

#[derive(Default, Debug)]
pub struct RepoHooks {
  pub in_project: Option<String>,
  pub enabled_list: Option<String>,
}

impl Manifest {
  pub fn parse(data: &[u8]) -> Result<Manifest, Error> {
    let data_str = std::str::from_utf8(data).context("invalid UTF-8 in manifest")?;
    parser::parse(data_str)
  }

  pub fn parse_file(path: impl AsRef<Path>) -> Result<Manifest, Error> {
    let path = path.as_ref();
    let data = std::fs::read(path).context(format!("failed to read {:?}", path))?;
    Manifest::parse(&data)
  }

  pub fn serialize(&self, output: Box<dyn Write>) -> Result<(), Error> {
    serializer::serialize(self, output)
  }
}
