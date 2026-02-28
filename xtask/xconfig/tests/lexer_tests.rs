use std::path::PathBuf;
use xconfig::kconfig::{Lexer, Token};

#[test]
fn test_lexer_keywords() {
    let input = "config menuconfig choice endchoice menu endmenu".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::Config));
    assert!(matches!(lexer.next_token().unwrap(), Token::MenuConfig));
    assert!(matches!(lexer.next_token().unwrap(), Token::Choice));
    assert!(matches!(lexer.next_token().unwrap(), Token::EndChoice));
    assert!(matches!(lexer.next_token().unwrap(), Token::Menu));
    assert!(matches!(lexer.next_token().unwrap(), Token::EndMenu));
}

#[test]
fn test_lexer_operators() {
    let input = "= != < <= > >= && || !".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::Eq));
    assert!(matches!(lexer.next_token().unwrap(), Token::NotEq));
    assert!(matches!(lexer.next_token().unwrap(), Token::Less));
    assert!(matches!(lexer.next_token().unwrap(), Token::LessEq));
    assert!(matches!(lexer.next_token().unwrap(), Token::Greater));
    assert!(matches!(lexer.next_token().unwrap(), Token::GreaterEq));
    assert!(matches!(lexer.next_token().unwrap(), Token::And));
    assert!(matches!(lexer.next_token().unwrap(), Token::Or));
    assert!(matches!(lexer.next_token().unwrap(), Token::Not));
}

#[test]
fn test_lexer_string_literal() {
    let input = r#""Hello, World!""#.to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    match lexer.next_token().unwrap() {
        Token::StringLit(s) => assert_eq!(s, "Hello, World!"),
        _ => panic!("Expected StringLit"),
    }
}

#[test]
fn test_lexer_identifier() {
    let input = "MY_CONFIG test_123".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "MY_CONFIG"),
        _ => panic!("Expected Identifier"),
    }

    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "test_123"),
        _ => panic!("Expected Identifier"),
    }
}

#[test]
fn test_lexer_numbers() {
    let input = "42 0x1a".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    match lexer.next_token().unwrap() {
        Token::Number(n) => assert_eq!(n, 42),
        _ => panic!("Expected Number"),
    }

    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0x1a"), // Hex numbers now returned as identifiers
        _ => panic!("Expected Identifier for hex number"),
    }
}

#[test]
fn test_lexer_comments() {
    let input = "config # This is a comment\nTEST".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::Config));
    assert!(matches!(lexer.next_token().unwrap(), Token::Newline));
    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "TEST"),
        _ => panic!("Expected Identifier"),
    }
}

#[test]
fn test_lexer_punctuation() {
    let input = "( ) [ ] ,".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::LParen));
    assert!(matches!(lexer.next_token().unwrap(), Token::RParen));
    assert!(matches!(lexer.next_token().unwrap(), Token::LBracket));
    assert!(matches!(lexer.next_token().unwrap(), Token::RBracket));
    assert!(matches!(lexer.next_token().unwrap(), Token::Comma));
}

#[test]
fn test_lexer_array_literal() {
    let input = "[1, 2, 3]".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::LBracket));
    assert!(matches!(lexer.next_token().unwrap(), Token::Number(1)));
    assert!(matches!(lexer.next_token().unwrap(), Token::Comma));
    assert!(matches!(lexer.next_token().unwrap(), Token::Number(2)));
    assert!(matches!(lexer.next_token().unwrap(), Token::Comma));
    assert!(matches!(lexer.next_token().unwrap(), Token::Number(3)));
    assert!(matches!(lexer.next_token().unwrap(), Token::RBracket));
}

#[test]
fn test_lexer_array_hex_values() {
    let input = "[0x10, 0x20, 0x30]".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    assert!(matches!(lexer.next_token().unwrap(), Token::LBracket));
    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0x10"),
        _ => panic!("Expected Identifier"),
    }
    assert!(matches!(lexer.next_token().unwrap(), Token::Comma));
    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0x20"),
        _ => panic!("Expected Identifier"),
    }
    assert!(matches!(lexer.next_token().unwrap(), Token::Comma));
    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0x30"),
        _ => panic!("Expected Identifier"),
    }
    assert!(matches!(lexer.next_token().unwrap(), Token::RBracket));
}


#[test]
fn test_lexer_hex_with_underscores() {
    let input = "0x1000_0000 0xfe00_0000".to_string();
    let mut lexer = Lexer::new(input, PathBuf::from("test"));

    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0x1000_0000"),
        _ => panic!("Expected Identifier for hex with underscores"),
    }

    match lexer.next_token().unwrap() {
        Token::Identifier(s) => assert_eq!(s, "0xfe00_0000"),
        _ => panic!("Expected Identifier for hex with underscores"),
    }
}
