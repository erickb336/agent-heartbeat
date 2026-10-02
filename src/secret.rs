//! The bearer secret for the receiver.
//!
//! It lives in the login Keychain (service "agent-heartbeat", account "ingest"). The tool reaches it
//! through Apple's `/usr/bin/security`: a Keychain item trusts the code signature of the program that
//! reads it, and a rebuilt binary has a new signature, so a direct read would ask for access after
//! each upgrade. The secret never appears on a command line: it goes to `security -i` on stdin.
//!
//! The site stores only the SHA-256 of the secret, so the secret itself never leaves the Mac except
//! in the Authorization header over HTTPS.

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

use crate::hash;

const SECURITY: &str = "/usr/bin/security";
const SERVICE: &str = "agent-heartbeat";
const ACCOUNT: &str = "ingest";

pub fn generate() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| e.to_string())?;
    Ok(hash::hex(&bytes))
}

/// The value the site stores: the SHA-256 of the secret, as hex.
pub fn fingerprint(secret: &str) -> String {
    hash::hex(&hash::sha256(secret.as_bytes()))
}

pub fn store(secret: &str) -> Result<(), String> {
    if secret.is_empty() || !secret.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\') {
        return Err("the secret must be printable ASCII without quotes, backslashes or spaces".into());
    }
    let mut child = Command::new(SECURITY)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start security: {e}"))?;
    let command = format!("add-generic-password -U -s {SERVICE} -a {ACCOUNT} -w \"{secret}\"\n");
    child.stdin.take().unwrap().write_all(command.as_bytes()).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("the Keychain did not accept the secret".into());
    }
    match read_keychain()? {
        Some(read) if read == secret => Ok(()),
        _ => Err("the Keychain did not return the stored secret".into()),
    }
}

pub fn read_keychain() -> Result<Option<String>, String> {
    let out = Command::new(SECURITY)
        .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start security: {e}"))?;
    if !out.status.success() {
        return Ok(None); // not found
    }
    let secret = String::from_utf8(out.stdout).map_err(|_| "the stored secret is not text".to_string())?;
    Ok(Some(secret.trim_end_matches('\n').to_string()).filter(|s| !s.is_empty()))
}

pub fn delete() -> Result<bool, String> {
    let status = Command::new(SECURITY)
        .args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot start security: {e}"))?;
    Ok(status.success())
}

/// Reads a secret file. It must belong to the current user, with mode 0600 or stricter.
pub fn read_file(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|_| "the secret file cannot be read".to_string())?;
    if meta.uid() != unsafe { libc::getuid() } || meta.permissions().mode() & 0o077 != 0 {
        return Err("the secret file must belong to you with mode 0600".into());
    }
    let text = fs::read_to_string(path).map_err(|_| "the secret file cannot be read".to_string())?;
    let secret = text.trim().to_string();
    if secret.is_empty() {
        return Err("the secret file is empty".into());
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;

    #[test]
    fn generated_secrets_are_long_random_hex() {
        let (a, b) = (generate().unwrap(), generate().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(fingerprint("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn secret_files_must_be_private() {
        let dir = std::env::temp_dir().join(format!("ah-secret-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let open = dir.join("open");
        fs::write(&open, "s3cret").unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_file(&open).is_err());
        let private = dir.join("private");
        let _ = fs::remove_file(&private);
        fs::OpenOptions::new().write(true).create(true).mode(0o600).open(&private).unwrap().write_all(b"s3cret\n").unwrap();
        assert_eq!(read_file(&private).unwrap(), "s3cret");
    }
}
