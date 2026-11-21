// No console window when the app is started by double-clicking (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive;
mod bcn;
mod gui;
mod tile;

use archive::{Archive, Progress, Status};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

const HELP: &str = "\
ttpf-viewer - browse and extract textures from .ttpf archives

USAGE
  ttpf-viewer                         open the app
  ttpf-viewer FILE.ttpf               open the app with a file
  ttpf-viewer extract FILE.ttpf DIR   save every readable texture as DIR/<number>.png
        --dds          save .dds files with the original data instead
  ttpf-viewer list FILE.ttpf          print the textures in the file
";

fn attach_console() {
    // A windows-subsystem app has no console; borrow the one it was started from, for CLI output.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn status_text(s: Status) -> String {
    match s {
        Status::Full => "ok".into(),
        Status::Reduced(l) => format!("full size unreadable, mip {l} available"),
        Status::Unavailable(p) => p.text().into(),
    }
}

fn run_cli(args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "extract" if args.len() >= 3 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let all: Vec<usize> = (0..arc.textures.len()).collect();
            let dds = args.iter().any(|a| a == "--dds");
            let (written, skipped) = archive::export(&arc, &all, &PathBuf::from(&args[2]), dds, &AtomicUsize::new(0))?;
            println!("saved {written} textures to {} ({skipped} unreadable)", args[2]);
            Ok(())
        }
        "list" if args.len() >= 2 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let mut counts = std::collections::BTreeMap::new();
            for t in &arc.textures {
                println!("#{:<6} {:>5}x{:<5} {:22} {:6} tiles  {}", t.index, t.w, t.h, t.format_name(), t.tile.name(), status_text(t.status));
                let key = match t.status {
                    Status::Full => "ok".to_string(),
                    Status::Reduced(_) => "reduced size".to_string(),
                    Status::Unavailable(p) => p.text().to_string(),
                };
                *counts.entry(key).or_insert(0) += 1;
            }
            println!("\n{} textures: {:?}", arc.textures.len(), counts);
            Ok(())
        }
        // development aid, not in the help: a checksum of every texture's shown image, to compare builds
        "checksums" if args.len() >= 2 => {
            use rayon::prelude::*;
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let lines: Vec<String> = (0..arc.textures.len())
                .into_par_iter()
                .map(|i| {
                    let t = &arc.textures[i];
                    let h = arc.best_image(i).map(|img| {
                        let mut h = std::hash::DefaultHasher::new();
                        std::hash::Hash::hash(&(img.w, img.h, &img.px), &mut h);
                        format!("{:016x}", std::hash::Hasher::finish(&h))
                    });
                    format!("{} {} {}", t.index, status_text(t.status), h.unwrap_or_else(|p| p.text().to_string()))
                })
                .collect();
            println!("{}", lines.join("\n"));
            Ok(())
        }
        // development aid, not in the help: raw stored bytes of a texture's top level
        "dump-raw" if args.len() >= 4 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let t: usize = args[2].parse().map_err(|_| "bad texture number")?;
            let (data, pw, ph) = arc.raw(t, 0).map_err(|p| p.text().to_string())?;
            std::fs::write(&args[3], &data).map_err(|e| e.to_string())?;
            let tex = &arc.textures[t];
            println!("{} {} {} {} {} {}", tex.w, tex.h, pw, ph, tex.format_name(), tex.tile.name());
            Ok(())
        }
        _ => Err(HELP.into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "extract" || a == "list" || a == "checksums" || a == "dump-raw" || a == "--help" || a == "-h") {
        attach_console();
        if let Err(e) = run_cli(&args) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = gui::run(args.first().map(PathBuf::from)) {
        attach_console();
        eprintln!("ttpf-viewer: {e}");
        std::process::exit(1);
    }
}
