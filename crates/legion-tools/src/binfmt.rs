//! Executable header inspection (ELF, Mach-O, PE) for pre-install target verification.
//!
//! Checking a candidate binary's format and CPU architecture before copying or renaming it over a
//! live `legion` binary prevents breaking a remote node with a wrong-OS or wrong-architecture
//! artifact (especially when installing from a local path via `from`).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use legion_core::proto::{ErrorBody, ErrorCode};

/// Enough leading bytes to cover ELF headers, Mach-O/fat headers, and PE DOS + COFF headers.
const HEADER_READ_BYTES: usize = 4096;

/// Coarse executable format and CPU architecture extracted from binary headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinaryKind {
    /// Linux/Unix ELF binary with its detected architecture (`x86_64`, `aarch64`, etc.).
    Elf(&'static str),
    /// Apple Mach-O single-architecture or universal (fat) binary with its detected architectures.
    MachO(Vec<&'static str>),
    /// Windows PE/COFF executable with its detected architecture (`x86_64`, `aarch64`, etc.).
    Pe(&'static str),
}

impl BinaryKind {
    fn describe(&self) -> String {
        match self {
            BinaryKind::Elf(arch) => format!("ELF ({arch})"),
            BinaryKind::MachO(arches) => format!("Mach-O ({})", arches.join(", ")),
            BinaryKind::Pe(arch) => format!("PE ({arch})"),
        }
    }
}

/// Reads the header of `path` and verifies its executable format and architecture match `target`.
pub fn verify_binary_matches_target(path: &Path, target: &str) -> Result<(), ErrorBody> {
    let mut file = File::open(path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not open {} to check binary format: {err}.",
                path.display()
            ),
        )
    })?;
    let mut buf = vec![0u8; HEADER_READ_BYTES];
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Could not read {} to check binary format: {err}.",
                    path.display()
                ),
            )
        })?;
        if n == 0 {
            break;
        }
        total += n;
    }
    buf.truncate(total);
    verify_header_matches_target(&buf, target)
}

/// Pure header check verifying that `bytes` represent an executable matching `target`.
pub fn verify_header_matches_target(bytes: &[u8], target: &str) -> Result<(), ErrorBody> {
    let expected = expected_for_target(target)?;
    let actual = inspect_header(bytes).ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Candidate binary is not a recognized ELF, Mach-O, or PE executable (expected {}).",
                expected.describe()
            ),
        )
        .with_hint("Provide a compiled legion executable matching the target machine's OS and architecture.")
    })?;

    let matches = match (&expected, &actual) {
        (Expected::Elf(want), BinaryKind::Elf(got)) => want == got,
        (Expected::MachO(want), BinaryKind::MachO(got)) => got.contains(want),
        (Expected::Pe(want), BinaryKind::Pe(got)) => want == got,
        _ => false,
    };

    if !matches {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Binary format mismatch for target {target}: file is {}, expected {}.",
                actual.describe(),
                expected.describe()
            ),
        )
        .with_hint("Use a legion binary compiled for the target machine's OS and architecture; nothing was replaced."));
    }

    Ok(())
}

/// Inspects the leading bytes of a binary and returns its detected [`BinaryKind`], if recognized.
pub fn inspect_header(bytes: &[u8]) -> Option<BinaryKind> {
    if bytes.len() >= 20 && &bytes[..4] == b"\x7fELF" {
        return parse_elf(bytes);
    }
    if bytes.len() >= 8 {
        match &bytes[..4] {
            // 64-bit Mach-O little-endian (MH_MAGIC_64)
            [0xCF, 0xFA, 0xED, 0xFE] => {
                let cputype = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                return Some(BinaryKind::MachO(vec![macho_cputype_name(cputype)]));
            }
            // 64-bit Mach-O big-endian (MH_CIGAM_64)
            [0xFE, 0xED, 0xFA, 0xCF] => {
                let cputype = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                return Some(BinaryKind::MachO(vec![macho_cputype_name(cputype)]));
            }
            // 32-bit Mach-O
            [0xCE, 0xFA, 0xED, 0xFE] | [0xFE, 0xED, 0xFA, 0xCE] => {
                return Some(BinaryKind::MachO(vec!["32-bit"]));
            }
            // Universal (fat) Mach-O (FAT_MAGIC, big-endian header)
            [0xCA, 0xFE, 0xBA, 0xBE] => {
                return parse_fat_macho(bytes);
            }
            _ => {}
        }
    }
    if bytes.len() >= 0x40 && &bytes[..2] == b"MZ" {
        return parse_pe(bytes);
    }
    None
}

