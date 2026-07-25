//! Private key loading with support for legacy OpenSSL-encrypted PEM.
//!
//! `russh::keys::load_secret_key` only recognizes classic PEM encryption when
//! `DEK-Info: AES-128-CBC,...` is present. Keys from older OpenSSL / ssh-keygen
//! often use `DES-EDE3-CBC`. Those ciphertext bytes are then mis-parsed as
//! PKCS#1 DER, producing:
//!   PKCS#1 ASN.1 error: expected SEQUENCE, got APPLICATION [12]
//!
//! For DES-EDE3-CBC we decrypt via OpenSSL EVP_BytesToKey + 3DES-CBC, then
//! hand a clear RSA PEM to russh.

use std::path::Path;

use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use des::TdesEde3;
use md5::{Digest, Md5};
use russh::keys::{decode_secret_key, load_secret_key, PrivateKey};
use tracing::debug;

use crate::error::AppError;

/// Load an SSH private key from disk (including DES-EDE3-CBC encrypted RSA PEM).
pub fn load_private_key(
    path: impl AsRef<Path>,
    passphrase: Option<&str>,
) -> Result<PrivateKey, AppError> {
    let path = path.as_ref();
    let pem = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::Auth(format!("找不到私钥文件: {}", path.display()))
        } else {
            AppError::Auth(format!("读取私钥失败: {e}"))
        }
    })?;

    match inspect_pem(&pem) {
        PemKind::LegacyDesEde3Cbc { iv_hex, body_b64 } => {
            let pass = passphrase.filter(|s| !s.is_empty()).ok_or_else(|| {
                AppError::Auth(
                    "该私钥已加密（DES-EDE3-CBC），请在「私钥口令 / passphrase」中填写口令后重试"
                        .into(),
                )
            })?;
            let clear = decrypt_des_ede3_cbc_rsa_pem(&iv_hex, &body_b64, pass)?;
            debug!("decrypted legacy DES-EDE3-CBC RSA PEM");
            decode_secret_key(&clear, None).map_err(map_key_error)
        }
        PemKind::EncryptedOther { cipher } => {
            // Still try russh first (AES-128-CBC etc.).
            match load_secret_key(path, passphrase) {
                Ok(k) => Ok(k),
                Err(e) => {
                    let _ = e;
                    let has_pass = passphrase.map(|s| !s.is_empty()).unwrap_or(false);
                    if !has_pass {
                        return Err(AppError::Auth(format!(
                            "该私钥已加密（{cipher}），请填写私钥口令（passphrase）后重试"
                        )));
                    }
                    Err(AppError::Auth(format!(
                        "无法解密私钥（算法 {cipher}）。请确认口令正确；\
                         或转换格式：ssh-keygen -p -f <key>  （转为 OpenSSH 格式）"
                    )))
                }
            }
        }
        PemKind::Other => load_secret_key(path, passphrase).map_err(map_key_error),
    }
}

fn map_key_error(e: impl ToString) -> AppError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("password")
        || lower.contains("decrypt")
        || lower.contains("encrypted")
        || lower.contains("mac")
        || lower.contains("key is encrypted")
    {
        AppError::Auth(
            "私钥解密失败：请填写正确的私钥口令（passphrase），或确认私钥文件有效".into(),
        )
    } else if lower.contains("asn.1") || lower.contains("pkcs") {
        AppError::Auth(
            "无法解析私钥。若密钥带口令请填写 passphrase；\
             旧版 DES-EDE3 加密 PEM 需正确口令。也可转换：\
             ssh-keygen -p -f <key>  或  openssl rsa -in <key> -out <clear.pem>"
                .into(),
        )
    } else {
        AppError::Auth(format!("无法加载私钥: {msg}"))
    }
}

enum PemKind {
    LegacyDesEde3Cbc { iv_hex: String, body_b64: String },
    EncryptedOther { cipher: String },
    Other,
}

