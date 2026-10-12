use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, ensure, Context, Result};
use sha1::{Digest, Sha1};

pub const MAGIC: u32 = 0x5A6F12E1;
const LOCALE_PREFIX: &str = "Client/Content/Aki/ConfigDB/en/";
const MOUNT_POINT: &str = "../../../";
const FNV_OFFSET: u64 = 0xCBF29CE484222325;
const FNV_PRIME: u64 = 0x100000001B3;
const FOOTER_PADDING: usize = 160;
const FOOTER_LEAD: usize = 17;
const FOOTER_VERSION: u32 = 12;
const ENTRY_FLAGS: u32 = 0xE0000000;

pub fn path_hash(path: &str) -> u64 {
    path.to_lowercase()
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .fold(FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
        })
}

fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("Klasör okunamadı: {}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

struct Output {
    writer: BufWriter<File>,
    position: u64,
}

impl Output {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.position += bytes.len() as u64;
        Ok(())
    }
}

pub fn pack(source: &Path, destination: &Path, cancelled: Option<&AtomicBool>) -> Result<PathBuf> {
    let mut found = Vec::new();
    collect_files(source, &mut found)?;
    let mut files = Vec::with_capacity(found.len());
    for path in found {
        let relative = path.strip_prefix(source)?;
        let parts: Vec<String> = relative
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .map(str::to_owned)
                    .context("Geçersiz dosya adı")
            })
            .collect::<Result<_>>()?;
        files.push((parts, path));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.is_empty() {
        bail!("No localization databases to pack");
    }
    for (parts, path) in &files {
        let relative = parts.join("/");
        ensure!(
            relative.starts_with(LOCALE_PREFIX) && path.extension().is_some_and(|ext| ext == "db"),
            "Only English localization databases may be packed"
        );
    }

    let mut out = Output {
        writer: BufWriter::new(File::create(destination)?),
        position: 0,
    };
    let mut entries: Vec<(String, u32, u32)> = Vec::with_capacity(files.len());
    for (parts, path) in &files {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            bail!("İşlem iptal edildi.");
        }
        let data = fs::read(path)?;
        let offset = out.position;
        let size = data.len() as u64;
        ensure!(
            offset.max(size) <= u64::from(u32::MAX),
            "V12 localization writer supports archives smaller than 4 GiB"
        );
        let mut header = Vec::with_capacity(53);
        header.extend_from_slice(&0u64.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&sha1(&data));
        header.push(0);
        header.extend_from_slice(&0u32.to_le_bytes());
        out.write(&header)?;
        out.write(&data)?;
        entries.push((parts.join("/"), offset as u32, size as u32));
    }

    let index_offset = out.position;
    let mut encoded: Vec<u8> = Vec::new();
    let mut hashes: Vec<u8> = (entries.len() as u32).to_le_bytes().to_vec();
    let mut directories: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    directories.entry("/".to_owned()).or_default();
    for (relative, offset, size) in &entries {
        let entry_offset = encoded.len() as u32;
        encoded.extend_from_slice(&ENTRY_FLAGS.to_le_bytes());
        encoded.push(0);
        encoded.extend_from_slice(&size.to_le_bytes());
        encoded.extend_from_slice(&offset.to_le_bytes());
        hashes.extend_from_slice(&path_hash(relative).to_le_bytes());
        hashes.extend_from_slice(&entry_offset.to_le_bytes());
        let (parent, name) = relative
            .rsplit_once('/')
            .context("Yol bir klasör içermeli")?;
        let mut parent = parent;
        directories
            .entry(format!("{parent}/"))
            .or_default()
            .insert(name.to_owned(), entry_offset);
        while let Some((head, _)) = parent.rsplit_once('/') {
            parent = head;
            directories.entry(format!("{parent}/")).or_default();
        }
    }
    hashes.extend_from_slice(&0u32.to_le_bytes());

    let mut directory: Vec<u8> = (directories.len() as u32).to_le_bytes().to_vec();
    for (name, files) in &directories {
        push_string(&mut directory, name);
        directory.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (filename, offset) in files {
            push_string(&mut directory, filename);
            directory.extend_from_slice(&offset.to_le_bytes());
        }
    }

    let mut primary: Vec<u8> = Vec::new();
    push_string(&mut primary, MOUNT_POINT);
    let primary_size = primary.len() + 4 + 8 + 2 * (4 + 8 + 8 + 20) + 4 + encoded.len() + 4;
    let hash_offset = index_offset + primary_size as u64;
    let dir_offset = hash_offset + hashes.len() as u64;
    primary.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    primary.extend_from_slice(&0u64.to_le_bytes());
    primary.extend_from_slice(&1u32.to_le_bytes());
    primary.extend_from_slice(&hash_offset.to_le_bytes());
    primary.extend_from_slice(&(hashes.len() as u64).to_le_bytes());
    primary.extend_from_slice(&sha1(&hashes));
    primary.extend_from_slice(&1u32.to_le_bytes());
    primary.extend_from_slice(&dir_offset.to_le_bytes());
    primary.extend_from_slice(&(directory.len() as u64).to_le_bytes());
    primary.extend_from_slice(&sha1(&directory));
    primary.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    primary.extend_from_slice(&encoded);
    primary.extend_from_slice(&0u32.to_le_bytes());
    ensure!(primary.len() == primary_size, "Primary index size mismatch");

    out.write(&primary)?;
    out.write(&hashes)?;
    out.write(&directory)?;

    let mut footer = vec![0u8; FOOTER_LEAD];
    footer.extend_from_slice(&MAGIC.to_le_bytes());
    footer.extend_from_slice(&FOOTER_VERSION.to_le_bytes());
    footer.extend_from_slice(&index_offset.to_le_bytes());
    footer.extend_from_slice(&(primary.len() as u64).to_le_bytes());
    footer.extend_from_slice(&sha1(&primary));
    footer.extend(std::iter::repeat_n(0u8, FOOTER_PADDING));
    out.write(&footer)?;
    out.writer.flush()?;
    Ok(destination.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_SHA1: &str = "B825A637EE0258C7FFEDD87C576588DB96A2AA29";
    const GOLDEN_LEN: u64 = 1160;

    fn fixture(root: &Path) {
        let base = root.join("Client/Content/Aki/ConfigDB/en");
        fs::create_dir_all(base.join("nested")).unwrap();
        let multi: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(base.join("lang_multi_text.db"), multi).unwrap();
        fs::write(base.join("lang_b.db"), b"abc").unwrap();
        fs::write(base.join("nested/lang_c.db"), b"nested-data").unwrap();
    }

    fn sha1_hex(data: &[u8]) -> String {
        sha1(data).iter().map(|b| format!("{b:02X}")).collect()
    }

    #[test]
    fn path_hash_matches_python() {
        assert_eq!(
            path_hash("Client/Content/Aki/ConfigDB/en/lang_b.db"),
            PATH_HASH_B
        );
        assert_eq!(path_hash("CLIENT/x"), path_hash("client/X"));
    }

    const PATH_HASH_B: u64 = 14394382356238260919;

    #[test]
    fn pack_matches_pinned_python_output() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let out = dir.path().join("out.pak");
        pack(dir.path(), &out, None).unwrap();
        let bytes = fs::read(&out).unwrap();
        assert_eq!(bytes.len() as u64, GOLDEN_LEN);
        assert_eq!(sha1_hex(&bytes), GOLDEN_SHA1);
    }

    #[test]
    fn pack_rejects_empty_and_foreign_files() {
        let empty = tempfile::tempdir().unwrap();
        assert!(pack(empty.path(), &empty.path().join("o.pak"), None).is_err());
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Other")).unwrap();
        fs::write(dir.path().join("Other/x.db"), b"x").unwrap();
        assert!(pack(dir.path(), &dir.path().join("o.pak"), None).is_err());
    }
}
