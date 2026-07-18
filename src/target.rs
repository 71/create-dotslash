// -------------------------------------------------------------------------------------------------
// MARK: Target

use std::str::FromStr;

use snafu::{OptionExt, ResultExt};

use crate::github::with_gh_token;

/// A file to download specified by the user.
#[derive(Clone)]
pub enum Target {
    GitHubRelease(GitHubRelease),
    Http(Http),
}

/// [`Target`]: download a release from GitHub.
#[derive(Clone)]
pub struct GitHubRelease {
    pub host: String,
    pub org: String,
    pub repo: String,
    pub release: Option<String>,
    pub platforms: [Platform; Platform::ALL.len()],
    pub platform_count: usize,
    pub prefer_gnu: bool,
    pub options: CommonOptions,
}

/// [`Http`]: download a file from an URL.
#[derive(Clone)]
pub struct Http {
    pub url: String,
    pub digest: Option<Digest>,
    pub size: Option<u64>,
    pub options: CommonOptions,
}

// -------------------------------------------------------------------------------------------------
// MARK: Artifact

/// Resolved information about an artifact to fetch. Some information
#[derive(Clone, Debug)]
pub struct Artifact {
    /// URL to use to fetch the file.
    pub url: String,

    /// The name of the artifact after parsing.
    pub name: String,

    /// The target platform.
    pub platform: Platform,

    /// Format of the file if it is an archive, or [`None`] if it is not an archive (we know of).
    pub format: Option<ArchiveFormat>,

    /// The path of the binary in its archive. Irrelevant if [`Self::format`] is `None`.
    pub path: Option<String>,
    /// The digest of the downloaded file, if known. If unknown, the artifact will be fetched and
    /// its digest computed.
    pub digest: Option<Digest>,
    /// The size of the downloaded file, if known. If unknown, the artifact will be fetched for its
    /// size.
    pub size: Option<u64>,
}

// -------------------------------------------------------------------------------------------------
// MARK: ArtifactName

#[derive(snafu::Snafu, Debug)]
pub enum ArtifactNameError {
    #[snafu(display("missing {part} in file name"))]
    Missing { part: &'static str },
    #[snafu(display("unknown segment in file name: {got}"))]
    Unknown { got: String },
    #[snafu(display("unknown format"))]
    UnknownFormat,
}

/// Information gathered about an [`Artifact`] based on its file name.
pub struct ArtifactName {
    pub name: String,
    pub libc: Option<Libc>,
    pub format: Option<ArchiveFormat>,
    pub platform: Platform,
}

impl FromStr for ArtifactName {
    type Err = ArtifactNameError;

