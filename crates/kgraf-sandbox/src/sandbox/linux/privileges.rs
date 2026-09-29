//! What the child still holds from its parent: inherited descriptors and
//! capabilities. Both would bypass Landlock and seccomp, which check only
//! new opens and new sockets. Child only: raw syscalls, no allocation.

use super::namespaces::StepError;

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

/// Mark every descriptor from 3 up close-on-exec, so the command starts
/// with stdin, stdout and stderr only. A descriptor the parent leaked
/// without O_CLOEXEC (a writable file outside the roots, a connected unix
/// socket) would otherwise pass straight through the sandbox. Marking
/// instead of closing keeps std's exec-status pipe and the setup reporter
/// working until exec.
///
/// `close_range(CLOSE_RANGE_CLOEXEC)` exists since Linux 5.11; Landlock ABI
/// 3 needs 6.2, but a vendor kernel may backport Landlock, so on ENOSYS or
/// EINVAL the descriptors are listed from `/proc/self/fd` with `getdents64`.
pub(super) fn cloexec_inherited() -> Result<(), StepError> {
    // SAFETY: close_range takes plain integers.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = errno();
    if e != libc::ENOSYS && e != libc::EINVAL {
        return Err(("close_range(CLOSE_RANGE_CLOEXEC) failed", e));
    }
    cloexec_by_listing()
}

fn cloexec_by_listing() -> Result<(), StepError> {
    // SAFETY: open, getdents64 into a stack buffer, fcntl and close on the
    // descriptor opened here. Records are parsed within the returned length.
    unsafe {
        let dir = libc::open(
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if dir < 0 {
            return Err(("cannot list /proc/self/fd", errno()));
        }
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::syscall(libc::SYS_getdents64, dir, buf.as_mut_ptr(), buf.len());
            let Ok(n) = usize::try_from(n) else {
                let e = errno();
                libc::close(dir);
                return Err(("getdents64 on /proc/self/fd failed", e));
            };
            if n == 0 {
                break;
            }
            let mut off = 0;
            // struct linux_dirent64: u64 ino, i64 off, u16 reclen, u8 type,
            // then the NUL-terminated name at byte 19.
            while off + 19 < n {
                let reclen = usize::from(u16::from_ne_bytes([buf[off + 16], buf[off + 17]]));
                if reclen == 0 {
                    break;
                }
                let mut fd: i32 = 0;
                let mut digits = 0;
                let mut i = off + 19;
                while i < off + reclen && buf[i].is_ascii_digit() {
                    fd = fd
                        .saturating_mul(10)
                        .saturating_add(i32::from(buf[i] - b'0'));
                    digits += 1;
                    i += 1;
                }
                if digits > 0 && fd >= 3 && fd != dir {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) != 0 {
                        let e = errno();
                        libc::close(dir);
                        return Err(("cannot mark an inherited descriptor close-on-exec", e));
                    }
                }
                off += reclen;
            }
        }
        libc::close(dir);
    }
    Ok(())
}

const CAP_SETPCAP: u32 = 8;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Drop every capability before exec: the bounding set (when the process
/// may, that is as root or inside its own user namespace), the ambient set,
/// then effective, permitted and inheritable. With `no_new_privs` already
/// set (Landlock's restrict_self sets it), exec cannot hand them back, even
/// to uid 0. Without this, a sandbox run as root keeps CAP_SYS_MODULE,
/// CAP_KILL, CAP_NET_RAW and the rest.
pub(super) fn drop_capabilities() -> Result<(), StepError> {
    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capget/capset read and write the two stack structs above;
    // prctl takes plain integers.
    unsafe {
        if libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) != 0 {
            return Err(("capget failed", errno()));
        }
        if data[0].effective & (1 << CAP_SETPCAP) != 0 {
            let mut cap: libc::c_ulong = 0;
            loop {
                if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                    // EINVAL: past the kernel's last capability.
                    if errno() == libc::EINVAL && cap > 0 {
                        break;
                    }
                    return Err(("cannot drop the capability bounding set", errno()));
                }
                cap += 1;
            }
        }
        if libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        ) != 0
        {
            return Err(("cannot clear ambient capabilities", errno()));
        }
        let zero = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        header.version = LINUX_CAPABILITY_VERSION_3;
        header.pid = 0;
        if libc::syscall(libc::SYS_capset, &header, zero.as_ptr()) != 0 {
            return Err(("capset to empty failed", errno()));
        }
    }
    Ok(())
}
