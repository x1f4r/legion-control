//! Records the source revision for `legion --version`, `meta.version`, and `hello`.
//!
//! Prefers `LEGION_BUILD_REV` when set (used by release builds), otherwise queries git for the
//! 12-character commit hash and whether tracked files are modified, and omits the revision cleanly
//! outside a git checkout (such as a source tarball). Never fails the build.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LEGION_BUILD_REV");

    if let Ok(raw) = env::var("LEGION_BUILD_REV") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let rev = normalize_env_rev(trimmed);
            println!("cargo:rustc-env=LEGION_BUILD_ID={rev}");
            return;
        }
    }

    let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(repo_root) = find_repo_root(&manifest_dir) else {
        return;
    };

    emit_git_rerun_hints(&repo_root);

    let Some(rev) = git_short_head(&repo_root) else {
        return;
    };
    let build = if git_tracked_dirty(&repo_root) {
        format!("{rev}, modified")
    } else {
        rev
    };
    println!("cargo:rustc-env=LEGION_BUILD_ID={build}");
}

fn normalize_env_rev(raw: &str) -> String {
    if raw.len() > 12 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        raw[..12].to_ascii_lowercase()
    } else {
        raw.to_owned()
    }
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

fn emit_git_rerun_hints(repo_root: &Path) {
    let dot_git = repo_root.join(".git");
    let (git_dir, common_dir) = if dot_git.is_dir() {
        (dot_git.clone(), dot_git)
    } else if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
        let Ok(content) = fs::read_to_string(&dot_git) else {
            return;
        };
        let Some(raw_gitdir) = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let git_dir = resolve_relative(repo_root, raw_gitdir);
        let common_dir = fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map_or_else(|| git_dir.clone(), |rel| resolve_relative(&git_dir, &rel));
        (git_dir, common_dir)
    } else {
        return;
    };

    let head_path = git_dir.join("HEAD");
    if head_path.exists() {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }

    if let Ok(head_contents) = fs::read_to_string(&head_path)
        && let Some(ref_name) = head_contents.trim().strip_prefix("ref:")
    {
        let ref_name = ref_name.trim();
        if !ref_name.is_empty() {
            let local_ref = git_dir.join(ref_name);
            let common_ref = common_dir.join(ref_name);
            if local_ref.exists() {
                println!("cargo:rerun-if-changed={}", local_ref.display());
            } else if common_ref.exists() {
                println!("cargo:rerun-if-changed={}", common_ref.display());
            }
            let packed_refs = common_dir.join("packed-refs");
            if packed_refs.exists() {
                println!("cargo:rerun-if-changed={}", packed_refs.display());
            }
        }
    }
}

fn resolve_relative(base: &Path, target: &str) -> PathBuf {
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn git_short_head(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rev = String::from_utf8(output.stdout).ok()?;
    let trimmed = rev.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn git_tracked_dirty(repo_root: &Path) -> bool {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}