fn inspect_pem(pem: &str) -> PemKind {
    let mut in_rsa = false;
    let mut encrypted = false;
    let mut cipher: Option<String> = None;
    let mut iv_hex: Option<String> = None;
    let mut body_b64 = String::new();

    for line in pem.lines() {
        let line = line.trim().trim_end_matches('\r');
        if line == "-----BEGIN RSA PRIVATE KEY-----" {
            in_rsa = true;
            continue;
        }
        if !in_rsa {
            if line.contains("BEGIN") && line.contains("PRIVATE KEY") && line.contains("ENCRYPTED")
            {
                return PemKind::EncryptedOther {
                    cipher: "PKCS#8".into(),
                };
            }
            continue;
        }
        if line.starts_with("-----END ") {
            break;
        }
        if line.starts_with("Proc-Type:") && line.to_ascii_uppercase().contains("ENCRYPTED") {
            encrypted = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("DEK-Info:") {
            let rest = rest.trim();
            let mut parts = rest.splitn(2, ',');
            cipher = parts.next().map(|s| s.trim().to_ascii_uppercase());
            iv_hex = parts.next().map(|s| s.trim().to_string());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        {
            body_b64.push_str(line);
        }
    }

    if !in_rsa || !encrypted {
        return PemKind::Other;
    }

    let c = cipher.unwrap_or_default();
    if c == "DES-EDE3-CBC" || c == "DES3" {
        if let Some(iv) = iv_hex {
            return PemKind::LegacyDesEde3Cbc {
                iv_hex: iv,
                body_b64,
            };
        }
    }
    PemKind::EncryptedOther {
        cipher: if c.is_empty() {
            "unknown".into()
        } else {
            c
        },
    }
}

fn decrypt_des_ede3_cbc_rsa_pem(
    iv_hex: &str,
    body_b64: &str,
    password: &str,
) -> Result<String, AppError> {
    let iv = hex::decode(iv_hex.trim())
        .map_err(|_| AppError::Auth("私钥 DEK-Info IV 不是合法十六进制".into()))?;
    if iv.len() != 8 {
        return Err(AppError::Auth(format!(
            "DES-EDE3-CBC 的 IV 应为 8 字节，实际为 {}",
            iv.len()
        )));
    }

    let ciphertext = data_encoding::BASE64
        .decode(body_b64.as_bytes())
        .map_err(|e| AppError::Auth(format!("私钥 Base64 解码失败: {e}")))?;

    // OpenSSL PEM: EVP_BytesToKey(MD5, salt=IV, count=1) → 24-byte key;
    // CBC uses DEK-Info IV (derived IV from BytesToKey is discarded).
    let key = evp_bytes_to_key_md5(password.as_bytes(), &iv, 24);

    let mut buf = ciphertext;
    type TdesCbc = cbc::Decryptor<TdesEde3>;
    let dec = TdesCbc::new_from_slices(&key, &iv)
        .map_err(|e| AppError::Auth(format!("初始化 3DES 解密失败: {e}")))?;
    let plain = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).map_err(|_| {
        AppError::Auth("私钥解密失败：口令可能不正确，或密钥文件已损坏".into())
    })?;

    if plain.first() != Some(&0x30) {
        return Err(AppError::Auth(
            "私钥解密后内容无效（口令可能不正确）".into(),
        ));
    }

    let body = data_encoding::BASE64.encode(plain);
    let mut out = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        // BASE64 alphabet is always UTF-8.
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str("-----END RSA PRIVATE KEY-----\n");
    Ok(out)
}

/// OpenSSL `EVP_BytesToKey` with MD5, count=1.
fn evp_bytes_to_key_md5(password: &[u8], salt: &[u8], key_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(key_len + 16);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < key_len {
        let mut hasher = Md5::new();
        hasher.update(&prev);
        hasher.update(password);
        hasher.update(salt);
        prev = hasher.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    out.truncate(key_len);
    out
}

#[cfg(test)]
mod tests {
    use super::{decrypt_des_ede3_cbc_rsa_pem, evp_bytes_to_key_md5, inspect_pem, PemKind};

    #[test]
    fn key_len_24() {
        let k = evp_bytes_to_key_md5(b"secret", b"12345678", 24);
        assert_eq!(k.len(), 24);
    }

    /// Round-trip against a DES-EDE3-CBC PEM produced by OpenSSL (if present).
    #[test]
    fn decrypt_openssl_des3_fixture_if_present() {
        let path = std::env::temp_dir().join("anchorterm-keytest").join("enc.pem");
        if !path.exists() {
            return;
        }
        let pem = std::fs::read_to_string(&path).unwrap();
        match inspect_pem(&pem) {
            PemKind::LegacyDesEde3Cbc { iv_hex, body_b64 } => {
                let clear =
                    decrypt_des_ede3_cbc_rsa_pem(&iv_hex, &body_b64, "testpass123").expect("decrypt");
                assert!(clear.contains("BEGIN RSA PRIVATE KEY"));
                assert!(!clear.contains("ENCRYPTED"));
            }
            _ => panic!("expected LegacyDesEde3Cbc, got non-matching kind"),
        }
    }
}
