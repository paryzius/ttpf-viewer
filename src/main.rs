// No console window when the app is started by double-clicking (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive;
mod bc6h;
mod bcn;
mod cube3d;
mod folder;
mod gui;
mod layout;
mod settings;
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
        --clip         HDR textures: clip values above white instead of tone-mapping
  ttpf-viewer extract-folder DIR OUT  the same for every .ttpf under DIR, one folder per file (--dds, --clip)
  ttpf-viewer panorama FILE.ttpf N OUT.png   save cube map #N as a 360-degree panorama (--y-up: Y is up)
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
    if args.iter().any(|a| a == "--clip") {
        bcn::TONEMAP.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    match args[0].as_str() {
        "extract" if args.len() >= 3 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let all: Vec<usize> = (0..arc.textures.len()).collect();
            let dds = args.iter().any(|a| a == "--dds");
            let (written, skipped) = archive::export(&arc, &all, &PathBuf::from(&args[2]), dds, &Default::default(), &AtomicUsize::new(0))?;
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
        "extract-folder" if args.len() >= 3 => {
            let t = folder::extract_all(PathBuf::from(&args[1]), PathBuf::from(&args[2]), args.iter().any(|a| a == "--dds"));
            let r = loop {
                if let Some(r) = t.take() {
                    break r;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }?;
            println!("saved {} textures from {} files to {} ({} unreadable, {} files couldn't be opened)", r.written, r.files - r.failed, args[2], r.skipped, r.failed);
            Ok(())
        }
        "panorama" if args.len() >= 4 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let t: usize = args[2].parse().map_err(|_| "bad texture number")?;
            if t >= arc.textures.len() || !arc.textures[t].cube {
                return Err(format!("#{t} is not a cube map"));
            }
            let faces = arc.cube_faces(t).map_err(|p| p.text().to_string())?;
            let img = cube3d::panorama(&faces, !args.iter().any(|a| a == "--y-up"));
            archive::save_png(&img, &PathBuf::from(&args[3]))?;
            println!("saved {}x{} panorama to {}", img.w, img.h, args[3]);
            Ok(())
        }
        // development aid, not in the help: copy a texture to the clipboard
        "copy" if args.len() >= 3 => {
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let t: usize = args[2].parse().map_err(|_| "bad texture number")?;
            let img = arc.best_image(t).map_err(|p| p.text().to_string())?;
            gui::copy_image(&img)?;
            println!("copied {}x{}", img.w, img.h);
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
        // development aid, not in the help: how well mip 1 matches a 2x2 average of mip 0, per texture
        "mipcheck" if args.len() >= 2 => {
            use rayon::prelude::*;
            let arc = Archive::open(&PathBuf::from(&args[1]), &Progress::default())?;
            let lines: Vec<String> = (0..arc.textures.len())
                .into_par_iter()
                .flat_map(|i| (0..arc.textures[i].layer_count()).map(move |l| (i, l)).collect::<Vec<_>>())
                .filter_map(|(i, layer)| {
                    let (Ok(a), Ok(b)) = (arc.image_in(i, layer, 0), arc.image_in(i, layer, 1)) else { return None };
                    if a.w < 8 || a.h < 8 || b.w != a.w / 2 || b.h != a.h / 2 {
                        return None;
                    }
                    let (mut err, mut spread, mut n) = (0f64, 0f64, 0f64);
                    let mean = a.px.iter().map(|&v| v as f64).sum::<f64>() / a.px.len() as f64;
                    for y in 0..b.h {
                        for x in 0..b.w {
                            for c in 0..3 {
                                let p = |xx: usize, yy: usize| a.px[(yy * a.w + xx) * 4 + c] as f64;
                                let avg = (p(2 * x, 2 * y) + p(2 * x + 1, 2 * y) + p(2 * x, 2 * y + 1) + p(2 * x + 1, 2 * y + 1)) / 4.0;
                                err += (avg - b.px[(y * b.w + x) * 4 + c] as f64).abs();
                                spread += (avg - mean).abs();
                                n += 1.0;
                            }
                        }
                    }
                    Some(format!("{i} {:.2} {:.2} {layer}", err / n, spread / n))
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
    if args.first().is_some_and(|a| a == "extract" || a == "list" || a == "checksums" || a == "mipcheck" || a == "extract-folder" || a == "panorama" || a == "copy" || a == "dump-raw" || a == "--help" || a == "-h") {
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
