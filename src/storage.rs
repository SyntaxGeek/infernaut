//! Small filesystem primitives shared by credentials and the configuration editor.
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use anyhow::{Context, Result};

/// The lock file stays in place; dropping the handle releases the OS lock.
pub fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open lock {}", path.display()))?;
    file.try_lock().with_context(|| {
        format!(
            "another process is updating {}; retry shortly",
            path.display()
        )
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_excludes_other_handles_and_releases_on_drop() {
        let path = std::env::temp_dir().join(format!("infernaut-lock-{}", rand::random::<u128>()));
        let first = lock(&path).unwrap();
        assert!(lock(&path).is_err());
        drop(first);
        drop(lock(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }
}
