use std::{cell::RefCell, collections::BTreeMap, io::Write};

use futures_util::StreamExt;
use snafu::ResultExt;

use crate::{
    fetch::{self, fetch},
    target::{ArchiveFormat, Artifacts, Digest, Os, Platform},
};

#[derive(clap::ValueEnum, Clone, PartialEq, Eq)]
pub enum Format {
    /// Dotslash file.
    Dotslash,
    /// Self-contained shell script; requires `/bin/sh`, `curl`, `tee`, `mkdir`, `chmod`, `flock`,
    /// and additional programs for computing digests (`b3sum` or `sha256sum`) and for extraction
    /// (or `mv` if no extraction is needed).
    Sh,
}

impl Format {
    pub async fn format(
        self,
        client: &reqwest::Client,
        name: &mut Option<String>,
        platforms: &Artifacts,
        interactive: bool,
        progress: bool,
    ) -> Result<Vec<u8>, snafu::WhateverLocal> {
        let mut entries = create_dotslash_file(
            client,
            platforms,
            /* prefer_sha256= */ self == Format::Sh,
            interactive,
            progress,
        )
        .await?;

        assert!(!entries.is_empty());

        let name = name.get_or_insert_with(|| {
            let any_path = &entries.first().unwrap().dotslash_url_entry.path;
            let file_name_start = any_path.rfind('/').map_or(0, |i| i + 1);
            let file_name = &any_path[file_name_start..];

            file_name
                .strip_suffix(".exe")
                .unwrap_or(file_name)
                .to_owned()
        });

        let mut out = Vec::new();

        match self {
            Format::Dotslash => {
                let file = DotslashFile {
                    name: name.clone(),
                    platforms: entries
                        .into_iter()
                        .map(|entry| (entry.dotslash_platform, entry.dotslash_url_entry))
                        .collect(),
                };

                out.write_all(b"#!/usr/bin/env dotslash\n").unwrap();
                serde_json::to_writer_pretty(&mut out, &file).unwrap();
            }
            Format::Sh => {
                entries.retain(|entry| {
                    !matches!(
                        entry.platform,
                        Platform::Known {
                            os: Os::Windows,
                            ..
                        }
                    )
                });

                to_shell(&mut out, name, &entries).unwrap()
            }
        }

        Ok(out)
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Dotslash

async fn create_dotslash_file(
    client: &reqwest::Client,
    platforms: &Artifacts,
    prefer_sha256: bool,
    interactive: bool,
    progress: bool,
) -> Result<Vec<Entry>, snafu::WhateverLocal> {
    let picked_path = interactive.then(|| RefCell::new(None));
    let picked_path = picked_path.as_ref();

    let progress = progress.then(|| cliclack::MultiProgress::new("Downloading artifacts..."));
    let progress = progress.as_ref();

    let mut tasks = futures_util::stream::FuturesUnordered::new();
    let mut platform_count = 0;

    for (platform, artifact) in platforms.iter() {
        let url = artifact
            .url
            .parse()
            .with_whatever_context(|_| format!("invalid url: {}", artifact.url))?;

        tasks.push(async move {
            if let Some(hash) = artifact.digest
                && let Some(size) = artifact.size
                && (artifact.path.is_some() || artifact.format.is_none())
            {
                return (
                    platform,
                    artifact,
                    Ok(fetch::FetchedArtifact {
                        size,
                        hash,
                        path: artifact.path.as_ref().unwrap_or(&artifact.name).clone(),
                    }),
                );
            }

            (
                platform,
                artifact,
                fetch(
                    client,
                    url,
                    artifact.name.clone(),
                    artifact.path.clone(),
                    artifact.format,
                    prefer_sha256,
                    progress,
                    picked_path,
                )
                .await,
            )
        });
        platform_count += 1;
    }

    let mut entries = Vec::with_capacity(platform_count);

    while let Some((platform, artifact, fetch_result)) = tasks.next().await {
        let platform_name = platform.name();

        let fetched = fetch_result
            .with_whatever_context(|_| format!("failed to fetch entry for {platform_name}"))?;

        let providers = {
            let url_provider = DotslashProvider::Url {
                url: artifact.url.clone(),
            };

            if let Some(gh_provider) = DotslashProvider::parse_github_provider(&artifact.url) {
                vec![url_provider, gh_provider]
            } else {
                vec![url_provider]
            }
        };

        let (hash, digest) = match fetched.hash {
            Digest::Blake3(blake3) => ("blake3", blake3.to_string()),
            Digest::Sha256(sha256) => ("sha256", to_hex_string(&sha256)),
        };

        let entry = DotslashUrlEntry {
            size: fetched.size,
            hash,
            digest,
            format: artifact.format.map(|format| format.as_str()),
            path: fetched.path,
            providers,
        };

        entries.push(Entry {
            platform,
            format: artifact.format,
            dotslash_platform: platform_name,
            dotslash_url_entry: entry,
        });
    }

    if let Some(progress) = progress {
        progress.stop();
    }

    entries.sort_by_key(|entry| entry.dotslash_platform);

    Ok(entries)
}

#[derive(Debug)]
struct Entry {
    format: Option<ArchiveFormat>,
    platform: Platform,
    dotslash_platform: &'static str,
    dotslash_url_entry: DotslashUrlEntry,
}

// -------------------------------------------------------------------------------------------------
// MARK: DotslashFile

// https://dotslash-cli.com/docs/dotslash-file/
#[derive(serde::Serialize)]
struct DotslashFile {
    name: String,
    platforms: BTreeMap<&'static str, DotslashUrlEntry>,
}

#[derive(serde::Serialize, Debug, Hash)]
struct DotslashUrlEntry {
    size: u64,
    hash: &'static str,
    digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    path: String,
    providers: Vec<DotslashProvider>,
}

#[derive(serde::Serialize, Debug, Hash)]
#[serde(tag = "type")]
enum DotslashProvider {
    #[serde(rename = "github-release")]
    GitHub {
        repo: String,
        tag: String,
        name: String,
    },
    #[serde(untagged)]
    Url { url: String },
}

impl DotslashProvider {
    fn parse_github_provider(url: &str) -> Option<DotslashProvider> {
        let gh_path = url.strip_prefix("https://github.com/")?;
        let mut segments = gh_path.split('/');

        let org = segments.next()?;
        let repo = segments.next()?;

        if segments.next() != Some("releases") {
            return None;
        }
        if segments.next() != Some("download") {
            return None;
        }

        let tag = segments.next()?;
        let name = segments.next()?;

        if segments.next().is_some() {
            return None;
        }

        Some(DotslashProvider::GitHub {
            repo: format!("{org}/{repo}"),
            tag: tag.to_owned(),
            name: name.to_owned(),
        })
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Shell

fn to_shell(out: &mut Vec<u8>, name: &str, entries: &[Entry]) -> Result<(), snafu::WhateverLocal> {
    if let Some([a, b]) =
        two_different_values(entries.iter().map(|entry| entry.dotslash_url_entry.format))
    {
        snafu::whatever!(
            "--format=sh requires all entries to have the same format, but found {a:?} != {b:?}"
        );
    }

    if let Some([a, b]) =
        two_different_values(entries.iter().map(|entry| entry.dotslash_url_entry.hash))
    {
        snafu::whatever!(
            "--format=sh requires all entries to have the same hash format, but found {a} != {b}"
        );
    }

    let entries_hash = {
        let mut hasher = Blake3Hasher(blake3::Hasher::new());

        std::hash::Hash::hash(&entries.len(), &mut hasher);

        for entry in entries {
            std::hash::Hash::hash(&entry.dotslash_platform, &mut hasher);
            std::hash::Hash::hash(&entry.dotslash_url_entry, &mut hasher);
        }

        hasher.0.finalize()
    };

    // We use `flock` to lock access to the directory as we fetch the binary on it:
    // https://man7.org/linux/man-pages/man1/flock.1.html.
    write!(
        out,
        r#"#!/bin/sh
set -eu

cache_dir="${{XDG_CACHE_HOME:-${{HOME:-}}/.cache/create-dotslash}}"
test "$cache_dir" != "/.cache" || (echo '$XDG_CACHE_HOME or $HOME must be set' >&2 && exit 1)

arch="$(uname -m)"
case "$arch" in
	arm64) arch=aarch64 ;;
	*) ;;
esac

os="$(uname)"
case "$os" in
	Linux|Cygwin) os=linux ;;
	Darwin) os=macos ;;
	*) os="$(echo "$os" | tr '[:upper:]' '[:lower:]')";;
esac

case "$os-$arch" in
"#
    )
    .unwrap();

    let unique_path = unique_value(
        entries
            .iter()
            .map(|entry| entry.dotslash_url_entry.path.as_str()),
    );

    for Entry {
        dotslash_platform,
        dotslash_url_entry,
        platform: _,
        format: _,
    } in entries
    {
        let DotslashUrlEntry {
            size,
            digest,
            path,
            providers,
            hash: _,
            format: _,
        } = dotslash_url_entry;

        let url = providers
            .iter()
            .find_map(|x| match x {
                DotslashProvider::Url { url } => Some(url),
                _ => None,
            })
            .expect("we always generate at least one URL entry");

        writeln!(out, "\t{dotslash_platform})").unwrap();

        if unique_path.is_none() {
            writeln!(out, "\t\tpath={path:?}").unwrap();
        }

        write!(
            out,
            r#"		digest={digest}
		size={size}
		url={url:?}
		;;
"#
        )
        .unwrap();
    }

    let supported_platforms = entries
        .iter()
        .map(|entry| entry.dotslash_platform)
        .collect::<Vec<_>>()
        .join(", ");

    // `curl -fsSL`: `--fail --silent --show-error --location`
    write!(
        out,
        r#"	*)
		echo "unknown platform $platform; supported: {supported_platforms}" >&2
		exit 1
		;;
esac

dir="$cache_dir/{name_escaped}-{entries_hash}"
bin="$dir/{path}"

test -x "$bin" && exec "$bin" "$@"

mkdir -p "$dir"
(flock 9; if ! test -x "$bin"; then
	rm "$dir/.lock"
	tmp="$dir/.tmp"
	checksum="$(curl -fsSL "$url" | tee "$tmp" | {digest_program} -b)"
	checksum="${{checksum%% *}}"

	test "$checksum" != "$digest" \
		&& echo "invalid {digest_program} checksum; expected $digest, but got $checksum" >&2 \
		&& rm -rf "$dir" \
		&& exit 2

	{extract}
	chmod +x "$bin"
	rm -f "$tmp"
fi) 9>"$dir/.lock"

exec "$bin" "$@"
"#,
        digest_program = if entries[0].dotslash_url_entry.hash == "sha256" {
            "sha256sum"
        } else {
            "b3sum"
        },
        name_escaped = name.escape_debug(),
        path = unique_path.unwrap_or("$path"),
        extract = if let Some(format) = entries[0].format {
            match format {
                // https://man7.org/linux/man-pages/man1/tar.1.html: `-x` to extract, `-f` to read
                // from file, `-C` changes to directory first
                ArchiveFormat::Tar => r#"tar -xf "$tmp" -C "$dir""#,
                // `-z` to filter through `gzip`
                ArchiveFormat::TarGz => r#"tar -xzf "$tmp" -C "$dir""#,
                // `-J` to filter through `xz`
                ArchiveFormat::TarXz => r#"tar -xJf "$tmp" -C "$dir""#,
                // `--zstd` to filter through `zstd`
                ArchiveFormat::TarZst => r#"tar --zstd -xf "$tmp" -C "$dir""#,
                // `-j` to filter through `bzip2`
                ArchiveFormat::TarBz2 => r#"tar -xjf "$tmp" -C "$dir""#,

                // https://linux.die.net/man/1/unzip: `-qq` quiet, `-d` to extract to directory
                ArchiveFormat::Zip => r#"unzip -qq "$tmp" -d "$dir""#,
                // https://linux.die.net/man/1/gzip: `-d` to decompress, `-c` to write to stdout
                ArchiveFormat::Gz => {
                    r#"mkdir -p "$(dirname "$path")" && gzip -dc "$tmp" > "$path""#
                }
                // https://linux.die.net/man/1/xz: same
                ArchiveFormat::Xz => r#"mkdir -p "$(dirname "$path")" && xz -dc "$tmp" > "$path""#,
                // https://manpages.debian.org/testing/zstd/zstd.1.en.html: same, `-o` to write to
                // file
                ArchiveFormat::Zst => {
                    r#"mkdir -p "$(dirname "$path")" && zstd -d "$tmp" -o "$path""#
                }
                // https://linux.die.net/man/1/bzip2: same
                ArchiveFormat::Bz2 => {
                    r#"mkdir -p "$(dirname "$path")" && bzip2 -dc "$tmp" > "$path""#
                }
            }
        } else {
            r#"mkdir -p "$(dirname "$path")" && mv "$tmp" "$path""#
        },
    )
    .unwrap();

    Ok(())
}

