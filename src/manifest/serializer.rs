use super::{FileOperation, Manifest};
use std::io::Write;

use anyhow::{Context, Error};

use minidom::element::Element;
use quick_xml::Writer;

macro_rules! populate_from_option {
  ($elem: expr, $option: expr, $attribute_name: expr) => {{
    if let Some(x) = &$option {
      $elem = $elem.attr($attribute_name, x.to_string());
    }
  }};
}

pub fn serialize(manifest: &Manifest, mut output: Box<dyn Write>) -> Result<(), Error> {
  let mut root = Element::builder("manifest").build();

  for remote in manifest.remotes.values() {
    let mut elem = Element::builder("remote");
    elem = elem.attr("name", &remote.name);
    elem = elem.attr("fetch", &remote.fetch);
    populate_from_option!(elem, remote.alias, "alias");
    populate_from_option!(elem, remote.review, "review");
    root.append_child(elem.build());
  }

  if let Some(default) = &manifest.default {
    let mut elem = Element::builder("default");
    populate_from_option!(elem, default.revision, "revision");
    populate_from_option!(elem, default.remote, "remote");
    populate_from_option!(elem, default.sync_j, "sync-j");
    populate_from_option!(elem, default.sync_c, "sync-c");
    root.append_child(elem.build());
  }

  if let Some(manifest_server) = &manifest.manifest_server {
    root.append_child(
      Element::builder("manifest-server")
        .attr("url", &manifest_server.url)
        .build(),
    );
  }

  if let Some(repo_hooks) = &manifest.repo_hooks {
    let mut elem = Element::builder("repo-hooks");
    populate_from_option!(elem, repo_hooks.in_project, "in-project");
    populate_from_option!(elem, repo_hooks.enabled_list, "enabled-list");
    root.append_child(elem.build());
  }

  for project in manifest.projects.values() {
    let mut elem = Element::builder("project");
    populate_from_option!(elem, project.path, "path");
    populate_from_option!(elem, project.remote, "remote");
    populate_from_option!(elem, project.revision, "revision");
    populate_from_option!(elem, project.dest_branch, "dest-branch");
    populate_from_option!(elem, project.sync_c, "sync-c");
    populate_from_option!(elem, project.clone_depth, "clone-depth");

    let groups = project.groups.clone().map(|vec| vec.join(","));
    populate_from_option!(elem, groups, "groups");

    for file_op in &project.file_operations {
      let child = match file_op {
        FileOperation::LinkFile { src, dst } => Element::builder("linkfile").attr("src", src).attr("dest", dst).build(),
        FileOperation::CopyFile { src, dst } => Element::builder("copyfile").attr("src", src).attr("dest", dst).build(),
      };
      elem = elem.append(child);
    }

    root.append_child(elem.build());
  }

  let mut fancy_writer = Writer::new_with_indent(&mut output, b' ', 4);
  root.to_writer(&mut fancy_writer).context("failed to write manifest")?;
  output.write_all(b"\n")?;

  Ok(())
}
