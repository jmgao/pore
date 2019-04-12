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

#![feature(fnbox)]
#![feature(futures_api)]

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate log;

#[macro_use]
extern crate failure;

#[macro_use]
extern crate indoc;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate clap;

#[macro_use]
extern crate maplit;

use std::io::Write;
use std::path::{Path, PathBuf};

use failure::Error;
use failure::ResultExt;

use clap::{Arg, SubCommand};

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
mod pool;
mod tree;
mod util;

use config::Config;
use manifest::Manifest;
use pool::Pool;
use tree::{CheckoutType, FileState, FetchType, GroupFilter, Tree};

lazy_static! {
  static ref AOSP_REMOTE_STYLE: console::Style = { console::Style::new().bold().green() };
  static ref NON_AOSP_REMOTE_STYLE: console::Style = { console::Style::new().bold().red() };
  static ref SLASH_STYLE: console::Style = { console::Style::new().bold() };
  static ref BRANCH_STYLE: console::Style = { console::Style::new().bold().blue() };
  static ref PROJECT_STYLE: console::Style = { console::Style::new().bold() };
}

fn parse_target(target: &str) -> Result<(String, String), Error> {
  let vec: Vec<&str> = target.split('/').collect();
  if vec.len() == 1 {
    Ok((vec[0].into(), "master".into()))
  } else if vec.len() == 2 {
    Ok((vec[0].into(), vec[1].into()))
  } else {
    bail!("invalid target '{}'", target);
  }
}

fn cmd_clone(
  config: Config,
  mut pool: &mut Pool,
  target: &str,
  directory: Option<&str>,
  group_filters: Option<&str>,
  fetch: bool,
) -> Result<i32, Error> {
  let (remote, branch) = parse_target(target)?;
  let remote_config = config.find_remote(&remote)?;
  let depot = config.find_depot(&remote_config.depot)?;

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
  let mut tree = Tree::construct(&depot, &tree_root, &remote_config, &branch, group_filters, fetch)?;
  let fetch_type = if fetch {
    // We just fetched the manifest.
    FetchType::FetchExceptManifest
  } else {
    FetchType::NoFetch
  };

  tree.sync(&config, &mut pool, &depot, None, fetch_type, CheckoutType::Checkout)
}