    fn from_str(mut file_name: &str) -> Result<Self, Self::Err> {
        let mut format = None;

        for &(ext, fmt) in ArchiveFormat::EXTENSIONS {
            if let Some(stripped) = file_name.strip_suffix(ext) {
                format = Some(fmt);
                file_name = stripped;
                break;
            }
        }

        if let Some(stripped) = file_name.strip_suffix(".exe") {
            file_name = stripped;
        }

        let mut arch = None;
        let mut os = None;
        let mut platform = None;
        let mut libc = None;
        let mut lower_buf = String::new();

        let mut position = 0;
        let mut name_end = None;
        let mut segments = file_name.split(['_', '-', '.']).peekable();

        while let Some(segment) = segments.next() {
            let segment_start = position;

            position += segment.len() + 1; // For separator.

            let lowercase_segment = if segment.bytes().any(|b| b.is_ascii_uppercase()) {
                lower_buf.clear();
                lower_buf.push_str(segment);
                lower_buf.make_ascii_lowercase();
                &lower_buf
            } else {
                segment
            };

            // We're not very strict here, and don't validate that e.g. we only match one
            // architecture, or that `msvc` always follows `windows`.
            match lowercase_segment {
                // Architectures.
                "aarch64" | "arm64" => arch = Some(Architecture::Aarch64),
                "aarch" if segments.next_if_eq(&"64").is_some() => {
                    arch = Some(Architecture::Aarch64)
                }
                "amd64" | "x64" => arch = Some(Architecture::X64),
                "x86" if segments.next_if_eq(&"64").is_some() => arch = Some(Architecture::X64),

                // OSes.
                "apple" => {
                    segments.next_if_eq(&"darwin");
                    os = Some(Os::Macos);
                }
                "darwin" | "macos" | "osx" => {
                    if segments.next_if_eq(&"universal").is_some() {
                        segments.next_if_eq(&"binary");
                        platform = Some(Platform::MacosUniversal);
                    } else {
                        os = Some(Os::Macos);
                    }
                }
                "linux" | "ubuntu" => os = Some(Os::Linux),
                "pc" if segments.next_if_eq(&"windows").is_some() => os = Some(Os::Windows),
                "unknown" if segments.next_if_eq(&"linux").is_some() => os = Some(Os::Linux),
                "windows" => os = Some(Os::Windows),

                // Library.
                "gnu" => libc = Some(Libc::Gnu),
                "musl" => libc = Some(Libc::Musl),
                "msvc" => {} // Ignore.

                // Other.
                "win64" => {
                    arch = Some(Architecture::X64);
                    os = Some(Os::Windows);
                }

                // Version.
                _ if segment
                    .starts_with(['v', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9'])
                    && segment[1..]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'+') =>
                {
                    // Do nothing; this will hit the `name_end` assignment below.
                }
                _ if (segment.starts_with("alpha")
                    || segment.starts_with("beta")
                    || segment.starts_with("dev")
                    || segment.starts_with("rc"))
                    && segment[2..].bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    // Do nothing; this will hit the `name_end` assignment below.
                }

                // Extension.
                _ => {
                    snafu::ensure!(name_end.is_none(), UnknownSnafu { got: segment });
                    continue; // So we don't set `name_end` below.
                }
            }

            if name_end.is_none() {
                name_end = Some(segment_start);
            }
        }

        let platform = if let Some(platform) = platform {
            platform
        } else {
            let arch = arch.context(MissingSnafu {
                part: "architecture",
            })?;
            let os = os.context(MissingSnafu { part: "os" })?;

            Platform::Known { arch, os }
        };

        let name = file_name[0..name_end.unwrap()]
            .trim_end_matches(['_', '-'])
            .to_owned();

        Ok(ArtifactName {
            name,
            format,
            platform,
            libc,
        })
    }
}

#[test]
fn parse_artifact_name() {
    let assert_artifact_name_libc = |file_name, name, platform, format, libc| {
        let actual = ArtifactName::from_str(file_name).unwrap();

        assert_eq!(actual.name, name);
        assert_eq!(actual.platform, platform);
        assert_eq!(actual.format, format);
        assert_eq!(actual.libc, libc);
    };
    let assert_artifact_name = |file_name, name, platform, format| {
        assert_artifact_name_libc(file_name, name, platform, format, None)
    };

    const LINUX_AARCH64: Platform = Platform::Known {
        arch: Architecture::Aarch64,
        os: Os::Linux,
    };
    const LINUX_X64: Platform = Platform::Known {
        arch: Architecture::X64,
        os: Os::Linux,
    };
    const MACOS_AARCH64: Platform = Platform::Known {
        arch: Architecture::Aarch64,
        os: Os::Macos,
    };
    const MACOS_X64: Platform = Platform::Known {
        arch: Architecture::X64,
        os: Os::Macos,
    };
    const WINDOWS_AARCH64: Platform = Platform::Known {
        arch: Architecture::Aarch64,
        os: Os::Windows,
    };
    const WINDOWS_X64: Platform = Platform::Known {
        arch: Architecture::X64,
        os: Os::Windows,
    };
    const WINDOWS_X64_MSVC: Platform = Platform::Known {
        arch: Architecture::X64,
        os: Os::Windows,
    };

    // https://github.com/protocolbuffers/protobuf/releases/tag/v28.2
    assert_artifact_name(
        "protoc-28.2-linux-aarch_64.zip",
        "protoc",
        LINUX_AARCH64,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name(
        "protoc-28.2-linux-x86_64.zip",
        "protoc",
        LINUX_X64,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name(
        "protoc-28.2-osx-aarch_64.zip",
        "protoc",
        MACOS_AARCH64,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name(
        "protoc-28.2-osx-universal_binary.zip",
        "protoc",
        Platform::MacosUniversal,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name(
        "protoc-28.2-osx-x86_64.zip",
        "protoc",
        MACOS_X64,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name(
        "protoc-28.2-win64.zip",
        "protoc",
        WINDOWS_X64,
        Some(ArchiveFormat::Zip),
    );

    // https://github.com/protocolbuffers/protobuf/releases/tag/v29.0-rc2
    assert_artifact_name(
        "protoc-29.0-rc-2-linux-x86_64.zip",
        "protoc",
        LINUX_X64,
        Some(ArchiveFormat::Zip),
    );

    // https://github.com/denoland/deno/releases/tag/v1.46.3
    assert_artifact_name(
        "deno-aarch64-apple-darwin.zip",
        "deno",
        MACOS_AARCH64,
        Some(ArchiveFormat::Zip),
    );

    assert_artifact_name_libc(
        "deno-aarch64-unknown-linux-gnu.zip",
        "deno",
        LINUX_AARCH64,
        Some(ArchiveFormat::Zip),
        Some(Libc::Gnu),
    );

    assert_artifact_name(
        "deno-x86_64-apple-darwin.zip",
        "deno",
        MACOS_X64,
        Some(ArchiveFormat::Zip),
    );

    assert_artifact_name(
        "deno-x86_64-pc-windows-msvc.zip",
        "deno",
        WINDOWS_X64_MSVC,
        Some(ArchiveFormat::Zip),
    );

    assert_artifact_name_libc(
        "deno-x86_64-unknown-linux-gnu.zip",
        "deno",
        LINUX_X64,
        Some(ArchiveFormat::Zip),
        Some(Libc::Gnu),
    );

    // https://github.com/bazelbuild/bazel/releases/tag/7.3.1
    assert_artifact_name("bazel-7.3.1-darwin-arm64", "bazel", MACOS_AARCH64, None);

    assert_artifact_name("bazel-7.3.1-darwin-x86_64", "bazel", MACOS_X64, None);

    assert_artifact_name("bazel-7.3.1-linux-arm64", "bazel", LINUX_AARCH64, None);

    assert_artifact_name("bazel-7.3.1-linux-x86_64", "bazel", LINUX_X64, None);

    assert_artifact_name(
        "bazel-7.3.1-windows-arm64.exe",
        "bazel",
        WINDOWS_AARCH64,
        None,
    );

    assert_artifact_name(
        "bazel-7.3.1-windows-arm64.zip",
        "bazel",
        WINDOWS_AARCH64,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name("bazel-7.3.1-windows-x86_64.exe", "bazel", WINDOWS_X64, None);
    assert_artifact_name(
        "bazel-7.3.1-windows-x86_64.zip",
        "bazel",
        WINDOWS_X64,
        Some(ArchiveFormat::Zip),
    );

    // https://github.com/facebook/buck2/releases/tag/2024-09-16
    assert_artifact_name(
        "buck2-aarch64-apple-darwin.zst",
        "buck2",
        MACOS_AARCH64,
        Some(ArchiveFormat::Zst),
    );

    assert_artifact_name_libc(
        "buck2-aarch64-unknown-linux-gnu.zst",
        "buck2",
        LINUX_AARCH64,
        Some(ArchiveFormat::Zst),
        Some(Libc::Gnu),
    );

    assert_artifact_name_libc(
        "buck2-aarch64-unknown-linux-musl.zst",
        "buck2",
        LINUX_AARCH64,
        Some(ArchiveFormat::Zst),
        Some(Libc::Musl),
    );

    assert_artifact_name(
        "buck2-x86_64-apple-darwin.zst",
        "buck2",
        MACOS_X64,
        Some(ArchiveFormat::Zst),
    );

    assert_artifact_name(
        "buck2-x86_64-pc-windows-msvc.exe.zst",
        "buck2",
        WINDOWS_X64_MSVC,
        Some(ArchiveFormat::Zst),
    );

    assert_artifact_name_libc(
        "buck2-x86_64-unknown-linux-gnu.zst",
        "buck2",
        LINUX_X64,
        Some(ArchiveFormat::Zst),
        Some(Libc::Gnu),
    );

    assert_artifact_name_libc(
        "buck2-x86_64-unknown-linux-musl.zst",
        "buck2",
        LINUX_X64,
        Some(ArchiveFormat::Zst),
        Some(Libc::Musl),
    );

    // https://github.com/BurntSushi/ripgrep/releases/tag/14.1.1
    assert_artifact_name(
        "ripgrep-14.1.1-aarch64-apple-darwin.tar.gz",
        "ripgrep",
        MACOS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name_libc(
        "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz",
        "ripgrep",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Gnu),
    );
    assert_artifact_name(
        "ripgrep-14.1.1-x86_64-apple-darwin.tar.gz",
        "ripgrep",
        MACOS_X64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name_libc(
        "ripgrep-14.1.1-x86_64-pc-windows-gnu.zip",
        "ripgrep",
        WINDOWS_X64,
        Some(ArchiveFormat::Zip),
        Some(Libc::Gnu),
    );
    assert_artifact_name(
        "ripgrep-14.1.1-x86_64-pc-windows-msvc.zip",
        "ripgrep",
        WINDOWS_X64_MSVC,
        Some(ArchiveFormat::Zip),
    );
    assert_artifact_name_libc(
        "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz",
        "ripgrep",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );

    // https://github.com/cameron-martin/bazel-lsp/releases/tag/v0.6.1
    assert_artifact_name(
        "bazel-lsp-0.6.1-osx-arm64",
        "bazel-lsp",
        MACOS_AARCH64,
        None,
    );

    // https://github.com/bazelbuild/bazel-watcher/releases/tag/v0.25.3
    assert_artifact_name("ibazel_darwin_arm64", "ibazel", MACOS_AARCH64, None);
    assert_artifact_name("ibazel_linux_amd64", "ibazel", LINUX_X64, None);
    assert_artifact_name("ibazel_windows_amd64.exe", "ibazel", WINDOWS_X64, None);

    // https://github.com/sharkdp/bat/releases/tag/v0.25.0
    assert_artifact_name(
        "bat-v0.25.0-aarch64-apple-darwin.tar.gz",
        "bat",
        MACOS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name_libc(
        "bat-v0.25.0-aarch64-unknown-linux-musl.tar.gz",
        "bat",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );
    assert_artifact_name_libc(
        "bat-v0.25.0-x86_64-unknown-linux-gnu.tar.gz",
        "bat",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Gnu),
    );

    // https://github.com/rr-debugger/rr/releases/tag/5.9.0
    assert_artifact_name(
        "rr-5.9.0-Linux-aarch64.tar.gz",
        "rr",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "rr-5.9.0-Linux-x86_64.tar.gz",
        "rr",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
    );

    // https://github.com/facebook/dotslash/releases/tag/v0.5.9
    assert_artifact_name_libc(
        "dotslash-linux-musl.aarch64.tar.gz",
        "dotslash",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );
    assert_artifact_name_libc(
        "dotslash-linux-musl.arm64.v0.5.9.tar.gz",
        "dotslash",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );
    assert_artifact_name_libc(
        "dotslash-linux-musl.x86_64.tar.gz",
        "dotslash",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );
    assert_artifact_name_libc(
        "dotslash-linux-musl.x86_64.v0.5.9.tar.gz",
        "dotslash",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
        Some(Libc::Musl),
    );
    assert_artifact_name(
        "dotslash-macos-amd64.v0.5.9.tar.gz",
        "dotslash",
        MACOS_X64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-macos-arm64.tar.gz",
        "dotslash",
        MACOS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-macos-arm64.v0.5.9.tar.gz",
        "dotslash",
        MACOS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-macos-x86_64.tar.gz",
        "dotslash",
        MACOS_X64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-ubuntu-22.04.aarch64.tar.gz",
        "dotslash",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-ubuntu-22.04.arm64.v0.5.9.tar.gz",
        "dotslash",
        LINUX_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-ubuntu-22.04.x86_64.tar.gz",
        "dotslash",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-ubuntu-22.04.x86_64.v0.5.9.tar.gz",
        "dotslash",
        LINUX_X64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-windows-arm64.tar.gz",
        "dotslash",
        WINDOWS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );
    assert_artifact_name(
        "dotslash-windows-arm64.v0.5.9.tar.gz",
        "dotslash",
        WINDOWS_AARCH64,
        Some(ArchiveFormat::TarGz),
    );

    // https://ziglang.org/builds/zig-x86_64-linux-0.17.0-dev.1422+e863bf3be.tar.xz
    assert_artifact_name(
        "zig-x86_64-linux-0.17.0-dev.1422+e863bf3be.tar.xz",
        "zig",
        LINUX_X64,
        Some(ArchiveFormat::TarXz),
    );
}

// -------------------------------------------------------------------------------------------------
// MARK: Digest

#[derive(Clone, Copy, Debug)]
pub enum Digest {
    Sha256([u8; 256 / 8]),
    Blake3(blake3::Hash),
}

impl Digest {
    /// Loads a digest from `s`: if it is an `https:` URL, fetches it and parses it. Otherwise,
    /// expects a `sha256:` or `blake3:` scheme followed by either a hexadecimal or base64 hash.
    pub async fn parse_or_fetch(
        client: &reqwest::Client,
        s: &str,
    ) -> Result<Self, snafu::WhateverLocal> {
        if s.starts_with("https:") {
            Self::fetch(client, s).await
        } else {
            Self::parse(s, "")
        }
    }

    pub async fn fetch(client: &reqwest::Client, url: &str) -> Result<Self, snafu::WhateverLocal> {
        let parsed =
            reqwest::Url::parse(url).with_whatever_context(|_| format!("invalid url: {url}"))?;

        let response = with_gh_token(client.get(parsed))
            .send()
            .await
            .with_whatever_context(|_| format!("failed to get {url}"))?
            .error_for_status()
            .with_whatever_context(|_| format!("failed to get {url}"))?;

        // We trust the URL we fetch (after all, it's pretty important, it gives us a digest we use
        // to trust the binary we'll execute later), so we just download the whole thing without a
        // limit (which we can't easily do: https://github.com/seanmonstar/reqwest/issues/1234).
        let response_bytes = response
            .bytes()
            .await
            .with_whatever_context(|_| format!("failed to read {url}"))?;

        // We expect something like a `sha256sum`, so a hex hash followed by a space and a filename.
        let data = &response_bytes[..];

        let hash_end = data
            .iter()
            .position(|x| x.is_ascii_whitespace())
            .unwrap_or(data.len());

        let bytes = Self::parse_digest_bytes(&data[..hash_end]).with_whatever_context(|| {
            format!("failed to parse digest in {url} ({})", data.escape_ascii())
        })?;

        Ok(Self::Sha256(bytes))
    }

    /// Parses a `<algo>:<hash>` or `<algo>-<hash>` string from `s`, with `<algo>` `sha256` or
    /// `blake3`.
    pub fn parse<'a>(mut data: &'a str, mut algo: &'a str) -> Result<Self, snafu::WhateverLocal> {
        if algo.is_empty() {
            (algo, data) = data
                .split_once([':', '-'])
                .with_whatever_context(|| format!("unknown digest {data:?}"))?;
        }

        let ctor: fn([u8; 32]) -> Digest = if algo == "sha256" {
            |bytes| Digest::Sha256(bytes)
        } else if algo == "blake3" {
            |bytes| Digest::Blake3(blake3::Hash::from_bytes(bytes))
        } else {
            snafu::whatever!("unknown digest algorithm {algo}")
        };

        let bytes = Self::parse_digest_bytes(data.as_bytes())
            .with_whatever_context(|| format!("failed to parse digest in {data:?}"))?;

        Ok(ctor(bytes))
    }

    /// Parses the digest data from either hex or base64.
    fn parse_digest_bytes(data: &[u8]) -> Option<[u8; 32]> {
        match data.len() {
            64 => Some(*blake3::Hash::from_hex(data).ok()?.as_bytes()),

            43 | 44 => {
                let engine = if data.iter().any(|&x| x == b'-' || x == b'_') {
                    base64::engine::general_purpose::URL_SAFE
                } else {
                    base64::engine::general_purpose::STANDARD
                };
                let mut bytes = [0u8; 32];

                base64::Engine::decode_slice(&engine, data, &mut bytes).ok()?;

                Some(bytes)
            }

            _ => None,
        }
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Platforms

/// [`Artifact`]s for all supported platforms.
#[derive(Default, Debug)]
pub struct Artifacts {
    pub linux_aarch64: Option<Artifact>,
    pub linux_x64: Option<Artifact>,
    pub macos_aarch64: Option<Artifact>,
    pub macos_x64: Option<Artifact>,
    pub windows_aarch64: Option<Artifact>,
    pub windows_x64: Option<Artifact>,
}

impl Artifacts {
    pub fn set(&mut self, artifact: Artifact) {
        match artifact.platform {
            Platform::Known { arch, os } => *self.os_arch_mut(os, arch) = Some(artifact),
            Platform::MacosUniversal => {
                self.macos_aarch64 = Some(Artifact {
                    platform: Platform::Known {
                        arch: Architecture::Aarch64,
                        os: Os::Macos,
                    },
                    ..artifact.clone()
                });
                self.macos_x64 = Some(Artifact {
                    platform: Platform::Known {
                        arch: Architecture::X64,
                        os: Os::Macos,
                    },
                    ..artifact
                });
            }
        }
    }

    pub fn os_arch_mut(&mut self, os: Os, arch: Architecture) -> &mut Option<Artifact> {
        self.platform_mut(Platform::Known { arch, os }).unwrap()
    }

    pub fn platform_mut(&mut self, platform: Platform) -> Option<&mut Option<Artifact>> {
        match platform {
            Platform::Known { arch, os } => Some(match (os, arch) {
                (Os::Linux, Architecture::Aarch64) => &mut self.linux_aarch64,
                (Os::Linux, Architecture::X64) => &mut self.linux_x64,
                (Os::Macos, Architecture::Aarch64) => &mut self.macos_aarch64,
                (Os::Macos, Architecture::X64) => &mut self.macos_x64,
                (Os::Windows, Architecture::Aarch64) => &mut self.windows_aarch64,
                (Os::Windows, Architecture::X64) => &mut self.windows_x64,
            }),
            Platform::MacosUniversal => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (Platform, &Artifact)> {
        let platforms = [
            (Os::Linux, Architecture::Aarch64, &self.linux_aarch64),
            (Os::Linux, Architecture::X64, &self.linux_x64),
            (Os::Macos, Architecture::Aarch64, &self.macos_aarch64),
            (Os::Macos, Architecture::X64, &self.macos_x64),
            (Os::Windows, Architecture::Aarch64, &self.windows_aarch64),
            (Os::Windows, Architecture::Aarch64, &self.windows_aarch64),
        ];

        platforms.into_iter().filter_map(|(os, arch, platform)| {
            Some((Platform::Known { arch, os }, platform.as_ref()?))
        })
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Architecture / OS

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Architecture {
    Aarch64,
    X64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

// -------------------------------------------------------------------------------------------------
// MARK: Options

#[derive(Clone, Default)]
pub struct CommonOptions {
    pub path: Option<String>,
}

impl CommonOptions {
    fn parse_with(
        s: &str,
        mut unknown_kv: impl FnMut(&str, Option<&str>) -> Result<(), clap::Error>,
    ) -> Result<CommonOptions, clap::Error> {
        let mut options = Self { path: None };

        if s.is_empty() {
            return Ok(options);
        }

        for entry in s.split(',') {
            let (k, v) = entry
                .split_once('=')
                .map_or((s, None), |(k, v)| (k, Some(v)));

            match k {
                "path" => {
                    options.path = Some(v.ok_or_else(|| err("path requires a value"))?.to_owned());
                }
                _ => unknown_kv(k, v)?,
            }
        }

        Ok(options)
    }
}

impl FromStr for Target {
    type Err = clap::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (before_hash, after_hash) = s.split_once('#').unwrap_or((s, ""));

        if let Some(url) = before_hash.strip_prefix("https:") {
            if let Some(gh_path) = url.strip_prefix("//github.com/") {
                // Try to parse a GitHub release.
                let mut segments = gh_path.split('/').peekable();

                if let Some(org) = segments.next()
                    && let Some(repo) = segments.next()
                {
                    let mut release = None;
                    let mut fully_parsed = false;

                    if segments.next_if_eq(&"releases").is_some() {
                        match segments.next() {
                            None => {
                                fully_parsed = true;
                            }
                            Some("latest") => {
                                fully_parsed = segments.next().is_none();
                            }
                            Some("tag") => {
                                if let Some(tag) = segments.next()
                                    && segments.next().is_none()
                                {
                                    release = Some(tag);
                                    fully_parsed = true;
                                }
                            }
                            Some(_) => {
                                // Unknown path, fallback to HTTP.
                            }
                        }
                    } else if segments.next().is_none() {
                        fully_parsed = true;
                    }

                    if fully_parsed {
                        return parse_gh_release("github.com", org, repo, release, after_hash)
                            .map(Target::GitHubRelease);
                    }
                }
            }

            parse_http(before_hash, after_hash).map(Target::Http)
        } else if let Some(path) = before_hash.strip_prefix("github:") {
            // Parse path.
            let mut segments = path.split('/').peekable();

            let host = segments
                .next_if(|s| s.contains('.'))
                .unwrap_or("github.com");
            let org = segments
                .next()
                .ok_or_else(|| err("expected org/repo or org/repo/release"))?;
            let repo = segments
                .next()
                .ok_or_else(|| err("expected org/repo or org/repo/release"))?;
            let release = segments.next();

            if release.is_some() && segments.next().is_some() {
                return Err(err("too many segments in github: uri"));
            }

            parse_gh_release(host, org, repo, release, after_hash).map(Target::GitHubRelease)
        } else {
            Err(clap::Error::new(clap::error::ErrorKind::ValueValidation))
        }
    }
}

fn parse_gh_release(
    host: &str,
    org: &str,
    repo: &str,
    release: Option<&str>,
    options: &str,
) -> Result<GitHubRelease, clap::Error> {
    // Parse options.
    let mut platforms = Platform::ALL;
    let mut platform_count = 0;

    let mut prefer_gnu = false;

    let options = CommonOptions::parse_with(options, |k, v| {
        match k {
            "platform" => {
                parse_platform(
                    &mut platforms,
                    &mut platform_count,
                    v.ok_or_else(|| err("platform must be given a value"))?,
                )?;
            }
            "gnu" => {
                if v.is_some() {
                    return Err(err("gnu does not take a value"));
                }
                prefer_gnu = true;
            }
            "musl" => {
                if v.is_some() {
                    return Err(err("musl does not take a value"));
                }
                prefer_gnu = false;
            }
            _ => return Err(err(format!("unknown option {k:?}"))),
        }

        Ok(())
    })?;

    Ok(GitHubRelease {
        host: host.to_owned(),
        org: org.to_owned(),
        repo: repo.to_owned(),
        release: release.map(str::to_owned),
        platforms,
        platform_count,
        prefer_gnu,
        options,
    })
}

fn parse_http(url: &str, options: &str) -> Result<Http, clap::Error> {
    // Parse options.
    let mut digest = None;
    let mut size = None;

    let options = CommonOptions::parse_with(options, |k, v| {
        match k {
            "sha256" => {
                digest = Some(
                    Digest::parse(
                        v.ok_or_else(|| err("sha256 must be given a value"))?,
                        "sha256",
                    )
                    .map_err(|e| err(e.to_string()))?,
                );
            }
            "blake3" => {
                digest = Some(
                    Digest::parse(
                        v.ok_or_else(|| err("blake3 must be given a value"))?,
                        "blake3",
                    )
                    .map_err(|e| err(e.to_string()))?,
                );
            }
            "digest" => {
                digest = Some(
                    Digest::parse(v.ok_or_else(|| err("digest must be given a value"))?, "")
                        .map_err(|e| err(e.to_string()))?,
                );
            }
            "size" => {
                let v = v
                    .ok_or_else(|| err("size must be given a value"))?
                    .parse::<u64>()
                    .map_err(|_| err("size is not a valid integer"))?;

                size = Some(v);
            }
            _ => return Err(err(format!("unknown option {k:?}"))),
        }

        Ok(())
    })?;

    Ok(Http {
        url: url.to_owned(),
        digest,
        options,
        size,
    })
}

fn parse_platform(
    platforms: &mut [Platform; Platform::ALL.len()],
    platform_count: &mut usize,
    s: &str,
) -> Result<(), clap::Error> {
    let mut add_platform = |arch, os| {
        let platform = Platform::Known { arch, os };

        if !platforms.contains(&platform) {
            platforms[*platform_count] = platform;
            *platform_count += 1;
        }
    };

    const ARCH_NAMES: &[(&str, Architecture)] = &[
        ("aarch64", Architecture::Aarch64),
        ("arm64", Architecture::Aarch64),
        ("amd64", Architecture::X64),
        ("x64", Architecture::X64),
        ("x86_64", Architecture::X64),
    ];
    const OS_NAMES: &[(&str, Os)] = &[
        ("linux", Os::Linux),
        ("macos", Os::Macos),
        ("windows", Os::Windows),
    ];

    if let Some((os, arch)) = s.split_once('-') {
        let os = OS_NAMES
            .iter()
            .find_map(|&(name, os)| (name == s).then_some(os))
            .ok_or_else(|| err(format!("unknown os {os:?}")))?;
        let arch = ARCH_NAMES
            .iter()
            .find_map(|&(name, arch)| (name == s).then_some(arch))
            .ok_or_else(|| err(format!("unknown arch {arch:?}")))?;

        add_platform(arch, os);
    } else {
        match s {
            "aarch64" | "arm64" => {
                add_platform(Architecture::Aarch64, Os::Linux);
                add_platform(Architecture::Aarch64, Os::Macos);
                add_platform(Architecture::Aarch64, Os::Windows);
            }
            "amd64" | "x64" | "x86_64" => {
                add_platform(Architecture::X64, Os::Linux);
                add_platform(Architecture::X64, Os::Macos);
                add_platform(Architecture::X64, Os::Windows);
            }
            "linux" => {
                add_platform(Architecture::Aarch64, Os::Linux);
                add_platform(Architecture::X64, Os::Linux);
            }
            "macos" => {
                add_platform(Architecture::Aarch64, Os::Macos);
                add_platform(Architecture::X64, Os::Macos);
            }
            "windows" => {
                add_platform(Architecture::Aarch64, Os::Windows);
                add_platform(Architecture::X64, Os::Windows);
            }
            _ => return Err(err(format!("unknown platform {s:?}"))),
        }
    }

    Ok(())
}

fn err(s: impl Into<String>) -> clap::Error {
    let mut error = clap::Error::new(clap::error::ErrorKind::ValueValidation);

    error.insert(
        clap::error::ContextKind::InvalidValue,
        clap::error::ContextValue::String(s.into()),
    );
    error
}

// -------------------------------------------------------------------------------------------------
// MARK: ArchiveFormat

/// The archive/compression format of a downloaded file, determined from its file extension.
///
/// See https://dotslash-cli.com/docs/dotslash-file/#artifact-format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveFormat {
    Tar,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Zip,
    Gz,
    Xz,
    Zst,
    Bz2,
}

impl ArchiveFormat {
    /// File extensions recognized by [`ArchiveFormat::from_file_name()`], longest/most specific
    /// first so e.g. `.tar.gz` is matched before `.gz`.
    pub const EXTENSIONS: &[(&str, ArchiveFormat)] = &[
        (".tar.zst", ArchiveFormat::TarZst),
        (".tar.gz", ArchiveFormat::TarGz),
        (".tar.xz", ArchiveFormat::TarXz),
        (".tar.bz2", ArchiveFormat::TarBz2),
        (".tgz", ArchiveFormat::TarGz),
        (".txz", ArchiveFormat::TarXz),
        (".tzst", ArchiveFormat::TarZst),
        (".tar", ArchiveFormat::Tar),
        (".zip", ArchiveFormat::Zip),
        (".bz2", ArchiveFormat::Bz2),
        (".gz", ArchiveFormat::Gz),
        (".xz", ArchiveFormat::Xz),
        (".zst", ArchiveFormat::Zst),
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            ArchiveFormat::Tar => "tar",
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::TarXz => "tar.xz",
            ArchiveFormat::TarZst => "tar.zst",
            ArchiveFormat::TarBz2 => "tar.bz2",
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Gz => "gz",
            ArchiveFormat::Xz => "xz",
            ArchiveFormat::Zst => "zst",
            ArchiveFormat::Bz2 => "bz2",
        }
    }

    /// Whether files of this format contain named entries whose paths can be listed (as opposed
    /// to e.g. a bare `gz` file, which is just a single compressed stream).
    pub const fn has_entries(&self) -> bool {
        match self {
            ArchiveFormat::Tar
            | ArchiveFormat::TarGz
            | ArchiveFormat::TarXz
            | ArchiveFormat::TarZst
            | ArchiveFormat::TarBz2
            | ArchiveFormat::Zip => true,
            ArchiveFormat::Gz | ArchiveFormat::Xz | ArchiveFormat::Zst | ArchiveFormat::Bz2 => {
                false
            }
        }
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Platform

/// Platform of an [`Artifact`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Known { arch: Architecture, os: Os },
    MacosUniversal,
}

impl Platform {
    pub const ALL: [Platform; 7] = [
        Platform::Known {
            arch: Architecture::Aarch64,
            os: Os::Linux,
        },
        Platform::Known {
            arch: Architecture::X64,
            os: Os::Linux,
        },
        Platform::Known {
            arch: Architecture::Aarch64,
            os: Os::Macos,
        },
        Platform::Known {
            arch: Architecture::X64,
            os: Os::Macos,
        },
        Platform::Known {
            arch: Architecture::Aarch64,
            os: Os::Windows,
        },
        Platform::Known {
            arch: Architecture::X64,
            os: Os::Windows,
        },
        Platform::MacosUniversal,
    ];

    pub const fn name(&self) -> &'static str {
        // https://dotslash-cli.com/docs/dotslash-file/
        match self {
            Platform::Known { arch, os } => match (os, arch) {
                (Os::Linux, Architecture::Aarch64) => "linux-aarch64",
                (Os::Linux, Architecture::X64) => "linux-x86_64",
                (Os::Macos, Architecture::Aarch64) => "macos-aarch64",
                (Os::Macos, Architecture::X64) => "macos-x86_64",
                (Os::Windows, Architecture::Aarch64) => "windows-aarch64",
                (Os::Windows, Architecture::X64) => "windows-x86_64",
            },
            Platform::MacosUniversal => "macos-universal",
        }
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: Miscellaneous

/// `libc` variant of a [`Platform`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Libc {
    Gnu,
    Musl,
}
