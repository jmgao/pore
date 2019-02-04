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
use std::sync::Arc;

use failure::Error;
use futures::executor::ThreadPool;
use futures::future;
use futures::task::SpawnExt;

use super::*;
use config::RemoteConfig;
use depot::Depot;

pub struct Tree {
  pub path: PathBuf,
  pub config: TreeConfig,
}

#[derive(Copy, Clone, PartialEq)]
pub enum FetchType {
  /// Fetch the manifest, then fetch everything.
  Fetch,

  /// Fetch everything but the manifest.
  FetchExceptManifest,

  /// Don't fetch anything, use only the local cache.
  NoFetch,
}

#[derive(Copy, Clone, PartialEq)]
pub enum CheckoutType {
  Checkout,
  NoCheckout,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TreeConfig {
  pub remote: String,
  pub branch: String,
  pub manifest: String,
  pub tags: Vec<String>,

  pub projects: Vec<String>,
}

#[derive(Clone)]
struct ProjectInfo {
  project_path: String,
  project_name: String,
  revision: String,
}

#[derive(Debug, PartialEq)]
pub enum FileState {
  New,
  Modified,
  Deleted,
  Renamed,
  TypeChange,
  Unchanged,
}

impl FileState {
  fn to_char(&self) -> char {
    match self {
      FileState::New => 'a',
      FileState::Modified => 'm',
      FileState::Deleted => 'd',
      FileState::Renamed => 'r',
      FileState::TypeChange => 'd',
      FileState::Unchanged => '-',
    }
  }
}

#[derive(Debug)]
pub struct FileStatus {
  pub filename: String,
  pub index: FileState,
  pub worktree: FileState,
}

#[derive(Debug)]
pub struct ProjectStatus {
  project_name: String,
  branch: Option<String>,
  files: Vec<FileStatus>,
}

impl Tree {
  pub fn construct<T: Into<PathBuf>>(
    depot: &Depot,
    path: T,
    remote_config: &RemoteConfig,
    branch: &str,
    fetch: bool,
  ) -> Result<Tree, Error> {
    let tree_root = path.into();

    // TODO: Add locking?
    util::assert_empty_directory(&tree_root)?;
    let pore_path = tree_root.join(".pore");
    let remote = &remote_config.name;

    std::fs::create_dir_all(&pore_path).context(format!("failed to create directory {:?}", pore_path))?;

    let manifest_path = pore_path.join("manifest");
    symlink("manifest/default.xml", pore_path.join("manifest.xml")).context("failed to create manifest symlink")?;

    if fetch {
      depot.fetch_repo(&remote_config, &remote_config.manifest, &branch, None, None)?;
    }
    depot.clone_repo(&remote_config, &remote_config.manifest, &branch, &manifest_path)?;

    let tree_config = TreeConfig {
      remote: remote_config.name.clone(),
      branch: branch.into(),
      manifest: remote_config.manifest.clone(),
      tags: Vec::new(),
      projects: Vec::new(),
    };

    let tree = Tree {
      path: tree_root.clone(),
      config: tree_config,
    };

    tree.write_config()?;
    Ok(tree)
  }

  pub fn from_path<T: Into<PathBuf>>(path: T) -> Result<Tree, Error> {
    let path: PathBuf = path.into();
    if path.join(".pore").exists() {
      let config = Tree::read_config(&path)?;
      Ok(Tree { path, config })
    } else {
      Err(format_err!("failed to find tree at {:?}", path))
    }
  }

  pub fn find_from_path<T: Into<PathBuf>>(path: T) -> Result<Tree, Error> {
    let original_path: PathBuf = path.into();
    let mut path: PathBuf = original_path.clone();
    while !path.join(".pore").exists() {
      if let Some(parent) = path.parent() {
        path = parent.to_path_buf();
      } else {
        bail!("failed to find tree enclosing {:?}", original_path);
      }
    }

    Tree::from_path(path)
  }

  fn write_config(&self) -> Result<(), Error> {
    let text = toml::to_string_pretty(&self.config).context("failed to serialize tree config")?;
    Ok(std::fs::write(self.path.join(".pore").join("tree.toml"), text).context("failed to write tree config")?)
  }

  fn read_config<T: AsRef<Path>>(tree_root: T) -> Result<TreeConfig, Error> {
    let tree_root: &Path = tree_root.as_ref();
    let text =
      std::fs::read_to_string(tree_root.join(".pore").join("tree.toml")).context("failed to read tree config")?;
    Ok(toml::from_str(&text).context("failed to deserialize tree config")?)
  }

