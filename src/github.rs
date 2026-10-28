use futures_util::StreamExt;
use snafu::ResultExt;

use crate::{
    Client, read_all, send,
    target::{self, Artifact, ArtifactName, Artifacts, Digest, Platform},
};

/// Converts the [`Target::GitHubRelease`] in `targets` into [`Artifacts`]. Returns [`None`] if
/// there is no `GitHubRelease` target, and [`Err`] if there is more than one.
pub async fn to_artifacts(
    client: &Client,
    target::GitHubRelease {
        host,
        org,
        repo,
        release,
        platforms,
        platform_count,
        prefer_gnu,
        options: target::CommonOptions { path },
    }: target::GitHubRelease,
) -> Result<Artifacts, snafu::WhateverLocal> {
    let platforms = &platforms[..platform_count];

    // Fetch the release from the GitHub API.
    let api_url = {
        let release_tag = std::fmt::from_fn(|f| {
            if let Some(release) = &release {
                f.write_str("tags/")?;
                f.write_str(release)
            } else {
                f.write_str("latest")
            }
        });

        format!("https://api.{host}/repos/{org}/{repo}/releases/{release_tag}")
    };

    let mut request = with_gh_token(Default::default())?;

    request.headers_mut().append(
        hyper::header::ACCEPT,
        hyper::header::HeaderValue::from_static("application/vnd.github+json"),
    );

    let response = send(client, &api_url, request).await?;

    let response_bytes = read_all(response)
        .await
        .with_whatever_context(|_| format!("failed to read release at {api_url}"))?;

    let mut response_json: GitHubReleaseResponse = serde_json::from_slice(&response_bytes)
        .with_whatever_context(|_| format!("failed to parse release at {api_url}"))?;

    let tag = release.as_deref().unwrap_or(&response_json.tag_name);

    snafu::ensure_whatever!(
        !response_json.assets.is_empty(),
        "no asset found in {org}/{repo}@{tag}",
    );

    // Keep all the assets that we can parse and that were requested by the user.
    let mut assets_by_platform = Platform::ALL.map(|platform| (platform, None));

    for (i, asset) in response_json.assets.iter_mut().enumerate() {
        if asset.name.ends_with(".sha256") {
            continue;
        }
        let Ok(name) = asset.name.parse::<ArtifactName>() else {
            continue;
        };
        if !platforms.is_empty() && !platforms.contains(&name.platform) {
            continue;
        }

        let (_, found) = assets_by_platform
            .iter_mut()
            .find(|(p, _)| *p == name.platform)
            .unwrap();

        let Some((found_name, found_asset, found_index)) = found else {
            *found = Some((name, std::mem::take(asset), i));
            continue;
        };

        // Multiple assets match the same platform; keep the asset that matches the `libc` we want.
        if (name.libc == Some(target::Libc::Gnu) && prefer_gnu)
            || (name.libc == Some(target::Libc::Musl) && !prefer_gnu)
        {
            *found_name = name;
            *found_asset = std::mem::take(asset);
            *found_index = i;
        }
    }

    if assets_by_platform.iter().filter(|x| x.1.is_some()).count() == 0 {
        let unknown_assets = response_json
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        snafu::whatever!(
            "no asset found{} in {org}/{repo}@{tag}; available {}: {}",
            match platform_count {
                0 => "",
                1 => " for requested platform",
                _ => " for requested platforms",
            },
            match unknown_assets.len() {
                1 => "asset",
                _ => "assets",
            },
            unknown_assets.join(", "),
        );
    }

    // For each asset we have, determine its digest (if possible).
    let mut artifacts = Artifacts::default();
    let mut futures = futures_util::stream::FuturesUnordered::new();

    for (_, asset) in assets_by_platform {
        let Some((name, asset, asset_index)) = asset else {
            continue;
        };
        let path = path.clone();

        if let Some(digest) = &asset.digest {
            match Digest::parse(digest, "") {
                Ok(digest) => {
                    artifacts.set(gh_artifact(name, asset, Some(digest), path));
                    continue;
                }
                Err(err) => eprintln!("failed to parse GitHub-provided digest: {err}"),
            }
        }

        let next_asset = response_json.assets.get(asset_index + 1);

        if let Some(next_asset) = next_asset
            && let Some(prefix) = next_asset.name.strip_suffix(".sha256")
            && prefix == asset.name
        {
            futures.push(async move {
                let digest = match Digest::fetch(client, &next_asset.browser_download_url).await {
                    Ok(digest) => Some(digest),
                    Err(err) => {
                        eprintln!("failed to fetch {}: {err}", next_asset.browser_download_url);
                        None
                    }
                };

                Ok(gh_artifact(name, asset, digest, path))
            });
            continue;
        }

        artifacts.set(gh_artifact(name, asset, None, path));
    }

    // Add all assets to `artifacts`.
    while let Some(result) = futures.next().await {
        artifacts.set(result?);
    }

    Ok(artifacts)
}

pub struct Release {
    pub tag_name: String,
}

pub async fn latest_releases(
    client: &Client,
    host: &str,
    org: &str,
    repo: &str,
) -> Result<Vec<Release>, snafu::WhateverLocal> {
    let api_url = format!("https://api.{host}/repos/{org}/{repo}/releases");
    let mut request = with_gh_token(Default::default())?;

    request.headers_mut().append(
        hyper::header::ACCEPT,
        hyper::header::HeaderValue::from_static("application/vnd.github+json"),
    );

    let response = send(client, &api_url, request).await?;

    let response_bytes = read_all(response)
        .await
        .with_whatever_context(|_| format!("failed to read releases at {api_url}"))?;

    let response_json: Vec<GitHubReleaseResponse> = serde_json::from_slice(&response_bytes)
        .with_whatever_context(|_| format!("failed to parse releases at {api_url}"))?;

    Ok(response_json
        .into_iter()
        .map(|release| Release {
            tag_name: release.tag_name,
        })
        .collect())
}

fn gh_artifact(
    ArtifactName {
        name,
        format,
        platform,
        libc: _,
    }: ArtifactName,
    asset: GitHubReleaseAsset,
    digest: Option<Digest>,
    path: Option<String>,
) -> Artifact {
    Artifact {
        url: asset.browser_download_url,
        name,
        platform,
        format,
        path,
        digest,
        size: Some(asset.size),
    }
}

pub fn with_gh_token<T>(
    mut request: hyper::Request<T>,
) -> Result<hyper::Request<T>, snafu::WhateverLocal> {
    // We intentionally do not add additional context (like the invalid value) below.
    match std::env::var("GITHUB_TOKEN") {
        Ok(token) => {
            // https://docs.github.com/en/rest/authentication/authenticating-to-the-rest-api?apiVersion=2026-03-10#authenticating-in-a-github-actions-workflow-using-curl
            request.headers_mut().append(
                hyper::header::AUTHORIZATION,
                hyper::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .whatever_context("GITHUB_TOKEN is invalid")?,
            );
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => snafu::whatever!("GITHUB_TOKEN is invalid"),
    }

    Ok(request)
}

#[derive(serde::Deserialize)]
struct GitHubReleaseResponse {
    assets: Vec<GitHubReleaseAsset>,
    tag_name: String,
}

#[derive(Clone, serde::Deserialize, Default)]
struct GitHubReleaseAsset {
    browser_download_url: String,
    digest: Option<String>,
    name: String,
    size: u64,
}
