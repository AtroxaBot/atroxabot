//! Embeds the app icon and its version in the Windows executable, so
//! Explorer, pinned taskbar entries and the window itself
//! (`Icon::from_resource`) show the icon, and the file's properties and the
//! Task Manager say "AtroxaBot" and which version it is.
//!
//! Both are written as a compiled resource file (`.res`) by hand, which
//! MSVC's linker takes directly: an `RT_GROUP_ICON` directory with id 1 that
//! points at one PNG-compressed `RT_ICON` per size in `assets/icon/`, and an
//! `RT_VERSION` with the version in `Cargo.toml`.

use std::path::PathBuf;
use std::{env, fs};

/// The pre-rendered sizes in `assets/icon/<size>.png`.
const SIZES: [u16; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// `RT_ICON`.
const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON`.
const RT_GROUP_ICON: u16 = 14;
/// `RT_VERSION`.
const RT_VERSION: u16 = 16;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        // ponytail: MSVC only; the GNU linker needs the .res turned into a COFF object (windres) first.
        return;
    }
    // The empty entry every .res file starts with.
    let mut res = Vec::new();
    entry(&mut res, 0, 0, &[]);
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    for (id, size) in (1..).zip(SIZES) {
        let png = fs::read(format!("assets/icon/{size}.png")).expect("the icon PNGs are checked in");
        entry(&mut res, RT_ICON, id, &png);
        // GRPICONDIRENTRY: 256 is written as 0.
        let side = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[side, side, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, &group);
    entry(&mut res, RT_VERSION, 1, &version_info());
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("icon.res");
    fs::write(&out, res).expect("OUT_DIR is writable");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// The version resource (`VS_VERSIONINFO`): the numbers of the package's
/// version, and the texts Explorer and the Task Manager show.
fn version_info() -> Vec<u8> {
    let number = |name: &str| -> u32 {
        env::var(name).ok().and_then(|value| value.parse().ok()).expect("cargo sets the version's numbers")
    };
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets the version");
    let (major, minor, patch) =
        (number("CARGO_PKG_VERSION_MAJOR"), number("CARGO_PKG_VERSION_MINOR"), number("CARGO_PKG_VERSION_PATCH"));
    // VS_FIXEDFILEINFO: signature, structure version, file and product version (each as two
    // 16-bit pairs), flags mask and flags, VOS_NT_WINDOWS32, VFT_APP, subtype, date.
    let pairs = [(major << 16) | (minor & 0xFFFF), (patch & 0xFFFF) << 16];
    let fixed = [0xFEEF_04BD, 0x0001_0000, pairs[0], pairs[1], pairs[0], pairs[1], 0x3F, 0, 0x0004_0004, 1, 0, 0, 0];
    let fixed: Vec<u8> = fixed.into_iter().flat_map(u32::to_le_bytes).collect();
    let texts = [
        ("CompanyName", "AtroxaBot"),
        ("FileDescription", "AtroxaBot"),
        ("FileVersion", version.as_str()),
        ("InternalName", "atroxabot"),
        ("OriginalFilename", "atroxabot.exe"),
        ("ProductName", "AtroxaBot"),
        ("ProductVersion", version.as_str()),
    ];
    let strings: Vec<Vec<u8>> = texts
        .into_iter()
        .map(|(name, text)| {
            let value: Vec<u16> = text.encode_utf16().chain([0]).collect();
            let bytes: Vec<u8> = value.iter().flat_map(|unit| unit.to_le_bytes()).collect();
            block(name, 1, &bytes, value.len() as u16, &[])
        })
        .collect();
    // US English, Unicode: the one table, and the translation that points at it.
    let table = block("040904B0", 1, &[], 0, &strings);
    let translation = block("Translation", 0, &[0x09, 0x04, 0xB0, 0x04], 4, &[]);
    let children = [block("StringFileInfo", 1, &[], 0, &[table]), block("VarFileInfo", 1, &[], 0, &[translation])];
    block("VS_VERSION_INFO", 0, &fixed, fixed.len() as u16, &children)
}

/// One block of a version resource: its length, its value's length (`size`:
/// bytes for binary, UTF-16 units for text), its `kind` (0 binary, 1 text),
/// its key, the value, then its children, each part starting on 4 bytes.
fn block(key: &str, kind: u16, value: &[u8], size: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0, 0];
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    for unit in key.encode_utf16().chain([0]) {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(value);
    for child in children {
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend_from_slice(child);
    }
    let length = out.len() as u16;
    out[..2].copy_from_slice(&length.to_le_bytes());
    out
}

/// Appends one resource: its 32-byte header (numeric type and name, neutral
/// language, moveable and discardable), then `data` padded to 4 bytes.
fn entry(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    let flags: u16 = if kind == 0 { 0 } else { 0x1030 };
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes());
    for part in [0xFFFF, kind, 0xFFFF, name] {
        res.extend_from_slice(&part.to_le_bytes());
    }
    res.extend_from_slice(&0u32.to_le_bytes());
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&0u16.to_le_bytes());
    res.extend_from_slice(&[0; 8]);
    res.extend_from_slice(data);
    res.resize(res.len().next_multiple_of(4), 0);
}
