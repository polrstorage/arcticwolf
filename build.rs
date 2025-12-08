use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = Path::new(&out_dir);

    // XDR v3 directory
    let xdr_v3 = PathBuf::from("xdr/v3");

    // Check if xdrgen is available
    let xdrgen_check = Command::new("xdrgen")
        .arg("--version")
        .output();

    if xdrgen_check.is_err() {
        eprintln!("WARNING: xdrgen not found in PATH");
        eprintln!("Please install xdrgen: cargo install xdrgen");
        panic!("xdrgen is required for build");
    }

    // List of XDR specs to compile
    let xdr_specs = vec![
        ("rpc.x", "rpc_generated.rs"),
        ("portmap.x", "portmap_generated.rs"),
        ("mount.x", "mount_generated.rs"),
        ("nfs.x", "nfs_generated.rs"),
    ];

    for (spec_file, output_file) in xdr_specs {
        let spec_path = xdr_v3.join(spec_file);
        let output_path = out_path.join(output_file);

        // Tell cargo to rerun if the spec changes
        println!("cargo:rerun-if-changed={}", spec_path.display());

        // Run xdrgen to compile XDR spec (outputs to stdout)
        let output = Command::new("xdrgen")
            .arg(&spec_path)
            .output()
            .unwrap_or_else(|e| panic!("Failed to run xdrgen: {}", e));

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("xdrgen failed for {}: {}", spec_file, stderr);
        }

        // Write stdout to output file
        let mut generated_code = String::from_utf8(output.stdout.clone())
            .unwrap_or_else(|e| panic!("Invalid UTF-8 in generated code: {}", e));

        // Fix Copy trait issue for union types with Box
        // Remove Copy from enums that contain Box<T> in any variant
        if spec_file == "nfs.x" {
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum ACCESS3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum ACCESS3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum FSINFO3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum FSINFO3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum FSSTAT3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum FSSTAT3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum PATHCONF3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum PATHCONF3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum WRITE3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum WRITE3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum SETATTR3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum SETATTR3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum CREATE3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum CREATE3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum REMOVE3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum REMOVE3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum MKDIR3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum MKDIR3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum RMDIR3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum RMDIR3res"
            );
            generated_code = generated_code.replace(
                "#[derive( Copy , Clone , Debug , Eq , PartialEq )] pub enum RENAME3res",
                "#[derive( Clone , Debug , Eq , PartialEq )] pub enum RENAME3res"
            );
        }

        fs::write(&output_path, generated_code.as_bytes())
            .unwrap_or_else(|e| panic!("Failed to write {}: {}", output_path.display(), e));

        println!("cargo:warning=Generated {} from {}", output_file, spec_file);
    }
}
