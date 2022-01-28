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

use std::path::{Path, PathBuf};

use anyhow::{Context, Error};

use super::config;
use super::util;

#[derive(Clone, Debug)]
pub struct Depot {
  pub name: String,
  pub path: PathBuf,
}

impl Depot {
  pub fn new(name: String, path: PathBuf) -> Result<Depot, Error> {
    Ok(Depot { name, path })
  }

  fn open_or_create_bare_repo<T: AsRef<Path>>(path: T) -> Result<git2::Repository, Error> {
    let repo = match git2::Repository::open_bare(&path) {
      Ok(repo) => repo,
      Err(_) => git2::Repository::init_bare(&path).context("failed to create repository")?,
    };
    Ok(repo)
  }

  // Reimplementation of clone by hand, because libgit2 doesn't support `clone -l`.
  fn clone_alternates<T: AsRef<Path>>(src: T, dst: T, bare: bool) -> Result<git2::Repository, Error> {
    let src: &Path = src.as_ref();
    let dst: &Path = dst.as_ref();

    let repo = if bare {
      git2::Repository::init_bare(&dst)
    } else {
      git2::Repository::init(&dst)
    };
    let repo = repo.context(format!("failed to create repository at {:?}", dst))?;

    let git_path = if bare { dst.to_path_buf() } else { dst.join(".git") };

    // Set its alternates.
    let alternates_path = git_path.join("objects").join("info").join("alternates");
    let source_path = src.join("objects");
    let alternates_contents = source_path.to_str().unwrap().to_owned() + "\n";
    std::fs::write(&alternates_path, &alternates_contents)
      .context(format!("failed to set alternates for new repository {:?}", dst))?;

    Ok(repo)
  }

  /// Get the path of the git directory given a path to a bare or non-bare repository.
  fn git_path<T: AsRef<Path>>(path: T) -> PathBuf {
    let path: &Path = path.as_ref();
    let nonbare = path.join(".git");
    if nonbare.exists() {
      nonbare
    } else {
      path.to_path_buf()
    }
  }

  fn replace_dir<T: AsRef<Path>>(src: T, dst: T) -> Result<(), Error> {
    let src: &Path = src.as_ref();
    let dst: &Path = dst.as_ref();

    if !src.exists() {
      // This can happen if we fetched a single commit directly, without any refs.
      eprintln!(
        "warning: attempted to replace {:?} with nonexistent directory {:?}",
        dst, src
      );
      return Ok(());
    }

    if dst.exists() {
      std::fs::remove_dir_all(&dst).context(format!("failed to delete {:?}", dst))?;
    }

    std::fs::create_dir_all(
      dst
        .parent()
        .ok_or_else(|| format_err!("failed to get parent of {:?}", dst))?,
    )
    .context(format!("failed to create directory {:?}", dst))?;

    copy_dir::copy_dir(&src, &dst).context(format!("failed to copy directory {:?} to {:?}", src, dst))?;
    Ok(())
  }

  pub fn apply_project_renames<T: AsRef<str>>(remote_config: &config::RemoteConfig, project: T) -> String {
    for rename in &remote_config.project_renames {
      if rename.regex.is_match(project.as_ref()) {
        let result = rename.regex.replace(project.as_ref(), &rename.replacement).into();
        return result;
      }
    }

    return project.as_ref().into();
  }

  pub fn objects_mirror<T: AsRef<str>>(&self, remote_config: &config::RemoteConfig, project: T) -> PathBuf {
    let repo_name: String = Depot::apply_project_renames(remote_config, project.as_ref()) + ".git";
    self.path.join("objects").join(repo_name)
  }

  pub fn refs_mirror<T: AsRef<str>>(&self, remote_config: &config::RemoteConfig, project: T) -> PathBuf {
    let remote: &str = remote_config.name.as_ref();
    let repo_name: String = Depot::apply_project_renames(remote_config, project.as_ref()) + ".git";
    self.path.join("refs").join(remote).join(repo_name)
  }

