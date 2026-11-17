// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! BPF uapi constants, generated from Linux's `include/uapi/linux/bpf.h`.
//!
//! DO NOT EDIT BY HAND. Regenerate with
//! `python3 xtask/bpf_uapi/gen.py --linux <linux-tree>` and review the diff:
//! every changed value is an ABI decision. Source header digest: `6e224aea138a265c`.
//!
//! `bpf_uapi_constants_match_linux()` in the tested module pins the values
//! this kernel depends on, independently of the generator.

#![expect(
    non_upper_case_globals,
    reason = "generated uapi table keeps the C spellings so a grep for a uapi name"
)]

/// C enum `bpf_anon_bpf_reg_0` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_reg_0 {
    pub const BPF_REG_0: u32 = 0;
    pub const BPF_REG_1: u32 = 1;
    pub const BPF_REG_2: u32 = 2;
    pub const BPF_REG_3: u32 = 3;
    pub const BPF_REG_4: u32 = 4;
    pub const BPF_REG_5: u32 = 5;
    pub const BPF_REG_6: u32 = 6;
    pub const BPF_REG_7: u32 = 7;
    pub const BPF_REG_8: u32 = 8;
    pub const BPF_REG_9: u32 = 9;
    pub const BPF_REG_10: u32 = 10;
    pub const __MAX_BPF_REG: u32 = 11;
}

/// C enum `bpf_cgroup_iter_order` (`include/uapi/linux/bpf.h`).
pub mod bpf_cgroup_iter_order {
    pub const BPF_CGROUP_ITER_ORDER_UNSPEC: u32 = 0;
    pub const BPF_CGROUP_ITER_SELF_ONLY: u32 = 1;
    pub const BPF_CGROUP_ITER_DESCENDANTS_PRE: u32 = 2;
    pub const BPF_CGROUP_ITER_DESCENDANTS_POST: u32 = 3;
    pub const BPF_CGROUP_ITER_ANCESTORS_UP: u32 = 4;
}

/// C enum `bpf_cmd` (`include/uapi/linux/bpf.h`).
pub mod bpf_cmd {
    pub const BPF_MAP_CREATE: u32 = 0;
    pub const BPF_MAP_LOOKUP_ELEM: u32 = 1;
    pub const BPF_MAP_UPDATE_ELEM: u32 = 2;
    pub const BPF_MAP_DELETE_ELEM: u32 = 3;
    pub const BPF_MAP_GET_NEXT_KEY: u32 = 4;
    pub const BPF_PROG_LOAD: u32 = 5;
    pub const BPF_OBJ_PIN: u32 = 6;
    pub const BPF_OBJ_GET: u32 = 7;
    pub const BPF_PROG_ATTACH: u32 = 8;
    pub const BPF_PROG_DETACH: u32 = 9;
    pub const BPF_PROG_TEST_RUN: u32 = 10;
    pub const BPF_PROG_GET_NEXT_ID: u32 = 11;
    pub const BPF_MAP_GET_NEXT_ID: u32 = 12;
    pub const BPF_PROG_GET_FD_BY_ID: u32 = 13;
    pub const BPF_MAP_GET_FD_BY_ID: u32 = 14;
    pub const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
    pub const BPF_PROG_QUERY: u32 = 16;
    pub const BPF_RAW_TRACEPOINT_OPEN: u32 = 17;
    pub const BPF_BTF_LOAD: u32 = 18;
    pub const BPF_BTF_GET_FD_BY_ID: u32 = 19;
    pub const BPF_TASK_FD_QUERY: u32 = 20;
    pub const BPF_MAP_LOOKUP_AND_DELETE_ELEM: u32 = 21;
    pub const BPF_MAP_FREEZE: u32 = 22;
    pub const BPF_BTF_GET_NEXT_ID: u32 = 23;
    pub const BPF_MAP_LOOKUP_BATCH: u32 = 24;
    pub const BPF_MAP_LOOKUP_AND_DELETE_BATCH: u32 = 25;
    pub const BPF_MAP_UPDATE_BATCH: u32 = 26;
    pub const BPF_MAP_DELETE_BATCH: u32 = 27;
    pub const BPF_LINK_CREATE: u32 = 28;
    pub const BPF_LINK_UPDATE: u32 = 29;
    pub const BPF_LINK_GET_FD_BY_ID: u32 = 30;
    pub const BPF_LINK_GET_NEXT_ID: u32 = 31;
    pub const BPF_ENABLE_STATS: u32 = 32;
    pub const BPF_ITER_CREATE: u32 = 33;
    pub const BPF_LINK_DETACH: u32 = 34;
    pub const BPF_PROG_BIND_MAP: u32 = 35;
}

/// C enum `bpf_map_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_map_type {
    pub const BPF_MAP_TYPE_UNSPEC: u32 = 0;
    pub const BPF_MAP_TYPE_HASH: u32 = 1;
    pub const BPF_MAP_TYPE_ARRAY: u32 = 2;
    pub const BPF_MAP_TYPE_PROG_ARRAY: u32 = 3;
    pub const BPF_MAP_TYPE_PERF_EVENT_ARRAY: u32 = 4;
    pub const BPF_MAP_TYPE_PERCPU_HASH: u32 = 5;
    pub const BPF_MAP_TYPE_PERCPU_ARRAY: u32 = 6;
    pub const BPF_MAP_TYPE_STACK_TRACE: u32 = 7;
    pub const BPF_MAP_TYPE_CGROUP_ARRAY: u32 = 8;
    pub const BPF_MAP_TYPE_LRU_HASH: u32 = 9;
    pub const BPF_MAP_TYPE_LRU_PERCPU_HASH: u32 = 10;
    pub const BPF_MAP_TYPE_LPM_TRIE: u32 = 11;
    pub const BPF_MAP_TYPE_ARRAY_OF_MAPS: u32 = 12;
    pub const BPF_MAP_TYPE_HASH_OF_MAPS: u32 = 13;
    pub const BPF_MAP_TYPE_DEVMAP: u32 = 14;
    pub const BPF_MAP_TYPE_SOCKMAP: u32 = 15;
    pub const BPF_MAP_TYPE_CPUMAP: u32 = 16;
    pub const BPF_MAP_TYPE_XSKMAP: u32 = 17;
    pub const BPF_MAP_TYPE_SOCKHASH: u32 = 18;
    pub const BPF_MAP_TYPE_CGROUP_STORAGE_DEPRECATED: u32 = 19;
    pub const BPF_MAP_TYPE_REUSEPORT_SOCKARRAY: u32 = 20;
    pub const BPF_MAP_TYPE_PERCPU_CGROUP_STORAGE_DEPRECATED: u32 = 21;
    pub const BPF_MAP_TYPE_QUEUE: u32 = 22;
    pub const BPF_MAP_TYPE_STACK: u32 = 23;
    pub const BPF_MAP_TYPE_SK_STORAGE: u32 = 24;
    pub const BPF_MAP_TYPE_DEVMAP_HASH: u32 = 25;
    pub const BPF_MAP_TYPE_STRUCT_OPS: u32 = 26;
    pub const BPF_MAP_TYPE_RINGBUF: u32 = 27;
    pub const BPF_MAP_TYPE_INODE_STORAGE: u32 = 28;
    pub const BPF_MAP_TYPE_TASK_STORAGE: u32 = 29;
    pub const BPF_MAP_TYPE_BLOOM_FILTER: u32 = 30;
    pub const BPF_MAP_TYPE_USER_RINGBUF: u32 = 31;
    pub const BPF_MAP_TYPE_CGRP_STORAGE: u32 = 32;
}

