use std::process::Command;

fn main() {
    // Compile ICP proto definitions.
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .file_descriptor_set_path(
            std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by Cargo"))
                .join("icp_handler_descriptor.bin"),
        )
        .compile_protos(
            &["proto/icp_handler/v1/icp_handler.proto"],
            &["proto"],
        )
        .expect("failed to compile icp_handler proto");

    // Embed build metadata the binary can report in health checks.
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_GIT_SHA={git_sha}");

    let build_time = chrono::Utc::now().to_rfc3339();
    println!("cargo:rustc-env=BUILD_TIME={build_time}");

    println!("cargo:rerun-if-changed=proto/icp_handler/v1/icp_handler.proto");
    println!("cargo:rerun-if-changed=build.rs");
}
