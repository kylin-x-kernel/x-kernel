// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::RefCell;

use unittest::def_test;

use super::*;

#[def_test]
fn test_sock_extended_err_origin_matches_linux_abi() {
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Local),
        SO_EE_ORIGIN_LOCAL
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Icmp),
        SO_EE_ORIGIN_ICMP
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Icmp6),
        SO_EE_ORIGIN_ICMP6
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::TxStatus),
        SO_EE_ORIGIN_TXSTATUS
    );
}

#[def_test]
fn scm_rights_body_caps_at_scm_max_fd() {
    assert_eq!(
        CMsg::rights_fd_count(SCM_MAX_FD * size_of::<i32>()),
        Ok(SCM_MAX_FD)
    );
    assert_eq!(
        CMsg::rights_fd_count((SCM_MAX_FD + 1) * size_of::<i32>()),
        Err(KError::InvalidInput)
    );
    // A body that is not a whole number of descriptors is malformed.
    assert_eq!(
        CMsg::rights_fd_count(size_of::<i32>() - 1),
        Err(KError::InvalidInput)
    );
    assert_eq!(CMsg::rights_fd_count(0), Ok(0));
}

#[def_test]
fn sendmsg_descriptor_budget_caps_at_scm_max_fd() {
    let mut budget = ScmFdBudget::default();
    assert_eq!(budget.charge(SCM_MAX_FD - 1), Ok(()));
    assert_eq!(budget.charge(1), Ok(()));
    // The total across a `sendmsg` is capped, not only each message.
    assert_eq!(budget.charge(1), Err(KError::InvalidInput));
    assert_eq!(ScmFdBudget::default().charge(SCM_MAX_FD), Ok(()));
}

fn sender_context(pid: u32) -> ScmSenderContext {
    // A non-privileged identity: every check_creds privilege escape hatch is
    // closed, so claims are validated strictly.
    ScmSenderContext {
        pid,
        cred: Arc::new(kcred::Cred::new(1000, 1000)),
        process: kprocess::init_proc(),
    }
}

fn push_cmsg(buf: &mut Vec<u8>, level: u32, ty: u32, body: &[u8]) {
    let cmsg_len = size_of::<cmsghdr>() + body.len();
    buf.extend_from_slice(&cmsg_len.to_ne_bytes());
    buf.extend_from_slice(&(level as i32).to_ne_bytes());
    buf.extend_from_slice(&(ty as i32).to_ne_bytes());
    buf.extend_from_slice(body);
    // Round the record up to the cmsghdr alignment, like libc does.
    let pad = cmsg_align(cmsg_len).unwrap_or(cmsg_len) - cmsg_len;
    buf.extend(core::iter::repeat_n(0u8, pad));
}

/// Runs `parse_control_msgs` with a no-op rights resolver.
fn parse(
    control: &[u8],
    sender: &ScmSenderContext,
) -> KResult<(Vec<CMsg>, Option<knet::SenderCredentials>)> {
    parse_with(control, sender, &mut |fds| {
        let _ = fds.len();
        Ok(CMsg::Rights { fds: Vec::new() })
    })
}

fn parse_with(
    control: &[u8],
    sender: &ScmSenderContext,
    resolve: &mut dyn FnMut(Vec<i32>) -> KResult<CMsg>,
) -> KResult<(Vec<CMsg>, Option<knet::SenderCredentials>)> {
    parse_with_family(control, CmsgFamily::Unix, sender, resolve)
}

fn parse_with_family(
    control: &[u8],
    family: CmsgFamily,
    sender: &ScmSenderContext,
    resolve: &mut dyn FnMut(Vec<i32>) -> KResult<CMsg>,
) -> KResult<(Vec<CMsg>, Option<knet::SenderCredentials>)> {
    parse_control_msgs(control, family, Some(sender), resolve)
}

fn parse_with_err(control: &[u8], sender: &ScmSenderContext) -> KError {
    match parse_with(control, sender, &mut |fds| {
        let _ = fds.len();
        Ok(CMsg::Rights { fds: Vec::new() })
    }) {
        Ok(_) => panic!("expected parse failure"),
        Err(e) => e,
    }
}

