mod downloader;
mod http_util;
mod potential_file;
use crate::downloader::AssetDownloader;
use clap::Parser;
use ghrepo::GHRepo;
use log::{error, LevelFilter};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::signal::ctrl_c;
use tokio_util::either::Either;

/// Download the release assets for the given releases of the given GitHub
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
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!("[{:<5}] {}", record.level(), message))
        })
        .level(LevelFilter::Info)
        .chain(std::io::stderr())
        .apply()
        .unwrap();
    let args = Arguments::parse();
    if args.tags.is_empty() && !args.all {
        eprintln!("No tags specified on command line");
        return ExitCode::FAILURE;
    }
    let download_dir = match args.download_dir {
        Some(d) => d,
        None => std::env::current_dir().expect("Could not determine current directory"),
    };
    let downloader = Arc::new(AssetDownloader::new(args.repo, download_dir));
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
            error!("Ctrl-C received; cancelling downloads");
            ExitCode::FAILURE
        }
    }
}
