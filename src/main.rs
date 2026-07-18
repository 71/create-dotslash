use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
};

use clap::Parser;
use futures_util::{StreamExt, stream::FuturesUnordered};
use snafu::{OptionExt, ResultExt};

mod archive;
mod fetch;
mod format;
mod github;
mod target;

use crate::{
    format::Format,
    target::{Artifact, ArtifactName, Artifacts, Digest, Http, Target},
};

// -------------------------------------------------------------------------------------------------
// MARK: Args

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to write the file to.
    #[arg(short, long, value_name = "FILE", value_hint = clap::ValueHint::AnyPath)]
    output: Option<PathBuf>,

    /// Overwrite --output file if it already exists.
    #[arg(short, long)]
    force: bool,

    /// The format of the output file.
    #[arg(long, default_value = "dotslash")]
    format: Format,

    /// The name to give to the program.
    #[arg(long, value_name = "NAME", value_hint = clap::ValueHint::Other)]
    name: Option<String>,

    /// Fail instead of prompting when some information is missing. Implicitly true if no terminal
    /// is connected.
    #[arg(long)]
    no_interactive: bool,

    /// Do not display progress. Implicitly true if no terminal is connected.
    #[arg(long)]
    no_progress: bool,

    /// The targets to download and parse for inclusion in the Dotslash file.
    ///
    /// The following formats are supported:
    ///
    /// - `https://some.url/path/to/file`: download the file at the given URL, using its extension
    ///   to determine its type.
    ///
    /// - `https://github.com/org/repo/releases/release`: download the given `release` from
    ///   `org/repo`. Also available as `github:org/repo/release` (or
    ///   `github:example.com/org/repo/release` for GHE).
    ///
    /// - `https://github.com/org/repo`: download the latest stable `release` from `org/repo`. Also
    ///   available as `github:org/repo` (or `github:example.com/org/repo` for GHE).
    ///
    /// Options can be specified after each URI using a hash `#`, and separated by a comma:
    ///
    /// - `path=<path>`: the path of the binary in its archive.
    ///
    /// HTTP releases additional support the following options:
    ///
    /// - `digest=<digest>`: the digest of the archive or binary; specifying both a `path` and a
    ///   `digest` allows the download of the archive to be skipped.
    ///
    /// - `sha256=<hash>`: same as `digest`, but with an implicit `sha256:` prefix.
    ///
    /// - `blake3=<hash>`: same as `digest`, but with an implicit `blake3:` prefix.
    ///
    /// GitHub releases additionally support the following options:
    ///
    /// - `platform=<platform>`: only download artifacts for the given platforms (architecture, OS,
    ///   or both separated by a hyphen `-`). Can be repeated.
    ///
    /// - `gnu`: prefer `gnu` over `musl` when both are available.
    ///
    /// - `musl`: prefer `musl` over `gnu` when both are available (default).
    ///
    /// As a special case, a file may also end with `.sha256`. The same file without the `.sha256`
    /// prefix will be downloaded instead and checked against the fetched `.sha256`.
    #[arg(required(true), value_name = "TARGETS")]
    targets: Vec<Target>,
}

// -------------------------------------------------------------------------------------------------
// MARK: main()

fn main() -> snafu::Report<snafu::WhateverLocal> {
    let args = Args::parse();

    snafu::Report::capture(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(tokio::runtime::LocalOptions::default())
            .whatever_context("could not create Tokio runtime")?
            .block_on(run(args))
    })
}

async fn run(args: Args) -> Result<(), snafu::WhateverLocal> {
    let mut name = args.name;
    let mut out_is_dir = false;

    if let Some(out) = &args.output {
        if out.is_dir() {
            // Allow `out` to point to a directory, in which case we write the program to
            // `out/name`.
            out_is_dir = true;
        } else {
            // Check early that we can write to `out`, before accessing the internet. But do not
            // create it yet.
            snafu::ensure_whatever!(
                args.force || !out.exists(),
                "cannot overwrite {}; pass --force to overwrite",
                out.display(),
            );

            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .with_whatever_context(|_| format!("failed to create {}", parent.display()))?;
            }

            if name.is_none()
                && let Some(stem) = out.file_stem()
                && let Some(stem) = stem.to_str()
            {
                name = Some(stem.to_owned());
            }
        }
    }

    let interactive = !args.no_interactive && std::io::stdin().is_terminal();
    let progress = interactive && !args.no_progress;

    let client = &reqwest::Client::builder()
        .user_agent(concat!(
            "https://github.com/71/",
            env!("CARGO_PKG_NAME"),
            "@",
            env!("CARGO_PKG_VERSION"),
        ))
        .build()
        .whatever_context("failed to create HTTP client")?;

    let platforms = expand_targets(client, args.targets).await?;
    let output = args
        .format
        .format(client, &mut name, &platforms, interactive, progress)
        .await?;

    if let Some(mut out) = args.output {
        if out_is_dir {
            out.push(name.unwrap());
        }

        let overwrite = args.force
            || (interactive
                && out.exists()
                && cliclack::Confirm::new(format!("Overwrite {}?", out.display()))
                    .interact()
                    .unwrap_or(false));

        let mut file_options = std::fs::OpenOptions::new();

        file_options
            .create(true)
            .create_new(!overwrite)
            .truncate(true)
            .write(true);

        #[cfg(not(target_os = "windows"))]
        {
            std::os::unix::fs::OpenOptionsExt::mode(&mut file_options, 0o777);
        }

        let mut file = file_options
            .open(&out)
            .with_whatever_context(|_| format!("failed to open {}", out.display()))?;

        file.write_all(&output)
            .with_whatever_context(|_| format!("failed to write to {}", out.display()))?;
    } else {
        std::io::stdout()
            .write_all(&output)
            .whatever_context("failed to write to stdout")?;
    }

    Ok(())
}

// -------------------------------------------------------------------------------------------------
// MARK: Helpers

async fn expand_targets(
    client: &reqwest::Client,
    mut targets: Vec<Target>,
) -> Result<Artifacts, snafu::WhateverLocal> {
    assert!(!targets.is_empty());

    if let Some(artifacts) = github::to_artifacts(client, &mut targets).await? {
        return Ok(artifacts);
    }

    let mut artifacts = Artifacts::default();
    let mut futures = FuturesUnordered::new();

    for target in targets {
        let Target::Http(http) = target else {
            unreachable!("ensured by `github::expand_release()`")
        };

        futures.push(expand_http(client, http));
    }

    while let Some(result) = futures.next().await {
        artifacts.set(result?);
    }

    Ok(artifacts)
}

async fn expand_http(
    client: &reqwest::Client,
    Http {
        mut url,
        mut digest,
        size,
        options: target::CommonOptions { path },
    }: Http,
) -> Result<Artifact, snafu::WhateverLocal> {
    if let Some(without_digest) = url.strip_suffix(".sha256") {
        // We were given a URL to some digest, fetch it and use it.
        digest = Some(Digest::parse_or_fetch(client, &url).await?);
        url.truncate(without_digest.len());
    }

    let file_name_start = url
        .rfind('/')
        .with_whatever_context(|| format!("invalid url: {}", url))?;

    let file_name = &url[file_name_start + 1..];
    let ArtifactName {
        name,
        format,
        platform,
        libc: _,
    } = file_name
        .parse()
        .with_whatever_context(|_| format!("invalid file name: {}", file_name))?;

    Ok(Artifact {
        url,
        name,
        platform,
        format,
        path,
        digest,
        size,
    })
}
