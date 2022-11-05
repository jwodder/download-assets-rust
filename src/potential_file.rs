use log::warn;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use tokio::fs::File;

/// A wrapper around a writable [`tokio::fs::File`] that deletes the file on
/// drop if `realize()` has not been called
pub struct PotentialFile {
    path: PathBuf,
    file: Option<File>,
}

impl PotentialFile {
    /// Create a new file at the given path
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let file = Some(File::create(&path).await?);
        Ok(PotentialFile {
            path: path.as_ref().into(),
            file,
        })
    }

    /// Mark the file as no longer "potential", so that it will not be deleted
    /// on drop.
    pub fn realize(mut self) {
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
        if let Some(f) = self.file.take() {
            drop(f);
            if let Err(e) = std::fs::remove_file(&self.path) {
                warn!("Failed to remove {}: {e}", self.path.display());
            }
        }
    }
}
