// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::path::PathBuf;

use xconfig::kconfig::Parser;

#[test]
fn test_source_recursion() {
    let kconfig_path = PathBuf::from("tests/fixtures/source/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/source");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    // Should successfully parse with source directive
    assert!(result.is_ok());
}

#[test]
fn test_circular_source_detection() {
    let kconfig_path = PathBuf::from("tests/fixtures/source/recursive/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/source");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    // Should detect circular dependency
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, xconfig::KconfigError::RecursiveSource { .. }));
}

#[test]
fn test_nested_source() {
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    // Should handle nested source directives
    assert!(result.is_ok());
    let ast = result.unwrap();

    // Verify we got content from multiple files
    assert!(ast.entries.len() > 3);
}

#[test]
fn test_source_escaping_srctree_is_rejected() {
    // A `source` directive that resolves outside srctree (via `..`) must be
    // rejected (CWE-22 containment), even when the target file exists.
    let srctree = tempfile::TempDir::new().unwrap();
    let srctree_path = srctree.path();
    let outside_name = format!(
        "{}_escape.conf",
        srctree_path.file_name().unwrap().to_str().unwrap()
    );
    let outside = srctree_path.parent().unwrap().join(&outside_name);
    std::fs::write(&outside, "config ESCAPE\n    bool\n").unwrap();
    let kconfig = srctree_path.join("Kconfig");
    std::fs::write(&kconfig, format!("source \"../{outside_name}\"\n")).unwrap();

    let mut parser = Parser::new(&kconfig, srctree_path).unwrap();
    let result = parser.parse();

    let _ = std::fs::remove_file(&outside);
    assert!(result.is_err(), "source escaping srctree must be rejected");
}