  fn progress_bar_style(project_count: usize) -> indicatif::ProgressStyle {
    let project_count_digits = project_count.to_string().len();
    let count = "{pos:>".to_owned() + &(6 - project_count_digits).to_string() + "}/{len}";
    let template = "[{elapsed_precise}] {prefix} ".to_owned() + &count + " {bar:40.cyan/blue}: {msg}";
    indicatif::ProgressStyle::default_bar()
      .template(&template)
      .progress_chars("##-")
  }

  fn sync_repos(
    &mut self,
    pool: &mut ThreadPool,
    depot: &Depot,
    remote_config: &RemoteConfig,
    projects: Vec<ProjectInfo>,
    fetch: bool,
  ) -> Result<(), Error> {
    let remote_config = Arc::new(remote_config.clone());
    let depot: Arc<Depot> = Arc::new(depot.clone());
    let projects: Vec<Arc<_>> = projects.into_iter().map(Arc::new).collect();
    let project_count = projects.len();
    let style = Tree::progress_bar_style(project_count);

    if fetch {
      let pb = Arc::new(indicatif::ProgressBar::new(project_count as u64));
      pb.set_style(style.clone());
      pb.set_prefix("fetching");
      pb.enable_steady_tick(1000);
      let mut handles = Vec::new();
      for project in &projects {
        let depot = Arc::clone(&depot);
        let remote_config = Arc::clone(&remote_config);
        let project_info = Arc::clone(&project);
        let pb = Arc::clone(&pb);

        let handle = pool
          .spawn_with_handle(future::lazy(move |_| {
            let result = depot.fetch_repo(
              &remote_config,
              &project_info.project_name,
              &project_info.revision,
              None,
              None,
            );
            pb.set_message(&project_info.project_name);
            pb.inc(1);
            result
          }))
          .map_err(|err| format_err!("failed to spawn job to fetch"))?;
        handles.push(handle);
      }

      let handles = pool.run(future::join_all(handles));
      pb.finish();

      let errors: Vec<_> = handles
        .into_iter()
        .filter(Result::is_err)
        .map(Result::unwrap_err)
        .collect();

      if !errors.is_empty() {
        for error in errors {
          eprintln!("{}", error);
        }
        bail!("failed to sync");
      }
    }

    let pb = Arc::new(indicatif::ProgressBar::new(project_count as u64));
    pb.set_style(style.clone());
    pb.set_prefix("checkout");
    pb.enable_steady_tick(1000);
    let mut checkout_handles = Vec::new();
    for project in &projects {
      let depot = Arc::clone(&depot);
      let remote_config = Arc::clone(&remote_config);
      let project_info = Arc::clone(&project);
      let project_path = self.path.join(&project.project_path);
      let pb = Arc::clone(&pb);

      let handle = pool
        .spawn_with_handle(future::lazy(move |_| -> Result<(), Error> {
          let project_name = &project_info.project_name;
          let revision = &project_info.revision;

          if project_path.exists() {
            depot.update_remote_refs(&remote_config, &project_name, &project_path)?;
            let repo =
              git2::Repository::open(&project_path).context(format!("failed to open repository {:?}", project_path))?;
            Depot::checkout_repo(&repo, &remote_config.name, &revision)?;
          } else {
            depot.clone_repo(&remote_config, &project_name, &revision, &project_path)?;
          }

          pb.set_message(&project_info.project_name);
          pb.inc(1);
          Ok(())
        }))
        .map_err(|err| format_err!("failed to spawn job to checkout repo"))?;
      checkout_handles.push(handle);
    }

    let checkout_handles = pool.run(future::join_all(checkout_handles));
    pb.finish();

    let errors: Vec<_> = checkout_handles
      .into_iter()
      .filter(Result::is_err)
      .map(Result::unwrap_err)
      .collect();
    if !errors.is_empty() {
      for error in &errors {
        eprintln!("{}", error);
      }
      bail!("failed to checkout");
    }

    // TODO: Figure out repos that have been removed, and warn about them (or delete them?)
    self.config.projects = projects.iter().map(|p| p.project_path.clone()).collect();
    self.write_config().context("failed to write tree config")?;

    Ok(())
  }