fn cmd_sync(
  config: Config,
  mut pool: &mut Pool,
  tree: &mut Tree,
  sync_under: Option<Vec<&str>>,
  fetch: FetchType,
  checkout: CheckoutType,
) -> Result<i32, Error> {
  let remote_config = config.find_remote(&tree.config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  tree.sync(&config, &mut pool, &depot, sync_under, fetch, checkout)
}

fn cmd_start(config: Config, tree: &mut Tree, branch_name: &str, directory: &Path) -> Result<i32, Error> {
  let remote_config = config.find_remote(&tree.config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  tree.start(&config, &depot, &remote_config, branch_name, &directory)
}

fn user_string_to_vec(users: Option<&str>) -> Vec<String> {
  users
    .unwrap_or("")
    .split(",")
    .filter(|r| !r.is_empty())
    .map(|r| {
      if r.contains("@") {
        r.to_string()
      } else {
        format!("{}@google.com", r)
      }
    })
    .collect()
}

fn cmd_upload(
  config: Config,
  pool: &mut Pool,
  tree: &mut Tree,
  upload_under: Option<Vec<&str>>,
  current_branch: bool,
  no_verify: bool,
  reviewers: Option<&str>,
  cc: Option<&str>,
  private: bool,
  wip: bool,
  branch_name_as_topic: bool,
  autosubmit: bool,
  presubmit_ready: bool,
) -> Result<i32, Error> {
  tree.upload(
    &config,
    pool,
    upload_under,
    current_branch,
    no_verify,
    &user_string_to_vec(reviewers),
    &user_string_to_vec(cc),
    private,
    wip,
    branch_name_as_topic,
    autosubmit,
    presubmit_ready,
  )
}

fn cmd_prune(config: Config, mut pool: &mut Pool, tree: &mut Tree) -> Result<i32, Error> {
  let remote_config = config.find_remote(&tree.config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  tree.prune(&config, &mut pool, &depot)
}

fn cmd_forall(
  config: Config,
  mut pool: &mut Pool,
  tree: &mut Tree,
  forall_under: Option<Vec<&str>>,
  command: &str,
) -> Result<i32, Error> {
  tree.forall(&config, &mut pool, forall_under, command)
}

fn cmd_preupload(
  config: Config,
  mut pool: &mut Pool,
  tree: &mut Tree,
  preupload_under: Option<Vec<&str>>,
) -> Result<i32, Error> {
  tree.preupload(&config, &mut pool, preupload_under)
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

    (@arg CONFIG: -c --config +takes_value "override default config file path (~/.pore.cfg)")
    (@arg CWD: -C +takes_value "run as if started in PATH instead of the current working directory")
    (@arg JOBS: -j +takes_value +global "number of jobs to use at a time, defaults to CPU_COUNT.")
    (@arg VERBOSE: -v ... "increase verbosity")

    (@subcommand init =>
      (about: "checkout a new tree into the current directory")
      (@arg TARGET: +required
        "the target to checkout in the format <REMOTE>[/<BRANCH>]\n\
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
        "the target to checkout in the format <REMOTE>[/<BRANCH>]\n\
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
      (@arg PATH: ...
        "path(s) beneath which repositories are synced\n\
         defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand sync =>
      (about: "fetch and checkout a tree's repositories")
      (@arg LOCAL: -l "don't fetch; use only the local cache")
      (@arg PATH: ...
        "path(s) beneath which repositories are synced\n\
         defaults to all repositories in the tree if unspecified"
      )
    )
    (@subcommand start =>
      (about: "start a branch in the current repository")
      (@arg BRANCH: +required "name of branch to create")
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
      (@arg AUTOSUBMIT: --autosubmit "enable autosubmit")
      (@arg PRESUBMIT_READY: --presubmit "queue the change for presubmit")
    )
    (@subcommand prune =>
      (about: "prune branches that have been merged")
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
          $PORE_ROOT_REL   relative path from the project to the root of the tree"
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
    (@subcommand manifest =>
      (about: "generate a manifest corresponding to the current state of the tree")
      (@arg OUTPUT: -o --output +takes_value value_name("FILE")
        "write result to FILE instead of to standard output"
      )
    )
    (@subcommand config =>
      (about: "prints the default configuration file")
    )
  )
  .subcommand(
    SubCommand::with_name("parse-manifest")
      .about("parse a manifest file and print it")
      .setting(clap::AppSettings::Hidden)
      .arg(Arg::with_name("PATH").takes_value(true).required(true)),
  );

  let matches = app.get_matches();
  if let Some(cwd) = matches.value_of("CWD") {
    if let Err(err) = std::env::set_current_dir(&cwd) {
      fatal!("failed to set working directory to {}: {}", cwd, err);
    }
  }

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
    Ok(config) => config,

    Err(_err) => {
      info!(
        "failed to read config file at {:?}, falling back to default config",
        config_path
      );
      config::Config::default()
    }
  };

  let mut pool = match matches.value_of("JOBS") {
    Some(jobs) => {
      if let Ok(jobs) = jobs.parse::<usize>() {
        Pool::with_size(jobs)
      } else {
        fatal!("failed to parse jobs value: {}", jobs);
      }
    }

    None => Pool::with_default_size(),
  };

  let result = || -> Result<i32, Error> {
    match matches.subcommand() {
      ("init", Some(submatches)) => {
        let fetch = !submatches.is_present("LOCAL");
        cmd_clone(
          config,
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
          config,
          &mut pool,
          &submatches.value_of("TARGET").unwrap(),
          submatches.value_of("DIRECTORY"),
          submatches.value_of("GROUP_FILTERS"),
          fetch,
        )
      }

      ("fetch", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        let sync_under = submatches.values_of("PATH").map(|values| values.collect());
        cmd_sync(
          config,
          &mut pool,
          &mut tree,
          sync_under,
          FetchType::Fetch,
          CheckoutType::NoCheckout,
        )
      }

      ("sync", Some(submatches)) => {
        let fetch = if submatches.is_present("LOCAL") {
          FetchType::NoFetch
        } else {
          FetchType::Fetch
        };
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        let sync_under = submatches.values_of("PATH").map(|values| values.collect());
        cmd_sync(config, &mut pool, &mut tree, sync_under, fetch, CheckoutType::Checkout)
      }

      ("start", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        let branch_name = submatches.value_of("BRANCH").unwrap();
        cmd_start(config, &mut tree, branch_name, &cwd)
      }

      ("upload", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        cmd_upload(
          config,
          &mut pool,
          &mut tree,
          submatches.values_of("PATH").map(|values| values.collect()),
          submatches.is_present("CURRENT_BRANCH"),
          submatches.is_present("NO_VERIFY"),
          submatches.value_of("REVIEWERS"),
          submatches.value_of("CC"),
          submatches.is_present("PRIVATE"),
          submatches.is_present("WIP"),
          submatches.is_present("BRANCH_NAME_AS_TOPIC"),
          submatches.is_present("AUTOSUBMIT"),
          submatches.is_present("PRESUBMIT_READY"),
        )
      }

      ("prune", Some(_submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        cmd_prune(config, &mut pool, &mut tree)
      }

      ("status", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let tree = Tree::find_from_path(cwd.clone())?;
        let status_under = submatches.values_of("PATH").map(|values| values.collect());

        let results = tree.status(config, &mut pool, status_under)?;
        let mut dirty = false;
        for result in results.successful {
          let project_name = &result.name;
          let project_status = &result.result;
          if project_status.branch == None && project_status.files.is_empty() {
            continue;
          }

          dirty = true;

          let project_line = PROJECT_STYLE.apply_to(format!("project {:64}", project_name));
          let branch = match &project_status.branch {
            Some(branch) => BRANCH_STYLE.apply_to(format!("branch {}", branch)),
            None => console::style("(*** NO BRANCH ***)".to_string()).red(),
          };
          println!("{}{}", project_line, branch);

          for file in &project_status.files {
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

      ("manifest", Some(submatches)) => {
        let output = submatches.value_of("OUTPUT");
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let tree = Tree::find_from_path(cwd.clone())?;
        tree.generate_manifest(config, &mut pool, output)
      }

      ("forall", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        let forall_under = submatches.values_of("PATH").map(|values| values.collect());
        let command = submatches
          .value_of("COMMAND")
          .ok_or_else(|| format_err!("no commands specified"))?;
        cmd_forall(config, &mut pool, &mut tree, forall_under, command)
      }

      ("preupload", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        let preupload_under = submatches.values_of("PATH").map(|values| values.collect());
        cmd_preupload(config, &mut pool, &mut tree, preupload_under)
      }

      ("config", Some(_submatches)) => {
        println!("{}", toml::to_string_pretty(&Config::default())?);
        Ok(0)
      }

      ("parse-manifest", Some(submatches)) => {
        println!("{:?}", Manifest::parse_file(submatches.value_of("PATH").unwrap())?);
        Ok(0)
      }

      _ => {
        unreachable!();
      }
    }
  }();

  match result {
    Ok(rc) => std::process::exit(rc),
    Err(err) => {
      let fail = err.as_fail();
      if let Some(cause) = fail.cause() {
        let root = cause.find_root_cause();
        writeln!(&mut ::std::io::stderr(), "fatal: {}: {}", fail, root).unwrap();
      } else {
        writeln!(&mut ::std::io::stderr(), "fatal: {}", fail).unwrap();
      }
    }
  }
}
