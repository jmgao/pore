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

#![allow(clippy::too_many_arguments)]

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate log;

#[macro_use]
extern crate anyhow;

#[macro_use]
extern crate serde_derive;

use std::cmp;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Error};
use atty::Stream;
use joinery::Joinable;
use progpool::{Job, Pool};

#[macro_export]
macro_rules! fatal {
  ($($tt:tt)*) => {{
    use std::io::Write;
    write!(&mut ::std::io::stderr(), "fatal: ").unwrap();
    writeln!(&mut ::std::io::stderr(), $($tt)*).unwrap();
    ::std::process::exit(1)
  }}
}

mod config;
mod depot;
mod hooks;
mod manifest;
mod tree;
mod update_check;
mod util;

use config::Config;
use depot::Depot;
use manifest::Manifest;
use tree::{CheckoutType, FetchTarget, FetchType, FileState, GroupFilter, Tree};
use update_check::UpdateChecker;

lazy_static! {
  static ref AOSP_REMOTE_STYLE: console::Style = console::Style::new().bold().green();
  static ref NON_AOSP_REMOTE_STYLE: console::Style = console::Style::new().bold().red();
  static ref SLASH_STYLE: console::Style = console::Style::new().bold();
  static ref BRANCH_STYLE: console::Style = console::Style::new().bold().cyan();
  static ref PROJECT_STYLE: console::Style = console::Style::new().bold();
}

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None,)]
struct Args {
  /// Emit Chromium trace file to TRACE_FILE
  #[arg(short, long)]
  trace_file: Option<PathBuf>,

  /// Override default config file path (~/.pore.toml, with fallback to /etc/pore.toml)
  #[arg(short, long)]
  config: Option<PathBuf>,

  /// Run as if started in PATH instead of the current working directory
  #[arg(short = 'C')]
  cwd: Option<PathBuf>,

  /// Number of jobs to use at a time, defaults to CPU_COUNT.
  #[arg(short, global = true)]
  jobs: Option<i32>,

  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Checkout a new tree into the current directory
  Init {
    /// The target to checkout in the format <REMOTE>[/<BRANCH>[:MANIFEST]]
    /// BRANCH defaults to master if unspecified
    #[clap(verbatim_doc_comment)]
    target: String,

    /// Filter projects that satisfy a comma delimited list of groups
    /// Groups can be prepended with - to specifically exclude them
    #[arg(short, verbatim_doc_comment)]
    group_filters: Option<String>,

    /// Don't fetch; use only the local cache
    #[arg(short)]
    local: bool,
  },

  /// Checkout a new tree into a new directory
  Clone {
    /// The target to checkout in the format <REMOTE>[/<BRANCH>[:MANIFEST]]
    /// BRANCH defaults to master if unspecified
    #[clap(verbatim_doc_comment)]
    target: String,

    /// The directory to create and checkout the tree into
    /// Defaults to BRANCH if unspecified
    #[clap(verbatim_doc_comment)]
    directory: Option<PathBuf>,

    /// Filter projects that satisfy a comma delimited list of groups
    /// Groups can be prepended with - to specifically exclude them
    #[arg(short, verbatim_doc_comment)]
    group_filters: Option<String>,

    /// Don't fetch; use only the local cache
    #[arg(short)]
    local: bool,
  },

  /// Fetch a tree's repositories without checking out
  Fetch {
    /// Fetch all remote refs (opposite of `repo sync -c`, which is the default behavior)
    #[arg(short = 'a', group = "targets")]
    fetch_all: bool,

    /// Specify a branch to fetch (can be used multiple times)
    #[arg(short, long, group = "targets")]
    branch: Option<Vec<String>>,

    /// Fetch all remote tags
    #[arg(short, long, group = "targets")]
    tags: bool,

    /// Path(s) beneath which repositories are synced
    /// Defaults to all repositories in the tree if unspecified
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,
  },

