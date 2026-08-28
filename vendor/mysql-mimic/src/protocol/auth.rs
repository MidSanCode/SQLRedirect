//! MySQL native password authentication.

use sha1::{Digest, Sha1};

/// Default authentication plugin name.
pub const AUTH_PLUGIN_NAME: &str = "mysql_native_password";

/// Safe ASCII characters for nonce generation (alphanumeric only).
/// Matches the Python mysql-mimic approach to avoid binary nonce issues
/// with Java-based clients (e.g., DBeaver/MySQL Connector/J).
const SAFE_NONCE_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Generate a 20-byte random scramble (nonce) for authentication.
///
/// Uses only ASCII alphanumeric characters to ensure compatibility with
/// all MySQL clients, including Java-based ones that decode nonces as ASCII.
pub fn generate_scramble() -> [u8; 20] {
    let mut scramble = [0u8; 20];
    for b in &mut scramble {
        let idx = rand::random::<usize>() % SAFE_NONCE_CHARS.len();
        *b = SAFE_NONCE_CHARS[idx];
    }
    scramble
}

/// Compute the MySQL native password hash:
///
/// `SHA1(password) XOR SHA1(scramble + SHA1(SHA1(password)))`
///
/// This is what the *client* sends; the server compares against its stored hash.
pub fn compute_auth_response(password: &[u8], scramble: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }

    // stage1 = SHA1(password)
    let stage1 = Sha1::digest(password);

    // stage2 = SHA1(stage1)
    let stage2 = Sha1::digest(stage1);

    // hash_stage = SHA1(scramble + stage2)
    let mut hasher = Sha1::new();
    hasher.update(scramble);
    hasher.update(stage2);
    let hash_stage = hasher.finalize();

    // XOR stage1 with hash_stage
    stage1
        .iter()
        .zip(hash_stage.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// Verify a client's auth response against a known password.
///
/// Returns `true` if the response matches.
pub fn verify_auth_response(password: &[u8], scramble: &[u8], client_response: &[u8]) -> bool {
    let expected = compute_auth_response(password, scramble);
    // Constant-time comparison would be better, but this is adequate for a mimic server.
    expected == client_response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scramble_ascii_safe() {
        let scramble = generate_scramble();
        assert!(!scramble.contains(&0));
        // All bytes should be ASCII alphanumeric
        for &b in &scramble {
            assert!(
                b.is_ascii_alphanumeric(),
                "Expected ASCII alphanumeric, got: {}",
                b
            );
        }
    }

    #[test]
    fn test_auth_roundtrip() {
        let password = b"secret";
        let scramble = generate_scramble();
        let response = compute_auth_response(password, &scramble);
        assert!(verify_auth_response(password, &scramble, &response));
    }

    #[test]
    fn test_empty_password() {
        let scramble = generate_scramble();
        let response = compute_auth_response(b"", &scramble);
        assert!(response.is_empty());
    }
}
