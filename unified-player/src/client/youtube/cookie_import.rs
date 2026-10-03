//! Local cookie import. Never include cookie contents in errors or diagnostics.
use std::{collections::BTreeMap, io::Read, path::Path};

use anyhow::{Context, Result};

const MAX_COOKIE_BYTES: u64 = 64 * 1024;

pub(crate) fn read_cookie_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .context("Cannot open cookie file; check the path and permissions.")?;
    anyhow::ensure!(file.metadata()?.is_file(), "Choose a regular cookie file.");
    let mut content = String::new();
    file.take(MAX_COOKIE_BYTES + 1)
        .read_to_string(&mut content)
        .context("Cookie file must be UTF-8 text.")?;
    anyhow::ensure!(
        content.len() as u64 <= MAX_COOKIE_BYTES,
        "Cookie file exceeds 64 KiB."
    );
    parse_cookie_file(&content)
}

fn parse_cookie_file(content: &str) -> Result<String> {
    let content = content.trim_matches(['\r', '\n', ' ']);
    let mut cookies = BTreeMap::new();
    if content.contains('\t') {
        for line in content.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') && !line.starts_with("#HttpOnly_") {
                continue;
            }
            let fields = line
                .trim_start_matches("#HttpOnly_")
                .split('\t')
                .collect::<Vec<_>>();
            anyhow::ensure!(
                fields.len() == 7,
                "Invalid Netscape cookie export; export YouTube cookies again."
            );
            let domain = fields[0].trim_start_matches('.');
            if domain != "youtube.com" && !domain.ends_with(".youtube.com") {
                continue;
            }
            insert_cookie(&mut cookies, fields[5], fields[6])?;
        }
    } else {
        let header = content
            .strip_prefix("Cookie:")
            .or_else(|| content.strip_prefix("cookie:"))
            .unwrap_or(content)
            .trim();
        anyhow::ensure!(
            !header.contains(['\r', '\n']),
            "Use a single Cookie request header or a Netscape cookie export."
        );
        for pair in header.split(';').filter(|pair| !pair.trim().is_empty()) {
            let (name, value) = pair.trim().split_once('=').context(
                "Invalid Cookie header; copy its value from a signed-in YouTube request.",
            )?;
            insert_cookie(&mut cookies, name, value)?;
        }
    }
    anyhow::ensure!(
        cookies
            .get("SAPISID")
            .is_some_and(|value| !value.is_empty())
            && ["SID", "LOGIN_INFO"]
                .iter()
                .any(|key| cookies.get(*key).is_some_and(|value| !value.is_empty())),
        "Signed-in YouTube cookies are missing. Sign in, reload YouTube Music, and export again."
    );
    Ok(cookies
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; "))
}

fn insert_cookie(cookies: &mut BTreeMap<String, String>, name: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            && value
                .bytes()
                .all(|c| (0x21..=0x7e).contains(&c) && c != b';'),
        "Invalid cookie entry; export signed-in YouTube cookies again."
    );
    cookies.insert(name.to_owned(), value.to_owned());
    Ok(())
}

pub(crate) fn persist_cookie(path: &Path, cookie: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // ytmapi-rs extracts SAPISID up to the next semicolon, even when it is last.
    // Persist the same delimiter used during validation so reopening works too.
    let cookie = format!("{};", cookie.trim_end_matches(';'));
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write(|file| {
            // Set permissions before writing credential bytes, including the temporary file.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            std::io::Write::write_all(file, cookie.as_bytes())
        })
        .context("Could not save imported YouTube credentials.")?;
    super::restrict_token_permissions(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_header_and_netscape_without_other_domains() {
        assert!(parse_cookie_file("SAPISID=identity; SID=session; OPTIONAL=").is_ok());
        assert!(parse_cookie_file("SAPISID=; SID=session").is_err());
        assert_eq!(
            parse_cookie_file("Cookie: SAPISID=identity; SID=session").unwrap(),
            "SAPISID=identity; SID=session"
        );
        let export = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tidentity\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tsession\n.evil-youtube.com\tTRUE\t/\tTRUE\t0\tOTHER\tprivate";
        assert_eq!(
            parse_cookie_file(export).unwrap(),
            "SAPISID=identity; SID=session"
        );
    }

    #[test]
    fn rejects_missing_identity_and_header_injection_without_echoing_values() {
        for content in [
            "SID=private",
            "SAPISID=private",
            "SAPISID=private\nSID=private",
            "SAPISID=private; SID=private\r\nX-Test: value",
        ] {
            let error = parse_cookie_file(content).unwrap_err().to_string();
            assert!(!error.contains("private"));
        }
    }

    #[test]
    fn import_is_bounded_and_persisted_privately() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("cookies.txt");
        std::fs::write(&path, "x".repeat(MAX_COOKIE_BYTES as usize + 1)).unwrap();
        assert!(read_cookie_file(&path).is_err());
        persist_cookie(&path, "LOGIN_INFO=session; SAPISID=identity").unwrap();
        assert!(read_cookie_file(&path).is_ok());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .ends_with("SAPISID=identity;"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