/// C enum `bpf_prog_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_prog_type {
    pub const BPF_PROG_TYPE_UNSPEC: u32 = 0;
    pub const BPF_PROG_TYPE_SOCKET_FILTER: u32 = 1;
    pub const BPF_PROG_TYPE_KPROBE: u32 = 2;
    pub const BPF_PROG_TYPE_SCHED_CLS: u32 = 3;
    pub const BPF_PROG_TYPE_SCHED_ACT: u32 = 4;
    pub const BPF_PROG_TYPE_TRACEPOINT: u32 = 5;
    pub const BPF_PROG_TYPE_XDP: u32 = 6;
    pub const BPF_PROG_TYPE_PERF_EVENT: u32 = 7;
    pub const BPF_PROG_TYPE_CGROUP_SKB: u32 = 8;
    pub const BPF_PROG_TYPE_CGROUP_SOCK: u32 = 9;
    pub const BPF_PROG_TYPE_LWT_IN: u32 = 10;
    pub const BPF_PROG_TYPE_LWT_OUT: u32 = 11;
    pub const BPF_PROG_TYPE_LWT_XMIT: u32 = 12;
    pub const BPF_PROG_TYPE_SOCK_OPS: u32 = 13;
    pub const BPF_PROG_TYPE_SK_SKB: u32 = 14;
    pub const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
    pub const BPF_PROG_TYPE_SK_MSG: u32 = 16;
    pub const BPF_PROG_TYPE_RAW_TRACEPOINT: u32 = 17;
    pub const BPF_PROG_TYPE_CGROUP_SOCK_ADDR: u32 = 18;
    pub const BPF_PROG_TYPE_LWT_SEG6LOCAL: u32 = 19;
    pub const BPF_PROG_TYPE_LIRC_MODE2: u32 = 20;
    pub const BPF_PROG_TYPE_SK_REUSEPORT: u32 = 21;
    pub const BPF_PROG_TYPE_FLOW_DISSECTOR: u32 = 22;
    pub const BPF_PROG_TYPE_CGROUP_SYSCTL: u32 = 23;
    pub const BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE: u32 = 24;
    pub const BPF_PROG_TYPE_CGROUP_SOCKOPT: u32 = 25;
    pub const BPF_PROG_TYPE_TRACING: u32 = 26;
    pub const BPF_PROG_TYPE_STRUCT_OPS: u32 = 27;
    pub const BPF_PROG_TYPE_EXT: u32 = 28;
    pub const BPF_PROG_TYPE_LSM: u32 = 29;
    pub const BPF_PROG_TYPE_SK_LOOKUP: u32 = 30;
    pub const BPF_PROG_TYPE_SYSCALL: u32 = 31;
    pub const BPF_PROG_TYPE_NETFILTER: u32 = 32;
}

/// C enum `bpf_attach_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_attach_type {
    pub const BPF_CGROUP_INET_INGRESS: u32 = 0;
    pub const BPF_CGROUP_INET_EGRESS: u32 = 1;
    pub const BPF_CGROUP_INET_SOCK_CREATE: u32 = 2;
    pub const BPF_CGROUP_SOCK_OPS: u32 = 3;
    pub const BPF_SK_SKB_STREAM_PARSER: u32 = 4;
    pub const BPF_SK_SKB_STREAM_VERDICT: u32 = 5;
    pub const BPF_CGROUP_DEVICE: u32 = 6;
    pub const BPF_SK_MSG_VERDICT: u32 = 7;
    pub const BPF_CGROUP_INET4_BIND: u32 = 8;
    pub const BPF_CGROUP_INET6_BIND: u32 = 9;
    pub const BPF_CGROUP_INET4_CONNECT: u32 = 10;
    pub const BPF_CGROUP_INET6_CONNECT: u32 = 11;
    pub const BPF_CGROUP_INET4_POST_BIND: u32 = 12;
    pub const BPF_CGROUP_INET6_POST_BIND: u32 = 13;
    pub const BPF_CGROUP_UDP4_SENDMSG: u32 = 14;
    pub const BPF_CGROUP_UDP6_SENDMSG: u32 = 15;
    pub const BPF_LIRC_MODE2: u32 = 16;
    pub const BPF_FLOW_DISSECTOR: u32 = 17;
    pub const BPF_CGROUP_SYSCTL: u32 = 18;
    pub const BPF_CGROUP_UDP4_RECVMSG: u32 = 19;
    pub const BPF_CGROUP_UDP6_RECVMSG: u32 = 20;
    pub const BPF_CGROUP_GETSOCKOPT: u32 = 21;
    pub const BPF_CGROUP_SETSOCKOPT: u32 = 22;
    pub const BPF_TRACE_RAW_TP: u32 = 23;
    pub const BPF_TRACE_FENTRY: u32 = 24;
    pub const BPF_TRACE_FEXIT: u32 = 25;
    pub const BPF_MODIFY_RETURN: u32 = 26;
    pub const BPF_LSM_MAC: u32 = 27;
    pub const BPF_TRACE_ITER: u32 = 28;
    pub const BPF_CGROUP_INET4_GETPEERNAME: u32 = 29;
    pub const BPF_CGROUP_INET6_GETPEERNAME: u32 = 30;
    pub const BPF_CGROUP_INET4_GETSOCKNAME: u32 = 31;
    pub const BPF_CGROUP_INET6_GETSOCKNAME: u32 = 32;
    pub const BPF_XDP_DEVMAP: u32 = 33;
    pub const BPF_CGROUP_INET_SOCK_RELEASE: u32 = 34;
    pub const BPF_XDP_CPUMAP: u32 = 35;
    pub const BPF_SK_LOOKUP: u32 = 36;
    pub const BPF_XDP: u32 = 37;
    pub const BPF_SK_SKB_VERDICT: u32 = 38;
    pub const BPF_SK_REUSEPORT_SELECT: u32 = 39;
    pub const BPF_SK_REUSEPORT_SELECT_OR_MIGRATE: u32 = 40;
    pub const BPF_PERF_EVENT: u32 = 41;
    pub const BPF_TRACE_KPROBE_MULTI: u32 = 42;
    pub const BPF_LSM_CGROUP: u32 = 43;
    pub const BPF_STRUCT_OPS: u32 = 44;
    pub const BPF_NETFILTER: u32 = 45;
    pub const BPF_TCX_INGRESS: u32 = 46;
    pub const BPF_TCX_EGRESS: u32 = 47;
    pub const BPF_TRACE_UPROBE_MULTI: u32 = 48;
    pub const BPF_CGROUP_UNIX_CONNECT: u32 = 49;
    pub const BPF_CGROUP_UNIX_SENDMSG: u32 = 50;
    pub const BPF_CGROUP_UNIX_RECVMSG: u32 = 51;
    pub const BPF_CGROUP_UNIX_GETPEERNAME: u32 = 52;
    pub const BPF_CGROUP_UNIX_GETSOCKNAME: u32 = 53;
    pub const BPF_NETKIT_PRIMARY: u32 = 54;
    pub const BPF_NETKIT_PEER: u32 = 55;
    pub const __MAX_BPF_ATTACH_TYPE: u32 = 56;
}

/// C enum `bpf_link_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_link_type {
    pub const BPF_LINK_TYPE_UNSPEC: u32 = 0;
    pub const BPF_LINK_TYPE_RAW_TRACEPOINT: u32 = 1;
    pub const BPF_LINK_TYPE_TRACING: u32 = 2;
    pub const BPF_LINK_TYPE_CGROUP: u32 = 3;
    pub const BPF_LINK_TYPE_ITER: u32 = 4;
    pub const BPF_LINK_TYPE_NETNS: u32 = 5;
    pub const BPF_LINK_TYPE_XDP: u32 = 6;
    pub const BPF_LINK_TYPE_PERF_EVENT: u32 = 7;
    pub const BPF_LINK_TYPE_KPROBE_MULTI: u32 = 8;
    pub const BPF_LINK_TYPE_STRUCT_OPS: u32 = 9;
    pub const BPF_LINK_TYPE_NETFILTER: u32 = 10;
    pub const BPF_LINK_TYPE_TCX: u32 = 11;
    pub const BPF_LINK_TYPE_UPROBE_MULTI: u32 = 12;
    pub const BPF_LINK_TYPE_NETKIT: u32 = 13;
    pub const __MAX_BPF_LINK_TYPE: u32 = 14;
}

