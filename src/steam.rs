//! Reads which games are installed in the local Steam libraries.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Installed Steam games as (app id, name), sorted by name. Proton, runtimes and
/// redistributables are left out. Libraries on a mount that doesn't answer within a
/// couple of seconds are skipped.
pub async fn installed_games() -> Vec<(u32, String)> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let roots = [home.join(".local/share/Steam"), home.join(".steam/steam"), home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")];
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in roots {
        let vdf = root.join("steamapps/libraryfolders.vdf");
        let Ok(text) = tokio::fs::read_to_string(&vdf).await else { continue };
        libraries.push(root);
        libraries.extend(vdf_values(&text, "path").map(PathBuf::from));
    }
    let mut seen = std::collections::HashSet::new();
    libraries.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));

    let scans = libraries.into_iter().map(|lib| {
        let task = tokio::task::spawn_blocking(move || scan_library(&lib));
        async move { tokio::time::timeout(Duration::from_secs(2), task).await.ok().and_then(Result::ok).unwrap_or_default() }
    });
    let mut games: Vec<(u32, String)> = futures_util::future::join_all(scans).await.into_iter().flatten().collect();
    games.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    games.dedup_by_key(|g| g.0);
    games
}

fn scan_library(lib: &Path) -> Vec<(u32, String)> {
    let Ok(dir) = std::fs::read_dir(lib.join("steamapps")) else { return vec![] };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id: u32 = name.strip_prefix("appmanifest_")?.strip_suffix(".acf")?.parse().ok()?;
            let text = std::fs::read_to_string(e.path()).ok()?;
            let title = vdf_values(&text, "name").next()?.to_string();
            (!is_tool(&title)).then_some((id, title))
        })
        .collect()
}

fn is_tool(name: &str) -> bool {
    ["Proton", "Steam Linux Runtime", "Steamworks Common Redistributables", "SteamVR"].iter().any(|p| name.starts_with(p))
}

/// Values of every `"key" "value"` line in a Valve KeyValues file.
fn vdf_values<'a>(text: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    text.lines().filter_map(move |line| {
        let mut parts = line.split('"').skip(1).step_by(2);
        (parts.next()? == key).then(|| parts.next()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values() {
        let text = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/a/b\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\" { \"path\" \"/c\" }\n}";
        assert_eq!(vdf_values(text, "path").collect::<Vec<_>>(), ["/a/b"]);
        assert_eq!(vdf_values("\t\"name\"\t\t\"Manor Lords\"", "name").next(), Some("Manor Lords"));
    }
}
