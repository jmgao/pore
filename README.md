pore
========

[![CircleCI](https://circleci.com/gh/jmgao/pore.svg?style=svg)](https://circleci.com/gh/jmgao/pore)

pore is a reimplementation of Android's repository management tool, [repo](https://gerrit.googlesource.com/git-repo/),
with a focus on performance. Tree-wide operations such as `status` and `sync` are up to [10 times faster](https://asciinema.org/a/2kSTE803umfAQQR9SR7GP8rCc) in pore. Additionally, pore always does the equivalent of repo's `--reference` transparently, so
a fresh checkout of a new tree takes on the order of one minute, instead of tens of minutes.

### Installation and usage

The following instructions probably work on a Debian-ish system:

```sh
sudo apt-get install -y build-essential ca-certificates curl git libssl-dev pkg-config ssh
curl https://sh.rustup.rs -sSf | sh -s -- -y
source $HOME/.cargo/env
cargo install --git https://github.com/jmgao/pore --force
```

By default, pore uses a configuration suited for AOSP development that stores its mirror in `~/.pore/android`.
If you wish to change this, either use a symlink, or edit the output of `pore config` and save it to `~/.pore.toml`.

### Caveats

* `pore sync` currently does not warn when a project is removed from the manifest.

### License

This project is licensed under the Apache License, Version 2.0.
