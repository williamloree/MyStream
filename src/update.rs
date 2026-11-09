//! Self-update from GitHub Releases. The exe is portable: a new version is
//! downloaded next to it, the running exe is renamed to `.old` (Windows allows
//! renaming a running exe) and the new one takes its place.

use sha2::{Digest, Sha256};

const REPO: &str = "williamloree/mystream";
pub const RELEASES_URL: &str = "https://github.com/williamloree/mystream/releases/latest";
const ASSET: &str = "mystream.exe";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    url: String,
    sha256: Option<String>,
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(concat!("MyStream/", env!("CARGO_PKG_VERSION"))).build()?)
}

/// Returns the latest release if it is newer than this build.
pub async fn check() -> anyhow::Result<Option<Release>> {
    Ok(latest().await?.filter(|r| is_newer(&r.version, env!("CARGO_PKG_VERSION"))))
}

async fn latest() -> anyhow::Result<Option<Release>> {
    let body = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let version = v["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
    let Some(asset) = v["assets"].as_array().and_then(|a| a.iter().find(|a| a["name"] == ASSET)) else {
        return Ok(None);
    };
    Ok(Some(Release {
        version,
        url: asset["browser_download_url"].as_str().unwrap_or_default().to_string(),
        sha256: asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
    }))
}

/// Downloads the release and swaps it in place of the running exe.
pub async fn install(r: &Release) -> anyhow::Result<()> {
    swap(&std::env::current_exe()?, &download(r).await?)
}

async fn download(r: &Release) -> anyhow::Result<bytes::Bytes> {
    let bytes = client()?.get(&r.url).send().await?.error_for_status()?.bytes().await?;
    if !bytes.starts_with(b"MZ") {
        anyhow::bail!("le fichier téléchargé n'est pas un exécutable");
    }
    if let Some(expected) = &r.sha256 {
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if &got != expected {
            anyhow::bail!("empreinte SHA-256 invalide, téléchargement corrompu");
        }
    }
    Ok(bytes)
}

fn swap(exe: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let new = exe.with_extension("exe.new");
    let old = exe.with_extension("exe.old");
    std::fs::write(&new, bytes)?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e.into());
    }
    Ok(())
}

/// Starts the (new) exe with the same arguments.
pub fn relaunch() -> anyhow::Result<()> {
    std::process::Command::new(std::env::current_exe()?).args(std::env::args().skip(1)).spawn()?;
    Ok(())
}

/// Removes the previous exe left by an update.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}

fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(remote) > parse(local)
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_compare() {
        assert!(super::is_newer("0.1.2", "0.1.1"));
        assert!(super::is_newer("0.10.0", "0.9.9"));
        assert!(!super::is_newer("0.1.1", "0.1.1"));
        assert!(!super::is_newer("0.1.0", "0.1.1"));
    }

    /// Real GitHub download (with SHA-256 check) swapped over a running exe.
    #[tokio::test]
    async fn download_and_swap_running_exe() {
        let r = super::latest().await.unwrap().expect("release");
        let bytes = super::download(&r).await.unwrap();
        let dir = std::env::temp_dir().join("mystream-update-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("mystream.exe");
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        let mut child = std::process::Command::new(&exe).args(["-n", "5", "127.0.0.1"]).spawn().unwrap();
        super::swap(&exe, &bytes).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), bytes);
        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
