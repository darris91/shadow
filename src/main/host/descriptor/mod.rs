use atomic_refcell::AtomicRefCell;
use std::convert::TryInto;
use std::sync::Arc;

use crate::cshadow as c;
use crate::host::syscall_types::SyscallResult;
use crate::utility::event_queue::{EventQueue, EventSource, Handle};
use crate::utility::SyncSendPointer;

pub mod descriptor_table;
pub mod pipe;

/// A trait we can use as a compile-time check to make sure that an object is Send.
trait IsSend: Send {}

/// A trait we can use as a compile-time check to make sure that an object is Sync.
trait IsSync: Sync {}

bitflags::bitflags! {
    /// These are flags that can potentially be changed from the plugin (analagous to the Linux
    /// `filp->f_flags` status flags). Not all `O_` flags are valid here. For example file access
    /// mode flags (ex: `O_RDWR`) are stored elsewhere, and file creation flags (ex: `O_CREAT`)
    /// are not stored anywhere. Many of these can be represented in different ways, for example:
    /// `O_NONBLOCK`, `SOCK_NONBLOCK`, `EFD_NONBLOCK`, `GRND_NONBLOCK`, etc, and not all have the
    /// same value.
    pub struct FileFlags: libc::c_int {
        const NONBLOCK = libc::O_NONBLOCK;
        const APPEND = libc::O_APPEND;
        const ASYNC = libc::O_ASYNC;
        const DIRECT = libc::O_DIRECT;
        const NOATIME = libc::O_NOATIME;
    }
}

bitflags::bitflags! {
    /// These are flags that should generally not change (analagous to the Linux `filp->f_mode`).
    /// Since the plugin will never see these values and they're not exposed by the kernel, we
    /// don't match the kernel `FMODE_` values here.
    ///
    /// Examples: https://github.com/torvalds/linux/blob/master/include/linux/fs.h#L111
    pub struct FileMode: u32 {
        const READ = 0b00000001;
        const WRITE = 0b00000010;
    }
}

bitflags::bitflags! {
    pub struct FileStatus: libc::c_int {
        /// Has been initialized and it is now OK to unblock any plugin waiting
        /// on a particular status.
        const ACTIVE = c::_Status_STATUS_DESCRIPTOR_ACTIVE;
        /// Can be read, i.e. there is data waiting for user.
        const READABLE = c::_Status_STATUS_DESCRIPTOR_READABLE;
        /// Can be written, i.e. there is available buffer space.
        const WRITABLE = c::_Status_STATUS_DESCRIPTOR_WRITABLE;
        /// User already called close.
        const CLOSED = c::_Status_STATUS_DESCRIPTOR_CLOSED;
        /// A wakeup operation occurred on a futex.
        const FUTEX_WAKEUP = c::_Status_STATUS_FUTEX_WAKEUP;
    }
}

impl From<c::Status> for FileStatus {
    fn from(status: c::Status) -> Self {
        // if any unexpected bits were present, then it's an error
        Self::from_bits(status).unwrap()
    }
}

impl From<FileStatus> for c::Status {
    fn from(status: FileStatus) -> Self {
        status.bits()
    }
}

#[derive(Clone, Debug)]
pub enum NewStatusListenerFilter {
    Never,
    OffToOn,
    OnToOff,
    Always,
}

/// A wrapper for a `*mut c::StatusListener` that increments its ref count when created,
/// and decrements when dropped.
struct LegacyStatusListener(SyncSendPointer<c::StatusListener>);

impl LegacyStatusListener {
    fn new(ptr: *mut c::StatusListener) -> Self {
        assert!(!ptr.is_null());
        unsafe { c::statuslistener_ref(ptr) };
        Self(SyncSendPointer(ptr))
    }

    fn ptr(&self) -> *mut c::StatusListener {
        self.0.ptr()
    }
}

impl Drop for LegacyStatusListener {
    fn drop(&mut self) {
        unsafe { c::statuslistener_unref(self.0.ptr()) };
    }
}

/// Stores event listener handles so that `c::StatusListener` objects can subscribe to events.
struct LegacyListenerHelper {
    handle_map: std::collections::HashMap<usize, Handle<(FileStatus, FileStatus)>>,
}

impl LegacyListenerHelper {
    fn new() -> Self {
        Self {
            handle_map: std::collections::HashMap::new(),
        }
    }

