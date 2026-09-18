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

pub fn cwd(pid: u32) -> Option<PathBuf> {
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
}

pub fn open_files(pid: u32) -> io::Result<Vec<OpenFile>> {
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
                });
            }
        }
        return Ok(paths);
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
