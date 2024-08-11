use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::fs::{File, ReadDir};
use tokio_stream::Stream;

// TODO: symbolic links?
pub struct FileGenerator {
    paths: Vec<PathBuf>,
    dir: Option<ReadDir>,
}

macro_rules! try_unwrap {
    ($val:expr) => {
        match $val {
            Ok(inner) => inner,
            Err(err) => return Some(Err(err)),
        }
    };
}

impl FileGenerator {
    pub fn new(path: &Path) -> Self {
        Self {
            paths: vec![path.to_path_buf()],
            dir: Default::default(),
        }
    }

    /// Returns the next file in the current subtree, recursively.
    pub async fn next(&mut self) -> Option<Result<PathBuf, io::Error>> {
        loop {
            if let Some(next_res) = self.next_in_dirs().await {
                return Some(next_res);
            }

            let next = self.paths.pop()?;
            let attr = try_unwrap!(tokio::fs::metadata(&next).await);
            if attr.is_dir() {
                let read_dir = try_unwrap!(tokio::fs::read_dir(next).await);
                self.dir = Some(read_dir);
            } else {
                return Some(Ok(next));
            }
        }
    }

    /// Returns the first file path in the current dir, if any.
    ///
    /// Will update the paths stack with any nested dirs it encounters
    async fn next_in_dirs(&mut self) -> Option<Result<PathBuf, io::Error>> {
        while let Some(dir) = self.dir.as_mut() {
            let entry = match dir.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    self.dir.take();
                    continue;
                }
                Err(err) => return Some(Err(err)),
            };

            let meta = try_unwrap!(entry.file_type().await);
            if meta.is_dir() {
                self.paths.push(entry.path());
                continue;
            } else {
                return Some(Ok(entry.path()));
            }
        }
        None
    }
}
