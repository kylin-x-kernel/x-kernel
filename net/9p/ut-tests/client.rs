// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};

use kerrno::KResult;
use unittest::{assert_eq, def_test};

use super::*;

struct FixedResponseTransport {
    response: Vec<u8>,
    reported_size: usize,
}

struct RecordingEmptyResponseTransport {
    request: Arc<Mutex<Vec<u8>>>,
    response_type: u8,
}

struct RecordingOpenTransport {
    request: Arc<Mutex<Vec<u8>>>,
}

struct RecordingStatFsTransport {
    request: Arc<Mutex<Vec<u8>>>,
}

struct NegotiationTransport {
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl P9Transport for FixedResponseTransport {
    fn request(&mut self, _request: &[u8], response: &mut [u8]) -> KResult<usize> {
        let copied = self.response.len().min(response.len());
        response[..copied].copy_from_slice(&self.response[..copied]);
        Ok(self.reported_size)
    }
}

impl P9Transport for RecordingEmptyResponseTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
        *self.request.lock() = request.to_vec();
        response[..4].copy_from_slice(&7u32.to_le_bytes());
        response[4] = self.response_type;
        response[5..7].copy_from_slice(&request[5..7]);
        Ok(7)
    }
}

impl P9Transport for RecordingOpenTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
        *self.request.lock() = request.to_vec();
        if matches!(request[4], TREAD | TWRITE | TREADDIR) {
            response[..4].copy_from_slice(&11u32.to_le_bytes());
            response[4] = request[4] + 1;
            response[5..7].copy_from_slice(&request[5..7]);
            response[7..11].copy_from_slice(&0u32.to_le_bytes());
            return Ok(11);
        }
        response[..4].copy_from_slice(&24u32.to_le_bytes());
        response[4] = RLOPEN;
        response[5..7].copy_from_slice(&request[5..7]);
        response[7] = 0;
        response[8..12].copy_from_slice(&0u32.to_le_bytes());
        response[12..20].copy_from_slice(&1u64.to_le_bytes());
        response[20..24].copy_from_slice(&8u32.to_le_bytes());
        Ok(24)
    }
}

impl P9Transport for RecordingStatFsTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
        *self.request.lock() = request.to_vec();
        let mut packet = Vec::new();
        packet.extend_from_slice(&67u32.to_le_bytes());
        packet.push(RSTATFS);
        packet.extend_from_slice(&request[5..7]);
        packet.extend_from_slice(&0x0102_1997u32.to_le_bytes());
        packet.extend_from_slice(&4096u32.to_le_bytes());
        for value in [1000u64, 400, 300, 200, 100, 42] {
            packet.extend_from_slice(&value.to_le_bytes());
        }
        packet.extend_from_slice(&255u32.to_le_bytes());
        response[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }
}

impl P9Transport for NegotiationTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
        self.requests.lock().push(request.to_vec());
        let mut packet = Vec::new();
        match request[4] {
            TVERSION => {
                packet.extend_from_slice(&21u32.to_le_bytes());
                packet.push(RVERSION);
                packet.extend_from_slice(&request[5..7]);
                packet.extend_from_slice(&DEFAULT_MSIZE.to_le_bytes());
                packet.extend_from_slice(&8u16.to_le_bytes());
                packet.extend_from_slice(b"9P2000.L");
            }
            TATTACH => {
                packet.extend_from_slice(&20u32.to_le_bytes());
                packet.push(RATTACH);
                packet.extend_from_slice(&request[5..7]);
                packet.push(0x80);
                packet.extend_from_slice(&0u32.to_le_bytes());
                packet.extend_from_slice(&1u64.to_le_bytes());
            }
            TCLUNK => {
                packet.extend_from_slice(&7u32.to_le_bytes());
                packet.push(RCLUNK);
                packet.extend_from_slice(&request[5..7]);
            }
            _ => return Err(KError::InvalidInput),
        }
        response[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }
}