/// C enum `bpf_perf_event_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_perf_event_type {
    pub const BPF_PERF_EVENT_UNSPEC: u32 = 0;
    pub const BPF_PERF_EVENT_UPROBE: u32 = 1;
    pub const BPF_PERF_EVENT_URETPROBE: u32 = 2;
    pub const BPF_PERF_EVENT_KPROBE: u32 = 3;
    pub const BPF_PERF_EVENT_KRETPROBE: u32 = 4;
    pub const BPF_PERF_EVENT_TRACEPOINT: u32 = 5;
    pub const BPF_PERF_EVENT_EVENT: u32 = 6;
}

/// C enum `bpf_anon_bpf_f_kprobe_multi_return` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_kprobe_multi_return {
    pub const BPF_F_KPROBE_MULTI_RETURN: u32 = 1;
}

/// C enum `bpf_anon_bpf_f_uprobe_multi_return` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_uprobe_multi_return {
    pub const BPF_F_UPROBE_MULTI_RETURN: u32 = 1;
}

/// C enum `bpf_map_update_flags` (`include/uapi/linux/bpf.h`).
pub mod bpf_map_update_flags {
    pub const BPF_ANY: u32 = 0;
    pub const BPF_NOEXIST: u32 = 1;
    pub const BPF_EXIST: u32 = 2;
    pub const BPF_F_LOCK: u32 = 4;
}

/// C enum `bpf_anon_bpf_f_no_prealloc` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_no_prealloc {
    pub const BPF_F_NO_PREALLOC: u32 = 1;
    pub const BPF_F_NO_COMMON_LRU: u32 = 2;
    pub const BPF_F_NUMA_NODE: u32 = 4;
    pub const BPF_F_RDONLY: u32 = 8;
    pub const BPF_F_WRONLY: u32 = 16;
    pub const BPF_F_STACK_BUILD_ID: u32 = 32;
    pub const BPF_F_ZERO_SEED: u32 = 64;
    pub const BPF_F_RDONLY_PROG: u32 = 128;
    pub const BPF_F_WRONLY_PROG: u32 = 256;
    pub const BPF_F_CLONE: u32 = 512;
    pub const BPF_F_MMAPABLE: u32 = 1024;
    pub const BPF_F_PRESERVE_ELEMS: u32 = 2048;
    pub const BPF_F_INNER_MAP: u32 = 4096;
    pub const BPF_F_LINK: u32 = 8192;
    pub const BPF_F_PATH_FD: u32 = 16384;
}

/// C enum `bpf_stats_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_stats_type {
    pub const BPF_STATS_RUN_TIME: u32 = 0;
}

/// C enum `bpf_stack_build_id_status` (`include/uapi/linux/bpf.h`).
pub mod bpf_stack_build_id_status {
    pub const BPF_STACK_BUILD_ID_EMPTY: u32 = 0;
    pub const BPF_STACK_BUILD_ID_VALID: u32 = 1;
    pub const BPF_STACK_BUILD_ID_IP: u32 = 2;
}

/// C enum `bpf_anon_bpf_f_recompute_csum` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_recompute_csum {
    pub const BPF_F_RECOMPUTE_CSUM: u32 = 1;
    pub const BPF_F_INVALIDATE_HASH: u32 = 2;
}

/// C enum `bpf_anon_bpf_f_hdr_field_mask` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_hdr_field_mask {
    pub const BPF_F_HDR_FIELD_MASK: u32 = 15;
}

/// C enum `bpf_anon_bpf_f_pseudo_hdr` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_pseudo_hdr {
    pub const BPF_F_PSEUDO_HDR: u32 = 16;
    pub const BPF_F_MARK_MANGLED_0: u32 = 32;
    pub const BPF_F_MARK_ENFORCE: u32 = 64;
}

/// C enum `bpf_anon_bpf_f_ingress` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_ingress {
    pub const BPF_F_INGRESS: u32 = 1;
}

/// C enum `bpf_anon_bpf_f_tuninfo_ipv6` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_tuninfo_ipv6 {
    pub const BPF_F_TUNINFO_IPV6: u32 = 1;
}

/// C enum `bpf_anon_bpf_f_skip_field_mask` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_skip_field_mask {
    pub const BPF_F_SKIP_FIELD_MASK: u32 = 255;
    pub const BPF_F_USER_STACK: u32 = 256;
    pub const BPF_F_FAST_STACK_CMP: u32 = 512;
    pub const BPF_F_REUSE_STACKID: u32 = 1024;
    pub const BPF_F_USER_BUILD_ID: u32 = 2048;
}

/// C enum `bpf_anon_bpf_f_zero_csum_tx` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_zero_csum_tx {
    pub const BPF_F_ZERO_CSUM_TX: u32 = 2;
    pub const BPF_F_DONT_FRAGMENT: u32 = 4;
    pub const BPF_F_SEQ_NUMBER: u32 = 8;
    pub const BPF_F_NO_TUNNEL_KEY: u32 = 16;
}

/// C enum `bpf_anon_bpf_f_tuninfo_flags` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_tuninfo_flags {
    pub const BPF_F_TUNINFO_FLAGS: u32 = 16;
}

/// C enum `bpf_anon_bpf_f_index_mask` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_index_mask {
    pub const BPF_F_INDEX_MASK: u32 = 4294967295;
    pub const BPF_F_CURRENT_CPU: u32 = 4294967295;
    pub const BPF_F_CTXLEN_MASK: u64 = 4503595332403200;
}

/// C enum `bpf_anon_bpf_csum_level_query` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_csum_level_query {
    pub const BPF_CSUM_LEVEL_QUERY: u32 = 0;
    pub const BPF_CSUM_LEVEL_INC: u32 = 1;
    pub const BPF_CSUM_LEVEL_DEC: u32 = 2;
    pub const BPF_CSUM_LEVEL_RESET: u32 = 3;
}

/// C enum `bpf_anon_bpf_f_adj_room_fixed_gso` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_adj_room_fixed_gso {
    pub const BPF_F_ADJ_ROOM_FIXED_GSO: u32 = 1;
    pub const BPF_F_ADJ_ROOM_ENCAP_L3_IPV4: u32 = 2;
    pub const BPF_F_ADJ_ROOM_ENCAP_L3_IPV6: u32 = 4;
    pub const BPF_F_ADJ_ROOM_ENCAP_L4_GRE: u32 = 8;
    pub const BPF_F_ADJ_ROOM_ENCAP_L4_UDP: u32 = 16;
    pub const BPF_F_ADJ_ROOM_NO_CSUM_RESET: u32 = 32;
    pub const BPF_F_ADJ_ROOM_ENCAP_L2_ETH: u32 = 64;
    pub const BPF_F_ADJ_ROOM_DECAP_L3_IPV4: u32 = 128;
    pub const BPF_F_ADJ_ROOM_DECAP_L3_IPV6: u32 = 256;
}

/// C enum `bpf_anon_bpf_adj_room_encap_l2_mask` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_adj_room_encap_l2_mask {
    pub const BPF_ADJ_ROOM_ENCAP_L2_MASK: u32 = 255;
    pub const BPF_ADJ_ROOM_ENCAP_L2_SHIFT: u32 = 56;
}

/// C enum `bpf_anon_bpf_f_sysctl_base_name` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_sysctl_base_name {
    pub const BPF_F_SYSCTL_BASE_NAME: u32 = 1;
}

/// C enum `bpf_anon_bpf_local_storage_get_f_create` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_local_storage_get_f_create {
    pub const BPF_LOCAL_STORAGE_GET_F_CREATE: u32 = 1;
    pub const BPF_SK_STORAGE_GET_F_CREATE: u32 = 1;
}

/// C enum `bpf_anon_bpf_f_get_branch_records_size` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_get_branch_records_size {
    pub const BPF_F_GET_BRANCH_RECORDS_SIZE: u32 = 1;
}

