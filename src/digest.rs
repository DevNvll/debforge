use std::path::Path;
use std::process::Command;

use crate::error::{AppError, Result};
use crate::process;

pub const SHA256_HEX_LENGTH: usize = 64;

pub fn sha256_file(path: &Path) -> Result<String> {
    let sha256sum = process::require_tool("sha256sum")?;
    sha256_file_with_tool(path, &sha256sum)
}

pub fn sha256_file_with_tool(path: &Path, sha256sum: &Path) -> Result<String> {
    let output = process::capture_text(Command::new(sha256sum).arg("--").arg(path))?;
    let digest = output
        .split_whitespace()
        .next()
        .ok_or_else(|| AppError::new("sha256sum did not return a digest."))?;
    normalize_sha256(digest)
}

pub fn normalize_sha256(value: &str) -> Result<String> {
    if value.len() != SHA256_HEX_LENGTH || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::new(
            "The SHA-256 value must contain exactly 64 hexadecimal characters.",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::normalize_sha256;

    #[test]
    fn validates_and_normalizes_sha256_text() {
        let uppercase = "A".repeat(64);
        assert_eq!(
            normalize_sha256(&uppercase).expect("digest"),
            "a".repeat(64)
        );
        assert!(normalize_sha256("abc").is_err());
        assert!(normalize_sha256(&"x".repeat(64)).is_err());
    }
}
