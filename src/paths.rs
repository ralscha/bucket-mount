//! S3 has a case-sensitive, flat namespace; Windows does not. Never silently alias keys.
use anyhow::{Result, bail, ensure};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 1024,
        "empty or oversized object key"
    );
    ensure!(!key.starts_with('/'), "absolute object key");
    for part in key.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "unsafe path component"
        );
        ensure!(
            part.encode_utf16().count() <= 255,
            "path component too long"
        );
        ensure!(
            !part.ends_with(['.', ' ']),
            "Windows trims trailing dots/spaces"
        );
        ensure!(
            !part.chars().any(|c| c < ' ' || "\\:*?\"<>|".contains(c)),
            "Windows-incompatible object key"
        );
        let stem = part.split('.').next().unwrap_or_default().to_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ),
            "reserved device name"
        );
        if let Some(n) = stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
        {
            ensure!(
                !matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                ),
                "reserved device name"
            );
        }
    }
    Ok(())
}

pub fn local_path(root: &Path, key: &str) -> Result<PathBuf> {
    validate_key(key)?;
    let path = root.join(key.replace('/', "\\"));
    // Reparse points in parents could redirect operations out of the mount.
    let mut parent = path.parent();
    while let Some(p) = parent {
        if p == root {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            ensure!(meta.is_dir(), "object path conflicts with an existing file");
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    ensure!(
                        crate::platform::inspect(p)?.placeholder,
                        "unsupported reparse directory in object path"
                    );
                }
            }
            ensure!(!meta.file_type().is_symlink(), "symlink in object path");
        }
        parent = p.parent();
    }
    Ok(path)
}

/// Reject the entire ambiguous namespace before touching disk, including file/directory aliases.
pub fn validate_namespace<'a>(keys: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut names = BTreeMap::<String, (String, bool)>::new();
    for key in keys {
        validate_key(key)?;
        let parts: Vec<_> = key.split('/').collect();
        for i in 1..=parts.len() {
            let original = parts[..i].join("/");
            let folded = original.to_uppercase();
            let is_file = i == parts.len();
            if let Some((old, old_file)) = names.get(&folded) {
                if old != &original || *old_file != is_file {
                    bail!("Windows namespace collision: {old:?} and {original:?}");
                }
            } else {
                names.insert(folded, (original, is_file));
            }
        }
    }
    Ok(())
}

pub fn conflict_key(key: &str, device: &str) -> String {
    let (parent, name) = key.rsplit_once('/').map_or(("", key), |(p, n)| (p, n));
    let suffix = format!(
        ".conflict-{}-{}",
        device
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(24)
            .collect::<String>(),
        uuid::Uuid::new_v4().simple()
    );
    let (stem, ext) = name
        .rsplit_once('.')
        .filter(|(s, _)| !s.is_empty())
        .map_or((name, String::new()), |(s, e)| (s, format!(".{e}")));
    fn truncate_utf16(text: &str, budget: usize) -> String {
        let mut used = 0;
        text.chars()
            .take_while(|c| {
                used += c.len_utf16();
                used <= budget
            })
            .collect()
    }
    let stem = truncate_utf16(stem, 80);
    // Truncation can expose an interior dot/space as the final character,
    // producing a Windows-invalid name just when a conflict must be saved.
    let ext = truncate_utf16(&ext, 24);
    let ext = ext.trim_end_matches(['.', ' ']);
    let name = format!("{stem}{suffix}{ext}");
    if parent.is_empty() || parent.len() + 1 + name.len() > 1024 {
        name
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_windows_traversal_and_aliases() {
        for key in [
            "../secret",
            "a/../b",
            "/a",
            "a//b",
            "a\\b",
            "a:b",
            "a.",
            "NUL.txt",
            "com1",
            "LPT².foo",
            "a/ ",
            "",
        ] {
            assert!(validate_key(key).is_err(), "{key}");
        }
        assert!(validate_key("dir/normal unicode é.txt").is_ok());
    }
    #[test]
    fn rejects_case_and_directory_collisions() {
        assert!(validate_namespace(["a/File", "a/file"]).is_err());
        assert!(validate_namespace(["a", "a/b"]).is_err());
        assert!(validate_namespace(["a/b", "A/c"]).is_err());
        assert!(validate_namespace(["a/b", "a/c"]).is_ok());
    }
    #[test]
    fn conflict_names_are_unique_and_valid() {
        let a = conflict_key("dir/report.docx", "desktop");
        assert!(a.starts_with("dir/report.conflict-desktop-"));
        assert!(a.ends_with(".docx"));
        validate_key(&a).unwrap();
        assert_ne!(a, conflict_key("dir/report.docx", "desktop"));
        let long_name = format!("{}.{}", "😀".repeat(90), "😀".repeat(30));
        validate_key(&conflict_key(&long_name, &"x".repeat(24))).unwrap();
        for extension in [
            format!("{} tail", "x".repeat(22)),
            format!("{}😀", "x".repeat(22)),
        ] {
            let original = format!("report.{extension}");
            validate_key(&original).unwrap();
            validate_key(&conflict_key(&original, "desktop")).unwrap();
        }
    }
}
