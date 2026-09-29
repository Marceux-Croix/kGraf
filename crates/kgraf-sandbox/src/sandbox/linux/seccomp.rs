//! seccomp filter for the Linux backend: socket families, cross-process
//! access and other escape syscalls, answered with EPERM.
//!
//! Ported (subset, reworked) from codex-rs `linux-sandbox/src/landlock.rs`
//! (`install_network_seccomp_filter_on_current_thread`, openai/codex commit
//! e72da2b5, Apache-2.0; see `third_party/codex/UPSTREAM`): the ptrace,
//! process_vm and io_uring denials, the socket-domain conditions and the use
//! of `seccompiler` to compile the BPF program.
//!
//! Deliberately different from upstream:
//! - `socket(AF_UNIX)` is denied in every mode. Upstream's restricted mode
//!   allows it and denies `connect`/`sendto` instead, which leaves `sendmsg`
//!   with a destination path open (a datagram to `/run/systemd/notify` or
//!   `/dev/log`). Without a unix socket no path can be reached at all;
//!   `socketpair(AF_UNIX)` stays open, which is what build tools use.
//! - With network allowed, `socket` is limited to AF_INET and AF_INET6.
//!   Upstream installs no filter at all in that mode, which leaves every
//!   unix socket (docker.sock, ssh-agent) reachable.
//! - The kernel keyring, mount-family calls, `setns`, `bpf`,
//!   `perf_event_open` and terminal injection (`TIOCSTI`, `TIOCLINUX`) are
//!   denied too.
//! - On x86_64 every x32-ABI syscall number is denied: seccompiler checks the
//!   audit architecture only, and x32 calls share AUDIT_ARCH_X86_64, so
//!   `socket` with the x32 bit set would otherwise pass the filter. A call
//!   through the i386 ABI (`int 0x80`) fails that architecture check and
//!   kills the process (SIGSYS).
//! - No new namespaces: `unshare` and `clone` with any CLONE_NEW* flag get
//!   EPERM, and `clone3` (whose flags live in memory a filter cannot read)
//!   gets ENOSYS, so libc falls back to `clone`. A nested user namespace
//!   would hand the worker CAP_NET_ADMIN and CAP_SYS_ADMIN over new
//!   namespaces, and with them netfilter and similar kernel attack surface
//!   (the CVE-2021-22555 class).

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch, sock_filter,
};

use super::super::Network;
use crate::error::SandboxError;

/// Syscalls denied outright, in every mode.
const DENIED: &[libc::c_long] = &[
    // Reading or writing another process's memory. Landlock already refuses
    // ptrace of a process outside the domain; this closes it for good.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
    // io_uring can open sockets without calling socket() (IORING_OP_SOCKET).
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    // The user's session keyring can hold credentials.
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    // Mount topology. Landlock refuses these too; denied here so a nested
    // user namespace cannot reach them either.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_mount_setattr,
    // Joining another process's namespaces.
    libc::SYS_setns,
    // Kernel attack surface with no use in a build.
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    // File handles bypass path lookup (and so Landlock's path rules).
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    // Inspecting or steering other processes.
    libc::SYS_kcmp,
    libc::SYS_process_madvise,
];

/// Every CLONE_NEW* flag `clone` accepts. CLONE_NEWTIME (0x80) is valid for
/// `unshare` only: in `clone`'s first argument the low byte is the exit
/// signal.
const CLONE_NEW_FLAGS: &[libc::c_int] = &[
    libc::CLONE_NEWNS,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
];

fn cond(arg: u8, op: SeccompCmpOp, value: u64) -> Result<SeccompCondition, SandboxError> {
    SeccompCondition::new(arg, SeccompCmpArgLen::Dword, op, value).map_err(build_error)
}

fn rule(conditions: Vec<SeccompCondition>) -> Result<SeccompRule, SandboxError> {
    SeccompRule::new(conditions).map_err(build_error)
}

fn build_error(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::KernelUnsupported(format!("cannot build the seccomp filter: {e}"))
}

