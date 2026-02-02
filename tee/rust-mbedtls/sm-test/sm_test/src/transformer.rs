use std::fmt::Write;
use std::ffi::CString;

/// 将十六进制字符串转换为字节数组
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    let mut chars = hex.chars().peekable();

    while let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
        let byte = u8::from_str_radix(&format!("{}{}", c1, c2), 16)
            .map_err(|_| "无效的十六进制字符")?;
        bytes.push(byte);
    }

    // 检查是否有奇数个字符
    if chars.next().is_some() {
        return Err("十六进制字符串长度必须为偶数");
    }

    Ok(bytes)
}

/// 将字节数组转换为十六进制字符串
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut hex, "{:02x}", b).unwrap();
    }
    hex
}

//将Rust字符串转化为C字符指针，区别在于C字符串以/0结尾
pub fn to_cstr_ptr(str:&str) -> *const u8{
    let c_str = CString::new(str).expect(str);
    let ptr = c_str.as_ptr();
    std::mem::forget(c_str);
    ptr
}

pub fn split_even_str(s: &str) -> (&str, &str) {
    // 确保字符串长度是偶数
    assert!(s.len() % 2 == 0, "字符串长度必须为偶数");

    let mid = s.len() / 2;
    s.split_at(mid)
}