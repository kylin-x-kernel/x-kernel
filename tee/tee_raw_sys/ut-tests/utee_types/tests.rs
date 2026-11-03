// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

    use super::utee_params;

    /// 七种 GP 类型 nibble（0/1/2/3/5/6/7）在四个槽位上均能通过
    /// `types` 打包、`param_types()` 读回；`set_value`/`value` 不扰动类型位。
    #[def_test]
    fn test_seven_gp_param_types_roundtrip_per_slot() {
        // GP nibble：0=None 1=ValueInput 2=ValueOutput 3=ValueInout
        //            5=MemrefInput 6=MemrefOutput 7=MemrefInout
        let nibbles: [u32; 7] = [0, 1, 2, 3, 5, 6, 7];
        for nib in nibbles {
            for slot in 0..4usize {
                let mut p = utee_params {
                    types: (nib as u64) << (slot * 4),
                    vals: [0; 8],
                };
                p.set_value(slot, 0x1122_3344_5566_7788, slot as u64 + 1);
                assert_eq!(
                    (p.param_types() >> (slot * 4)) & 0xF,
                    nib,
                    "nibble {nib} 在槽 {slot} 打包读回不一致"
                );
                // 其他槽保持零：位拆装不串槽。
                for other in 0..4usize {
                    if other != slot {
                        assert_eq!((p.param_types() >> (other * 4)) & 0xF, 0);
                        assert_eq!(p.value(other), (0, 0));
                    }
                }
                assert_eq!(p.value(slot), (0x1122_3344_5566_7788, slot as u64 + 1));
            }
        }
    }

    /// 四个槽装下互不相同的四种类型 + 全部 VALUE 槽独立读写的组合用例；
    /// 全零默认值即四个 NONE。
    #[def_test]
    fn test_packed_types_and_value_slots() {
        let mut p = utee_params {
            types: 0,
            vals: [0; 8],
        };
        assert_eq!(p.param_types(), 0, "默认值应为四个 NONE");
        // 槽 0..3 依次 None / ValueInput / ValueOutput / ValueInout。
        p.types = 0x3210;
        p.set_value(0, 0, 0);
        p.set_value(1, u32::MAX as u64, u32::MAX as u64);
        p.set_value(2, 1, 2);
        p.set_value(3, 0xFFFF_FFFF_FFFF_FFFF, 42);
        assert_eq!(p.param_types(), 0x3210);
        assert_eq!(p.value(1), (u32::MAX as u64, u32::MAX as u64));
        assert_eq!(p.value(2), (1, 2));
        assert_eq!(p.value(3), (0xFFFF_FFFF_FFFF_FFFF, 42));

        // set_value 只改 vals：类型位不受影响。
        p.set_value(2, 9, 9);
        assert_eq!(p.param_types(), 0x3210);
        assert_eq!(p.value(2), (9, 9));
    }