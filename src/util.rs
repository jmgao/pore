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

pub fn parse_revision<T: AsRef<str>, U: AsRef<str>>(
  repo: &git2::Repository,
  remote: T,
  revision: U,
) -> Result<git2::Object, Error> {
  let remote: &str = remote.as_ref();
  let revision: &str = revision.as_ref();

  // Revision can point either directly to a git hash, or to a remote branch.
  // Try the git hash first.
  let object = match repo.revparse_single(&revision) {
    Ok(obj) => obj,
    Err(err) => {
      let spec = format!("{}/{}", remote, revision);
      repo
        .revparse_single(&spec)
        .context(format!("failed to find commit {} in {:?}", spec, repo.path()))?
    }
  };

  Ok(object)
}
