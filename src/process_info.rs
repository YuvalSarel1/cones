//! Read the known process directly. A system-wide lsof scan added hundreds of milliseconds
//! to every refresh. These are the same kernel-reported paths, with no cached liveness:
//! libproc on macOS, procfs and netlink on Linux.
use std::{io, path::PathBuf};

/// lsof, for a process the direct read may not open. Linux distributions put it on `PATH`.
#[cfg(target_os = "macos")]
pub const LSOF: &str = "/usr/sbin/lsof";
#[cfg(not(target_os = "macos"))]
pub const LSOF: &str = "lsof";

/// A process's working directory, read from the kernel. Counted as an open-file read: a pass
/// asks once per process it found, and the fallback for a process it may not read is `lsof`.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    crate::observe::read(
        crate::observe::op::OPEN_FILES,
        || cwd_of(pid),
        Option::is_some,
    )
}

/// [`cwd`] uncounted, for a caller that asks on every drawn frame.
pub fn cwd_of(pid: u32) -> Option<PathBuf> {
    imp::cwd_of(pid)
}

/// The parent's pid, `None` once the process has gone.
pub fn parent(pid: u32) -> Option<u32> {
    imp::parent(pid)
}

pub struct OpenFile {
    pub device: u64,
    pub inode: u64,
    pub path: Option<PathBuf>,
}

pub fn open_files(pid: u32) -> io::Result<Vec<OpenFile>> {
    crate::observe::read(
        crate::observe::op::OPEN_FILES,
        || imp::open_files_of(pid),
        Result::is_ok,
    )
}

/// A Unix-domain socket a process holds, by the kernel's handles as lsof prints them: its own
/// and the one it is connected to, zero when unconnected. A connection is the pair of sockets
/// whose handles name each other, whichever processes hold them.
pub struct UnixSocket {
    pub handle: u64,
    pub peer: u64,
}

pub fn unix_sockets(pid: u32) -> io::Result<Vec<UnixSocket>> {
    crate::observe::read(
        crate::observe::op::OPEN_FILES,
        || imp::unix_sockets_of(pid),
        Result::is_ok,
    )
}

#[cfg(target_os = "macos")]
mod imp {
    //! libproc: the same kernel-reported paths lsof prints.
    use super::{OpenFile, UnixSocket};
    use std::{ffi::OsString, io, mem::size_of, os::unix::ffi::OsStringExt, path::PathBuf};

    #[link(name = "proc")]
    unsafe extern "C" {}

    // sys/proc_info.h: libc exposes vnode_info_path and the calls, but not these two wrappers.
    #[repr(C)]
    struct ProcFileInfo {
        open_flags: u32,
        status: u32,
        offset: libc::off_t,
        file_type: i32,
        guard_flags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfo {
        file: ProcFileInfo,
        vnode: libc::vnode_info_path,
    }

    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    const SOCKINFO_UN: i32 = 3;

    // sys/proc_info.h: sockbuf_info, socket_info and socket_fdinfo, also absent from libc.
    #[repr(C)]
    struct SockbufInfo {
        cc: u32,
        hiwat: u32,
        mbcnt: u32,
        mbmax: u32,
        lowat: u32,
        flags: i16,
        timeo: i16,
    }

    #[repr(C)]
    struct SocketInfo {
        stat: libc::vinfo_stat,
        so: u64,
        pcb: u64,
        kind_type: i32,
        protocol: i32,
        family: i32,
        options: i16,
        linger: i16,
        state: i16,
        qlen: i16,
        incqlen: i16,
        qlimit: i16,
        timeo: i16,
        error: u16,
        oobmark: u32,
        rcv: SockbufInfo,
        snd: SockbufInfo,
        kind: i32,
        rfu: u32,
        /// socket_info.soi_proto; for SOCKINFO_UN, pri_un.unsi_conn_so is the first field.
        proto: [u64; 66],
    }

    #[repr(C)]
    struct SocketFdInfo {
        file: ProcFileInfo,
        socket: SocketInfo,
    }

    // The kernel rejects a buffer smaller than its own `struct socket_fdinfo`.
    const _: () = assert!(size_of::<SocketFdInfo>() == 792);

    fn path(value: &libc::vnode_info_path) -> Option<PathBuf> {
        let bytes: Vec<u8> = value
            .vip_path
            .iter()
            .flatten()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        (bytes.first() == Some(&b'/') && bytes.len() < libc::MAXPATHLEN as usize)
            .then(|| PathBuf::from(OsString::from_vec(bytes)))
    }

