use std::borrow::Cow;

use snafu::{OptionExt, ResultExt};

use crate::{
    Client, format, github,
    target::{self, Artifacts},
};

pub async fn to_artifacts(
    client: &Client,
    target::DotslashFile { path, update }: &target::DotslashFile,
    interactive: bool,
) -> Result<Artifacts, snafu::WhateverLocal> {
    let mut file = std::fs::read_to_string(path)
        .with_whatever_context(|_| format!("cannot read {}", path.display()))?;

    // Remove shebang.
    if file.starts_with("#!")
        && let Some(line_end) = file.find('\n')
    {
        file.replace_range(0..line_end, ""); // Keep the `\n` to simplify the next loop.
    }

    strip_jsonc(&mut file);

    // Parse as JSON.
    let file: format::DotslashFile = serde_json::from_str(&file)
        .with_whatever_context(|_| format!("cannot parse {} as Dotslash file", path.display()))?;

    let update_to = if let Some(to) = update
        && !to.is_empty()
    {
        Some(Cow::Borrowed(to.as_str()))
    } else if update.is_some() {
        // Find the latest version if we have a GitHub release.
        let gh_repo = file
            .platforms
            .values()
            .flat_map(|platform| &platform.providers)
            .find_map(|provider| {
                if let format::DotslashProvider::GitHub {
                    repo,
                    tag: _,
                    name: _,
                } = provider
                {
                    Some(repo.as_str())
                } else {
                    None
                }
            });

        let mut releases = if let Some(repo) = gh_repo
            && let Some((org, repo)) = repo.split_once('/')
        {
            match github::latest_releases(client, "github.com", org, repo).await {
                Ok(releases) => releases,
                Err(err) => {
                    eprintln!("cannot fetch latest releases of github.com/{org}/{repo}: {err}");

                    vec![]
                }
            }
        } else {
            vec![]
        };

        // Prompt.
        Some(if interactive {
            Cow::Owned(
                dialoguer::Input::new()
                    .interact_text()
                    .whatever_context("cannot prompt release")?,
            )
        } else if !releases.is_empty() {
            Cow::Owned(releases.swap_remove(0).tag_name)
        } else {
            snafu::whatever!("no release found in {}", path.display());
        })
    } else {
        None
    };

    // Convert to artifacts.
    let mut artifacts = Artifacts::default();

    for (platform, entry) in file.platforms {
        let platform = target::Platform::from_dotslash(platform)
            .with_whatever_context(|| format!("unknown platform {platform:?}"))?;

        let format = entry
            .format
            .map(|f| {
                target::ArchiveFormat::from_dotslash(f)
                    .with_whatever_context(|| format!("unknown format {f:?}"))
            })
            .transpose()?;

        let mut url = None;
        let mut version = None;

        for provider in entry.providers {
            match provider {
                format::DotslashProvider::GitHub { repo, tag, name } => {
                    if url.is_none() {
                        url = Some(format!(
                            "https://github.com/{repo}/releases/download/{tag}/{name}"
                        ));
                    }
                    if version.is_none() {
                        version = Some(tag);
                    }
                }
                format::DotslashProvider::Url { url: provider_url } => {
                    if version.is_none()
                        && let Some(v) = guess_version(&provider_url)
                    {
                        version = Some(v.to_owned());
                    }
                    if url.is_none() {
                        url = Some(provider_url);
                    }
                }
            }
        }

        let url = url.with_whatever_context(|| format!("no provider for platform {platform:?}"))?;
        let version = version.with_whatever_context(|| {
            format!("cannot determine version of artifact in platform {platform:?}")
        })?;

        let path = entry.path;

        let artifact = if let Some(update_to) = update_to.as_deref() {
            target::Artifact {
                name: file.name.clone(),
                platform,
                format,
                url: replace_version(url, &version, update_to),
                path: Some(replace_version(path, &version, update_to)),
                digest: None,
                size: None,
            }
        } else {
            let digest = match entry.hash {
                "blake3" => blake3::Hash::from_hex(entry.digest.as_bytes())
                    .ok()
                    .map(target::Digest::Blake3),
                "sha256" => target::Digest::parse_digest_bytes(entry.digest.as_bytes())
                    .map(target::Digest::Sha256),
                _ => None,
            };

            target::Artifact {
                name: file.name.clone(),
                platform,
                url,
                format,
                path: Some(path),
                digest,
                size: Some(entry.size),
            }
        };

        artifacts.set(artifact);
    }

    Ok(artifacts)
}

// -------------------------------------------------------------------------------------------------
// MARK: Versions

/// Guesses the version in `s`, returning [`None`] if no version or more than one distinct version
/// is found.
fn guess_version(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut guess = None;
    let mut i = 0;

    while i < bytes.len() {
        let at_boundary = i == 0
            || can_start_version(bytes[i - 1])
            || (bytes[i - 1] == b'v' && (i == 1 || can_start_version(bytes[i - 2])));

        if !at_boundary || !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }

        let Some(len) = version_len(&bytes[i..]) else {
            i += 1;
            continue;
        };
        let candidate = &s[i..i + len];

        match guess {
            None => guess = Some(candidate),
            Some(prev) if prev == candidate => {}
            Some(_) => return None, // Ambiguous.
        }

        i += len;
    }

    guess
}