enum Expected {
    Elf(&'static str),
    MachO(&'static str),
    Pe(&'static str),
}

impl Expected {
    fn describe(&self) -> String {
        match self {
            Expected::Elf(arch) => format!("ELF ({arch})"),
            Expected::MachO(arch) => format!("Mach-O ({arch})"),
            Expected::Pe(arch) => format!("PE ({arch})"),
        }
    }
}

fn expected_for_target(target: &str) -> Result<Expected, ErrorBody> {
    if target.contains("linux") {
        if target.starts_with("x86_64-") {
            return Ok(Expected::Elf("x86_64"));
        }
        if target.starts_with("aarch64-") {
            return Ok(Expected::Elf("aarch64"));
        }
    } else if target.contains("apple-darwin") {
        if target.starts_with("aarch64-") {
            return Ok(Expected::MachO("aarch64"));
        }
        if target.starts_with("x86_64-") {
            return Ok(Expected::MachO("x86_64"));
        }
    } else if target.contains("windows") {
        if target.starts_with("x86_64-") {
            return Ok(Expected::Pe("x86_64"));
        }
        if target.starts_with("aarch64-") {
            return Ok(Expected::Pe("aarch64"));
        }
    }
    Err(ErrorBody::new(
        ErrorCode::Unsupported,
        format!("Cannot validate executable format for unsupported target {target:?}."),
    ))
}

fn parse_elf(bytes: &[u8]) -> Option<BinaryKind> {
    let class = bytes[4];
    let data = bytes[5];
    let machine = match data {
        1 => u16::from_le_bytes([bytes[18], bytes[19]]),
        2 => u16::from_be_bytes([bytes[18], bytes[19]]),
        _ => return None,
    };
    let arch = match (class, machine) {
        (2, 62) => "x86_64",
        (2, 183) => "aarch64",
        (1, 40) => "arm32",
        (1, 3) => "x86",
        (2, 243) => "riscv64",
        _ => "unknown",
    };
    Some(BinaryKind::Elf(arch))
}

fn macho_cputype_name(cputype: u32) -> &'static str {
    match cputype {
        0x0100_0007 => "x86_64",
        0x0100_000C => "aarch64",
        0x0000_0007 => "x86",
        0x0000_000C => "arm32",
        _ => "unknown",
    }
}

fn parse_fat_macho(bytes: &[u8]) -> Option<BinaryKind> {
    let count = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    // Java class files also start with 0xCAFEBABE, where bytes 4..8 are minor/major version
    // (typically major >= 45). Real Mach-O fat binaries hold a small number of arch slices.
    if count == 0 || count > 16 {
        return None;
    }
    let needed = 8usize.checked_add(count.checked_mul(20)?)?;
    if bytes.len() < needed {
        return None;
    }
    let mut arches = Vec::with_capacity(count);
    for idx in 0..count {
        let offset = 8 + idx * 20;
        let cputype = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        arches.push(macho_cputype_name(cputype));
    }
    Some(BinaryKind::MachO(arches))
}

fn parse_pe(bytes: &[u8]) -> Option<BinaryKind> {
    let pe_offset =
        u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
    let end = pe_offset.checked_add(6)?;
    if bytes.len() < end || &bytes[pe_offset..pe_offset + 4] != b"PE\0\0" {
        return None;
    }
    let machine = u16::from_le_bytes([bytes[pe_offset + 4], bytes[pe_offset + 5]]);
    let arch = match machine {
        0x8664 => "x86_64",
        0xAA64 => "aarch64",
        0x014C => "x86",
        _ => "unknown",
    };
    Some(BinaryKind::Pe(arch))
}

/// Builds a minimal valid ELF/Mach-O/PE header matching `target`, for offline tests.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn synthetic_header_for_target(target: &str) -> Vec<u8> {
    if target.contains("linux") {
        let mut buf = vec![0u8; 64];
        buf[..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // 64-bit
        buf[5] = 1; // little-endian
        let machine: u16 = if target.starts_with("aarch64-") {
            183
        } else {
            62
        };
        buf[18..20].copy_from_slice(&machine.to_le_bytes());
        buf
    } else if target.contains("apple-darwin") {
        let mut buf = vec![0u8; 32];
        buf[..4].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE]);
        let cputype: u32 = if target.starts_with("aarch64-") {
            0x0100_000C
        } else {
            0x0100_0007
        };
        buf[4..8].copy_from_slice(&cputype.to_le_bytes());
        buf
    } else {
        let mut buf = vec![0u8; 0x90];
        buf[..2].copy_from_slice(b"MZ");
        buf[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        buf[0x80..0x84].copy_from_slice(b"PE\0\0");
        let machine: u16 = if target.starts_with("aarch64-") {
            0xAA64
        } else {
            0x8664
        };
        buf[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_elf_macho_fat_and_pe_headers_and_refuses_mismatches() {
        let elf_x64 = synthetic_header_for_target("x86_64-unknown-linux-musl");
        let elf_arm64 = synthetic_header_for_target("aarch64-unknown-linux-musl");
        let macho_arm64 = synthetic_header_for_target("aarch64-apple-darwin");
        let macho_x64 = synthetic_header_for_target("x86_64-apple-darwin");
        let pe_x64 = synthetic_header_for_target("x86_64-pc-windows-msvc");
        let pe_arm64 = synthetic_header_for_target("aarch64-pc-windows-msvc");

        verify_header_matches_target(&elf_x64, "x86_64-unknown-linux-musl").unwrap();
        verify_header_matches_target(&elf_arm64, "aarch64-unknown-linux-musl").unwrap();
        verify_header_matches_target(&macho_arm64, "aarch64-apple-darwin").unwrap();
        verify_header_matches_target(&macho_x64, "x86_64-apple-darwin").unwrap();
        verify_header_matches_target(&pe_x64, "x86_64-pc-windows-msvc").unwrap();
        verify_header_matches_target(&pe_arm64, "aarch64-pc-windows-msvc").unwrap();

        // Universal Mach-O containing both x86_64 and arm64 slices matches both macOS targets.
        let mut fat = vec![0u8; 48];
        fat[..4].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        fat[4..8].copy_from_slice(&2u32.to_be_bytes());
        fat[8..12].copy_from_slice(&0x0100_0007u32.to_be_bytes());
        fat[28..32].copy_from_slice(&0x0100_000Cu32.to_be_bytes());
        verify_header_matches_target(&fat, "x86_64-apple-darwin").unwrap();
        verify_header_matches_target(&fat, "aarch64-apple-darwin").unwrap();

        // Wrong architecture is refused.
        let wrong_arch =
            verify_header_matches_target(&elf_arm64, "x86_64-unknown-linux-musl").unwrap_err();
        assert_eq!(wrong_arch.code, ErrorCode::InvalidArgs);
        assert!(
            wrong_arch.message.contains("aarch64"),
            "{}",
            wrong_arch.message
        );

        // Wrong OS format is refused.
        let wrong_os =
            verify_header_matches_target(&pe_x64, "x86_64-unknown-linux-musl").unwrap_err();
        assert_eq!(wrong_os.code, ErrorCode::InvalidArgs);
        assert!(wrong_os.message.contains("PE"), "{}", wrong_os.message);

        // Truncated or plain text input is refused.
        assert_eq!(
            verify_header_matches_target(b"not a binary", "x86_64-unknown-linux-musl")
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );
    }
}
