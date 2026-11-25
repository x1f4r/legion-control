//! Compiles in an optional preview update public key from `LEGION_PREVIEW_UPDATE_KEY` and an
//! optional build sequence from `LEGION_BUILD_SEQUENCE`.
//!
//! Official release builds leave `LEGION_PREVIEW_UPDATE_KEY` unset so no preview key exists in the
//! binary and preview update sets can never pass verification. When set at build time, the key is
//! strictly validated as an Ed25519 public key (SPKI PEM file, inline SPKI PEM, raw 32-byte
//! base64, 44-byte SPKI base64, or 64-char hex) and any invalid input fails the build immediately.

use std::env;
use std::fs;
use std::path::Path;

/// ASN.1 DER SubjectPublicKeyInfo prefix for a 32-byte Ed25519 public key (RFC 8410).
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LEGION_PREVIEW_UPDATE_KEY");
    println!("cargo:rerun-if-env-changed=LEGION_BUILD_SEQUENCE");

    let out_dir = match env::var("OUT_DIR") {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("legion-update build.rs: OUT_DIR is not set: {err}");
            std::process::exit(1);
        }
    };

    let preview_key = match env::var("LEGION_PREVIEW_UPDATE_KEY") {
        Ok(raw) if !raw.trim().is_empty() => match load_preview_key(raw.trim()) {
            Ok(bytes) => Some(bytes),
            Err(msg) => {
                eprintln!("legion-update build.rs: invalid LEGION_PREVIEW_UPDATE_KEY: {msg}");
                std::process::exit(1);
            }
        },
        _ => None,
    };

    let generated = match preview_key {
        Some(bytes) => {
            let formatted = bytes
                .iter()
                .map(|b| format!("0x{b:02x}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "/// Optional Ed25519 preview update public key compiled in via `LEGION_PREVIEW_UPDATE_KEY`.\n\
                 pub const PREVIEW_PUBLIC_KEY: Option<[u8; 32]> = Some([{formatted}]);\n"
            )
        }
        None => {
            "/// Optional Ed25519 preview update public key compiled in via `LEGION_PREVIEW_UPDATE_KEY`.\n\
             pub const PREVIEW_PUBLIC_KEY: Option<[u8; 32]> = None;\n"
                .to_owned()
        }
    };

    let dest = Path::new(&out_dir).join("preview_key.rs");
    if let Err(err) = fs::write(&dest, generated) {
        eprintln!(
            "legion-update build.rs: failed to write {}: {err}",
            dest.display()
        );
        std::process::exit(1);
    }

    if let Ok(raw_seq) = env::var("LEGION_BUILD_SEQUENCE") {
        let trimmed = raw_seq.trim();
        if !trimmed.is_empty() {
            match trimmed.parse::<u64>() {
                Ok(seq) if seq > 0 => {
                    println!("cargo:rustc-env=LEGION_UPDATE_SEQUENCE={seq}");
                }
                _ => {
                    eprintln!(
                        "legion-update build.rs: invalid LEGION_BUILD_SEQUENCE: expected a positive integer"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

fn load_preview_key(input: &str) -> Result<[u8; 32], String> {
    if input.contains("BEGIN PUBLIC KEY") {
        return parse_public_key_text(input);
    }
    let candidate = Path::new(input);
    if candidate.is_file() {
        println!("cargo:rerun-if-changed={}", candidate.display());
        let text = fs::read_to_string(candidate)
            .map_err(|err| format!("could not read key file {}: {err}", candidate.display()))?;
        return parse_public_key_text(&text);
    }
    if input.contains('/')
        || input.contains('\\')
        || input.ends_with(".pem")
        || input.ends_with(".pub")
    {
        return Err(format!("key file {} does not exist", candidate.display()));
    }
    parse_public_key_text(input)
}

fn parse_public_key_text(text: &str) -> Result<[u8; 32], String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("key input is empty".to_owned());
    }
    if trimmed.contains("PRIVATE KEY") {
        return Err(
            "refusing private key material in LEGION_PREVIEW_UPDATE_KEY; pass only the public key"
                .to_owned(),
        );
    }

    let raw = if trimmed.contains("-----BEGIN PUBLIC KEY-----") {
        let mut body = String::new();
        let mut inside = false;
        let mut closed = false;
        for line in trimmed.lines() {
            let line = line.trim();
            if line == "-----BEGIN PUBLIC KEY-----" {
                if inside {
                    return Err("duplicate PEM header".to_owned());
                }
                inside = true;
                continue;
            }
            if line == "-----END PUBLIC KEY-----" {
                if !inside {
                    return Err("unexpected PEM footer".to_owned());
                }
                closed = true;
                break;
            }
            if inside {
                body.push_str(line);
            }
        }
        if !inside || !closed || body.is_empty() {
            return Err("malformed SPKI PEM public key".to_owned());
        }
        decode_base64(&body)?
    } else if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        decode_hex(trimmed)?
    } else {
        decode_base64(trimmed)?
    };

    let key: [u8; 32] = if raw.len() == 44 {
        if raw[..12] != ED25519_SPKI_PREFIX {
            return Err("SPKI key is not an Ed25519 public key".to_owned());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw[12..]);
        out
    } else if raw.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw);
        out
    } else {
        return Err(format!(
            "expected a 32-byte raw Ed25519 public key or 44-byte SPKI DER key, got {} bytes",
            raw.len()
        ));
    };

    if key == [0u8; 32] {
        return Err("all-zero public key is not valid".to_owned());
    }

    Ok(key)
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i]).ok_or_else(|| "invalid hex digit".to_owned())?;
        let lo = hex_nibble(bytes[i + 1]).ok_or_else(|| "invalid hex digit".to_owned())?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() || !clean.len().is_multiple_of(4) {
        return Err("invalid base64 length".to_owned());
    }
    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    for (chunk_idx, chunk) in clean.as_chunks::<4>().0.iter().enumerate() {
        let is_last = (chunk_idx + 1) * 4 == clean.len();
        let a = b64_val(chunk[0]).ok_or_else(|| "invalid base64 character".to_owned())?;
        let b = b64_val(chunk[1]).ok_or_else(|| "invalid base64 character".to_owned())?;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        if (pad2 || pad3) && !is_last {
            return Err("unexpected base64 padding before end".to_owned());
        }
        if pad2 && !pad3 {
            return Err("invalid base64 padding".to_owned());
        }
        out.push((a << 2) | (b >> 4));
        if !pad2 {
            let c = b64_val(chunk[2]).ok_or_else(|| "invalid base64 character".to_owned())?;
            out.push((b << 4) | (c >> 2));
            if !pad3 {
                let d = b64_val(chunk[3]).ok_or_else(|| "invalid base64 character".to_owned())?;
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

fn b64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