fn credentials_body(pid: u32, uid: u32, gid: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&pid.to_ne_bytes());
    body.extend_from_slice(&uid.to_ne_bytes());
    body.extend_from_slice(&gid.to_ne_bytes());
    body
}

#[def_test]
fn two_aligned_cmsgs_both_parse() {
    // Distinct real/effective ids make the two claims differ while both stay
    // adoptable, so the returned claim proves the second header was reached.
    let mut cred = kcred::Cred::root();
    cred.set_reuid(Some(1001), Some(1002))
        .expect("root sets real/effective id");
    let sender = ScmSenderContext {
        pid: 4242,
        cred: Arc::new(cred),
        process: kprocess::init_proc(),
    };
    let mut buf = Vec::new();
    // A rights-sized record followed by a padded one: the second header must
    // be found at the aligned offset, not right after the first cmsg_len.
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1001, 0),
    );
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1002, 0),
    );

    let (msgs, credentials) = parse(&buf, &sender).expect("both cmsgs parse");
    assert!(msgs.is_empty());
    // Linux overwrites scm->creds per validated claim: the last one wins.
    let creds = credentials.expect("last claim wins");
    assert_eq!(creds.uid, 1002);
}

#[def_test]
fn own_credentials_claim_parses() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        // The sender's own pid and a uid/gid it holds (ruid==euid==suid here).
        &credentials_body(4242, 1000, 1000),
    );
    let (msgs, credentials) = parse(&buf, &sender).expect("own claim accepted");
    assert!(msgs.is_empty());
    let creds = credentials.expect("claim returned");
    assert_eq!((creds.uid, creds.gid), (1000, 1000));
}

#[def_test]
fn foreign_credentials_claim_is_rejected() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        // Neither the sender's pid nor its uid/gid, and no privilege: Linux
        // scm_check_creds rejects with EPERM.
        &credentials_body(9999, 0, 0),
    );
    assert!(matches!(
        parse(&buf, &sender),
        Err(ref e) if *e == KError::from(LinuxError::EPERM)
    ));
}

#[def_test]
fn short_or_overrunning_header_is_invalid() {
    let sender = sender_context(4242);

    // A buffer too small for even one header holds no message, like Linux
    // `CMSG_FIRSTHDR` returning NULL.
    assert_eq!(
        parse(&[0u8; size_of::<cmsghdr>() - 1], &sender)
            .unwrap()
            .0
            .len(),
        0
    );

    // A cmsg_len shorter than the header itself is malformed.
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000),
    );
    buf[0] = 0;
    buf[1] = 0;
    assert_eq!(parse_with_err(&buf, &sender), KError::InvalidInput);

    // cmsg_len overruns the buffer.
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000),
    );
    buf[0] = 0xff;
    assert_eq!(parse_with_err(&buf, &sender), KError::InvalidInput);

    // Unknown SOL_SOCKET type.
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_SOCKET, 0x7fff, &[]);
    assert_eq!(parse_with_err(&buf, &sender), KError::InvalidInput);
}

#[def_test]
fn trailing_partial_record_ends_the_walk() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000),
    );
    // A fragment that cannot hold another header is ignored, matching
    // Linux `__cmsg_nxthdr` returning NULL.
    buf.extend_from_slice(&[0u8; size_of::<cmsghdr>() - 1]);
    let (msgs, credentials) = parse(&buf, &sender).unwrap();
    assert!(msgs.is_empty());
    assert!(credentials.is_some());
}

#[def_test]
fn credentials_length_is_exact() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000)[..size_of::<ucred>() - 1],
    );
    assert_eq!(parse_with_err(&buf, &sender), KError::InvalidInput);
}

#[def_test]
fn non_sol_socket_levels_are_skipped() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(&mut buf, 0 /* IPPROTO_IP */, 1, &[1, 2, 3, 4]);
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000),
    );
    let (msgs, credentials) = parse(&buf, &sender).expect("foreign level skipped");
    assert!(msgs.is_empty());
    assert!(credentials.is_some());
}

