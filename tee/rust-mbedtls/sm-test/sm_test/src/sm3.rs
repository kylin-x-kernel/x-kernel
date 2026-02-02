use crate::transformer::{hex_to_bytes,bytes_to_hex};
use mbedtls::hash::Md;
use mbedtls::hash::Type::SM3;

///SM3散列函数，输入字符串形式16进制，输出字符串形式的16进制散列值
/// input:字符串形式的十六进制数，如"15A3D86E"，本质是字节数组可视化
/// output:字符串形式的十六进制数,本质是字节数组的可视化
pub fn sm3_hash(input:&str) -> String {
    //对输入的字符串操作，转换为字节数组
    let input_bytes = hex_to_bytes(input).unwrap();
    //初始化及获取裸指针
    let mut output = [0u8; 32];
    //散列操作
    Md::hash(SM3, &input_bytes, &mut output).unwrap();
    let output_hex = bytes_to_hex(&output);
    output_hex
}