  /// Fetch and checkout a tree's repositories
  Sync {
    /// Don't fetch; use only the local cache
    #[arg(short)]
    local: bool,

    /// Fetch all remote refs (opposite of `repo sync -c`, which is the default behavior)
    #[arg(short = 'a')]
    fetch_all: bool,

    /// Specify a branch to fetch (can be used multiple times)
    #[arg(short, long)]
    branch: Option<Vec<String>>,

    /// Detach projects back to manifest revision
    #[arg(short)]
    detach: bool,

    /// Don't checkout, only update the refs
    #[arg(short, long = "refs-only")]
    refs_only: bool,

    /// Fetch all remote tags
    #[arg(short, long)]
    tags: bool,

    /// Path(s) beneath which repositories are synced
    /// Defaults to all repositories in the tree if unspecified
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,

    /// After sync, don't pull LFS.
    #[arg(long = "no-lfs")]
    no_lfs: bool,
  },

  /// Start a branch in the current repository
  Start {
    /// Name of branch to create
    branch: String,

    /// Point branch at this revision instead of upstream
    #[arg(short, long, visible_alias = "rev")]
    revision: Option<String>,

    /// Path to the project to start a branch in. If omitted, the current
    /// directory will be used.
    path: Option<PathBuf>,
  },

  /// Rebase local branch onto upstream branch
  Rebase {
    /// Pass --interactive to git rebase (single project only)
    #[arg(short, long)]
    interactive: bool,

    /// Pass --autosquash to git rebase
    #[arg(long)]
    autosquash: bool,

    /// Path(s) of the projects to rebase
    /// Defaults to all repositories in the tree
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,
  },

  /// Upload patches to Gerrit
  Upload {
    /// Path(s) of the projects to be uploaded
    path: Option<Vec<PathBuf>>,

    /// Upload current git branch
    #[arg(long = "cbr")]
    current_branch: bool,

    /// Skip pre-upload hooks
    #[arg(long = "no-verify")]
    no_verify: bool,

    /// Comma separated list of reviewers
    /// User names without a domain will be assumed to be @google.com
    #[arg(long = "re", verbatim_doc_comment)]
    reviewers: Option<String>,

    /// Comma separated list of users to CC
    /// User names without a domain will be assumed to be @google.com"
    #[arg(long, verbatim_doc_comment)]
    cc: Option<String>,

    /// Upload as private change
    #[arg(long)]
    private: bool,

    /// Upload as work in progress change
    #[arg(long)]
    wip: bool,

    /// Use local branch name as topic
    #[arg(short = 't')]
    branch_name_as_topic: bool,

    /// Enable autosubmit
    #[arg(long)]
    autosubmit: bool,

    /// Do not enable autosubmit
    #[arg(long = "no-autosubmit")]
    no_autosubmit: bool,

    /// Queue the change for presubmit
    #[arg(long)]
    presubmit: bool,

    /// Do not queue the change for presubmit
    #[arg(long = "no-presubmit")]
    no_presubmit: bool,

    /// Description to set for the new patch set
    #[arg(short = 'm', long = "message")]
    ps_description: Option<String>,

    /// Don't upload; just show upload commands
    #[arg(long = "dry-run")]
    dry_run: bool,
  },

  /// Prune branches that have been merged
  Prune {
    /// Path(s) to prune
    /// Defaults to all repositories in the tree
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,
  },

  /// Show working tree status across the entire tree
  Status {
    /// Path(s) beneath which to calculate status
    /// Defaults to all repositories in the tree if unspecified
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,

    /// Suppress output
    #[arg(long, short)]
    quiet: bool,
  },

  /// Run a command in each project in the tree
  #[command(after_help = "
        Commands will be run with the current working directory inside each project,
        and with the following environment variables defined:

          $PORE_ROOT       absolute path of the root of the tree
          $PORE_ROOT_REL   relative path from the project to the root of the tree
          $REPO_PROJECT    name of the git repository in the remote
          $REPO_PATH       relative path from the root of the tree to project")]
  Forall {
    /// Path(s) beneath which to run commands
    /// Defaults to all repositories in the tree if unspecified
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,

    /// Command to run
    #[arg(short)]
    command: String,
  },