#[def_test]
fn bad_rights_fd_wins_over_later_malformed_cmsg() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_SOCKET, SCM_RIGHTS, &100i32.to_ne_bytes());
    push_cmsg(&mut buf, SOL_SOCKET, 0x7fff, &[]);
    // Linux resolves descriptors inside the per-cmsg loop (scm_fp_copy), so
    // the earlier bad fd's EBADF preempts the later unknown type's EINVAL.
    let resolved = RefCell::new(Vec::new());
    let err = match parse_with(&buf, &sender, &mut |fds| {
        resolved.borrow_mut().push(fds.clone());
        if fds.contains(&100) {
            Err(KError::BadFileDescriptor)
        } else {
            Ok(CMsg::Rights { fds: Vec::new() })
        }
    }) {
        Ok(_) => panic!("expected EBADF"),
        Err(e) => e,
    };
    assert_eq!(err, KError::BadFileDescriptor);
    assert_eq!(*resolved.borrow(), vec![vec![100]]);
}

#[def_test]
fn invalid_credential_ids_are_einval() {
    // `(uid_t)-1` is an unmapped id: Linux rejects it with EINVAL before any
    // permission check, even for a privileged sender.
    let mut cred = kcred::Cred::root();
    cred.set_reuid(Some(1001), Some(1002))
        .expect("root sets ids");
    let sender = ScmSenderContext {
        pid: 4242,
        cred: Arc::new(cred),
        process: kprocess::init_proc(),
    };
    for body in [
        credentials_body(4242, u32::MAX, 1000),
        credentials_body(4242, 1000, u32::MAX),
    ] {
        let mut buf = Vec::new();
        push_cmsg(&mut buf, SOL_SOCKET, SCM_CREDENTIALS, &body);
        assert_eq!(parse_with_err(&buf, &sender), KError::InvalidInput);
    }
}

#[def_test]
fn ip_level_control_data_is_rejected_without_ip_cmsg_support() {
    // IP sockets: IP-level control data is not applied here, so it must fail
    // instead of being silently accepted.
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_IP, 8 /* IP_PKTINFO */, &[0u8; 12]);
    let err = parse_control_msgs(&buf, CmsgFamily::Other, None, &mut |_| {
        Ok(CMsg::Rights { fds: Vec::new() })
    })
    .err()
    .expect("SOL_IP rejected");
    assert_eq!(err, KError::InvalidInput);

    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_IPV6, 50 /* IPV6_PKTINFO */, &[0u8; 20]);
    assert_eq!(
        parse_control_msgs(&buf, CmsgFamily::Other, None, &mut |_| Ok(CMsg::Rights {
            fds: Vec::new()
        }),)
        .err()
        .expect("SOL_IPV6 rejected"),
        KError::InvalidInput
    );

    // Other non-SOL_SOCKET levels are skipped like Linux does for IP sockets.
    let mut buf = Vec::new();
    push_cmsg(&mut buf, 99, 1, &[1, 2, 3, 4]);
    let (msgs, credentials) = parse_control_msgs(&buf, CmsgFamily::Other, None, &mut |_| {
        Ok(CMsg::Rights { fds: Vec::new() })
    })
    .expect("unknown level skipped");
    assert!(msgs.is_empty());
    assert!(credentials.is_none());
}

#[def_test]
fn other_families_ignore_unix_control_messages() {
    // Non-Unix, non-netlink families do not run the generic SCM walk. Their
    // transport paths ignore SCM_RIGHTS and SCM_CREDENTIALS.
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_SOCKET, SCM_RIGHTS, &100i32.to_ne_bytes());
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(9999, 0, 0),
    );
    let mut resolved = false;
    let (msgs, credentials) = parse_control_msgs(&buf, CmsgFamily::Other, None, &mut |_fds| {
        resolved = true;
        Ok(CMsg::Rights { fds: Vec::new() })
    })
    .expect("unix-only control messages are ignored");
    assert!(msgs.is_empty());
    assert!(credentials.is_none());
    assert!(!resolved, "rights must not be resolved on non-Unix sockets");
}

