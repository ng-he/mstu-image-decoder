//! The latest image, in a file for this plugin's own page.
//!
//! Live readings carry sizes rather than payloads -- the engine keeps the
//! length of a bytes field and not the bytes -- and the desktop already
//! serves a local file to a plugin's page. So the picture goes through a
//! file, and the reading names it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the file is rewritten. A page redraws at about this rate, and
/// anything faster is writing for nobody.
const EVERY: Duration = Duration::from_millis(250);

pub struct Preview {
    path: PathBuf,
    wrote: Option<Instant>,
    on: bool,
}

impl Preview {
    /// One file per plugin instance, beside the engine's own socket where
    /// there is a runtime directory for it.
    pub fn new(id: &str) -> Self {
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());

        Self {
            path: dir.join(format!("mstu-preview-{id}.jpg")),
            wrote: None,
            on: true,
        }
    }

    /// Whether previews are wanted at all.
    pub fn set(&mut self, on: bool) {
        self.on = on;

        if !on {
            self.forget();
        }
    }

    /// The file to show, if there is one.
    pub fn path(&self) -> &str {
        match self.wrote.is_some() {
            true => self.path.to_str().unwrap_or_default(),
            false => "",
        }
    }

    /// Keeps `jpeg` for the page, at most as often as `EVERY`.
    pub fn keep(&mut self, jpeg: &[u8]) {
        if !self.on || !self.due() {
            return;
        }

        // Written beside and renamed over, so a page reading it never catches
        // half an image.
        let part = self.path.with_extension("part");

        if fs::write(&part, jpeg).is_err() {
            return;
        }

        if fs::rename(&part, &self.path).is_ok() {
            self.wrote = Some(Instant::now());
        } else {
            let _ = fs::remove_file(&part);
        }
    }

    fn due(&self) -> bool {
        self.wrote
            .map(|wrote| wrote.elapsed() >= EVERY)
            .unwrap_or(true)
    }

    /// Takes the file away, which `Drop` does and a setting may ask for.
    fn forget(&mut self) {
        if self.wrote.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }

        let _ = fs::remove_file(self.path.with_extension("part"));
    }

    /// Where the file would be, whether or not one is there.
    pub fn at(&self) -> &Path {
        &self.path
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.forget();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(name: &str) -> Preview {
        let mut preview = Preview::new(&format!("test-{}-{name}", std::process::id()));

        preview.forget();
        preview
    }

    #[test]
    fn nothing_is_named_before_an_image_is_kept() {
        let preview = preview("named");

        assert_eq!(preview.path(), "");
        assert!(!preview.at().exists());
    }

    #[test]
    fn the_image_kept_is_the_file_named() {
        let mut preview = preview("kept");

        preview.keep(&[0xff, 0xd8, 0xff, 0x00]);

        assert_eq!(preview.path(), preview.at().to_str().unwrap());
        assert_eq!(
            fs::read(preview.at()).expect("the file"),
            [0xff, 0xd8, 0xff, 0x00]
        );
    }

    /// A page redraws a few times a second, and writing faster is writing for
    /// nobody.
    #[test]
    fn images_are_not_written_faster_than_a_page_reads_them() {
        let mut preview = preview("often");

        preview.keep(&[1]);
        preview.keep(&[2]);

        assert_eq!(fs::read(preview.at()).expect("the file"), [1]);
    }

    #[test]
    fn previews_turned_off_leave_no_file_behind() {
        let mut preview = preview("off");

        preview.keep(&[1]);
        assert!(preview.at().exists());

        preview.set(false);

        assert!(!preview.at().exists());
        assert_eq!(preview.path(), "");

        preview.keep(&[2]);
        assert!(!preview.at().exists(), "and keeps none after that");
    }

    #[test]
    fn the_file_goes_when_the_plugin_does() {
        let at = {
            let mut preview = preview("gone");

            preview.keep(&[1]);
            preview.at().to_path_buf()
        };

        assert!(!at.exists());
    }
}
