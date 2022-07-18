use async_stream::{stream, try_stream};
use clap::Parser;
use fern::Dispatch;
use futures::future::Either;
use ghrepo::GHRepo;
use indent_write::indentable::Indentable;
use itertools::Itertools; // for .join()
use log::{error, info, warn, LevelFilter};
use reqwest::{header::LINK, Client, Response};
use serde::Deserialize;
use std::env::current_dir;
use std::io::stderr;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::fs::{create_dir_all, remove_file, File};
use tokio::io::AsyncWriteExt;
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
    /// Fetch the details for the release with the given tag from the API.  If
    /// there is no such release, returns `None`.
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

    /// Returns a stream of `Release` objects for the given tags.  Tags which
    /// do not correspond to an extant release are discarded.  If an error
    /// occurs, pending retrievals are cancelled, and the error is reraised.
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
                    Ok(Some(r)) => {yield Ok(r); }
                    Ok(None) => {continue;}
                    Err(e) => {yield Err(e); }
                }
            }
        }
    }

    /// Paginate through the repository's releases and yield each one
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

    /// Download the assets for the given releases.  Returns `true` iff all
    /// downloads completed successfully.
    ///
    /// If an HTTP error occurs while iterating over `releaseiter()`, it is
    /// logged and the method returns `False` without downloading anything.
    /// Non-HTTP errors propagate out.
    ///
    /// If an unexpected error occurs while downloading some file, all
    /// remaining downloads are cancelled.
    async fn download_release_assets<S>(&self, mut releaseiter: S) -> Result<bool, anyhow::Error>
    where
        S: Stream<Item = Result<Release, anyhow::Error>> + std::marker::Unpin,
    {
        let mut releases = Vec::new();
        // We wait until after all releases have been fetched before calling
        // spawn() in order to properly "cancel" if any errors occur while
        // fetching.
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
                    return Ok(false);
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
            return Ok(true);
        }
        let mut downloaded = 0;
        let mut failed = 0;
        let stream = aiter_until_error(tasks);
        tokio::pin!(stream);
        while let Some(r) = stream.next().await {
            match r {
                Ok(true) => downloaded += 1,
                Ok(false) => failed += 1,
                Err(e) => return Err(e),
            }
        }
        info!("{downloaded} assets downloaded successfully, {failed} downloads failed");
        Ok(failed == 0)
    }

    /// Download the given asset belonging to the given release.
    ///
    /// If an error occurs, the download file is deleted.
    // TODO: Look for a way to delete the file if the task is cancelled.
    async fn download_asset(&self, release: Release, asset: Asset) -> Result<bool, anyhow::Error> {
        let parent = self.download_dir.join(&release.tag_name);
        let target = parent.join(&asset.name);
        info!(
            "{}: Downloading {} to {}",
            &release.tag_name,
            &asset.name,
            target.display()
        );
        if let Err(e) = create_dir_all(&parent).await {
            error!("Error creating {}: {e}", parent.display());
            return Ok(false);
        }
        let r = self.client.get(&asset.download_url).send().await?;
        let r = match StatusError::error_for_status(r).await {
            Ok(r) => r,
            Err(e) => {
                error!("{e}");
                return Ok(false);
            }
        };
        let mut downloaded = 0;
        let mut fp = match File::create(&target).await {
            Ok(f) => f,
            Err(e) => {
                error!("Error opening {}: {e}", target.display());
                return Ok(false);
            }
        };
        let mut stream = r.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    if let Err(e) = fp.write(&chunk).await {
                        error!("Error writing to {}: {e}", target.display());
                        let _ = remove_file(target);
                        return Err(e.into());
                    }
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
                Err(e) => {
                    error!("Error reading data from {}: {e}", &asset.download_url);
                    let _ = remove_file(target);
                    return Err(e.into());
                }
            }
        }
        info!(
            "{}: {} saved to {}",
            &release.tag_name,
            &asset.name,
            target.display()
        );
        Ok(true)
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
    // TODO: Use select! with a cntrl-c listener here:
    tokio::pin!(releases);
    match downloader.download_release_assets(releases).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Given a list of tasks, yield their results as they become available.  If a
/// task returns an error, all further tasks are cancelled.
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
                Err(_) => {
                    tasks.shutdown().await;
                    // TODO: Yield or report some error?
                    break;
                }
            }
        }
    }
}

#[derive(Debug)]
struct StatusError {
    url: reqwest::Url,
    status: reqwest::StatusCode,
    body: Option<String>,
}

impl StatusError {
    async fn error_for_status(r: Response) -> Result<Response, StatusError> {
        let status = r.status();
        if status.is_client_error() || status.is_server_error() {
            let url = r.url().clone();
            // TODO: If the response body is JSON, pretty-print it.
            let body = r.text().await.ok();
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
                "Request to {} returned {}: {}\n\n{}\n",
                self.url,
                self.status,
                self.status.canonical_reason().unwrap_or("<Unknown Reason>"),
                text.indented("    "),
            ),
            None => write!(
                f,
                "Request to {} returned {}: {}",
                self.url,
                self.status,
                self.status.canonical_reason().unwrap_or("<Unknown Reason>"),
            ),
        }
    }
}

impl std::error::Error for StatusError {}

fn get_next_link(r: &Response) -> Option<String> {
    let header_value = r.headers().get(LINK)?.to_str().ok()?;
    parse_link_header::parse_with_rel(header_value)
        .ok()
        .and_then(|links| links.get("next").map(|ln| ln.raw_uri.clone()))
}
