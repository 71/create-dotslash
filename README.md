# create-dotslash

Create [`dotslash`](https://dotslash-cli.com/) files from URLs, automatically
computing sizes, hashes, and paths.

- Fetch the binaries in the latest version of https://github.com/71/ifctc and
  create a `ifctc` file:

  ```sh
  $ create-dotslash github:71/ifctc --output .
  $ ./ifctc --version
  ifctc 0.1.12
  ```

- Fetch the archives in `radicle`, pick the one called `rad`, then print the
  `dotslash` file to `stdout`:

  ```sh
  $ create-dotslash https://files.radicle.dev/releases/latest/radicle-1.9.1-{aarch64,x86_64}-{apple-darwin,unknown-linux-musl}.tar.xz
  Binary in https://files.radicle.dev/releases/latest/radicle-1.9.1-aarch64-apple-darwin.tar.xz:
  > git-remote-rad
    rad
    radicle-node
  ```

- Generate a `/bin/sh` script instead of relying on Dotslash; used to bootstrap
  `tools/dotslash` itself, so you can drop `dotslash` in your repo and not think
  about it:

  ```sh
  create-dotslash github:facebook/dotslash --format sh --output tools
  ```

  The script will use `curl`, `sha256sum` and `tar` for execution.

> [!NOTE]
>
> Windows is supported and built as part of the CI, but not tested.
> Contributions would be welcome.

## Usage

```sh
$ create-dotslash --help
Usage: create-dotslash [OPTIONS] <TARGETS>...

Arguments:
  <TARGETS>...
          The targets to download and parse for inclusion in the Dotslash file.
          
          The following formats are supported:
          
          - `https://some.url/path/to/file`: download the file at the given URL,
          using its extension to determine its type.
          
          - `https://github.com/org/repo/releases/release`: download the given
          `release` from `org/repo`. Also available as `github:org/repo/release`
          (or `github:example.com/org/repo/release` for GHE).
          
          - `https://github.com/org/repo`: download the latest stable `release`
          from `org/repo`. Also available as `github:org/repo` (or
          `github:example.com/org/repo` for GHE).
          
          Options can be specified after each URI using a hash `#`, and
          separated by a comma:
          
          - `path=<path>`: the path of the binary in its archive.
          
          HTTP releases additional support the following options:
          
          - `digest=<digest>`: the digest of the archive or binary; specifying
          both a `path` and a `digest` allows the download of the archive to be
          skipped.
          
          - `sha256=<hash>`: same as `digest`, but with an implicit `sha256:`
          prefix.
          
          - `blake3=<hash>`: same as `digest`, but with an implicit `blake3:`
          prefix.
          
          GitHub releases additionally support the following options:
          
          - `platform=<platform>`: only download artifacts for the given
          platforms (architecture, OS, or both separated by a hyphen `-`). Can
          be repeated.
          
          - `gnu`: prefer `gnu` over `musl` when both are available.
          
          - `musl`: prefer `musl` over `gnu` when both are available (default).
          
          As a special case, a file may also end with `.sha256`. The same file
          without the `.sha256` prefix will be downloaded instead and checked
          against the fetched `.sha256`.

Options:
  -o, --output <FILE>
          Path to write the file to

  -f, --force
          Overwrite --output file if it already exists

      --format <FORMAT>
          The format of the output file

          Possible values:
          - dotslash: Dotslash file
          - sh:       Self-contained shell script; requires `/bin/sh`, `curl`,
            `tee`, `mkdir`, `chmod`, `flock`, and additional programs for
            computing digests (`b3sum` or `sha256sum`) and for extraction (or
            `mv` if no extraction is needed)
          
          [default: dotslash]

      --name <NAME>
          The name to give to the program

      --no-interactive
          Fail instead of prompting when some information is missing; implicitly
          true if no terminal is connected

      --no-progress
          Do not display progress; implicitly true if no terminal is connected

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```