    fn add_listener(
        &mut self,
        ptr: *mut c::StatusListener,
        event_source: &mut EventSource<(FileStatus, FileStatus)>,
    ) {
        assert!(!ptr.is_null());

        // if it's already listening, don't add a second time
        if self.handle_map.contains_key(&(ptr as usize)) {
            return;
        }

        // this will ref the pointer and unref it when the closure is dropped
        let ptr_wrapper = LegacyStatusListener::new(ptr);

        let handle = event_source.add_listener(move |(status, changed), _event_queue| unsafe {
            c::statuslistener_onStatusChanged(ptr_wrapper.ptr(), status.into(), changed.into())
        });

        // use a usize as the key so we don't accidentally deref the pointer
        self.handle_map.insert(ptr as usize, handle);
    }

    fn remove_listener(&mut self, ptr: *mut c::StatusListener) {
        assert!(!ptr.is_null());
        self.handle_map.remove(&(ptr as usize));
    }
}

/// A specified event source that passes a status and the changed bits to the function, but only if
/// the monitored bits have changed and if the change the filter is satisfied.
struct StatusEventSource {
    inner: EventSource<(FileStatus, FileStatus)>,
    legacy_helper: LegacyListenerHelper,
}

impl StatusEventSource {
    pub fn new() -> Self {
        Self {
            inner: EventSource::new(),
            legacy_helper: LegacyListenerHelper::new(),
        }
    }

    pub fn add_listener(
        &mut self,
        monitoring: FileStatus,
        filter: NewStatusListenerFilter,
        notify_fn: impl Fn(FileStatus, FileStatus, &mut EventQueue) + Send + Sync + 'static,
    ) -> Handle<(FileStatus, FileStatus)> {
        self.inner
            .add_listener(move |(status, changed), event_queue| {
                // true if any of the bits we're monitoring have changed
                let flipped = monitoring.intersects(changed);

                // true if any of the bits we're monitoring are set
                let on = monitoring.intersects(status);

                let notify = match filter {
                    // at least one monitored bit is on, and at least one has changed
                    NewStatusListenerFilter::OffToOn => flipped && on,
                    // all monitored bits are off, and at least one has changed
                    NewStatusListenerFilter::OnToOff => flipped && !on,
                    // at least one monitored bit has changed
                    NewStatusListenerFilter::Always => flipped,
                    NewStatusListenerFilter::Never => false,
                };

                if !notify {
                    return;
                }

                (notify_fn)(status, changed, event_queue)
            })
    }

    pub fn add_legacy_listener(&mut self, ptr: *mut c::StatusListener) {
        self.legacy_helper.add_listener(ptr, &mut self.inner);
    }

    pub fn remove_legacy_listener(&mut self, ptr: *mut c::StatusListener) {
        self.legacy_helper.remove_listener(ptr);
    }

    pub fn notify_listeners(
        &mut self,
        status: FileStatus,
        changed: FileStatus,
        event_queue: &mut EventQueue,
    ) {
        self.inner.notify_listeners((status, changed), event_queue)
    }
}

/// Represents a POSIX description, or a Linux "struct file".
#[derive(Clone)]
pub enum PosixFile {
    Pipe(Arc<AtomicRefCell<pipe::PipeFile>>),
}

// will not compile if `PosixFile` is not Send + Sync
impl IsSend for PosixFile {}
impl IsSync for PosixFile {}

impl PosixFile {
    pub fn borrow(&self) -> PosixFileRef {
        match self {
            Self::Pipe(ref f) => PosixFileRef::Pipe(f.borrow()),
        }
    }

    pub fn try_borrow(&self) -> Result<PosixFileRef, atomic_refcell::BorrowError> {
        Ok(match self {
            Self::Pipe(ref f) => PosixFileRef::Pipe(f.try_borrow()?),
        })
    }

    pub fn borrow_mut(&self) -> PosixFileRefMut {
        match self {
            Self::Pipe(ref f) => PosixFileRefMut::Pipe(f.borrow_mut()),
        }
    }

    pub fn try_borrow_mut(&self) -> Result<PosixFileRefMut, atomic_refcell::BorrowMutError> {
        Ok(match self {
            Self::Pipe(ref f) => PosixFileRefMut::Pipe(f.try_borrow_mut()?),
        })
    }

    pub fn canonical_handle(&self) -> usize {
        match self {
            Self::Pipe(f) => Arc::as_ptr(f) as usize,
        }
    }
}

impl std::fmt::Debug for PosixFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipe(_) => write!(f, "Pipe")?,
        }

        if let Ok(file) = self.try_borrow() {
            write!(
                f,
                "(status: {:?}, flags: {:?})",
                file.status(),
                file.get_flags()
            )
        } else {
            write!(f, "(already borrowed)")
        }
    }
}

