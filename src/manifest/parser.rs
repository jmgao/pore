use failure::{Error, ResultExt};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::manifest::*;

/// Assign a value to an Option after asserting that it is None.
macro_rules! populate_option {
  ($option: expr, $value: expr) => {{
    if $option.is_some() {
      bail!("{} already has a value", stringify!($option))
    }
    $option = Some($value);
  }};
}

pub fn parse(data: &str) -> Result<Manifest, Error> {
  let mut manifest = None;

  let mut reader = Reader::from_str(data);
  reader.trim_text(true);

  let mut buf = Vec::new();
  loop {
    let event = reader
      .read_event(&mut buf)
      .context(format!("failed to parse XML at position {}", reader.buffer_position()))?;

    match event {
      Event::Start(e) => {
        let tag_name = e.name();
        match tag_name {
          b"manifest" => match manifest {
            Some(_) => bail!("multiple manifest tags in manifest"),
            None => {
              manifest = Some(parse_manifest(&e, &mut reader)?);
            }
          },

          _ => bail!(
            "unexpected start tag in manifest.xml: {}",
            std::str::from_utf8(tag_name).unwrap_or("???")
          ),
        }
      }

      Event::Empty(e) => bail!(
        "unexpected empty element in manifest.xml: {}",
        std::str::from_utf8(e.name()).unwrap_or("???")
      ),

      Event::Eof => break,

      Event::Decl(_) => {}
      Event::Comment(_) => {}

      e => bail!(
        "unexpected event in manifest.xml at position {}: {:?}",
        reader.buffer_position(),
        e
      ),
    }
  }

  Ok(manifest.ok_or_else(|| format_err!("failed to find a manifest tag"))?)
}

fn parse_manifest(_event: &BytesStart, mut reader: &mut Reader<&[u8]>) -> Result<Manifest, Error> {
  let mut manifest = Manifest::default();
  let mut buf = Vec::new();
  loop {
    let event = reader
      .read_event(&mut buf)
      .context(format!("failed to parse XML at position {}", reader.buffer_position()))?;

    match event {
      Event::Start(e) => {
        let tag_name = e.name();
        match tag_name {
          b"project" => {
            let project = parse_project(&e, &mut reader, true)?;
            let path = PathBuf::from(project.path());
            if manifest.projects.contains_key(&path) {
              bail!("duplicate project {:?}", path);
            }
            manifest.projects.insert(path, project);
          }

          _ => bail!(
            "unexpected start tag in <manifest>: {}",
            std::str::from_utf8(tag_name).unwrap_or("???")
          ),
        }
      }

      Event::Empty(e) => match e.name() {
        b"project" => {
          let project = parse_project(&e, &mut reader, false)?;
          let path = PathBuf::from(project.path());
          if manifest.projects.contains_key(&path) {
            bail!("duplicate project {:?}", path);
          }
          manifest.projects.insert(path, project);
        }

        b"remote" => {
          let remote = parse_remote(&e, &reader)?;
          if manifest.remotes.contains_key(&remote.name) {
            bail!("duplicate remotes with name {}", remote.name);
          }
          manifest.remotes.insert(remote.name.clone(), remote);
        }

        b"default" => populate_option!(manifest.default, parse_default(&e, &reader)?),
        b"manifest-server" => populate_option!(manifest.manifest_server, parse_manifest_server(&e, &reader)?),
        b"repo-hooks" => populate_option!(manifest.repo_hooks, parse_repo_hooks(&e, &reader)?),

        _ => bail!(
          "unexpected empty element in <manifest>: {}",
          std::str::from_utf8(e.name()).unwrap_or("???")
        ),
      },

      Event::End(_) => break,

      Event::Comment(_) => {}

      e => bail!(
        "unexpected event in <manifest> at position {}: {:?}",
        reader.buffer_position(),
        e
      ),
    }
  }

  Ok(manifest)
}

fn parse_remote(event: &BytesStart, reader: &Reader<&[u8]>) -> Result<Remote, Error> {
  let mut remote = Remote::default();
  let mut name = None;
  let mut fetch = None;

  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"name" => populate_option!(name, value),
      b"alias" => populate_option!(remote.alias, value),
      b"fetch" => populate_option!(fetch, value),
      b"review" => populate_option!(remote.review, value),
      b"revision" => populate_option!(remote.revision, value),
      key => eprintln!(
        "warning: unexpected attribute in <remote>: {}",
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }

  ensure!(name != None, "name not specified in <remote>");
  ensure!(fetch != None, "fetch not specified in <remote>");
  remote.name = name.unwrap();
  remote.fetch = fetch.unwrap();

  Ok(remote)
}

