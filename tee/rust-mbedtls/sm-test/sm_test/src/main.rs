mod transformer;
mod sm3;
mod sm4;
extern crate mbedtls_sys;
use sm3::sm3_hash;
use sm4::sm4_gcm;
use crate::sm4::{sm4_de_encrypt_ctr, sm4_decrypt_cbc, sm4_encrypt_cbc};

fn main() {
    //SM3散列
    let sm3_r = sm3_hash("616263") == "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0";
    println!("SM3-Test:{sm3_r}");
    //SM4 GCM加密和解密
    let z = sm4_gcm("69EEDF3777E594C30E94E9C5E2BCE467","A3330638A809BA358D6C098E",
     "0C29FC4907119F99C492E2FA7B633F4E165BE53585ABED718BA39CAB80A06392731E5CE6E3581DCAF119037D998A0F522D680A9DCB405AADF800C0C798BAE38A",
    "AAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCDDDDDDDDDDDDDDDDEEEEEEEEEEEEEEEEFFFFFFFFFFFFFFFFEEEEEEEEEEEEEEEEAAAAAAAAAAAAAAAA",
    "FEEDFACEDEADBEEFFEEDFACEDEADBEEFABADDAD2", "197F6CC5523DA36A3B2C429244C470AA");
    println!("SM4-GCM-Test:{z}");
    //SM4 CBC加解密
    let z3 = sm4_encrypt_cbc("0123456789abcdeffedcba9876543210", "0123456789abcdeffedcba9876543210",
    "0123456789abcdeffedcba987654") == 
    "978ba5ccf768ab0f111640cc8e94e32a";
    println!("SM4-CBC-Enc-Test:{z3}");

    let z4 = sm4_decrypt_cbc("0123456789abcdeffedcba9876543210", "0123456789abcdeffedcba9876543210",
    "978ba5ccf768ab0f111640cc8e94e32a") == 
    "0123456789abcdeffedcba987654";
    println!("SM4-CBC-Dec-Test:{z4}");
    //SM4 CTR加解密
    let z5 = sm4_de_encrypt_ctr("0123456789abcdeffedcba9876543210", "000102030405060708090a0b0c0d0e0f",
    "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeeeffffffffaaaaaaaabbbbbbbb") == 
    "ac3236cb861dd316e6413b4e3c7524b781e9e3a5bf5c03fe703bb94f3abb16a1";
    println!("SM4-CTR-Enc-Test:{z5}");

    let z6 = sm4_de_encrypt_ctr("0123456789abcdeffedcba9876543210", "000102030405060708090a0b0c0d0e0f",
    "ac3236cb861dd316e6413b4e3c7524b781e9e3a5bf5c03fe703bb94f3abb16a1") == 
    "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeeeffffffffaaaaaaaabbbbbbbb";
    println!("SM4-CTR-Dec-Test:{z6}");
}

#[cfg(test)]
mod test{
    use super::*;

    //SM3散列测试
    #[test]
    fn sm3_test_1(){
        assert_eq!( sm3_hash("616263") , 
        String::from("66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"));
    }

    #[test]
    fn sm3_test_2(){
        assert_eq!( sm3_hash("68747470733a2f2f636f6e73742e6e65742e636e2f") , 
        String::from("bc028f836a92dced100b500f087d4223201ff2f60ef0bb76e84e9a5a6f9be74a"));
    }

    #[test]
    fn sm3_test_3(){
        assert_eq!( sm3_hash("3701adfea1ffbba7f574db56b7c1e17aba30c40006020d82220b501a199ed413") , 
        String::from("b3d11a006f8b066e6f1374e91fa1b2a08c148afb8258b6d6a966fe9e7c0878ea"));
    }

    //SM4 GCM加密和解密测试
    #[test]
    fn sm4_gcm_test_1(){
        assert!(sm4_gcm("69EEDF3777E594C30E94E9C5E2BCE467","A3330638A809BA358D6C098E",
     "0C29FC4907119F99C492E2FA7B633F4E165BE53585ABED718BA39CAB80A06392731E5CE6E3581DCAF119037D998A0F522D680A9DCB405AADF800C0C798BAE38A",
    "AAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCDDDDDDDDDDDDDDDDEEEEEEEEEEEEEEEEFFFFFFFFFFFFFFFFEEEEEEEEEEEEEEEEAAAAAAAAAAAAAAAA",
    "FEEDFACEDEADBEEFFEEDFACEDEADBEEFABADDAD2", "197F6CC5523DA36A3B2C429244C470AA"));
    }


