//! NetSarang (Xshell/Xftp) session secret encryption.
//!
//! Validated against Xshell/Xftp 8.1 session files on Chinese Windows:
//! - Cipher: ARC4 (RC4) with key = SHA256(key_material)
//! - Payload: base64( ciphertext ‖ SHA256(plaintext) )
//! - key_material (v7 / v8.1, no master password):
//!   `reverse( reverse(username) + SID )` encoded with system ANSI (CP_ACP / GBK)
//!
//! Plaintext `UserKeyPassPhrase=` is **ignored** by Xftp; must write ciphertext.

use std::process::Command;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Encrypt a password / key-passphrase for embedding in `.xfp` / `.xsh`.
pub fn encrypt_session_secret(plaintext: &str) -> Result<String, AppError> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    let (user, sid) = current_user_and_sid()?;
    let key_mat = key_material_v7_v8(&user, &sid);
    Ok(encrypt_with_key_material(plaintext.as_bytes(), &key_mat))
}

fn encrypt_with_key_material(plaintext: &[u8], key_material: &[u8]) -> String {
    let key = Sha256::digest(key_material);
    let checksum = Sha256::digest(plaintext);
    let ciphertext = rc4(&key, plaintext);
    let mut blob = ciphertext;
    blob.extend_from_slice(&checksum);
    B64.encode(blob)
}

#[cfg(test)]
fn decrypt_with_key_material(b64: &str, key_material: &[u8]) -> Option<Vec<u8>> {
    let data = B64.decode(b64).ok()?;
    if data.len() < 32 {
        return None;
    }
    let (ct, checksum) = data.split_at(data.len() - 32);
    let key = Sha256::digest(key_material);
    let plain = rc4(&key, ct);
    if Sha256::digest(&plain)[..] == *checksum {
        Some(plain)
    } else {
        None
    }
}

/// Version 7 / 8.1 key string (no master password), then system ACP bytes.
fn key_material_v7_v8(username: &str, sid: &str) -> Vec<u8> {
    // strkey1 = reverse(username) + sid
    // strkey2 = reverse(strkey1)
    let rev_user: String = username.chars().rev().collect();
    let strkey1 = format!("{rev_user}{sid}");
    let strkey2: String = strkey1.chars().rev().collect();
    encode_system_ansi(&strkey2)
}

fn current_user_and_sid() -> Result<(String, String), AppError> {
    #[cfg(windows)]
    {
        // Username: prefer %USERNAME% (correct Unicode). whoami CSV can garble CJK.
        let short = std::env::var("USERNAME").map_err(|_| {
            AppError::Io("无法读取 %USERNAME%（NetSarang 加密需要当前用户名）".into())
        })?;
        let short = short.trim().to_string();
        if short.is_empty() {
            return Err(AppError::Io("USERNAME 为空".into()));
        }

        let mut cmd = Command::new("whoami");
        cmd.args(["/user", "/fo", "csv"]);
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let out = cmd
            .output()
            .map_err(|e| AppError::Io(format!("whoami failed: {e}")))?;
        if !out.status.success() {
            return Err(AppError::Io("whoami /user failed".into()));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // "User Name","SID" / "domain\user","S-1-5-21-..."
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let _header = lines.next();
        let row = lines
            .next()
            .ok_or_else(|| AppError::Io("whoami output empty".into()))?;
        // SID is the last CSV field (quoted).
        let sid = row
            .rsplit(',')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if !sid.starts_with("S-1-") {
            return Err(AppError::Io(format!("无法解析 SID: {row}")));
        }
        return Ok((short, sid));
    }
    #[cfg(not(windows))]
    {
        Err(AppError::Message(
            "NetSarang secret encryption is only supported on Windows".into(),
        ))
    }
}

/// Encode Unicode with the system ANSI code page (CP_ACP), matching .NET Encoding.Default.
#[cfg(windows)]
fn encode_system_ansi(s: &str) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    use std::ffi::OsStr;

    #[link(name = "kernel32")]
    extern "system" {
        fn WideCharToMultiByte(
            code_page: u32,
            flags: u32,
            wide: *const u16,
            wide_len: i32,
            multi: *mut u8,
            multi_len: i32,
            default_char: *const i8,
            used_default: *mut i32,
        ) -> i32;
    }
    const CP_ACP: u32 = 0;

    let wide: Vec<u16> = OsStr::new(s).encode_wide().collect();
    unsafe {
        let need = WideCharToMultiByte(
            CP_ACP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if need <= 0 {
            // Fallback: lossy UTF-8 (wrong for CJK usernames, but better than panic).
            return s.as_bytes().to_vec();
        }
        let mut buf = vec![0u8; need as usize];
        let written = WideCharToMultiByte(
            CP_ACP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            buf.as_mut_ptr(),
            need,
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if written <= 0 {
            return s.as_bytes().to_vec();
        }
        buf.truncate(written as usize);
        buf
    }
}

#[cfg(not(windows))]
fn encode_system_ansi(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// Classic ARC4 (same as SharpXDecrypt / HyperSine).
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j: usize = 0;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
        s.swap(i, j);
    }
    let mut i = 0usize;
    j = 0;
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        i = (i + 1) % 256;
        j = (j + s[i] as usize) % 256;
        s.swap(i, j);
        let k = s[(s[i] as usize + s[j] as usize) % 256];
        out.push(b ^ k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii_secret() {
        let user = "testuser";
        let sid = "S-1-5-21-1-2-3-1001";
        let mat = key_material_v7_v8(user, sid);
        let plain = b"hello";
        let enc = encrypt_with_key_material(plain, &mat);
        let dec = decrypt_with_key_material(&enc, &mat).expect("decrypt");
        assert_eq!(dec, plain);
    }

    #[test]
    fn decrypt_known_xshell_passphrase_if_local() {
        // Live check: only passes on the developer machine that owns the sample.
        // Ciphertext from a local Xshell 8.1 session; plaintext is the key passphrase.
        let known = "HyZtCKMEINuDBt362Ke5MiRCWANfzzJdi2mrMJbo9egW+CKmRA==";
        let Ok((user, sid)) = current_user_and_sid() else {
            return;
        };
        let mat = key_material_v7_v8(&user, &sid);
        if let Some(pt) = decrypt_with_key_material(known, &mat) {
            assert_eq!(pt, b"zscvb");
            // re-encrypt must match (RC4 stream is deterministic with same key)
            let re = encrypt_with_key_material(b"zscvb", &mat);
            assert_eq!(re, known);
        }
    }
}
