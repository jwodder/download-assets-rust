This is a Rust program for downloading all the assets for one or more releases
of a given GitHub repository in parallel.  It was written as an exercise in
beginning asynchronous Rust and is a translation (with some behavior changes)
of [this Python script][pygist].

[pygist]: https://gist.github.com/jwodder/c0ad1a5a0b6fda18c15dbdb405e1e549

Usage
=====

    download-assets [<options>] <repo> [<release> ...]

Download all the assets for each of the given releases (identified by tag name)
of the given GitHub repository.

The repository can be specified as either `OWNER/NAME` or
`https://github.com/OWNER/NAME`.

At least one release or the `--all` option must be specified.

Options
-------

- `-A`, `--all` — Download assets for all releases of the repository

- `-d PATH`, `--download-dir PATH` — Specify the directory in which to save the
  assets.  The directory need not already exist.  The assets for each release
  will be placed in a subdirectory named after the release's tag.  [default:
  current directory]
