//! Files that go with a message. Pictures a model can look at (PNG, JPEG,
//! GIF, WebP, up to [`MAX_IMAGE`] bytes) are sent as content; every other
//! file is named by its path, for the agent to read with its own tools.
// ponytail: a picture is read and encoded again for every request that carries it, and one that
// was moved or deleted later is named by path instead. Copy pictures into the app's data folder
// (and cache their encoding) if either starts to matter.

use std::path::Path;

/// The largest picture sent as content: the API takes 5 MB of base64, which is 3.75 MB of file.
pub const MAX_IMAGE: u64 = 3_500_000;

/// A picture, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its MIME type, e.g. `image/png`.
    pub media_type: &'static str,
    /// Its bytes in base64.
    pub data: String,
}

/// The MIME type of a picture a model can look at, by the file's extension.
#[must_use]
pub fn media_type(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Splits `files` into the pictures that can be sent as content and the
/// paths of everything else (other kinds of file, and pictures that are too
/// large or cannot be read).
#[must_use]
pub fn split(files: &[String]) -> (Vec<Image>, Vec<&str>) {
    let (mut images, mut named) = (Vec::new(), Vec::new());
    for file in files {
        let small = std::fs::metadata(file).is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_IMAGE);
        let image = media_type(file).filter(|_| small).and_then(|media_type| {
            let bytes = std::fs::read(file).ok()?;
            Some(Image { media_type, data: atroxabot_mcp::base64(&bytes) })
        });
        match image {
            Some(image) => images.push(image),
            None => named.push(file.as_str()),
        }
    }
    (images, named)
}

/// `text` with the files in `named` listed under it, for the agent to read.
#[must_use]
pub fn with_paths(text: &str, named: &[&str]) -> String {
    if named.is_empty() {
        return text.to_owned();
    }
    let list: Vec<String> = named.iter().map(|file| format!("- {file}")).collect();
    format!("{}\n\nAttached files:\n{}", text.trim_end(), list.join("\n")).trim_start().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_go_as_content_and_the_rest_by_path() {
        let dir = std::env::temp_dir().join(format!("atroxabot-attach-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = |name: &str| dir.join(name).display().to_string();
        std::fs::write(path("shot.PNG"), [1, 2, 3]).expect("write");
        std::fs::write(path("notes.md"), "hi").expect("write");
        let files = [path("shot.PNG"), path("notes.md"), path("gone.jpg")];
        let (images, named) = split(&files);
        assert_eq!(images, [Image { media_type: "image/png", data: "AQID".into() }]);
        assert_eq!(named, [files[1].as_str(), files[2].as_str()], "a missing picture is still named");
        assert_eq!(with_paths("Look ", &["/a.md", "/b.txt"]), "Look\n\nAttached files:\n- /a.md\n- /b.txt");
        assert_eq!(with_paths("", &["/a.md"]), "Attached files:\n- /a.md");
        assert_eq!(with_paths("Just this", &[]), "Just this");
        assert_eq!((media_type("a.JPeG"), media_type("a.svg"), media_type("png")), (Some("image/jpeg"), None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
