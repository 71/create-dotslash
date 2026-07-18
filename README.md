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
  ◇ Downloading artifacts...
  ◆ Binary in https://files.radicle.dev/releases/latest/radicle-1.9.1-aarch64-unknown-linux-musl.tar.xz:
  │  ○ git-remote-rad
  │  ● rad
  │  ○ radicle-node
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
> Windows is supported, but not tested or built in the CI. Contributions would
> be welcome.
