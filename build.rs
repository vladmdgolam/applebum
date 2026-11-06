//! Compiles translations, embeds Windows resources and links GLEW for libprojectM.

/// Known names for vcpkg's static GLEW library, in preferred order.
/// Names differ across vcpkg versions and triplets, so use the installed one.
#[cfg(windows)]
const GLEW_NAMES: &[&str] = &["glew32s", "libglew32", "glew32"];

/// Returns the vcpkg triplet matching the target architecture and CRT mode.
#[cfg(windows)]
fn vcpkg_triplet() -> Option<String> {
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        _ => return None,
    };
    let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|feature| feature == "crt-static");
    let suffix = if static_crt { "static" } else { "static-md" };
    Some(format!("{arch}-windows-{suffix}"))
}

/// Returns the known GLEW library installed in `lib`.
#[cfg(windows)]
fn glew_library(lib: &std::path::Path) -> Option<&'static str> {
    let present: Vec<String> = std::fs::read_dir(lib)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.eq_ignore_ascii_case("lib") {
                Some(path.file_stem()?.to_str()?.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();
    GLEW_NAMES
        .iter()
        .copied()
        .find(|name| present.iter().any(|found| found == name))
}

fn main() {
    #[cfg(feature = "soloist-native-audio")]
    native_soloist_audio();
    #[cfg(feature = "apple-music")]
    apple_music_bridge();
    fastframe_i18n::build::compile_catalogs("assets/i18n");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/windows/spotifast.ico");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("packaging/windows/spotifast.ico")
            .set("ProductName", "Spotifast")
            // Task Manager and the Windows mixer name the process by its description.
            .set("FileDescription", "Spotifast");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows resources not embedded: {error}");
        }
        // Static libprojectM requires the GLEW library installed by vcpkg.
        if std::env::var_os("CARGO_FEATURE_MILKDROP").is_some() {
            println!("cargo:rerun-if-env-changed=VCPKG_INSTALLATION_ROOT");
            if let (Some(root), Some(triplet)) =
                (std::env::var_os("VCPKG_INSTALLATION_ROOT"), vcpkg_triplet())
            {
                let lib = std::path::Path::new(&root)
                    .join("installed")
                    .join(triplet)
                    .join("lib");
                println!("cargo:rustc-link-search=native={}", lib.display());
                match glew_library(&lib) {
                    Some(name) => println!("cargo:rustc-link-lib=static={name}"),
                    None => {
                        // Include the directory listing in the error for diagnosis.
                        let listing = std::fs::read_dir(&lib)
                            .map(|entries| {
                                entries
                                    .flatten()
                                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_else(|error| format!("unreadable: {error}"));
                        println!(
                            "cargo:warning=no GLEW library in {}; it holds: {listing}",
                            lib.display()
                        );
                    }
                }
            }
        }
    }
}

#[cfg(feature = "soloist-native-audio")]
fn native_soloist_audio() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let source = match target.as_str() {
        "macos" => "src/soloist/native/macos.mm",
        "windows" => "src/soloist/native/windows.cpp",
        _ => return,
    };
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-changed=src/soloist/native/audio.h");
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").file(source);
    if target == "macos" {
        build.flag("-fobjc-arc");
        for framework in ["Foundation", "CoreAudio", "AudioToolbox"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    } else {
        build.flag_if_supported("/EHsc");
        println!("cargo:rustc-link-lib=mmdevapi");
        println!("cargo:rustc-link-lib=ole32");
        println!("cargo:rustc-link-lib=uuid");
    }
    build.compile("spotifast_soloist_audio");
}

/// Compiles the Swift bridge to MusicKit into a static library.
#[cfg(feature = "apple-music")]
fn apple_music_bridge() {
    let source = "src/apple/native/Bridge.swift";
    println!("cargo:rerun-if-changed={source}");
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let library = format!("{out}/libambridge.a");
    let status = std::process::Command::new("xcrun")
        .args(["swiftc", "-parse-as-library", "-emit-library", "-static", "-O", "-swift-version", "5"])
        .args(["-target", "arm64-apple-macos15.0", "-module-name", "AMBridge", "-o", &library, source])
        .status()
        .expect("swiftc runs through xcrun");
    assert!(status.success(), "the Swift bridge didn't compile");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=ambridge");
    for framework in ["Foundation", "MusicKit", "CoreAudio", "AudioToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    let toolchain = std::process::Command::new("xcrun")
        .args(["--toolchain", "default", "--find", "swiftc"])
        .output()
        .expect("xcrun finds swiftc");
    let swiftc = String::from_utf8_lossy(&toolchain.stdout);
    if let Some(bin) = std::path::Path::new(swiftc.trim()).parent()
        && let Some(usr) = bin.parent()
    {
        println!("cargo:rustc-link-search=native={}", usr.join("lib/swift/macosx").display());
    }
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-lib=framework=Network");
    airplay_sender(&out);
}

/// Builds the vendored AirPlay sender and our thin wrapper around it.
#[cfg(feature = "apple-music")]
fn airplay_sender(out: &str) {
    let source = "third_party/airplay2-sender-cpp";
    println!("cargo:rerun-if-changed={source}/VENDORED_COMMIT");
    println!("cargo:rerun-if-changed=src/apple/native/airplay.cpp");
    let build = format!("{out}/airplay");
    let configured = std::process::Command::new("cmake")
        .args(["-S", source, "-B", &build, "-DCMAKE_BUILD_TYPE=Release"])
        .args(["-DAIRPLAY_BUILD_EXAMPLE=OFF", "-DAIRPLAY_BUILD_TESTS=OFF"])
        .arg("-DCMAKE_OSX_DEPLOYMENT_TARGET=15.0")
        .status()
        .expect("cmake runs");
    assert!(configured.success(), "the AirPlay sender didn't configure");
    let built = std::process::Command::new("cmake")
        .args(["--build", &build, "--target", "raop_sender", "-j"])
        .status()
        .expect("cmake runs");
    assert!(built.success(), "the AirPlay sender didn't build");
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .include(format!("{source}/src"))
        .file("src/apple/native/airplay.cpp")
        .flag("-mmacosx-version-min=15.0")
        .compile("am_airplay");
    // Dependents first, for the linker.
    for library in ["raop_sender", "airplay_crypto", "ed25519", "mbedcrypto"] {
        let file = format!("lib{library}.a");
        let folder = find_file(std::path::Path::new(&build), &file)
            .unwrap_or_else(|| panic!("{file} wasn't built"));
        println!("cargo:rustc-link-search=native={}", folder.display());
        println!("cargo:rustc-link-lib=static={library}");
    }
}

/// The folder under `root` that holds `file`.
#[cfg(feature = "apple-music")]
fn find_file(root: &std::path::Path, file: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, file) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|name| name == file) {
            return Some(root.to_path_buf());
        }
    }
    None
}
