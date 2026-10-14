//! Installs plugins from `.streamDeckPlugin` archives or unpacked `.sdPlugin` folders.
//!
//! New versions are unpacked into a staging folder and validated before they
//! replace anything, so a broken download never destroys a working install.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::plugins::{manifest, plugins_dir};

/// Marketplace plugins with DRM ship an encrypted manifest that starts with this.
const DRM_MAGIC: &[u8] = b"ELGATO";

pub fn install(src: &Path) -> Result<PathBuf> {
    let root = plugins_dir();
    std::fs::create_dir_all(&root)?;
    let staging = root.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    let result = (|| {
        let unpacked = if src.is_dir() {
            let name = src.file_name().context("bad path")?;
            let dest = staging.join(name);
            copy_dir(src, &dest)?;
            dest
        } else {
            extract(src, &staging)?
        };
        check_manifest(&unpacked)?;
        manifest::load(&unpacked)?;
        Ok(unpacked)
    })();

    let unpacked = match result {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let dest = root.join(unpacked.file_name().unwrap());
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(&unpacked, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(dest)
}

/// Gives a clear reason for manifests we can't read, instead of a parse error.
fn check_manifest(dir: &Path) -> Result<()> {
    let bytes = std::fs::read(dir.join("manifest.json")).context("plugin has no manifest.json")?;
    if bytes.starts_with(DRM_MAGIC) {
        bail!(
            "this plugin uses Elgato Marketplace DRM: its manifest is encrypted so that only the official \
             Stream Deck app can load it. Ask the author for a build without DRM (e.g. from GitHub)."
        );
    }
    Ok(())
}

fn extract(archive: &Path, into: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("not a zip archive")?;
    // Archives contain a single top-level `<uuid>.sdPlugin/` folder.
    let top = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok()?.enclosed_name())
        .filter_map(|p| p.components().next().map(|c| c.as_os_str().to_owned()))
        .find(|c| c.to_string_lossy().ends_with(".sdPlugin"));
    let Some(top) = top else { bail!("archive has no .sdPlugin folder") };
    zip.extract(into)?;
    Ok(into.join(top))
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
