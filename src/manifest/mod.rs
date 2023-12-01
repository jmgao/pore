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

use anyhow::{Context, Error};

mod parser;
mod serializer;

use crate::tree::TreeConfig;
use crate::Config;

// The repo manifest format is described at
// https://gerrit.googlesource.com/git-repo/+/master/docs/manifest-format.md

#[derive(Default, Debug)]
pub struct Manifest {
  pub remotes: HashMap<String, Remote>,
  pub projects: BTreeMap<PathBuf, Project>,
  pub default: Option<Default>,
  pub manifest_server: Option<ManifestServer>,
  pub superproject: Option<SuperProject>,
  pub contactinfo: Option<ContactInfo>,
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

#[derive(Debug)]
pub struct SuperProject {
  pub name: String,
  pub remote: String,
}

#[derive(Debug)]
pub struct ContactInfo {
  pub bug_url: String,
}

#[derive(Clone, Default, Debug)]
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
  pub annotations: HashMap<String, String>,
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

    Ok(dest_branch)
  }
}

#[derive(Default, Debug)]
pub struct ExtendProject {
  pub name: String,
  pub path: Option<String>,
  pub groups: Option<Vec<String>>,
  pub revision: Option<String>,
  pub remote: Option<String>,
}

impl ExtendProject {
  pub fn extend(&self, project: &Project) -> Project {
    // Limit changes to projects at the specified path
    if let Some(path) = &self.path {
      if *path != project.path() {
        return project.clone();
      }
    }

    let mut extended = project.clone();

    if let Some(groups) = &self.groups {
      let mut old_groups = project.groups.clone().unwrap_or_default();
      old_groups.extend(groups.clone());
      extended.groups = Some(old_groups);
    }

    if let Some(revision) = &self.revision {
      extended.revision = Some(revision.clone());
    }

    if let Some(remote) = &self.remote {
      extended.remote = Some(remote.clone());
    }

    extended
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

fn canonicalize_url(url: &str) -> &str {
  url.trim_end_matches('/')
}

impl Manifest {
  pub fn parse(directory: impl AsRef<Path>, file: impl AsRef<Path>) -> Result<Manifest, Error> {
    parser::parse(directory.as_ref(), file.as_ref())
  }

  pub fn serialize(&self, output: Box<dyn Write>) -> Result<(), Error> {
    serializer::serialize(self, output)
  }

  pub fn resolve_project_remote(
    &self,
    config: &Config,
    tree_config: &TreeConfig,
    project: &Project,
  ) -> Result<(String, &Remote), Error> {
    let project_remote_name = project.find_remote(self)?;
    let project_remote = self
      .remotes
      .get(&project_remote_name)
      .ok_or_else(|| format_err!("remote {} missing in manifest", project_remote_name))?;

    // repo allows the use of ".." to mean the URL from which the manifest was cloned.
    if project_remote.fetch == ".." {
      return Ok((tree_config.remote.clone(), project_remote));
    }

    let url = canonicalize_url(&project_remote.fetch);
    for remote in &config.remotes {
      if url == canonicalize_url(&remote.url) {
        return Ok((remote.name.clone(), project_remote));
      }
      for other_url in remote.other_urls.as_deref().unwrap_or(&[]) {
        if url == canonicalize_url(other_url) {
          return Ok((remote.name.clone(), project_remote));
        }
      }
    }

    Err(format_err!("couldn't find remote in configuration matching '{}'", url))
  }
}