  pub fn sync(
    &mut self,
    config: &Config,
    mut pool: &mut ThreadPool,
    depot: &Depot,
    sync_under: Option<Vec<&str>>,
    fetch: FetchType,
    checkout: CheckoutType,
  ) -> Result<(), Error> {
    ensure!(sync_under.is_none(), "limiting sync by path is currently unimplemented");

    // Sync the manifest repo first.
    let remote_config = config.find_remote(&self.config.remote)?;
    let manifest = vec![ProjectInfo {
      project_path: ".pore/manifest".into(),
      project_name: self.config.manifest.clone(),
      revision: self.config.branch.clone(),
    }];

    self.sync_repos(&mut pool, depot, &remote_config, manifest, fetch == FetchType::Fetch)?;

    let manifest_path = self.path.join(".pore").join("manifest.xml");
    let manifest = Manifest::parse_file(&manifest_path).context("failed to read manifest")?;

    // TODO: This assumes that all projects are under the same remote. Either remove this assumption or assert it?
    let default_revision = manifest
      .default
      .and_then(|def| def.revision)
      .unwrap_or_else(|| self.config.branch.clone());

    let projects: Vec<ProjectInfo> = manifest
      .projects
      .iter()
      .map(|project| ProjectInfo {
        project_path: project.path(),
        project_name: project.name.clone(),
        revision: project.revision.clone().unwrap_or_else(|| default_revision.clone()),
      })
      .collect();

    self.sync_repos(&mut pool, depot, &remote_config, projects, fetch != FetchType::NoFetch)?;

    Ok(())
  }

  pub fn status(&self, config: Config, pool: &mut ThreadPool, status_under: Option<Vec<&str>>) -> Result<(), Error> {
    ensure!(
      status_under.is_none(),
      "limiting status by path is currently unimplemented"
    );
    let projects = self.config.projects.clone();
    let project_count = projects.len();
    let style = Tree::progress_bar_style(project_count);

    let pb = Arc::new(indicatif::ProgressBar::new(project_count as u64));
    pb.set_style(Tree::progress_bar_style(project_count));
    pb.set_prefix("git status");

    let tree_root = Arc::new(self.path.clone());

    let mut handles = Vec::new();
    for project in projects {
      let pb = Arc::clone(&pb);
      let tree_root = Arc::clone(&tree_root);
      let handle = pool
        .spawn_with_handle(future::lazy(move |_| -> Result<ProjectStatus, Error> {
          let path = tree_root.join(&project);
          let repo = git2::Repository::open(&path).context(format!("failed to open repository {}", project))?;

          let head = repo
            .head()
            .context(format!("failed to get HEAD for repository {}", project))?;
          let branch = if head.is_branch() {
            Some(head.shorthand().unwrap().to_string())
          } else {
            None
          };

          let statuses = repo
            .statuses(Some(git2::StatusOptions::new().include_untracked(true)))
            .context(format!("failed to get status of repository {}", project))?;

          pb.set_message(&project.to_string());
          pb.inc(1);

          let files: Vec<FileStatus> = statuses
            .iter()
            .map(|status| {
              let filename = status.path().unwrap_or("???").to_string();
              let flags = status.status();
              let index = if flags.is_index_new() {
                FileState::New
              } else if flags.is_index_modified() {
                FileState::Modified
              } else if flags.is_index_deleted() {
                FileState::Deleted
              } else if flags.is_index_renamed() {
                FileState::Renamed
              } else if flags.is_index_typechange() {
                FileState::TypeChange
              } else {
                FileState::Unchanged
              };

              let worktree = if flags.is_wt_new() {
                FileState::New
              } else if flags.is_wt_modified() {
                FileState::Modified
              } else if flags.is_wt_deleted() {
                FileState::Deleted
              } else if flags.is_wt_renamed() {
                FileState::Renamed
              } else if flags.is_wt_typechange() {
                FileState::TypeChange
              } else {
                FileState::Unchanged
              };

              FileStatus {
                filename,
                index,
                worktree,
              }
            })
            .collect();

          Ok(ProjectStatus {
            project_name: project.clone(),
            branch,
            files,
          })
        }))
        .map_err(|err| format_err!("failed to spawn job to run git status"))?;
      handles.push(handle);
    }

    let results = pool.run(future::join_all(handles));
    pb.finish_and_clear();

    // TODO: Should we be printing the results here, or out in main.rs?
    let mut errors = Vec::new();
    for result in results {
      match result {
        Ok(project_status) => {
          if project_status.branch == None && project_status.files.is_empty() {
            continue;
          }

          let project_line = console::style(format!("project {:64}", project_status.project_name)).bold();
          let branch = match project_status.branch {
            Some(branch) => console::style(format!("branch {}", branch)).bold(),
            None => console::style("(*** NO BRANCH ***)".to_string()).red(),
          };
          println!("{}{}", project_line, branch);

          for file in project_status.files {
            let index = file.index.to_char().to_uppercase().to_string();
            let worktree = file.worktree.to_char();
            let mut line = console::style(format!(" {}{}     {}", index, worktree, file.filename));
            if file.worktree != FileState::Unchanged {
              line = line.red();
            } else {
              line = line.green();
            }

            println!("{}", line)
          }
        }

        Err(err) => errors.push(err),
      }
    }

    if !errors.is_empty() {
      for error in errors {
        eprintln!("{}", error);
      }
      bail!("failed to git status");
    }

    Ok(())
  }
}
