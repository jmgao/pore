## pore

pore is a reimplementation of Android's repository management tool, [repo](https://gerrit.googlesource.com/git-repo/),
with a focus on performance. Tree-wide operations such as `status` and `sync` are up to [10 times faster](https://asciinema.org/a/2kSTE803umfAQQR9SR7GP8rCc) in pore. Additionally, pore always does the equivalent of repo's `--reference` transparently, so
a fresh checkout of a new tree takes on the order of one minute, instead of tens of minutes.

### Installation and usage

pore requires a nightly version of rust to compile. Follow the instructions at https://rustup.rs/, select nightly, and
build pore with `cargo build --release`. By default, pore uses a configuration suited for AOSP development that stores
its mirror in `~/.pore/android`. If you wish to change this, either use a symlink, or edit the output of `pore config`
and save it to `~/.pore.toml`.

### Caveats

* `pore upload` is currently unimplemented: use `git push aosp foo:refs/for/master` instead (but be aware that repohooks
  will be skipped).
* libgit2 [doesn't support shallow clones](https://github.com/libgit2/libgit2/issues/3058), which makes the initial
  sync take a substantial amount of time and eat up a lot of disk space (~100 GB in the depot for `aosp/master`).
* Calling out to git-remote-helpers is currently unimplemented.
* `pore sync` currently does not warn when a project is removed from the manifest.

### License

This project is licensed under the Apache License, Version 2.0.