/// C enum `bpf_anon_bpf_rb_no_wakeup` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_rb_no_wakeup {
    pub const BPF_RB_NO_WAKEUP: u32 = 1;
    pub const BPF_RB_FORCE_WAKEUP: u32 = 2;
}

/// C enum `bpf_anon_bpf_rb_avail_data` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_rb_avail_data {
    pub const BPF_RB_AVAIL_DATA: u32 = 0;
    pub const BPF_RB_RING_SIZE: u32 = 1;
    pub const BPF_RB_CONS_POS: u32 = 2;
    pub const BPF_RB_PROD_POS: u32 = 3;
}

/// C enum `bpf_anon_bpf_ringbuf_busy_bit` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_ringbuf_busy_bit {
    pub const BPF_RINGBUF_BUSY_BIT: u32 = 2147483648;
    pub const BPF_RINGBUF_DISCARD_BIT: u32 = 1073741824;
    pub const BPF_RINGBUF_HDR_SZ: u32 = 8;
}

/// C enum `bpf_anon_bpf_sk_lookup_f_replace` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_sk_lookup_f_replace {
    pub const BPF_SK_LOOKUP_F_REPLACE: u32 = 1;
    pub const BPF_SK_LOOKUP_F_NO_REUSEPORT: u32 = 2;
}

/// C enum `bpf_adj_room_mode` (`include/uapi/linux/bpf.h`).
pub mod bpf_adj_room_mode {
    pub const BPF_ADJ_ROOM_NET: u32 = 0;
    pub const BPF_ADJ_ROOM_MAC: u32 = 1;
}

/// C enum `bpf_hdr_start_off` (`include/uapi/linux/bpf.h`).
pub mod bpf_hdr_start_off {
    pub const BPF_HDR_START_MAC: u32 = 0;
    pub const BPF_HDR_START_NET: u32 = 1;
}

/// C enum `bpf_lwt_encap_mode` (`include/uapi/linux/bpf.h`).
pub mod bpf_lwt_encap_mode {
    pub const BPF_LWT_ENCAP_SEG6: u32 = 0;
    pub const BPF_LWT_ENCAP_SEG6_INLINE: u32 = 1;
    pub const BPF_LWT_ENCAP_IP: u32 = 2;
}

/// C enum `bpf_anon_bpf_f_bprm_secureexec` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_bprm_secureexec {
    pub const BPF_F_BPRM_SECUREEXEC: u32 = 1;
}

/// C enum `bpf_anon_bpf_f_broadcast` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_broadcast {
    pub const BPF_F_BROADCAST: u32 = 8;
    pub const BPF_F_EXCLUDE_INGRESS: u32 = 16;
}

/// C enum `bpf_anon_bpf_skb_tstamp_unspec` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_skb_tstamp_unspec {
    pub const BPF_SKB_TSTAMP_UNSPEC: u32 = 0;
    pub const BPF_SKB_TSTAMP_DELIVERY_MONO: u32 = 1;
}

/// C enum `bpf_ret_code` (`include/uapi/linux/bpf.h`).
pub mod bpf_ret_code {
    pub const BPF_OK: u32 = 0;
    pub const BPF_DROP: u32 = 2;
    pub const BPF_REDIRECT: u32 = 7;
    pub const BPF_LWT_REROUTE: u32 = 128;
    pub const BPF_FLOW_DISSECTOR_CONTINUE: u32 = 129;
}

/// C enum `tcx_action_base` (`include/uapi/linux/bpf.h`).
pub mod tcx_action_base {
    pub const TCX_NEXT: i32 = -1;
    pub const TCX_PASS: u32 = 0;
    pub const TCX_DROP: u32 = 2;
    pub const TCX_REDIRECT: u32 = 7;
}

/// C enum `xdp_action` (`include/uapi/linux/bpf.h`).
pub mod xdp_action {
    pub const XDP_ABORTED: u32 = 0;
    pub const XDP_DROP: u32 = 1;
    pub const XDP_PASS: u32 = 2;
    pub const XDP_TX: u32 = 3;
    pub const XDP_REDIRECT: u32 = 4;
}

/// C enum `sk_action` (`include/uapi/linux/bpf.h`).
pub mod sk_action {
    pub const SK_DROP: u32 = 0;
    pub const SK_PASS: u32 = 1;
}

/// C enum `bpf_anon_bpf_sock_ops_rto_cb_flag` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_sock_ops_rto_cb_flag {
    pub const BPF_SOCK_OPS_RTO_CB_FLAG: u32 = 1;
    pub const BPF_SOCK_OPS_RETRANS_CB_FLAG: u32 = 2;
    pub const BPF_SOCK_OPS_STATE_CB_FLAG: u32 = 4;
    pub const BPF_SOCK_OPS_RTT_CB_FLAG: u32 = 8;
    pub const BPF_SOCK_OPS_PARSE_ALL_HDR_OPT_CB_FLAG: u32 = 16;
    pub const BPF_SOCK_OPS_PARSE_UNKNOWN_HDR_OPT_CB_FLAG: u32 = 32;
    pub const BPF_SOCK_OPS_WRITE_HDR_OPT_CB_FLAG: u32 = 64;
    pub const BPF_SOCK_OPS_ALL_CB_FLAGS: u32 = 127;
}

/// C enum `bpf_anon_bpf_sock_ops_void` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_sock_ops_void {
    pub const BPF_SOCK_OPS_VOID: u32 = 0;
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        unitless_time_integer,
        reason = "CG-011-time-unit-names: Linux UAPI name `BPF_SOCK_OPS_TIMEOUT_INIT` from \
                  `include/uapi/linux/bpf.h`"
    )]
    pub const BPF_SOCK_OPS_TIMEOUT_INIT: u32 = 1;
    pub const BPF_SOCK_OPS_RWND_INIT: u32 = 2;
    pub const BPF_SOCK_OPS_TCP_CONNECT_CB: u32 = 3;
    pub const BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB: u32 = 4;
    pub const BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB: u32 = 5;
    pub const BPF_SOCK_OPS_NEEDS_ECN: u32 = 6;
    pub const BPF_SOCK_OPS_BASE_RTT: u32 = 7;
    pub const BPF_SOCK_OPS_RTO_CB: u32 = 8;
    pub const BPF_SOCK_OPS_RETRANS_CB: u32 = 9;
    pub const BPF_SOCK_OPS_STATE_CB: u32 = 10;
    pub const BPF_SOCK_OPS_TCP_LISTEN_CB: u32 = 11;
    pub const BPF_SOCK_OPS_RTT_CB: u32 = 12;
    pub const BPF_SOCK_OPS_PARSE_HDR_OPT_CB: u32 = 13;
    pub const BPF_SOCK_OPS_HDR_OPT_LEN_CB: u32 = 14;
    pub const BPF_SOCK_OPS_WRITE_HDR_OPT_CB: u32 = 15;
}

/// C enum `bpf_anon_bpf_tcp_established` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_tcp_established {
    pub const BPF_TCP_ESTABLISHED: u32 = 1;
    pub const BPF_TCP_SYN_SENT: u32 = 2;
    pub const BPF_TCP_SYN_RECV: u32 = 3;
    pub const BPF_TCP_FIN_WAIT1: u32 = 4;
    pub const BPF_TCP_FIN_WAIT2: u32 = 5;
    pub const BPF_TCP_TIME_WAIT: u32 = 6;
    pub const BPF_TCP_CLOSE: u32 = 7;
    pub const BPF_TCP_CLOSE_WAIT: u32 = 8;
    pub const BPF_TCP_LAST_ACK: u32 = 9;
    pub const BPF_TCP_LISTEN: u32 = 10;
    pub const BPF_TCP_CLOSING: u32 = 11;
    pub const BPF_TCP_NEW_SYN_RECV: u32 = 12;
    pub const BPF_TCP_BOUND_INACTIVE: u32 = 13;
    pub const BPF_TCP_MAX_STATES: u32 = 14;
}

