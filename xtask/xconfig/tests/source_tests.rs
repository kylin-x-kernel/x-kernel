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