fn test_client(response: Vec<u8>, reported_size: usize) -> Arc<P9Client> {
    Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: response.len().max(8) as u32,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::Unknown,
            transport: Box::new(FixedResponseTransport {
                response,
                reported_size,
            }),
        }),
    })
}

#[def_test]
fn test_negotiate_uses_linux_default_attach_identity() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let client = P9Client::new(Box::new(NegotiationTransport {
        requests: requests.clone(),
    }));

    let root = client.negotiate().unwrap();

    let requests = requests.lock();
    let attach = &requests[1];
    assert_eq!(attach[4], TATTACH);
    assert_eq!(&attach[17..23], b"nobody");
    assert_eq!(u16::from_le_bytes(attach[23..25].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(attach[25..29].try_into().unwrap()),
        u32::MAX
    );
    drop(requests);
    drop(root);
}

#[def_test]
fn test_send_recv_rejects_transport_length_beyond_buffer() {
    let client = test_client(Vec::new(), 9);
    assert_eq!(
        client
            .state
            .lock()
            .send_recv(Vec::new(), RVERSION, 1)
            .unwrap_err(),
        KError::InvalidData
    );
}

#[def_test]
fn test_fid_allocator_never_wraps_into_a_live_identifier() {
    let client = test_client(Vec::new(), 0);
    let mut state = client.state.lock();
    state.live_fids.insert(NO_FID - 1);
    state.live_fids.insert(0);
    state.next_fid = NO_FID - 1;
    assert_eq!(state.alloc_fid(), Ok(1));
    state.release_fid(0);
    state.next_fid = 0;
    assert_eq!(state.alloc_fid(), Ok(0));
}

#[def_test]
fn test_final_fid_reference_clunks_and_releases_identifier() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let mut live_fids = BTreeSet::new();
    live_fids.insert(41);
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 64,
            next_tag: 1,
            next_fid: 42,
            live_fids,
            version: P9Version::P2000L,
            transport: Box::new(RecordingEmptyResponseTransport {
                request: request.clone(),
                response_type: RCLUNK,
            }),
        }),
    });
    let fid = P9Fid::new(
        client.clone(),
        41,
        Qid {
            type_: 0,
            version: 0,
            path: 1,
        },
    );

    drop(fid);

    let request = request.lock();
    assert_eq!(request[4], TCLUNK);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    drop(request);
    assert_eq!(client.state.lock().live_fids.contains(&41), false);
}

#[def_test]
fn test_send_recv_rejects_header_length_mismatch() {
    let mut response = vec![0u8; 7];
    response[..4].copy_from_slice(&8u32.to_le_bytes());
    response[4] = RVERSION;
    response[5..7].copy_from_slice(&1u16.to_le_bytes());
    let client = test_client(response, 7);
    assert_eq!(
        client
            .state
            .lock()
            .send_recv(Vec::new(), RVERSION, 1)
            .unwrap_err(),
        KError::InvalidData
    );
}

#[def_test]
fn test_readlink_rejects_embedded_nul_from_server() {
    let mut response = vec![0u8; 12];
    response[..4].copy_from_slice(&12u32.to_le_bytes());
    response[4] = RREADLINK;
    response[5..7].copy_from_slice(&1u16.to_le_bytes());
    response[7..9].copy_from_slice(&3u16.to_le_bytes());
    response[9..12].copy_from_slice(b"a\0b");
    let client = test_client(response, 12);

    assert_eq!(client.state.lock().read_link(41), Err(KError::InvalidData));
}

#[def_test]
fn test_rlerror_preserves_server_errno() {
    let mut response = vec![0u8; 11];
    response[..4].copy_from_slice(&11u32.to_le_bytes());
    response[4] = RLERROR;
    response[5..7].copy_from_slice(&1u16.to_le_bytes());
    response[7..11].copy_from_slice(&(LinuxError::ENOSYS.into_raw() as u32).to_le_bytes());
    let client = test_client(response, 11);
    assert_eq!(
        client
            .state
            .lock()
            .send_recv(Vec::new(), RVERSION, 1)
            .unwrap_err()
            .canonicalize(),
        KError::Unsupported
    );
}

