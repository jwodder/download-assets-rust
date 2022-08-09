use crate::http_util::{get_next_link, StatusError};
use crate::potential_file::PotentialFile;
use crate::task_stream::TaskStream;
use anyhow::Context;
use async_stream::try_stream;
use ghrepo::GHRepo;
use itertools::Itertools; // for .join()
use log::{error, info, warn};
use reqwest::Client;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::create_dir_all;
use tokio::io::AsyncWriteExt;
use tokio_stream::{Stream, StreamExt};

static USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CARGO_PKG_REPOSITORY"),
    ")",
);

/// A client for asynchronously downloading release assets of a given GitHub
/// repository
#[derive(Clone, Debug)]
pub struct AssetDownloader {
    /// The reqwest client object
    client: Client,
    /// The parsed owner & name of the repository we're downloading from
    repo: GHRepo,
    /// The root directory in which to save release assets.  The assets for a
    /// given release will all be placed in a subdirectory named after the
    /// release's tag.
    download_dir: PathBuf,
}

impl AssetDownloader {
    /// Construct a new `AssetDownloader` for the given repository and download
    /// directory
    ///
    /// # Panics
    ///
    /// Panics if unable to build the [`reqwest::Client`]
    pub fn new(repo: GHRepo, download_dir: PathBuf) -> Self {
        AssetDownloader {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("Error creating client"),
            repo,
            download_dir,
        }
    }

