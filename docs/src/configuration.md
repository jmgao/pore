# Configuration

pore uses a global configuration file located at `~/.pore.toml`. If not present,
a default configuration with an AOSP remote with a mirror located at
`~/.pore/android` will be used.

The following configuration file shows all of the available options.

```toml
# If true, enables autosubmit by default when uploading changes.
autosubmit = false

# If true, mark the change as ready for presubmit by default when uploading
# changes.
presubmit = false

# A repo remote. Effectively the git repository containing the manifest file for
# the repo projects. This block can be repeated for as many remotes as
# necessary.
[[remotes]]

# Name of the remote. This is what will be used at $REMOTE in
# `pore clone $REMOTE/$BRANCH`.
name = 'aosp'

# Base URL of the remote.
url = 'https://android.googlesource.com/'

# Path to the manifest project.
manifest = 'platform/manifest'

# Selects which depot to use for objects fetched from this remote. Remotes using
# the same depot will share a mirror to reduce duplication of git objects.
depot = 'android'

# Defines a depot to be used by remotes. A depot is similar to a repo mirror.
[depots.android]

# The path to the depot location on disk.
path = '/path/to/depots/android'
```
