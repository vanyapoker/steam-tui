use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
fn run(command: &mut Command) {
    let status = command
        .status()
        .expect("Install CMake, make, Perl and a C++17 compiler to build the networking transport");
    assert!(
        status.success(),
        "native transport build failed: {command:?}"
    );
}
fn main() {
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=../../vendor/GameNetworkingSockets");
    println!("cargo:rerun-if-env-changed=CMAKE");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("transport");
    let cmake = env::var_os("CMAKE").unwrap_or_else(|| "cmake".into());
    // Build matching protoc/runtime and crypto statically. Host protobuf/OpenSSL
    // shared libraries are usually absent from the game's container runtime.
    let protobuf = protobuf_src::include().parent().unwrap().to_path_buf();
    let crypto_root = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("openssl-build/install");
    if !crypto_root.join("lib/libcrypto.a").is_file() {
        openssl_src::Build::new().build();
    }
    let crypto_include = crypto_root.join("include");
    let crypto_lib = crypto_root.join("lib");
    run(Command::new(&cmake)
        .args(["-S", "native", "-B"])
        .arg(&out)
        .args([
            "-DCMAKE_BUILD_TYPE=Release",
            "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
        ])
        .arg(format!("-DCMAKE_PREFIX_PATH={}", protobuf.display()))
        .arg(format!(
            "-DProtobuf_DIR={}/lib/cmake/protobuf",
            protobuf.display()
        ))
        .arg(format!(
            "-DOPENSSL_INCLUDE_DIR={}",
            crypto_include.display()
        ))
        .arg(format!(
            "-DOPENSSL_CRYPTO_LIBRARY={}/libcrypto.a",
            crypto_lib.display()
        ))
        .arg(format!(
            "-DOPENSSL_SSL_LIBRARY={}/libssl.a",
            crypto_lib.display()
        ))
        .arg("-DCMAKE_DISABLE_FIND_PACKAGE_PkgConfig=TRUE")
        .arg("-UOPENSSL_NEW_ENOUGH")
        .arg("-DOPENSSL_USE_STATIC_LIBS=ON")
        .arg("-DProtobuf_USE_STATIC_LIBS=ON"));
    run(Command::new(&cmake)
        .arg("--build")
        .arg(&out)
        .args(["--target", "steam_tui_transport", "--parallel"])
        .arg(env::var("NUM_JOBS").unwrap_or("2".into())));
    // Merge native archives so the Rust linker can resolve circular references
    // among protobuf/Abseil without depending on distro-specific library lists.
    let combined = out.join("libsteam_tui_networking.a");
    let mut archives = vec![
        out.join("libsteam_tui_transport.a"),
        out.join("gns/src/libGameNetworkingSockets_s.a"),
        crypto_lib.join("libcrypto.a"),
    ];
    for entry in fs::read_dir(protobuf.join("lib")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "a") {
            archives.push(path);
        }
    }
    let mut ar = Command::new(env::var_os("AR").unwrap_or_else(|| "ar".into()))
        .arg("-M")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = ar.stdin.as_mut().unwrap();
        writeln!(stdin, "CREATE {}", combined.display()).unwrap();
        for archive in &archives {
            writeln!(stdin, "ADDLIB {}", archive.display()).unwrap();
        }
        writeln!(stdin, "SAVE\nEND").unwrap();
    }
    drop(ar.stdin.take());
    assert!(ar.wait().unwrap().success(), "native archive merge failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=steam_tui_networking");
    // Avoid a dependency on a newer libstdc++ than the container provides.
    let cxx = env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let result = Command::new(cxx)
        .arg("-print-file-name=libstdc++.a")
        .output()
        .unwrap();
    let stdcpp = Path::new(std::str::from_utf8(&result.stdout).unwrap().trim());
    assert!(stdcpp.is_file(), "C++ static runtime required");
    println!(
        "cargo:rustc-link-search=native={}",
        stdcpp.parent().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=stdc++");
    // GNS service threads live for the game process. SteamAPI may dlclose and
    // reload the client; keep their code and protobuf descriptors resident.
    println!("cargo:rustc-cdylib-link-arg=-Wl,-z,nodelete");
    println!("cargo:rustc-cdylib-link-arg=-Wl,-z,defs");
}