#[def_test]
fn netlink_rejects_scm_rights_with_einval() {
    // `__scm_send` rejects descriptor passing on every non-Unix family, even
    // before it reaches the SCM_CREDENTIALS type check.
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_SOCKET, SCM_RIGHTS, &100i32.to_ne_bytes());
    assert!(matches!(
        parse_with_family(&buf, CmsgFamily::Netlink, &sender, &mut |fds| {
            let _ = fds.len();
            Ok(CMsg::Rights { fds: Vec::new() })
        }),
        Err(KError::InvalidInput)
    ));
}

#[def_test]
fn netlink_accepts_scm_credentials_claim() {
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4242, 1000, 1000),
    );
    let (msgs, credentials) = parse_with_family(&buf, CmsgFamily::Netlink, &sender, &mut |_| {
        Ok(CMsg::Rights { fds: Vec::new() })
    })
    .expect("netlink accepts SCM_CREDENTIALS");
    assert!(msgs.is_empty());
    let credentials = credentials.expect("claim returned");
    assert_eq!(credentials.uid, 1000);
    assert_eq!(credentials.gid, 1000);
}

#[def_test]
fn netlink_ignores_non_socket_cmsg_levels() {
    // Netlink never calls `sock_cmsg_send`, so IP-level and other unknown
    // levels are ignored rather than validated.
    let sender = sender_context(4242);
    let mut buf = Vec::new();
    push_cmsg(&mut buf, SOL_IP, 8 /* IP_PKTINFO */, &[0u8; 12]);
    push_cmsg(&mut buf, 99, 1, &[1, 2, 3, 4]);
    let (msgs, credentials) = parse_with_family(&buf, CmsgFamily::Netlink, &sender, &mut |_| {
        Ok(CMsg::Rights { fds: Vec::new() })
    })
    .expect("non-SOL_SOCKET levels ignored");
    assert!(msgs.is_empty());
    assert!(credentials.is_none());
}

#[def_test]
fn privileged_foreign_pid_claim_resolves_the_named_process() {
    let sender = ScmSenderContext {
        pid: 4242,
        cred: Arc::new(kcred::Cred::root()),
        process: kprocess::init_proc(),
    };
    let init = kprocess::init_proc();
    let claim_pid = init.visible_pid();
    let mut buf = Vec::new();
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(claim_pid, 0, 0),
    );

    let (_, credentials) = parse(&buf, &sender).expect("privileged claim resolves");
    let resolved = credentials.expect("claim returned");
    // Linux stores find_get_pid(claim.pid): delivery names the resolved task.
    // init is the only process published in the unit-test environment, so this
    // cannot tell the lookup apart from sender substitution; the companion
    // ESRCH test rejects an implementation that skips the lookup entirely.
    let identity = resolved.pid.as_ref().expect("claim carries an identity");
    assert_eq!(identity.root_nr(), init.pid());
}

#[def_test]
fn privileged_claim_naming_a_missing_pid_is_esrch() {
    let sender = ScmSenderContext {
        pid: 4242,
        cred: Arc::new(kcred::Cred::root()),
        process: kprocess::init_proc(),
    };
    let mut buf = Vec::new();
    // A pid with no published process in the sender's namespace.
    push_cmsg(
        &mut buf,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(4_000_001, 0, 0),
    );
    assert_eq!(parse_with_err(&buf, &sender), KError::NoSuchProcess);
}

