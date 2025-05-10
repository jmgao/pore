# pore

pore is a reimplementation of Android's repository management tool,
[repo](https://gerrit.googlesource.com/git-repo/), with a focus on performance.
Tree-wide operations such as `status` and `sync` are up to [10 times
faster](https://asciinema.org/a/2kSTE803umfAQQR9SR7GP8rCc) in pore.
Additionally, pore always does the equivalent of repo's `--reference`
transparently, so a fresh checkout of a new tree takes on the order of one
minute, instead of tens of minutes.