/// [`std::hash::Hasher`] using [`blake3`].
struct Blake3Hasher(blake3::Hasher);

impl std::hash::Hasher for Blake3Hasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        unreachable!()
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Helpers

/// If all values in `iter` are all equal to a unique value, yields it. Otherwise, yields [`None`].
fn unique_value<T: Eq>(iter: impl IntoIterator<Item = T>) -> Option<T> {
    let mut iter = iter.into_iter();
    let a = iter.next()?;

    for next in iter {
        if next != a {
            return None;
        }
    }

    Some(a)
}

/// Yields `iter.next()`, and the first value after that which is different from it, or [`None`] if
/// no such value can be found.
fn two_different_values<T: Eq>(iter: impl IntoIterator<Item = T>) -> Option<[T; 2]> {
    let mut iter = iter.into_iter();
    let a = iter.next()?;

    for next in iter {
        if next != a {
            return Some([a, next]);
        }
    }

    None
}

fn to_hex_string(sha256: &[u8; 32]) -> String {
    let (chunks, rest) = sha256.as_chunks::<8>();
    let chunks: [[u8; 8]; 4] = chunks.try_into().unwrap();
    let chunks = chunks.map(u64::from_be_bytes);

    debug_assert!(rest.is_empty());

    format!(
        "{:016x}{:016x}{:016x}{:016x}",
        chunks[0], chunks[1], chunks[2], chunks[3],
    )
}
