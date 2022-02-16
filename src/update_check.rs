use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::Error;

#[derive(Debug, Serialize, Deserialize)]
struct VersionFile {
  versions: Vec<Version>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
  pub number: String,
  pub date: String,
  pub changes: Vec<String>,
}

pub struct UpdateChecker {
  result: mpsc::Receiver<Result<Option<Vec<Version>>, Error>>,
}

const UPDATE_URL: &str = "https://raw.githubusercontent.com/jmgao/pore/master/versions.yaml";

impl UpdateChecker {
  fn parse(data: String) -> Result<Option<Vec<Version>>, Error> {
    Ok(Some(serde_yaml::from_str::<VersionFile>(&data)?.versions))
  }

  pub fn fetch() -> UpdateChecker {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
      if let Ok(response) = reqwest::blocking::get(UPDATE_URL) {
        if let Ok(text) = response.text() {
          let _ = tx.send(UpdateChecker::parse(text));
          return;
        }
      }

      // Don't error if we can't reach the server.
      let _ = tx.send(Ok(None));
    });

    UpdateChecker { result: rx }
  }

  #[allow(dead_code)]
  pub fn from_file(path: &Path) -> UpdateChecker {
    let (tx, rx) = mpsc::sync_channel(1);
    let result = || -> Result<Option<Vec<Version>>, Error> {
      let data = std::fs::read_to_string(path)?;
      UpdateChecker::parse(data)
    }();
    tx.send(result).unwrap();
    UpdateChecker { result: rx }
  }

  pub fn filter_new_versions(versions: Vec<Version>) -> Vec<Version> {
    let mut result = Vec::new();
    let current_version = version_compare::Version::from(crate_version!());
    for version in versions {
      if version.number == "unreleased" {
        continue;
      }

      if current_version < version_compare::Version::from(&version.number) {
        result.push(version.clone());
      }
    }
    result
  }

  pub fn finish(self) {
    let result = self.result.recv();
    if let Ok(Ok(Some(result))) = result {
      let filtered = UpdateChecker::filter_new_versions(result);
      if !filtered.is_empty() {
        println!("pore v{} ({}) is available!", filtered[0].number, filtered[0].date);
        println!("");
        for version in filtered {
          println!("  {} ({})", version.number, version.date);
          for change in version.changes {
            println!("    {}", change);
          }
        }
      }
    }
  }
}
