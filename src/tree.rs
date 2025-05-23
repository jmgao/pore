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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::iter::FromIterator as _;
use std::ops::Deref as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Error};
use progpool::{ExecutionResult, ExecutionResults, Job, Pool};
use url::Url;
use walkdir::WalkDir;

use crate::config::{Config, ManifestConfig, RemoteConfig};
use crate::depot::Depot;
use crate::hooks;
use crate::manifest::{self, FileOperation, Manifest};
use crate::util::{self, create_symlink, ssh_mux_path};
use crate::{aosp_remote_style, branch_style, non_aosp_remote_style, project_style, slash_style};

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

#[derive(Clone, PartialEq, Eq)]
pub enum FetchTarget {
  /// Fetch whatever each project has specified as its upstream revision.
  Upstream,

  /// Fetch specific revisions for the project.
  Specific(HashSet<String>),

  /// Fetch everything.
  All,
}

impl FetchTarget {
  fn empty() -> FetchTarget {
    FetchTarget::Specific(HashSet::new())
  }

  fn reify<T: Into<String>>(&self, upstream_rev: T) -> FetchTarget {
    match self {
      FetchTarget::Upstream => {
        let mut set = HashSet::new();
        set.insert(upstream_rev.into());
        FetchTarget::Specific(set)
      }

      x => x.clone(),
    }
  }

  fn merge(&mut self, other: &FetchTarget) {
    if *self == FetchTarget::Upstream || *other == FetchTarget::Upstream {
      panic!("unreified FetchTarget");
    }

    if *self == FetchTarget::All || *other == FetchTarget::All {
      *self = FetchTarget::All;
      return;
    }

    match self {
      FetchTarget::Specific(ref mut lhs) => match other {
        FetchTarget::Specific(ref rhs) => {
          *lhs = lhs.union(rhs).cloned().collect();
        }

        _ => unreachable!(),
      },

      _ => unreachable!(),
    }
  }
}

#[derive(Copy, Clone, PartialEq)]
pub enum CheckoutType {
  Checkout,
  RefsOnly,
  NoCheckout,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupFilter {
  Include(String),
  Exclude(String),
}

#[derive(PartialEq)]
enum FilterResult {
  Unspecified,
  Include,
  Exclude,
}

impl FilterResult {
  fn combine(&mut self, rhs: FilterResult) {
    if rhs != FilterResult::Unspecified {
      *self = rhs;
    }
  }
}

impl GroupFilter {
  fn apply_filter(filters: &[GroupFilter], groups: &[String], default: FilterResult) -> FilterResult {
    let mut result = default;

    for filter in filters {
      match filter {
        GroupFilter::Include(group) => {
          if groups.contains(group) {
            result = FilterResult::Include;
          }
        }

        GroupFilter::Exclude(group) => {
          if groups.contains(group) {
            result = FilterResult::Exclude;
          }
        }
      }
    }

    result
  }

  fn filter_manifest(manifest_filters: &[GroupFilter], groups: &[String]) -> FilterResult {
    // If groups are unspecified, it defaults to including all projects that aren't marked notdefault.
    let mut filter_result;
    let mut filters;

    if manifest_filters.is_empty() {
      filter_result = FilterResult::Include;
      filters = vec![GroupFilter::Exclude("notdefault".into())];
    } else {
      filter_result = FilterResult::Exclude;
      filters = Vec::new();

      let default = GroupFilter::Include("default".into());
      for filter in manifest_filters {
        if filter == &default {
          filter_result = FilterResult::Include;
          filters.push(GroupFilter::Exclude("notdefault".into()));
        } else {
          filters.push(filter.clone());
        }
      }
    }

    filter_result.combine(GroupFilter::apply_filter(&filters, groups, FilterResult::Unspecified));
    filter_result
  }