fn parse_default(event: &BytesStart, reader: &Reader<&[u8]>) -> Result<Default, Error> {
  let mut default = Default::default();

  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"revision" => populate_option!(default.revision, value),
      b"remote" => populate_option!(default.remote, value),
      b"sync-j" => populate_option!(default.sync_j, value.parse::<u32>().context("failed to parse sync-j")?),
      b"sync-c" => populate_option!(default.sync_j, value.parse::<u32>().context("failed to parse sync-c")?),

      b"upstream" => {
        // Ignored attribute. Used to limit the scope of the fetch with -c when a project is pinned
        // to a revision, but we just fetch the revision itself rather than the full upstream
        // branch like repo does.
      }

      key => eprintln!(
        "warning: unexpected attribute in <default>: {}",
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }

  Ok(default)
}

fn parse_manifest_server(event: &BytesStart, reader: &Reader<&[u8]>) -> Result<ManifestServer, Error> {
  let mut url = None;
  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"url" => populate_option!(url, value),
      key => eprintln!(
        "warning: unexpected attribute in <manifest-server>: {}",
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }

  ensure!(url != None, "url not specified in <manifest-server>");
  Ok(ManifestServer { url: url.unwrap() })
}

fn parse_project(event: &BytesStart, reader: &mut Reader<&[u8]>, has_children: bool) -> Result<Project, Error> {
  let mut project = Project::default();
  let mut name = None;
  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"name" => populate_option!(name, value),
      b"path" => populate_option!(project.path, value),
      b"remote" => populate_option!(project.remote, value),
      b"revision" => populate_option!(project.revision, value),
      b"dest-branch" => populate_option!(project.dest_branch, value),
      b"groups" => populate_option!(project.groups, value.split(',').map(ToString::to_string).collect()),
      b"sync-c" => populate_option!(project.sync_c, value.parse::<bool>().context("failed to parse sync-c")?),
      b"clone-depth" => populate_option!(
        project.clone_depth,
        value.parse::<u32>().context("failed to parse clone-depth")?
      ),

      b"upstream" => {
        // Unnecessary attribute. Used to limit the scope of the fetch with -c when a project is
        // pinned to a revision, but we just fetch the revision itself rather than the full
        // upstream branch like repo does.
      }

      key => eprintln!(
        "warning: unexpected attribute in <project>: {}",
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }

  ensure!(name != None, "name not specified in <project>");
  project.name = name.unwrap();

  if has_children {
    let mut buf = Vec::new();
    loop {
      let event = reader
        .read_event(&mut buf)
        .context(format!("failed to parse XML at position {}", reader.buffer_position()))?;

      match event {
        Event::Start(e) => bail!(
          "unexpected start tag in <project>: {}",
          std::str::from_utf8(e.name()).unwrap_or("???")
        ),

        Event::Empty(e) => match e.name() {
          b"copyfile" => {
            let op = parse_file_operation(&e, &reader, true)?;
            project.file_operations.push(op);
          }

          b"linkfile" => {
            let op = parse_file_operation(&e, &reader, false)?;
            project.file_operations.push(op);
          }

          _ => bail!(
            "unexpected empty element in <project>: {}",
            std::str::from_utf8(e.name()).unwrap_or("???")
          ),
        },

        Event::End(_) => break,

        Event::Comment(_) => {}

        e => bail!(
          "unexpected event in <project> at position {}: {:?}",
          reader.buffer_position(),
          e
        ),
      }
    }
  }

  Ok(project)
}

fn parse_file_operation(event: &BytesStart, reader: &Reader<&[u8]>, copy: bool) -> Result<FileOperation, Error> {
  let op_name = if copy { "copyfile" } else { "linkfile" };

  let mut src = None;
  let mut dst = None;
  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"src" => populate_option!(src, value),
      b"dest" => populate_option!(dst, value),
      key => eprintln!(
        "warning: unexpected attribute in <{}>: {}",
        op_name,
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }

  ensure!(src != None, "src not specified in <{}>", op_name);
  ensure!(dst != None, "dest not specified in <{}>", op_name);

  if copy {
    Ok(FileOperation::CopyFile {
      src: src.unwrap(),
      dst: dst.unwrap(),
    })
  } else {
    Ok(FileOperation::LinkFile {
      src: src.unwrap(),
      dst: dst.unwrap(),
    })
  }
}

fn parse_repo_hooks(event: &BytesStart, reader: &Reader<&[u8]>) -> Result<RepoHooks, Error> {
  let mut hooks = RepoHooks::default();
  for attribute in event.attributes() {
    let attribute = attribute?;
    let value = attribute.unescape_and_decode_value(&reader)?;
    match attribute.key {
      b"in-project" => populate_option!(hooks.in_project, value),
      b"enabled-list" => populate_option!(hooks.enabled_list, value),
      key => eprintln!(
        "warning: unexpected attribute in <repo-hooks>: {}",
        std::str::from_utf8(key).unwrap_or("???")
      ),
    }
  }
  Ok(hooks)
}