  /// Run repo's preupload hooks
  Preupload {
    /// Path(s) beneath which to run preupload hooks
    /// Defaults to all repositories in the tree if unspecified
    #[clap(verbatim_doc_comment)]
    path: Option<Vec<PathBuf>>,
  },

  /// Import an existing repo tree into a depot
  Import {
    /// Copy objects instead of hard-linking
    #[arg(short, long)]
    copy: bool,

    /// The repo mirror to import from
    /// Defaults to the current working directory if unspecified
    #[clap(verbatim_doc_comment)]
    directory: Option<PathBuf>,
  },

  /// List repositories
  List {},

  /// Find projects that were removed from the manifest
  #[command(name = "find-deleted")]
  FindDeleted {},

  /// Generate a manifest corresponding to the current state of the tree
  Manifest {
    /// Write result to FILE instead of to standard output
    #[arg(name = "FILE", short = 'o', long = "output")]
    output: Option<PathBuf>,
  },

  /// Prints the parsed configuration file
  Config {
    /// Print the default configuration
    #[arg(long)]
    default: bool,
  },

  /// Implementation of repo info
  Info {
    /// Disable all remote operations
    #[arg(short, long = "local-only")]
    local: bool,

    path: Option<Vec<PathBuf>>,
  },
}

impl std::fmt::Display for Commands {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Commands::Init { .. } => write!(f, "init"),
      Commands::Clone { .. } => write!(f, "clone"),
      Commands::Fetch { .. } => write!(f, "fetch"),
      Commands::Sync { .. } => write!(f, "sync"),
      Commands::Start { .. } => write!(f, "start"),
      Commands::Rebase { .. } => write!(f, "rebase"),
      Commands::Upload { .. } => write!(f, "upload"),
      Commands::Prune { .. } => write!(f, "prune"),
      Commands::Status { .. } => write!(f, "status"),
      Commands::Forall { .. } => write!(f, "forall"),
      Commands::Preupload { .. } => write!(f, "preupload"),
      Commands::Import { .. } => write!(f, "import"),
      Commands::List { .. } => write!(f, "list"),
      Commands::FindDeleted { .. } => write!(f, "find-deleted"),
      Commands::Manifest { .. } => write!(f, "manifest"),
      Commands::Config { .. } => write!(f, "config"),
      Commands::Info { .. } => write!(f, "info"),
    }
  }
}

fn parse_target(target: &str) -> Result<(String, Option<String>, Option<String>), Error> {
  let vec: Vec<&str> = target.split('/').collect();
  if vec.len() > 2 {
    bail!("invalid target '{}'", target)
  }
  let remote = vec[0].into();

  if let Some(branch_file) = vec.get(1) {
    let v: Vec<&str> = branch_file.split(':').collect();
    if v.len() > 2 {
      bail!("invalid target '{}'", target)
    }
    let branch = Some(v[0].into());
    let file = v.get(1).map(|&s| s.into());
    return Ok((remote, branch, file));
  }

  Ok((remote, None, None))
}

