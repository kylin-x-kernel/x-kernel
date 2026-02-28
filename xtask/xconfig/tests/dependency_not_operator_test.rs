use std::path::PathBuf;
use xconfig::kconfig::{Parser, SymbolTable, SymbolType};
use xconfig::ui::dependency_resolver::DependencyResolver;

#[test]
fn test_not_operator_allows_enable_when_negated_symbol_disabled() {
    // Test case: depends on !PREEMPT, PREEMPT is disabled
    let kconfig_path = PathBuf::from("tests/fixtures/not_operator/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/not_operator");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("PREEMPT".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("ADVANCED_FEATURES".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("SCHEDULER_RT".to_string(), SymbolType::Bool);

    // Setup: ADVANCED_FEATURES=y, PREEMPT=n
    symbol_table.set_value("ADVANCED_FEATURES", "y".to_string());
    symbol_table.set_value("PREEMPT", "n".to_string());

    // Test: Should be able to enable SCHEDULER_RT
    let result = resolver.can_enable("SCHEDULER_RT", &symbol_table);
    assert!(
        result.is_ok(),
        "SCHEDULER_RT should be allowed when ADVANCED_FEATURES=y and PREEMPT=n. Error: {:?}",
        result.err()
    );
}

#[test]
fn test_not_operator_blocks_enable_when_negated_symbol_enabled() {
    // Test case: depends on !PREEMPT, but PREEMPT is enabled
    let kconfig_path = PathBuf::from("tests/fixtures/not_operator/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/not_operator");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("PREEMPT".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("ADVANCED_FEATURES".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("SCHEDULER_RT".to_string(), SymbolType::Bool);

    // Setup: ADVANCED_FEATURES=y, PREEMPT=y
    symbol_table.set_value("ADVANCED_FEATURES", "y".to_string());
    symbol_table.set_value("PREEMPT", "y".to_string());

    // Test: Should NOT be able to enable SCHEDULER_RT
    let result = resolver.can_enable("SCHEDULER_RT", &symbol_table);
    assert!(
        result.is_err(),
        "SCHEDULER_RT should be blocked when PREEMPT=y"
    );

    // Test: Error message should mention the dependency
    if let Err(err) = result {
        let error_msg = err.to_string();
        assert!(
            error_msg.contains("SCHEDULER_RT"),
            "Error message should mention SCHEDULER_RT"
        );
        assert!(
            error_msg.contains("!PREEMPT") || error_msg.contains("PREEMPT"),
            "Error message should mention PREEMPT: {}",
            error_msg
        );
    }
}

#[test]
fn test_simple_not_dependency() {
    // Test case: depends on !FEATURE_A
    let kconfig_path = PathBuf::from("tests/fixtures/not_operator/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/not_operator");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_B".to_string(), SymbolType::Bool);

    // Test 1: FEATURE_A disabled, should allow FEATURE_B
    symbol_table.set_value("FEATURE_A", "n".to_string());
    let result = resolver.can_enable("FEATURE_B", &symbol_table);
    assert!(
        result.is_ok(),
        "FEATURE_B should be allowed when FEATURE_A=n"
    );

    // Test 2: FEATURE_A enabled, should block FEATURE_B
    symbol_table.set_value("FEATURE_A", "y".to_string());
    let result = resolver.can_enable("FEATURE_B", &symbol_table);
    assert!(
        result.is_err(),
        "FEATURE_B should be blocked when FEATURE_A=y"
    );
}

#[test]
fn test_complex_not_expression() {
    // Test case: depends on !(FEATURE_A && FEATURE_B)
    let kconfig_path = PathBuf::from("tests/fixtures/not_operator/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/not_operator");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("FEATURE_A".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_B".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("FEATURE_C".to_string(), SymbolType::Bool);

    // Test 1: Both disabled - should allow (!(n && n) = !(n) = true)
    symbol_table.set_value("FEATURE_A", "n".to_string());
    symbol_table.set_value("FEATURE_B", "n".to_string());
    let result = resolver.can_enable("FEATURE_C", &symbol_table);
    assert!(
        result.is_ok(),
        "FEATURE_C should be allowed when both A and B are disabled"
    );

    // Test 2: A enabled, B disabled - should allow (!(y && n) = !(n) = true)
    symbol_table.set_value("FEATURE_A", "y".to_string());
    symbol_table.set_value("FEATURE_B", "n".to_string());
    let result = resolver.can_enable("FEATURE_C", &symbol_table);
    assert!(
        result.is_ok(),
        "FEATURE_C should be allowed when only A is enabled"
    );

    // Test 3: A disabled, B enabled - should allow (!(n && y) = !(n) = true)
    symbol_table.set_value("FEATURE_A", "n".to_string());
    symbol_table.set_value("FEATURE_B", "y".to_string());
    let result = resolver.can_enable("FEATURE_C", &symbol_table);
    assert!(
        result.is_ok(),
        "FEATURE_C should be allowed when only B is enabled"
    );

    // Test 4: Both enabled - should block (!(y && y) = !(y) = false)
    symbol_table.set_value("FEATURE_A", "y".to_string());
    symbol_table.set_value("FEATURE_B", "y".to_string());
    let result = resolver.can_enable("FEATURE_C", &symbol_table);
    assert!(
        result.is_err(),
        "FEATURE_C should be blocked when both A and B are enabled"
    );
}

#[test]
fn test_not_operator_with_and_precedence() {
    // Test the original problem case more thoroughly
    let kconfig_path = PathBuf::from("tests/fixtures/not_operator/Kconfig");
    let srctree = PathBuf::from("tests/fixtures/not_operator");

    let mut parser = Parser::new(&kconfig_path, &srctree).unwrap();
    let ast = parser.parse().unwrap();

    let mut resolver = DependencyResolver::new();
    resolver.build_from_entries(&ast.entries);

    let mut symbol_table = SymbolTable::new();
    symbol_table.add_symbol("PREEMPT".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("ADVANCED_FEATURES".to_string(), SymbolType::Bool);
    symbol_table.add_symbol("SCHEDULER_RT".to_string(), SymbolType::Bool);

    // Test all 4 combinations
    let test_cases = vec![
        ("n", "n", false), // ADVANCED=n, PREEMPT=n -> (n && !n) = (n && y) = n -> blocked
        ("n", "y", false), // ADVANCED=n, PREEMPT=y -> (n && !y) = (n && n) = n -> blocked
        ("y", "n", true),  // ADVANCED=y, PREEMPT=n -> (y && !n) = (y && y) = y -> allowed
        ("y", "y", false), // ADVANCED=y, PREEMPT=y -> (y && !y) = (y && n) = n -> blocked
    ];

    for (advanced, preempt, should_allow) in test_cases {
        symbol_table.set_value("ADVANCED_FEATURES", advanced.to_string());
        symbol_table.set_value("PREEMPT", preempt.to_string());

        let result = resolver.can_enable("SCHEDULER_RT", &symbol_table);
        if should_allow {
            assert!(
                result.is_ok(),
                "SCHEDULER_RT should be allowed when ADVANCED_FEATURES={} and PREEMPT={}. Error: {:?}",
                advanced,
                preempt,
                result.err()
            );
        } else {
            assert!(
                result.is_err(),
                "SCHEDULER_RT should be blocked when ADVANCED_FEATURES={} and PREEMPT={}",
                advanced,
                preempt
            );
        }
    }
}
