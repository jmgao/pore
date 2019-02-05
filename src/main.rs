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

#![allow(unused_variables)]

#[macro_use]
extern crate log;

#[macro_use]
extern crate failure;

extern crate futures;

#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate serde_xml_rs;
extern crate toml;

#[macro_use]
extern crate clap;

#[macro_use]
extern crate maplit;

extern crate dirs;
extern crate pathdiff;
extern crate shellexpand;

extern crate git2;

extern crate console;
extern crate indicatif;

use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use failure::Error;
use failure::ResultExt;
use futures::executor::ThreadPool;

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
mod manifest;
mod tree;
mod util;

use config::Config;
use manifest::Manifest;
use tree::{CheckoutType, FetchType, Tree};

fn unimplemented_subcommand(function: &str) -> ! {
  fatal!("unimplemented subcommand {}", function);
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
  mut pool: &mut ThreadPool,
  target: &str,
  directory: Option<&str>,
  fetch: bool,
) -> Result<i32, Error> {
  let (remote, branch) = parse_target(target)?;
  let remote_config = config.find_remote(&remote)?;
  let depot = config.find_depot(&remote_config.depot)?;

  let tree_root = PathBuf::from(directory.unwrap_or(&branch));
  if let Err(err) = std::fs::create_dir_all(&tree_root) {
    bail!("failed to create tree root {:?}: {}", tree_root, err);
  }

  // TODO: Add locking?
  let mut tree = Tree::construct(&depot, &tree_root, &remote_config, &branch, fetch)?;
  let fetch_type = if fetch {
    // We just fetched the manifest.
    FetchType::FetchExceptManifest
  } else {
    FetchType::NoFetch
  };

  tree.sync(&config, &mut pool, &depot, None, fetch_type, CheckoutType::NoCheckout)
}

fn cmd_sync(
  config: Config,
  mut pool: &mut ThreadPool,
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

fn cmd_prune(config: Config, mut pool: &mut ThreadPool, tree: &mut Tree) -> Result<i32, Error> {
  let remote_config = config.find_remote(&tree.config.remote)?;
  let depot = config.find_depot(&remote_config.depot)?;
  tree.prune(&config, &mut pool, &depot)
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

    Err(err) => {
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
        ThreadPool::builder().pool_size(jobs).create()
      } else {
        fatal!("failed to parse jobs value: {}", jobs);
      }
    }

    None => ThreadPool::new(),
  }
  .unwrap_or_else(|err| fatal!("failed to create job pool: {}", err));

  let result = || -> Result<i32, Error> {
    match matches.subcommand() {
      ("init", Some(submatches)) => {
        let fetch = !submatches.is_present("LOCAL");
        cmd_clone(
          config,
          &mut pool,
          &submatches.value_of("TARGET").unwrap(),
          Some("."),
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

      ("upload", Some(submatches)) => unimplemented_subcommand("upload"),

      ("prune", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let mut tree = Tree::find_from_path(cwd.clone())?;
        cmd_prune(config, &mut pool, &mut tree)
      }

      ("status", Some(submatches)) => {
        let cwd = std::env::current_dir().context("failed to get current working directory")?;
        let tree = Tree::find_from_path(cwd.clone())?;
        let status_under = submatches.values_of("PATH").map(|values| values.collect());
        tree.status(config, &mut pool, status_under)
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