pub enum PosixFileRef<'a> {
    Pipe(atomic_refcell::AtomicRef<'a, pipe::PipeFile>),
}

pub enum PosixFileRefMut<'a> {
    Pipe(atomic_refcell::AtomicRefMut<'a, pipe::PipeFile>),
}

impl PosixFileRef<'_> {
    enum_passthrough!(self, (), Pipe;
        pub fn status(&self) -> FileStatus
    );
    enum_passthrough!(self, (), Pipe;
        pub fn get_flags(&self) -> FileFlags
    );
}

impl PosixFileRefMut<'_> {
    enum_passthrough!(self, (), Pipe;
        pub fn status(&self) -> FileStatus
    );
    enum_passthrough!(self, (), Pipe;
        pub fn get_flags(&self) -> FileFlags
    );
    enum_passthrough!(self, (event_queue), Pipe;
        pub fn close(&mut self, event_queue: &mut EventQueue) -> SyscallResult
    );
    enum_passthrough!(self, (flags), Pipe;
        pub fn set_flags(&mut self, flags: FileFlags)
    );
    enum_passthrough!(self, (ptr), Pipe;
        pub fn add_legacy_listener(&mut self, ptr: *mut c::StatusListener)
    );
    enum_passthrough!(self, (ptr), Pipe;
        pub fn remove_legacy_listener(&mut self, ptr: *mut c::StatusListener)
    );

    enum_passthrough_generic!(self, (bytes, offset, event_queue), Pipe;
        pub fn read<W>(&mut self, bytes: W, offset: libc::off_t, event_queue: &mut EventQueue) -> SyscallResult
        where W: std::io::Write + std::io::Seek
    );

    enum_passthrough_generic!(self, (source, offset, event_queue), Pipe;
        pub fn write<R>(&mut self, source: R, offset: libc::off_t, event_queue: &mut EventQueue) -> SyscallResult
        where R: std::io::Read + std::io::Seek
    );
}

impl std::fmt::Debug for PosixFileRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipe(_) => write!(f, "Pipe")?,
        }

        write!(
            f,
            "(status: {:?}, flags: {:?})",
            self.status(),
            self.get_flags()
        )
    }
}

impl std::fmt::Debug for PosixFileRefMut<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipe(_) => write!(f, "Pipe")?,
        }

        write!(
            f,
            "(status: {:?}, flags: {:?})",
            self.status(),
            self.get_flags()
        )
    }
}

bitflags::bitflags! {
    // Linux only supports a single descriptor flag:
    // https://www.gnu.org/software/libc/manual/html_node/Descriptor-Flags.html
    pub struct DescriptorFlags: u32 {
        const CLOEXEC = libc::FD_CLOEXEC as u32;
    }
}

#[derive(Clone, Debug)]
pub struct Descriptor {
    /// The PosixFile that this descriptor points to.
    file: PosixFile,
    /// Descriptor flags.
    flags: DescriptorFlags,
    /// A count of how many open descriptors there are with reference to this file. Since a
    /// reference to the file can be held by other objects like an epoll file, it should
    /// be true that `Arc::strong_count(&self.count)` <= `Arc::strong_count(&self.file)`.
    open_count: Arc<()>,
}

impl Descriptor {
    pub fn new(file: PosixFile) -> Self {
        Self {
            file,
            flags: DescriptorFlags::empty(),
            open_count: Arc::new(()),
        }
    }

    pub fn get_file(&self) -> &PosixFile {
        &self.file
    }

    pub fn get_flags(&self) -> DescriptorFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: DescriptorFlags) {
        self.flags = flags;
    }

    /// Close the descriptor, and if this is the last descriptor pointing to its file, close
    /// the file as well.
    pub fn close(self, event_queue: &mut EventQueue) -> Option<SyscallResult> {
        // this isn't subject to race conditions since we should never access descriptors
        // from multiple threads at the same time
        if Arc::<()>::strong_count(&self.open_count) == 1 {
            Some(self.file.borrow_mut().close(event_queue))
        } else {
            None
        }
    }
}

/// Represents an owned reference to a legacy descriptor. Will decrement the descriptor's ref
/// count when dropped.
#[derive(Debug)]
pub struct OwnedLegacyDescriptor(SyncSendPointer<c::LegacyDescriptor>);

impl OwnedLegacyDescriptor {
    /// Does not increment the legacy descriptor's ref count, but will decrement the ref count
    /// when dropped.
    pub fn new(ptr: *mut c::LegacyDescriptor) -> Self {
        Self(SyncSendPointer(ptr))
    }