/// C enum `bpf_anon_tcp_bpf_iw` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_tcp_bpf_iw {
    pub const TCP_BPF_IW: u32 = 1001;
    pub const TCP_BPF_SNDCWND_CLAMP: u32 = 1002;
    pub const TCP_BPF_DELACK_MAX: u32 = 1003;
    pub const TCP_BPF_RTO_MIN: u32 = 1004;
    pub const TCP_BPF_SYN: u32 = 1005;
    pub const TCP_BPF_SYN_IP: u32 = 1006;
    pub const TCP_BPF_SYN_MAC: u32 = 1007;
}

/// C enum `bpf_anon_bpf_load_hdr_opt_tcp_syn` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_load_hdr_opt_tcp_syn {
    pub const BPF_LOAD_HDR_OPT_TCP_SYN: u32 = 1;
}

/// C enum `bpf_anon_bpf_write_hdr_tcp_current_mss` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_write_hdr_tcp_current_mss {
    pub const BPF_WRITE_HDR_TCP_CURRENT_MSS: u32 = 1;
    pub const BPF_WRITE_HDR_TCP_SYNACK_COOKIE: u32 = 2;
}

/// C enum `bpf_devcg_acc` (`include/uapi/linux/bpf.h`).
pub mod bpf_devcg_acc {
    pub const BPF_DEVCG_ACC_MKNOD: u32 = 1;
    pub const BPF_DEVCG_ACC_READ: u32 = 2;
    pub const BPF_DEVCG_ACC_WRITE: u32 = 4;
}

/// C enum `bpf_devcg_dev` (`include/uapi/linux/bpf.h`).
pub mod bpf_devcg_dev {
    pub const BPF_DEVCG_DEV_BLOCK: u32 = 1;
    pub const BPF_DEVCG_DEV_CHAR: u32 = 2;
}

/// C enum `bpf_anon_bpf_fib_lookup_direct` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_fib_lookup_direct {
    pub const BPF_FIB_LOOKUP_DIRECT: u32 = 1;
    pub const BPF_FIB_LOOKUP_OUTPUT: u32 = 2;
    pub const BPF_FIB_LOOKUP_SKIP_NEIGH: u32 = 4;
    pub const BPF_FIB_LOOKUP_TBID: u32 = 8;
    pub const BPF_FIB_LOOKUP_SRC: u32 = 16;
}

/// C enum `bpf_anon_bpf_fib_lkup_ret_success` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_fib_lkup_ret_success {
    pub const BPF_FIB_LKUP_RET_SUCCESS: u32 = 0;
    pub const BPF_FIB_LKUP_RET_BLACKHOLE: u32 = 1;
    pub const BPF_FIB_LKUP_RET_UNREACHABLE: u32 = 2;
    pub const BPF_FIB_LKUP_RET_PROHIBIT: u32 = 3;
    pub const BPF_FIB_LKUP_RET_NOT_FWDED: u32 = 4;
    pub const BPF_FIB_LKUP_RET_FWD_DISABLED: u32 = 5;
    pub const BPF_FIB_LKUP_RET_UNSUPP_LWT: u32 = 6;
    pub const BPF_FIB_LKUP_RET_NO_NEIGH: u32 = 7;
    pub const BPF_FIB_LKUP_RET_FRAG_NEEDED: u32 = 8;
    pub const BPF_FIB_LKUP_RET_NO_SRC_ADDR: u32 = 9;
}

/// C enum `bpf_check_mtu_flags` (`include/uapi/linux/bpf.h`).
pub mod bpf_check_mtu_flags {
    pub const BPF_MTU_CHK_SEGS: u32 = 1;
}

/// C enum `bpf_check_mtu_ret` (`include/uapi/linux/bpf.h`).
pub mod bpf_check_mtu_ret {
    pub const BPF_MTU_CHK_RET_SUCCESS: u32 = 0;
    pub const BPF_MTU_CHK_RET_FRAG_NEEDED: u32 = 1;
    pub const BPF_MTU_CHK_RET_SEGS_TOOBIG: u32 = 2;
}

/// C enum `bpf_task_fd_type` (`include/uapi/linux/bpf.h`).
pub mod bpf_task_fd_type {
    pub const BPF_FD_TYPE_RAW_TRACEPOINT: u32 = 0;
    pub const BPF_FD_TYPE_TRACEPOINT: u32 = 1;
    pub const BPF_FD_TYPE_KPROBE: u32 = 2;
    pub const BPF_FD_TYPE_KRETPROBE: u32 = 3;
    pub const BPF_FD_TYPE_UPROBE: u32 = 4;
    pub const BPF_FD_TYPE_URETPROBE: u32 = 5;
}

/// C enum `bpf_anon_bpf_flow_dissector_f_parse_1st_frag` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_flow_dissector_f_parse_1st_frag {
    pub const BPF_FLOW_DISSECTOR_F_PARSE_1ST_FRAG: u32 = 1;
    pub const BPF_FLOW_DISSECTOR_F_STOP_AT_FLOW_LABEL: u32 = 2;
    pub const BPF_FLOW_DISSECTOR_F_STOP_AT_ENCAP: u32 = 4;
}

/// C enum `bpf_anon_btf_f_compact` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_btf_f_compact {
    pub const BTF_F_COMPACT: u32 = 1;
    pub const BTF_F_NONAME: u32 = 2;
    pub const BTF_F_PTR_RAW: u32 = 4;
    pub const BTF_F_ZERO: u32 = 8;
}

/// C enum `bpf_core_relo_kind` (`include/uapi/linux/bpf.h`).
pub mod bpf_core_relo_kind {
    pub const BPF_CORE_FIELD_BYTE_OFFSET: u32 = 0;
    pub const BPF_CORE_FIELD_BYTE_SIZE: u32 = 1;
    pub const BPF_CORE_FIELD_EXISTS: u32 = 2;
    pub const BPF_CORE_FIELD_SIGNED: u32 = 3;
    pub const BPF_CORE_FIELD_LSHIFT_U64: u32 = 4;
    pub const BPF_CORE_FIELD_RSHIFT_U64: u32 = 5;
    pub const BPF_CORE_TYPE_ID_LOCAL: u32 = 6;
    pub const BPF_CORE_TYPE_ID_TARGET: u32 = 7;
    pub const BPF_CORE_TYPE_EXISTS: u32 = 8;
    pub const BPF_CORE_TYPE_SIZE: u32 = 9;
    pub const BPF_CORE_ENUMVAL_EXISTS: u32 = 10;
    pub const BPF_CORE_ENUMVAL_VALUE: u32 = 11;
    pub const BPF_CORE_TYPE_MATCHES: u32 = 12;
}

/// C enum `bpf_anon_bpf_f_timer_abs` (`include/uapi/linux/bpf.h`).
pub mod bpf_anon_bpf_f_timer_abs {
    pub const BPF_F_TIMER_ABS: u32 = 1;
    pub const BPF_F_TIMER_CPU_PIN: u32 = 2;
}