fn cmd_clone(
  config: Arc<Config>,
  mut pool: &mut Pool,
  target: &str,
  directory: Option<PathBuf>,
  group_filters: Option<&str>,
  fetch: bool,
) -> Result<i32, Error> {
  let (manifest, branch, file) = parse_target(target)?;
  let manifest_config = config.find_manifest(&manifest)?;
  let remote_config = config.find_remote(&manifest_config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  let branch = branch.as_ref().unwrap_or(&manifest_config.default_branch);
  let file = file.as_ref().unwrap_or(&manifest_config.default_manifest_file);

  let tree_root = directory.unwrap_or(PathBuf::from(&branch));
  if let Err(err) = std::fs::create_dir_all(&tree_root) {
    bail!("failed to create tree root {:?}: {}", tree_root, err);
  }

  let group_filters = group_filters
    .map(|s| {
      s.split(',')
        .map(|group| {
          if group.starts_with('-') {
            GroupFilter::Exclude(group[1..].to_string())
          } else {
            GroupFilter::Include(group.to_string())
          }
        })
        .collect()
    })
    .unwrap_or_else(Vec::new);

  // TODO: Add locking?
  let mut tree = Tree::construct(
    &depot,
    &tree_root,
    &manifest_config,
    &remote_config,
    &branch,
    &file,
    group_filters,
    fetch,
  )?;
  let fetch_type = if fetch {
    // We just fetched the manifest.
    FetchType::FetchExceptManifest
  } else {
    FetchType::NoFetch
  };

  tree.sync(
    Arc::clone(&config),
    &mut pool,
    None,
    fetch_type,
    FetchTarget::Upstream,
    CheckoutType::Checkout,
    false,
    false,
    false,
  )
}

struct ProjectStatusDisplayData {
  location: String,
  branch: String,
  top_commit: String,
  files: Vec<String>,
  dirty: bool,
}

impl ProjectStatusDisplayData {
  pub fn from_status(status: &tree::ProjectStatus) -> ProjectStatusDisplayData {
    let branch = match &status.branch {
      Some(branch) => BRANCH_STYLE.apply_to(format!("branch {}", branch)),
      None => console::style("no branch".to_string()).red(),
    };

    let files: Vec<_> = status
      .files
      .iter()
      .map(|file| {
        let index = file.index.to_char().to_uppercase().to_string();
        let worktree = file.worktree.to_char();
        let mut line = console::style(format!(" {}{}     {}", index, worktree, file.filename));
        if file.worktree != FileState::Unchanged {
          line = line.red();
        } else {
          line = line.green();
        }

        line.to_string()
      })
      .collect();

    let dirty = status.branch.is_none() || !files.is_empty();

    ProjectStatusDisplayData {
      location: PROJECT_STYLE.apply_to(format!("project {}", status.path)).to_string(),
      branch: format!("{}{}", branch, util::ahead_behind(status.ahead, status.behind)),
      top_commit: status.commit_summary.clone().unwrap_or_default(),
      files,
      dirty,
    }
  }
}

struct TreeStatusDisplayData {
  projects: Vec<ProjectStatusDisplayData>,
  max_project_length: usize,
  max_branch_length: usize,
}

impl TreeStatusDisplayData {
  pub fn from_results(results: Vec<&tree::ProjectStatus>) -> TreeStatusDisplayData {
    let mut max_project_length = 0;
    let mut max_branch_length = 0;

    let mut projects = Vec::new();
    for result in results {
      if !TreeStatusDisplayData::should_report(&result) {
        continue;
      }

      let project = ProjectStatusDisplayData::from_status(&result);

      max_project_length = cmp::max(
        max_project_length,
        console::measure_text_width(project.location.as_str()),
      );
      max_branch_length = cmp::max(max_branch_length, console::measure_text_width(project.branch.as_str()));

      projects.push(project);
    }

    TreeStatusDisplayData {
      projects,
      max_project_length,
      max_branch_length,
    }
  }

  pub fn should_report(status: &tree::ProjectStatus) -> bool {
    return status.ahead != 0 || status.behind != 0 || !status.files.is_empty();
  }
}

fn cmd_status(
  config: Arc<Config>,
  mut pool: &mut Pool,
  tree: &Tree,
  status_under: Option<Vec<PathBuf>>,
  quiet: bool,
) -> Result<i32, Error> {
  pool.quiet(quiet);

  let results = tree.status(Arc::clone(&config), &mut pool, status_under)?;
  let mut status = 0;

  let column_padding = 4;
  let display_data = TreeStatusDisplayData::from_results(results.successful.iter().map(|r| &r.result).collect());
  for project in display_data.projects {
    if project.dirty {
      status += 1;
    }

    let project_column = console::pad_str(
      project.location.as_str(),
      display_data.max_project_length + column_padding,
      console::Alignment::Left,
      None,
    );

    if quiet {
      println!("{}", project_column);
      continue;
    }

    let branch_column = console::pad_str(
      project.branch.as_str(),
      display_data.max_branch_length + column_padding,
      console::Alignment::Left,
      None,
    );
    let summary = console::truncate_str(project.top_commit.as_str(), 53, "...");
    println!("{}{}{}", project_column, branch_column, summary);

    for line in &project.files {
      println!("{}", line)
    }
  }

  if !results.failed.is_empty() {
    for error in results.failed {
      eprintln!("{}: {}", error.name, error.result);
    }
    bail!("failed to git status");
  }

  Ok(status)
}

fn cmd_import(config: Arc<Config>, pool: &mut Pool, target_path: Option<PathBuf>, copy: bool) -> Result<i32, Error> {
  let target_path = target_path.unwrap_or(PathBuf::from("."));
  let target_metadata = std::fs::metadata(&target_path)?;

  // Make sure that we're actually in a repo tree.
  let manifest_dir = target_path.join(".repo").join("manifests");
  let manifest_path = target_path.join(".repo").join("manifest.xml");

  let manifest = Manifest::parse(&manifest_dir, &manifest_path)?;
  let manifest_default = manifest.default.expect("manifest missing <default>");
  let manifest_default_remote = manifest_default.remote.expect("manifest <default> missing remote");

  let mut remote_projects: HashMap<String, HashSet<String>> = HashMap::new();
  for (_, project) in manifest.projects {
    let name = project.name;
    let remote = project.remote.unwrap_or(manifest_default_remote.clone());
    remote_projects.entry(remote).or_default().insert(name);
  }

  let mut job = Job::with_name("import");
  for (remote, projects) in remote_projects {
    let remote_config = config.find_remote(&remote)?;
    let depot = config.find_depot(&remote_config.depot)?;
    std::fs::create_dir_all(&depot.path).context("failed to create depot directory")?;
    let depot_metadata = std::fs::metadata(&depot.path)?;

    if !copy && depot_metadata.dev() != target_metadata.dev() {
      bail!(
        "Depot ({:?}) and target ({:?}) appear to be on different filesystems.\n       \
             Either move the depot to the same filesystem, or use --copy to copy instead.",
        &depot.path,
        target_path
      );
    }

    let depot = Arc::new(depot);

    for project in projects {
      let depot = Arc::clone(&depot);

      let src_path = target_path
        .join(".repo")
        .join("project-objects")
        .join(project.clone() + ".git");
      let depot_path = depot.objects_mirror(remote_config, &Depot::apply_project_renames(remote_config, &project));

      if !src_path.is_dir() {
        eprintln!("Skipping missing project: {}", project);
        continue;
      }

      job.add_task(project.clone(), move |_| -> Result<(), Error> {
        if !depot_path.is_dir() {
          // Create a new repository in its location.
          git2::Repository::init_bare(&depot_path).context("failed to create git repository")?;
        }

        // Copy the objects over.
        let depot_object_path = depot_path.join("objects");
        let src_objects_path = src_path.join("objects");
        let src_object_dirs = std::fs::read_dir(src_objects_path).context("failed to read repository object dir")?;
        for dir in src_object_dirs {
          let dir = dir.context("failed to stat object dir")?;
          if dir.file_name() == "info" {
            // We don't want to copy objects/info over: it doesn't contain anything we want,
            // and it might have weird stuff like alternates.
            continue;
          }

          let depot_dir = depot_object_path.join(dir.file_name());
          std::fs::create_dir_all(&depot_dir)?;

          let src_objects = std::fs::read_dir(dir.path()).context("failed to read repository objects")?;
          for file in src_objects {
            let file = file.context("failed to stat object")?;
            let target_path = depot_dir.join(file.file_name());

            if copy {
              std::fs::copy(file.path(), target_path)?;
            } else {
              std::fs::hard_link(file.path(), target_path)?;
            }
          }
        }

        Ok(())
      });
    }
  }

  let result = pool.execute(job);
  if !result.failed.is_empty() {
    for failure in result.failed {
      eprintln!("{}: {:?}", failure.name, failure.result);
    }
    bail!("failed to import");
  }

  Ok(0)
}

// Sets GIT_TRACE2_PARENT_SID for matching multiple git traces to a single pore session
//
// See:
//   https://github.com/git/git/blob/master/trace2/tr2_sid.c
//   https://gerrit-review.googlesource.com/c/git-repo/+/254331
fn set_trace_id() {
  let key = "GIT_TRACE2_PARENT_SID";

  let mut my_id = match std::env::var_os(key) {
    Some(mut val) => {
      val.push("/");
      val
    }
    None => OsString::new(),
  };

  my_id.push(format!(
    "pore-{}-P{:08x}",
    chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
    std::process::id()
  ));

  std::env::set_var(key, my_id)
}

fn cmd_info(config: Arc<Config>, tree: &Tree, paths: &[&Path]) -> Result<i32, Error> {
  let manifest_branch = &tree.config.branch;
  // TODO: Actually use the manifest for this.
  let manifest_merge_branch = format!("refs/heads/{}", manifest_branch);

  // Get the active groups from the tree config
  let group_filters = tree.config.group_filters.clone();
  let group_filters: Vec<String> = group_filters
    .unwrap_or(vec![GroupFilter::Include(String::from("default"))])
    .iter()
    .map(|gf| match gf {
      GroupFilter::Include(s) => s.clone(),
      GroupFilter::Exclude(s) => "-".to_string() + &s,
    })
    .collect();
  let group_filters = group_filters.join_with(",");

  println!("Manifest branch: {}", manifest_branch);
  println!("Manifest merge branch: {}", manifest_merge_branch);
  println!("Manifest groups: {}", group_filters);
  println!("----------------------------");

  let manifest = tree.read_manifest()?;
  let projects = tree.collect_manifest_projects(config, &manifest, None)?;
  let selected_projects = if paths.is_empty() {
    projects
  } else {
    // This is slower than it needs to be, but the common case is that paths.len() == 1.
    let mut result = Vec::new();
    for path in paths {
      let mut found = false;

      let realpath = std::fs::canonicalize(path).context("failed to canonicalize path")?;
      let project_path = pathdiff::diff_paths(realpath, &tree.path).context("path not in tree?")?;
      let project_path = project_path.to_string_lossy();
      for manifest_project in &projects {
        if manifest_project.project_path == project_path {
          result.push(manifest_project.clone());
          found = true;
          break;
        }
      }

      if !found {
        bail!("project {:?} not found", path);
      }
    }
    result
  };

  for project in selected_projects {
    let project_path = tree.path.join(project.project_path);
    let repo = git2::Repository::open(&project_path).context("failed to open git repository")?;
    let current_head = repo.head().context("failed to get HEAD")?;
    let current_head_commit = current_head.peel_to_commit().context("failed to peel HEAD to commit")?;
    let head_detached = repo.head_detached().context("failed to check if HEAD is detached")?;
    let current_branch = if head_detached {
      None
    } else {
      Some(
        current_head
          .shorthand()
          .context("branch name contained invalid UTF-8")?,
      )
    };
    let mut local_branches = Vec::new();
    let branches = repo
      .branches(Some(git2::BranchType::Local))
      .context("failed to fetch branches")?;
    for branch in branches {
      let branch_name = branch?
        .0
        .name()?
        .context("branch name contained invalid UTF-8")?
        .to_string();
      local_branches.push(branch_name);
    }

    println!("Project: {}", project.project_name);
    println!("Mount path: {}", project_path.to_string_lossy());
    println!("Current revision: {}", current_head_commit.id());
    if let Some(branch) = current_branch {
      println!("Current branch: {}", branch);
    }
    println!("Manifest revision: {}", project.revision);
    if local_branches.len() == 0 {
      println!("Local Branches: 0");
    } else {
      println!(
        "Local Branches: {} [{}]",
        local_branches.len(),
        local_branches.join_with(", ")
      );
    }
    println!("----------------------------");
  }
  Ok(0)
}

fn main() {
  let args: Args = Args::parse();

  if let Some(trace_file) = args.trace_file {
    match std::fs::File::create(trace_file) {
      Ok(file) => {
        let tracer = tracing_chromium::Tracer::from_output(Box::new(file));
        tracing_facade::set_boxed_tracer(Box::new(tracer));
      }

      Err(err) => {
        fatal!("failed to open trace file: {}", err);
      }
    }
  }

  if let Some(cwd) = args.cwd {
    if let Err(err) = std::env::set_current_dir(&cwd) {
      fatal!("failed to set working directory to {}: {}", cwd.display(), err);
    }
  }
  let cwd = std::env::current_dir().expect("failed to get current working directory");

  set_trace_id();

  let repo_compat = std::env::args().next() == Some("repo".into());
  let config_path = match args.config {
    Some(path) => {
      info!("using provided config path {:?}", path);
      PathBuf::from(path)
    }

    None => {
      let path = dirs::home_dir()
        .expect("failed to find home directory")
        .join(".pore.toml");
      if !path.exists() {
        info!("falling back to global config path /etc/pore.toml");
        "/etc/pore.toml".into()
      } else {
        info!("using default config path {:?}", path);
        path
      }
    }
  };

  let config = match Config::from_path(&config_path) {
    Ok(config) => Arc::new(config),

    Err(err) => {
      eprintln!(
        "warning: failed to read config file at {:?}, falling back to default config: {}",
        config_path, err,
      );
      Arc::new(Config::default())
    }
  };

  let cmd = args.command;
  let pool_size = args
    .jobs
    .or_else(|| {
      // Command-specific override
      config.parallelism.get(cmd.to_string().as_str()).cloned().or_else(|| {
        // Global override
        config.parallelism.get("global").cloned()
      })
    })
    .unwrap_or(0);

  let num_cpus = num_cpus::get();
  let pool_size = if pool_size == 0 {
    num_cpus
  } else if pool_size < 0 {
    cmp::min(num_cpus, (-pool_size) as usize)
  } else {
    pool_size as usize
  };
  let mut pool = Pool::with_size(pool_size);

  let update_checker = if config.update_check && atty::is(Stream::Stdout) {
    Some(UpdateChecker::fetch())
  } else {
    None
  };

  let command_action = || -> Result<i32, Error> {
    match cmd {
      Commands::Init {
        target,
        group_filters,
        local,
      } => {
        let fetch = !local;
        cmd_clone(
          Arc::clone(&config),
          &mut pool,
          &target,
          Some(PathBuf::from(".")),
          group_filters.as_deref(),
          fetch,
        )
      }
      Commands::Clone {
        target,
        directory,
        group_filters,
        local,
      } => {
        let fetch = !local;
        cmd_clone(
          Arc::clone(&config),
          &mut pool,
          target.as_str(),
          directory,
          group_filters.as_deref(),
          fetch,
        )
      }
      Commands::Fetch {
        fetch_all,
        branch,
        tags,
        path,
      } => {
        let mut tree = Tree::find_from_path(cwd)?;
        let fetch_tags = tags || fetch_all;

        let fetch_target = {
          if fetch_all {
            FetchTarget::All
          } else if let Some(branches) = branch {
            FetchTarget::Specific(branches.into_iter().collect())
          } else {
            FetchTarget::Upstream
          }
        };

        tree.sync(
          Arc::clone(&config),
          &mut pool,
          path,
          FetchType::Fetch,
          fetch_target,
          CheckoutType::NoCheckout,
          false,
          fetch_tags,
          false,
        )
      }
      Commands::Sync {
        local,
        fetch_all,
        branch,
        detach,
        refs_only,
        tags,
        path,
        no_lfs,
      } => {
        let fetch_type = if local { FetchType::NoFetch } else { FetchType::Fetch };
        let mut tree = Tree::find_from_path(cwd)?;

        let fetch_tags = tags || fetch_all;

        let fetch_target = {
          if fetch_all {
            FetchTarget::All
          } else if branch.is_none() {
            FetchTarget::Upstream
          } else {
            let branches = branch.unwrap().iter().map(|s| s.to_string()).collect();
            FetchTarget::Specific(branches)
          }
        };
        tree.sync(
          Arc::clone(&config),
          &mut pool,
          path,
          fetch_type,
          fetch_target,
          if refs_only {
            CheckoutType::RefsOnly
          } else {
            CheckoutType::Checkout
          },
          detach,
          fetch_tags,
          no_lfs,
        )
      }
      Commands::Start { branch, revision, path } => {
        let tree = Tree::find_from_path(cwd.clone())?;

        let remote_config = config.find_remote(&tree.config.remote)?;
        let depot = config.find_depot(&remote_config.depot)?;

        tree.start(Arc::clone(&config), &depot, branch, revision, &path.unwrap_or(cwd))
      }
      Commands::Rebase {
        interactive,
        autosquash,
        path,
      } => {
        let tree = Tree::find_from_path(cwd)?;
        tree.rebase(config, &mut pool, interactive, autosquash, path)
      }
      Commands::Upload {
        path,
        current_branch,
        no_verify,
        reviewers,
        cc,
        private,
        wip,
        branch_name_as_topic,
        autosubmit,
        no_autosubmit,
        presubmit,
        no_presubmit,
        ps_description,
        dry_run,
      } => {
        let tree = Tree::find_from_path(cwd)?;
        let autosubmit_upload = if autosubmit {
          true
        } else if no_autosubmit {
          false
        } else {
          config.autosubmit
        };

        let presubmit_upload = if presubmit {
          true
        } else if no_presubmit {
          false
        } else {
          config.presubmit
        };

        fn user_string_to_vec(users: Option<&str>) -> Vec<String> {
          users
            .unwrap_or("")
            .split(',')
            .filter(|r| !r.is_empty())
            .map(|r| {
              if r.contains('@') {
                r.to_string()
              } else {
                format!("{}@google.com", r)
              }
            })
            .collect()
        }

        tree.upload(
          Arc::clone(&config),
          &mut pool,
          path,
          current_branch,
          no_verify,
          &user_string_to_vec(reviewers.as_deref()),
          &user_string_to_vec(cc.as_deref()),
          private,
          wip,
          branch_name_as_topic,
          autosubmit_upload,
          presubmit_upload,
          ps_description.as_deref(),
          dry_run,
        )
      }
      Commands::Prune { path } => {
        let tree = Tree::find_from_path(cwd)?;
        let remote_config = config.find_remote(&tree.config.remote)?;
        let depot = config.find_depot(&remote_config.depot)?;

        tree.prune(config, &mut pool, &depot, path)
      }
      Commands::Status { path, quiet } => {
        let tree = Tree::find_from_path(cwd)?;
        cmd_status(Arc::clone(&config), &mut pool, &tree, path, quiet)
      }
      Commands::Forall { path, command } => {
        let tree = Tree::find_from_path(cwd)?;
        let command = command;

        tree.forall(Arc::clone(&config), &mut pool, path, command.as_str(), repo_compat)
      }
      Commands::Preupload { path } => {
        let tree = Tree::find_from_path(cwd)?;
        tree.preupload(config, &mut pool, path)
      }
      Commands::Import { copy, directory } => cmd_import(config, &mut pool, directory, copy),
      Commands::List {} => {
        let tree = Tree::find_from_path(cwd)?;
        tree.list(config)
      }
      Commands::FindDeleted {} => {
        let tree = Tree::find_from_path(cwd)?;
        tree.find_deleted(config, &mut pool)
      }
      Commands::Manifest { output } => {
        let tree = Tree::find_from_path(cwd)?;
        tree.generate_manifest(Arc::clone(&config), &mut pool, output)
      }
      Commands::Config { default } => {
        if default {
          println!("{}", Config::default_string());
        } else {
          println!("{}", toml::to_string_pretty(config.as_ref())?);
        }
        Ok(0)
      }
      Commands::Info { path, .. } => {
        let tree = Tree::find_from_path(cwd)?;
        let paths_vec = match &path {
          None => Vec::new(),
          Some(paths) => paths.iter().map(PathBuf::as_path).collect(),
        };
        cmd_info(config, &tree, &paths_vec)
      }
    }
  };

  if let Some(u) = update_checker {
    u.finish();
  }

  match command_action() {
    Ok(rc) => std::process::exit(rc),
    Err(err) => {
      writeln!(&mut std::io::stderr(), "fatal: {:#}", err).unwrap();
    }
  }
}