/// Returns the length of the version at the start of `s`, if any.
fn version_len(s: &[u8]) -> Option<usize> {
    let digits = |from: usize| s[from..].iter().take_while(|b| b.is_ascii_digit()).count();

    let mut end = digits(0);
    let mut components = 1;

    while s.get(end) == Some(&b'.') && s.get(end + 1).is_some_and(u8::is_ascii_digit) {
        end += 1 + digits(end + 1);
        components += 1;
    }

    if components < 2 {
        return None;
    }

    const PRERELEASE_TAGS: &[&[u8]] = &[
        b"dev", b"alpha", b"beta", b"rc", b"pre", b"nightly", b"canary", b"preview",
    ];

    // Optional pre-release suffix like `-dev` or `-rc1`.
    if s.get(end) == Some(&b'-') {
        let word_len = s[end + 1..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        let word = &s[end + 1..end + 1 + word_len];

        if PRERELEASE_TAGS
            .iter()
            .any(|tag| word.starts_with(tag) && word[tag.len()..].iter().all(u8::is_ascii_digit))
        {
            end += 1 + word_len;
        }
    }

    (end == s.len() || ends_version(s[end])).then_some(end)
}

#[test]
fn test_guess_version() {
    assert_eq!(
        guess_version(
            "https://github.com/facebook/dotslash/releases/download/v0.5.9/dotslash-linux-musl.arm64.v0.5.9.tar.gz",
        ),
        Some("0.5.9"),
    );

    assert_eq!(
        guess_version(
            "https://ziglang.org/builds/zig-aarch64-linux-0.17.0-dev.1422+e863bf3be.tar.xz",
        ),
        Some("0.17.0-dev"),
    );

    assert_eq!(
        guess_version(
            "https://github.com/helix-editor/helix/releases/download/25.07.1/helix-25.07.1-aarch64-linux.tar.xz",
        ),
        Some("25.07.1"),
    );

    assert_eq!(
        guess_version(
            "https://github.com/facebook/dotslash/releases/download/v0.5.9/dotslash-linux-musl.arm64.v0.5.8.tar.gz",
        ),
        None, // Two versions detected.
    );

    assert_eq!(
        guess_version(
            "https://github.com/facebook/dotslash/releases/download/nope/dotslash-linux-musl.arm64.nope.tar.gz",
        ),
        None, // No version detected.
    );

    assert_eq!(
        guess_version(
            "https://github.com/BurntSushi/ripgrep/releases/download/14.1.1/ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
        ),
        Some("14.1.1"), // `_64` is not a version.
    );

    assert_eq!(
        guess_version("https://example.com/api/v1/tool-2.0.0-rc1-linux.zip"),
        Some("2.0.0-rc1"), // `v1` has a single component; `-linux` is not a pre-release tag.
    );

    assert_eq!(
        guess_version("https://example.com/tool-1.2.3x.tar.gz"),
        None, // Version must end at a non-alphanumeric character.
    );

    assert_eq!(
        guess_version("https://example.com/releases/v15/tool.tar.gz"),
        None, // Single-component versions are not detected.
    );

    assert_eq!(guess_version("1.2"), Some("1.2"));
}

fn replace_version(mut s: String, old_version: &str, new_version: &str) -> String {
    let mut offset = 1;

    while let Some(index) = s[offset..].find(old_version) {
        let prev_ch = s.as_bytes()[offset + index - 1];

        let correct_start = can_start_version(prev_ch)
            || (index > 0
                && prev_ch == b'v'
                && can_start_version(s.as_bytes()[offset + index - 2]));
        let correct_end = offset + index + old_version.len() == s.len()
            || ends_version(s.as_bytes()[offset + index + old_version.len()]);

        if correct_start && correct_end {
            s.replace_range(
                offset + index..offset + index + old_version.len(),
                new_version,
            );
            offset += index + new_version.len();
        } else {
            offset += index;
        }
    }

    s
}

const fn can_start_version(ch: u8) -> bool {
    matches!(ch, b'.' | b'-' | b'_' | b'/')
}

const fn ends_version(ch: u8) -> bool {
    !ch.is_ascii_alphanumeric()
}

// -------------------------------------------------------------------------------------------------
// MARK: jsonc

/// Removes comments and trailing commas, turning a JSONC string into JSON.
fn strip_jsonc(file: &mut String) {
    let mut offset = 0;
    let mut in_string = false;

    while let Some(index) = file[offset..].find(['\\', '/', '"', ']', '}']) {
        offset += index;

        match (file.as_bytes()[offset], file.as_bytes().get(offset + 1)) {
            (b'"', _) => {
                in_string = !in_string;
                offset += 1;
            }
            (b'\\', _) => {
                let escaped = file[offset + 1..].chars().next();
                offset += 1 + escaped.map_or(0, char::len_utf8);
            }
            (b'/', Some(b'/')) if !in_string => {
                let len = file[offset..].find('\n').unwrap_or(file.len() - offset);
                file.replace_range(offset..offset + len, "");
            }
            (b'/', Some(b'*')) if !in_string => {
                let len = file[offset + 2..]
                    .find("*/")
                    .map_or(file.len() - offset, |end| end + 4);
                file.replace_range(offset..offset + len, "");
            }
            (b']' | b'}', _) if !in_string => {
                let before = file[..offset].trim_end();
                if before.ends_with(',') {
                    file.remove(before.len() - 1);
                    offset -= 1;
                }
                offset += 1;
            }
            _ => offset += 1,
        }
    }
}
