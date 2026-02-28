use crate::error::{KconfigError, Result};
use crate::kconfig::symbol::SymbolTable;

/// Evaluates a shell expression like $(if condition,then,else) or $(VAR_NAME)
pub fn evaluate_shell_expr(expr: &str, symbols: &SymbolTable) -> Result<String> {
    let trimmed = expr.trim();

    // Handle $(if ...) expressions
    if trimmed.starts_with("$(if ") && trimmed.ends_with(')') {
        return evaluate_if_expr(trimmed, symbols);
    }

    // Handle $(VAR_NAME) variable references
    if trimmed.starts_with("$(") && trimmed.ends_with(')') {
        let var_name = &trimmed[2..trimmed.len() - 1];
        let value = symbols.get_value(var_name).unwrap_or_else(|| String::new());
        return Ok(value);
    }

    // Return as-is if not a shell expression
    Ok(trimmed.to_string())
}

/// Evaluates an if expression: $(if condition,then_value,else_value)
fn evaluate_if_expr(expr: &str, symbols: &SymbolTable) -> Result<String> {
    // Remove $(if and trailing )
    let inner = &expr[5..expr.len() - 1];

    // Split by commas, but need to handle nested expressions
    let parts = split_if_parts(inner)?;

    if parts.len() != 3 {
        return Err(KconfigError::InvalidExpression(format!(
            "if expression must have 3 parts (condition, then, else), got {}",
            parts.len()
        )));
    }

    let condition = parts[0].trim();
    let then_value = parts[1].trim();
    let else_value = parts[2].trim();

    // Evaluate the condition
    let cond_result = evaluate_condition(condition, symbols)?;

    // Choose the appropriate branch and recursively evaluate it
    let result_expr = if cond_result { then_value } else { else_value };

    // Recursively evaluate the result (it might contain nested shell expressions)
    evaluate_shell_expr(result_expr, symbols)
}

/// Evaluates a condition (variable reference or constant)
fn evaluate_condition(condition: &str, symbols: &SymbolTable) -> Result<bool> {
    let trimmed = condition.trim();

    // Handle $(VAR_NAME) syntax
    if trimmed.starts_with("$(") && trimmed.ends_with(')') {
        let var_name = &trimmed[2..trimmed.len() - 1];
        return Ok(symbols.is_enabled(var_name));
    }

    // Direct variable name
    Ok(symbols.is_enabled(trimmed))
}

/// Splits if expression parts by commas, respecting nested parentheses
fn split_if_parts(input: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if escape_next {
            // Previous character was a backslash, so this character is escaped
            current.push(ch);
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => {
                // Mark that next character should be escaped
                escape_next = true;
                current.push(ch);
            }
            '"' => {
                in_string = !in_string;
                current.push(ch);
            }
            '(' if !in_string => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_string => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_string => {
                parts.push(current.trim().to_string());
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if !current.is_empty() {
        parts.push(current.trim().to_string());
    }

    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kconfig::ast::SymbolType;

    #[test]
    fn test_simple_variable_reference() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("TEST_VAR".to_string(), SymbolType::Bool);
        symbols.set_value("TEST_VAR", "myvalue".to_string());

        let result = evaluate_shell_expr("$(TEST_VAR)", &symbols).unwrap();
        assert_eq!(result, "myvalue");
    }

    #[test]
    fn test_undefined_variable() {
        let symbols = SymbolTable::new();
        let result = evaluate_shell_expr("$(UNDEFINED)", &symbols).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_simple_if_true() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("CONDITION".to_string(), SymbolType::Bool);
        symbols.set_value("CONDITION", "y".to_string());

        let result = evaluate_shell_expr("$(if $(CONDITION),yes,no)", &symbols).unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_simple_if_false() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("CONDITION".to_string(), SymbolType::Bool);
        symbols.set_value("CONDITION", "n".to_string());

        let result = evaluate_shell_expr("$(if $(CONDITION),yes,no)", &symbols).unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_nested_if_expressions() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("ARCH_AARCH64".to_string(), SymbolType::Bool);
        symbols.add_symbol("ARCH_X86_64".to_string(), SymbolType::Bool);
        symbols.set_value("ARCH_AARCH64", "n".to_string());
        symbols.set_value("ARCH_X86_64", "y".to_string());

        let expr = "$(if $(ARCH_AARCH64),aarch64,$(if $(ARCH_X86_64),x86_64,unknown))";
        let result = evaluate_shell_expr(expr, &symbols).unwrap();
        assert_eq!(result, "x86_64");
    }

    #[test]
    fn test_deeply_nested_if() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("ARCH_AARCH64".to_string(), SymbolType::Bool);
        symbols.add_symbol("ARCH_RISCV64".to_string(), SymbolType::Bool);
        symbols.add_symbol("ARCH_X86_64".to_string(), SymbolType::Bool);
        symbols.add_symbol("ARCH_LOONGARCH64".to_string(), SymbolType::Bool);

        symbols.set_value("ARCH_AARCH64", "n".to_string());
        symbols.set_value("ARCH_RISCV64", "y".to_string());
        symbols.set_value("ARCH_X86_64", "n".to_string());
        symbols.set_value("ARCH_LOONGARCH64", "n".to_string());

        let expr = "$(if $(ARCH_AARCH64),aarch64,$(if $(ARCH_RISCV64),riscv64,$(if $(ARCH_X86_64),x86_64,$(if $(ARCH_LOONGARCH64),loongarch64,unknown))))";
        let result = evaluate_shell_expr(expr, &symbols).unwrap();
        assert_eq!(result, "riscv64");
    }

    #[test]
    fn test_all_conditions_false() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("ARCH_AARCH64".to_string(), SymbolType::Bool);
        symbols.add_symbol("ARCH_X86_64".to_string(), SymbolType::Bool);

        symbols.set_value("ARCH_AARCH64", "n".to_string());
        symbols.set_value("ARCH_X86_64", "n".to_string());

        let expr = "$(if $(ARCH_AARCH64),aarch64,$(if $(ARCH_X86_64),x86_64,unknown))";
        let result = evaluate_shell_expr(expr, &symbols).unwrap();
        assert_eq!(result, "unknown");
    }

    #[test]
    fn test_escaped_quotes_in_string() {
        let mut symbols = SymbolTable::new();
        symbols.add_symbol("USE_QUOTES".to_string(), SymbolType::Bool);
        symbols.set_value("USE_QUOTES", "y".to_string());

        // Test that escaped quotes don't break the parser
        let expr = r#"$(if $(USE_QUOTES),"value with \"quotes\"",plain)"#;
        let result = evaluate_shell_expr(expr, &symbols).unwrap();
        assert_eq!(result, r#""value with \"quotes\"""#);
    }
}