    #[test]
    fn sm4_cbc_enc_test_1(){
        assert_eq!(sm4_encrypt_cbc("0123456789abcdeffedcba9876543210", "0123456789abcdeffedcba9876543210",
        "0123456789abcdeffedcba9876543210"),
        "2677f46b09c122cc975533105bd4a22a3b880e6867772522ae55d2f0ae7478ae");
    }

    #[test]
    fn sm4_cbc_enc_test_2(){
        assert_eq!(sm4_encrypt_cbc("02A127C011E8ABC02D5FB9205FB408A1", "0123456789abcdeffedcba9876543210",
        "23fcbc7bec81a1ece15469d112e8f558"),
        "1c6d6d52e4dd40c2798940a6c37aa4820f304a170a08adf4b34fb582a8189b11");
    }

    #[test]
    fn sm4_cbc_enc_test_3(){
        assert_eq!(sm4_encrypt_cbc("469d112e8f55823fcbc7bec81a1ece15", "0123456789abcdeffedcba9876543210",
        "37aa4821c6d6d52e4dd40c2798940a6c"),
        "d30dbf5f1359be9b52ce5f0d922e042717e859733a006754eb4d1d05eb910ccd");
    }

    #[test]
    fn sm4_cbc_dec_test_1(){
        assert_eq!(sm4_decrypt_cbc("0123456789abcdeffedcba9876543210", "0123456789abcdeffedcba9876543210",
        "2677f46b09c122cc975533105bd4a22a3b880e6867772522ae55d2f0ae7478ae"),
        "0123456789abcdeffedcba9876543210");
    }

    #[test]
    fn sm4_cbc_dec_test_2(){
        assert_eq!(sm4_decrypt_cbc("02A127C011E8ABC02D5FB9205FB408A1", "0123456789abcdeffedcba9876543210",
        "1c6d6d52e4dd40c2798940a6c37aa4820f304a170a08adf4b34fb582a8189b11"),
        "23fcbc7bec81a1ece15469d112e8f558");
    }

    #[test]
    fn sm4_cbc_dec_test_3(){
        assert_eq!(sm4_decrypt_cbc("469d112e8f55823fcbc7bec81a1ece15", "0123456789abcdeffedcba9876543210",
        "d30dbf5f1359be9b52ce5f0d922e042717e859733a006754eb4d1d05eb910ccd"),
        "37aa4821c6d6d52e4dd40c2798940a6c");
    }

    #[test]
    fn sm4_ctr_dec_enc_test_1(){
        assert_eq!(sm4_de_encrypt_ctr("0123456789abcdeffedcba9876543210", "000102030405060708090a0b0c0d0e0f",
        "ac3236cb861dd316e6413b4e3c7524b781e9e3a5bf5c03fe703bb94f3abb16a1"),
        "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeeeffffffffaaaaaaaabbbbbbbb");
    }

    #[test]
    fn sm4_ctr_dec_enc_test_2(){
        assert_eq!(sm4_de_encrypt_ctr("c2798940a6c37aa4821c6d6d52e4dd40", "c7bec81a1ece15469d112e8f55823fcb",
        "03bb94f3abb16a1ac3236cb861dd316e6413b4e3c7524b781e9e3a5bf5c03fe7"),
        "5d60de70ba48c9b0b9c7472370c8a79c63e0be89051e5d05ae7ae35d073cb099");
    }

    #[test]
    fn sm4_ctr_dec_enc_test_3(){
        assert_eq!(sm4_de_encrypt_ctr("c2798940a6c37aa4821c6d6d52e4dd40", "c7bec81a1ece15469d112e8f55823fcb",
        "5c03fe703bb94f3abb16cb861d6a1ac323e3a5bfd316e6413b4e3c7524b781e9"),
        "02d8b4f32a40ec90c1f2e01d0c7f8c312410afd5115af03c8baae573d64b0e97");
    }
}

