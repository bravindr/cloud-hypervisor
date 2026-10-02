use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=src/qpl_shim.c");
    println!("cargo:rerun-if-env-changed=QPL_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=QPL_LIB_DIR");
    println!("cargo:rerun-if-env-changed=DTO_LIB_DIR");
    println!("cargo:rerun-if-env-changed=DTO_SRC_DIR");

    if env::var_os("CARGO_FEATURE_DTO").is_some() {
        link_dto();
    }

    if env::var_os("CARGO_FEATURE_QPL").is_none() {
        return;
    }

    let include_dir =
        env::var("QPL_INCLUDE_DIR").unwrap_or_else(|_| "/usr/local/include".to_owned());
    let library_dir = env::var("QPL_LIB_DIR").unwrap_or_else(|_| "/usr/local/lib64".to_owned());
    cc::Build::new()
        .file("src/qpl_shim.c")
        .include(include_dir)
        .warnings(true)
        .compile("qpl_shim");
    println!("cargo:rustc-link-search=native={library_dir}");
    println!("cargo:rustc-link-lib=static=qpl");
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=dl");
}

/// The DTO revision this daemon is written against: the explicit async API
/// (`dto_submit_compare`, `dto_submit_memfill`, `dto_submit_transl_fetch`) and
/// the `libdto_explicit` target (no libc interposers) live on this branch.
const DTO_REPO: &str = "https://github.com/byrnedj/DTO";
const DTO_BRANCH: &str = "ch-async-ops";
const DTO_REV: &str = "53d37322af526aa5c7cb7e176c0d6612b8e1d9d7";

/// Link `libdto_explicit`. Either build it from a DTO checkout named by
/// `DTO_SRC_DIR` (cmake, into OUT_DIR), or take a prebuilt library from
/// `DTO_LIB_DIR` (default `/usr/local/lib`). DTO is linked dynamically, as it
/// ships, with an rpath to wherever the library was found.
fn link_dto() {
    let dto_dir = if let Some(src) = env::var_os("DTO_SRC_DIR") {
        let src = PathBuf::from(src);
        let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("dto");
        println!("cargo:rerun-if-changed={}", src.join("dto.c").display());
        println!("cargo:rerun-if-changed={}", src.join("dto.h").display());
        let status = Command::new("cmake")
            .args(["-S"])
            .arg(&src)
            .arg("-B")
            .arg(&out)
            .args([
                "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
                "-DDTO_BUILD_EXPLICIT=ON",
            ])
            .status()
            .expect("running cmake for DTO");
        assert!(
            status.success(),
            "cmake configure of DTO in {} failed",
            src.display()
        );
        let status = Command::new("cmake")
            .args(["--build"])
            .arg(&out)
            .args(["--target", "dto_explicit", "--parallel"])
            .status()
            .expect("running cmake --build for DTO");
        assert!(
            status.success(),
            "building libdto_explicit from {} failed",
            src.display()
        );
        let header = fs::read_to_string(src.join("dto.h")).unwrap_or_default();
        assert!(
            header.contains("dto_submit_compare"),
            "{} is not the DTO revision this daemon needs ({DTO_REPO} branch {DTO_BRANCH}, {DTO_REV})",
            src.display()
        );
        out.display().to_string()
    } else {
        env::var("DTO_LIB_DIR").unwrap_or_else(|_| "/usr/local/lib".to_owned())
    };
    println!("cargo:rustc-link-search=native={dto_dir}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dto_dir}");
    println!("cargo:rustc-link-lib=dylib=dto_explicit");
}