    pub fn ptr(&self) -> *mut c::LegacyDescriptor {
        self.0.ptr()
    }
}

impl Drop for OwnedLegacyDescriptor {
    fn drop(&mut self) {
        // unref the legacy descriptor object
        unsafe { c::descriptor_unref(self.0.ptr() as *mut core::ffi::c_void) };
    }
}

// don't implement copy or clone without considering the legacy descriptor's ref count
#[derive(Debug)]
pub enum CompatDescriptor {
    New(Descriptor),
    Legacy(OwnedLegacyDescriptor),
}

// will not compile if `CompatDescriptor` is not Send + Sync
impl IsSend for CompatDescriptor {}
impl IsSync for CompatDescriptor {}

impl CompatDescriptor {
    pub fn into_raw(descriptor: Box<Self>) -> *mut Self {
        Box::into_raw(descriptor)
    }

    pub fn from_raw(descriptor: *mut Self) -> Option<Box<Self>> {
        if descriptor.is_null() {
            return None;
        }

        unsafe { Some(Box::from_raw(descriptor)) }
    }

    /// Update the handle.
    /// This is a no-op for non-legacy descriptors.
    pub fn set_handle(&mut self, handle: u32) {
        if let CompatDescriptor::Legacy(d) = self {
            unsafe { c::descriptor_setHandle(d.ptr(), handle.try_into().unwrap()) }
        }
        // new descriptor types don't store their file handle, so do nothing
    }
}

mod export {
    use super::*;

    /// The new compat descriptor takes ownership of the reference to the legacy descriptor and
    /// does not increment its ref count, but will decrement the ref count when this compat
    /// descriptor is freed/dropped.
    #[no_mangle]
    pub extern "C" fn compatdescriptor_fromLegacy(
        legacy_descriptor: *mut c::LegacyDescriptor,
    ) -> *mut CompatDescriptor {
        assert!(!legacy_descriptor.is_null());

        let descriptor = CompatDescriptor::Legacy(OwnedLegacyDescriptor::new(legacy_descriptor));
        CompatDescriptor::into_raw(Box::new(descriptor))
    }

    /// If the compat descriptor is a legacy descriptor, returns a pointer to the legacy
    /// descriptor object. Otherwise returns NULL. The legacy descriptor's ref count is not
    /// modified, so the pointer must not outlive the lifetime of the compat descriptor.
    #[no_mangle]
    pub extern "C" fn compatdescriptor_asLegacy(
        descriptor: *const CompatDescriptor,
    ) -> *mut c::LegacyDescriptor {
        assert!(!descriptor.is_null());

        let descriptor = unsafe { &*descriptor };

        if let CompatDescriptor::Legacy(d) = descriptor {
            d.ptr()
        } else {
            std::ptr::null_mut()
        }
    }

    /// When the compat descriptor is freed/dropped, it will decrement the legacy descriptor's
    /// ref count.
    #[no_mangle]
    pub extern "C" fn compatdescriptor_free(descriptor: *mut CompatDescriptor) {
        CompatDescriptor::from_raw(descriptor);
    }

    /// If the compat descriptor is a new descriptor, returns a pointer to the reference-counted
    /// posix file object. Otherwise returns NULL. The posix file object's ref count is not
    /// modified, so the pointer must not outlive the lifetime of the compat descriptor.
    #[no_mangle]
    pub extern "C" fn compatdescriptor_borrowPosixFile(
        descriptor: *const CompatDescriptor,
    ) -> *const PosixFile {
        assert!(!descriptor.is_null());

        let descriptor = unsafe { &*descriptor };

        match descriptor {
            CompatDescriptor::Legacy(_) => std::ptr::null_mut(),
            CompatDescriptor::New(d) => d.get_file(),
        }
    }

    /// If the compat descriptor is a new descriptor, returns a pointer to the reference-counted
    /// posix file object. Otherwise returns NULL. The posix file object's ref count is
    /// incremented, so the pointer must always later be passed to `posixfile_drop()`, otherwise
    /// the memory will leak.
    #[no_mangle]
    pub extern "C" fn compatdescriptor_newRefPosixFile(
        descriptor: *const CompatDescriptor,
    ) -> *const PosixFile {
        assert!(!descriptor.is_null());

        let descriptor = unsafe { &*descriptor };

        match descriptor {
            CompatDescriptor::Legacy(_) => std::ptr::null_mut(),
            CompatDescriptor::New(d) => Box::into_raw(Box::new(d.get_file().clone())),
        }
    }

