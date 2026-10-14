//! Installs plugins from `.streamDeckPlugin` archives or unpacked `.sdPlugin` folders.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::plugins::{manifest, plugins_dir};

pub fn install(src: &Path) -> Result<PathBuf> {
    let dest_root = plugins_dir();
    std::fs::create_dir_all(&dest_root)?;
    let dest = if src.is_dir() {
        let name = src.file_name().context("bad path")?;
        let dest = dest_root.join(name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        copy_dir(src, &dest)?;
        dest
    } else {
        extract(src, &dest_root)?
    };
    let info = manifest::load(&dest)?;
    println!("installed {} {} -> {}", info.name, info.version, dest.display());
    if info.runtime.is_none() {
        println!("  warning: no code this host can run on linux");
    }
    Ok(dest)
}

fn extract(archive: &Path, dest_root: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("not a zip archive")?;
    // Archives contain a single top-level `<uuid>.sdPlugin/` folder.
    let top = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok()?.enclosed_name())
        .filter_map(|p| p.components().next().map(|c| c.as_os_str().to_owned()))
        .find(|c| c.to_string_lossy().ends_with(".sdPlugin"));
    let Some(top) = top else { bail!("archive has no .sdPlugin folder") };
    let dest = dest_root.join(&top);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    zip.extract(dest_root)?;
    Ok(dest)
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else if ty.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}
