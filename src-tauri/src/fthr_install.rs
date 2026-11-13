//! Offers to download and install the FTHR replay video engine.
//!
//! The engine is not bundled with AimMod. When it is missing, AimMod asks once
//! (a native dialog, so it works in minimal mode without any webview) and, if
//! the user agrees, downloads the pinned release, verifies its SHA-256 and
//! extracts it into the per-user tools directory. Recording starts using it
//! straight away; no restart is needed. Declining is remembered; the tray item
//! offers the install again.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

const ENGINE_URL: &str =
    "https://gitlab.com/9dotd1/fthr/-/releases/v4-aimmod/downloads/FTHR-Engine-v4-aimmod-win64.zip";
/// SHA-256 of `FTHR-Engine-v4-aimmod-win64.zip` from the v4-aimmod release.
const ENGINE_SHA256: &str = "41ea83fb44997b3df00c84888f1b4c9bfa0ad0d4ab2b7d720d9751c4a41adeca";
const ENGINE_EXE_NAME: &str = "FTHRclips.exe";
const DECLINED_MARKER: &str = "fthr-engine.declined";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const DIALOG_TITLE: &str = "AimMod replay video";

static IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Called at startup when no engine was found. Prompts unless the user
/// already declined.
pub fn offer_install_on_startup(app: &AppHandle) {
    if declined_marker(app).is_some_and(|marker| marker.is_file()) {
        log::info!("fthr_install: engine missing; install was declined earlier (tray offers it)");
        return;
    }
    spawn_offer(app, true);
}

/// Called from the tray: always asks, even after an earlier decline.
pub fn offer_install(app: &AppHandle) {
    spawn_offer(app, false);
}

fn spawn_offer(app: &AppHandle, from_startup: bool) {
    let app = app.clone();
    // Blocking dialogs must never run on the main (event loop) thread.
    let _ = std::thread::Builder::new()
        .name("fthr-install".into())
        .spawn(move || offer(&app, from_startup));
}

fn offer(app: &AppHandle, from_startup: bool) {
    if crate::fthr_recorder::engine_installed() {
        if !from_startup {
            app.dialog()
                .message("The FTHR replay video engine is already installed.")
                .title(DIALOG_TITLE)
                .kind(MessageDialogKind::Info)
                .blocking_show();
        }
        return;
    }
    if IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return;
    }

    let accepted = app
        .dialog()
        .message(format!(
            "AimMod records replay video with the FTHR engine, which is not installed.\n\n\
             Download it now (about 46 MB) from:\n{}\n\n\
             Without it, replays may not be saved.",
            download_url()
        ))
        .title(DIALOG_TITLE)
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Download and install".into(),
            "Not now".into(),
        ))
        .blocking_show();

    if !accepted {
        if from_startup {
            if let Some(marker) = declined_marker(app) {
                if let Some(parent) = marker.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&marker, b"");
            }
        }
        log::info!("fthr_install: user declined the engine install");
        IN_PROGRESS.store(false, Ordering::SeqCst);
        return;
    }
    if let Some(marker) = declined_marker(app) {
        let _ = fs::remove_file(marker);
    }

    match install(app) {
        Ok(exe) => {
            log::info!("fthr_install: engine installed at {}", exe.display());
            crate::fthr_recorder::engine_available(exe);
            app.dialog()
                .message("The FTHR replay video engine is installed. Replays will now include video.")
                .title(DIALOG_TITLE)
                .kind(MessageDialogKind::Info)
                .blocking_show();
        }
        Err(error) => {
            log::warn!("fthr_install: engine install failed: {error}");
            app.dialog()
                .message(format!(
                    "Could not install the FTHR replay video engine:\n{error}\n\n\
                     You can try again from the AimMod tray menu."
                ))
                .title(DIALOG_TITLE)
                .kind(MessageDialogKind::Error)
                .blocking_show();
        }
    }
    IN_PROGRESS.store(false, Ordering::SeqCst);
}

fn declined_marker(app: &AppHandle) -> Option<PathBuf> {
    let dir = crate::fthr_recorder::managed_engine_dir(app)?;
    Some(dir.parent()?.join(DECLINED_MARKER))
}

/// `AIMMOD_FTHR_ENGINE_URL` overrides the release; it then requires
/// `AIMMOD_FTHR_ENGINE_SHA256` so every download stays verified.
fn download_url() -> String {
    std::env::var("AIMMOD_FTHR_ENGINE_URL").unwrap_or_else(|_| ENGINE_URL.to_string())
}

fn expected_sha256() -> Result<String, String> {
    match std::env::var("AIMMOD_FTHR_ENGINE_URL") {
        Ok(_) => std::env::var("AIMMOD_FTHR_ENGINE_SHA256")
            .map(|s| s.trim().to_ascii_lowercase())
            .map_err(|_| "AIMMOD_FTHR_ENGINE_URL is set without AIMMOD_FTHR_ENGINE_SHA256".into()),
        Err(_) => Ok(ENGINE_SHA256.to_string()),
    }
}

fn install(app: &AppHandle) -> Result<PathBuf, String> {
    let target = crate::fthr_recorder::managed_engine_dir(app)
        .ok_or("could not resolve the AimMod data directory")?;
    let tools_dir = target.parent().ok_or("invalid engine directory")?.to_path_buf();
    fs::create_dir_all(&tools_dir).map_err(|e| format!("create {}: {e}", tools_dir.display()))?;

    let expected = expected_sha256()?;
    let url = download_url();
    log::info!("fthr_install: downloading {url}");
    let client = reqwest::blocking::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let bytes = client
        .get(&url)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
        .map_err(|e| format!("download failed: {e}"))?;

    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(format!(
            "download checksum mismatch (expected {expected}, got {actual})"
        ));
    }

    let staging = tools_dir.join("fthr-engine.staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| format!("clear staging: {e}"))?;
    }
    fs::create_dir_all(&staging).map_err(|e| format!("create staging: {e}"))?;
    let result = extract_and_place(&bytes, &staging, &target);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn extract_and_place(bytes: &[u8], staging: &Path, target: &Path) -> Result<PathBuf, String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("open archive: {e}"))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| format!("read archive: {e}"))?;
        // `enclosed_name` rejects absolute paths and `..` traversal.
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let out_path = staging.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("extract: {e}"))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("extract: {e}"))?;
        }
        let mut output = fs::File::create(&out_path).map_err(|e| format!("extract: {e}"))?;
        std::io::copy(&mut entry, &mut output).map_err(|e| format!("extract: {e}"))?;
    }

    let engine_dir = find_engine_dir(staging)
        .ok_or_else(|| format!("{ENGINE_EXE_NAME} not found in the downloaded archive"))?;
    if target.exists() {
        fs::remove_dir_all(target).map_err(|e| format!("replace {}: {e}", target.display()))?;
    }
    fs::rename(&engine_dir, target).map_err(|e| format!("install to {}: {e}", target.display()))?;
    Ok(target.join(ENGINE_EXE_NAME))
}

fn find_engine_dir(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join(ENGINE_EXE_NAME).is_file() {
            return Some(dir);
        }
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            if entry.path().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn find_engine_dir_locates_nested_engine() {
        let root = std::env::temp_dir().join(format!("fthr-install-test-{}", std::process::id()));
        let nested = root.join("FTHR-Engine-v4-aimmod-win64");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join(ENGINE_EXE_NAME), b"").unwrap();
        assert_eq!(find_engine_dir(&root), Some(nested));
        let _ = fs::remove_dir_all(&root);
    }
}
