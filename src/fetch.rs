use std::cell::RefCell;

use snafu::ResultExt;

use crate::{
    archive::{self, Archive},
    target::{ArchiveFormat, Digest},
};

/// Information determined by fetching an artifact.
pub struct FetchedArtifact {
    /// The size of the artifact in bytes.
    pub size: u64,
    /// The hash of the artifact _before_ decompression (if relevant).
    pub hash: Digest,
    /// The path of the binary within its archive (if relevant).
    pub path: String,
}

/// Fetches the artifact at the given URL, determining necessary information to extract and use it.
///
/// If interactive input is enabled, `picked_path_if_interactive` should point to a cell which
/// contains the result of prompting the user for a path (when ambiguous), so that we may reuse that
/// path instead of repeatedly prompting.
#[expect(clippy::too_many_arguments)]
pub async fn fetch(
    client: &reqwest::Client,
    url: reqwest::Url,
    name: String,
    path: Option<String>,
    file_format: Option<ArchiveFormat>,
    prefer_sha256: bool,
    progress: Option<&cliclack::MultiProgress>,
    picked_path_if_interactive: Option<&RefCell<Option<String>>>,
) -> Result<FetchedArtifact, snafu::WhateverLocal> {
    // Send request.
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .with_whatever_context(|_| format!("failed to GET {url}"))?
        .error_for_status()
        .with_whatever_context(|_| format!("failed to GET {url}"))?;

    // Read response, computing the hash, size, and optionally collecting archive entries.
    let mut archive = if path.is_none()
        && let Some(file_format) = file_format
        && file_format.has_entries()
    {
        Some(Archive::new(file_format))
    } else {
        None
    };

    let mut hasher = if prefer_sha256 {
        Hasher::Sha256(hmac_sha256::Hash::new())
    } else {
        Hasher::Blake3(blake3::Hasher::new())
    };
    let mut size = 0;

    let progress_bar = progress.map(|p| {
        p.add({
            let bar = if let Some(len) = response.content_length() {
                cliclack::ProgressBar::new(len).with_download_template()
            } else {
                cliclack::ProgressBar::new(1).with_spinner_template()
            };

            let name = if let Some(mut segments) = url.path_segments()
                && let Some(segment) = segments.next_back()
                && !segment.is_empty()
            {
                segment
            } else {
                url.path()
            };

            bar.start(name);
            bar
        })
    });

    while let Some(chunk) = response
        .chunk()
        .await
        .with_whatever_context(|_| format!("failed to read response from {url}"))?
    {
        if let Some(progress_bar) = &progress_bar {
            if progress_bar.length() == Some(size) {
                progress_bar.set_length(size + chunk.len() as u64);
            }
            progress_bar.set_position(size + chunk.len() as u64);
        }

        size += chunk.len() as u64;

        match &mut hasher {
            Hasher::Blake3(blake3) => {
                _ = blake3.update(&chunk);
            }
            Hasher::Sha256(sha256) => sha256.update(&chunk),
        }

        if let Some(archive) = &mut archive {
            archive.update(chunk, &url).await?;
        }
    }

    let path = if let Some(archive) = archive {
        pick_path(
            &url,
            archive.finish(&url).await?,
            picked_path_if_interactive,
        )?
    } else {
        path.unwrap_or(name)
    };

    if let Some(progress_bar) = progress_bar {
        progress_bar.clear();
    }

    Ok(FetchedArtifact {
        size,
        hash: match hasher {
            Hasher::Blake3(blake3) => Digest::Blake3(blake3.finalize()),
            Hasher::Sha256(sha256) => Digest::Sha256(sha256.finalize()),
        },
        path,
    })
}

#[expect(clippy::large_enum_variant)]
enum Hasher {
    Blake3(blake3::Hasher),
    Sha256(hmac_sha256::Hash),
}

/// Returns the path in `entries` to use as the binary path, possibly prompting the user for it.
fn pick_path(
    url: &reqwest::Url,
    mut entries: Vec<archive::Entry>,
    picked_path_if_interactive: Option<&RefCell<Option<String>>>,
) -> Result<String, snafu::WhateverLocal> {
    snafu::ensure_whatever!(!entries.is_empty(), "cannot determine path of {url}");

    if entries.len() == 1 {
        return Ok(entries.pop().unwrap().path);
    }

    let Some(picked_path) = picked_path_if_interactive else {
        snafu::whatever!(
            "cannot determine path of {url} among {}",
            entries.join(", "),
        );
    };

    // Strip the longest common prefix before prompting.
    let first_path = entries[0].path.as_str();
    let mut common_prefix_len = 0;

    while let Some(pos) = first_path[common_prefix_len..].find('/') {
        let prefix = &first_path[common_prefix_len..common_prefix_len + pos + 1];

        if !entries[1..]
            .iter()
            .all(|entry| entry.path[common_prefix_len..].starts_with(prefix))
        {
            break;
        }

        common_prefix_len += pos + 1;
    }

    let mut display_paths = entries.iter().map(|entry| &entry.path[common_prefix_len..]);

    // Determine if the path we already picked is available. We do this after stripping the
    // common prefix in case it has some platform-specific components.
    let path_index = if let Some(path) = &*picked_path.borrow()
        && let Some(index) = display_paths.clone().position(|x| x == path)
    {
        // Reuse the path the user already picked.
        index
    } else {
        // Prompt the user for the right path.
        let path_index = display_paths
            .clone()
            .enumerate()
            .fold(
                cliclack::select(format!("Binary in {url}:")),
                |select, (i, path)| select.item(i, path, ""),
            )
            .interact()
            .with_whatever_context(|_| format!("failed to prompt binary for {url}"))?;

        *picked_path.borrow_mut() = Some(display_paths.nth(path_index).unwrap().to_owned());

        path_index
    };

    // Make sure to use the path in `entries`, rather than the one in `display_paths`.
    Ok(entries.swap_remove(path_index).path)
}
