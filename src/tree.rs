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

use std::fmt;
use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use failure::Error;

use crate::util::*;
use crate::*;

use config::RemoteConfig;
use depot::Depot;
use manifest::FileOperation;
use pool::{ExecutionResult, ExecutionResults, Job, Pool};

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

#[derive(Debug)]
pub enum GroupFilter {
  Include(String),
  Exclude(String),
}

impl GroupFilter {
  fn filter_project(filters: &[GroupFilter], project: &manifest::Project) -> bool {
    if filters.is_empty() {
      return true;
    }

    let groups = project.groups.as_ref().map(|vec| vec.as_slice()).unwrap_or(&[]);

    let mut included = false;
    let mut excluded = false;

    for filter in filters {
      match filter {
        GroupFilter::Include(group) => {
          if groups.contains(group) {
            included = true;
          }
        }

        GroupFilter::Exclude(group) => {
          if groups.contains(group) {
            excluded = true;
          }
        }
      }
    }

    included && !excluded
  }
}

// toml-rs can't serialize enums.
impl serde::Serialize for GroupFilter {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match self {
      GroupFilter::Include(group) => serializer.serialize_str(group),
      GroupFilter::Exclude(group) => serializer.serialize_str(&("-".to_string() + group)),
    }
  }
}

struct GroupFilterVisitor;
impl<'de> serde::de::Visitor<'de> for GroupFilterVisitor {
  type Value = GroupFilter;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("a group")
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    if value.starts_with('-') {
      let string = value[1..].to_string();
      if string.is_empty() {
        Err(E::custom("empty group name"))
      } else {
        Ok(GroupFilter::Exclude(string))
      }
    } else if value.is_empty() {
      Err(E::custom("empty group name"))
    } else {
      Ok(GroupFilter::Include(value.to_string()))
    }
  }
}

