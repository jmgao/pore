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

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use failure::Error;
use futures::executor::ThreadPool;
use futures::future;
use futures::task::SpawnExt;

use super::*;
use config::RemoteConfig;
use depot::Depot;
use manifest::FileOperation;

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

#[derive(Clone, Debug)]
struct ProjectInfo {
  project_path: String,
  project_name: String,
  revision: String,
  file_ops: Vec<manifest::FileOperation>,
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

  fn read_manifest(&self) -> Result<Manifest, Error> {
    let manifest_path = self.path.join(".pore").join("manifest.xml");
    let manifest = Manifest::parse_file(&manifest_path).context("failed to read manifest")?;
    Ok(manifest)
  }

  fn collect_manifest_projects(&self, manifest: &Manifest) -> Vec<ProjectInfo> {
    // TODO: This assumes that all projects are under the same remote. Either remove this assumption or assert it?
    let default_revision = manifest
      .default
      .as_ref()
      .and_then(|def| def.revision.clone())
      .unwrap_or_else(|| self.config.branch.clone());

    let projects: Vec<ProjectInfo> = manifest
      .projects
      .iter()
      .map(|(project_path, project)| ProjectInfo {
        project_path: project.path(),
        project_name: project.name.clone(),
        revision: project.revision.clone().unwrap_or_else(|| default_revision.clone()),
        file_ops: project.file_operations.clone(),
      })
      .collect();

    manifest
      .projects
      .iter()
      .map(|(project_path, project)| ProjectInfo {
        project_path: project_path.to_str().expect("project path not UTF-8").into(),
        project_name: project.name.clone(),
        revision: project.revision.clone().unwrap_or_else(|| default_revision.clone()),
        file_ops: project.file_operations.clone(),
      })
      .collect()
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
    checkout: CheckoutType,
  ) -> Result<i32, Error> {
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
          eprintln!("{}: {}", error, error.find_root_cause());
        }
        bail!("failed to sync");
      }
    }

    if checkout == CheckoutType::Checkout {
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
          .spawn_with_handle(future::lazy(move |_| -> Result<Option<(String, Error)>, Error> {
            let project_name = &project_info.project_name;
            let revision = &project_info.revision;

            let result = if project_path.exists() {
              depot.update_remote_refs(&remote_config, &project_name, &project_path)?;
              let repo = git2::Repository::open(&project_path)
                .context(format!("failed to open repository {:?}", project_path))?;

              // There's two things to be concerned about here:
              //  - HEAD might be attached to a branch
              //  - the repo might have uncommitted changes in the index or worktree
              //
              // If HEAD is attached to a branch, we choose to do nothing (for now). At some point, we should probably
              // try to perform the equivalent of `git pull --rebase`.
              //
              // If the repo has uncommitted changes, do a dry-run first, and give up if we have any conflicts.
              if !repo.head_detached()? {
                let head = repo.head()?;
                let branch_name = head.shorthand().unwrap();
                Some((
                  project_info.project_path.to_string(),
                  format_err!("currently on a branch ({})", branch_name),
                ))
              } else {
                let new_head = util::parse_revision(&repo, &remote_config.name, &revision)
                  .context(format!("failed to find revision to sync to in {}", project_name))?;

                let current_head = repo
                  .head()
                  .context(format!("failed to get HEAD in {:?}", project_info.project_path))?;

                // Current head can't be a symbolic reference, because it has to be detached.
                let current_head = current_head
                  .target()
                  .ok_or_else(|| format_err!("failed to get target of HEAD in {:?}", project_info.project_path))?;

                // Only do anything if we're not already on the new HEAD.
                if current_head != new_head.id() {
                  let probe = repo.checkout_tree(&new_head, Some(git2::build::CheckoutBuilder::new().dry_run()));
                  match probe {
                    Ok(()) => {
                      repo.checkout_tree(&new_head, None).context(format!(
                        "failed to checkout to {:?} in {:?}",
                        new_head, project_info.project_path
                      ))?;

                      repo
                        .set_head_detached(new_head.id())
                        .context(format!("failed to detach HEAD in {:?}", project_info.project_path))?;

                      None
                    }
                    Err(err) => Some((project_info.project_path.to_string(), err.into())),
                  }
                } else {
                  None
                }
              }
            } else {
              depot.clone_repo(&remote_config, &project_name, &revision, &project_path)?;
              None
            };

            pb.set_message(&project_info.project_name);
            pb.inc(1);
            Ok(result)
          }))
          .map_err(|err| format_err!("failed to spawn job to checkout repo"))?;
        checkout_handles.push(handle);
      }

      let checkout_handles = pool.run(future::join_all(checkout_handles));
      pb.finish();

      for handle in checkout_handles {
        match handle {
          Ok(None) => {}
          Ok(Some((project_path, error))) => {
            println!("{}", console::style(project_path).bold());
            println!("{}", console::style(format!("  {}", error)).red());
          }

          Err(err) => {
            println!("{}", console::style(err.to_string()).red());
          }
        }
      }

      // Perform linkfiles/copyfiles.
      for project in &projects {
        // src is the target of the link/the file that is copied, and is a relative path from the project.
        // dst is the location of the link/copy that the rule creates, and is a relative path from the tree root.
        for op in &project.file_ops {
          let src_path = self.path.join(&project.project_path).join(&op.src());
          let dst_path = self.path.join(&op.dst());

          if let Err(err) = std::fs::remove_file(&dst_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
              bail!("failed to unlink file {:?}: {}", dst_path, err);
            }
          }

          match op {
            FileOperation::LinkFile { .. } => {
              // repo makes the symlinks as relative symlinks.
              let base = dst_path
                .parent()
                .ok_or_else(|| format_err!("linkfile destination is the root?"))?;
              let target = pathdiff::diff_paths(&src_path, &base)
                .ok_or_else(|| format_err!("failed to calculate path diff for {:?} -> {:?}", dst_path, src_path,))?;
              std::os::unix::fs::symlink(target, dst_path)?;
            }

            FileOperation::CopyFile { .. } => {
              std::fs::copy(src_path, dst_path)?;
            }
          }
        }
      }

      // TODO: Figure out repos that have been removed, and warn about them (or delete them?)
      self.config.projects = projects.iter().map(|p| p.project_path.clone()).collect();
      self.write_config().context("failed to write tree config")?;
    }

    Ok(0)
  }

  pub fn sync(
    &mut self,
    config: &Config,
    mut pool: &mut ThreadPool,
    depot: &Depot,
    sync_under: Option<Vec<&str>>,
    fetch: FetchType,
    checkout: CheckoutType,
  ) -> Result<i32, Error> {
    ensure!(sync_under.is_none(), "limiting sync by path is currently unimplemented");

    // Sync the manifest repo first.
    let remote_config = config.find_remote(&self.config.remote)?;
    let manifest = vec![ProjectInfo {
      project_path: ".pore/manifest".into(),
      project_name: self.config.manifest.clone(),
      revision: self.config.branch.clone(),
      file_ops: Vec::new(),
    }];

    self.sync_repos(
      &mut pool,
      depot,
      &remote_config,
      manifest,
      fetch == FetchType::Fetch,
      checkout,
    )?;

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest);
    self.sync_repos(
      &mut pool,
      depot,
      &remote_config,
      projects,
      fetch != FetchType::NoFetch,
      checkout,
    )?;
    Ok(0)
  }

  pub fn status(&self, config: Config, pool: &mut ThreadPool, status_under: Option<Vec<&str>>) -> Result<i32, Error> {
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
    let mut dirty = false;
    for result in results {
      match result {
        Ok(project_status) => {
          if project_status.branch == None && project_status.files.is_empty() {
            continue;
          }

          dirty = true;

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

    if dirty {
      Ok(1)
    } else {
      Ok(0)
    }
  }

  pub fn start(
    &self,
    config: &Config,
    depot: &Depot,
    remote_config: &RemoteConfig,
    branch_name: &str,
    directory: &Path,
  ) -> Result<i32, Error> {
    let flags = git2::RepositoryOpenFlags::empty();
    let repo = git2::Repository::open_ext(&directory, flags, &self.path).context("failed to find git repository")?;

    // Find the project path.
    let project_path =
      pathdiff::diff_paths(repo.path(), &self.path).ok_or_else(|| format_err!("failed to calculate project name"))?;

    // The path we calculated was the path to the .git directory.
    ensure!(
      project_path.file_name().unwrap().to_str().unwrap() == ".git",
      "unexpected project path: {:?}",
      project_path
    );

    let project_path = project_path
      .parent()
      .ok_or_else(|| format_err!("invalid project path"))?;

    let manifest = self.read_manifest()?;
    let project = manifest
      .projects
      .get(project_path)
      .ok_or_else(|| format_err!("failed to find project {:?}", project_path))?;

    // TODO: This assumes that all projects are under the same remote. Either remove this assumption or assert it?
    let revision = project.revision.clone().unwrap_or_else(|| {
      manifest
        .default
        .and_then(|def| def.revision)
        .unwrap_or_else(|| self.config.branch.clone())
    });

    let object = util::parse_revision(&repo, &remote_config.name, &revision)?;
    let commit = object.peel_to_commit().context("failed to peel object to commit")?;

    let mut branch = repo
      .branch(&branch_name, &commit, false)
      .context(format_err!("failed to create branch {}", branch_name))?;
    branch
      .set_upstream(Some(&format!("{}/{}", remote_config.name, revision)))
      .context("failed to set branch upstream")?;

    repo.checkout_tree(&object, None)?;
    repo
      .set_head(&format!("refs/heads/{}", branch_name))
      .context(format_err!("failed to set HEAD to {}", branch_name))?;

    Ok(0)
  }

  pub fn prune(&self, config: &Config, pool: &mut ThreadPool, depot: &Depot) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest);
    let project_count = projects.len();

    let pb = Arc::new(indicatif::ProgressBar::new(project_count as u64));
    pb.set_style(Tree::progress_bar_style(project_count));
    pb.set_prefix("pruning");

    let tree_root = Arc::new(self.path.clone());

    struct PruneResult {
      project_name: String,
      pruned_branches: Vec<String>,
    }

    let depot = Arc::new(depot.clone());

    let mut handles = Vec::new();
    for project in projects {
      let pb = Arc::clone(&pb);
      let depot = Arc::clone(&depot);
      let tree_root = Arc::clone(&tree_root);
      let handle = pool
        .spawn_with_handle(future::lazy(move |_| -> Result<Option<PruneResult>, Error> {
          let path = tree_root.join(&project.project_path);
          let tree_repo =
            git2::Repository::open(&path).context(format!("failed to open repository {:?}", project.project_path))?;

          let obj_repo_path = depot.objects_mirror(project.project_name);
          let obj_repo = git2::Repository::open_bare(&obj_repo_path)
            .context(format!("failed to open object repository {:?}", obj_repo_path))?;

          let branches = tree_repo.branches(Some(git2::BranchType::Local))?;

          let mut detach = None;
          let mut prunable = Vec::new();
          for branch in branches {
            let (branch, _) = branch?;
            let is_head = branch.is_head();
            let branch_name = branch
              .name()?
              .ok_or_else(|| format_err!("branch has name with invalid UTF-8"))?
              .to_string();
            let commit = branch
              .into_reference()
              .peel_to_commit()
              .context("failed to resolve branch to commit")?;
            let commit_hash = commit.id();

            // Search for this commit in the object repo.
            if let Ok(commit) = obj_repo.find_commit(commit_hash) {
              // Found a prunable commit.
              if is_head {
                detach = Some(commit_hash);
              }
              prunable.push(branch_name);
            }
          }

          if let Some(commit) = detach {
            tree_repo.set_head_detached(commit)?;
          }

          for branch_name in &prunable {
            let mut branch = tree_repo.find_branch(&branch_name, git2::BranchType::Local)?;
            branch
              .delete()
              .context(format!("failed to delete branch {}", branch_name))?;
          }

          pb.set_message(&project.project_path);
          pb.inc(1);

          Ok(Some(PruneResult {
            project_name: project.project_path.clone(),
            pruned_branches: prunable,
          }))
        }))
        .map_err(|err| format_err!("failed to spawn job to fetch"))?;
      handles.push(handle);
    }

    let results = pool.run(future::join_all(handles));
    pb.finish_and_clear();

    let mut errors = Vec::new();
    let mut pruned = Vec::new();

    for result in results {
      match result {
        Ok(Some(result)) => {
          if !result.pruned_branches.is_empty() {
            pruned.push(result)
          }
        }

        Ok(None) => {}
        Err(err) => errors.push(err),
      }
    }

    for error in &errors {
      eprintln!("{}", error);
    }

    for result in pruned {
      println!("{}", console::style(result.project_name).bold());
      for branch in result.pruned_branches {
        println!("  {}", console::style(branch).red());
      }
    }

    if errors.is_empty() {
      Ok(0)
    } else {
      Ok(1)
    }
  }

  pub fn forall(
    &self,
    config: &Config,
    pool: &mut ThreadPool,
    forall_under: Option<Vec<&str>>,
    command: &str,
  ) -> Result<i32, Error> {
    ensure!(
      forall_under.is_none(),
      "limiting forall by path is currently unimplemented"
    );

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest);
    let project_count = projects.len();

    let pb = Arc::new(indicatif::ProgressBar::new(project_count as u64));
    pb.set_style(Tree::progress_bar_style(project_count));
    pb.set_prefix("forall");

    let tree_root = Arc::new(self.path.clone());
    let command = Arc::new(command.to_string());
    let mut handles = Vec::new();

    struct CommandResult {
      project_path: String,
      rc: i32,
      output: Vec<u8>,
    }

    for project in projects {
      let pb = Arc::clone(&pb);
      let tree_root = Arc::clone(&tree_root);
      let command = Arc::clone(&command);

      let handle = pool
        .spawn_with_handle(future::lazy(move |_| -> Result<CommandResult, Error> {
          let path = tree_root.join(&project.project_path);

          let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(command.deref())
            .current_dir(&path)
            .output()?;

          // TODO: Rust's process builder API kinda sucks, there's no way to spawn a process with
          //       stdout and stderr being the same pipe, to order their output chronologically.
          let output = result.stdout;
          let rc = result.status.code().unwrap();

          pb.set_message(&project.project_path);
          pb.inc(1);

          Ok(CommandResult {
            project_path: project.project_path.clone(),
            rc,
            output,
          })
        }))
        .map_err(|err| format_err!("failed to spawn job"))?;

      handles.push(handle);
    }

    let results = pool.run(future::join_all(handles));
    pb.finish_and_clear();

    let mut rc = 0;
    for result in results {
      let result = result?;
      if result.rc == 0 {
        println!("{}", console::style(result.project_path).bold());
      } else {
        println!(
          "{} (rc = {})",
          console::style(result.project_path).red().bold(),
          result.rc
        );
        rc = result.rc;
      }
      let lines = result.output.split(|&c| c == b'\n');
      for line in lines {
        let mut stdout = std::io::stdout();
        stdout.write_all(b"  ")?;
        stdout.write_all(line)?;
        stdout.write_all(b"\n")?;
      }
    }

    Ok(rc)
  }
}
