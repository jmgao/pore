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
extern crate indoc;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate clap;

use std::cmp;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Error};
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
  directory: Option<&str>,
  group_filters: Option<&str>,
  fetch: bool,
) -> Result<i32, Error> {
  let (manifest, branch, file) = parse_target(target)?;
  let manifest_config = config.find_manifest(&manifest)?;
  let remote_config = config.find_remote(&manifest_config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  let branch = branch.as_ref().unwrap_or(&manifest_config.default_branch);
  let file = file.as_ref().unwrap_or(&manifest_config.default_manifest_file);

  let tree_root = PathBuf::from(directory.unwrap_or(&branch));
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
  )
}

struct ProjectStatusDisplayData {
  location: String,
  branch: String,
  top_commit: String,
  files: Vec<String>,
}

impl ProjectStatusDisplayData {
  pub fn from_status(status: &tree::ProjectStatus) -> ProjectStatusDisplayData {
    let ahead = if status.ahead != 0 {
      Some(console::style(format!("↑{}", status.ahead)).green())
    } else {
      None
    };

    let behind = if status.behind != 0 {
      Some(console::style(format!("↓{}", status.behind)).red())
    } else {
      None
    };

    let ahead_behind = match (ahead, behind) {
      (Some(a), Some(b)) => format!(" [{} {}]", a, b),
      (Some(a), None) => format!(" [{}]", a),
      (None, Some(b)) => format!(" [{}]", b),
      (None, None) => "".to_string(),
    };

    let branch = match &status.branch {
      Some(branch) => BRANCH_STYLE.apply_to(format!("branch {}", branch)),
      None => console::style("no branch".to_string()).red(),
    };

    ProjectStatusDisplayData {
      location: PROJECT_STYLE.apply_to(format!("project {}", status.path)).to_string(),
      branch: format!("{}{}", branch, ahead_behind),
      top_commit: status.commit_summary.clone().unwrap_or_default(),
      files: status
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
        .collect(),
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
  status_under: Option<Vec<&str>>,
) -> Result<i32, Error> {
  let results = tree.status(Arc::clone(&config), &mut pool, status_under)?;
  let mut dirty = false;

  let column_padding = 4;
  let display_data = TreeStatusDisplayData::from_results(results.successful.iter().map(|r| &r.result).collect());
  for project in display_data.projects {
    dirty = true;

    let project_column = console::pad_str(
      project.location.as_str(),
      display_data.max_project_length + column_padding,
      console::Alignment::Left,
      None,
    );
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

  if dirty {
    Ok(1)
  } else {
    Ok(0)
  }
}

fn cmd_import(config: Arc<Config>, pool: &mut Pool, target_path: Option<&str>, copy: bool) -> Result<i32, Error> {
  let target_path = target_path.unwrap_or(".");
  let target_metadata = std::fs::metadata(target_path)?;

  // Make sure that we're actually in a repo tree.
  let target_path = Path::new(target_path);
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

fn get_overridable_option_value(matches: &clap::ArgMatches, enabled_name: &str, disabled_name: &str) -> Option<bool> {
  let last_enabled_index = matches.indices_of(enabled_name).map(Iterator::max);
  let last_disabled_index = matches.indices_of(disabled_name).map(Iterator::max);
  match (last_enabled_index, last_disabled_index) {
    (Some(enabled), Some(disabled)) => Some(enabled > disabled),
    (Some(_enabled), None) => Some(true),
    (None, Some(_disabled)) => Some(false),
    (None, None) => None,
  }
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

fn main() {
  let app = clap_app!(pore =>
    (version: crate_version!())
    (@setting SubcommandRequiredElseHelp)
    (@setting VersionlessSubcommands)

    (global_setting: clap::AppSettings::ColoredHelp)

    // The defaults for these have the wrong tense and capitalization.
    // TODO: Write an upstream patch to allow overriding these in the subcommends globally.
    (help_message: "print help message")
    (version_message: "print version information")

    (@arg CONFIG: -c --config +takes_value "override default config file path (~/.pore.toml)")
    (@arg CWD: -C +takes_value "run as if started in PATH instead of the current working directory")
    (@arg JOBS: -j +takes_value +global "number of jobs to use at a time, defaults to CPU_COUNT.")
    (@arg TRACE_FILE: -t --("trace_file") +takes_value "emit Chromium trace file to TRACE_FILE")
    (@arg VERBOSE: -v ... "increase verbosity")

    (@subcommand init =>
      (about: "checkout a new tree into the current directory")
      (@arg TARGET: +required
        "the target to checkout in the format <REMOTE>[/<BRANCH>[:MANIFEST]]\n\
         BRANCH defaults to master if unspecified"
      )
      (@arg GROUP_FILTERS: -g +takes_value
        "filter projects that satisfy a comma delimited list of groups\n\
         groups can be prepended with - to specifically exclude them"
      )
      (@arg LOCAL: -l "don't fetch; use only the local cache")
    )
    (@subcommand clone =>
      (about: "checkout a new tree into a new directory")
      (@arg TARGET: +required
        "the target to checkout in the format <MANIFEST>[/<BRANCH>[:MANIFEST_FILE]]\n\
         BRANCH defaults to master if unspecified"
      )
      (@arg DIRECTORY:
        "the directory to create and checkout the tree into.\n\
         defaults to BRANCH if unspecified"
      )
      (@arg GROUP_FILTERS: -g +takes_value
        "filter projects that satisfy a comma delimited list of groups\n\
         groups can be prepended with - to specifically exclude them"
      )
      (@arg LOCAL: -l "don't fetch; use only the local cache")
    )
    (@subcommand fetch =>
      (about: "fetch a tree's repositories without checking out")
      (@group targets =>
        (@arg FETCH_ALL: -a "fetch all remote refs (opposite of `repo sync -c`, which is the default behavior)")
        (@arg BRANCH: -b --branch +takes_value +multiple number_of_values(1)
          "specify a branch to fetch (can be used multiple times)"
        )
        (@arg FETCH_TAGS: -t --tags "fetch all remote tags")
      )
      (@arg PATH: ...
        "path(s) beneath which repositories are synced\n\
         defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand sync =>
      (about: "fetch and checkout a tree's repositories")
      (@arg LOCAL: -l "don't fetch; use only the local cache")
      (@arg FETCH_ALL: -a "fetch all remote refs (opposite of `repo sync -c`, which is the default behavior)")
      (@arg BRANCH: -b --branch +takes_value +multiple number_of_values(1)
        "specify a branch to fetch (can be used multiple times)"
      )
      (@arg REFS_ONLY: -r --("refs-only") "don't checkout, only update the refs")
      (@arg FETCH_TAGS: -t --tags "fetch all remote tags")
      (@arg PATH: ...
        "path(s) beneath which repositories are synced\n\
         defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand start =>
      (about: "start a branch in the current repository")
      (@arg BRANCH: +required "name of branch to create")
    )
    (@subcommand rebase =>
      (about: "rebase local branch onto upstream branch")
      (@arg INTERACTIVE: -i --interactive "pass --interactive to git rebase (single project only)")
      (@arg AUTOSQUASH: --autosquash "pass --autosquash to git rebase")
      (@arg PATH: ...
        "path(s) of the projects to rebase\n\
         defaults to all repositoriries in the tree"
      )
    )
    (@subcommand upload =>
      (about: "upload patches to Gerrit")
      (@arg PATH: ...
        "path(s) of the projects to be uploaded"
      )
      (@arg CURRENT_BRANCH: --cbr "upload current git branch")
      (@arg NO_VERIFY: --("no-verify") "skip pre-upload hooks")
      (@arg REVIEWERS: --re +takes_value
        "comma separated list of reviewers\n\
         user names without a domain will be assumed to be @google.com"
      )
      (@arg CC: --cc +takes_value
        "comma separated list of users to CC\n\
         user names without a domain will be assumed to be @google.com"
      )
      (@arg PRIVATE: --private "upload as private change")
      (@arg WIP: --wip "upload as work in progress change")
      (@arg BRANCH_NAME_AS_TOPIC: -t "use local branch name as topic")
      (@arg AUTOSUBMIT: --autosubmit +multiple "enable autosubmit")
      (@arg NO_AUTOSUBMIT: --("no-autosubmit") +multiple "do not enable autosubmit")
      (@arg PRESUBMIT: --presubmit +multiple "queue the change for presubmit")
      (@arg NO_PRESUBMIT: --("no-presubmit") +multiple "do not queue the change for presubmit")
      (@arg PS_DESCRIPTION: -m --message +takes_value "description to set for the new patch set")
      (@arg DRY_RUN: --("dry-run") "don't upload; just show upload commands")
    )
    (@subcommand prune =>
      (about: "prune branches that have been merged")
      (@arg PATH: ...
         "path(s) to prune\n\
          defaults to all repositories in the tree"
      )
    )
    (@subcommand status =>
      (about: "show working tree status across the entire tree")
      (@arg PATH: ...
        "path(s) beneath which to calculate status\n\
         defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand forall =>
      (about: "run a command in each project in the tree")
      (after_help: indoc!("
        Commands will be run with the current working directory inside each project,
        and with the following environment variables defined:

          $PORE_ROOT       absolute path of the root of the tree
          $PORE_ROOT_REL   relative path from the project to the root of the tree
          $REPO_PROJECT    name of the git repository in the remote
          $REPO_PATH       relative path from the root of the tree to project"
      ))
      (@arg PATH: ...
         "path(s) beneath which to run commands\n\
          defaults to all repositories in the tree if unspecified"
      )
      (@arg COMMAND: -c +takes_value +required
        "command to run."
      )
    )
    (@subcommand preupload =>
      (about: "run repo's preupload hooks")
      (@arg PATH: ...
         "path(s) beneath which to run preupload hooks\n\
          defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand import =>
      (about: "import an existing repo tree into a depot")
      (@arg COPY: -c --copy "copy objects instead of hard-linking")
      (@arg DIRECTORY:
        "the repo mirror to import from.\n\
         defaults to the current working directory if unspecified"
      )
    )
    (@subcommand list =>
      (about: "list repositories")
    )
    (@subcommand find_deleted =>
      (name: "find-deleted")
      (about: "find projects that were removed from the manifest")
    )
    (@subcommand manifest =>
      (about: "generate a manifest corresponding to the current state of the tree")
      (@arg OUTPUT: -o --output +takes_value value_name("FILE")
        "write result to FILE instead of to standard output"
      )
    )
    (@subcommand config =>
      (about: "prints the parsed configuration file")
      (@arg DEFAULT: --default "print the default configuration")
    )
  );

  let matches = app.get_matches();

  if let Some(trace_file) = matches.value_of("TRACE_FILE") {
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

  if let Some(cwd) = matches.value_of("CWD") {
    if let Err(err) = std::env::set_current_dir(&cwd) {
      fatal!("failed to set working directory to {}: {}", cwd, err);
    }
  }
  let cwd = std::env::current_dir().expect("failed to get current working directory");

  set_trace_id();

  let repo_compat = std::env::args().next() == Some("repo".into());
  let config_path = match matches.value_of("CONFIG") {
    Some(path) => {
      info!("using provided config path {:?}", path);
      PathBuf::from(path)
    }

    None => {
      let path = dirs::home_dir()
        .expect("failed to find home directory")
        .join(".pore.toml");
      info!("using default config path {:?}", path);
      path
    }
  };

  let config = match config::Config::from_path(&config_path) {
    Ok(config) => Arc::new(config),

    Err(err) => {
      eprintln!(
        "warning: failed to read config file at {:?}, falling back to default config: {}",
        config_path, err,
      );
      Arc::new(config::Config::default())
    }
  };

  let cmd = matches.subcommand();
  let pool_size = matches.value_of("JOBS").map(|job_str| {
    if let Ok(jobs) = job_str.parse::<i32>() {
      jobs
    } else {
      fatal!("failed to parse jobs value: {}", job_str);
    }
  }).or_else(|| {
    // Command-specific override
    config.parallelism.get(cmd.0).cloned().or_else(|| {
      // Global override
      config.parallelism.get("global").cloned()
    })
  }).unwrap_or(0);

  let num_cpus = num_cpus::get();
  let pool_size = if pool_size == 0 {
    num_cpus
  } else if pool_size < 0 {
    std::cmp::min(num_cpus, (-pool_size) as usize)
  } else {
    pool_size as usize
  };
  let mut pool = Pool::with_size(pool_size);

  let update_checker = if config.update_check && isatty::stdout_isatty() {
    Some(UpdateChecker::fetch())
  } else {
    None
  };

  let result = || -> Result<i32, Error> {
    match cmd {
      ("init", Some(submatches)) => {
        let fetch = !submatches.is_present("LOCAL");
        cmd_clone(
          Arc::clone(&config),
          &mut pool,
          &submatches.value_of("TARGET").unwrap(),
          Some("."),
          submatches.value_of("GROUP_FILTERS"),
          fetch,
        )
      }

      ("clone", Some(submatches)) => {
        let fetch = !submatches.is_present("LOCAL");
        cmd_clone(
          Arc::clone(&config),
          &mut pool,
          &submatches.value_of("TARGET").unwrap(),
          submatches.value_of("DIRECTORY"),
          submatches.value_of("GROUP_FILTERS"),
          fetch,
        )
      }

      ("fetch", Some(submatches)) => {
        let mut tree = Tree::find_from_path(cwd)?;
        let fetch_under = submatches.values_of("PATH").map(Iterator::collect);

        let branches: Option<Vec<_>> = submatches.values_of("BRANCH").map(Iterator::collect);
        let fetch_all = submatches.is_present("FETCH_ALL");
        let fetch_tags = submatches.is_present("FETCH_TAGS") || fetch_all;

        let fetch_target = {
          if fetch_all {
            FetchTarget::All
          } else if branches.is_none() {
            FetchTarget::Upstream
          } else {
            let branches = branches.unwrap().iter().map(|s| s.to_string()).collect();
            FetchTarget::Specific(branches)
          }
        };

        tree.sync(
          Arc::clone(&config),
          &mut pool,
          fetch_under,
          FetchType::Fetch,
          fetch_target,
          CheckoutType::NoCheckout,
          fetch_tags,
        )
      }

      ("sync", Some(submatches)) => {
        let fetch_type = if submatches.is_present("LOCAL") {
          FetchType::NoFetch
        } else {
          FetchType::Fetch
        };
        let mut tree = Tree::find_from_path(cwd)?;
        let sync_under = submatches.values_of("PATH").map(Iterator::collect);

        let branches: Option<Vec<_>> = submatches.values_of("BRANCH").map(Iterator::collect);
        let fetch_all = submatches.is_present("FETCH_ALL");
        let fetch_tags = submatches.is_present("FETCH_TAGS") || fetch_all;

        let fetch_target = {
          if fetch_all {
            FetchTarget::All
          } else if branches.is_none() {
            FetchTarget::Upstream
          } else {
            let branches = branches.unwrap().iter().map(|s| s.to_string()).collect();
            FetchTarget::Specific(branches)
          }
        };
        let refs_only = submatches.is_present("REFS_ONLY");
        tree.sync(
          Arc::clone(&config),
          &mut pool,
          sync_under,
          fetch_type,
          fetch_target,
          if refs_only {
            CheckoutType::RefsOnly
          } else {
            CheckoutType::Checkout
          },
          fetch_tags,
        )
      }

      ("start", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd.clone())?;
        let branch_name = submatches.value_of("BRANCH").unwrap();

        let remote_config = config.find_remote(&tree.config.remote)?;
        let depot = config.find_depot(&remote_config.depot)?;

        tree.start(Arc::clone(&config), &depot, branch_name, &cwd)
      }

      ("upload", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let autosubmit = get_overridable_option_value(&submatches, "AUTOSUBMIT", "NO_AUTOSUBMIT");
        let presubmit = get_overridable_option_value(&submatches, "PRESUBMIT", "NO_PRESUBMIT");

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
          submatches.values_of("PATH").map(Iterator::collect),
          submatches.is_present("CURRENT_BRANCH"),
          submatches.is_present("NO_VERIFY"),
          &user_string_to_vec(submatches.value_of("REVIEWERS")),
          &user_string_to_vec(submatches.value_of("CC")),
          submatches.is_present("PRIVATE"),
          submatches.is_present("WIP"),
          submatches.is_present("BRANCH_NAME_AS_TOPIC"),
          autosubmit.unwrap_or(config.autosubmit),
          presubmit.unwrap_or(config.presubmit),
          submatches.value_of("PS_DESCRIPTION"),
          submatches.is_present("DRY_RUN"),
        )
      }

      ("prune", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let remote_config = config.find_remote(&tree.config.remote)?;
        let depot = config.find_depot(&remote_config.depot)?;

        let prune_under = submatches.values_of("PATH").map(Iterator::collect);
        tree.prune(config, &mut pool, &depot, prune_under)
      }

      ("rebase", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let interactive = submatches.is_present("INTERACTIVE");
        let autosquash = submatches.is_present("AUTOSQUASH");
        let rebase_under = submatches.values_of("PATH").map(Iterator::collect);
        tree.rebase(config, &mut pool, interactive, autosquash, rebase_under)
      }

      ("status", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let status_under = submatches.values_of("PATH").map(Iterator::collect);
        cmd_status(Arc::clone(&config), &mut pool, &tree, status_under)
      }

      ("manifest", Some(submatches)) => {
        let output = submatches.value_of("OUTPUT");
        let tree = Tree::find_from_path(cwd)?;
        tree.generate_manifest(Arc::clone(&config), &mut pool, output)
      }

      ("forall", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let forall_under = submatches.values_of("PATH").map(Iterator::collect);
        let command = submatches
          .value_of("COMMAND")
          .ok_or_else(|| format_err!("no commands specified"))?;

        tree.forall(Arc::clone(&config), &mut pool, forall_under, command, repo_compat)
      }

      ("preupload", Some(submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        let preupload_under = submatches.values_of("PATH").map(Iterator::collect);
        tree.preupload(config, &mut pool, preupload_under)
      }

      ("import", Some(submatches)) => cmd_import(
        config,
        &mut pool,
        submatches.value_of("DIRECTORY"),
        submatches.is_present("COPY"),
      ),

      ("list", Some(_submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        tree.list(config)
      }

      ("find-deleted", Some(_submatches)) => {
        let tree = Tree::find_from_path(cwd)?;
        tree.find_deleted(config, &mut pool)
      }

      ("config", Some(submatches)) => {
        if submatches.is_present("DEFAULT") {
          println!("{}", Config::default_string());
        } else {
          println!("{}", toml::to_string_pretty(config.as_ref())?);
        }
        Ok(0)
      }

      _ => {
        unreachable!();
      }
    }
  }();

  if let Some(u) = update_checker {
    u.finish();
  }

  match result {
    Ok(rc) => std::process::exit(rc),
    Err(err) => {
      writeln!(&mut ::std::io::stderr(), "fatal: {:#}", err).unwrap();
    }
  }
}
