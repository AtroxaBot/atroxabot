//! Unpacking a `.tar.gz`, the way GitHub serves a repository. Written by
//! hand for archives from anywhere: only folders and plain files are made,
//! never links or devices, nothing lands outside the destination, and every
//! size is bounded.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Most bytes an archive may unpack to.
const MAX_TOTAL: u64 = 512 << 20;
/// Largest single file.
const MAX_FILE: u64 = 100 << 20;
/// Most entries.
const MAX_ENTRIES: usize = 50_000;
/// Largest extended header (a long name, pax records).
const MAX_HEADER_DATA: u64 = 64 << 10;
/// Longest path kept, in bytes.
const MAX_PATH: usize = 400;
/// A tar block.
const BLOCK: usize = 512;

/// Unpacks the gzip-compressed tar `input` into the folder `dest` (which
/// must exist), dropping the archive's top folder (GitHub's `repo-commit/`).
/// Links, devices and paths that would leave `dest` or that some system
/// cannot hold are skipped.
///
/// # Errors
/// The input is not a gzip-compressed tar, is cut short or too large, or a
/// file cannot be written.
pub(super) fn unpack(input: impl Read, dest: &Path) -> Result<(), String> {
    let mut reader = flate2::read::GzDecoder::new(input).take(MAX_TOTAL);
    let mut header = [0u8; BLOCK];
    // What an extended header says about the entry after it.
    let (mut long_name, mut pax_path, mut pax_size) = (None::<String>, None::<String>, None::<u64>);
    for _ in 0..MAX_ENTRIES {
        if !read_block(&mut reader, &mut header)? || header.iter().all(|&byte| byte == 0) {
            return Ok(());
        }
        if !checksum_matches(&header) {
            return Err("the download is not a tar archive".to_owned());
        }
        let own = number(&header[124..136]).ok_or("the archive has a damaged entry")?;
        match header[156] {
            // A pax header: records for the next entry.
            b'x' => {
                let data = read_small(&mut reader, own)?;
                for (key, value) in pax_records(&data) {
                    match key {
                        "path" => pax_path = Some(value.to_owned()),
                        "size" => pax_size = value.parse().ok(),
                        _ => {}
                    }
                }
                continue;
            }
            // GNU's long name for the next entry.
            b'L' => {
                let data = read_small(&mut reader, own)?;
                long_name = Some(String::from_utf8_lossy(&data).trim_end_matches('\0').to_owned());
                continue;
            }
            // Global records, GNU long link names: nothing this needs.
            b'g' | b'K' => {
                skip(&mut reader, padded(own))?;
                continue;
            }
            _ => {}
        }
        let size = pax_size.take().unwrap_or(own);
        let name = pax_path.take().or_else(|| long_name.take()).unwrap_or_else(|| ustar_name(&header));
        let target = inside(&name).map(|relative| dest.join(relative));
        match (header[156], target) {
            (b'0' | b'\0' | b'7', Some(target)) => {
                if size > MAX_FILE {
                    return Err(format!("{name} is too large"));
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("cannot unpack {name}: {e}"))?;
                }
                let mut file = fs::File::create(&target).map_err(|e| format!("cannot unpack {name}: {e}"))?;
                let copied = io::copy(&mut (&mut reader).take(size), &mut file)
                    .map_err(|e| format!("cannot unpack {name}: {e}"))?;
                if copied < size {
                    return Err("the archive is cut short or too large".to_owned());
                }
                skip(&mut reader, padded(size) - size)?;
            }
            (b'5', Some(target)) => {
                fs::create_dir_all(&target).map_err(|e| format!("cannot unpack {name}: {e}"))?;
                skip(&mut reader, padded(size))?;
            }
            // Links, devices, and paths that cannot be kept.
            _ => skip(&mut reader, padded(size))?,
        }
    }
    Err("the archive has too many entries".to_owned())
}

/// Reads one block; `false` at the end of the input.
fn read_block(reader: &mut impl Read, block: &mut [u8; BLOCK]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < BLOCK {
        match reader.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("the archive is cut short or too large".to_owned()),
            Ok(read) => filled += read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("cannot read the archive: {e}")),
        }
    }
    Ok(true)
}

/// Reads an extended header's data, and the padding after it.
fn read_small(reader: &mut impl Read, size: u64) -> Result<Vec<u8>, String> {
    if size > MAX_HEADER_DATA {
        return Err("the archive has an oversized header".to_owned());
    }
    let mut data = Vec::new();
    (&mut *reader).take(size).read_to_end(&mut data).map_err(|e| format!("cannot read the archive: {e}"))?;
    if (data.len() as u64) < size {
        return Err("the archive is cut short".to_owned());
    }
    skip(reader, padded(size) - size)?;
    Ok(data)
}

/// Reads past `count` bytes.
fn skip(reader: &mut impl Read, count: u64) -> Result<(), String> {
    let skipped =
        io::copy(&mut reader.take(count), &mut io::sink()).map_err(|e| format!("cannot read the archive: {e}"))?;
    if skipped < count { Err("the archive is cut short or too large".to_owned()) } else { Ok(()) }
}

/// `size` rounded up to whole blocks.
fn padded(size: u64) -> u64 {
    size.div_ceil(BLOCK as u64) * BLOCK as u64
}

/// Whether the header's checksum field matches its bytes (the field itself counted as spaces).
fn checksum_matches(header: &[u8; BLOCK]) -> bool {
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(at, &byte)| if (148..156).contains(&at) { u64::from(b' ') } else { u64::from(byte) })
        .sum();
    number(&header[148..156]) == Some(sum)
}