  fn filter_project(
    manifest_filters: &[GroupFilter],
    additional_filters: Option<&[GroupFilter]>,
    project: &manifest::Project,
  ) -> bool {
    let groups = project.groups.as_deref().unwrap_or(&[]);
    let mut result = GroupFilter::filter_manifest(manifest_filters, groups);
    if result == FilterResult::Exclude {
      // Not synced, skip.
      return false;
    }

    if let Some(additional_filters) = additional_filters {
      // If we're given filters, reject projects that aren't included.
      result.combine(GroupFilter::apply_filter(
        additional_filters,
        groups,
        FilterResult::Exclude,
      ));
    }
    result == FilterResult::Include
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
impl serde::de::Visitor<'_> for GroupFilterVisitor {
  type Value = GroupFilter;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("a group")
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: serde::de::Error,
  {
    if let Some(value) = value.strip_prefix('-') {
      if value.is_empty() {
        Err(E::custom("empty group name"))
      } else {
        Ok(GroupFilter::Exclude(value.to_string()))
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
pub struct ProjectInfo {
  pub project_path: String,
  pub project_name: String,
  pub remote: String,
  pub revision: String,
  pub file_ops: Vec<manifest::FileOperation>,
  pub manifest_project: bool,
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
  #[allow(dead_code)]
  pub name: String,
  pub path: String,
  pub branch: Option<String>,
  pub commit: git2::Oid,
  pub commit_summary: Option<String>,
  pub files: Vec<FileStatus>,
  pub ahead: usize,
  pub behind: usize,
}

#[derive(Debug)]
pub struct ProjectBranchStatus {
  pub name: String,
  pub branches: Vec<BranchStatus>,
}

#[derive(Debug)]
pub struct BranchStatus {
  pub name: String,
  pub is_head: bool,
}

pub struct BranchInfo<'repo> {
  pub name: String,
  pub commit: git2::Commit<'repo>,
}

impl<'repo> BranchInfo<'repo> {
  fn from_branch(branch: git2::Branch) -> Result<BranchInfo, Error> {
    let name = branch
      .name()
      .context("could not determine branch name")?
      .ok_or_else(|| format_err!("branch name is not valid UTF-8"))?;
    let commit = branch
      .get()
      .peel_to_commit()
      .with_context(|| format!("failed to get commit for {}", name))?;

    Ok(BranchInfo {
      name: name.to_string(),
      commit,
    })
  }

  fn from_ref(reference: git2::Reference) -> Result<BranchInfo, Error> {
    ensure!(reference.is_branch(), "expected reference to refer to a branch");
    BranchInfo::from_branch(git2::Branch::wrap(reference))
  }

  fn from_branch_name(
    repo: &'repo git2::Repository,
    branch_name: &str,
    branch_type: git2::BranchType,
  ) -> Result<BranchInfo<'repo>, Error> {
    BranchInfo::from_branch(repo.find_branch(branch_name, branch_type).with_context(|| {
      format!(
        "could not find {} branch {}",
        match branch_type {
          git2::BranchType::Local => "local",
          git2::BranchType::Remote => "remote",
        },
        branch_name
      )
    })?)
  }

  fn name_without_remote(&self) -> &str {
    let components: Vec<&str> = self.name.split('/').collect();
    assert!(components.len() == 1 || components.len() == 2);
    components.last().unwrap()
  }
}

struct CommitSummary {
  pub id: git2::Oid,
  pub summary: String,
}

struct UploadSummary {
  pub project_path: String,
  pub src_branch: String,
  pub dest_remote: String,
  pub dest_branch: String,
  pub commit_summaries: Vec<CommitSummary>,
}

fn confirm_upload(upload_summaries: Vec<&UploadSummary>, autosubmit: bool) -> Result<(), Error> {
  let mut lines: Vec<String> = Vec::new();
  for upload_summary in upload_summaries {
    let is_aosp = upload_summary.dest_remote == "aosp";
    lines.push(format!(
      "{}: {} commit{} from branch {} to {}{}{}",
      project_style().apply_to(&upload_summary.project_path),
      upload_summary.commit_summaries.len(),
      if upload_summary.commit_summaries.len() == 1 {
        ""
      } else {
        "s"
      },
      branch_style().apply_to(&upload_summary.src_branch),
      if is_aosp {
        aosp_remote_style().apply_to(&upload_summary.dest_remote)
      } else {
        non_aosp_remote_style().apply_to(&upload_summary.dest_remote)
      },
      slash_style().apply_to("/"),
      branch_style().apply_to(&upload_summary.dest_branch),
    ));

    for commit_summary in &upload_summary.commit_summaries {
      lines.push(format!(
        "  {:.10} {}",
        console::style(commit_summary.id).cyan(),
        commit_summary.summary
      ))
    }
  }

  lines.push(format!(
    "Continue with upload? {}[y/N]? ",
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

fn summarize_upload(
  project_path: &str,
  repo: &git2::Repository,
  src: &BranchInfo,
  dest: &BranchInfo,
  dest_remote: &str,
  commits: &[git2::Oid],
) -> Result<UploadSummary, Error> {
  let mut commit_summaries: Vec<CommitSummary> = Vec::new();

  for commit_oid in commits {
    let commit = repo
      .find_commit(*commit_oid)
      .with_context(|| format!("could not find commit matching {}", commit_oid))?;
    commit_summaries.push(CommitSummary {
      id: commit.id(),
      summary: commit
        .summary()
        .ok_or_else(|| format_err!("commit message for {} is not valid UTF-8", commit.id()))?
        .to_string(),
    })
  }

  Ok(UploadSummary {
    project_path: project_path.to_string(),
    src_branch: src.name.to_string(),
    dest_remote: dest_remote.to_string(),
    dest_branch: dest.name_without_remote().to_string(),
    commit_summaries,
  })
}

impl Tree {
  pub fn construct<T: Into<PathBuf>>(
    depot: &Depot,
    path: T,
    manifest_config: &ManifestConfig,
    remote_config: &RemoteConfig,
    branch: &str,
    file: &str,
    group_filters: Vec<GroupFilter>,
    fetch: bool,
  ) -> Result<Tree, Error> {
    let tree_root = path.into();

    // TODO: Add locking?
    util::assert_empty_directory(&tree_root)?;
    let pore_path = tree_root.join(".pore");

    std::fs::create_dir_all(&pore_path).with_context(|| format!("failed to create directory {:?}", pore_path))?;

    let manifest_path = pore_path.join("manifest");
    let manifest_file = PathBuf::from("manifest").join(file);
    create_symlink(manifest_file, pore_path.join("manifest.xml")).context("failed to create manifest symlink")?;

    let manifest_project = &manifest_config.project;
    if fetch {
      depot.fetch_repo(
        remote_config,
        manifest_project,
        Some(&[branch.to_string()]),
        false,
        None,
      )?;
    }
    depot.clone_repo(remote_config, manifest_project, branch, &manifest_path)?;

    let tree_config = TreeConfig {
      remote: remote_config.name.clone(),
      branch: branch.into(),
      manifest: manifest_config.project.clone(),
      tags: Vec::new(),
      projects: Vec::new(),
      group_filters: Some(group_filters),
    };

    let tree = Tree {
      path: tree_root,
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
    std::fs::write(self.path.join(".pore").join("tree.toml"), text).context("failed to write tree config")
  }

  fn read_config<T: AsRef<Path>>(tree_root: T) -> Result<TreeConfig, Error> {
    let tree_root: &Path = tree_root.as_ref();
    let text =
      std::fs::read_to_string(tree_root.join(".pore").join("tree.toml")).context("failed to read tree config")?;
    toml::from_str(&text).context("failed to deserialize tree config")
  }

  pub fn read_manifest(&self) -> Result<Manifest, Error> {
    let manifest_path = self.path.join(".pore").join("manifest");
    let manifest_file = self.path.join(".pore").join("manifest.xml");
    let manifest = Manifest::parse(&manifest_path, &manifest_file).context("failed to read manifest")?;
    Ok(manifest)
  }

  pub fn collect_manifest_projects(
    &self,
    config: &Config,
    manifest: &Manifest,
    under: Option<Vec<PathBuf>>,
    additional_group_filters: Option<Vec<GroupFilter>>,
    repo_compat: bool,
  ) -> Result<Vec<ProjectInfo>, Error> {
    let default_revision = manifest
      .default
      .as_ref()
      .and_then(|def| def.revision.clone())
      .unwrap_or_else(|| self.config.branch.clone());

    let group_filters = self.config.group_filters.as_deref().unwrap_or(&[]);

    // The correctness of this seems dubious if the paths are accessed via symlinks or mount points,
    // but repo doesn't handle this either.
    let tree_root = std::fs::canonicalize(&self.path).context("failed to canonicalize tree path")?;
    let mut paths = Vec::new();
    for path in under.unwrap_or_default() {
      if std::fs::exists(&path)? {
        let requested_path = std::fs::canonicalize(&path)
          .with_context(|| format!("failed to canonicalize requested path '{}'", path.display()))?;
        paths.push(
          pathdiff::diff_paths(&requested_path, &tree_root)
            .ok_or_else(|| format_err!("failed to calculate path diff for {}", path.display()))?,
        );
      } else if path.is_relative() {
        paths.push(path)
      } else {
        return Err(format_err!(
          "requested project path is not relative to tree root and does not exist '{}'",
          path.display()
        ));
      }
    }

    let filtered_projects = manifest
      .projects
      .iter()
      .filter(|(_project_path, project)| {
        GroupFilter::filter_project(group_filters, additional_group_filters.as_deref(), project)
      })
      .filter(|(project_path, _project)| {
        if repo_compat {
          paths.is_empty() || paths.iter().any(|path| path == *project_path)
        } else {
          paths.is_empty() || paths.iter().any(|path| Path::new(path).starts_with(project_path))
        }
      });

    let mut projects = Vec::new();
    for (project_path, project) in filtered_projects {
      let (remote, remote_config) = manifest.resolve_project_remote(config, &self.config, project)?;
      let revision = project
        .revision
        .clone()
        .or_else(|| remote_config.revision.clone())
        .or_else(|| manifest.default.as_ref().and_then(|m| m.revision.clone()))
        .unwrap_or_else(|| default_revision.clone());

      projects.push(ProjectInfo {
        project_path: project_path.to_str().expect("project path not UTF-8").into(),
        project_name: project.name.clone(),
        remote,
        revision,
        file_ops: project.file_operations.clone(),
        manifest_project: false,
      });
    }
    Ok(projects)
  }

  fn sync_repos(
    &mut self,
    pool: &mut Pool,
    config: &Config,
    projects: Vec<ProjectInfo>,
    fetch_target: Option<FetchTarget>,
    checkout: CheckoutType,
    do_project_cleanup: bool,
    detach: bool,
    fetch_tags: bool,
    ssh_masters: &mut HashMap<String, std::process::Child>,
    no_lfs: bool,
  ) -> Result<i32, Error> {
    if let Some(target) = fetch_target {
      // The same underlying repository might be checked out into multiple directories.
      #[derive(PartialEq, Eq, Hash)]
      struct FetchProject {
        remote: String,
        project_name: String,
      }
      let mut fetch_projects = HashMap::<FetchProject, FetchTarget>::new();
      let mut remote_names = HashSet::new();

      for project in &projects {
        let key = FetchProject {
          remote: project.remote.clone(),
          project_name: project.project_name.clone(),
        };
        let local_target = target.clone().reify(&project.revision);
        fetch_projects
          .entry(key)
          .or_insert_with(FetchTarget::empty)
          .merge(&local_target);
        remote_names.insert(project.remote.clone());
      }

      // Spawn an ssh ControlMaster to eliminate ssh startup latency.
      for remote_name in remote_names {
        let remote = match config.find_remote(&remote_name) {
          Ok(x) => x,
          Err(_) => continue,
        };

        if let Ok(parsed) = Url::parse(&remote.url) {
          if parsed.scheme() != "ssh" {
            continue;
          }

          if let Some(host) = parsed.host_str() {
            ssh_masters.entry(host.into()).or_insert_with(|| {
              let mut cmd = std::process::Command::new("ssh");
              cmd.arg("-M");
              cmd.arg("-N");
              cmd.arg("-o").arg(format!("ControlPath {}", ssh_mux_path()));
              cmd.arg(host);
              cmd.spawn().expect("failed to start ssh ControlMaster")
            });
          }
        }
      }

      let mut job = Job::with_name("fetching");
      for (project, target) in &fetch_projects {
        let project_name = &project.project_name;
        let remote_name = &project.remote;
        let target = target.clone();

        job.add_task(project_name, move || -> Result<(), Error> {
          let remote = config
            .find_remote(remote_name)
            .with_context(|| format!("failed to find remote {}", remote_name))?;
          let depot = config
            .find_depot(&remote.depot)
            .with_context(|| format!("failed to find depot for remote {}", remote_name))?;

          let target_vec: Option<Vec<String>> = match target {
            FetchTarget::Upstream => panic!("unreified FetchTarget"),
            FetchTarget::Specific(targets) => Some(targets.iter().cloned().collect()),
            FetchTarget::All => None,
          };
          let target = target_vec.as_deref();
          depot
            .fetch_repo(remote, project_name, target, fetch_tags, None)
            .with_context(|| format!("failed to fetch for project {}", project_name,))?;
          Ok(())
        });
      }

      let result = pool.execute(job);
      if !result.failed.is_empty() {
        for failure in result.failed {
          eprintln!("{}: {:?}", failure.name, failure.result);
        }
        bail!("failed to sync");
      }
    }

    if checkout == CheckoutType::Checkout || checkout == CheckoutType::RefsOnly {
      let lfs_projects = dashmap::DashMap::<String, PathBuf>::with_capacity(10);
      let mut job = Job::with_name("checkout");
      let tree_root = &self.path;

      for project in &projects {
        let lfs_projects = &lfs_projects;
        let project_path = tree_root.join(&project.project_path);
        let manifest_branch = self.config.branch.clone();

        job.add_task(&project.project_path, move || {
          let remote = config
            .find_remote(&project.remote)
            .with_context(|| format!("failed to find remote {}", project.remote))?;
          let depot = config
            .find_depot(&remote.depot)
            .with_context(|| format!("failed to find depot for remote {}", project.remote))?;

          let project_name = &project.project_name;
          let revision = &project.revision;

          if project_path.exists() {
            depot
              .update_remote_refs(remote, &project.project_name, &project_path)
              .context("failed to update remote refs")?;
          }

          if checkout == CheckoutType::Checkout {
            if project_path.exists() {
              let repo = git2::Repository::open(&project_path).context("failed to open repository".to_string())?;

              // There's two things to be concerned about here:
              //  - HEAD might be attached to a branch
              //  - the repo might have uncommitted changes in the index or worktree
              //
              // If HEAD is attached to a branch, we choose to do nothing (for now), unless explicitly told to detach.
              // At some point, we should probably try to perform the equivalent of `git pull --rebase`.
              //
              // If the repo has uncommitted changes, do a dry-run first, and give up if we have any conflicts.
              let head_detached = repo.head_detached().context("failed to check if HEAD is detached")?;
              let current_head = repo.head();
              let current_head_oid = match current_head {
                Ok(ref head) => Some(head.target().context("HEAD not a direct reference?")?),
                Err(_) => None,
              };

              let new_head = util::parse_revision(&repo, &remote.name, revision)
                .with_context(|| {
                  format!(
                    "failed to find revision to sync to (wanted {}/{} in {:?})",
                    remote.name, revision, project_path
                  )
                })?
                .peel_to_commit()?;

              if Some(new_head.id()) == current_head_oid {
                // We're already at the top of tree.
              } else {
                if let Some(current_head_oid) = current_head_oid {
                  if detach {
                    repo
                      .set_head_detached(current_head_oid)
                      .context("failed to set HEAD detached")?;
                  } else {
                    // Check if the new head descends from the current one.
                    if !repo
                      .graph_descendant_of(new_head.id(), current_head_oid)
                      .context("graph descendent of failed")?
                    {
                      let (ahead, behind) = repo
                        .graph_ahead_behind(current_head_oid, new_head.id())
                        .context("graph ahead behind failed")?;
                      let head_name = if head_detached {
                        console::style("no branch".to_string()).red().to_string()
                      } else {
                        let head = current_head.unwrap();
                        let head_short = head.shorthand().context("branch name contains invalid UTF-8")?;
                        format!("branch {}", branch_style().apply_to(&head_short))
                      };
                      bail!("{} {}", head_name, util::ahead_behind(ahead, behind));
                    }
                  }
                }

                // Do a dry run first to look for dirty changes.
                repo
                  .checkout_tree(
                    new_head.as_object(),
                    Some(git2::build::CheckoutBuilder::new().dry_run()),
                  )
                  .with_context(|| format!("failed to dry run checkout to {:?}", new_head))?;

                repo
                  .checkout_tree(new_head.as_object(), None)
                  .with_context(|| format!("failed to checkout to {:?}", new_head))?;
                repo
                  .reset(new_head.as_object(), git2::ResetType::Soft, None)
                  .with_context(|| format!("failed to move HEAD to {:?}", new_head))?;
              }

              // Create a 'm/<manifest branch>' tag pointing to this commit.
              let tag_name = format!("m/{}", manifest_branch);
              repo.tag_lightweight(&tag_name, new_head.as_object(), true)?;
            } else {
              depot.clone_repo(remote, project_name, revision, &project_path)?;
            }

            if project.manifest_project {
              // Some tools look at the upstream tracking branch of .repo/manifest to determine
              // what manifest branch is being used.
              let repo = git2::Repository::open(&project_path).context("failed to open repository".to_string())?;
              let head = repo
                .head()
                .context("failed to get HEAD")?
                .peel_to_commit()
                .context("failed to peel HEAD to commit")?;

              let mut branch = match repo.find_branch("default", git2::BranchType::Local) {
                Ok(branch) => branch,
                Err(_) => repo
                  .branch("default", &head, true)
                  .context("failed to create manifest default branch")?,
              };

              // TODO: repo uses origin as the upstream, regardless of what the remote is called.
              branch
                .set_upstream(Some(&format!("{}/{}", project.remote, project.revision)))
                .with_context(|| {
                  format!(
                    "failed to set manifest branch upstream to {}/{}",
                    project.remote, project.revision
                  )
                })?;
              repo
                .set_head("refs/heads/default")
                .context("failed to set manifest HEAD")?;
            }

            // Set up symlinks to repo hooks.
            let hooks_dir = project_path.join(".git").join("hooks");
            let relpath = pathdiff::diff_paths(tree_root, &hooks_dir)
              .ok_or_else(|| format_err!("failed to calculate path diff from hooks to tree root"))?
              .join(".pore")
              .join("hooks");
            for filename in hooks::hooks().keys() {
              let target = relpath.join(filename);
              let symlink_path = hooks_dir.join(filename);
              let _ = std::fs::remove_file(&symlink_path);
              create_symlink(&target, &symlink_path)
                .with_context(|| format!("failed to create symlink at {:?}", &symlink_path))?;
            }

            if !no_lfs && Path::exists(&project_path.join(".lfsconfig")) {
              lfs_projects.insert(project.project_path.clone(), project_path);
            }
          }

          Ok(())
        });
      }

      let results = pool.execute(job);
      for failure in &results.failed {
        println!("{}", project_style().apply_to(&failure.name));
        println!("{}", console::style(format!("  {}", failure.result)).red());
      }

      // Perform linkfiles/copyfiles.
      for project in &projects {
        // src is the target of the link/the file that is copied, and is a relative path from the project.
        // dst is the location of the link/copy that the rule creates, and is a relative path from the tree root.
        for op in &project.file_ops {
          let src_path = self.path.join(&project.project_path).join(op.src());
          let dst_path = self.path.join(op.dst());

          let base = dst_path
            .parent()
            .ok_or_else(|| format_err!("linkfile destination is the root?"))?;

          if let Err(err) = std::fs::remove_file(&dst_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
              bail!("failed to unlink file {:?}: {}", dst_path, err);
            }
          }

          let _ = std::fs::create_dir_all(base)
            .map_err(|err| eprintln!("warning: failed to create directory {:?}: {}", &base, err));

          match op {
            FileOperation::LinkFile { .. } => {
              // repo makes the symlinks as relative symlinks.
              let target = pathdiff::diff_paths(&src_path, base)
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

      if do_project_cleanup {
        let lost_found = self.path.join("lost+found");
        std::fs::create_dir_all(&lost_found).context("failed to create lost+found directory")?;
        let find_ignore = lost_found.join(".find-ignore");
        std::fs::OpenOptions::new()
          .write(true)
          .create(true)
          .truncate(true)
          .open(find_ignore)
          .context("failed to create lost+found/.find-ignore")?;

        let previous: HashSet<String> = HashSet::from_iter(self.config.projects.iter().cloned());
        self.config.projects = projects.iter().map(|p| p.project_path.clone()).collect();
        let current: HashSet<String> = HashSet::from_iter(self.config.projects.iter().cloned());

        // Since we're using rename to move projects, we need to handle parents before children
        // (which will then fail because they've already been moved).
        let mut diff: Vec<_> = previous.difference(&current).collect();
        diff.sort();

        for ref project in diff {
          let src_path = self.path.join(project);
          let date = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
          let dst_path = self.path.join("lost+found").join(&date).join(project);
          println!("Moving deleted project {} to {:?}", project, dst_path);
          let result = std::fs::create_dir_all(&dst_path)
            .context("failed to create lost+found directory")
            .and_then(|_| {
              std::fs::rename(&src_path, &dst_path)
                .with_context(|| format!("failed to move project from {:?} to {:?}", src_path, dst_path))
            });

          if let Err(e) = result {
            eprintln!("warning: {}", e);
          }
        }
        self.write_config().context("failed to write tree config")?;
      }

      if !lfs_projects.is_empty() {
        let mut job = Job::with_name("lfs pull");

        for (project_name, project_path) in lfs_projects {
          job.add_task(project_name, move || -> Result<(), Error> {
            macro_rules! run_git {
              ($args:expr, $msg:expr) => {
                std::process::Command::new("git")
                  .args($args)
                  .current_dir(&project_path)
                  .stdout(std::process::Stdio::null())
                  .stderr(std::process::Stdio::null())
                  .status()
                  .expect($msg);
              };
            }

            run_git!(["lfs", "install", "--local"], "Failed to install Git LFS");
            run_git!(["lfs", "pull"], "Failed to pull Git LFS assets");

            Ok(())
          });
        }

        let results = pool.execute(job);
        for failure in &results.failed {
          println!("{}", project_style().apply_to(&failure.name));
          println!("{}", console::style(format!("  {}", failure.result)).red());
        }
      }
    }

    Ok(0)
  }

  fn write_hook(&self, directory: &Path, filename: &str, contents: &str) -> Result<(), Error> {
    let path = self.path.join(directory);
    std::fs::create_dir_all(&path).with_context(|| format!("failed to create directory: {:?}", path))?;
    let file_path = path.join(filename);
    let mut file =
      std::fs::File::create(&file_path).with_context(|| format!("failed to open file at {:?}", file_path))?;
    file
      .write_all(contents.as_bytes())
      .with_context(|| format!("failed to write hook at {:?}", file_path))?;

    // Currently no std APIs for dealing with file permissions on Windows.
    // Not running this on Windows probably isn't an issue since everything
    // should be executable unless the code was checked out to a directory
    // that prohibits it.
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt as _;

      let mut permissions = file.metadata()?.permissions();
      permissions.set_mode(0o700);
      file.set_permissions(permissions)?;
    }

    Ok(())
  }

  pub fn update_hooks(&self) -> Result<(), Error> {
    // Just always do this, since it's cheap.
    let hooks_dir = PathBuf::new().join(".pore").join("hooks");
    for (filename, contents) in hooks::hooks() {
      self.write_hook(hooks_dir.as_path(), filename, contents)?;
    }

    Ok(())
  }

  pub fn ensure_repo_compat(&self) -> Result<(), Error> {
    std::fs::create_dir_all(self.path.join(".repo")).context("failed to create dummy .repo dir")?;

    // Create symlinks for manifests and manifest.xml.
    create_symlink("../.pore/manifest", self.path.join(".repo").join("manifests"))?;
    create_symlink("../.pore/manifest.xml", self.path.join(".repo").join("manifest.xml"))?;

    // Write a script that forwards repo to pore.
    let repo_bin_dir = PathBuf::new().join(".repo").join("repo");
    self.write_hook(
      repo_bin_dir.as_path(),
      "repo",
      "#!/bin/bash\nexec -a repo pore \"${@}\"\n",
    )?;

    // Also write a main.py to the directory so that a bare `repo` can use it.
    self.write_hook(
      repo_bin_dir.as_path(),
      "main.py",
      include_str!("repo_main_trampoline.py"),
    )?;

    Ok(())
  }

  pub fn checkout(&self, config: &Config, pool: &mut Pool, target_branch: &str) -> Result<i32, Error> {
    let projects = self.collect_manifest_projects(config, &self.read_manifest()?, None, None, false)?;

    let mut job = Job::with_name("checkout");

    for project in &projects {
      let path = self.path.join(&project.project_path);
      job.add_task(&project.project_path, move || -> Result<Option<&str>, Error> {
        let repo =
          git2::Repository::open(&path).with_context(|| format!("failed to open object repository {:?}", path))?;

        let maybe_parse = match repo.revparse_ext(target_branch) {
          Ok(result) => Ok(Some(result)),
          Err(error) => {
            if error.code() == git2::ErrorCode::NotFound {
              Ok(None)
            } else {
              Err(error)
            }
          }
        }?;

        if let Some((object, reference)) = maybe_parse {
          repo.checkout_tree(&object, None)?;

          match reference {
            Some(repo_ref) => repo.set_head(repo_ref.name().unwrap())?,
            None => repo.set_head_detached(object.id())?,
          }

          Ok(Some(&project.project_path))
        } else {
          Ok(None)
        }
      });
    }

    let results = pool.execute(job);

    if !results.failed.is_empty() {
      for error in results.failed {
        eprintln!("{}: {}", error.name, error.result);
      }
      return Ok(1);
    }

    let mut has_projects = false;
    let checkout_projects = results.successful.into_iter().filter_map(|result| result.result);

    for checkout_project in checkout_projects {
      has_projects = true;
      println!("Checked out {checkout_project}");
    }

    if has_projects {
      Ok(0)
    } else {
      eprintln!("error: no project has branch {target_branch}");
      Ok(1)
    }
  }

  pub fn sync(
    &mut self,
    config: &Config,
    pool: &mut Pool,
    sync_under: Option<Vec<PathBuf>>,
    fetch_type: FetchType,
    fetch_target: FetchTarget,
    checkout: CheckoutType,
    detach: bool,
    fetch_tags: bool,
    no_lfs: bool,
  ) -> Result<i32, Error> {
    let mut ssh_masters = HashMap::new();
    let result = self.sync_impl(
      config,
      pool,
      sync_under,
      fetch_type,
      fetch_target,
      checkout,
      detach,
      fetch_tags,
      &mut ssh_masters,
      no_lfs,
    );
    for (host, child) in ssh_masters.iter_mut() {
      let mut cmd = std::process::Command::new("ssh");
      cmd.arg("-o").arg("ControlMaster no");
      cmd.arg("-o").arg(format!("ControlPath {}", ssh_mux_path()));
      cmd.arg("-O").arg("exit");
      cmd.arg(host);
      cmd.output().expect("failed to stop ssh ControlMaster");
      child.wait().expect("child exited with failure");
    }
    result
  }

  fn sync_impl(
    &mut self,
    config: &Config,
    pool: &mut Pool,
    sync_under: Option<Vec<PathBuf>>,
    fetch_type: FetchType,
    fetch_target: FetchTarget,
    checkout: CheckoutType,
    detach: bool,
    fetch_tags: bool,
    ssh_masters: &mut HashMap<String, std::process::Child>,
    no_lfs: bool,
  ) -> Result<i32, Error> {
    // Sync the manifest repo first.
    let manifest = vec![ProjectInfo {
      project_path: ".pore/manifest".into(),
      project_name: self.config.manifest.clone(),
      remote: self.config.remote.clone(),
      revision: self.config.branch.clone(),
      file_ops: Vec::new(),
      manifest_project: true,
    }];

    if fetch_type != FetchType::NoFetch {
      self.sync_repos(
        pool,
        config,
        manifest,
        Some(FetchTarget::Upstream),
        checkout,
        false,
        detach,
        fetch_tags,
        ssh_masters,
        no_lfs,
      )?;
    }

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, sync_under.clone(), None, false)?;
    self.sync_repos(
      pool,
      config,
      projects,
      if fetch_type == FetchType::NoFetch {
        None
      } else {
        Some(fetch_target)
      },
      checkout,
      sync_under.is_none(),
      detach,
      fetch_tags,
      ssh_masters,
      no_lfs || fetch_type == FetchType::NoFetch,
    )?;

    if sync_under.is_none() {
      self.update_hooks()?;
      self.ensure_repo_compat()?;
    }

    Ok(0)
  }

  pub fn branches(
    &self,
    config: Config,
    pool: &mut Pool,
  ) -> Result<ExecutionResults<ProjectBranchStatus, Error>, Error> {
    let projects = self.collect_manifest_projects(&config, &self.read_manifest()?, None, None, false)?;

    let mut job = Job::with_name("branches");

    for project in projects {
      job.add_task(
        project.project_path.clone(),
        move || -> Result<ProjectBranchStatus, Error> {
          let repo = git2::Repository::open(self.path.join(&project.project_path))
            .with_context(|| format!("failed to open object repository {:?}", project.project_path))?;

          let topic_branch_results = repo.branches(Some(git2::BranchType::Local))?;
          let mut branches = Vec::new();

          for topic_branch_result in topic_branch_results {
            let (topic_branch, _) = topic_branch_result?;
            branches.push(BranchStatus {
              name: topic_branch
                .name()?
                .expect("Branch should have utf-8 encoded name")
                .to_owned(),
              is_head: topic_branch.is_head(),
            });
          }

          Ok(ProjectBranchStatus {
            name: project.project_name,
            branches,
          })
        },
      );
    }

    Ok(pool.execute(job))
  }

  pub fn status(
    &self,
    config: &Config,
    pool: &mut Pool,
    status_under: Option<Vec<PathBuf>>,
  ) -> Result<ExecutionResults<ProjectStatus, Error>, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, status_under, None, false)?;

    let mut job = Job::with_name("status");
    for project in projects {
      job.add_task(project.project_path.clone(), move || -> Result<ProjectStatus, Error> {
        let path = self.path.join(&project.project_path);
        let repo = git2::Repository::open(&path)
          .with_context(|| format!("failed to open repository {}", project.project_path))?;

        let head = repo
          .head()
          .with_context(|| format!("failed to get HEAD for repository {}", project.project_path))?;
        let commit = head.peel_to_commit().context("failed to peel HEAD to commit")?;
        let branch = if head.is_branch() {
          Some(head.shorthand().unwrap().to_string())
        } else {
          None
        };

        let statuses = repo
          .statuses(Some(git2::StatusOptions::new().include_untracked(true)))
          .with_context(|| format!("failed to get status of repository {}", project.project_path))?;

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

        let upstream_object = util::parse_revision(&repo, &project.remote, &project.revision)?;
        let upstream_commit = upstream_object
          .peel_to_commit()
          .context("failed to peel upstream object to commit")?;

        let (ahead, behind) = repo.graph_ahead_behind(commit.id(), upstream_commit.id())?;

        Ok(ProjectStatus {
          name: project.project_name.clone(),
          path: project.project_path.clone(),
          branch,
          commit: commit.id(),
          commit_summary: commit.summary().map(|s| s.to_string()),
          files,
          ahead,
          behind,
        })
      });
    }

    Ok(pool.execute(job))
  }

  pub fn start(
    &self,
    config: &Config,
    _depot: &Depot,
    branch_name: String,
    revision: Option<String>,
    directory: &Path,
  ) -> Result<i32, Error> {
    let flags = git2::RepositoryOpenFlags::empty();
    let repo = git2::Repository::open_ext(directory, flags, &self.path).context("failed to find git repository")?;

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

    let upstream = project.find_revision(&manifest)?;
    let (remote, _remote_config) = manifest.resolve_project_remote(config, &self.config, project)?;
    let object = util::parse_revision(&repo, &remote, revision.as_ref().unwrap_or(&upstream))?;
    let commit = object.peel_to_commit().context("failed to peel object to commit")?;

    let mut branch = repo
      .branch(&branch_name, &commit, false)
      .with_context(|| format!("failed to create branch {}", branch_name))?;
    branch
      .set_upstream(Some(&format!("{}/{}", remote, upstream)))
      .context("failed to set branch upstream")?;

    repo.checkout_tree(&object, None)?;
    repo
      .set_head(&format!("refs/heads/{}", branch_name))
      .with_context(|| format!("failed to set HEAD to {}", branch_name))?;

    Ok(0)
  }

  pub fn upload(
    &self,
    config: &Config,
    pool: &mut Pool,
    upload_under: Option<Vec<PathBuf>>,
    current_branch: bool,
    no_verify: bool,
    reviewers: &[String],
    ccs: &[String],
    private: bool,
    wip: bool,
    branch_name_as_topic: bool,
    autosubmit: bool,
    presubmit_ready: bool,
    ps_description: Option<&str>,
    dry_run: bool,
  ) -> Result<i32, Error> {
    // TODO: Figure out how 0 (all projects) should work.
    ensure!(
      upload_under.as_ref().is_some_and(|v| !v.is_empty()),
      "pathless upload not yet implemented; must specify which projects to upload"
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

    struct UploadInfo {
      project_path: String,
      summary: UploadSummary,
      command: std::process::Command,
    }

    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, upload_under, None, false)?;
    let mut uploads: Vec<UploadInfo> = Vec::new();

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

      // TODO: Obey <remote review="...">?
      let remote_name = project_meta.find_remote(&manifest)?;
      let dest_branch_name = project_meta.find_dest_branch(&manifest)?;

      let remote_config = config.find_remote(&remote_name)?;

      let head = repo.head().context("could not determine HEAD")?;
      let src_branch_info = BranchInfo::from_ref(head)?;
      let dest_branch_info = BranchInfo::from_branch_name(
        &repo,
        format!("{}/{}", remote_name, dest_branch_name).as_str(),
        git2::BranchType::Remote,
      )?;

      let commits = util::find_independent_commits(&repo, &src_branch_info.commit, &dest_branch_info.commit)?;
      if commits.is_empty() {
        bail!(format!("No commits to upload for {}", &project.project_path));
      }

      let summary = summarize_upload(
        &project.project_path,
        &repo,
        &src_branch_info,
        &dest_branch_info,
        &remote_name,
        &commits,
      )?;

      let upload_options = remote_config.default_upload_options.clone();
      let cmd = util::make_push_command(
        self.path.join(&project.project_path),
        &remote_name,
        dest_branch_info.name_without_remote(),
        &util::UploadOptions {
          ccs,
          reviewers,
          topic: if branch_name_as_topic {
            Some(src_branch_info.name)
          } else {
            None
          },
          autosubmit,
          presubmit_ready,
          private,
          wip,
          ps_description,
          upload_options,
        },
      );

      uploads.push(UploadInfo {
        project_path: project.project_path.clone(),
        summary,
        command: cmd,
      })
    }

    if !no_verify {
      // Separate the upload prompt from preupload hook output.
      println!();
    }

    confirm_upload(uploads.iter().map(|u| &u.summary).collect(), autosubmit)?;

    let mut job = Job::with_name("uploading");
    for UploadInfo {
      project_path,
      summary: _,
      mut command,
    } in uploads
    {
      job.add_task(project_path, move || -> Result<String, Error> {
        if dry_run {
          Ok(format!("running: {:?}", command))
        } else {
          let git_output = command.output().context("failed to spawn git push")?;
          Ok(String::from_utf8_lossy(&git_output.stderr).to_string())
        }
      });
    }

    let results = pool.execute(job);
    let mut failed = false;
    for error in results.failed {
      eprintln!("{}: {}", error.name, error.result);
      failed = true;
    }

    for result in results.successful {
      println!("{}", result.result);
    }

    if !failed {
      Ok(0)
    } else {
      Ok(1)
    }
  }

  pub fn prune(
    &self,
    config: &Config,
    pool: &mut Pool,
    depot: &Depot,
    prune_under: Option<Vec<PathBuf>>,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, prune_under, None, false)?;

    let mut job = Job::with_name("pruning");

    struct PruneResult {
      pruned_branches: Vec<String>,
    }

    for project in &projects {
      job.add_task(&project.project_path, move || -> Result<PruneResult, Error> {
        let path = self.path.join(&project.project_path);
        let tree_repo = git2::Repository::open(&path)
          .with_context(|| format!("failed to open repository {:?}", project.project_path))?;

        let remote_config = config
          .find_remote(&project.remote)
          .with_context(|| format!("failed to find remote {}", project.remote))?;
        let local_project = Depot::apply_project_renames(remote_config, &project.project_name);
        let obj_repo_path = depot.objects_mirror(remote_config, &local_project);
        let obj_repo = git2::Repository::open_bare(&obj_repo_path)
          .with_context(|| format!("failed to open object repository {:?}", obj_repo_path))?;

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
          let mut branch = tree_repo.find_branch(branch_name, git2::BranchType::Local)?;
          branch
            .delete()
            .with_context(|| format!("failed to delete branch {}", branch_name))?;
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
        println!("{}", project_style().apply_to(result.name));
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
    config: &Config,
    pool: &mut Pool,
    interactive: bool,
    autosquash: bool,
    rebase_under: Option<Vec<PathBuf>>,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, rebase_under, None, false)?;

    if interactive {
      ensure!(
        projects.len() == 1,
        "interactive rebase not possible with multiple projects"
      );
      self.interactive_rebase(manifest, autosquash, projects[0].clone())
    } else {
      ensure!(!autosquash, "--autosquash is only valid with --interactive");
      self.non_interactive_rebase(pool, manifest, projects)
    }
  }

  fn interactive_rebase(&self, manifest: Manifest, autosquash: bool, project: ProjectInfo) -> Result<i32, Error> {
    let project_meta = manifest
      .projects
      .get(&PathBuf::from(&project.project_path))
      .ok_or_else(|| format_err!("failed to find project {:?}", project.project_path))?;

    let remote_name = project_meta.find_remote(&manifest)?;
    let dest_branch = project_meta.find_dest_branch(&manifest)?;

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

  fn non_interactive_rebase(
    &self,
    pool: &mut Pool,
    manifest: Manifest,
    projects: Vec<ProjectInfo>,
  ) -> Result<i32, Error> {
    let mut job = Job::with_name("rebasing");
    for project in &projects {
      let manifest = &manifest;
      job.add_task(&project.project_path, move || -> Result<bool, Error> {
        let path = self.path.join(&project.project_path);
        let repo = git2::Repository::open(&path)
          .with_context(|| format!("failed to open repository {:?}", project.project_path))?;

        if repo.head_detached().context("failed to check if HEAD is detached")? {
          // Don't rebase detached HEADs. This behavior matches repo.
          return Ok(true);
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

        let output = std::process::Command::new("git")
          .current_dir(&path)
          .arg("rebase")
          .arg(format!("{}/{}", remote_name, dest_branch))
          .output()
          .context("failed to spawn git rebase")?;

        Ok(output.status.success())
      })
    }

    let results = pool.execute(job);
    let mut failed = false;
    for error in results.failed {
      eprintln!("{}: {}", error.name, error.result);
      failed = true;
    }

    let conflicts: Vec<&ExecutionResult<bool>> = results.successful.iter().filter(|r| !r.result).collect();
    if !conflicts.is_empty() {
      failed = true;
      println!(
        "failed to rebase {} project{}:",
        conflicts.len(),
        if conflicts.len() == 1 { "" } else { "s" }
      );
      for conflict in conflicts {
        println!("  {}", project_style().clone().red().apply_to(&conflict.name));
      }
    }

    if failed {
      Ok(1)
    } else {
      Ok(0)
    }
  }

  pub fn forall(
    &self,
    config: &Config,
    pool: &mut Pool,
    forall_under: Option<Vec<PathBuf>>,
    group_filters: Option<Vec<GroupFilter>>,
    command: &str,
    repo_compat: bool,
  ) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, forall_under, group_filters, repo_compat)?;

    let mut job = Job::with_name("forall");

    struct CommandResult {
      rc: i32,
      output: Vec<u8>,
    }

    for project in &projects {
      job.add_task(&project.project_path, move || -> Result<CommandResult, Error> {
        let path = self.path.join(&project.project_path);
        let rel_to_root = pathdiff::diff_paths(&self.path, &path)
          .ok_or_else(|| format_err!("failed to calculate relative path to root"))?;

        let result = std::process::Command::new("sh")
          .arg("-c")
          .arg(command)
          .env("PORE_ROOT", self.path.as_os_str())
          .env("PORE_ROOT_REL", rel_to_root.as_os_str())
          .env("REPO_PROJECT", &project.project_name)
          .env("REPO_PATH", &project.project_path)
          .env("REPO_RREV", &project.revision)
          .env("REPO_REMOTE", &project.remote)
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
      let display = !result.output.is_empty() || (!repo_compat && result.rc != 0);
      if display {
        let mut stdout = std::io::stdout();
        if repo_compat {
          stdout.write_all(&result.output)?;
        } else {
          if result.rc == 0 {
            println!("{}", project_style().apply_to(project_name));
          } else {
            println!("{} (rc = {})", console::style(project_name).red().bold(), result.rc);
            rc = result.rc;
          }

          let lines = result.output.split(|&c| c == b'\n');

          for line in lines {
            stdout.write_all(b"  ")?;
            stdout.write_all(line)?;
            stdout.write_all(b"\n")?;
          }
        }
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

  pub fn preupload(&self, config: &Config, pool: &mut Pool, under: Option<Vec<PathBuf>>) -> Result<i32, Error> {
    let manifest = self.read_manifest().context("failed to read manifest")?;
    let projects = self
      .collect_manifest_projects(config, &manifest, under, None, false)
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
    for project in manifest.projects.values() {
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

    let hook_path = self.path.join(&hook_project_path).join("pre-upload.py");
    let mut job = Job::with_name("preupload");

    struct PresubmitResult {
      rc: i32,
      output: Vec<u8>,
    }

    for project in &projects {
      let project_path = self.path.join(&project.project_path);
      let hook_path = &hook_path;

      job.add_task(&project.project_path, move || -> Result<PresubmitResult, Error> {
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

        let upstream_object = util::parse_revision(&repo, &project.remote, &project.revision)?;
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
          let mut cmd = std::process::Command::new(hook_path.deref());
          cmd.current_dir(&project_path);
          cmd.arg("--project").arg(&project_path);

          for commit in commits {
            cmd.arg(commit.to_string());
          }

          let result = cmd.output()?;

          // TODO: Rust's process builder API kinda sucks, there's no way to spawn a process with
          //       stdout and stderr being the same pipe, to order their output chronologically.
          let mut output = result.stdout;
          if !result.stderr.is_empty() {
            if !output.is_empty() {
              output.push(b'\n');
            }
            output.extend(&result.stderr);
          }

          let rc = result.status.code().unwrap();

          Ok(PresubmitResult { rc, output })
        }
      });
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
        } else if !result.output.is_empty() {
          writeln!(output, "{}:", console::style(&project_name).green().bold()).unwrap();
        }
        output.write_all(&result.output).unwrap();
        output
      })
      .filter(|vec| !vec.is_empty())
      .collect();
    let _ = std::io::stdout().write_all(&outputs.join(&b'\n'));

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

  pub fn generate_manifest(&self, config: &Config, pool: &mut Pool, output: Option<PathBuf>) -> Result<i32, Error> {
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
          project_style().apply_to(&project_name)
        );
      }
      let manifest_project = manifest
        .projects
        .iter_mut()
        .find_map(|(_k, project)| {
          let path = project.path.as_ref()?;
          (path.as_str() == project_name).then_some(project)
        })
        .ok_or_else(|| format_err!("failed to find project {} in manifest", project_name))?;
      manifest_project.revision = Some(project_status.commit.to_string());
    }

    let output: Box<dyn Write> = match output {
      Some(path) => Box::new(std::fs::File::create(&path)?),
      None => Box::new(std::io::stdout()),
    };

    manifest.serialize(output).context("failed to serialize manifest")?;
    Ok(0)
  }

  pub fn list(&self, config: &Config) -> Result<i32, Error> {
    let manifest = self.read_manifest()?;
    let projects = self.collect_manifest_projects(config, &manifest, None, None, false)?;
    for project in projects {
      println!("{} : {}", project.project_path, project.project_name);
    }
    Ok(0)
  }

  pub fn find_deleted(&self, _config: &Config, _pool: &mut Pool) -> Result<i32, Error> {
    // First, find the repos in the tree.
    let mut it = WalkDir::new(&self.path).into_iter();
    let mut projects: HashSet<String> = HashSet::new();
    loop {
      let entry = match it.next() {
        None => break,
        Some(Err(err)) => panic!("ERROR: {}", err),
        Some(Ok(entry)) => entry,
      };
      if entry.file_type().is_dir() {
        if entry.file_name() == ".pore" {
          it.skip_current_dir();
        } else if entry.file_name() == ".git" {
          let path = entry.path().parent().unwrap();
          let relpath = pathdiff::diff_paths(path, &self.path).unwrap();
          projects.insert(relpath.to_str().unwrap().to_string());
          it.skip_current_dir();
        }
      }
    }

    let expected: HashSet<String> = HashSet::from_iter(self.config.projects.iter().cloned());
    let diff: Vec<&String> = projects.difference(&expected).collect();
    for p in &diff {
      println!("{}", p);
    }

    Ok(if diff.is_empty() { 0 } else { 1 })
  }
}
