//! Settings kept between runs, as `key=value` lines in settings.txt in the user's config folder
//! (%APPDATA%\ttpf-viewer on Windows, ~/.config/ttpf-viewer elsewhere). Unknown or broken lines are ignored.

use std::path::PathBuf;

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub sort: String,
    pub descending: bool,
    pub thumb_size: f32,
    pub show: String,
    pub show_unavailable: bool,
    pub tonemap: bool,
    pub as_dds: bool,
    pub z_up: bool,
    pub folder: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            sort: "number".into(),
            descending: false,
            thumb_size: 160.0,
            show: "all".into(),
            show_unavailable: false,
            tonemap: true,
            as_dds: false,
            z_up: true,
            folder: None,
        }
    }
}

fn file() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("ttpf-viewer").join("settings.txt"))
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let flag = v == "true";
            match k.trim() {
                "sort" => s.sort = v.into(),
                "descending" => s.descending = flag,
                "thumb_size" => s.thumb_size = v.parse::<f32>().unwrap_or(s.thumb_size).clamp(64.0, 384.0),
                "show" => s.show = v.into(),
                "show_unavailable" => s.show_unavailable = flag,
                "tonemap" => s.tonemap = flag,
                "as_dds" => s.as_dds = flag,
                "z_up" => s.z_up = flag,
                "folder" if !v.is_empty() => s.folder = Some(PathBuf::from(v)).filter(|p| p.is_dir()),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(f) = file() else { return };
        let text = format!(
            "sort={}\ndescending={}\nthumb_size={}\nshow={}\nshow_unavailable={}\ntonemap={}\nas_dds={}\nz_up={}\nfolder={}\n",
            self.sort,
            self.descending,
            self.thumb_size,
            self.show,
            self.show_unavailable,
            self.tonemap,
            self.as_dds,
            self.z_up,
            self.folder.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        );
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(f, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join(format!("ttpf-viewer-test-{}", std::process::id()));
        // SAFETY: only this test reads these variables
        unsafe {
            std::env::set_var("APPDATA", &dir);
        }
        let s = Settings {
            sort: "size".into(),
            descending: true,
            thumb_size: 222.0,
            show: "cube".into(),
            show_unavailable: true,
            tonemap: false,
            as_dds: true,
            z_up: false,
            folder: Some(std::env::temp_dir()),
        };
        s.save();
        let back = Settings::load();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(back == s, "settings changed on the way back");
    }
}
