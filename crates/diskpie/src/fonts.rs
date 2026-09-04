//! Optional system fallback fonts for scripts the embedded faces do not cover.
//!
//! The embedded Ubuntu and Hack faces render Latin, Greek, and Cyrillic. File
//! names in CJK, Thai, Devanagari, Arabic, Myanmar, Tibetan, or Javanese would
//! otherwise appear as boxes. At startup, before the native window exists, the
//! composition root reads well-known Windows font files when they are present
//! and appends them as fallbacks after the embedded fonts. Missing files are
//! skipped silently; a read failure never fails startup; the fonts are read at
//! runtime from the user's own Windows installation and never redistributed.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use eframe::egui::{FontData, FontDefinitions, FontFamily};

/// Upper bound on the total bytes read from system font files.
pub const MAX_TOTAL_FONT_BYTES: u64 = 64 * 1_024 * 1_024;

/// One candidate system font: relative file name and face index within it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemFontCandidate {
    pub file_name: &'static str,
    pub face_index: u32,
}

/// Fallbacks in priority order. Segoe UI comes first because it also covers
/// the symbols and extended Latin ranges common in Windows file names.
pub const WINDOWS_FALLBACK_FONTS: [SystemFontCandidate; 10] = [
    SystemFontCandidate { file_name: "segoeui.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "msyh.ttc", face_index: 0 },
    SystemFontCandidate { file_name: "malgun.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "YuGothR.ttc", face_index: 0 },
    SystemFontCandidate { file_name: "Nirmala.ttc", face_index: 0 },
    SystemFontCandidate { file_name: "leelawad.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "ebrima.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "mmrtext.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "himalaya.ttf", face_index: 0 },
    SystemFontCandidate { file_name: "javatext.ttf", face_index: 0 },
];

/// A font file already read into memory, ready to be appended as a fallback.
#[derive(Clone, Debug)]
pub struct LoadedFont {
    pub name: String,
    pub bytes: Vec<u8>,
    pub face_index: u32,
}

/// Fonts read at startup plus a count of files skipped for the byte budget.
#[derive(Clone, Debug, Default)]
pub struct SystemFonts {
    pub loaded: Vec<LoadedFont>,
    pub skipped_for_budget: usize,
}

impl SystemFonts {
    /// Whether any fallback was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// Appends the loaded fonts as fallbacks after every embedded face.
    #[must_use]
    pub fn apply_to(&self, mut definitions: FontDefinitions) -> FontDefinitions {
        for font in &self.loaded {
            let data = FontData {
                font: Cow::Owned(font.bytes.clone()),
                index: font.face_index,
                tweak: Default::default(),
            };
            definitions.font_data.insert(font.name.clone(), Arc::new(data));
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                let names = definitions.families.entry(family).or_default();
                if !names.iter().any(|name| name == &font.name) {
                    names.push(font.name.clone());
                }
            }
        }
        definitions
    }
}

/// The Windows fonts directory: `%WINDIR%\Fonts`, else `C:\Windows\Fonts`.
#[must_use]
pub fn windows_fonts_directory() -> PathBuf {
    let windir = std::env::var_os("WINDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    windir.join("Fonts")
}

/// Reads the well-known fallbacks from the Windows fonts directory.
#[must_use]
pub fn load_windows_fallbacks() -> SystemFonts {
    load_from_directory(&windows_fonts_directory(), &WINDOWS_FALLBACK_FONTS, MAX_TOTAL_FONT_BYTES)
}

/// Reads `candidates` beneath `directory`, skipping missing or unreadable
/// files and any file that would exceed `max_total_bytes` in total.
#[must_use]
pub fn load_from_directory(
    directory: &Path,
    candidates: &[SystemFontCandidate],
    max_total_bytes: u64,
) -> SystemFonts {
    let mut fonts = SystemFonts::default();
    let mut remaining = max_total_bytes;
    for candidate in candidates {
        let path = directory.join(candidate.file_name);
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > remaining {
            fonts.skipped_for_budget += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(length) = u64::try_from(bytes.len()) else {
            continue;
        };
        if length > remaining {
            fonts.skipped_for_budget += 1;
            continue;
        }
        remaining -= length;
        fonts.loaded.push(LoadedFont {
            name: format!("system-{}-{}", candidate.file_name, candidate.face_index),
            bytes,
            face_index: candidate.face_index,
        });
    }
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let unique = format!(
            "diskpie-fonts-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).expect("fixture directory");
        directory
    }

    #[test]
    fn missing_files_are_skipped_and_the_budget_is_enforced() {
        let directory = temporary_directory("budget");
        std::fs::write(directory.join("a.ttf"), vec![1_u8; 10]).unwrap();
        std::fs::write(directory.join("b.ttf"), vec![2_u8; 10]).unwrap();
        let candidates = [
            SystemFontCandidate { file_name: "missing.ttf", face_index: 0 },
            SystemFontCandidate { file_name: "a.ttf", face_index: 0 },
            SystemFontCandidate { file_name: "b.ttf", face_index: 1 },
        ];

        let fonts = load_from_directory(&directory, &candidates, 15);
        assert_eq!(fonts.loaded.len(), 1);
        assert_eq!(fonts.loaded[0].name, "system-a.ttf-0");
        assert_eq!(fonts.skipped_for_budget, 1);

        let all = load_from_directory(&directory, &candidates, 1_000);
        assert_eq!(all.loaded.len(), 2);
        assert_eq!(all.loaded[1].face_index, 1);
        std::fs::remove_dir_all(&directory).expect("fixture cleanup");
    }

    #[test]
    fn fallbacks_are_appended_after_embedded_fonts_in_both_families() {
        let fonts = SystemFonts {
            loaded: vec![LoadedFont { name: "system-x".to_owned(), bytes: vec![0], face_index: 0 }],
            skipped_for_budget: 0,
        };
        let definitions = fonts.apply_to(FontDefinitions::default());
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let names = &definitions.families[&family];
            assert_eq!(names.last().map(String::as_str), Some("system-x"));
            assert!(names.len() > 1, "embedded fonts must stay in front");
        }
        assert!(definitions.font_data.contains_key("system-x"));
        assert!(!fonts.is_empty());
    }

    #[test]
    fn fonts_directory_falls_back_to_the_default_windows_root() {
        let directory = windows_fonts_directory();
        assert!(directory.ends_with("Fonts"));
    }
}
