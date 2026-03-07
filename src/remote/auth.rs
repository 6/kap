use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Default directory for remote access data.
pub fn data_dir() -> PathBuf {
    dirs_home().join(".devg").join("remote")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// A paired device record.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub paired_at: String,
    pub last_seen: String,
}

/// Generate a self-signed ECDSA TLS certificate, or load existing one.
/// Returns (cert_pem, key_pem, cert_sha256_hex).
pub fn load_or_generate_tls(dir: &Path) -> Result<(String, String, String)> {
    std::fs::create_dir_all(dir).context("creating remote data dir")?;

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path).context("reading cert.pem")?;
        let key_pem = std::fs::read_to_string(&key_path).context("reading key.pem")?;
        let fingerprint = cert_sha256(&cert_pem)?;
        return Ok((cert_pem, key_pem, fingerprint));
    }

    eprintln!("[remote] generating self-signed TLS certificate");

    let params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .context("creating cert params")?;
    // rcgen 0.13 infers algorithm from the key pair

    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generating key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing certificate")?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    std::fs::write(&cert_path, &cert_pem).context("writing cert.pem")?;
    std::fs::write(&key_path, &key_pem).context("writing key.pem")?;

    // Restrict key file permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    let fingerprint = cert_sha256(&cert_pem)?;
    Ok((cert_pem, key_pem, fingerprint))
}

/// Compute SHA-256 fingerprint of a PEM certificate (over the DER bytes).
fn cert_sha256(pem: &str) -> Result<String> {
    let der = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<String>();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &der)
        .context("decoding cert PEM")?;
    let hash = Sha256::digest(&bytes);
    Ok(hex::encode(hash))
}

/// Load or generate the pairing token.
pub fn load_or_generate_pairing_token(dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir)?;
    let token_path = dir.join("token");

    if token_path.exists() {
        let token = std::fs::read_to_string(&token_path)?.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let token = generate_token();
    std::fs::write(&token_path, &token)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(token)
}

/// Generate a 256-bit random token as base64url.
fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// Rotate the pairing token (called after successful pairing).
pub fn rotate_pairing_token(dir: &Path) -> Result<String> {
    let token = generate_token();
    let token_path = dir.join("token");
    std::fs::write(&token_path, &token)?;
    Ok(token)
}

