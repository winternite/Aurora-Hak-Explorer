use std::{error::Error, fs};

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::str::contains;
use tempfile::tempdir;

const MODEL: &str = "\
newmodel demo
setsupermodel demo null
beginmodelgeom demo
node dummy demo
  parent null
endnode
endmodelgeom demo
donemodel
";

#[test]
fn cli_compiles_decompiles_and_validates() -> Result<(), Box<dyn Error>> {
    let directory = tempdir()?;
    let source = directory.path().join("demo.mdl.ascii");
    let binary = directory.path().join("demo.mdl");
    let ascii = directory.path().join("rebuilt.mdl.ascii");
    fs::write(&source, MODEL)?;

    cargo_bin_cmd!("nwnmdlcomp")
        .args(["compile", "--output"])
        .arg(&binary)
        .arg(&source)
        .assert()
        .success()
        .stdout(contains("demo.mdl.ascii"));

    cargo_bin_cmd!("nwnmdlcomp")
        .args(["decompile", "--output"])
        .arg(&ascii)
        .arg(&binary)
        .assert()
        .success();

    cargo_bin_cmd!("nwnmdlcomp")
        .arg("validate")
        .arg(&ascii)
        .assert()
        .success()
        .stdout(contains("OK"));

    let rebuilt = fs::read_to_string(ascii)?;
    assert!(rebuilt.contains("node dummy demo"));
    Ok(())
}
