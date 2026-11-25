//! Compiles in an optional preview update public key from `LEGION_PREVIEW_UPDATE_KEY` and an
//! optional build sequence from `LEGION_BUILD_SEQUENCE`.
//!
//! Official release builds leave `LEGION_PREVIEW_UPDATE_KEY` unset so no preview key exists in the
//! binary and preview update sets can never pass verification. When set at build time, the key must
//! be an Ed25519 SubjectPublicKeyInfo PEM (`-----BEGIN PUBLIC KEY-----`, either as a file path or
//! inline PEM); private keys and other encodings fail the build immediately. The build prints
//! whether a preview key was compiled in (its `SHA256:<base64>` fingerprint, never key material).

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
            let fp = fingerprint(&bytes);
            println!("cargo:warning=legion-update: preview update key compiled in ({fp})");
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
            println!("cargo:warning=legion-update: release update key only (no preview key)");
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

    let seq_value: u64 = match env::var("LEGION_BUILD_SEQUENCE") {
        Ok(raw_seq) if !raw_seq.trim().is_empty() => {
            let trimmed = raw_seq.trim();
            match trimmed.parse::<u64>() {
                Ok(seq) if is_valid_sequence(seq) => seq,
                _ => {
                    eprintln!(
                        "legion-update build.rs: invalid LEGION_BUILD_SEQUENCE={trimmed:?}: expected a UTC YYYYMMDDHHMM timestamp or a small positive test sequence (1..=20000000)"
                    );
                    std::process::exit(1);
                }
            }
        }
        _ => 0,
    };
    println!("cargo:rustc-env=LEGION_UPDATE_SEQUENCE={seq_value}");
}

fn is_valid_sequence(seq: u64) -> bool {
    if seq == 0 {
        return false;
    }
    if seq <= 20_000_000 {
        return true;
    }
    if !(202_001_010_000..=299_912_312_359).contains(&seq) {
        return false;
    }
    let minute = (seq % 100) as u32;
    let hour = ((seq / 100) % 100) as u32;
    let day = ((seq / 10_000) % 100) as u32;
    let month = ((seq / 1_000_000) % 100) as u32;
    let year = (seq / 100_000_000) as i64;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || day < 1 {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    day <= max_day
}

fn load_preview_key(input: &str) -> Result<[u8; 32], String> {
    if input.contains("PRIVATE KEY") {
        return Err(
            "refusing private key material in LEGION_PREVIEW_UPDATE_KEY; pass only the SPKI public key PEM"
                .to_owned(),
        );
    }
    if input.contains("-----BEGIN PUBLIC KEY-----") {
        return parse_spki_pem(input);
    }
    let candidate = Path::new(input);
    if !candidate.is_file() {
        return Err(format!(
            "expected a path to an Ed25519 SPKI PEM file (-----BEGIN PUBLIC KEY-----) or inline PEM; {} does not exist",
            candidate.display()
        ));
    }
    println!("cargo:rerun-if-changed={}", candidate.display());
    let text = fs::read_to_string(candidate)
        .map_err(|err| format!("could not read key file {}: {err}", candidate.display()))?;
    parse_spki_pem(&text)
}

fn parse_spki_pem(text: &str) -> Result<[u8; 32], String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("public key PEM is empty".to_owned());
    }
    if trimmed.contains("PRIVATE KEY") {
        return Err(
            "refusing private key material in LEGION_PREVIEW_UPDATE_KEY; pass only the SPKI public key PEM"
                .to_owned(),
        );
    }

    let mut body = String::new();
    let mut inside = false;
    let mut closed = false;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "-----BEGIN PUBLIC KEY-----" {
            if inside || closed {
                return Err("duplicate or unexpected BEGIN PUBLIC KEY header".to_owned());
            }
            inside = true;
            continue;
        }
        if line == "-----END PUBLIC KEY-----" {
            if !inside {
                return Err("unexpected END PUBLIC KEY footer".to_owned());
            }
            inside = false;
            closed = true;
            continue;
        }
        if !inside {
            return Err(
                "unexpected content outside -----BEGIN PUBLIC KEY----- / -----END PUBLIC KEY-----"
                    .to_owned(),
            );
        }
        body.push_str(line);
    }
    if !closed || body.is_empty() {
        return Err(
            "expected an Ed25519 SPKI public key PEM (-----BEGIN PUBLIC KEY-----)".to_owned(),
        );
    }

    let raw = decode_base64(&body)?;
    if raw.len() != 44 {
        return Err(format!(
            "expected a 44-byte Ed25519 SubjectPublicKeyInfo DER public key, got {} bytes",
            raw.len()
        ));
    }
    if raw[..12] != ED25519_SPKI_PREFIX {
        return Err("SPKI public key does not carry the Ed25519 algorithm OID".to_owned());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw[12..]);

    // Reject all-zero and trivial low-order Ed25519 points.
    if key == [0u8; 32] || is_small_order_point(&key) {
        return Err("public key is not a valid Ed25519 point".to_owned());
    }

    Ok(key)
}

fn is_small_order_point(k: &[u8; 32]) -> bool {
    const LOW_ORDER: [[u8; 32]; 4] = [
        [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ],
        [
            0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0x80,
        ],
        [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0x80,
        ],
    ];
    LOW_ORDER.iter().any(|p| p == k)
}

fn fingerprint(key: &[u8; 32]) -> String {
    let digest = sha256(key);
    format!("SHA256:{}", encode_base64_no_pad(&digest))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn encode_base64_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3f) as usize] as char);
        }
    }
    out
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
