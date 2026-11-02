//! Terminal-only secret input, shared by the existing commands and setup adapter.
use anyhow::{Result, bail};

pub fn secret_input(label: &str, limit: usize, from_stdin: bool) -> Result<String> {
    use std::io::{BufRead, Read};
    let password = if from_stdin {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(1025)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| anyhow::anyhow!("Cannot read password from stdin"))?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Password must be UTF-8"))?
    } else {
        rpassword::prompt_password(format!("{label}: ")).map_err(|_| {
            let flag = if label == "Password" {
                "--password-stdin"
            } else {
                "--code-stdin"
            };
            anyhow::anyhow!("Cannot read hidden {label}; use {flag} for pipes")
        })?
    };
    if password.is_empty() || password.len() > limit {
        bail!("{label} must contain 1 to {limit} UTF-8 bytes");
    }
    Ok(password)
}
