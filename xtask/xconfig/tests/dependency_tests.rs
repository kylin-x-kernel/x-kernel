use std::path::PathBuf;
use xconfig::kconfig::{Parser, SymbolTable, SymbolType};
use xconfig::ui::dependency_resolver::DependencyResolver;

#[test]
fn test_dependency_resolver_initialization() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    // Resolver should be initialized without errors
}

#[test]
fn test_depends_on_blocks_enable() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("BASE_LIB".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);

    // BASE_LIB is disabled by default
    symbol_table.set_value("BASE_LIB", "n".to_string());

    // Try to enable FEATURE_A which depends on BASE_LIB
    let result = resolver.can_enable("FEATURE_A", &symbol_table);
    assert!(
        result.is_err(),
        "Should not be able to enable FEATURE_A when BASE_LIB is disabled"
    );

    // Enable BASE_LIB first
    symbol_table.set_value("BASE_LIB", "y".to_string());

    // Now FEATURE_A should be enableable
    let result = resolver.can_enable("FEATURE_A", &symbol_table);
    assert!(
        result.is_ok(),
        "Should be able to enable FEATURE_A when BASE_LIB is enabled"
    );
}

#[test]
fn test_select_cascade() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("BASE_LIB".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("HELPER_MODULE".to_string(), SymbolType::Bool);

    // Enable BASE_LIB and FEATURE_A
    symbol_table.set_value("BASE_LIB", "y".to_string());
    symbol_table.set_value("FEATURE_A", "y".to_string());

    // Apply selects
    let selected = resolver.apply_selects("FEATURE_A", &mut symbol_table);

    // HELPER_MODULE should be automatically enabled
    assert!(
        selected.contains(&"HELPER_MODULE".to_string()),
        "HELPER_MODULE should be in the selected list"
    );
    assert!(
        symbol_table.is_enabled("HELPER_MODULE"),
        "HELPER_MODULE should be enabled"
    );
}

#[test]
fn test_reverse_select_blocks_disable() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("BASE_LIB".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("HELPER_MODULE".to_string(), SymbolType::Bool);

    // Enable all
    symbol_table.set_value("BASE_LIB", "y".to_string());
    symbol_table.set_value("FEATURE_A", "y".to_string());
    symbol_table.set_value("HELPER_MODULE", "y".to_string());

    // Try to disable HELPER_MODULE while FEATURE_A is enabled
    let result = resolver.can_disable("HELPER_MODULE", &symbol_table);
    assert!(
        result.is_err(),
        "Should not be able to disable HELPER_MODULE when FEATURE_A selects it"
    );
}

#[test]
fn test_imply_suggests() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("FEATURE_B".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("OPTIONAL_FEATURE".to_string(), SymbolType::Bool);

    // Enable FEATURE_B
    symbol_table.set_value("FEATURE_B", "y".to_string());

    // Get implied symbols
    let implied = resolver.get_implied_symbols("FEATURE_B", &symbol_table);

    // OPTIONAL_FEATURE should be implied
    assert!(
        implied.contains(&"OPTIONAL_FEATURE".to_string()),
        "OPTIONAL_FEATURE should be implied by FEATURE_B"
    );
}

#[test]
fn test_disable_cascade_check() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("BASE_LIB".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("TRISTATE_OPTION".to_string(), SymbolType::Tristate);

    // Enable all
    symbol_table.set_value("BASE_LIB", "y".to_string());
    symbol_table.set_value("FEATURE_A", "y".to_string());
    symbol_table.set_value("TRISTATE_OPTION", "m".to_string());

    // Check what will be affected if BASE_LIB is disabled
    let affected = resolver.check_disable_cascade("BASE_LIB", &symbol_table);

    // FEATURE_A and TRISTATE_OPTION should be affected
    assert!(
        affected.contains(&"FEATURE_A".to_string())
            || affected.contains(&"TRISTATE_OPTION".to_string()),
        "Disabling BASE_LIB should affect dependent symbols"
    );
}

#[test]
fn test_tristate_dependency() {
    let kconfig_path = PathBuf::from("tests/fixtures/dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("BASE_LIB".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("TRISTATE_OPTION".to_string(), SymbolType::Tristate);

    // BASE_LIB is disabled
    symbol_table.set_value("BASE_LIB", "n".to_string());

    // Try to enable TRISTATE_OPTION
    let result = resolver.can_enable("TRISTATE_OPTION", &symbol_table);
    assert!(
        result.is_err(),
        "Should not be able to enable TRISTATE_OPTION when BASE_LIB is disabled"
    );

    // Enable BASE_LIB
    symbol_table.set_value("BASE_LIB", "y".to_string());

    // Now should be able to enable
    let result = resolver.can_enable("TRISTATE_OPTION", &symbol_table);
    assert!(
        result.is_ok(),
        "Should be able to enable TRISTATE_OPTION when BASE_LIB is enabled"
    );
}

#[test]
fn test_imply_respects_dependencies() {
    // Setup: A imply B, B depends on C
    let kconfig_path = PathBuf::from("tests/fixtures/imply_dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/imply_dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("C".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("B".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("A".to_string(), SymbolType::Bool);

    // Scenario 1: C is disabled
    symbol_table.set_value("C", "n".to_string());
    symbol_table.set_value("A", "y".to_string());
    symbol_table.set_value("B", "n".to_string());

    let implied = resolver.get_implied_symbols("A", &symbol_table);

    assert!(
        implied.is_empty(),
        "B should NOT be implied when C (B's dependency) is disabled. Got: {:?}",
        implied
    );

    // Scenario 2: C is enabled
    symbol_table.set_value("C", "y".to_string());

    let implied = resolver.get_implied_symbols("A", &symbol_table);

    assert!(
        implied.contains(&"B".to_string()),
        "B SHOULD be implied when C (B's dependency) is enabled. Got: {:?}",
        implied
    );
}

#[test]
fn test_imply_complex_dependency_chain() {
    let kconfig_path = PathBuf::from("tests/fixtures/imply_dependency/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/imply_dependency");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("D".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("E".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("F".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("G".to_string(), SymbolType::Bool);

    // Test with full chain disabled
    symbol_table.set_value("D", "n".to_string());
    symbol_table.set_value("E", "n".to_string());
    symbol_table.set_value("F", "n".to_string());
    symbol_table.set_value("G", "y".to_string());

    let implied = resolver.get_implied_symbols("G", &symbol_table);

    assert!(
        implied.is_empty(),
        "F should NOT be implied when dependency chain is broken. Got: {:?}",
        implied
    );

    // Test with full chain enabled
    symbol_table.set_value("D", "y".to_string());
    symbol_table.set_value("E", "y".to_string());

    let implied = resolver.get_implied_symbols("G", &symbol_table);

    assert!(
        implied.contains(&"F".to_string()),
        "F SHOULD be implied when full dependency chain is satisfied. Got: {:?}",
        implied
    );
}