/// `#define` values declared in `include/uapi/linux/bpf.h`.
pub mod defines {
    pub const BPF_ABS: u32 = 32;
    pub const BPF_ADD: u32 = 0;
    pub const BPF_ALU: u32 = 4;
    pub const BPF_ALU64: u32 = 7;
    pub const BPF_AND: u32 = 80;
    pub const BPF_ARSH: u32 = 192;
    pub const BPF_ATOMIC: u32 = 192;
    pub const BPF_B: u32 = 16;
    pub const BPF_BUILD_ID_SIZE: u32 = 20;
    pub const BPF_CALL: u32 = 128;
    pub const BPF_CGROUP_MAX_PROGS: u32 = 64;
    pub const BPF_CMPXCHG: u32 = 241;
    pub const BPF_DIV: u32 = 48;
    pub const BPF_DW: u32 = 24;
    pub const BPF_END: u32 = 208;
    pub const BPF_EXIT: u32 = 144;
    pub const BPF_FETCH: u32 = 1;
    pub const BPF_FROM_BE: u32 = 8;
    pub const BPF_FROM_LE: u32 = 0;
    pub const BPF_F_AFTER: u32 = 16;
    pub const BPF_F_ALLOW_MULTI: u32 = 2;
    pub const BPF_F_ALLOW_OVERRIDE: u32 = 1;
    pub const BPF_F_ANY_ALIGNMENT: u32 = 2;
    pub const BPF_F_BEFORE: u32 = 8;
    pub const BPF_F_ID: u32 = 32;
    pub const BPF_F_NETFILTER_IP_DEFRAG: u32 = 1;
    pub const BPF_F_QUERY_EFFECTIVE: u32 = 1;
    pub const BPF_F_REPLACE: u32 = 4;
    pub const BPF_F_SLEEPABLE: u32 = 16;
    pub const BPF_F_STRICT_ALIGNMENT: u32 = 1;
    pub const BPF_F_TEST_REG_INVARIANTS: u32 = 128;
    pub const BPF_F_TEST_RND_HI32: u32 = 4;
    pub const BPF_F_TEST_RUN_ON_CPU: u32 = 1;
    pub const BPF_F_TEST_STATE_FREQ: u32 = 8;
    pub const BPF_F_TEST_XDP_LIVE_FRAMES: u32 = 2;
    pub const BPF_F_XDP_DEV_BOUND_ONLY: u32 = 64;
    pub const BPF_F_XDP_HAS_FRAGS: u32 = 32;
    pub const BPF_H: u32 = 8;
    pub const BPF_IMM: u32 = 0;
    pub const BPF_IND: u32 = 64;
    pub const BPF_JA: u32 = 0;
    pub const BPF_JEQ: u32 = 16;
    pub const BPF_JGE: u32 = 48;
    pub const BPF_JGT: u32 = 32;
    pub const BPF_JLE: u32 = 176;
    pub const BPF_JLT: u32 = 160;
    pub const BPF_JMP: u32 = 5;
    pub const BPF_JMP32: u32 = 6;
    pub const BPF_JNE: u32 = 80;
    pub const BPF_JSET: u32 = 64;
    pub const BPF_JSGE: u32 = 112;
    pub const BPF_JSGT: u32 = 96;
    pub const BPF_JSLE: u32 = 208;
    pub const BPF_JSLT: u32 = 192;
    pub const BPF_K: u32 = 0;
    pub const BPF_LD: u32 = 0;
    pub const BPF_LDX: u32 = 1;
    pub const BPF_LEN: u32 = 128;
    pub const BPF_LSH: u32 = 96;
    pub const BPF_MAXINSNS: u32 = 4096;
    pub const BPF_MEM: u32 = 96;
    pub const BPF_MEMSX: u32 = 128;
    pub const BPF_MISC: u32 = 7;
    pub const BPF_MOD: u32 = 144;
    pub const BPF_MOV: u32 = 176;
    pub const BPF_MSH: u32 = 160;
    pub const BPF_MUL: u32 = 32;
    pub const BPF_NEG: u32 = 128;
    pub const BPF_OBJ_NAME_LEN: u32 = 16;
    pub const BPF_OR: u32 = 64;
    pub const BPF_PSEUDO_BTF_ID: u32 = 3;
    pub const BPF_PSEUDO_CALL: u32 = 1;
    pub const BPF_PSEUDO_FUNC: u32 = 4;
    pub const BPF_PSEUDO_KFUNC_CALL: u32 = 2;
    pub const BPF_PSEUDO_MAP_FD: u32 = 1;
    pub const BPF_PSEUDO_MAP_IDX: u32 = 5;
    pub const BPF_PSEUDO_MAP_IDX_VALUE: u32 = 6;
    pub const BPF_PSEUDO_MAP_VALUE: u32 = 2;
    pub const BPF_RET: u32 = 6;
    pub const BPF_RSH: u32 = 112;
    pub const BPF_ST: u32 = 2;
    pub const BPF_STX: u32 = 3;
    pub const BPF_SUB: u32 = 16;
    pub const BPF_TAG_SIZE: u32 = 8;
    pub const BPF_TO_BE: u32 = 8;
    pub const BPF_TO_LE: u32 = 0;
    pub const BPF_W: u32 = 0;
    pub const BPF_X: u32 = 8;
    pub const BPF_XADD: u32 = 192;
    pub const BPF_XCHG: u32 = 225;
    pub const BPF_XOR: u32 = 160;
}

/// Values this table declares that are NOT from the uapi headers.
///
/// Each one is a deliberate exception and needs its source named in
/// the generator; the conformance test pins this list so a second
/// exception cannot slip in unnoticed.
pub const NON_UAPI_VALUES: &[&str] = &["BPF_CGROUP_MAX_PROGS"];

/// Names of the enum modules in this table.
pub const ENUM_MODULES: &[&str] = &[
    "bpf_anon_bpf_reg_0",
    "bpf_cgroup_iter_order",
    "bpf_cmd",
    "bpf_map_type",
    "bpf_prog_type",
    "bpf_attach_type",
    "bpf_link_type",
    "bpf_perf_event_type",
    "bpf_anon_bpf_f_kprobe_multi_return",
    "bpf_anon_bpf_f_uprobe_multi_return",
    "bpf_map_update_flags",
    "bpf_anon_bpf_f_no_prealloc",
    "bpf_stats_type",
    "bpf_stack_build_id_status",
    "bpf_anon_bpf_f_recompute_csum",
    "bpf_anon_bpf_f_hdr_field_mask",
    "bpf_anon_bpf_f_pseudo_hdr",
    "bpf_anon_bpf_f_ingress",
    "bpf_anon_bpf_f_tuninfo_ipv6",
    "bpf_anon_bpf_f_skip_field_mask",
    "bpf_anon_bpf_f_zero_csum_tx",
    "bpf_anon_bpf_f_tuninfo_flags",
    "bpf_anon_bpf_f_index_mask",
    "bpf_anon_bpf_csum_level_query",
    "bpf_anon_bpf_f_adj_room_fixed_gso",
    "bpf_anon_bpf_adj_room_encap_l2_mask",
    "bpf_anon_bpf_f_sysctl_base_name",
    "bpf_anon_bpf_local_storage_get_f_create",
    "bpf_anon_bpf_f_get_branch_records_size",
    "bpf_anon_bpf_rb_no_wakeup",
    "bpf_anon_bpf_rb_avail_data",
    "bpf_anon_bpf_ringbuf_busy_bit",
    "bpf_anon_bpf_sk_lookup_f_replace",
    "bpf_adj_room_mode",
    "bpf_hdr_start_off",
    "bpf_lwt_encap_mode",
    "bpf_anon_bpf_f_bprm_secureexec",
    "bpf_anon_bpf_f_broadcast",
    "bpf_anon_bpf_skb_tstamp_unspec",
    "bpf_ret_code",
    "tcx_action_base",
    "xdp_action",
    "sk_action",
    "bpf_anon_bpf_sock_ops_rto_cb_flag",
    "bpf_anon_bpf_sock_ops_void",
    "bpf_anon_bpf_tcp_established",
    "bpf_anon_tcp_bpf_iw",
    "bpf_anon_bpf_load_hdr_opt_tcp_syn",
    "bpf_anon_bpf_write_hdr_tcp_current_mss",
    "bpf_devcg_acc",
    "bpf_devcg_dev",
    "bpf_anon_bpf_fib_lookup_direct",
    "bpf_anon_bpf_fib_lkup_ret_success",
    "bpf_check_mtu_flags",
    "bpf_check_mtu_ret",
    "bpf_task_fd_type",
    "bpf_anon_bpf_flow_dissector_f_parse_1st_frag",
    "bpf_anon_btf_f_compact",
    "bpf_core_relo_kind",
    "bpf_anon_bpf_f_timer_abs",
];

/// `#define`s in the uapi headers that are not integer constants.
///
/// Listed so a reader can tell "absent on purpose" from "the generator
/// dropped it". Function-like accessor macros (`BPF_CLASS(code)`) and
/// expressions the evaluator cannot fold are the only entries here.
pub const NOT_CONSTANTS: &[&str] = &[];

