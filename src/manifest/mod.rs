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
  pub revision: Option<String>,
}

#[derive(Default, Debug)]
pub struct Default {
  pub revision: Option<String>,
  pub remote: Option<String>,
  pub sync_j: Option<u32>,
  pub sync_c: Option<bool>,
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

  pub sync_c: Option<bool>,
  pub clone_depth: Option<u32>,

  pub file_operations: Vec<FileOperation>,
}

impl Project {
  pub fn path(&self) -> String {
    self.path.clone().unwrap_or_else(|| self.name.clone())
  }

  pub fn find_remote(&self, manifest: &Manifest) -> Result<String, Error> {
    let remote_name = self
      .remote
      .as_ref()
      .or_else(|| manifest.default.as_ref().and_then(|default| default.remote.as_ref()))
      .ok_or_else(|| format_err!("project {} has no remote", self.name))?
      .clone();

    Ok(remote_name)
  }

  pub fn find_revision(&self, manifest: &Manifest) -> Result<String, Error> {
    if let Some(revision) = &self.revision {
      return Ok(revision.clone());
    }

    if let Some(default) = &manifest.default {
      if let Some(revision) = &default.revision {
        return Ok(revision.clone());
      }
    }

    let remote_name = self.find_remote(manifest)?;
    manifest
      .remotes
      .get(&remote_name)
      .as_ref()
      .and_then(|remote| remote.revision.as_ref())
      .cloned()
      .ok_or_else(|| format_err!("project {} has no revision", self.name))
  }

  pub fn find_dest_branch(&self, manifest: &Manifest) -> Result<String, Error> {
    // repo seems to only look at project to calculate dest_branch, but that seems wrong.
    let dest_branch = self
      .dest_branch
      .clone()
      .ok_or_else(|| ())
      .or_else(|_| self.find_revision(manifest))
      .context(format!("project {} has no dest_branch or revision", self.name))?;

    Ok(dest_branch.clone())
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
  pub fn parse(directory: impl AsRef<Path>, filename: impl AsRef<str>) -> Result<Manifest, Error> {
    parser::parse(directory.as_ref(), filename.as_ref())
  }

  pub fn serialize(&self, output: Box<dyn Write>) -> Result<(), Error> {
    serializer::serialize(self, output)
  }
}
