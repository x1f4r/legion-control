//! Embeds the static web interface from `apps/ui` into `legion-doors` at compile time, skipping
//! reference mockups, tests, and documentation, and computes a deterministic build ETag.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    if let Err(err) = generate() {
        eprintln!("legion-doors build.rs: {err}");
        process::exit(1);
    }
}

fn generate() -> Result<(), String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?;
    let out_dir = env::var("OUT_DIR").map_err(|e| e.to_string())?;
    let ui_root = PathBuf::from(&manifest_dir)
        .join("..")
        .join("..")
        .join("apps")
        .join("ui");
    let ui_root = fs::canonicalize(&ui_root)
        .map_err(|e| format!("cannot locate apps/ui at {}: {e}", ui_root.display()))?;

    println!("cargo:rerun-if-changed={}", ui_root.display());

    let mut entries = Vec::new();
    walk(&ui_root, &ui_root, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (web_path, disk_path) in &entries {
        println!("cargo:rerun-if-changed={}", disk_path.display());
        let bytes =
            fs::read(disk_path).map_err(|e| format!("cannot read {}: {e}", disk_path.display()))?;
        hasher.update(web_path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&bytes);
        hasher.update(&[0]);
    }
    let digest = hasher.finalize();
    let etag = format!("\"{}\"", hex_prefix(&digest, 16));

    let out_file = Path::new(&out_dir).join("ui_assets.rs");
    let mut file =
        fs::File::create(&out_file).map_err(|e| format!("cannot create ui_assets.rs: {e}"))?;

    writeln!(file, "pub(crate) const ETAG: &str = {etag:?};").map_err(|e| e.to_string())?;
    writeln!(
        file,
        "pub(crate) fn get(path: &str) -> Option<(&'static [u8], &'static str)> {{"
    )
    .map_err(|e| e.to_string())?;
    writeln!(file, "    match path {{").map_err(|e| e.to_string())?;
    for (web_path, disk_path) in &entries {
        let mime = mime_for(web_path);
        let disk_str = disk_path.to_string_lossy();
        if web_path == "/index.html" {
            writeln!(
                file,
                "        \"/\" | \"/index.html\" => Some((include_bytes!({disk_str:?}), {mime:?})),"
            )
            .map_err(|e| e.to_string())?;
        } else {
            writeln!(
                file,
                "        {web_path:?} => Some((include_bytes!({disk_str:?}), {mime:?})),"
            )
            .map_err(|e| e.to_string())?;
        }
    }
    writeln!(file, "        _ => None,").map_err(|e| e.to_string())?;
    writeln!(file, "    }}").map_err(|e| e.to_string())?;
    writeln!(file, "}}").map_err(|e| e.to_string())?;

    Ok(())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let mut items: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read directory {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    items.sort();

    for path in items {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let meta =
            fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            if matches!(name, "design" | "test" | "node_modules") {
                continue;
            }
            println!("cargo:rerun-if-changed={}", path.display());
            walk(root, &path, out)?;
        } else if meta.is_file() {
            if !is_runtime_asset(name) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("cannot strip prefix: {e}"))?;
            let mut web_path = String::from("/");
            for (idx, comp) in rel.components().enumerate() {
                if idx > 0 {
                    web_path.push('/');
                }
                web_path.push_str(&comp.as_os_str().to_string_lossy());
            }
            out.push((web_path, path));
        }
    }
    Ok(())
}

fn is_runtime_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".html")
        || lower.ends_with(".css")
        || lower.ends_with(".js")
        || lower.ends_with(".svg")
        || lower.ends_with(".png")
        || lower.ends_with(".webmanifest")
}

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".webmanifest") {
        "application/manifest+json; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else {
        "application/octet-stream"
    }
}

fn hex_prefix(bytes: &[u8; 32], count: usize) -> String {
    let mut out = String::with_capacity(count * 2);
    for byte in bytes.iter().take(count) {
        let hi = byte >> 4;
        let lo = byte & 0x0f;
        out.push(hex_nibble(hi));
        out.push(hex_nibble(lo));
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

// Minimal SHA-256 (FIPS 180-4) so build.rs stays free of build-dependencies.
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
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

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(input.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&input[..take]);
            self.buf_len += take;
            input = &input[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while input.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&input[..64]);
            self.compress(&block);
            input = &input[64..];
        }
        if !input.is_empty() {
            self.buf[..input.len()].copy_from_slice(input);
            self.buf_len = input.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        if self.buf_len > 56 {
            let pad = [0u8; 64];
            self.update(&pad[..64 - self.buf_len]);
        }
        if self.buf_len < 56 {
            let pad = [0u8; 56];
            self.update(&pad[..56 - self.buf_len]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
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
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (k, word) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(*word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}