    /// Decrement the ref count of the posix file object. The pointer must not be used after
    /// calling this function.
    #[no_mangle]
    pub extern "C" fn posixfile_drop(file: *const PosixFile) {
        assert!(!file.is_null());

        unsafe { Box::from_raw(file as *mut PosixFile) };
    }

    /// Get the status of the posix file object.
    #[allow(unused_variables)]
    #[no_mangle]
    pub extern "C" fn posixfile_getStatus(file: *const PosixFile) -> c::Status {
        assert!(!file.is_null());

        let file = unsafe { &*file };

        file.borrow().status().into()
    }

    /// Add a status listener to the posix file object. This will increment the status
    /// listener's ref count, and will decrement the ref count when this status listener is
    /// removed or when the posix file is freed/dropped.
    #[no_mangle]
    pub extern "C" fn posixfile_addListener(
        file: *const PosixFile,
        listener: *mut c::StatusListener,
    ) {
        assert!(!file.is_null());
        assert!(!listener.is_null());

        let file = unsafe { &*file };

        file.borrow_mut().add_legacy_listener(listener);
    }

    /// Remove a listener from the posix file object.
    #[no_mangle]
    pub extern "C" fn posixfile_removeListener(
        file: *const PosixFile,
        listener: *mut c::StatusListener,
    ) {
        assert!(!file.is_null());
        assert!(!listener.is_null());

        let file = unsafe { &*file };

        file.borrow_mut().remove_legacy_listener(listener);
    }

    /// Get the canonical handle for a posix file object. Two posix file objects refer to the same
    /// underlying data if their handles are equal.
    #[no_mangle]
    pub extern "C" fn posixfile_getCanonicalHandle(file: *const PosixFile) -> libc::uintptr_t {
        assert!(!file.is_null());

        let file = unsafe { &*file };

        file.canonical_handle()
    }
}

impl std::fmt::Debug for c::SysCallReturn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SysCallReturn")
            .field("state", &self.state)
            .field("retval", &self.retval)
            .field("cond", &self.cond)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::syscall::Trigger;
    use crate::host::syscall_condition::SysCallCondition;
    use crate::host::syscall_types::SyscallError;

    #[test]
    fn test_syscallresult_roundtrip() {
        for val in vec![
            Ok(1.into()),
            Err(SyscallError::Errno(nix::errno::Errno::EPERM)),
            Err(SyscallError::Cond(SysCallCondition::new(Trigger::from(
                c::Trigger {
                    type_: 1,
                    object: c::TriggerObject {
                        as_pointer: std::ptr::null_mut(),
                    },
                    status: 2,
                },
            )))),
        ]
        .drain(..)
        {
            // We can't easily compare the value to the roundtripped result, since
            // roundtripping consumes the original value, and SysCallReturn doesn't implement Clone.
            // Compare their debug strings instead.
            let orig_debug = format!("{:?}", &val);
            let roundtripped = SyscallResult::from(c::SysCallReturn::from(val));
            let roundtripped_debug = format!("{:?}", roundtripped);
            assert_eq!(orig_debug, roundtripped_debug);
        }
    }

    #[test]
    fn test_syscallreturn_roundtrip() {
        let condition = SysCallCondition::new(Trigger::from(c::Trigger {
            type_: 1,
            object: c::TriggerObject {
                as_pointer: std::ptr::null_mut(),
            },
            status: 2,
        }));
        for val in vec![
            c::SysCallReturn {
                state: c::SysCallReturnState_SYSCALL_DONE,
                retval: 1.into(),
                cond: std::ptr::null_mut(),
            },
            c::SysCallReturn {
                state: c::SysCallReturnState_SYSCALL_BLOCK,
                retval: 0.into(),
                cond: condition.into_inner(),
            },
            c::SysCallReturn {
                state: c::SysCallReturnState_SYSCALL_NATIVE,
                retval: 0.into(),
                cond: std::ptr::null_mut(),
            },
        ]
        .drain(..)
        {
            // We can't easily compare the value to the roundtripped result,
            // since roundtripping consumes the original value, and
            // SysCallReturn doesn't implement Clone. Compare their debug
            // strings instead.
            let orig_debug = format!("{:?}", &val);
            let roundtripped = c::SysCallReturn::from(SyscallResult::from(val));
            let roundtripped_debug = format!("{:?}", roundtripped);
            assert_eq!(orig_debug, roundtripped_debug);
        }
    }
}
