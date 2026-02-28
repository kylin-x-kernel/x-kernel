use crate::error::{KconfigError, Result};
use crate::kconfig::ast::Expr;
use crate::kconfig::shell_expr::evaluate_shell_expr;
use crate::kconfig::symbol::SymbolTable;

pub fn evaluate_expr(expr: &Expr, symbols: &SymbolTable) -> Result<bool> {
    match expr {
        Expr::Symbol(name) => {
            // Check if symbol is defined and enabled
            Ok(symbols.is_enabled(name))
        }
        Expr::Const(val) => {
            // "y", "m", "n" constants
            Ok(val == "y" || val == "m")
        }
        Expr::ShellExpr(shell_expr) => {
            // Evaluate shell expression and check if result is truthy
            let result = evaluate_shell_expr(shell_expr, symbols)?;
            Ok(!result.is_empty() && result != "n" && result != "0")
        }
        Expr::Not(inner) => Ok(!evaluate_expr(inner, symbols)?),
        Expr::And(left, right) => {
            Ok(evaluate_expr(left, symbols)? && evaluate_expr(right, symbols)?)
        }
        Expr::Or(left, right) => {
            Ok(evaluate_expr(left, symbols)? || evaluate_expr(right, symbols)?)
        }
        Expr::Equal(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(left_val == right_val)
        }
        Expr::NotEqual(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(left_val != right_val)
        }
        Expr::Less(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(compare_values(&left_val, &right_val)? < 0)
        }
        Expr::LessEqual(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(compare_values(&left_val, &right_val)? <= 0)
        }
        Expr::Greater(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(compare_values(&left_val, &right_val)? > 0)
        }
        Expr::GreaterEqual(left, right) => {
            let left_val = get_expr_value(left, symbols)?;
            let right_val = get_expr_value(right, symbols)?;
            Ok(compare_values(&left_val, &right_val)? >= 0)
        }
    }
}

fn get_expr_value(expr: &Expr, symbols: &SymbolTable) -> Result<String> {
    match expr {
        Expr::Symbol(name) => Ok(symbols.get_value(name).unwrap_or_else(|| "n".to_string())),
        Expr::Const(val) => Ok(val.clone()),
        Expr::ShellExpr(shell_expr) => evaluate_shell_expr(shell_expr, symbols),
        _ => Err(KconfigError::InvalidExpression(
            "Complex expression in comparison".to_string(),
        )),
    }
}

fn compare_values(left: &str, right: &str) -> Result<i32> {
    // Try to parse as numbers
    if let (Ok(l), Ok(r)) = (left.parse::<i64>(), right.parse::<i64>()) {
        return Ok((l - r).signum() as i32);
    }

    // String comparison
    Ok(left.cmp(right) as i32)
}
