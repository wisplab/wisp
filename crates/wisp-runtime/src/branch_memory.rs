//! Immutable branch snapshots. macOS can snapshot the live VM mapping with
//! kernel COW; other platforms retain the anonymous file-backed implementation.
use anyhow::{anyhow, Result};
use std::io::Write;
use std::os::unix::io::AsRawFd;

pub(crate) struct BranchMemory {
    source: Source,
    pub len: usize,
}

enum Source {
    File(std::fs::File),
    #[cfg(target_os = "macos")]
    Mach(macos::CowRegion),
}

impl BranchMemory {
    /// Caller keeps the parent quiescent for this call. After capture returns,
    /// subsequent parent writes cannot modify the snapshot or any child.
    pub fn capture(data: &[u8]) -> Result<Self> {
        #[cfg(target_os = "macos")]
        if std::env::var("WISP_BRANCH_SNAPSHOT").as_deref() != Ok("file") {
            match macos::CowRegion::capture(data) {
                Ok(region) => {
                    return Ok(Self {
                        source: Source::Mach(region),
                        len: data.len(),
                    })
                }
                Err(error) => {
                    tracing::warn!(%error, "kernel COW snapshot unavailable; using file snapshot")
                }
            }
        }
        Self::capture_file(data)
    }

    fn capture_file(data: &[u8]) -> Result<Self> {
        let mut file = tempfile::tempfile()?;
        file.write_all(data)?;
        Ok(Self {
            source: Source::File(file),
            len: data.len(),
        })
    }

    pub fn backend(&self) -> &'static str {
        match &self.source {
            Source::File(_) => "file",
            #[cfg(target_os = "macos")]
            Source::Mach(_) => "mach-cow",
        }
    }

    /// Replace exactly the committed, page-aligned linear-memory mapping of a
    /// fresh child, leaving its surrounding Wasmtime reservation/guards intact.
    ///
    /// # Safety
    /// `base` must own a writable VM mapping of exactly `len` bytes, aligned to
    /// host pages. There must be no guest execution or other access to it until
    /// this operation completes. The mapping is still owned by the child Store.
    pub unsafe fn restore(&self, base: *mut u8, len: usize) -> Result<()> {
        let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        if len != self.len || page == 0 || (base as usize) % page != 0 || len % page != 0 {
            return Err(anyhow!(
                "branch snapshot restore requires matching page-aligned linear memory"
            ));
        }
        match &self.source {
            Source::File(file) => {
                let mapped = libc::mmap(
                    base.cast(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED,
                    file.as_raw_fd(),
                    0,
                );
                if mapped == libc::MAP_FAILED {
                    return Err(anyhow!(
                        "branch file snapshot remap failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            Source::Mach(region) => region.restore(base),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use mach2::{
        traps::mach_task_self,
        vm::{mach_vm_deallocate, mach_vm_remap},
        vm_inherit::VM_INHERIT_NONE,
        vm_statistics::{VM_FLAGS_ANYWHERE, VM_FLAGS_OVERWRITE},
    };
    // copy=TRUE is essential: neither remap is a shared writable alias.

    // Store only addresses, not Rust references to mutable guest memory. This
    // region owns its VM mapping and exposes only immutable COW remapping.
    pub(super) struct CowRegion {
        address: u64,
        len: usize,
    }

    impl CowRegion {
        pub fn capture(data: &[u8]) -> Result<Self> {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            if page == 0
                || data.is_empty()
                || (data.as_ptr() as usize) % page != 0
                || data.len() % page != 0
            {
                return Err(anyhow!("kernel COW requires a page-aligned guest region"));
            }
            let mut address = 0;
            unsafe {
                remap(
                    data.as_ptr() as u64,
                    &mut address,
                    data.len(),
                    VM_FLAGS_ANYWHERE,
                )?;
            }
            Ok(Self {
                address,
                len: data.len(),
            })
        }
        pub unsafe fn restore(&self, base: *mut u8) -> Result<()> {
            let mut address = base as u64;
            remap(self.address, &mut address, self.len, VM_FLAGS_OVERWRITE)?;
            debug_assert_eq!(address, base as u64);
            Ok(())
        }
    }

    unsafe fn remap(source: u64, address: &mut u64, len: usize, flags: i32) -> Result<()> {
        let task = mach_task_self();
        let mut current = 0;
        let mut maximum = 0;
        let status = mach_vm_remap(
            task,
            address,
            len as u64,
            0,
            flags,
            task,
            source,
            1,
            &mut current,
            &mut maximum,
            VM_INHERIT_NONE,
        );
        if status != 0 {
            return Err(anyhow!(
                "mach_vm_remap(copy=TRUE) failed: kernel status {status}"
            ));
        }
        Ok(())
    }

    impl Drop for CowRegion {
        fn drop(&mut self) {
            let status =
                unsafe { mach_vm_deallocate(mach_task_self(), self.address, self.len as u64) };
            if status != 0 {
                tracing::warn!(status, "failed to release branch COW snapshot");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mapping {
        base: *mut u8,
        len: usize,
    }
    impl Mapping {
        fn new() -> Self {
            let len = 16 * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED);
            Self {
                base: base.cast(),
                len,
            }
        }
        fn bytes(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.base, self.len) }
        }
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.base.cast(), self.len) }, 0);
        }
    }
    fn check(snapshot: BranchMemory, parent: &mut Mapping) {
        let mut a = Mapping::new();
        let mut b = Mapping::new();
        parent.bytes().fill(2);
        unsafe {
            snapshot.restore(a.base, a.len).unwrap();
            snapshot.restore(b.base, b.len).unwrap();
        }
        assert!(a.bytes().iter().all(|&x| x == 1));
        assert!(b.bytes().iter().all(|&x| x == 1));
        a.bytes().fill(3);
        assert!(parent.bytes().iter().all(|&x| x == 2));
        assert!(b.bytes().iter().all(|&x| x == 1));
        // Snapshot lifetime must not control a child's memory lifetime.
        drop(snapshot);
        assert!(a.bytes().iter().all(|&x| x == 3));
        assert!(b.bytes().iter().all(|&x| x == 1));
    }
    #[test]
    fn file_snapshot_isolates_parent_and_siblings() {
        let mut parent = Mapping::new();
        parent.bytes().fill(1);
        let snapshot = BranchMemory::capture_file(parent.bytes()).unwrap();
        check(snapshot, &mut parent);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_snapshot_isolates_parent_and_siblings() {
        let mut parent = Mapping::new();
        parent.bytes().fill(1);
        let region = macos::CowRegion::capture(parent.bytes()).unwrap();
        let snapshot = BranchMemory {
            source: Source::Mach(region),
            len: parent.len,
        };
        assert_eq!(snapshot.backend(), "mach-cow");
        check(snapshot, &mut parent);
    }
    #[test]
    fn restore_rejects_misaligned_or_wrong_size_region() {
        let mut parent = Mapping::new();
        parent.bytes().fill(1);
        let snapshot = BranchMemory::capture_file(parent.bytes()).unwrap();
        let child = Mapping::new();
        assert!(unsafe { snapshot.restore(child.base.wrapping_add(1), child.len) }.is_err());
        assert!(unsafe { snapshot.restore(child.base, child.len / 2) }.is_err());
    }
}
