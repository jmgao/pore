"""A launcher that will be run by `repo`.

There's a .repo/repo/repo tool that gets installed which runs pore in a repo-compatible
mode, but unless run directly (which few tools that run repo do, they usually expect the
thing in PATH to work), that won't be used. repo, when run from PATH rather the
installed .repo/repo/repo, will check for a .repo/repo/main.py and run that. For a real
repo tree, that's the full implementation of repo. We don't want that, we just want to
intercept that call and redirect to pore.
"""
import argparse
import subprocess
from pathlib import Path

THIS_DIR = Path(__file__).parent


def main() -> None:
    parser = argparse.ArgumentParser()
    # These arguments are passed to the real repo by the repo launched (the thing in
    # PATH). We don't care about them, but we need to ignore them.
    parser.add_argument("--repo-dir")
    parser.add_argument("--wrapper-version")
    parser.add_argument("--wrapper-path")
    _, other_args = parser.parse_known_args()

    # It also follows those arguments with -- before forwarding the user's arguments.
    # clap doesn't like that, nor does it need it, so just drop it.
    if other_args and other_args[0] == "--":
        other_args = other_args[1:]
    subprocess.run([str(THIS_DIR / "repo")] + other_args, check=True)


if __name__ == "__main__":
    main()