    /// Fetch the details for the release with the given tag from the GitHub
    /// API.
    ///
    /// If there is no such release, this returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// This will return a [`reqwest::Error`] if the HTTP request fails or if
    /// the response body cannot be decoded.  It will return a [`StatusError`]
    /// if the response has a 4xx or 5xx status code other than 404.
    pub async fn get_release(&self, tag: &str) -> Result<Option<Release>, anyhow::Error> {
        info!("Fetching details for release {tag}");
        let r = self
            .client
            .get(format!("{}/releases/tags/{}", self.repo.api_url(), tag))
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            warn!("{tag}: no such release");
            return Ok(None);
        }
        let r = StatusError::error_for_status(r).await?;
        Ok(Some(r.json::<Release>().await?))
    }

    /// Returns a stream of `Release` objects for the given tags.
    ///
    /// Tags which do not correspond to an extant release are discarded.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`get_release()`].
    pub fn get_many_releases(
        self: &Arc<Self>,
        tags: Vec<String>,
    ) -> impl Stream<Item = Result<Release, anyhow::Error>> {
        let mut tasks = TaskStream::new(32);
        for t in tags {
            let downloader = Arc::clone(self);
            tasks.spawn(async move { downloader.get_release(&t).await });
        }
        tasks.into_stream().filter_map(|r| r.transpose())
    }

    /// Paginate through the repository's releases and yield each one
    ///
    /// # Errors
    ///
    /// This will return a [`reqwest::Error`] if an HTTP request fails or if a
    /// response body cannot be decoded.  It will return a [`StatusError`] if a
    /// response has a 4xx or 5xx status code.
    pub fn get_all_releases(&self) -> impl Stream<Item = Result<Release, anyhow::Error>> {
        info!("Fetching all releases for {}", self.repo);
        let repo = self.repo.clone();
        let client = self.client.clone();
        try_stream! {
            let mut url = Some(format!("{}/releases", repo.api_url()));
            while let Some(u) = url {
                let r = StatusError::error_for_status(client.get(u).send().await?).await?;
                url = get_next_link(&r);
                for rel in r.json::<Vec<Release>>().await? {
                    yield rel;
                }
            }
        }
    }

    /// Download the assets for the given releases.
    ///
    /// Returns `true` iff all downloads completed successfully.
    ///
    /// If an error occurs while iterating over `releaseiter`, the error is
    /// logged and the method returns `false` without downloading anything.
    pub async fn download_release_assets<S>(self: &Arc<Self>, releaseiter: S) -> bool
    where
        S: Stream<Item = Result<Release, anyhow::Error>>,
    {
        let mut releases = Vec::new();
        let mut success = true;
        // We wait until after all releases have been fetched before calling
        // spawn() in order to properly "cancel" if any errors occur while
        // fetching.
        tokio::pin!(releaseiter);
        while let Some(r) = releaseiter.next().await {
            match r {
                Ok(rel) => {
                    if !rel.assets.is_empty() {
                        info!(
                            "Found release {} with assets: {}",
                            rel.tag_name,
                            rel.assets.iter().map(|asset| &asset.name).join(", ")
                        );
                        releases.push(rel);
                    } else {
                        info!("Release {} has no assets", rel.tag_name);
                    }
                }
                Err(e) => {
                    error!("{e:#}");
                    success = false;
                }
            }
        }
        if !success {
            info!("Not downloading anything due to errors fetching release data");
            return false;
        }
        let mut tasks = TaskStream::new(32);
        for rel in releases {
            for asset in &rel.assets {
                let downloader = Arc::clone(self);
                let rel = rel.clone();
                let asset = asset.clone();
                tasks.spawn(async move { downloader.download_asset(rel, asset).await });
            }
        }
        if tasks.is_empty() {
            info!("No assets to download");
            return true;
        }
        let mut downloaded = 0;
        let mut failed = 0;
        let mut stream = tasks.into_stream();
        while let Some(r) = stream.next().await {
            match r {
                Ok(()) => downloaded += 1,
                Err(e) => {
                    error!("{e:#}");
                    failed += 1;
                }
            }
        }
        info!("{downloaded} assets downloaded successfully, {failed} downloads failed");
        failed == 0
    }

    /// Download the given asset belonging to the given release.
    ///
    /// If an error occurs or if the task is cancelled, the download file is
    /// deleted.
    ///
    /// # Errors
    ///
    /// - Returns [`std::io::Error`] if an error occurs while creating the
    ///   download file or one of its parent directories or while writing data
    ///   to the file.
    /// - Returns [`reqwest::Error`] if the HTTP request fails or an error
    ///   occurs while downloading the body.
    /// - Returns [`StatusError`] if the HTTP response has a 4xx or 5xx status
    ///   code.
    async fn download_asset(&self, release: Release, asset: Asset) -> anyhow::Result<()> {
        let parent = self.download_dir.join(&release.tag_name);
        let target = parent.join(&asset.name);
        info!(
            "{}: Downloading {} to {}",
            &release.tag_name,
            &asset.name,
            target.display()
        );
        create_dir_all(&parent)
            .await
            .with_context(|| format!("Error creating {}", parent.display()))?;
        let r = StatusError::error_for_status(self.client.get(&asset.download_url).send().await?)
            .await?;
        let mut downloaded = 0;
        let mut fp = PotentialFile::new(&target)
            .await
            .with_context(|| format!("Error opening {}", target.display()))?;
        let mut stream = r.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .with_context(|| format!("Error reading data from {}", &asset.download_url))?;
            fp.write_all(&chunk)
                .await
                .with_context(|| format!("Error writing to {}", target.display()))?;
            downloaded += chunk.len();
            info!(
                "{}: {}: downloaded {} / {} bytes ({:.2}%)",
                &release.tag_name,
                &asset.name,
                downloaded,
                &asset.size,
                (downloaded as f64) / (asset.size as f64) * 100.0,
            );
        }
        fp.realize();
        info!(
            "{}: {} saved to {}",
            &release.tag_name,
            &asset.name,
            target.display()
        );
        Ok(())
    }
}

/// A release of a GitHub repository
///
/// (The actual API returns more fields than this, but these are the only ones
/// we're interested in.)
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

/// An asset of a GitHub release
///
/// (The actual API returns more fields than this, but these are the only ones
/// we're interested in.)
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    name: String,
    #[serde(rename = "browser_download_url")]
    download_url: String,
    size: u64,
}
