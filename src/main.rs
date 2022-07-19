use anyhow::Context;
use async_stream::{stream, try_stream};
use clap::Parser;
use fern::Dispatch;
use futures::future::Either;
use ghrepo::GHRepo;
use indent_write::indentable::Indentable;
use itertools::Itertools; // for .join()
use log::{error, info, warn, LevelFilter};
use mime::{Mime, JSON};
use reqwest::{
    header::{CONTENT_TYPE, LINK},
    Client, Response,
};
use serde::Deserialize;
use serde_json::{to_string_pretty, value::Value};
use std::env::current_dir;
use std::io::stderr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tokio::fs::{create_dir_all, File};
use tokio::io::AsyncWriteExt;
use tokio::signal::ctrl_c;
use tokio::task::JoinSet;
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
struct AssetDownloader {
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
    async fn get_release(&self, tag: &str) -> Result<Option<Release>, anyhow::Error> {
        info!("Fetching details on release {tag}");
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
    /// If an error is encountered, pending retrievals are cancelled, and the
    /// error will be the last item yielded.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`get_release()`].
    fn get_many_releases(
        &self,
        tags: Vec<String>,
    ) -> impl Stream<Item = Result<Release, anyhow::Error>> {
        let mut tasks = JoinSet::new();
        for t in tags {
            let downloader = self.clone();
            tasks.spawn(async move { downloader.get_release(&t).await });
        }
        stream! {
            for await rel in aiter_until_error(tasks) {
                match rel {
                    Ok(Some(r)) => yield Ok(r),
                    Ok(None) => (),
                    Err(e) => yield Err(e),
                }
            }
        }
    }

    /// Paginate through the repository's releases and yield each one
    ///
    /// # Errors
    ///
    /// This will return a [`reqwest::Error`] if an HTTP request fails or if a
    /// response body cannot be decoded.  It will return a [`StatusError`] if a
    /// response has a 4xx or 5xx status code.
    fn get_all_releases(&self) -> impl Stream<Item = Result<Release, anyhow::Error>> {
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
    async fn download_release_assets<S>(&self, releaseiter: S) -> bool
    where
        S: Stream<Item = Result<Release, anyhow::Error>>,
    {
        let mut releases = Vec::new();
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
                    error!("{e}");
                    return false;
                }
            }
        }
        let mut tasks = JoinSet::new();
        for rel in releases {
            for asset in &rel.assets {
                let downloader = self.clone();
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
        while let Some(r) = tasks.join_next().await {
            match r {
                Ok(Ok(())) => downloaded += 1,
                Ok(Err(e)) => {
                    error!("{e:#}");
                    failed += 1;
                }
                Err(e) => {
                    if e.is_panic() {
                        tasks.shutdown().await;
                        std::panic::resume_unwind(e.into_panic());
                    }
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
            fp.write(&chunk)
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
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

/// An asset of a GitHub release
///
/// (The actual API returns more fields than this, but these are the only ones
/// we're interested in.)
#[derive(Clone, Debug, Deserialize)]
struct Asset {
    name: String,
    #[serde(rename = "browser_download_url")]
    download_url: String,
    size: u64,
}

/// Download the release assets for the given tags of the given GitHub
/// repository
#[derive(Parser)]
struct Arguments {
    /// Download assets for all releases
    #[clap(short = 'A', long)]
    all: bool,

    /// Directory in which to download assets [default: current directory]
    #[clap(short, long)]
    download_dir: Option<PathBuf>,

    /// The GitHub repository from which to download assets.  Can be specified
    /// as either OWNER/NAME or https://github.com/OWNER/NAME.
    #[clap(value_parser)]
    repo: GHRepo,

    /// The tags of the releases to download.  At least one tag or the --all
    /// option must be specified.
    tags: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!("[{:<5}] {}", record.level(), message))
        })
        .level(LevelFilter::Info)
        .chain(stderr())
        .apply()
        .unwrap();
    let args = Arguments::parse();
    if args.tags.is_empty() && !args.all {
        eprintln!("No tags specified on command line");
        return ExitCode::FAILURE;
    }
    let download_dir = match args.download_dir {
        Some(d) => d,
        None => current_dir().expect("Could not determine current directory"),
    };
    let downloader = AssetDownloader {
        client: Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("Error creating client"),
        repo: args.repo,
        download_dir,
    };
    let releases = if args.all {
        Either::Left(downloader.get_all_releases())
    } else {
        Either::Right(downloader.get_many_releases(args.tags))
    };
    tokio::select! {
        r = downloader.download_release_assets(releases) => {
            if r {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        },
        _ = ctrl_c() => {
            info!("Ctrl-C received; cancelling downloads");
            ExitCode::FAILURE
        }
    }
}

/// Given a set of tasks, yield their results as they become available.  If a
/// task returns an error, all further tasks are cancelled, and the error will
/// be the last item yielded.
fn aiter_until_error<T: 'static, E: 'static>(
    mut tasks: JoinSet<Result<T, E>>,
) -> impl Stream<Item = Result<T, E>> {
    stream! {
        while let Some(r) = tasks.join_next().await {
            match r {
                Ok(Ok(r)) => yield Ok(r),
                Ok(Err(e)) => {
                    tasks.shutdown().await;
                    yield Err(e);
                    break;
                },
                Err(e) => {
                    if e.is_panic() {
                        tasks.shutdown().await;
                        std::panic::resume_unwind(e.into_panic());
                    }
                }
            }
        }
    }
}

/// Error raised for a 4xx or 5xx HTTP response that includes the response body
#[derive(Debug)]
struct StatusError {
    url: reqwest::Url,
    status: reqwest::StatusCode,
    body: Option<String>,
}

impl StatusError {
    /// If the given response has a 4xx or 5xx status code, construct & return
    /// a `StatusError`; otherwise, return the response unchanged.
    async fn error_for_status(r: Response) -> Result<Response, StatusError> {
        let status = r.status();
        if status.is_client_error() || status.is_server_error() {
            let url = r.url().clone();
            // If the response body is JSON, pretty-print it.
            let body = if is_json_response(&r) {
                r.json::<Value>()
                    .await
                    .ok()
                    .map(|v| to_string_pretty(&v).unwrap())
            } else {
                r.text().await.ok()
            };
            Err(StatusError { url, status, body })
        } else {
            Ok(r)
        }
    }
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.body {
            Some(text) => write!(
                f,
                "Request to {} returned {}\n\n{}\n",
                self.url,
                self.status,
                text.indented("    "),
            ),
            None => write!(f, "Request to {} returned {}", self.url, self.status),
        }
    }
}

impl std::error::Error for StatusError {}

/// Return the "rel=next" URL, if any, from the response's "Link" header.
fn get_next_link(r: &Response) -> Option<String> {
    let header_value = r.headers().get(LINK)?.to_str().ok()?;
    parse_link_header::parse_with_rel(header_value)
        .ok()
        .and_then(|links| links.get("next").map(|ln| ln.raw_uri.clone()))
}

/// Returns `true` iff the response's Content-Type header indicates the body is
/// JSON
fn is_json_response(r: &Response) -> bool {
    r.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Mime>().ok())
        .map(|ct| {
            ct.type_() == "application" && (ct.subtype() == "json" || ct.suffix() == Some(JSON))
        })
        .unwrap_or(false)
}

/// A wrapper around a writable [`tokio::fs::File`] that deletes the file on
/// drop if `realize()` has not been called
struct PotentialFile {
    path: PathBuf,
    file: Option<File>,
}

impl PotentialFile {
    /// Create a new file at the given path
    async fn new<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let file = Some(File::create(&path).await?);
        Ok(PotentialFile {
            path: path.as_ref().into(),
            file,
        })
    }

    /// Mark the file as no longer "potential", so that it will not be deleted
    /// on drop.
    fn realize(mut self) {
        self.file.take();
    }
}

impl Deref for PotentialFile {
    type Target = File;

    fn deref(&self) -> &File {
        self.file
            .as_ref()
            .expect("Cannot use PotentialFile after calling realize()")
    }
}

impl DerefMut for PotentialFile {
    fn deref_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("Cannot use PotentialFile after calling realize()")
    }
}

impl Drop for PotentialFile {
    fn drop(&mut self) {
        match self.file.take() {
            Some(f) => {
                drop(f);
                match std::fs::remove_file(&self.path) {
                    Ok(_) => (),
                    Err(e) => warn!("Failed to remove {}: {e}", self.path.display()),
                }
            }
            None => (),
        }
    }
}