fn target_arch() -> Result<TargetArch, SandboxError> {
    if cfg!(target_arch = "x86_64") {
        Ok(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(TargetArch::aarch64)
    } else if cfg!(target_arch = "riscv64") {
        Ok(TargetArch::riscv64)
    } else {
        Err(SandboxError::KernelUnsupported(format!(
            "no seccomp filter for architecture {}",
            std::env::consts::ARCH
        )))
    }
}

/// Compile the filter in the parent, so a failure is an error before any
/// process starts and the child only has to install it.
pub(super) fn build(network: Network) -> Result<BpfProgram, SandboxError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in DENIED {
        // An empty rule list matches unconditionally.
        rules.insert(*nr, Vec::new());
    }
    let af = |v: libc::c_int| u64::try_from(v).unwrap_or(u64::MAX);
    let socket = match network {
        Network::Deny => Vec::new(),
        // Deny when the domain is neither AF_INET nor AF_INET6 (the
        // conditions of one rule are ANDed).
        Network::Allow => vec![rule(vec![
            cond(0, SeccompCmpOp::Ne, af(libc::AF_INET))?,
            cond(0, SeccompCmpOp::Ne, af(libc::AF_INET6))?,
        ])?],
    };
    rules.insert(libc::SYS_socket, socket);
    rules.insert(
        libc::SYS_socketpair,
        vec![rule(vec![cond(0, SeccompCmpOp::Ne, af(libc::AF_UNIX))?])?],
    );
    // One rule per flag (rules for one syscall are ORed): deny when that bit
    // is set, whatever else is.
    let flag_rules = |flags: &[libc::c_int]| -> Result<Vec<SeccompRule>, SandboxError> {
        flags
            .iter()
            .map(|f| {
                let bit = u64::from(f.cast_unsigned());
                rule(vec![cond(0, SeccompCmpOp::MaskedEq(bit), bit)?])
            })
            .collect()
    };
    rules.insert(libc::SYS_clone, flag_rules(CLONE_NEW_FLAGS)?);
    let mut unshare = CLONE_NEW_FLAGS.to_vec();
    unshare.push(libc::CLONE_NEWTIME);
    rules.insert(libc::SYS_unshare, flag_rules(&unshare)?);
    // The ioctl request is an int to the kernel: compare the low 32 bits,
    // so high bits set by the caller cannot slip past.
    rules.insert(
        libc::SYS_ioctl,
        vec![
            rule(vec![cond(1, SeccompCmpOp::Eq, libc::TIOCSTI)?])?,
            rule(vec![cond(1, SeccompCmpOp::Eq, libc::TIOCLINUX)?])?,
        ],
    );

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )
    .map_err(build_error)?;
    let mut program: BpfProgram = filter.try_into().map_err(build_error)?;
    insert_guards(&mut program, cfg!(target_arch = "x86_64"));
    Ok(program)
}

/// Insert, right after seccompiler's three-instruction architecture check,
/// the tests seccompiler cannot express (it has one match action):
/// `clone3` answers ENOSYS, and on x86_64 any syscall number with the x32
/// bit (0x4000_0000) set answers EPERM. The check's `jeq` jumps one
/// instruction forward, onto the first instruction inserted here; every
/// later jump is relative and stays within the part after the insertion.
fn insert_guards(program: &mut BpfProgram, x32: bool) {
    const ARCH_CHECK_LEN: usize = 3;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    let stmt = |code: u16, k: u32| sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    // if A <op> k fall through to the RET that follows, else skip it
    let jump = |op: u16, k: u32| sock_filter {
        code: BPF_JMP | op | BPF_K,
        jt: 0,
        jf: 1,
        k,
    };
    let errno = |e: i32| stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ERRNO | e.cast_unsigned());
    let clone3 = u32::try_from(libc::SYS_clone3).unwrap_or(u32::MAX);
    // A = seccomp_data.nr (offset 0)
    let mut guard = vec![stmt(BPF_LD | BPF_W | BPF_ABS, 0)];
    if x32 {
        guard.extend([jump(BPF_JGE, X32_SYSCALL_BIT), errno(libc::EPERM)]);
    }
    guard.extend([jump(BPF_JEQ, clone3), errno(libc::ENOSYS)]);
    program.splice(ARCH_CHECK_LEN..ARCH_CHECK_LEN, guard);
}

// Classic BPF opcodes (linux/bpf_common.h).
const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_K: u16 = 0x00;

/// Install `program` on the calling thread. Called in the forked child:
/// one `seccomp` syscall on a stack struct, no allocation. The caller has
/// already set `no_new_privs` (Landlock's `restrict_self` does).
pub(super) fn install(program: &[sock_filter]) -> Result<(), i32> {
    let len = u16::try_from(program.len()).map_err(|_| libc::E2BIG)?;
    let prog = libc::sock_fprog {
        len,
        // The kernel copies the program; it only reads through this pointer.
        filter: program.as_ptr().cast_mut().cast(),
    };
    // SAFETY: `prog` points at `len` valid instructions for the duration of
    // the call; SECCOMP_SET_MODE_FILTER with flags 0 reads them and returns.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &prog as *const libc::sock_fprog,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL))
    }
}

/// Whether the kernel supports seccomp filters returning an errno
/// (`SECCOMP_GET_ACTION_AVAIL`, Linux 4.14). Changes nothing.
pub(super) fn available() -> bool {
    const SECCOMP_GET_ACTION_AVAIL: libc::c_ulong = 2;
    let action: u32 = libc::SECCOMP_RET_ERRNO;
    // SAFETY: the kernel reads one u32 from `action`, which outlives the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_GET_ACTION_AVAIL,
            0,
            &action as *const u32,
        )
    };
    rc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_compiles_for_both_network_modes() {
        for network in [Network::Allow, Network::Deny] {
            let program = build(network).unwrap();
            assert!(program.len() > DENIED.len(), "{network:?}");
        }
    }

    #[test]
    fn the_guards_sit_right_after_the_architecture_check() {
        let program = build(Network::Deny).unwrap();
        assert_eq!(program[3].code, BPF_LD | BPF_W | BPF_ABS);
        let mut i = 4;
        if cfg!(target_arch = "x86_64") {
            assert_eq!(program[i].k, 0x4000_0000);
            assert_eq!(
                program[i + 1].k,
                libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
            );
            i += 2;
        }
        assert_eq!(i64::from(program[i].k), libc::SYS_clone3);
        assert_eq!(
            program[i + 1].k,
            libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32
        );
    }
}
