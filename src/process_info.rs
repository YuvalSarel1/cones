//! Read the known process directly. A system-wide lsof scan added hundreds of milliseconds
//! to every refresh. These are the same kernel-reported paths, with no cached liveness.
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

pub struct OpenFile {
    pub device: u64,
    pub inode: u64,
    pub path: Option<PathBuf>,
}

pub fn open_files(pid: u32) -> io::Result<Vec<OpenFile>> {
    crate::observe::read(
        crate::observe::op::OPEN_FILES,
        || open_files_of(pid),
        Result::is_ok,
    )
}

fn open_files_of(pid: u32) -> io::Result<Vec<OpenFile>> {
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
        || unix_sockets_of(pid),
        Result::is_ok,
    )
}

fn unix_sockets_of(pid: u32) -> io::Result<Vec<UnixSocket>> {
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
