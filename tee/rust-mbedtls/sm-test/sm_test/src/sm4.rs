use crate::transformer::{bytes_to_hex, hex_to_bytes};
use mbedtls::cipher::raw::{CipherId, CipherMode};
use mbedtls::cipher::{Authenticated, Cipher, Encryption, Traditional};

fn sm4_decrypt_auth(
    key: &[u8],
    iv: &[u8],
    ad: &[u8],
    cipher_and_tag: &[u8],
    cipher_mode: CipherMode,
    tag_len: usize,
    plain_out: &mut [u8]) {
    let cipher = Cipher::<_, Authenticated, _>::new(CipherId::SM4, cipher_mode, 128).unwrap();
    let cipher = cipher.set_key_iv(key, iv).unwrap();
    cipher.decrypt_auth(ad, cipher_and_tag, plain_out, tag_len).unwrap();
}

fn sm4_encrypt_auth(
    key: &[u8],
    iv: &[u8],
    ad: &[u8],
    plain_text: &[u8],
    cipher_mode: CipherMode,
    tag_len: usize,
    cipher_and_tag_out: &mut [u8]) {
    let cipher = Cipher::<_, Authenticated, _>::new(CipherId::SM4, cipher_mode, 128).unwrap();
    let cipher = cipher.set_key_iv(key, iv).unwrap();
    cipher.encrypt_auth(ad, plain_text, cipher_and_tag_out, tag_len).unwrap();
}

fn sm4_encrypt(key: &[u8], iv: &[u8], plain_text: &[u8], cipher_mode: CipherMode, cipher_out: &mut [u8]) -> usize{
    let cipher = Cipher::<Encryption, Traditional, _>::new(CipherId::SM4, cipher_mode, 128).unwrap();
    let cipher = cipher.set_key_iv(key, iv).unwrap();
    let (len,_) = cipher.encrypt(plain_text, cipher_out).unwrap();
    len
}

fn sm4_decrypt(key: &[u8], iv: &[u8], cipher_text: &[u8], cipher_mode: CipherMode, plain_out: &mut [u8]) -> usize {
    let cipher = Cipher::<_, Traditional, _>::new(CipherId::SM4, cipher_mode, 128).unwrap();
    let cipher = cipher.set_key_iv(key, iv).unwrap();
    let (len,_) = cipher.decrypt(cipher_text, plain_out).unwrap();
    len
}

///SM4 GCM模式
/// key：加密密钥，字符串形式的十六进制数，本质是字节数组
/// iv：初始向量，字符串形式的十六进制数，本质是字节数组
/// cipher：密文，字符串形式的十六进制数，本质是字节数组
/// clear：明文，字符串形式的十六进制数，本质是字节数组
/// ad：附加数据，影响验证标签tag的生成，字符串形式的十六进制数，本质是字节数组
/// tag：验证标签，哈希值的一种，字符串形式的十六进制数，本质是字节数组
/// output：true代表成功，false代表失败
pub fn sm4_gcm(key: &str, iv: &str, cipher: &str,clear: &str, ad: &str, tag: &str) -> bool{
    //将传入的参数转换为字节数组
    let key_bytes = hex_to_bytes(key).unwrap();
    let iv_bytes = hex_to_bytes(iv).unwrap();
    let ad_bytes = hex_to_bytes(ad).unwrap();
    let clear_bytes = hex_to_bytes(clear).unwrap();
    let cipher_mode = CipherMode::GCM;
    let tag_len = 16;
    let out_len = clear_bytes.len() + tag_len;
    let mut cipher_and_tag_out = vec![0u8; out_len];
    let mut plain_out = vec![0u8; clear_bytes.len()];
    //GCM加密
    sm4_encrypt_auth(&key_bytes, &iv_bytes, &ad_bytes, &clear_bytes, cipher_mode, tag_len, &mut cipher_and_tag_out);
    if (cipher.to_string() + tag).to_uppercase() != bytes_to_hex(&cipher_and_tag_out).to_uppercase(){
        return false;
    }
    //GCM解密
    sm4_decrypt_auth(&key_bytes, &iv_bytes, &ad_bytes, &cipher_and_tag_out, cipher_mode, tag_len, &mut plain_out);
    if clear.to_uppercase() != bytes_to_hex(&plain_out).to_uppercase(){
        return false;
    }
    true
}

///SM4 CBC加密
/// key：对称密钥
/// iv：初始向量
/// src：明文
/// output：密文
pub fn sm4_encrypt_cbc(key: &str, iv:&str, src: &str) -> String{
    let result;

    let key_bytes = hex_to_bytes(key).unwrap();
    let iv_bytes = hex_to_bytes(iv).unwrap();
    let src_bytes = hex_to_bytes(src).unwrap();

    let slen = src_bytes.len();
    //PKCS#7填充
    let dlen = slen + 16;
    let cipher_mode = CipherMode::CBC;
    let mut cipher_out = vec![0u8; dlen];
    let len = sm4_encrypt(&key_bytes, &iv_bytes, &src_bytes, cipher_mode, &mut cipher_out);
    result = bytes_to_hex(&cipher_out[..len]);
    result
}

///SM4 CBC解密
/// key：对称密钥
/// iv：初始向量
/// src：密文
/// output：明文
pub fn sm4_decrypt_cbc(key: &str, iv:&str, src: &str) -> String{
    let result;

    let key_bytes = hex_to_bytes(key).unwrap();
    let iv_bytes = hex_to_bytes(iv).unwrap();
    let src_bytes = hex_to_bytes(src).unwrap();

    let cipher_mode = CipherMode::CBC;
    let slen = src_bytes.len();
    let dlen = slen+16;
    let mut cipher_out = vec![0u8; dlen];
    let len = sm4_decrypt(&key_bytes, &iv_bytes, &src_bytes, cipher_mode, &mut cipher_out);
    result = bytes_to_hex(&cipher_out[..len]);
    result
}

///SM4 CTR加解密
/// key：对称密钥
/// iv：初始向量
/// src：密文或明文
/// output：明文或密文
pub fn sm4_de_encrypt_ctr(key: &str, iv:&str, src: &str) -> String{
    let result;

    let key_bytes = hex_to_bytes(key).unwrap();
    let iv_bytes = hex_to_bytes(iv).unwrap();
    let src_bytes = hex_to_bytes(src).unwrap();

    let slen = src_bytes.len();
    //PKCS#7填充
    let dlen;
    if slen % 16 == 0{
        dlen = slen + 16;
    }
    else{
        dlen = slen - (slen % 16) + 16;
    }
    let cipher_mode = CipherMode::CTR;
    let mut cipher_out = vec![0u8; dlen];
    let len = sm4_decrypt(&key_bytes, &iv_bytes, &src_bytes, cipher_mode, &mut cipher_out);
    result = bytes_to_hex(&cipher_out[..len]);
    result
}