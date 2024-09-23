use super::{FileOperation, Manifest};
use std::borrow::Cow;
use std::io::Write;

use anyhow::Result;

use quick_xml::{events, Writer};

fn populate<'a, V: Into<Cow<'a, [u8]>>>(elem: &mut events::BytesStart, key: &'static str, value: V) {
  elem.push_attribute(events::attributes::Attribute {
    key: key.as_bytes(),
    value: value.into(),
  })
}

fn populate_from_str(elem: &mut events::BytesStart, key: &'static str, value: &str) {
  populate(elem, key, value.as_bytes())
}

fn populate_from_option<V: ToString>(elem: &mut events::BytesStart, key: &'static str, value: &Option<V>) {
  if let Some(value) = value {
    populate(elem, key, value.to_string().into_bytes())
  }
}

fn populate_from_str_option(elem: &mut events::BytesStart, key: &'static str, value: &Option<String>) {
  if let Some(value) = value.as_deref() {
    populate_from_str(elem, key, value)
  }
}

fn write_element<W: Write>(
  writer: &mut Writer<W>,
  name: &'static str,
  attributes: impl FnOnce(&mut events::BytesStart),
  children: impl FnOnce(&mut Writer<W>) -> Result<()>,
) -> Result<()> {
  let mut elem = events::BytesStart::borrowed_name(name.as_bytes());
  attributes(&mut elem);
  writer.write_event(events::Event::Start(elem))?;
  children(writer)?;
  writer.write_event(events::Event::End(events::BytesEnd::borrowed(name.as_bytes())))?;
  Ok(())
}

pub fn serialize(manifest: &Manifest, mut output: Box<dyn Write>) -> Result<()> {
  write_element(
    &mut Writer::new_with_indent(&mut output, b' ', 4),
    "manifest",
    |_| {},
    |writer| {
      for remote in manifest.remotes.values() {
        write_element(
          writer,
          "remote",
          |elem| {
            populate_from_str(elem, "name", &remote.name);
            populate_from_str(elem, "fetch", &remote.fetch);
            populate_from_str_option(elem, "alias", &remote.alias);
            populate_from_str_option(elem, "review", &remote.review);
          },
          |_| Ok(()),
        )?;
      }

      if let Some(default) = &manifest.default {
        write_element(
          writer,
          "default",
          |elem| {
            populate_from_str_option(elem, "revision", &default.revision);
            populate_from_str_option(elem, "remote", &default.remote);
            populate_from_option(elem, "sync-j", &default.sync_j);
            populate_from_option(elem, "sync-c", &default.sync_c);
          },
          |_| Ok(()),
        )?;
      }

      if let Some(manifest_server) = &manifest.manifest_server {
        write_element(
          writer,
          "manifest-server",
          |elem| {
            populate_from_str(elem, "url", &manifest_server.url);
          },
          |_| Ok(()),
        )?;
      }

      if let Some(repo_hooks) = &manifest.repo_hooks {
        write_element(
          writer,
          "repo-hooks",
          |elem| {
            populate_from_option(elem, "in-project", &repo_hooks.in_project);
            populate_from_option(elem, "enabled-list", &repo_hooks.enabled_list);
          },
          |_| Ok(()),
        )?;
      }

      for project in manifest.projects.values() {
        write_element(
          writer,
          "project",
          |elem| {
            populate_from_str(elem, "name", &project.name);
            populate_from_str_option(elem, "path", &project.path);
            populate_from_str_option(elem, "remote", &project.remote);
            populate_from_str_option(elem, "revision", &project.revision);
            populate_from_option(elem, "dest-branch", &project.dest_branch);
            populate_from_option(elem, "sync-c", &project.sync_c);
            populate_from_option(elem, "clone-depth", &project.clone_depth);
            if let Some(groups) = &project.groups {
              populate(elem, "groups", groups.join(",").into_bytes())
            }
          },
          |writer| {
            for file_op in &project.file_operations {
              let (name, src, dst) = match file_op {
                FileOperation::LinkFile { src, dst } => ("linkfile", src, dst),
                FileOperation::CopyFile { src, dst } => ("copyfile", src, dst),
              };
              write_element(
                writer,
                name,
                |elem| {
                  populate_from_str(elem, "src", src);
                  populate_from_str(elem, "dest", dst);
                },
                |_| Ok(()),
              )?;
            }
            Ok(())
          },
        )?;
      }
      Ok(())
    },
  )?;

  Ok(())
}
