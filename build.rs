use std::env;

#[path = "src/build_info.rs"]
#[allow(dead_code)]
// The build script shares validation; identity accessors are used by the binary.
mod build_info;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_info.rs");
    println!("cargo:rerun-if-env-changed=HERDR_PACKAGE_MANAGER");
    let package_manager = env::var("HERDR_PACKAGE_MANAGER");
    let source_commit = env::var("HERDR_BUILD_COMMIT");
    let package_manager = match &package_manager {
        Ok(value) => Some(value.as_str()),
        Err(env::VarError::NotPresent) => None,
        Err(err) => panic!("failed to read HERDR_PACKAGE_MANAGER: {err}"),
    };
    build_info::validate_package_identity(package_manager, source_commit.as_deref().ok())
        .unwrap_or_else(|err| panic!("{err}"));
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );
}