/// Hash a token for storage (we never store plaintext session tokens).
pub fn hash_token(token: &str) -> String {
    let hash = Sha256::digest(token.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

/// Load paired devices from devices.json.
pub fn load_devices(dir: &Path) -> Vec<PairedDevice> {
    let path = dir.join("devices.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save paired devices to devices.json.
pub fn save_devices(dir: &Path, devices: &[PairedDevice]) -> Result<()> {
    let path = dir.join("devices.json");
    let json = serde_json::to_string_pretty(devices)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Validate a bearer token against the pairing token or any paired device.
/// Returns Some(device_id) if valid session token, or Some("pairing") if pairing token.
pub fn validate_token(dir: &Path, token: &str) -> Option<String> {
    // Check pairing token
    let pairing_token = std::fs::read_to_string(dir.join("token"))
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !pairing_token.is_empty() && constant_time_eq(token, &pairing_token) {
        return Some("pairing".to_string());
    }

    // Check session tokens
    let token_hash = hash_token(token);
    let devices = load_devices(dir);
    for device in &devices {
        if device.token_hash == token_hash {
            return Some(device.id.clone());
        }
    }

    None
}

/// Pair a new device: consume the pairing token, issue a session token, rotate.
pub fn pair_device(dir: &Path, device_name: &str) -> Result<String> {
    let session_token = generate_token();
    let now = chrono::Utc::now().to_rfc3339();

    let device = PairedDevice {
        id: generate_short_id(),
        name: device_name.to_string(),
        token_hash: hash_token(&session_token),
        paired_at: now.clone(),
        last_seen: now,
    };

    let mut devices = load_devices(dir);
    devices.push(device);
    save_devices(dir, &devices)?;

    // Rotate pairing token so it can't be reused
    rotate_pairing_token(dir)?;

    Ok(session_token)
}

/// Remove a paired device by ID.
pub fn revoke_device(dir: &Path, device_id: &str) -> Result<bool> {
    let mut devices = load_devices(dir);
    let before = devices.len();
    devices.retain(|d| d.id != device_id);
    let removed = devices.len() < before;
    save_devices(dir, &devices)?;
    Ok(removed)
}

fn generate_short_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Constant-time string comparison.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Detect the local WiFi IP address.
pub fn local_ip() -> Option<String> {
    // Bind a UDP socket and connect to a known address to determine
    // which local interface would be used for LAN traffic.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

/// Render a QR code to the terminal.
pub fn print_qr(data: &str) {
    use qrcode::QrCode;

    let code = match QrCode::new(data) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[remote] failed to generate QR code: {e}");
            eprintln!("[remote] pairing URL: {data}");
            return;
        }
    };

    // Render using Unicode half-blocks for compact output.
    // Each character encodes two vertical modules: top and bottom.
    let colors = code.to_colors();
    let width = code.width();
    let modules: Vec<bool> = colors.iter().map(|c| *c == qrcode::Color::Dark).collect();

    // Add 1-module quiet zone
    let total_w = width + 2;
    let total_h = width + 2;

    let get = |r: usize, c: usize| -> bool {
        if r == 0 || r == total_h - 1 || c == 0 || c == total_w - 1 {
            false // quiet zone
        } else {
            modules[(r - 1) * width + (c - 1)]
        }
    };

    println!();
    // Process two rows at a time using half-block characters
    let mut row = 0;
    while row < total_h {
        let mut line = String::from("  "); // indent
        for col in 0..total_w {
            let top = get(row, col);
            let bottom = if row + 1 < total_h {
                get(row + 1, col)
            } else {
                false
            };
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        println!("{line}");
        row += 2;
    }
    println!();
    println!("  Scan with the devg app to pair");
    println!("  {data}");
    println!();
}

// We need hex encoding. Use sha2's digest output directly.
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "devg-auth-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generate_and_load_tls() {
        let dir = temp_dir("tls");
        let (cert1, key1, fp1) = load_or_generate_tls(&dir).unwrap();
        assert!(cert1.contains("BEGIN CERTIFICATE"));
        assert!(key1.contains("BEGIN PRIVATE KEY"));
        assert_eq!(fp1.len(), 64); // SHA-256 hex

        // Loading again returns the same cert
        let (cert2, _key2, fp2) = load_or_generate_tls(&dir).unwrap();
        assert_eq!(cert1, cert2);
        assert_eq!(fp1, fp2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pairing_token_lifecycle() {
        let dir = temp_dir("token");
        let token1 = load_or_generate_pairing_token(&dir).unwrap();
        assert!(!token1.is_empty());

        // Loading again returns the same token
        let token2 = load_or_generate_pairing_token(&dir).unwrap();
        assert_eq!(token1, token2);

        // Rotating gives a new token
        let token3 = rotate_pairing_token(&dir).unwrap();
        assert_ne!(token1, token3);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn device_pairing_and_validation() {
        let dir = temp_dir("pair");
        let _pairing_token = load_or_generate_pairing_token(&dir).unwrap();

        // Pair a device
        let session_token = pair_device(&dir, "Test iPhone").unwrap();
        assert!(!session_token.is_empty());

        // Session token validates
        let result = validate_token(&dir, &session_token);
        assert!(result.is_some());
        assert_ne!(result.unwrap(), "pairing");

        // Random token does not validate
        assert!(validate_token(&dir, "bogus-token").is_none());

        // Pairing token was rotated
        let old_pairing = _pairing_token;
        let new_pairing = std::fs::read_to_string(dir.join("token"))
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(old_pairing, new_pairing);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn revoke_device() {
        let dir = temp_dir("revoke");
        load_or_generate_pairing_token(&dir).unwrap();
        let _token = pair_device(&dir, "Test").unwrap();

        let devices = load_devices(&dir);
        assert_eq!(devices.len(), 1);
        let id = devices[0].id.clone();

        let removed = super::revoke_device(&dir, &id).unwrap();
        assert!(removed);
        assert!(load_devices(&dir).is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("ab", "abc"));
    }

    #[test]
    fn local_ip_returns_something() {
        // This may fail in CI with no network, but should work locally
        let ip = local_ip();
        if let Some(ref ip) = ip {
            assert!(!ip.is_empty());
            assert!(!ip.starts_with("127."));
        }
    }
}