#[def_test(user)]
fn netlink_sendmsg_forwards_validated_explicit_credentials() {
    use core::mem::size_of;

    use kio::Cursor;
    use knet::{
        KernelAncillaryData, RecvOptions, SocketAddrEx, SocketOps,
        netlink::{NETLINK_ROUTE, NetlinkAddr, NetlinkSocket, NetlinkSocketType},
        options::{Configurable, SetSocketOption},
    };
    use linux_raw_sys::net::{AF_NETLINK, SOCK_RAW, msghdr};
    use posix_types::{IoVec, UserConstPtr, net::socket_addr::sockaddr_nl};
    use unittest_support::TestUserArray;

    let subscriber = NetlinkSocket::new(NETLINK_ROUTE, NetlinkSocketType::Raw);
    subscriber
        .bind(SocketAddrEx::Netlink(NetlinkAddr {
            pid: 777,
            groups: 0,
        }))
        .unwrap();
    subscriber
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();

    let claim_pid = kprocess::init_proc().visible_pid();
    let mut control = [0u8; 32];
    control[0..8].copy_from_slice(&28usize.to_ne_bytes());
    control[8..12].copy_from_slice(&(SOL_SOCKET as i32).to_ne_bytes());
    control[12..16].copy_from_slice(&(SCM_CREDENTIALS as i32).to_ne_bytes());
    control[16..20].copy_from_slice(&claim_pid.to_ne_bytes());
    control[20..24].copy_from_slice(&4321u32.to_ne_bytes());
    control[24..28].copy_from_slice(&8765u32.to_ne_bytes());
    let control = TestUserArray::from_array(control).unwrap();

    let payload = TestUserArray::from_array([0x5a_u8]).unwrap();
    let iovec = TestUserArray::from_array([IoVec {
        iov_base: payload.as_user_ptr(),
        iov_len: 1,
    }])
    .unwrap();
    let destination = TestUserArray::from_array([sockaddr_nl {
        nl_family: AF_NETLINK as u16,
        nl_pad: 0,
        nl_pid: 777,
        nl_groups: 0,
    }])
    .unwrap();
    let message = TestUserArray::from_array([msghdr {
        msg_name: destination.as_user_ptr().cast(),
        msg_namelen: size_of::<sockaddr_nl>() as i32,
        msg_iov: iovec.as_user_ptr().cast(),
        msg_iovlen: 1,
        msg_control: control.as_user_ptr().cast(),
        msg_controllen: control.len(),
        msg_flags: 0,
    }])
    .unwrap();

    let fd = crate::sys_socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE as u32).unwrap() as i32;
    assert_eq!(
        crate::sys_sendmsg(
            fd,
            UserConstPtr::from(message.as_user_ptr().cast_const()),
            0,
        ),
        Ok(1)
    );

    let mut data = [0u8; 1];
    let mut ancillary = Vec::new();
    subscriber
        .recv(
            Cursor::new(data.as_mut_slice()),
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(data, [0x5a]);
    let credentials = ancillary
        .pop()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected SCM_CREDENTIALS ancillary data");
    };
    let identity = credentials.pid.as_ref().expect("claimed pid resolved");
    assert_eq!(identity.root_nr(), claim_pid);
    assert_eq!(credentials.uid, 4321);
    assert_eq!(credentials.gid, 8765);

    kprocess::current_files().close_file(fd).unwrap();
}

#[def_test]
fn scm_credentials_translate_a_namespaced_claim_to_global_ids() {
    use alloc::sync::Arc;

    let initial = kcred::initial_user_namespace();
    let child = kcred::UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    child
        .write_uid_map(
            &kcred::Cred::root(),
            &kcred::Cred::root(),
            alloc::vec![kcred::IdMapExtent::new(0, 1000, 1).unwrap()],
        )
        .unwrap();
    child
        .write_gid_map(
            &kcred::Cred::root(),
            &kcred::Cred::root(),
            alloc::vec![kcred::IdMapExtent::new(0, 1000, 1).unwrap()],
        )
        .unwrap();

    let pid = kprocess::init_proc().pid();
    let sender = ScmSenderContext {
        pid,
        cred: Arc::new(kcred::Cred::new(1000, 1000).with_user_namespace(child)),
        process: kprocess::init_proc(),
    };
    let mut control = Vec::new();
    push_cmsg(
        &mut control,
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &credentials_body(pid, 0, 0),
    );
    let (_, creds) = parse(&control, &sender).unwrap();
    let creds = creds.unwrap();
    assert_eq!(creds.uid, 1000);
    assert_eq!(creds.gid, 1000);
}