    pub fn cwd_of(pid: u32) -> Option<PathBuf> {
        let pid = i32::try_from(pid).ok().filter(|&p| p > 1)?;
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        (read == size).then(|| path(&info.pvi_cdir)).flatten()
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let pid = i32::try_from(pid).ok().filter(|p| *p > 1)?;
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (read == size).then_some(info.pbi_ppid)
    }

    pub fn open_files_of(pid: u32) -> io::Result<Vec<OpenFile>> {
        let (pid, fds) = fds(pid)?;
        let mut paths = Vec::new();
        for fd in fds
            .iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
            let size = size_of::<VnodeFdInfo>() as libc::c_int;
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    (&mut info as *mut VnodeFdInfo).cast(),
                    size,
                )
            };
            // Descriptors can close between the list and this read.
            if read == size {
                paths.push(OpenFile {
                    device: u64::from(info.vnode.vip_vi.vi_stat.vst_dev),
                    inode: info.vnode.vip_vi.vi_stat.vst_ino,
                    path: path(&info.vnode),
                });
            }
        }
        Ok(paths)
    }

    pub fn unix_sockets_of(pid: u32) -> io::Result<Vec<UnixSocket>> {
        let (pid, fds) = fds(pid)?;
        let mut sockets = Vec::new();
        for fd in fds
            .iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
        {
            let mut info: SocketFdInfo = unsafe { std::mem::zeroed() };
            let size = size_of::<SocketFdInfo>() as libc::c_int;
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDSOCKETINFO,
                    (&mut info as *mut SocketFdInfo).cast(),
                    size,
                )
            };
            if read == size && info.socket.kind == SOCKINFO_UN {
                sockets.push(UnixSocket {
                    handle: info.socket.so,
                    peer: info.socket.proto[0],
                });
            }
        }
        Ok(sockets)
    }

    /// The process's descriptors, with the pid as libproc takes it.
    fn fds(pid: u32) -> io::Result<(i32, Vec<libc::proc_fdinfo>)> {
        let pid = i32::try_from(pid)
            .ok()
            .filter(|&p| p > 1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"))?;
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut capacity = needed as usize / size_of::<libc::proc_fdinfo>() + 128;
        for _ in 0..3 {
            let mut fds = vec![
                libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0
                };
                capacity
            ];
            let bytes = i32::try_from(fds.len() * size_of::<libc::proc_fdinfo>())
                .map_err(|_| io::Error::other("process fd list is too large"))?;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    fds.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if read < 0 {
                return Err(io::Error::last_os_error());
            }
            if read >= bytes {
                capacity *= 2;
                continue;
            }
            fds.truncate(read as usize / size_of::<libc::proc_fdinfo>());
            return Ok((pid, fds));
        }
        Err(io::Error::other("process fd list kept growing"))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! procfs for paths and parents; `sock_diag` netlink for which Unix socket a socket is
    //! connected to, which procfs does not say.
    use super::{OpenFile, UnixSocket};
    use std::{
        collections::HashMap,
        fs, io,
        mem::size_of,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::fs::MetadataExt,
        },
        path::PathBuf,
    };

    fn proc(pid: u32, rest: &str) -> Option<PathBuf> {
        (pid > 1 && i32::try_from(pid).is_ok()).then(|| format!("/proc/{pid}/{rest}").into())
    }

    pub fn cwd_of(pid: u32) -> Option<PathBuf> {
        fs::read_link(proc(pid, "cwd")?)
            .ok()
            .filter(|path| path.is_absolute())
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(proc(pid, "stat")?).ok()?;
        // The command name in parentheses may hold spaces and parentheses of its own.
        let (_, after) = stat.rsplit_once(')')?;
        after.split_whitespace().nth(1)?.parse().ok()
    }

    /// Each descriptor's link target, read from `/proc/<pid>/fd`.
    fn links(pid: u32) -> io::Result<Vec<(PathBuf, PathBuf)>> {
        let dir = proc(pid, "fd")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"))?;
        Ok(fs::read_dir(dir)?
            .flatten()
            // Descriptors can close between the list and this read.
            .filter_map(|fd| Some((fd.path(), fs::read_link(fd.path()).ok()?)))
            .collect())
    }

    pub fn open_files_of(pid: u32) -> io::Result<Vec<OpenFile>> {
        Ok(links(pid)?
            .into_iter()
            // Pipes, sockets and anonymous inodes link to `type:[n]`, not to a path.
            .filter(|(_, target)| target.is_absolute())
            .filter_map(|(fd, target)| {
                let metadata = fs::metadata(&fd).ok()?;
                Some(OpenFile {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    path: Some(target),
                })
            })
            .collect())
    }

    pub fn unix_sockets_of(pid: u32) -> io::Result<Vec<UnixSocket>> {
        let held: Vec<u64> = links(pid)?
            .into_iter()
            .filter_map(|(_, target)| {
                target
                    .to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse()
                    .ok()
            })
            .collect();
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let peers = unix_peers()?;
        Ok(held
            .into_iter()
            .filter_map(|handle| {
                Some(UnixSocket {
                    handle,
                    peer: *peers.get(&handle)?,
                })
            })
            .collect())
    }

    // linux/sock_diag.h and linux/unix_diag.h; libc carries the netlink header but not these.
    const NETLINK_SOCK_DIAG: libc::c_int = 4;
    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    const UDIAG_SHOW_PEER: u32 = 4;
    const UNIX_DIAG_PEER: u16 = 2;
    const NLMSG_HEADER: usize = 16;
    const UNIX_DIAG_MSG: usize = 16;

    #[repr(C)]
    struct Request {
        header: libc::nlmsghdr,
        family: u8,
        protocol: u8,
        pad: u16,
        states: u32,
        inode: u32,
        show: u32,
        cookie: [u32; 2],
    }

    fn word(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }

    fn half(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_ne_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
    }

    const fn align(n: usize) -> usize {
        (n + 3) & !3
    }

    /// Every Unix socket on the machine by inode, with the inode of the socket it is connected
    /// to, zero when unconnected. One dump answers for every descriptor of one process.
    // ponytail: one dump per process asked about, a few per pass; share it per pass if a
    // machine with many thousands of sockets makes that slow.
    fn unix_peers() -> io::Result<HashMap<u64, u64>> {
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                NETLINK_SOCK_DIAG,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let request = Request {
            header: libc::nlmsghdr {
                nlmsg_len: size_of::<Request>() as u32,
                nlmsg_type: SOCK_DIAG_BY_FAMILY,
                nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16,
                nlmsg_seq: 1,
                nlmsg_pid: 0,
            },
            family: libc::AF_UNIX as u8,
            protocol: 0,
            pad: 0,
            states: u32::MAX,
            inode: 0,
            show: UDIAG_SHOW_PEER,
            cookie: [0; 2],
        };
        let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        let sent = unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                (&request as *const Request).cast(),
                size_of::<Request>(),
                0,
                (&kernel as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut peers = HashMap::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if read < 0 {
                return Err(io::Error::last_os_error());
            }
            let bytes = &buffer[..read as usize];
            let mut at = 0;
            while at + NLMSG_HEADER <= bytes.len() {
                let (Some(length), Some(kind)) = (word(bytes, at), half(bytes, at + 4)) else {
                    break;
                };
                let length = length as usize;
                if length < NLMSG_HEADER || at + length > bytes.len() {
                    return Err(io::Error::other("truncated sock_diag message"));
                }
                match kind {
                    k if k == libc::NLMSG_DONE as u16 => return Ok(peers),
                    k if k == libc::NLMSG_ERROR as u16 => {
                        let errno = word(bytes, at + NLMSG_HEADER).unwrap_or(0) as i32;
                        return Err(io::Error::from_raw_os_error(-errno));
                    }
                    _ => {}
                }
                let message = &bytes[at + NLMSG_HEADER..at + length];
                if let Some(inode) = word(message, 4) {
                    let mut peer = 0;
                    let mut attribute = UNIX_DIAG_MSG;
                    while let (Some(size), Some(kind)) =
                        (half(message, attribute), half(message, attribute + 2))
                    {
                        let size = usize::from(size);
                        if size < 4 {
                            break;
                        }
                        if kind == UNIX_DIAG_PEER {
                            peer = word(message, attribute + 4).unwrap_or(0);
                        }
                        attribute += align(size);
                    }
                    peers.insert(u64::from(inode), u64::from(peer));
                }
                at += align(length);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_current_working_directory_from_the_kernel() {
        assert_eq!(cwd(std::process::id()), std::fs::canonicalize(".").ok());
        assert!(cwd(u32::MAX).is_none());
    }

    #[test]
    fn an_open_file_appears_and_disappears_when_its_descriptor_closes() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("thread.lock")).unwrap();
        let metadata = file.metadata().unwrap();
        let identity = (metadata.dev(), metadata.ino());
        assert!(
            open_files(std::process::id())
                .unwrap()
                .iter()
                .any(|file| (file.device, file.inode) == identity)
        );
        drop(file);
        assert!(
            !open_files(std::process::id())
                .unwrap()
                .iter()
                .any(|file| (file.device, file.inode) == identity)
        );
    }
}
