use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
};

const ICONS: &[&str] = &[
    "bucket-mount",
    "tray-on-dark-idle",
    "tray-on-dark-syncing",
    "tray-on-dark-paused",
    "tray-on-dark-error",
    "tray-on-light-idle",
    "tray-on-light-syncing",
    "tray-on-light-paused",
    "tray-on-light-error",
];

fn main() {
    setup_payload();
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=assets/windows.rc");
    println!("cargo::rerun-if-changed=assets/windows.manifest");
    for icon in ICONS {
        println!("cargo::rerun-if-changed=assets/icons/{icon}.ico");
    }
    for variable in ["RC", "WINDRES", "PATH"] {
        println!("cargo::rerun-if-env-changed={variable}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric_version = ["MAJOR", "MINOR", "PATCH"]
        .map(|component| {
            env::var(format!("CARGO_PKG_VERSION_{component}"))
                .unwrap()
                .parse::<u16>()
                .expect("Windows file-version components must fit in 16 bits")
                .to_string()
        })
        .join(",");
    let source = fs::read_to_string(root.join("assets/windows.rc"))
        .expect("read Windows resource template")
        .replace("@FILE_VERSION@", &format!("{numeric_version},0"))
        .replace("@PACKAGE_VERSION@", &version);
    let resource_source = output.join("bucket-mount.rc");
    fs::write(&resource_source, source).expect("write Windows resource source");

    let resource = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let resource = output.join("bucket-mount.res");
        compile(
            "RC",
            &["rc.exe", "llvm-rc.exe"],
            &root,
            &[
                "/nologo".into(),
                "/c65001".into(),
                "/I".into(),
                root.clone().into_os_string(),
                "/fo".into(),
                resource.clone().into_os_string(),
                resource_source.into_os_string(),
            ],
        );
        resource
    } else {
        let architecture = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
        let (compiler, format) = match architecture.as_str() {
            "x86_64" => ("x86_64-w64-mingw32-windres", "pe-x86-64"),
            "x86" => ("i686-w64-mingw32-windres", "pe-i386"),
            _ => panic!("Unsupported Windows resource architecture: {architecture}"),
        };
        let resource = output.join("bucket-mount.o");
        compile(
            "WINDRES",
            &[compiler, "llvm-windres", "windres"],
            &root,
            &[
                format!("--target={format}").into(),
                "--input-format=rc".into(),
                "--output-format=coff".into(),
                "--codepage=65001".into(),
                "--include-dir".into(),
                root.clone().into_os_string(),
                "--input".into(),
                resource_source.into_os_string(),
                "--output".into(),
                resource.clone().into_os_string(),
            ],
        );
        resource
    };
    // Include the same resources in test executables so native UI checks can
    // exercise the exact resource-loading path used by the application.
    println!("cargo::rustc-link-arg={}", resource.display());
}

fn setup_payload() {
    println!("cargo::rerun-if-env-changed=BUCKET_MOUNT_SETUP_PAYLOAD");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // A shell can define this variable with no value; that must mean "no payload"
    // rather than a path that cannot be resolved.
    let payload = env::var_os("BUCKET_MOUNT_SETUP_PAYLOAD").filter(|value| !value.is_empty());
    let source = match payload {
        Some(path) => {
            let path = fs::canonicalize(&path).unwrap_or_else(|error| {
                panic!(
                    "BUCKET_MOUNT_SETUP_PAYLOAD must name the built application executable to embed, but {} is not usable: {error}",
                    path.display()
                )
            });
            let bytes = fs::read(&path).expect("read setup application payload");
            assert!(
                bytes.starts_with(b"MZ"),
                "setup payload must be a Windows executable"
            );
            let pe_offset = bytes
                .get(0x3c..0x40)
                .map(|offset| u32::from_le_bytes(offset.try_into().unwrap()) as usize)
                .filter(|offset| *offset >= 64)
                .expect("setup payload must have a PE header");
            let header = bytes
                .get(pe_offset..pe_offset.checked_add(6).expect("PE offset overflow"))
                .expect("setup payload PE header is truncated");
            assert_eq!(
                &header[..4],
                b"PE\0\0",
                "invalid setup payload PE signature"
            );
            let architecture = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
            let expected_machine = match architecture.as_str() {
                "x86_64" => 0x8664,
                "aarch64" => 0xaa64,
                "x86" => 0x014c,
                _ => panic!("unsupported setup payload architecture: {architecture}"),
            };
            assert_eq!(
                u16::from_le_bytes(header[4..6].try_into().unwrap()),
                expected_machine,
                "setup payload architecture must match the setup target ({architecture})"
            );
            println!("cargo::rerun-if-changed={}", path.display());
            format!("static APPLICATION: &[u8] = include_bytes!({path:?});\n")
        }
        None => "static APPLICATION: &[u8] = &[];\n".to_owned(),
    };
    fs::write(output.join("setup_payload.rs"), source).expect("write setup payload source");
}

fn compile(variable: &str, candidates: &[&str], root: &Path, arguments: &[std::ffi::OsString]) {
    let override_path = env::var_os(variable);
    let tools: Vec<_> = match override_path {
        Some(path) => vec![path],
        None => candidates.iter().map(std::ffi::OsString::from).collect(),
    };
    for tool in tools {
        match Command::new(&tool)
            .current_dir(root)
            .args(arguments)
            .output()
        {
            Ok(result) => {
                assert!(
                    result.status.success(),
                    "Windows resource compiler {} failed:\n{}\n{}",
                    tool.to_string_lossy(),
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                return;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => panic!(
                "Could not start Windows resource compiler {}: {error}",
                tool.to_string_lossy()
            ),
        }
    }
    panic!(
        "Windows resource compiler not found. Use a Visual Studio Developer PowerShell with a Windows SDK, or add the LLVM MinGW bin directory to PATH. Set {variable} to override the resource compiler executable."
    );
}
