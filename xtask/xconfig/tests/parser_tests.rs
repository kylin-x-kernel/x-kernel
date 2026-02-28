use std::path::PathBuf;
use xconfig::kconfig::Parser;

#[test]
fn test_parse_basic_config() {
    let kconfig_path = PathBuf::from("tests/fixtures/basic/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/basic");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    // Should parse 3 config entries
    assert_eq!(ast.entries.len(), 3);
}

#[test]
fn test_parse_source_directive() {
    let kconfig_path = PathBuf::from("tests/fixtures/source/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/source");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    assert!(result.is_ok());
    let ast = result.unwrap();

    // Should have mainmenu, config, source, and entries from sourced file
    assert!(ast.entries.len() >= 2);
}

#[test]
fn test_parse_example_project() {
    let kconfig_path = PathBuf::from("examples/sample_project/Kconfig");
    let srctree = PathBuf::from("examples/sample_project");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    if let Err(e) = &result {
        eprintln!("Parse error: {}", e);
    }
    assert!(result.is_ok());
    let ast = result.unwrap();

    // Should parse all entries including sourced files
    assert!(ast.entries.len() > 0);
}

#[test]
fn test_parse_conditional_defaults() {
    use xconfig::kconfig::ast::{Entry, Expr};

    let kconfig_path = PathBuf::from("tests/fixtures/conditional_defaults/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/conditional_defaults");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let result = parser.parse();

    if let Err(e) = &result {
        eprintln!("Parse error: {}", e);
    }
    assert!(result.is_ok());
    let ast = result.unwrap();

    // Find the ARCH config
    let arch_config = ast.entries.iter().find_map(|entry| {
        if let Entry::Config(config) = entry {
            if config.name == "ARCH" {
                return Some(config);
            }
        }
        None
    });

    assert!(arch_config.is_some(), "ARCH config not found");
    let arch_config = arch_config.unwrap();

    // Verify it has 4 defaults (3 conditional + 1 unconditional)
    assert_eq!(arch_config.properties.defaults.len(), 4);

    // Check first conditional default
    let default = &arch_config.properties.defaults[0];
    assert!(matches!(&default.value, Expr::Const(s) if s == "aarch64"));
    assert!(default.condition.is_some());
    if let Some(Expr::Symbol(sym)) = &default.condition {
        assert_eq!(sym, "ARCH_AARCH64");
    } else {
        panic!("Expected Symbol condition");
    }

    // Check second conditional default
    let default = &arch_config.properties.defaults[1];
    assert!(matches!(&default.value, Expr::Const(s) if s == "riscv64"));
    assert!(default.condition.is_some());
    if let Some(Expr::Symbol(sym)) = &default.condition {
        assert_eq!(sym, "ARCH_RISCV64");
    } else {
        panic!("Expected Symbol condition");
    }

    // Check third conditional default
    let default = &arch_config.properties.defaults[2];
    assert!(matches!(&default.value, Expr::Const(s) if s == "x86_64"));
    assert!(default.condition.is_some());

    // Check fourth unconditional default (fallback)
    let default = &arch_config.properties.defaults[3];
    assert!(matches!(&default.value, Expr::Const(s) if s == "unknown"));
    assert!(
        default.condition.is_none(),
        "Last default should be unconditional"
    );
}