#[def_test]
fn test_dir_entry_name_matches_linux_limit() {
    assert_eq!(validate_dir_entry_name(&"a".repeat(255)), Ok(()));
    assert_eq!(
        validate_dir_entry_name(&"a".repeat(256)),
        Err(KError::InvalidData)
    );
    assert_eq!(
        validate_dir_entry_name("invalid\0name"),
        Err(KError::InvalidData)
    );
}

#[def_test]
fn test_walk_rejects_more_than_linux_maxwelem() {
    let names = ["name"; P9_MAXWELEM + 1];
    assert_eq!(validate_names(&names), Err(KError::InvalidInput));
}

#[def_test]
fn test_link_encodes_directory_fid_before_target_fid() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 64,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingEmptyResponseTransport {
                request: request.clone(),
                response_type: RLINK,
            }),
        }),
    });
    let qid = Qid {
        type_: 0,
        version: 0,
        path: 1,
    };
    let directory = P9Fid::new(client.clone(), 41, qid);
    let target = P9Fid::new(client, 72, qid);

    directory.link(&target, "linked").unwrap();

    let request = request.lock();
    assert_eq!(request[4], TLINK);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    assert_eq!(u32::from_le_bytes(request[11..15].try_into().unwrap()), 72);
    directory.mode.store(FID_MODE_INVALID, Ordering::Release);
    target.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_open_encodes_linux_server_mode_mask() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 64,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingOpenTransport {
                request: request.clone(),
            }),
        }),
    });
    let fid = P9Fid::new(
        client,
        41,
        Qid {
            type_: 0,
            version: 0,
            path: 1,
        },
    );

    fid.open(
        P9OpenMode::ReadWrite,
        P9OpenFlags::CREATE
            | P9OpenFlags::EXCLUSIVE
            | P9OpenFlags::TRUNCATE
            | P9OpenFlags::APPEND
            | P9OpenFlags::SYNC,
    )
    .unwrap();

    let request = request.lock();
    assert_eq!(request[4], TLOPEN);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    assert_eq!(
        u32::from_le_bytes(request[11..15].try_into().unwrap()),
        P9_DOTL_RDWR
            | P9OpenFlags::CREATE.bits()
            | P9OpenFlags::EXCLUSIVE.bits()
            | P9OpenFlags::TRUNCATE.bits()
            | P9OpenFlags::APPEND.bits()
    );
    fid.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_open_failure_can_retry_and_io_obeys_server_iounit() {
    let client = test_client(Vec::new(), 0);
    let fid = P9Fid::new(
        client.clone(),
        41,
        Qid {
            type_: 0,
            version: 0,
            path: 1,
        },
    );
    assert_eq!(
        fid.open(P9OpenMode::ReadWrite, P9OpenFlags::empty())
            .is_err(),
        true
    );

    let request = Arc::new(Mutex::new(Vec::new()));
    {
        let mut state = client.state.lock();
        state.msize = 4096;
        state.transport = Box::new(RecordingOpenTransport {
            request: request.clone(),
        });
    }
    fid.open(P9OpenMode::ReadWrite, P9OpenFlags::empty())
        .unwrap();
    assert_eq!(
        fid.open(P9OpenMode::Read, P9OpenFlags::empty()),
        Err(KError::BadState)
    );
    fid.read(0, 32).unwrap();
    assert_eq!(
        u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
        8
    );
    fid.write(0, &[0; 32]).unwrap();
    assert_eq!(
        u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
        8
    );
    fid.read_dir(0).unwrap();
    assert_eq!(
        u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
        8
    );
    fid.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_statfs_decodes_linux_dotl_fields() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 128,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingStatFsTransport {
                request: request.clone(),
            }),
        }),
    });
    let fid = P9Fid::new(
        client,
        41,
        Qid {
            type_: 0x80,
            version: 0,
            path: 1,
        },
    );

    let stats = fid.statfs().unwrap();
    assert_eq!(
        stats,
        P9StatFs {
            fs_type: 0x0102_1997,
            block_size: 4096,
            blocks: 1000,
            blocks_free: 400,
            blocks_available: 300,
            file_count: 200,
            free_file_count: 100,
            fs_id: 42,
            name_length: 255,
        }
    );
    let request = request.lock();
    assert_eq!(request[4], TSTATFS);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    fid.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_setattr_encodes_valid_bits_and_timestamps() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 128,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingEmptyResponseTransport {
                request: request.clone(),
                response_type: RSETATTR,
            }),
        }),
    });
    let fid = P9Fid::new(
        client,
        41,
        Qid {
            type_: 0,
            version: 0,
            path: 1,
        },
    );

    fid.setattr(P9SetAttr {
        mode: Some(0o640),
        uid: Some(1000),
        gid: Some(1001),
        size: Some(123),
        atime: Some(P9SetAttrTime::Explicit(10, 20)),
        mtime: Some(P9SetAttrTime::Current),
        should_update_ctime: true,
    })
    .unwrap();

    let request = request.lock();
    assert_eq!(request[4], TSETATTR);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    assert_eq!(
        u32::from_le_bytes(request[11..15].try_into().unwrap()),
        0xff
    );
    assert_eq!(
        u32::from_le_bytes(request[15..19].try_into().unwrap()),
        0o640
    );
    assert_eq!(
        u32::from_le_bytes(request[19..23].try_into().unwrap()),
        1000
    );
    assert_eq!(
        u32::from_le_bytes(request[23..27].try_into().unwrap()),
        1001
    );
    assert_eq!(u64::from_le_bytes(request[27..35].try_into().unwrap()), 123);
    assert_eq!(u64::from_le_bytes(request[35..43].try_into().unwrap()), 10);
    assert_eq!(u64::from_le_bytes(request[43..51].try_into().unwrap()), 20);
    assert_eq!(u64::from_le_bytes(request[51..59].try_into().unwrap()), 0);
    assert_eq!(u64::from_le_bytes(request[59..67].try_into().unwrap()), 0);
    fid.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_renameat_encodes_directory_fids_and_names() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 128,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingEmptyResponseTransport {
                request: request.clone(),
                response_type: RRENAMEAT,
            }),
        }),
    });
    let qid = Qid {
        type_: 0x80,
        version: 0,
        path: 1,
    };
    let old_dir = P9Fid::new(client.clone(), 41, qid);
    let new_dir = P9Fid::new(client, 72, qid);

    old_dir.rename_at("old", &new_dir, "new").unwrap();

    let request = request.lock();
    assert_eq!(request[4], TRENAMEAT);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    assert_eq!(&request[13..16], b"old");
    assert_eq!(u32::from_le_bytes(request[16..20].try_into().unwrap()), 72);
    assert_eq!(&request[22..25], b"new");
    old_dir.mode.store(FID_MODE_INVALID, Ordering::Release);
    new_dir.mode.store(FID_MODE_INVALID, Ordering::Release);
}

#[def_test]
fn test_unlinkat_encodes_remove_directory_flag() {
    let request = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(P9Client {
        state: Mutex::new(P9ClientState {
            msize: 128,
            next_tag: 1,
            next_fid: 1,
            live_fids: BTreeSet::new(),
            version: P9Version::P2000L,
            transport: Box::new(RecordingEmptyResponseTransport {
                request: request.clone(),
                response_type: RUNLINKAT,
            }),
        }),
    });
    let directory = P9Fid::new(
        client,
        41,
        Qid {
            type_: 0x80,
            version: 0,
            path: 1,
        },
    );

    directory.unlink_at("victim", true).unwrap();

    let request = request.lock();
    assert_eq!(request[4], TUNLINKAT);
    assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
    assert_eq!(&request[13..19], b"victim");
    assert_eq!(
        u32::from_le_bytes(request[19..23].try_into().unwrap()),
        P9_DOTL_AT_REMOVEDIR
    );
    directory.mode.store(FID_MODE_INVALID, Ordering::Release);
}
