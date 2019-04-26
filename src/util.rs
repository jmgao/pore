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

use std::fmt::Debug;
use std::io;
use std::path::Path;

use failure::{Error, ResultExt};

pub fn assert_empty_directory<T: AsRef<Path> + Debug>(directory_path: T) -> Result<(), Error> {
  match std::fs::read_dir(&directory_path) {
    Ok(dir) => {
      let children: Vec<io::Result<std::fs::DirEntry>> = dir.collect();
      if !children.is_empty() {
        bail!(
          "destination path {:?} already exists and is not an empty directory",
          directory_path
        );
      }
    }

    Err(err) => {
      bail!("failed to open directory {:?}: {}", directory_path, err);
    }
  }

  Ok(())
}

/// Create a symlink, or if it already exists, check that it points to the right place.
pub fn create_symlink<T: AsRef<Path> + Debug, U: AsRef<Path> + Debug>(target: T, symlink_path: U) -> Result<(), Error> {
  let target = target.as_ref();
  let symlink_path = symlink_path.as_ref();

  match std::os::unix::fs::symlink(target, symlink_path) {
    Ok(()) => Ok(()),
    Err(err) => {
      if err.kind() == std::io::ErrorKind::AlreadyExists {
        let actual_target =
          std::fs::read_link(symlink_path).context(format_err!("failed to readlink {:?}", symlink_path))?;
        if actual_target != target {
          bail!(
            "mismatch in symlink {:?}, wanted {:?}, actual {:?}",
            symlink_path,
            target,
            actual_target
          );
        } else {
          Ok(())
        }
      } else {
        Err(err).context(format_err!("failed to readlink"))?
      }
    }
  }
}

pub fn parse_revision<T: AsRef<str>, U: AsRef<str>>(
  repo: &git2::Repository,
  remote: T,
  revision: U,
) -> Result<git2::Object, Error> {
  let remote: &str = remote.as_ref();
  let revision: &str = revision.as_ref();

  // Revision can point either directly to a git hash, or to a remote branch.
  // Try <remote>/<revision> first, then fall back to <revision>, so that in
  // the case where there's a local master branch and a remote master branch,
  // we correctly pick the remote one.
  let object = match repo.revparse_single(&format!("{}/{}", remote, revision)) {
    Ok(obj) => obj,
    Err(_) => repo.revparse_single(&revision).context(format!(
      "failed to find revision {} from remote {} in {:?}",
      revision,
      remote,
      repo.path()
    ))?,
  };

  Ok(object)
}

fn find_independent_commits_inner(
  from: &git2::Commit,
  to: &git2::Commit,
  mut accumulator: &mut Vec<git2::Oid>,
) -> Result<(), Error> {
  if from.id() == to.id() {
    return Ok(());
  }

  accumulator.push(from.id());
  for parent in from.parents() {
    find_independent_commits_inner(&parent, to, &mut accumulator)?;
  }
  Ok(())
}

/// Find the commits that will be added when merging `src` into `dst`.
///
/// Roughly equivalent to `git show-branch --independent src dst`.
pub fn find_independent_commits(
  repo: &git2::Repository,
  src: &git2::Commit,
  dst: &git2::Commit,
) -> Result<Vec<git2::Oid>, Error> {
  let src_id = src.id();
  let dst_id = dst.id();
  let common_ancestor = repo.merge_base(src_id, dst_id).context(format_err!(
    "could not find common ancestor of commits {:?} and {:?}",
    src,
    dst
  ))?;

  let mut accumulator = Vec::new();
  find_independent_commits_inner(
    &src,
    &repo
      .find_commit(common_ancestor)
      .context(format_err!("could not find commit matching {}", common_ancestor))?,
    &mut accumulator,
  )?;
  Ok(accumulator)
}

pub fn read_line() -> Result<String, Error> {
  let mut line = String::new();
  std::io::stdin().read_line(&mut line)?;
  if line.ends_with('\n') {
    line.pop();
  }
  Ok(line)
}
