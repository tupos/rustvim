use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn workspace_dir(manifest_dir: &Path) -> PathBuf {
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("rustvim_core must be located below src/nvim")
        .to_path_buf()
}

fn build_dir(workspace_dir: &Path) -> PathBuf {
    let Some(configured_dir) = env::var_os("RUSTVIM_BUILD_DIR") else {
        return workspace_dir.join("build");
    };

    let configured_dir = PathBuf::from(configured_dir);
    if configured_dir.is_absolute() {
        configured_dir
    } else {
        workspace_dir.join(configured_dir)
    }
}

fn build_libnvim(build_dir: &Path) {
    let cache_path = build_dir.join("CMakeCache.txt");
    if !cache_path.exists() {
        panic!(
            "CMake build directory {} is not configured; run CMake before cargo",
            build_dir.display()
        );
    }

    let cmake = env::var_os("CMAKE").unwrap_or_else(|| OsString::from("cmake"));
    let mut command = Command::new(cmake);
    command
        .arg("--build")
        .arg(build_dir)
        .arg("--target")
        .arg("libnvim");

    if let Some(config) = env::var_os("RUSTVIM_CMAKE_CONFIG")
        && !config.is_empty()
    {
        command.arg("--config").arg(config);
    }

    let status = command.status().unwrap_or_else(|error| {
        panic!(
            "failed to build libnvim in {}: {error}",
            build_dir.display()
        )
    });
    if !status.success() {
        panic!(
            "building libnvim in {} failed with {status}",
            build_dir.display()
        );
    }
}

fn cmake_cache_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let (entry, value) = line.split_once('=')?;
        let (name, _) = entry.split_once(':')?;
        (name == key).then_some(value)
    })
}

fn cmake_cache_enabled(contents: &str, key: &str) -> bool {
    matches!(
        cmake_cache_value(contents, key),
        Some("1" | "ON" | "TRUE" | "YES")
    )
}

fn library_link_name(path: &Path) -> Option<&str> {
    let file_name = path.file_name()?.to_str()?;
    let file_name = file_name.strip_prefix("lib").unwrap_or(file_name);

    [".a", ".so", ".dylib", ".tbd", ".lib"]
        .into_iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))
}

fn link_cmake_library(cache: &str, key: &str) {
    let Some(library) = cmake_cache_value(cache, key) else {
        return;
    };
    if library.ends_with("-NOTFOUND") {
        return;
    }

    let path = Path::new(library);
    let directory = path
        .parent()
        .unwrap_or_else(|| panic!("{key} has no parent directory"));
    let name =
        library_link_name(path).unwrap_or_else(|| panic!("cannot derive link name from {library}"));

    println!("cargo::rustc-link-search=native={}", directory.display());
    println!("cargo::rustc-link-lib={name}");
}

fn main() {
    println!("cargo::rerun-if-env-changed=CMAKE");
    println!("cargo::rerun-if-env-changed=RUSTVIM_BUILD_DIR");
    println!("cargo::rerun-if-env-changed=RUSTVIM_CMAKE_BUILD");
    println!("cargo::rerun-if-env-changed=RUSTVIM_CMAKE_CONFIG");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let workspace_dir = workspace_dir(&manifest_dir);
    let build_dir = build_dir(&workspace_dir);
    println!(
        "cargo::rerun-if-changed={}",
        workspace_dir.join("CMakeLists.txt").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        workspace_dir.join("cmake").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        workspace_dir.join("src/nvim").display()
    );

    if env::var_os("RUSTVIM_CMAKE_BUILD").is_none() {
        build_libnvim(&build_dir);
    }

    let cache_path = build_dir.join("CMakeCache.txt");
    println!("cargo::rerun-if-changed={}", cache_path.display());
    let cache = fs::read_to_string(&cache_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", cache_path.display()));

    println!(
        "cargo::rustc-link-search=native={}",
        build_dir.join("lib").display()
    );

    link_cmake_library(&cache, "LUV_LIBRARY");
    link_cmake_library(&cache, "TREESITTER_LIBRARY");
    link_cmake_library(&cache, "UTF8PROC_LIBRARY");

    if cmake_cache_enabled(&cache, "ENABLE_LIBINTL") {
        link_cmake_library(&cache, "LIBINTL_LIBRARY");
        link_cmake_library(&cache, "ICONV_LIBRARY");
    }
    if cmake_cache_enabled(&cache, "ENABLE_UNIBILIUM") {
        link_cmake_library(&cache, "UNIBILIUM_LIBRARY");
    }
    if cmake_cache_enabled(&cache, "ENABLE_WASMTIME") {
        link_cmake_library(&cache, "WASMTIME_LIBRARY");
    }

    if cmake_cache_enabled(&cache, "PREFER_LUA") {
        link_cmake_library(&cache, "LUA_LIBRARY");
    } else {
        link_cmake_library(&cache, "LUAJIT_LIBRARY");
    }

    if cfg!(unix) {
        println!("cargo::rustc-link-lib=m");
        println!("cargo::rustc-link-lib=util");
    }

    link_cmake_library(&cache, "LIBUV_LIBRARY");
    for (cache_key, library) in [
        ("HAVE_LIBDL", "dl"),
        ("HAVE_LIBKSTAT", "kstat"),
        ("HAVE_LIBKVM", "kvm"),
        ("HAVE_LIBNSL", "nsl"),
        ("HAVE_LIBPERFSTAT", "perfstat"),
        ("HAVE_LIBRT", "rt"),
        ("HAVE_LIBSENDFILE", "sendfile"),
    ] {
        if cmake_cache_enabled(&cache, cache_key) {
            println!("cargo::rustc-link-lib={library}");
        }
    }
}