/// `enum bpf_func_id` helper ids (`___BPF_FUNC_MAPPER`).
pub mod bpf_func_id {
    pub const BPF_FUNC_unspec: u32 = 0;
    pub const BPF_FUNC_map_lookup_elem: u32 = 1;
    pub const BPF_FUNC_map_update_elem: u32 = 2;
    pub const BPF_FUNC_map_delete_elem: u32 = 3;
    pub const BPF_FUNC_probe_read: u32 = 4;
    pub const BPF_FUNC_ktime_get_ns: u32 = 5;
    pub const BPF_FUNC_trace_printk: u32 = 6;
    pub const BPF_FUNC_get_prandom_u32: u32 = 7;
    pub const BPF_FUNC_get_smp_processor_id: u32 = 8;
    pub const BPF_FUNC_skb_store_bytes: u32 = 9;
    pub const BPF_FUNC_l3_csum_replace: u32 = 10;
    pub const BPF_FUNC_l4_csum_replace: u32 = 11;
    pub const BPF_FUNC_tail_call: u32 = 12;
    pub const BPF_FUNC_clone_redirect: u32 = 13;
    pub const BPF_FUNC_get_current_pid_tgid: u32 = 14;
    pub const BPF_FUNC_get_current_uid_gid: u32 = 15;
    pub const BPF_FUNC_get_current_comm: u32 = 16;
    pub const BPF_FUNC_get_cgroup_classid: u32 = 17;
    pub const BPF_FUNC_skb_vlan_push: u32 = 18;
    pub const BPF_FUNC_skb_vlan_pop: u32 = 19;
    pub const BPF_FUNC_skb_get_tunnel_key: u32 = 20;
    pub const BPF_FUNC_skb_set_tunnel_key: u32 = 21;
    pub const BPF_FUNC_perf_event_read: u32 = 22;
    pub const BPF_FUNC_redirect: u32 = 23;
    pub const BPF_FUNC_get_route_realm: u32 = 24;
    pub const BPF_FUNC_perf_event_output: u32 = 25;
    pub const BPF_FUNC_skb_load_bytes: u32 = 26;
    pub const BPF_FUNC_get_stackid: u32 = 27;
    pub const BPF_FUNC_csum_diff: u32 = 28;
    pub const BPF_FUNC_skb_get_tunnel_opt: u32 = 29;
    pub const BPF_FUNC_skb_set_tunnel_opt: u32 = 30;
    pub const BPF_FUNC_skb_change_proto: u32 = 31;
    pub const BPF_FUNC_skb_change_type: u32 = 32;
    pub const BPF_FUNC_skb_under_cgroup: u32 = 33;
    pub const BPF_FUNC_get_hash_recalc: u32 = 34;
    pub const BPF_FUNC_get_current_task: u32 = 35;
    pub const BPF_FUNC_probe_write_user: u32 = 36;
    pub const BPF_FUNC_current_task_under_cgroup: u32 = 37;
    pub const BPF_FUNC_skb_change_tail: u32 = 38;
    pub const BPF_FUNC_skb_pull_data: u32 = 39;
    pub const BPF_FUNC_csum_update: u32 = 40;
    pub const BPF_FUNC_set_hash_invalid: u32 = 41;
    pub const BPF_FUNC_get_numa_node_id: u32 = 42;
    pub const BPF_FUNC_skb_change_head: u32 = 43;
    pub const BPF_FUNC_xdp_adjust_head: u32 = 44;
    pub const BPF_FUNC_probe_read_str: u32 = 45;
    pub const BPF_FUNC_get_socket_cookie: u32 = 46;
    pub const BPF_FUNC_get_socket_uid: u32 = 47;
    pub const BPF_FUNC_set_hash: u32 = 48;
    pub const BPF_FUNC_setsockopt: u32 = 49;
    pub const BPF_FUNC_skb_adjust_room: u32 = 50;
    pub const BPF_FUNC_redirect_map: u32 = 51;
    pub const BPF_FUNC_sk_redirect_map: u32 = 52;
    pub const BPF_FUNC_sock_map_update: u32 = 53;
    pub const BPF_FUNC_xdp_adjust_meta: u32 = 54;
    pub const BPF_FUNC_perf_event_read_value: u32 = 55;
    pub const BPF_FUNC_perf_prog_read_value: u32 = 56;
    pub const BPF_FUNC_getsockopt: u32 = 57;
    pub const BPF_FUNC_override_return: u32 = 58;
    pub const BPF_FUNC_sock_ops_cb_flags_set: u32 = 59;
    pub const BPF_FUNC_msg_redirect_map: u32 = 60;
    pub const BPF_FUNC_msg_apply_bytes: u32 = 61;
    pub const BPF_FUNC_msg_cork_bytes: u32 = 62;
    pub const BPF_FUNC_msg_pull_data: u32 = 63;
    pub const BPF_FUNC_bind: u32 = 64;
    pub const BPF_FUNC_xdp_adjust_tail: u32 = 65;
    pub const BPF_FUNC_skb_get_xfrm_state: u32 = 66;
    pub const BPF_FUNC_get_stack: u32 = 67;
    pub const BPF_FUNC_skb_load_bytes_relative: u32 = 68;
    pub const BPF_FUNC_fib_lookup: u32 = 69;
    pub const BPF_FUNC_sock_hash_update: u32 = 70;
    pub const BPF_FUNC_msg_redirect_hash: u32 = 71;
    pub const BPF_FUNC_sk_redirect_hash: u32 = 72;
    pub const BPF_FUNC_lwt_push_encap: u32 = 73;
    pub const BPF_FUNC_lwt_seg6_store_bytes: u32 = 74;
    pub const BPF_FUNC_lwt_seg6_adjust_srh: u32 = 75;
    pub const BPF_FUNC_lwt_seg6_action: u32 = 76;
    pub const BPF_FUNC_rc_repeat: u32 = 77;
    pub const BPF_FUNC_rc_keydown: u32 = 78;
    pub const BPF_FUNC_skb_cgroup_id: u32 = 79;
    pub const BPF_FUNC_get_current_cgroup_id: u32 = 80;
    pub const BPF_FUNC_get_local_storage: u32 = 81;
    pub const BPF_FUNC_sk_select_reuseport: u32 = 82;
    pub const BPF_FUNC_skb_ancestor_cgroup_id: u32 = 83;
    pub const BPF_FUNC_sk_lookup_tcp: u32 = 84;
    pub const BPF_FUNC_sk_lookup_udp: u32 = 85;
    pub const BPF_FUNC_sk_release: u32 = 86;
    pub const BPF_FUNC_map_push_elem: u32 = 87;
    pub const BPF_FUNC_map_pop_elem: u32 = 88;
    pub const BPF_FUNC_map_peek_elem: u32 = 89;
    pub const BPF_FUNC_msg_push_data: u32 = 90;
    pub const BPF_FUNC_msg_pop_data: u32 = 91;
    pub const BPF_FUNC_rc_pointer_rel: u32 = 92;
    pub const BPF_FUNC_spin_lock: u32 = 93;
    pub const BPF_FUNC_spin_unlock: u32 = 94;
    pub const BPF_FUNC_sk_fullsock: u32 = 95;
    pub const BPF_FUNC_tcp_sock: u32 = 96;
    pub const BPF_FUNC_skb_ecn_set_ce: u32 = 97;
    pub const BPF_FUNC_get_listener_sock: u32 = 98;
    pub const BPF_FUNC_skc_lookup_tcp: u32 = 99;
    pub const BPF_FUNC_tcp_check_syncookie: u32 = 100;
    pub const BPF_FUNC_sysctl_get_name: u32 = 101;
    pub const BPF_FUNC_sysctl_get_current_value: u32 = 102;
    pub const BPF_FUNC_sysctl_get_new_value: u32 = 103;
    pub const BPF_FUNC_sysctl_set_new_value: u32 = 104;
    pub const BPF_FUNC_strtol: u32 = 105;
    pub const BPF_FUNC_strtoul: u32 = 106;
    pub const BPF_FUNC_sk_storage_get: u32 = 107;
    pub const BPF_FUNC_sk_storage_delete: u32 = 108;
    pub const BPF_FUNC_send_signal: u32 = 109;
    pub const BPF_FUNC_tcp_gen_syncookie: u32 = 110;
    pub const BPF_FUNC_skb_output: u32 = 111;
    pub const BPF_FUNC_probe_read_user: u32 = 112;
    pub const BPF_FUNC_probe_read_kernel: u32 = 113;
    pub const BPF_FUNC_probe_read_user_str: u32 = 114;
    pub const BPF_FUNC_probe_read_kernel_str: u32 = 115;
    pub const BPF_FUNC_tcp_send_ack: u32 = 116;
    pub const BPF_FUNC_send_signal_thread: u32 = 117;
    pub const BPF_FUNC_jiffies64: u32 = 118;
    pub const BPF_FUNC_read_branch_records: u32 = 119;
    pub const BPF_FUNC_get_ns_current_pid_tgid: u32 = 120;
    pub const BPF_FUNC_xdp_output: u32 = 121;
    pub const BPF_FUNC_get_netns_cookie: u32 = 122;
    pub const BPF_FUNC_get_current_ancestor_cgroup_id: u32 = 123;
    pub const BPF_FUNC_sk_assign: u32 = 124;
    pub const BPF_FUNC_ktime_get_boot_ns: u32 = 125;
    pub const BPF_FUNC_seq_printf: u32 = 126;
    pub const BPF_FUNC_seq_write: u32 = 127;
    pub const BPF_FUNC_sk_cgroup_id: u32 = 128;
    pub const BPF_FUNC_sk_ancestor_cgroup_id: u32 = 129;
    pub const BPF_FUNC_ringbuf_output: u32 = 130;
    pub const BPF_FUNC_ringbuf_reserve: u32 = 131;
    pub const BPF_FUNC_ringbuf_submit: u32 = 132;
    pub const BPF_FUNC_ringbuf_discard: u32 = 133;
    pub const BPF_FUNC_ringbuf_query: u32 = 134;
    pub const BPF_FUNC_csum_level: u32 = 135;
    pub const BPF_FUNC_skc_to_tcp6_sock: u32 = 136;
    pub const BPF_FUNC_skc_to_tcp_sock: u32 = 137;
    pub const BPF_FUNC_skc_to_tcp_timewait_sock: u32 = 138;
    pub const BPF_FUNC_skc_to_tcp_request_sock: u32 = 139;
    pub const BPF_FUNC_skc_to_udp6_sock: u32 = 140;
    pub const BPF_FUNC_get_task_stack: u32 = 141;
    pub const BPF_FUNC_load_hdr_opt: u32 = 142;
    pub const BPF_FUNC_store_hdr_opt: u32 = 143;
    pub const BPF_FUNC_reserve_hdr_opt: u32 = 144;
    pub const BPF_FUNC_inode_storage_get: u32 = 145;
    pub const BPF_FUNC_inode_storage_delete: u32 = 146;
    pub const BPF_FUNC_d_path: u32 = 147;
    pub const BPF_FUNC_copy_from_user: u32 = 148;
    pub const BPF_FUNC_snprintf_btf: u32 = 149;
    pub const BPF_FUNC_seq_printf_btf: u32 = 150;
    pub const BPF_FUNC_skb_cgroup_classid: u32 = 151;
    pub const BPF_FUNC_redirect_neigh: u32 = 152;
    pub const BPF_FUNC_per_cpu_ptr: u32 = 153;
    pub const BPF_FUNC_this_cpu_ptr: u32 = 154;
    pub const BPF_FUNC_redirect_peer: u32 = 155;
    pub const BPF_FUNC_task_storage_get: u32 = 156;
    pub const BPF_FUNC_task_storage_delete: u32 = 157;
    pub const BPF_FUNC_get_current_task_btf: u32 = 158;
    pub const BPF_FUNC_bprm_opts_set: u32 = 159;
    pub const BPF_FUNC_ktime_get_coarse_ns: u32 = 160;
    pub const BPF_FUNC_ima_inode_hash: u32 = 161;
    pub const BPF_FUNC_sock_from_file: u32 = 162;
    pub const BPF_FUNC_check_mtu: u32 = 163;
    pub const BPF_FUNC_for_each_map_elem: u32 = 164;
    pub const BPF_FUNC_snprintf: u32 = 165;
    pub const BPF_FUNC_sys_bpf: u32 = 166;
    pub const BPF_FUNC_btf_find_by_name_kind: u32 = 167;
    pub const BPF_FUNC_sys_close: u32 = 168;
    pub const BPF_FUNC_timer_init: u32 = 169;
    pub const BPF_FUNC_timer_set_callback: u32 = 170;
    pub const BPF_FUNC_timer_start: u32 = 171;
    pub const BPF_FUNC_timer_cancel: u32 = 172;
    pub const BPF_FUNC_get_func_ip: u32 = 173;
    pub const BPF_FUNC_get_attach_cookie: u32 = 174;
    pub const BPF_FUNC_task_pt_regs: u32 = 175;
    pub const BPF_FUNC_get_branch_snapshot: u32 = 176;
    pub const BPF_FUNC_trace_vprintk: u32 = 177;
    pub const BPF_FUNC_skc_to_unix_sock: u32 = 178;
    pub const BPF_FUNC_kallsyms_lookup_name: u32 = 179;
    pub const BPF_FUNC_find_vma: u32 = 180;
    pub const BPF_FUNC_loop: u32 = 181;
    pub const BPF_FUNC_strncmp: u32 = 182;
    pub const BPF_FUNC_get_func_arg: u32 = 183;
    pub const BPF_FUNC_get_func_ret: u32 = 184;
    pub const BPF_FUNC_get_func_arg_cnt: u32 = 185;
    pub const BPF_FUNC_get_retval: u32 = 186;
    pub const BPF_FUNC_set_retval: u32 = 187;
    pub const BPF_FUNC_xdp_get_buff_len: u32 = 188;
    pub const BPF_FUNC_xdp_load_bytes: u32 = 189;
    pub const BPF_FUNC_xdp_store_bytes: u32 = 190;
    pub const BPF_FUNC_copy_from_user_task: u32 = 191;
    pub const BPF_FUNC_skb_set_tstamp: u32 = 192;
    pub const BPF_FUNC_ima_file_hash: u32 = 193;
    pub const BPF_FUNC_kptr_xchg: u32 = 194;
    pub const BPF_FUNC_map_lookup_percpu_elem: u32 = 195;
    pub const BPF_FUNC_skc_to_mptcp_sock: u32 = 196;
    pub const BPF_FUNC_dynptr_from_mem: u32 = 197;
    pub const BPF_FUNC_ringbuf_reserve_dynptr: u32 = 198;
    pub const BPF_FUNC_ringbuf_submit_dynptr: u32 = 199;
    pub const BPF_FUNC_ringbuf_discard_dynptr: u32 = 200;
    pub const BPF_FUNC_dynptr_read: u32 = 201;
    pub const BPF_FUNC_dynptr_write: u32 = 202;
    pub const BPF_FUNC_dynptr_data: u32 = 203;
    pub const BPF_FUNC_tcp_raw_gen_syncookie_ipv4: u32 = 204;
    pub const BPF_FUNC_tcp_raw_gen_syncookie_ipv6: u32 = 205;
    pub const BPF_FUNC_tcp_raw_check_syncookie_ipv4: u32 = 206;
    pub const BPF_FUNC_tcp_raw_check_syncookie_ipv6: u32 = 207;
    pub const BPF_FUNC_ktime_get_tai_ns: u32 = 208;
    pub const BPF_FUNC_user_ringbuf_drain: u32 = 209;
    pub const BPF_FUNC_cgrp_storage_get: u32 = 210;
    pub const BPF_FUNC_cgrp_storage_delete: u32 = 211;
}