  pub fn fetch_repo(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    targets: Option<&[String]>,
    fetch_tags: bool,
    depth: Option<i32>,
  ) -> Result<(), Error> {
    ensure!(!project.starts_with('/'), "invalid project path {}", project);
    ensure!(!project.ends_with('/'), "invalid project path {}", project);

    // TODO: Add locking?
    let objects_path = self.objects_mirror(&remote_config, project);
    let repo_url = remote_config.url.to_owned() + project + ".git";

    let objects_repo = Depot::open_or_create_bare_repo(&objects_path)?;
    if objects_repo.find_remote(&remote_config.name).is_ok() {
      objects_repo.remote_set_url(&remote_config.name, &repo_url)?;
    } else {
      objects_repo
        .remote(&remote_config.name, &repo_url)
        .context("failed to create remote")?;
    }

    // Disable automatic `git gc`.
    let mut config = objects_repo
      .config()
      .context(format!("failed to get config for repo at {:?}", objects_path))?;
    config.set_i32("gc.auto", 0).context("failed to set gc.auto")?;

    // Always use git directly.
    // libgit2 sometimes has pathologically bad performance while fetching some repositories.
    // We don't lose that much from shelling out to git to fetch, since we're mostly bound on bandwidth.
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(&objects_path).arg("fetch").arg(&remote_config.name);

    if fetch_tags {
      cmd.arg("--tags");
    } else {
      cmd.arg("--no-tags");
    }

    if let Some(depth) = depth {
      cmd.arg("--depth");
      cmd.arg(depth.to_string());
    }

    if let Some(targets) = targets {
      for target in targets {
        cmd.arg(&target);
      }
    }

    // If tree.rs spawned an ssh ControlMaster, use it.
    cmd.env(
      "GIT_SSH_COMMAND",
      format!("ssh -o 'ControlMaster no' -o 'ControlPath {}'", util::ssh_mux_path()),
    );

    let git_output = cmd.output().context("failed to spawn git fetch")?;
    if !git_output.status.success() {
      bail!("git fetch failed: {}", String::from_utf8_lossy(&git_output.stderr));
    }

    let refs_path = self.refs_mirror(remote_config, project);
    if git2::Repository::open(&refs_path).is_err() {
      Depot::clone_alternates(&objects_path, &refs_path, true).context("failed to clone alternates")?;
    }

    let objects_refs = objects_path.join("refs").join("remotes").join(&remote_config.name);
    let refs_refs = refs_path.join("refs").join("heads");
    Depot::replace_dir(&objects_refs, &refs_refs).context("failed to replace heads")?;

    let objects_tags = objects_path.join("refs").join("tags");
    let refs_tags = refs_path.join("refs").join("tags");
    Depot::replace_dir(&objects_tags, &refs_tags).context("failed to replace tags")?;

    Ok(())
  }

  pub fn clone_repo<T: AsRef<Path>>(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    branch: &str,
    path: T,
  ) -> Result<(), Error> {
    let path: &Path = path.as_ref();

    let repo = Depot::clone_alternates(self.objects_mirror(&remote_config, project), path.to_path_buf(), false)?;
    repo
      .remote(
        &remote_config.name,
        self.refs_mirror(remote_config, project).to_str().unwrap(),
      )
      .context("failed to create remote")?;

    // TODO: The push URL should be based on the review element of the manifest.
    // Projects may fetch from one remote but push to another.
    repo
      .remote_set_pushurl(&remote_config.name, Some(&format!("{}{}", remote_config.url, project)))
      .context("failed to set remote pushurl")?;

    self.update_remote_refs(&remote_config, project, &path)?;

    let head = util::parse_revision(&repo, &remote_config.name, &branch)?;
    repo
      .checkout_tree(&head, None)
      .context(format!("failed to checkout HEAD at {:?}", repo.path()))?;
    repo
      .set_head_detached(head.id())
      .context(format!("failed to set HEAD to {:?}", repo.path()))?;
    Ok(())
  }

  pub fn update_remote_refs<T: AsRef<Path>>(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    path: T,
  ) -> Result<(), Error> {
    let path: &Path = path.as_ref();

    // TODO: Respect <remote alias="...">?
    let mirror_path = self.refs_mirror(remote_config, project);
    let repo_path = Depot::git_path(path);
    let mirror_refs = mirror_path.join("refs").join("heads");
    let repo_refs = repo_path.join("refs").join("remotes").join(&remote_config.name);

    Depot::replace_dir(&mirror_refs, &repo_refs).context("failed to replace remotes")?;

    let mirror_tags = mirror_path.join("refs").join("tags");
    let repo_tags = repo_path.join("refs").join("tags");
    Depot::replace_dir(&mirror_tags, &repo_tags)
  }
}