impl<'de> serde::Deserialize<'de> for GroupFilter {
  fn deserialize<D>(deserializer: D) -> Result<GroupFilter, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    deserializer.deserialize_str(GroupFilterVisitor)
  }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TreeConfig {
  pub remote: String,
  pub branch: String,
  pub manifest: String,
  pub tags: Vec<String>,

  pub projects: Vec<String>,
  pub group_filters: Option<Vec<GroupFilter>>,
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
  pub fn to_char(&self) -> char {
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
  pub branch: Option<String>,
  pub commit: git2::Oid,
  pub files: Vec<FileStatus>,
}

fn summarize_upload(
  project_name: &str,
  repo: &git2::Repository,
  dst_remote: &str,
  dst_branch_name: &str,
  autosubmit: bool,
) -> Result<(), Error> {
  let head = repo.head().context("could not determine HEAD")?;
  ensure!(head.is_branch(), "expected HEAD to refer to a branch");
  let src_branch = git2::Branch::wrap(head);
  let src_branch_name = src_branch
    .name()
    .context("could not determine branch name for HEAD")?
    .ok_or_else(|| format_err!("branch name is not valid UTF-8"))?;

  let dst_branch = repo
    .find_branch(
      format!("{}/{}", dst_remote, dst_branch_name).as_str(),
      git2::BranchType::Remote,
    )
    .context(format!("could not find branch {}", dst_branch_name))?;

  let (ahead, _behind) = repo
    .graph_ahead_behind(branch_to_commit(&src_branch)?.id(), branch_to_commit(&dst_branch)?.id())
    .context(format!(
      "could not determine state of {} against {}",
      src_branch_name, dst_branch_name
    ))?;

  let src_commit = src_branch
    .get()
    .peel_to_commit()
    .context(format!("failed to get commit for {}", src_branch_name))?;
  let dst_commit = dst_branch
    .get()
    .peel_to_commit()
    .context(format!("failed to get commit for {}/{}", dst_remote, dst_branch_name))?;
  let commits = find_independent_commits(&repo, &src_commit, &dst_commit)?;

  let is_aosp = dst_remote == "aosp";
  let mut lines = vec![format!(
    "Upload {} commit{} from branch {} of project {}",
    ahead,
    if commits.len() == 1 { "" } else { "s" },
    BRANCH_STYLE.apply_to(src_branch_name),
    PROJECT_STYLE.apply_to(project_name),
  )];

  for commit_oid in &commits {
    let commit = repo
      .find_commit(*commit_oid)
      .context(format!("could not find commit matching {}", commit_oid))?;
    lines.push(format!(
      "  {:.10} {}",
      console::style(commit.id()).cyan(),
      commit
        .summary()
        .ok_or_else(|| format_err!("commit message for {} is not valid UTF-8", commit.id()))?
    ))
  }

  if commits.is_empty() {
    bail!("no commits to upload");
  }

  lines.push(format!(
    "to {}{}{} {}[y/N]? ",
    if is_aosp {
      AOSP_REMOTE_STYLE.apply_to(dst_remote)
    } else {
      NON_AOSP_REMOTE_STYLE.apply_to(dst_remote)
    },
    SLASH_STYLE.apply_to("/"),
    BRANCH_STYLE.apply_to(dst_branch_name),
    if autosubmit {
      format!("({}) ", console::style("autosubmit enabled").red().bold())
    } else {
      "".into()
    }
  ));

  print!("{}", lines.join("\n"));
  std::io::stdout().flush()?;

  let line = util::read_line()?;
  match line.as_str() {
    "y" | "Y" => {
      // Print an extra newline to separate gerrit's output from the confirmation prompt.
      println!();
      Ok(())
    }

    _ => Err(format_err!("upload aborted by user")),
  }
}

impl Tree {
  pub fn construct<T: Into<PathBuf>>(
    depot: &Depot,
    path: T,
    remote_config: &RemoteConfig,
    branch: &str,
    group_filters: Vec<GroupFilter>,
    fetch: bool,
  ) -> Result<Tree, Error> {
    let tree_root = path.into();

    // TODO: Add locking?
    util::assert_empty_directory(&tree_root)?;
    let pore_path = tree_root.join(".pore");

    std::fs::create_dir_all(&pore_path).context(format!("failed to create directory {:?}", pore_path))?;

    let manifest_path = pore_path.join("manifest");
    create_symlink("manifest/default.xml", pore_path.join("manifest.xml"))
      .context("failed to create manifest symlink")?;

    if fetch {
      depot.fetch_repo(&remote_config, &remote_config.manifest, &branch, None)?;
    }
    depot.clone_repo(&remote_config, &remote_config.manifest, &branch, &manifest_path)?;

    let tree_config = TreeConfig {
      remote: remote_config.name.clone(),
      branch: branch.into(),
      manifest: remote_config.manifest.clone(),
      tags: Vec::new(),
      projects: Vec::new(),
      group_filters: Some(group_filters),
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

  fn collect_manifest_projects(
    &self,
    manifest: &Manifest,
    under: Option<Vec<&str>>,
  ) -> Result<Vec<ProjectInfo>, Error> {
    // TODO: This assumes that all projects are under the same remote. Either remove this assumption or assert it?
    let default_revision = manifest
      .default
      .as_ref()
      .and_then(|def| def.revision.clone())
      .unwrap_or_else(|| self.config.branch.clone());

    let group_filters = self
      .config
      .group_filters
      .as_ref()
      .map(|vec| vec.as_slice())
      .unwrap_or(&[]);

    // The correctness of this seems dubious if the paths are accessed via symlinks or mount points,
    // but repo doesn't handle this either.
    let tree_root = std::fs::canonicalize(&self.path).context("failed to canonicalize tree path")?;
    let mut paths = Vec::new();
    for path in under.unwrap_or_default() {
      let requested_path =
        std::fs::canonicalize(&path).context(format!("failed to canonicalize requested path '{}'", path))?;
      paths.push(
        pathdiff::diff_paths(&requested_path, &tree_root)
          .ok_or_else(|| format_err!("failed to calculate path diff for {}", path))?,
      );
    }

    Ok(
      manifest
        .projects
        .iter()
        .filter(|(_project_path, project)| GroupFilter::filter_project(&group_filters, &project))
        .filter(|(project_path, _project)| {
          paths.is_empty() || paths.iter().any(|path| Path::new(path).starts_with(project_path))
        })
        .map(|(project_path, project)| ProjectInfo {
          project_path: project_path.to_str().expect("project path not UTF-8").into(),
          project_name: project.name.clone(),
          revision: project.revision.clone().unwrap_or_else(|| default_revision.clone()),
          file_ops: project.file_operations.clone(),
        })
        .collect(),
    )
  }

  fn sync_repos(
    &mut self,
    pool: &mut Pool,
    depot: &Depot,
    remote_config: &RemoteConfig,
    projects: Vec<ProjectInfo>,
    fetch: bool,
    checkout: CheckoutType,
  ) -> Result<i32, Error> {
    let remote_config = Arc::new(remote_config.clone());
    let depot: Arc<Depot> = Arc::new(depot.clone());
    let projects: Vec<Arc<_>> = projects.into_iter().map(Arc::new).collect();

    if fetch {
      let mut job = Job::with_name("fetching");
      for project in &projects {
        let depot = Arc::clone(&depot);
        let remote_config = Arc::clone(&remote_config);
        let project_info = Arc::clone(&project);

        job.add_task(project_info.project_name.clone(), move |_| {
          depot.fetch_repo(&remote_config, &project_info.project_name, &project_info.revision, None)
        });
      }

      let result = pool.execute(job);
      if !result.failed.is_empty() {
        for failure in result.failed {
          eprintln!("{}: {}", failure.name, failure.result.find_root_cause());
        }
        bail!("failed to sync");
      }
    }

    if checkout == CheckoutType::Checkout {
      let mut job = Job::with_name("checkout");
      let tree_root = Arc::new(self.path.clone());

      for project in &projects {
        let depot = Arc::clone(&depot);
        let remote_config = Arc::clone(&remote_config);
        let project_info = Arc::clone(&project);
        let project_path = self.path.join(&project.project_path);
        let tree_root = Arc::clone(&tree_root);

        job.add_task(project.project_name.clone(), move |_| {
          let project_name = &project_info.project_name;
          let revision = &project_info.revision;

          if project_path.exists() {
            depot
              .update_remote_refs(&remote_config, &project_name, &project_path)
              .context("failed to update remote refs")?;

            let repo = git2::Repository::open(&project_path).context("failed to open repository".to_string())?;

            // There's two things to be concerned about here:
            //  - HEAD might be attached to a branch
            //  - the repo might have uncommitted changes in the index or worktree
            //
            // If HEAD is attached to a branch, we choose to do nothing (for now). At some point, we should probably
            // try to perform the equivalent of `git pull --rebase`.
            //
            // If the repo has uncommitted changes, do a dry-run first, and give up if we have any conflicts.
            let head_detached = repo.head_detached().context("failed to check if HEAD is detached")?;
            let current_head = repo.head().context("failed to get HEAD")?;

            if !head_detached {
              let branch_name = current_head
                .shorthand()
                .ok_or_else(|| format_err!("failed to get shorthand for HEAD"))?;
              bail!("currently on a branch ({})", branch_name);
            } else {
              let new_head = util::parse_revision(&repo, &remote_config.name, &revision)
                .context("failed to find revision to sync to".to_string())?;

              // Current head can't be a symbolic reference, because it has to be detached.
              let current_head = current_head
                .target()
                .ok_or_else(|| format_err!("failed to get target of HEAD"))?;

              // Only do anything if we're not already on the new HEAD.
              if current_head != new_head.id() {
                let probe = repo.checkout_tree(&new_head, Some(git2::build::CheckoutBuilder::new().dry_run()));
                if let Err(err) = probe {
                  bail!(err);
                }

                repo
                  .checkout_tree(&new_head, None)
                  .context(format!("failed to checkout to {:?}", new_head))?;

                repo.set_head_detached(new_head.id()).context("failed to detach HEAD")?;
              }
            }
          } else {
            depot.clone_repo(&remote_config, &project_name, &revision, &project_path)?;
          }

          // Set up symlinks to repo hooks.
          let hooks_dir = project_path.join(".git").join("hooks");
          let relpath = pathdiff::diff_paths(&tree_root, &hooks_dir)
            .ok_or_else(|| format_err!("failed to calculate path diff from hooks to tree root"))?
            .join(".pore")
            .join("hooks");
          for filename in hooks::hooks().keys() {
            let target = relpath.join(filename);
            let symlink_path = hooks_dir.join(filename);
            let _ = std::fs::remove_file(&symlink_path);
            create_symlink(&target, &symlink_path)
              .context(format!("failed to create symlink at {:?}", &symlink_path))?;
          }

          Ok(())
        });
      }

      let results = pool.execute(job);
      for failure in &results.failed {
        println!("{}", PROJECT_STYLE.apply_to(&failure.name));
        println!("{}", console::style(format!("  {}", failure.result)).red());
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

              let _ = create_symlink(target, &dst_path)
                .map_err(|err| eprintln!("warning: failed to create symlink at {:?}: {}", dst_path, err));
            }

            FileOperation::CopyFile { .. } => {
              let _ = std::fs::copy(&src_path, &dst_path).map_err(|err| {
                eprintln!(
                  "warning: failed to copy file from {:?} to {:?}: {}",
                  src_path, dst_path, err
                )
              });
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

  pub fn update_hooks(&self) -> Result<(), Error> {
    // Just always do this, since it's cheap.
    let hooks_dir = self.path.join(".pore").join("hooks");
    std::fs::create_dir_all(&hooks_dir).context("failed to create hooks directory")?;
    for (filename, contents) in hooks::hooks() {
      let path = hooks_dir.join(filename);
      let mut file = std::fs::File::create(&path).context(format!("failed to open hook at {:?}", path))?;
      file
        .write_all(contents.as_bytes())
        .context(format!("failed to create hook at {:?}", path))?;
      let mut permissions = file.metadata()?.permissions();
      permissions.set_mode(0o700);
      file.set_permissions(permissions)?;
    }
    Ok(())
  }

  pub fn ensure_repo_compat(&self) -> Result<(), Error> {
    std::fs::create_dir_all(self.path.join(".repo")).context("failed to create dummy .repo dir")?;

    // Create symlinks for manifests and manifest.xml.
    create_symlink("../.pore/manifest", self.path.join(".repo").join("manifests"))?;
    create_symlink("../.pore/manifest.xml", self.path.join(".repo").join("manifest.xml"))?;

    Ok(())
  }

  pub fn sync(
    &mut self,
    config: &Config,
    mut pool: &mut Pool,
    depot: &Depot,
    sync_under: Option<Vec<&str>>,
    fetch: FetchType,
    checkout: CheckoutType,
  ) -> Result<i32, Error> {
    // Sync the manifest repo first.
    let remote_config = config.find_remote(&self.config.remote)?;
    let manifest = vec![ProjectInfo {
      project_path: ".pore/manifest".into(),
      project_name: self.config.manifest.clone(),
      revision: self.config.branch.clone(),
      file_ops: Vec::new(),
    }];

    self.update_hooks()?;

    self.sync_repos(
      &mut pool,
      depot,
      &remote_config,
      manifest,
      fetch == FetchType::Fetch,
      checkout,
    )?;

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, sync_under)?;
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

  pub fn status(
    &self,
    _config: &Config,
    pool: &mut Pool,
    status_under: Option<Vec<&str>>,
  ) -> Result<ExecutionResults<ProjectStatus>, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, status_under)?;
    let projects: Vec<Arc<_>> = projects.into_iter().map(Arc::new).collect();

    let mut job = Job::with_name("status");
    let tree_root = Arc::new(self.path.clone());

    for project in &projects {
      let project = Arc::clone(&project);
      let tree_root = Arc::clone(&tree_root);
      job.add_task(project.project_name.clone(), move |_| -> Result<ProjectStatus, Error> {
        let path = tree_root.join(&project.project_path);
        let repo =
          git2::Repository::open(&path).context(format!("failed to open repository {}", project.project_path))?;

        let head = repo
          .head()
          .context(format!("failed to get HEAD for repository {}", project.project_path))?;
        let commit = head.peel_to_commit().context("failed to peel HEAD to commit")?.id();
        let branch = if head.is_branch() {
          Some(head.shorthand().unwrap().to_string())
        } else {
          None
        };

        let statuses = repo
          .statuses(Some(git2::StatusOptions::new().include_untracked(true)))
          .context(format!("failed to get status of repository {}", project.project_path))?;

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

        Ok(ProjectStatus { branch, commit, files })
      });
    }

    Ok(pool.execute(job))
  }

  pub fn start(
    &self,
    _config: &Config,
    _depot: &Depot,
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
      .context(format!("failed to create branch {}", branch_name))?;
    branch
      .set_upstream(Some(&format!("{}/{}", remote_config.name, revision)))
      .context("failed to set branch upstream")?;

    repo.checkout_tree(&object, None)?;
    repo
      .set_head(&format!("refs/heads/{}", branch_name))
      .context(format!("failed to set HEAD to {}", branch_name))?;

    Ok(0)
  }

  pub fn upload(
    &self,
    config: &Config,
    pool: &mut Pool,
    upload_under: Option<Vec<&str>>,
    current_branch: bool,
    no_verify: bool,
    reviewers: &Vec<String>,
    ccs: &Vec<String>,
    private: bool,
    wip: bool,
    branch_name_as_topic: bool,
    autosubmit: bool,
    presubmit_ready: bool,
  ) -> Result<i32, Error> {
    // TODO: Use a pool for >1, figure out how 0 (all projects) should work.
    ensure!(
      upload_under.as_ref().map_or(false, |v| v.len() == 1),
      "multi-project upload not yet implemented"
    );

    ensure!(
      current_branch,
      "interactive workflow not yet implemented; --cbr is required"
    );

    if !no_verify {
      let result = self.preupload(config, pool, upload_under.clone())?;
      if result != 0 {
        bail!("preupload hooks failed");
      }
    }

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, upload_under)?;
    for project in projects {
      let repo = git2::Repository::open(self.path.join(&project.project_path))
        .context("failed to open repository".to_string())?;

      if repo.head_detached().context("failed to check if HEAD is detached")? {
        bail!("cannot upload from detached HEAD")
      }

      let project_meta = manifest
        .projects
        .get(&PathBuf::from(&project.project_path))
        .ok_or_else(|| format_err!("failed to find project {:?}", project.project_path))?;

      let remote_name = project_meta
        .remote
        .as_ref()
        .or_else(|| manifest.default.as_ref().and_then(|d| d.remote.as_ref()))
        .ok_or_else(|| {
          format_err!(
            "project {:?} did not specify a dest_branch and manifest has no default revision",
            project_meta
          )
        })?;

      let dest_branch = project_meta
        .revision
        .as_ref()
        .or_else(|| manifest.default.as_ref().and_then(|d| d.revision.as_ref()))
        .ok_or_else(|| {
          format_err!(
            "project {:?} did not specify a dest_branch and manifest has no default revision",
            project_meta
          )
        })?;

      if !no_verify {
        // Separate the upload prompt from preupload hook output.
        println!();
      }

      summarize_upload(&project.project_name, &repo, &remote_name, &dest_branch, autosubmit)?;

      // https://gerrit-review.googlesource.com/Documentation/user-upload.html#push_options
      // git push $REMOTE HEAD:refs/for/$UPSTREAM_BRANCH%$OPTIONS
      let ref_spec = format!("HEAD:refs/for/{}", dest_branch);
      let mut cmd = std::process::Command::new("git");
      cmd.current_dir(self.path.join(&project.project_path)).arg("push");

      for reviewer in reviewers {
        cmd.arg("-o");
        cmd.arg(format!("r={}", reviewer));
      }

      for cc in ccs {
        cmd.arg("-o");
        cmd.arg(format!("cc={}", cc));
      }

      if wip {
        cmd.arg("-o");
        cmd.arg("wip");
      }

      if private {
        cmd.arg("-o");
        cmd.arg("private");
      }

      if autosubmit {
        cmd.arg("-o");
        cmd.arg("l=Autosubmit");
      }

      if presubmit_ready {
        cmd.arg("-o");
        cmd.arg("l=Presubmit-Ready");
      }

      if branch_name_as_topic {
        let head = repo.head().context("could not determine HEAD")?;
        ensure!(head.is_branch(), "expected HEAD to refer to a branch");
        let branch = git2::Branch::wrap(head);
        cmd.arg("-o");
        cmd.arg(format!("topic={}", branch_name(&branch)?));
      }

      cmd.arg(remote_name).arg(ref_spec);

      let git_output = cmd.output().context("failed to spawn git push")?;
      println!("{}", String::from_utf8_lossy(&git_output.stderr));
    }

    Ok(0)
  }

  pub fn prune(
    &self,
    _config: &Config,
    pool: &mut Pool,
    depot: &Depot,
    prune_under: Option<Vec<&str>>,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, prune_under)?;

    let mut job = Job::with_name("pruning");
    let tree_root = Arc::new(self.path.clone());

    struct PruneResult {
      pruned_branches: Vec<String>,
    }

    let depot = Arc::new(depot.clone());
    for project in projects {
      let depot = Arc::clone(&depot);
      let tree_root = Arc::clone(&tree_root);
      job.add_task(project.project_name.clone(), move |_| -> Result<PruneResult, Error> {
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
          if obj_repo.find_commit(commit_hash).is_ok() {
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

        Ok(PruneResult {
          pruned_branches: prunable,
        })
      });
    }

    let results = pool.execute(job);
    let mut failed = false;
    for error in results.failed {
      eprintln!("{}: {}", error.name, error.result);
      failed = true;
    }

    for result in results.successful {
      if !result.result.pruned_branches.is_empty() {
        println!("{}", PROJECT_STYLE.apply_to(result.name));
        for branch in result.result.pruned_branches {
          println!("  {}", console::style(branch).red());
        }
      }
    }

    if !failed {
      Ok(0)
    } else {
      Ok(1)
    }
  }

  pub fn rebase(
    &self,
    pool: &mut Pool,
    interactive: bool,
    autosquash: bool,
    rebase_under: Option<Vec<&str>>,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, rebase_under)?;

    if interactive {
      ensure!(
        projects.len() == 1,
        "interactive rebase not possible with multiple projects"
      );
      self.interactive_rebase(manifest, autosquash, projects[0].clone())
    } else {
      ensure!(!autosquash, "--autosquash is only valid with --interactive");
      self.non_interactive_rebase(pool, projects)
    }
  }

  fn interactive_rebase(&self, manifest: Manifest, autosquash: bool, project: ProjectInfo) -> Result<i32, Error> {
    let project_meta = manifest
      .projects
      .get(&PathBuf::from(&project.project_path))
      .ok_or_else(|| format_err!("failed to find project {:?}", project.project_path))?;

    let remote_name = project_meta
      .remote
      .as_ref()
      .or_else(|| manifest.default.as_ref().and_then(|d| d.remote.as_ref()))
      .ok_or_else(|| {
        format_err!(
          "project {:?} did not specify a dest_branch and manifest has no default revision",
          project_meta
        )
      })?;

    let dest_branch = project_meta
      .revision
      .as_ref()
      .or_else(|| manifest.default.as_ref().and_then(|d| d.revision.as_ref()))
      .ok_or_else(|| {
        format_err!(
          "project {:?} did not specify a dest_branch and manifest has no default revision",
          project_meta
        )
      })?;

    let mut cmd = std::process::Command::new("git");
    cmd
      .current_dir(self.path.join(&project.project_path))
      .arg("rebase")
      .arg("--interactive");
    if autosquash {
      cmd.arg("--autosquash");
    }

    cmd.arg(format!("{}/{}", remote_name, dest_branch));

    cmd
      .spawn()
      .context("failed to spawn git rebase")?
      .wait()
      .context("failed to wait on git rebase")?;

    Ok(0)
  }

  fn non_interactive_rebase(&self, _pool: &mut Pool, projects: Vec<ProjectInfo>) -> Result<i32, Error> {
    Err(format_err!("not interactive rebase is not implemented"))
  }

  pub fn forall(
    &self,
    _config: &Config,
    pool: &mut Pool,
    forall_under: Option<Vec<&str>>,
    command: &str,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(&manifest, forall_under)?;

    let mut job = Job::with_name("forall");
    let tree_root = Arc::new(self.path.clone());
    let command = Arc::new(command.to_string());

    struct CommandResult {
      rc: i32,
      output: Vec<u8>,
    }

    for project in projects {
      let tree_root = Arc::clone(&tree_root);
      let command = Arc::clone(&command);

      job.add_task(project.project_name.clone(), move |_| -> Result<CommandResult, Error> {
        let path = tree_root.join(&project.project_path);
        let rel_to_root = pathdiff::diff_paths(&tree_root, &path)
          .ok_or_else(|| format_err!("failed to calculate relative path to root"))?;

        let result = std::process::Command::new("sh")
          .arg("-c")
          .arg(command.deref())
          .env("PORE_ROOT", tree_root.as_os_str())
          .env("PORE_ROOT_REL", rel_to_root.as_os_str())
          .env("REPO_PROJECT", project.project_name)
          .current_dir(&path)
          .output()?;

        // TODO: Rust's process builder API kinda sucks, there's no way to spawn a process with
        //       stdout and stderr being the same pipe, to order their output chronologically.
        let output = result.stdout;
        let rc = result.status.code().unwrap();

        Ok(CommandResult { rc, output })
      });
    }

    let results = pool.execute(job);

    let mut rc = 0;
    for result in results.successful {
      let project_name = result.name;
      let result = result.result;
      if result.rc == 0 {
        println!("{}", PROJECT_STYLE.apply_to(project_name));
      } else {
        println!("{} (rc = {})", console::style(project_name).red().bold(), result.rc);
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

    if !results.failed.is_empty() {
      for result in results.failed {
        eprintln!("{}: {}", console::style(result.name).red().bold(), result.result);
        rc = 1;
      }
    }
    Ok(rc)
  }

  pub fn preupload(&self, config: &Config, pool: &mut Pool, under: Option<Vec<&str>>) -> Result<i32, Error> {
    let manifest = self.read_manifest().context("failed to read manifest")?;
    let remote_config = config
      .find_remote(&self.config.remote)
      .context(format!("failed to find remote {}", self.config.remote))?;
    let projects = self
      .collect_manifest_projects(&manifest, under)
      .context("failed to collect manifest projects")?;

    let hook_project_name = match manifest.repo_hooks {
      Some(hooks) => {
        // TODO: Bail out if pre-upload isn't in enabled-list.
        match hooks.in_project {
          Some(project) => project,
          None => return Ok(0),
        }
      }

      None => {
        eprintln!("no preupload hooks configured");
        return Ok(0);
      }
    };

    // repo-hooks looks for a .repo directory, so make one.
    self
      .ensure_repo_compat()
      .context("failed to create repo compatibility files")?;

    let mut hook_project = None;
    for (_, project) in &manifest.projects {
      if project.name == hook_project_name {
        hook_project = Some(project);
        break;
      }
    }

    let hook_project = hook_project.ok_or_else(|| format_err!("couldn't find hook project '{}'", hook_project_name))?;
    let hook_project_path = hook_project
      .path
      .clone()
      .ok_or_else(|| format_err!("no path for repo-hook project '{}'", hook_project_name))?;

    let mut job = Job::with_name("preupload");

    let remote_config = Arc::new(remote_config);
    let hook_path = Arc::new(self.path.join(&hook_project_path).join("pre-upload.py"));

    struct PresubmitResult {
      rc: i32,
      output: Vec<u8>,
    }

    for project in projects {
      let remote_config = Arc::clone(&remote_config);
      let project_path = self.path.join(&project.project_path);
      let hook_path = Arc::clone(&hook_path);

      job.add_task(
        project.project_path.clone(),
        move |_| -> Result<PresubmitResult, Error> {
          let repo = git2::Repository::open(project_path.deref()).context("failed to open repository".to_string())?;

          let head = repo.head().context("could not determine HEAD")?;
          let head_branch = git2::Branch::wrap(head);
          if head_branch.name().is_err() {
            // repo-hooks need to be on a branch.
            return Ok(PresubmitResult {
              rc: 0,
              output: Vec::new(),
            });
          }

          let current_head = repo
            .head()
            .context("failed to get HEAD")?
            .peel_to_commit()
            .context("failed to peel HEAD to commit")?;

          let upstream_object = util::parse_revision(&repo, &remote_config.name, &project.revision)?;
          let upstream_commit = upstream_object
            .peel_to_commit()
            .context("failed to peel upstream object to commit")?;

          let commits = util::find_independent_commits(&repo, &current_head, &upstream_commit)?;
          if commits.is_empty() {
            Ok(PresubmitResult {
              rc: 0,
              output: Vec::new(),
            })
          } else {
            let mut cmd = std::process::Command::new(hook_path.deref().clone());
            cmd.current_dir(&project_path);
            cmd.arg("--project").arg(&project_path);

            for commit in commits {
              cmd.arg(format!("{}", commit));
            }

            let result = cmd.output()?;

            // TODO: Rust's process builder API kinda sucks, there's no way to spawn a process with
            //       stdout and stderr being the same pipe, to order their output chronologically.
            let mut output = result.stdout;
            if !result.stderr.is_empty() {
              if !output.is_empty() {
                output.push('\n' as u8);
              }
              output.extend(&result.stderr);
            }

            let rc = result.status.code().unwrap();

            Ok(PresubmitResult { rc, output })
          }
        },
      );
    }

    let results = pool.execute(job);

    let mut rc = 0;
    let outputs: Vec<Vec<u8>> = results
      .successful
      .iter()
      .map(|result| {
        let mut output = Vec::new();
        let project_name = &result.name;
        let result = &result.result;
        if result.rc != 0 {
          writeln!(
            output,
            "{} (rc = {}):",
            console::style(&project_name).red().bold(),
            result.rc
          )
          .unwrap();
          rc = result.rc;
        } else {
          if !result.output.is_empty() {
            writeln!(output, "{}:", console::style(&project_name).green().bold()).unwrap();
          }
        }
        output.write_all(&result.output).unwrap();
        output
      })
      .filter(|vec| !vec.is_empty())
      .collect();
    let _ = std::io::stdout().write_all(&outputs.join(&('\n' as u8)));

    for failure in results.failed {
      println!(
        "{}: {}: {}",
        failure.name,
        console::style("failed to run preupload hook").red().bold(),
        failure.result
      );
      rc = 1;
    }

    Ok(rc)
  }

  pub fn generate_manifest(&self, config: &Config, pool: &mut Pool, output: Option<&str>) -> Result<i32, Error> {
    let status = self.status(config, pool, None)?;
    let mut bail = false;
    for failed in status.failed {
      eprintln!("failed to get status on {}: {}", failed.name, failed.result);
      bail = true;
    }
    if bail {
      return Ok(1);
    }

    let mut manifest = self.read_manifest()?;
    for project in status.successful {
      let ExecutionResult {
        name: project_name,
        result: project_status,
      } = project;

      // TODO: Check to see if the commit we have is available on upstream.
      if !project_status.files.is_empty() {
        eprintln!(
          "warning: untracked changes in project {}",
          PROJECT_STYLE.apply_to(&project_name)
        );
      }
      let mut manifest_project = manifest
        .projects
        .iter_mut()
        .find(|(_k, v)| v.name == project_name)
        .ok_or_else(|| format_err!("failed to find project {} in manifest", project_name))?;
      manifest_project.1.revision = Some(format!("{}", project_status.commit));
    }

    let output: Box<dyn Write> = match output {
      Some(path) => Box::new(std::fs::File::create(&path)?),
      None => Box::new(std::io::stdout()),
    };

    manifest.serialize(output).context("failed to serialize manifest")?;
    Ok(0)
  }
}
