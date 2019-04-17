# Getting Started

## Installation

pore currently requires a nightly version of rust to compile, due to the use of
the not-yet-stabilized futures API.

The following instructions probably work on a Debian-ish system:

```sh
sudo apt-get install -y \
    build-essential \
    ca-certificates \
    curl \
    git \
    libssl-dev \
    pkg-config \
    ssh
curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain nightly-2019-04-16
source $HOME/.cargo/env
cargo install --git https://github.com/jmgao/pore --force
```

## repo init

To clone a new repo tree, use `pore clone $REMOTE/$BRANCH`. For example, to
clone AOSP master:

```sh
pore clone aosp/master
```

This behaves similarly to the following repo workflow:

```sh
mkdir master
cd master
repo init -u https://android.googlesource.com/platform/manifest -b master
repo sync -c -j$NCPUS
```

pore will automatically create the new directory, determine the manifest URL,
and use an appropriate number of threads without additional configuration.

By default, pore is configured to use an AOSP remote with a mirror in
`~/.pore/android`. To configure additional remotes or change the mirror
location, see the [Configuration](./configuration.md) chapter.

## repo start

To create a new local development branch with pore:

```sh
pore start my-development-branch
```

Behavior matches that of repo: a new branch will be created rooted at the head
of the upstream branch.

## repo upload

pore upload behaves similarly to repo upload, but has some additional options
for new features that have been added to Gerrit:

```sh
pore upload --cbr --autosubmit --presubmit --re=jmgao .
```

The `--autosubmit` and `--presubmit` flags set the corresponding votes in Gerrit
when the patch is uploaded.

Note that user names passed to `--re` or `--cc` without an explicit domain are
assumed to be @google.com.

Multiproject upload and the interactive (non-`--cbr`) workflows are currently
not supported.

## repo sync

To sync with upstream:

```sh
pore sync
```

## repo rebase

To rebase local changes onto upstream:

```sh
pore rebase
```

## repo prune

To prune local branches that have been submitted:


```
pore prune
```
