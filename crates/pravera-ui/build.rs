fn main() {
    // Only embed an icon when building for Windows — the resource is ignored
    // elsewhere and `winres` would try to invoke `rc` needlessly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Render the tile logo (dark square + blob) to several sizes and pack them
    // into a single .ico so Explorer picks the right one per view. `resvg`
    // rasterises the 64×64 vector at each size; the tile stays crisp because
    // it is a vector at the source.
    let svg = std::fs::read_to_string("assets/logo.svg")
        .expect("assets/logo.svg must be present for the exe icon");

    let sizes = [256u32, 48, 32, 16];
    let mut images = Vec::new();

    for size in sizes {
        let opts = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_str(&svg, &opts).expect("logo.svg parses");
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let sx = size as f32 / 64.0;
        let sy = size as f32 / 64.0;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(sx, sy),
            &mut pixmap.as_mut(),
        );
        let rgba = pixmap.take();
        // `ico` expects RGBA, 8-bit per channel, straight (which `tiny-skia`
        // already is).
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        images.push(image);
    }

    let icon_dir = std::path::Path::new("assets/icon.ico");
    // Ensure the directory exists (it does) and write the ICO.
    if let Some(parent) = icon_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::File::create(icon_dir).expect("create icon.ico");
    let icon = ico::IconDir::new(ico::ResourceType::Icon);
    let mut icon_dir_obj = icon;
    for img in images {
        icon_dir_obj.add_entry(ico::IconDirEntry::encode(&img).expect("encode ico entry"));
    }
    icon_dir_obj.write(file).expect("write ico");

    // Tell `winres` to embed it. `set_icon` expects a path relative to the
    // crate root; we just wrote `assets/icon.ico`.
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    if let Err(e) = res.compile() {
        eprintln!("winres failed (non-fatal for non-Windows builds): {e}");
    }

    // Elevation, asked for on the binary only. The whole product assumes an
    // elevated process — input injection is subject to UIPI, autostart is a
    // scheduled task because Windows will not elevate a Run entry, and the
    // virtual display driver's `pnputil` install needs it. The flag is
    // `rustc-link-arg-bins`, so the library's test harness does not inherit
    // the requirement and `cargo test` keeps working unelevated.
    //
    // (`pravera.manifest` documents why this is a linker argument rather than
    // a manifest file: two manifest fragments that disagree about the level
    // are a hard `mt.exe` merge error. `/MANIFEST:EMBED` must come first —
    // without it `link.exe` writes the UAC statement to a side-by-side
    // `.manifest` file that never ships. Neither flag may contain a space,
    // which the linker would split into a phantom input file; `uiAccess` is
    // omitted because `'false'` is the default.)
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");

    // Re-run if the source SVG or a bundled driver changes.
    println!("cargo:rerun-if-changed=assets/logo.svg");
    println!("cargo:rerun-if-changed=assets/logo-mark.svg");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/idd");
    // Keep the bundled IDD visible to `cargo run` without a manual copy: mirror
    // `assets/idd/` next to the exe when it exists, so the runtime probe
    // `exe_dir/idd` sees it even from `target/debug/`.
    {
        let exe_idd = std::path::Path::new("assets/idd");
        if exe_idd.is_dir() {
            if let Ok(entries) = std::fs::read_dir(exe_idd) {
                let has_inf = entries.flatten().any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("inf"))
                });
                if has_inf {
                    // `OUT_DIR` is `target/debug/build/pravera-ui-…/out`. The
                    // exe lives three parents up for a normal `cargo build`.
                    if let Ok(out) = std::env::var("OUT_DIR") {
                        let out_path = std::path::Path::new(&out);
                        // Walk up to the `targetdebug`/`targetrelease` dir.
                        let mut target_dir = out_path;
                        for _ in 0..4 {
                            if let Some(parent) = target_dir.parent() {
                                target_dir = parent;
                            }
                        }
                        // Heuristic: if we landed in `target`, look inside
                        // `debug`/`release` for the exe.
                        let candidates = ["debug", "release"];
                        for profile in candidates {
                            let candidate = target_dir.join(profile).join("idd");
                            // Only create the mirror if we can — a failure is
                            // not a build failure, dev fallback still works via
                            // `bundled_assets_dir()`.
                            if target_dir.join(profile).is_dir() {
                                let _ = std::fs::create_dir_all(&candidate);
                                // Copy .inf/.cat/.sys if missing.
                                if let Ok(src_entries) = std::fs::read_dir(exe_idd) {
                                    for entry in src_entries.flatten() {
                                        let src = entry.path();
                                        if src.is_file() {
                                            let dst = candidate.join(src.file_name().unwrap());
                                            // Don't overwrite a newer user drop.
                                            if !dst.exists() {
                                                let _ = std::fs::copy(&src, &dst);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
