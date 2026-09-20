use std::path::{Component, Path, PathBuf};

/// Remote names may select a file, never a directory or an alternate path.
pub(crate) fn validate_filename(filename: &str) -> anyhow::Result<()> {
    let mut components = Path::new(filename).components();
    anyhow::ensure!(
        !filename.is_empty()
            && !filename.contains(['/', '\\', '\0'])
            && matches!(components.next(), Some(Component::Normal(_)))
            && components.next().is_none(),
        "invalid received filename"
    );
    Ok(())
}

pub(crate) struct Download {
    temp: tempfile::NamedTempFile,
    directory: PathBuf,
    filename: String,
}

impl Download {
    pub(crate) fn new(directory: &Path, filename: &str) -> anyhow::Result<Self> {
        validate_filename(filename)?;
        Ok(Self {
            temp: tempfile::NamedTempFile::new_in(directory)?,
            directory: directory.to_owned(),
            filename: filename.to_owned(),
        })
    }

    pub(crate) fn writer(&self) -> anyhow::Result<tokio::fs::File> {
        // Clone the already-open descriptor; never reopen a path that could
        // have been replaced by a symlink between allocation and writing.
        Ok(tokio::fs::File::from_std(self.temp.as_file().try_clone()?))
    }

    pub(crate) fn finish(self, unique: bool) -> anyhow::Result<PathBuf> {
        let mut temp = self.temp;
        temp.as_file().sync_all()?;
        let destination = self.directory.join(&self.filename);
        if !unique {
            // Atomic cache replacement replaces a symlink itself, not its target.
            temp.persist(&destination)?;
            return Ok(destination);
        }

        let name = Path::new(&self.filename);
        let stem = name.file_stem().unwrap().to_string_lossy();
        let extension = name
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        for suffix in 0u32.. {
            let path = if suffix == 0 {
                destination.clone()
            } else {
                self.directory.join(format!("{stem} ({suffix}){extension}"))
            };
            match temp.persist_noclobber(&path) {
                Ok(_) => return Ok(path),
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => temp = e.file,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("no available destination filename")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_remote_paths() {
        for name in ["", ".", "..", "../x", "/tmp/x", "a/b", "a/", "a\\b", "x\0y"] {
            assert!(validate_filename(name).is_err(), "{name:?}");
        }
        for name in ["image.jpg", ".image", "holiday photo.png", "a..b"] {
            assert!(validate_filename(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn concurrent_names_do_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Download::new(dir.path(), "image.jpg").unwrap();
        let mut b = Download::new(dir.path(), "image.jpg").unwrap();
        a.temp.write_all(b"first").unwrap();
        b.temp.write_all(b"second").unwrap();
        let a = a.finish(true).unwrap();
        let b = b.finish(true).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(a).unwrap(), b"first");
        assert_eq!(std::fs::read(b).unwrap(), b"second");
    }

    #[test]
    fn symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"unchanged").unwrap();
        for unique in [true, false] {
            let name = if unique { "shared.jpg" } else { "cached.jpg" };
            std::os::unix::fs::symlink(outside.path(), dir.path().join(name)).unwrap();
            let mut download = Download::new(dir.path(), name).unwrap();
            download.temp.write_all(b"image").unwrap();
            let saved = download.finish(unique).unwrap();
            assert_eq!(std::fs::read(saved).unwrap(), b"image");
            assert_eq!(std::fs::read(outside.path()).unwrap(), b"unchanged");
        }
    }

    #[test]
    fn dropped_download_removes_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let download = Download::new(dir.path(), "image.jpg").unwrap();
        let temporary = download.temp.path().to_owned();
        assert!(temporary.exists());
        drop(download);
        assert!(!temporary.exists());
        assert!(!dir.path().join("image.jpg").exists());
    }
}
