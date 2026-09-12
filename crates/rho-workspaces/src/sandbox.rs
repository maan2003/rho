//! Landlock policy for VCS-masked sandbox workspaces.

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: i32 = 1;
const REQUIRED_ABI: i32 = 7;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const FS_READ: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
const FS_FILE_RW: u64 = FS_READ_FILE | FS_WRITE_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
const FS_ALL: u64 = FS_READ
    | FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM
    | FS_REFER
    | FS_TRUNCATE
    | FS_IOCTL_DEV;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const NET_ALL: u64 = NET_BIND_TCP | NET_CONNECT_TCP;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// A complete ruleset fd, built before fork and applied in the child's
/// `pre_exec` hook using only async-signal-safe syscalls.
#[derive(Debug)]
pub struct Policy {
    ruleset: OwnedFd,
}

impl Policy {
    pub fn new(writable: &[PathBuf], path: &OsStr) -> anyhow::Result<Self> {
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                CREATE_RULESET_VERSION,
            )
        } as i32;
        anyhow::ensure!(
            abi >= REQUIRED_ABI,
            "sandbox requires Landlock ABI {REQUIRED_ABI} (kernel provides {abi})"
        );

        let attr = RulesetAttr {
            handled_access_fs: FS_ALL,
            handled_access_net: NET_ALL,
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr,
                std::mem::size_of::<RulesetAttr>(),
                0,
            )
        } as i32;
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("create Landlock ruleset");
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut read_only = vec![
            PathBuf::from("/nix/store"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/etc"),
            PathBuf::from("/proc"),
            PathBuf::from("/sys"),
            PathBuf::from("/dev"),
        ];
        read_only.extend(std::env::split_paths(path).filter(|path| path.is_absolute()));
        read_only.sort();
        read_only.dedup();
        for path in read_only.into_iter().filter(|path| path.exists()) {
            add_path_rule(&ruleset, &path, FS_READ)
                .with_context(|| format!("allow sandbox runtime path {}", path.display()))?;
        }

        for path in writable {
            add_path_rule(&ruleset, path, FS_ALL)
                .with_context(|| format!("allow writable sandbox path {}", path.display()))?;
        }
        for path in [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
        ]
        .into_iter()
        .map(Path::new)
        .filter(|path| path.exists())
        {
            add_path_rule(&ruleset, path, FS_FILE_RW)
                .with_context(|| format!("allow writable device path {}", path.display()))?;
        }
        let pts = Path::new("/dev/pts");
        if pts.exists() {
            add_path_rule(&ruleset, pts, FS_ALL).context("allow writable /dev/pts")?;
        }
        Ok(Self { ruleset })
    }

    pub fn restrict_self(&self) -> std::io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        restrict_socket_families()?;
        Ok(())
    }
}

/// Landlock ABI 7 restricts TCP but not UDP. Deny creation of every socket
/// family except Unix sockets; pathname access and abstract-socket scoping
/// remain governed by Landlock. Child commands inherit the filter.
fn restrict_socket_families() -> std::io::Result<()> {
    const LD_W_ABS: u16 = 0x20;
    const JMP_JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    let mut filter = [
        stmt(LD_W_ABS, 4),
        jump(JMP_JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, 0),
        jump(JMP_JEQ_K, libc::SYS_socket as u32, 0, 3),
        stmt(LD_W_ABS, 16),
        jump(JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0),
        stmt(RET_K, RET_ERRNO | libc::EACCES as u32),
        stmt(RET_K, RET_ALLOW),
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

const fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn add_path_rule(ruleset: &OwnedFd, path: &Path, allowed_access: u64) -> anyhow::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).context("Landlock path contains NUL")?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open Landlock path");
    }
    let parent = unsafe { OwnedFd::from_raw_fd(fd) };
    let attr = PathBeneathAttr {
        allowed_access,
        parent_fd: parent.as_raw_fd(),
    };
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            RULE_PATH_BENEATH,
            &attr,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("add Landlock path rule");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{TcpStream, UdpSocket};
    use std::process::Command;

    use super::Policy;

    const CHILD_ENV: &str = "RHO_LANDLOCK_TEST_CHILD";

    #[test]
    fn landlock_child_helper() {
        let Some(allowed) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let allowed = std::path::PathBuf::from(allowed);
        let denied =
            std::path::PathBuf::from(std::env::var_os("RHO_LANDLOCK_TEST_DENIED").unwrap());
        let path = std::env::var_os("PATH").unwrap();
        let policy = Policy::new(std::slice::from_ref(&allowed), &path).unwrap();
        policy.restrict_self().unwrap();

        std::fs::write(allowed.join("written"), "ok").unwrap();
        assert!(std::fs::read_to_string(denied).is_err());
        assert!(TcpStream::connect("127.0.0.1:9").is_err());
        let udp = UdpSocket::bind("127.0.0.1:0");
        assert!(udp.is_err(), "UDP bind unexpectedly escaped Landlock");
    }

    #[test]
    fn restricts_filesystem_and_network_in_child() {
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let denied = temp.path().join("denied");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::write(&denied, "secret").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::tests::landlock_child_helper",
                "--nocapture",
            ])
            .env(CHILD_ENV, &allowed)
            .env("RHO_LANDLOCK_TEST_DENIED", &denied)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Landlock child failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(allowed.join("written")).unwrap(),
            "ok"
        );
    }
}