/// A numeric field: octal text, or base-256 when its first bit is set.
fn number(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|&byte| byte & 0x80 != 0) {
        let mut value: u64 = u64::from(field[0] & 0x7f);
        for &byte in &field[1..] {
            value = value.checked_mul(256)?.checked_add(u64::from(byte))?;
        }
        return Some(value);
    }
    let text = std::str::from_utf8(field).ok()?.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() { Some(0) } else { u64::from_str_radix(text, 8).ok() }
}

/// The name a ustar header gives: its prefix, a slash, and its name.
fn ustar_name(header: &[u8; BLOCK]) -> String {
    let field = |range: std::ops::Range<usize>| {
        let bytes = &header[range];
        let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let (name, prefix) = (field(0..100), if &header[257..262] == b"ustar" { field(345..500) } else { String::new() });
    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
}

/// The `key=value` records of a pax header (`<length> key=value\n` each).
fn pax_records(data: &[u8]) -> Vec<(&str, &str)> {
    let mut records = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some(space) = rest.iter().position(|&byte| byte == b' ') else { break };
        let Some(length) = std::str::from_utf8(&rest[..space]).ok().and_then(|text| text.parse::<usize>().ok()) else {
            break;
        };
        if length <= space + 1 || length > rest.len() {
            break;
        }
        if let Ok(record) = std::str::from_utf8(&rest[space + 1..length])
            && let Some((key, value)) = record.trim_end_matches('\n').split_once('=')
        {
            records.push((key, value));
        }
        rest = &rest[length..];
    }
    records
}

/// Where the archive path `name` goes, relative to the destination: its
/// top folder dropped, and `None` when it would leave the destination or
/// holds a name some system cannot store.
pub(super) fn inside(name: &str) -> Option<PathBuf> {
    if name.len() > MAX_PATH {
        return None;
    }
    let mut parts = name.split('/').filter(|part| !part.is_empty() && *part != ".");
    parts.next()?;
    let mut path = PathBuf::new();
    for part in parts {
        if !portable(part) {
            return None;
        }
        path.push(part);
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

/// Whether `part` is a file name every system can hold: no `..`, no
/// characters Windows refuses, no trailing dot or space, no device name.
pub(super) fn portable(part: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1",
        "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = part.split('.').next().unwrap_or_default().to_ascii_lowercase();
    !part.is_empty()
        && part != ".."
        && !part.ends_with(['.', ' '])
        && !part.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        && !DEVICES.contains(&stem.as_str())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A tar entry's header: `kind` is the type flag.
    fn header(name: &str, size: usize, kind: u8) -> [u8; BLOCK] {
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        header
    }

    /// A gzip-compressed tar of `(name, kind, data)` entries.
    fn archive(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, kind, data) in entries {
            tar.extend_from_slice(&header(name, data.len(), *kind));
            tar.extend_from_slice(data);
            tar.resize(tar.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        tar.extend_from_slice(&[0u8; BLOCK * 2]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    /// A pax record, its length counting itself.
    fn record(key: &str, value: &str) -> Vec<u8> {
        let body = format!(
            " {key}={value}
"
        );
        let mut length = body.len() + 1;
        while length.to_string().len() + body.len() != length {
            length += 1;
        }
        format!("{length}{body}").into_bytes()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atroxabot-archive-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unpacks_files_without_the_top_folder() {
        let dir = temp("plain");
        let (escape, pax) = (record("path", "../../evil"), record("path", "repo-1/long/pax/name.md"));
        let bytes = archive(&[
            ("repo-1/", b'5', b""),
            ("repo-1/skills/pdf/SKILL.md", b'0', b"---\nname: pdf\n---\n"),
            ("repo-1/escape", b'x', &escape),
            ("repo-1/ignored", b'0', b"evil"),
            ("repo-1/link", b'2', b""),
            ("repo-1/x", b'x', &pax),
            ("repo-1/short", b'0', b"pax"),
            ("repo-1/CON.txt", b'0', b"device"),
        ]);
        unpack(bytes.as_slice(), &dir).unwrap();
        assert_eq!(fs::read_to_string(dir.join("skills/pdf/SKILL.md")).unwrap(), "---\nname: pdf\n---\n");
        assert_eq!(fs::read_to_string(dir.join("long/pax/name.md")).unwrap(), "pax", "a pax path names the entry");
        assert!(!dir.join("link").exists() && !dir.join("CON.txt").exists());
        assert!(!dir.join("ignored").exists() && !dir.parent().unwrap().join("evil").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_damaged_and_short_archives() {
        let dir = temp("damaged");
        let mut bytes = archive(&[("repo/a", b'0', b"hello")]);
        assert!(unpack(&b"not gzip"[..], &dir).is_err());
        bytes.truncate(bytes.len() / 2);
        assert!(unpack(bytes.as_slice(), &dir).is_err(), "cut short");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&[7u8; BLOCK]).unwrap();
        assert!(unpack(gz.finish().unwrap().as_slice(), &dir).is_err(), "a block that is no header");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paths_stay_inside_and_portable() {
        assert_eq!(inside("top/a/b.md"), Some(PathBuf::from("a").join("b.md")));
        assert_eq!(inside("/top/./a"), Some(PathBuf::from("a")));
        assert_eq!(inside("top"), None);
        for bad in ["top/../x", "top/a/..", "top/c:x", "top/a\\b", "top/aux", "top/trail.", "top/nul.txt"] {
            assert_eq!(inside(bad), None, "{bad}");
        }
        assert_eq!(number(b"0000017\0"), Some(15));
        assert_eq!(number(&[0x80, 0, 0, 1, 0]), Some(256));
        assert_eq!(pax_records(b"12 path=abc\n9 size=7\n"), [("path", "abc"), ("size", "7")]);
        assert!(pax_records(b"999 path=x\n").is_empty(), "a length past the end");
    }
}
