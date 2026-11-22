use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Maximum total attachment size per message (15 MB).
pub const MAX_ATTACHMENT_BYTES: usize = 15 * 1024 * 1024;

/// Directory where received files are stored, relative to the data directory.
pub fn downloads_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("downloads")
}

/// Ensure the downloads directory exists and return its path.
pub fn ensure_downloads_dir(data_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = downloads_dir(data_dir);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Store a file received from a peer under `data/downloads/<YYYYMMDD>_<username>_<filename>`.
/// If the target name already exists, a numeric suffix is appended before the extension.
pub fn store_incoming_file(
    data_dir: &Path,
    created_at: i64,
    username: &str,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = ensure_downloads_dir(data_dir)?;
    let filename = sanitize_filename(filename);
    let username = sanitize_filename(username);
    let date = yyyymmdd_from_millis(created_at);
    let base = format!("{}_{}_{}", date, username, filename);
    let path = unique_path(&dir, &base)?;
    fs::write(&path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;
    }
    Ok(path)
}

/// Ask the OS to open `path` with the default application.
pub fn open_with_os(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        let path_str = path.to_str().unwrap_or("");
        Command::new("cmd")
            .args(["/C", "start", "", path_str])
            .spawn()?;
    }
    Ok(())
}

fn unique_path(dir: &Path, base: &str) -> std::io::Result<PathBuf> {
    let path = dir.join(base);
    if !path.exists() {
        return Ok(path);
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{}", e)),
        None => (base, String::new()),
    };
    for i in 1.. {
        let candidate = dir.join(format!("{} ({}){}", stem, i, ext));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    unreachable!()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c => c,
        })
        .collect()
}

fn yyyymmdd_from_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1_000);
    let dt = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}{:02}{:02}", dt.year(), dt.month() as u8, dt.day())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_file_with_flat_dated_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_incoming_file(dir.path(), 1_735_689_600_000, "alice", "note.txt", b"hello")
            .unwrap();
        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("20250101_alice_note.txt"), "got {name}");
    }

    #[test]
    fn deduplicates_colliding_names() {
        let dir = tempfile::tempdir().unwrap();
        store_incoming_file(dir.path(), 1_735_689_600_000, "alice", "note.txt", b"first")
            .unwrap();
        let second =
            store_incoming_file(dir.path(), 1_735_689_600_000, "alice", "note.txt", b"second")
                .unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"second");
        let name = second.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("20250101_alice_note (1).txt"), "got {name}");
    }

    #[test]
    fn sanitizes_dangerous_filename_characters() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_incoming_file(dir.path(), 1_735_689_600_000, "a/b", "x/y\0z.txt", b"hi")
            .unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.contains("a_b"));
        assert!(name.contains("x_y_z.txt"));
    }
}
