use crossbeam_utils::atomic::AtomicCell;
use gethostname::gethostname;
#[cfg(all(unix, not(target_os = "redox")))]
use nix::unistd::sethostname;
use num_traits::ToPrimitive;
use socket2::{Domain, Protocol, Socket, Type as SocketType};
use std::convert::TryFrom;
use std::io;
use std::mem::MaybeUninit;
use std::net::{self, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::builtins::int;
use crate::builtins::pystr::PyStrRef;
use crate::builtins::pytype::PyTypeRef;
use crate::builtins::tuple::PyTupleRef;
use crate::byteslike::{PyBytesLike, PyRwBytesLike};
use crate::common::lock::{PyRwLock, PyRwLockReadGuard, PyRwLockWriteGuard};
use crate::exceptions::{IntoPyException, PyBaseExceptionRef};
use crate::function::{FuncArgs, OptionalArg, OptionalOption};
use crate::VirtualMachine;
use crate::{
    BorrowValue, Either, IntoPyObject, PyClassImpl, PyObjectRef, PyRef, PyResult, PyValue,
    StaticType, TryFromObject, TypeProtocol,
};

#[cfg(unix)]
type RawSocket = std::os::unix::io::RawFd;
#[cfg(windows)]
type RawSocket = std::os::windows::raw::SOCKET;

#[cfg(unix)]
macro_rules! errcode {
    ($e:ident) => {
        c::$e
    };
}
#[cfg(windows)]
macro_rules! errcode {
    ($e:ident) => {
        paste::paste!(c::[<WSA $e>])
    };
}

#[cfg(unix)]
use libc as c;
#[cfg(windows)]
mod c {
    pub use winapi::shared::ws2def::*;
    pub use winapi::um::winsock2::{
        SD_BOTH as SHUT_RDWR, SD_RECEIVE as SHUT_RD, SD_SEND as SHUT_WR, SOCK_DGRAM, SOCK_RAW,
        SOCK_RDM, SOCK_STREAM, SOL_SOCKET, SO_BROADCAST, SO_ERROR, SO_LINGER, SO_OOBINLINE,
        SO_REUSEADDR, SO_TYPE, *,
    };
}

#[pyclass(module = "socket", name = "socket")]
#[derive(Debug)]
pub struct PySocket {
    kind: AtomicCell<i32>,
    family: AtomicCell<i32>,
    proto: AtomicCell<i32>,
    pub(crate) timeout: AtomicCell<f64>,
    sock: PyRwLock<Socket>,
}

impl Default for PySocket {
    fn default() -> Self {
        PySocket {
            kind: AtomicCell::default(),
            family: AtomicCell::default(),
            proto: AtomicCell::default(),
            timeout: AtomicCell::new(-1.0),
            sock: PyRwLock::new(invalid_sock()),
        }
    }
}

impl PyValue for PySocket {
    fn class(_vm: &VirtualMachine) -> &PyTypeRef {
        Self::static_type()
    }
}

pub type PySocketRef = PyRef<PySocket>;

#[pyimpl(flags(BASETYPE))]
impl PySocket {
    pub fn sock(&self) -> PyRwLockReadGuard<'_, Socket> {
        self.sock.read()
    }

    fn sock_mut(&self) -> PyRwLockWriteGuard<'_, Socket> {
        self.sock.write()
    }

    #[pyslot]
    fn tp_new(cls: PyTypeRef, _args: FuncArgs, vm: &VirtualMachine) -> PyResult<PyRef<Self>> {
        Self::default().into_ref_with_type(vm, cls)
    }

    #[pymethod(name = "__init__")]
    fn init(
        &self,
        family: OptionalArg<i32>,
        socket_kind: OptionalArg<i32>,
        proto: OptionalArg<i32>,
        fileno: OptionalOption<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        let mut family = family.unwrap_or(-1);
        let mut socket_kind = socket_kind.unwrap_or(-1);
        let mut proto = proto.unwrap_or(-1);
        // should really just be to_index() but test_socket tests the error messages explicitly
        let fileno = match fileno.flatten() {
            Some(o) if o.isinstance(&vm.ctx.types.float_type) => {
                return Err(vm.new_type_error("integer argument expected, got float".to_owned()))
            }
            Some(o) => {
                let int = vm.to_index_opt(o).unwrap_or_else(|| {
                    Err(vm.new_type_error("an integer is required".to_owned()))
                })?;
                Some(int::try_to_primitive::<RawSocket>(int.borrow_value(), vm)?)
            }
            None => None,
        };
        let sock;
        if let Some(fileno) = fileno {
            sock = sock_from_raw(fileno, vm)?;
            match sock.local_addr() {
                Ok(addr) if family == -1 => family = addr.family() as i32,
                Err(e)
                    if family == -1
                        || matches!(
                            e.raw_os_error(),
                            Some(errcode!(ENOTSOCK)) | Some(errcode!(EBADF))
                        ) =>
                {
                    std::mem::forget(sock);
                    return Err(e.into_pyexception(vm));
                }
                _ => {}
            }
            if socket_kind == -1 {
                // TODO: when socket2 cuts a new release, type will be available on all os
                // socket_kind = sock.r#type().map_err(|e| e.into_pyexception(vm))?.into();
                let res = unsafe {
                    c::getsockopt(
                        sock_fileno(&sock) as _,
                        c::SOL_SOCKET,
                        c::SO_TYPE,
                        &mut socket_kind as *mut libc::c_int as *mut _,
                        &mut (std::mem::size_of::<i32>() as _),
                    )
                };
                if res < 0 {
                    return Err(super::os::errno_err(vm));
                }
            }
            cfg_if::cfg_if! {
                if #[cfg(any(
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "fuchsia",
                    target_os = "linux",
                ))] {
                    if proto == -1 {
                        proto = sock.protocol().map_err(|e| e.into_pyexception(vm))?.map_or(0, Into::into);
                    }
                } else {
                    proto = 0;
                }
            }
        } else {
            if family == -1 {
                family = c::AF_INET as i32
            }
            if socket_kind == -1 {
                socket_kind = c::SOCK_STREAM
            }
            if proto == -1 {
                proto = 0
            }
            sock = Socket::new(
                Domain::from(family),
                SocketType::from(socket_kind),
                Some(Protocol::from(proto)),
            )
            .map_err(|err| err.into_pyexception(vm))?;
        };
        self.init_inner(family, socket_kind, proto, sock, vm)
    }

    fn init_inner(
        &self,
        family: i32,
        socket_kind: i32,
        proto: i32,
        sock: Socket,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        self.family.store(family);
        self.kind.store(socket_kind);
        self.proto.store(proto);
        let mut s = self.sock.write();
        *s = sock;
        let timeout = DEFAULT_TIMEOUT.load();
        self.timeout.store(timeout);
        if timeout >= 0.0 {
            s.set_nonblocking(true)
                .map_err(|e| e.into_pyexception(vm))?;
        }
        Ok(())
    }

    #[inline]
    fn sock_op<F, R>(&self, vm: &VirtualMachine, select: SelectKind, f: F) -> PyResult<R>
    where
        F: FnMut() -> io::Result<R>,
    {
        self.sock_op_err(vm, select, f)
            .map_err(|e| e.into_pyexception(vm))
    }

    /// returns Err(blocking)
    pub fn get_timeout(&self) -> Result<Duration, bool> {
        let timeout = self.timeout.load();
        if timeout > 0.0 {
            Ok(Duration::from_secs_f64(timeout))
        } else {
            Err(timeout != 0.0)
        }
    }

    fn sock_op_err<F, R>(
        &self,
        vm: &VirtualMachine,
        select: SelectKind,
        f: F,
    ) -> Result<R, IoOrPyException>
    where
        F: FnMut() -> io::Result<R>,
    {
        self.sock_op_timeout_err(vm, select, self.get_timeout().ok(), f)
    }

    fn sock_op_timeout_err<F, R>(
        &self,
        vm: &VirtualMachine,
        select: SelectKind,
        timeout: Option<Duration>,
        mut f: F,
    ) -> Result<R, IoOrPyException>
    where
        F: FnMut() -> io::Result<R>,
    {
        let deadline = timeout.map(Deadline::new);

        loop {
            if deadline.is_some() || matches!(select, SelectKind::Connect) {
                let interval = deadline.as_ref().map(|d| d.time_until()).transpose()?;
                let res = sock_select(&self.sock(), select, interval);
                match res {
                    Ok(true) => return Err(IoOrPyException::Timeout),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        vm.check_signals()?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                    Ok(false) => {} // no timeout, continue as normal
                }
            }

            let err = loop {
                // loop on interrupt
                match f() {
                    Ok(x) => return Ok(x),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => vm.check_signals()?,
                    Err(e) => break e,
                }
            };
            if timeout.is_some() && err.kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            return Err(err.into());
        }
    }

    fn extract_address(
        &self,
        addr: PyObjectRef,
        caller: &str,
        vm: &VirtualMachine,
    ) -> PyResult<socket2::SockAddr> {
        let family = self.family.load();
        match family {
            #[cfg(unix)]
            c::AF_UNIX => {
                use std::os::unix::ffi::OsStrExt;
                let buf = crate::byteslike::BufOrStr::try_from_object(vm, addr)?;
                let path = buf.borrow_value();
                let path = std::ffi::OsStr::from_bytes(&path);
                socket2::SockAddr::unix(path)
                    .map_err(|_| vm.new_os_error("AF_UNIX path too long".to_owned()))
            }
            c::AF_INET => {
                let tuple: PyTupleRef = addr.downcast().map_err(|obj| {
                    vm.new_type_error(format!(
                        "{}(): AF_INET address must be tuple, not {}",
                        caller,
                        obj.class().name
                    ))
                })?;
                let tuple = tuple.borrow_value();
                if tuple.len() != 2 {
                    return Err(
                        vm.new_type_error("AF_INET address must be a pair (host, post)".to_owned())
                    );
                }
                let addr = Address::from_tuple(tuple, vm)?;
                let mut addr4 = get_addr(vm, addr.host.borrow_value(), c::AF_INET)?;
                match &mut addr4 {
                    SocketAddr::V4(addr4) => {
                        addr4.set_port(addr.port);
                    }
                    SocketAddr::V6(_) => unreachable!(),
                }
                Ok(addr4.into())
            }
            c::AF_INET6 => {
                let tuple: PyTupleRef = addr.downcast().map_err(|obj| {
                    vm.new_type_error(format!(
                        "{}(): AF_INET6 address must be tuple, not {}",
                        caller,
                        obj.class().name
                    ))
                })?;
                let tuple = tuple.borrow_value();
                match tuple.len() {
                    2 | 3 | 4 => {}
                    _ => {
                        return Err(vm.new_type_error(
                            "AF_INET6 address must be a tuple (host, port[, flowinfo[, scopeid]])"
                                .to_owned(),
                        ))
                    }
                }
                let (addr, flowinfo, scopeid) = Address::from_tuple_ipv6(tuple, vm)?;
                let mut addr6 = get_addr(vm, addr.host.borrow_value(), c::AF_INET6)?;
                match &mut addr6 {
                    SocketAddr::V6(addr6) => {
                        addr6.set_port(addr.port);
                        addr6.set_flowinfo(flowinfo);
                        addr6.set_scope_id(scopeid);
                    }
                    SocketAddr::V4(_) => unreachable!(),
                }
                Ok(addr6.into())
            }
            _ => Err(vm.new_os_error(format!("{}(): bad family", caller))),
        }
    }

    fn connect_inner(
        &self,
        address: PyObjectRef,
        caller: &str,
        vm: &VirtualMachine,
    ) -> Result<(), IoOrPyException> {
        let sock_addr = self.extract_address(address, caller, vm)?;

        let err = match self.sock().connect(&sock_addr) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };

        let wait_connect = if err.kind() == io::ErrorKind::Interrupted {
            vm.check_signals()?;
            self.timeout.load() != 0.0
        } else {
            #[cfg(unix)]
            use c::EINPROGRESS;
            #[cfg(windows)]
            use c::WSAEWOULDBLOCK as EINPROGRESS;

            self.timeout.load() > 0.0 && err.raw_os_error() == Some(EINPROGRESS)
        };

        if wait_connect {
            // basically, connect() is async, and it registers an "error" on the socket when it's
            // done connecting. SelectKind::Connect fills the errorfds fd_set, so if we wake up
            // from poll and the error is EISCONN then we know that the connect is done
            self.sock_op_err(vm, SelectKind::Connect, || {
                let sock = self.sock();
                let err = sock.take_error()?;
                match err {
                    Some(e) if e.raw_os_error() == Some(libc::EISCONN) => Ok(()),
                    Some(e) => Err(e),
                    // TODO: is this accurate?
                    None => Ok(()),
                }
            })
        } else {
            Err(err.into())
        }
    }

    #[pymethod]
    fn connect(&self, address: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        self.connect_inner(address, "connect", vm)
            .map_err(|e| e.into_pyexception(vm))
    }

    #[pymethod]
    fn connect_ex(&self, address: PyObjectRef, vm: &VirtualMachine) -> PyResult<i32> {
        match self.connect_inner(address, "connect_ex", vm) {
            Ok(()) => Ok(0),
            Err(err) => err.errno(),
        }
    }

    #[pymethod]
    fn bind(&self, address: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        let sock_addr = self.extract_address(address, "bind", vm)?;
        self.sock()
            .bind(&sock_addr)
            .map_err(|err| err.into_pyexception(vm))
    }

    #[pymethod]
    fn listen(&self, backlog: OptionalArg<i32>, vm: &VirtualMachine) -> PyResult<()> {
        let backlog = backlog.unwrap_or(128);
        let backlog = if backlog < 0 { 0 } else { backlog };
        self.sock()
            .listen(backlog)
            .map_err(|err| err.into_pyexception(vm))
    }

    #[pymethod]
    fn _accept(&self, vm: &VirtualMachine) -> PyResult<(RawSocket, PyObjectRef)> {
        let (sock, addr) = self.sock_op(vm, SelectKind::Read, || self.sock().accept())?;
        let fd = into_sock_fileno(sock);
        Ok((fd, get_addr_tuple(&addr, vm)))
    }

    #[pymethod]
    fn recv(
        &self,
        bufsize: usize,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<Vec<u8>> {
        let flags = flags.unwrap_or(0);
        let mut buffer = Vec::with_capacity(bufsize);
        let sock = self.sock();
        let n = self.sock_op(vm, SelectKind::Read, || {
            sock.recv_with_flags(spare_capacity_mut(&mut buffer), flags)
        })?;
        unsafe { buffer.set_len(n) };
        Ok(buffer)
    }

    #[pymethod]
    fn recv_into(
        &self,
        buf: PyRwBytesLike,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<usize> {
        let flags = flags.unwrap_or(0);
        let sock = self.sock();
        let mut buf = buf.borrow_value();
        let buf = &mut *buf;
        self.sock_op(vm, SelectKind::Read, || {
            sock.recv_with_flags(slice_as_uninit(buf), flags)
        })
    }

    #[pymethod]
    fn recvfrom(
        &self,
        bufsize: isize,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<(Vec<u8>, PyObjectRef)> {
        let flags = flags.unwrap_or(0);
        let bufsize = bufsize
            .to_usize()
            .ok_or_else(|| vm.new_value_error("negative buffersize in recvfrom".to_owned()))?;
        let mut buffer = Vec::with_capacity(bufsize);
        let (n, addr) = self.sock_op(vm, SelectKind::Read, || {
            self.sock()
                .recv_from_with_flags(spare_capacity_mut(&mut buffer), flags)
        })?;
        unsafe { buffer.set_len(n) };
        Ok((buffer, get_addr_tuple(&addr, vm)))
    }

    #[pymethod]
    fn recvfrom_into(
        &self,
        buf: PyRwBytesLike,
        nbytes: OptionalArg<isize>,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<(usize, PyObjectRef)> {
        let mut buf = buf.borrow_value();
        let buf = &mut *buf;
        let buf = match nbytes {
            OptionalArg::Present(i) => {
                let i = i.to_usize().ok_or_else(|| {
                    vm.new_value_error("negative buffersize in recvfrom_into".to_owned())
                })?;
                buf.get_mut(..i).ok_or_else(|| {
                    vm.new_value_error("nbytes is greater than the length of the buffer".to_owned())
                })?
            }
            OptionalArg::Missing => buf,
        };
        let flags = flags.unwrap_or(0);
        let sock = self.sock();
        let (n, addr) = self.sock_op(vm, SelectKind::Read, || {
            sock.recv_from_with_flags(slice_as_uninit(buf), flags)
        })?;
        Ok((n, get_addr_tuple(&addr, vm)))
    }

    #[pymethod]
    fn send(
        &self,
        bytes: PyBytesLike,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<usize> {
        let flags = flags.unwrap_or(0);
        let buf = bytes.borrow_value();
        let buf = &*buf;
        self.sock_op(vm, SelectKind::Write, || {
            self.sock().send_with_flags(buf, flags)
        })
    }

    #[pymethod]
    fn sendall(
        &self,
        bytes: PyBytesLike,
        flags: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        let flags = flags.unwrap_or(0);

        let timeout = self.get_timeout().ok();

        let deadline = timeout.map(Deadline::new);

        let buf = bytes.borrow_value();
        let buf = &*buf;
        let mut buf_offset = 0;
        // now we have like 3 layers of interrupt loop :)
        while buf_offset < buf.len() {
            let interval = deadline
                .as_ref()
                .map(|d| d.time_until().map_err(|e| e.into_pyexception(vm)))
                .transpose()?;
            self.sock_op_timeout_err(vm, SelectKind::Write, interval, || {
                let subbuf = &buf[buf_offset..];
                buf_offset += self.sock().send_with_flags(subbuf, flags)?;
                Ok(())
            })
            .map_err(|e| e.into_pyexception(vm))?;
            vm.check_signals()?;
        }
        Ok(())
    }

    #[pymethod]
    fn sendto(
        &self,
        bytes: PyBytesLike,
        arg2: PyObjectRef,
        arg3: OptionalArg<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult<usize> {
        // signature is bytes[, flags], address
        let (flags, address) = match arg3 {
            OptionalArg::Present(arg3) => {
                // should just be i32::try_from_obj but tests check for error message
                let int = vm.to_index_opt(arg2).unwrap_or_else(|| {
                    Err(vm.new_type_error("an integer is required".to_owned()))
                })?;
                let flags = int::try_to_primitive::<i32>(int.borrow_value(), vm)?;
                (flags, arg3)
            }
            OptionalArg::Missing => (0, arg2),
        };
        let addr = self.extract_address(address, "sendto", vm)?;
        let buf = bytes.borrow_value();
        let buf = &*buf;
        self.sock_op(vm, SelectKind::Write, || {
            self.sock().send_to_with_flags(buf, &addr, flags)
        })
    }

    #[pymethod]
    fn close(&self, vm: &VirtualMachine) -> PyResult<()> {
        let sock = self.detach();
        if sock != INVALID_SOCKET {
            _socket_close(sock, vm)?;
        }
        Ok(())
    }
    #[pymethod]
    #[inline]
    fn detach(&self) -> RawSocket {
        into_sock_fileno(std::mem::replace(&mut *self.sock_mut(), invalid_sock()))
    }

    #[pymethod]
    fn fileno(&self) -> RawSocket {
        sock_fileno(&self.sock())
    }

    #[pymethod]
    fn getsockname(&self, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        let addr = self
            .sock()
            .local_addr()
            .map_err(|err| err.into_pyexception(vm))?;

        Ok(get_addr_tuple(&addr, vm))
    }
    #[pymethod]
    fn getpeername(&self, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        let addr = self
            .sock()
            .peer_addr()
            .map_err(|err| err.into_pyexception(vm))?;

        Ok(get_addr_tuple(&addr, vm))
    }

    #[pymethod]
    fn gettimeout(&self) -> Option<f64> {
        let timeout = self.timeout.load();
        if timeout >= 0.0 {
            Some(timeout)
        } else {
            None
        }
    }

    #[pymethod]
    fn setblocking(&self, block: bool, vm: &VirtualMachine) -> PyResult<()> {
        self.timeout.store(if block { -1.0 } else { 0.0 });
        self.sock()
            .set_nonblocking(!block)
            .map_err(|err| err.into_pyexception(vm))
    }

    #[pymethod]
    fn getblocking(&self) -> bool {
        self.timeout.load() != 0.0
    }

    #[pymethod]
    fn settimeout(&self, timeout: Option<Duration>, vm: &VirtualMachine) -> PyResult<()> {
        self.timeout
            .store(timeout.map_or(-1.0, |d| d.as_secs_f64()));
        // even if timeout is > 0 the socket needs to be nonblocking in order for us to select() on
        // it
        self.sock()
            .set_nonblocking(timeout.is_some())
            .map_err(|err| err.into_pyexception(vm))
    }

    #[pymethod]
    fn getsockopt(
        &self,
        level: i32,
        name: i32,
        buflen: OptionalArg<i32>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let fd = sock_fileno(&self.sock()) as _;
        let buflen = buflen.unwrap_or(0);
        if buflen == 0 {
            let mut flag: libc::c_int = 0;
            let mut flagsize = std::mem::size_of::<libc::c_int>() as _;
            let ret = unsafe {
                c::getsockopt(
                    fd,
                    level,
                    name,
                    &mut flag as *mut libc::c_int as *mut _,
                    &mut flagsize,
                )
            };
            if ret < 0 {
                Err(super::os::errno_err(vm))
            } else {
                Ok(vm.ctx.new_int(flag))
            }
        } else {
            if buflen <= 0 || buflen > 1024 {
                return Err(vm.new_os_error("getsockopt buflen out of range".to_owned()));
            }
            let mut buf = vec![0u8; buflen as usize];
            let mut buflen = buflen as _;
            let ret =
                unsafe { c::getsockopt(fd, level, name, buf.as_mut_ptr() as *mut _, &mut buflen) };
            buf.truncate(buflen as usize);
            if ret < 0 {
                Err(super::os::errno_err(vm))
            } else {
                Ok(vm.ctx.new_bytes(buf))
            }
        }
    }

    #[pymethod]
    fn setsockopt(
        &self,
        level: i32,
        name: i32,
        value: Option<Either<PyBytesLike, i32>>,
        optlen: OptionalArg<u32>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        let fd = sock_fileno(&self.sock()) as _;
        let ret = match (value, optlen) {
            (Some(Either::A(b)), OptionalArg::Missing) => b.with_ref(|b| unsafe {
                c::setsockopt(fd, level, name, b.as_ptr() as *const _, b.len() as _)
            }),
            (Some(Either::B(ref val)), OptionalArg::Missing) => unsafe {
                c::setsockopt(
                    fd,
                    level,
                    name,
                    val as *const i32 as *const _,
                    std::mem::size_of::<i32>() as _,
                )
            },
            (None, OptionalArg::Present(optlen)) => unsafe {
                c::setsockopt(fd, level, name, std::ptr::null(), optlen as _)
            },
            _ => {
                return Err(
                    vm.new_type_error("expected the value arg xor the optlen arg".to_owned())
                );
            }
        };
        if ret < 0 {
            Err(super::os::errno_err(vm))
        } else {
            Ok(())
        }
    }

    #[pymethod]
    fn shutdown(&self, how: i32, vm: &VirtualMachine) -> PyResult<()> {
        let how = match how {
            c::SHUT_RD => Shutdown::Read,
            c::SHUT_WR => Shutdown::Write,
            c::SHUT_RDWR => Shutdown::Both,
            _ => {
                return Err(
                    vm.new_value_error("`how` must be SHUT_RD, SHUT_WR, or SHUT_RDWR".to_owned())
                )
            }
        };
        self.sock()
            .shutdown(how)
            .map_err(|err| err.into_pyexception(vm))
    }

    #[pyproperty(name = "type")]
    fn kind(&self) -> i32 {
        self.kind.load()
    }
    #[pyproperty]
    fn family(&self) -> i32 {
        self.family.load()
    }
    #[pyproperty]
    fn proto(&self) -> i32 {
        self.proto.load()
    }

    #[pymethod(magic)]
    fn repr(&self) -> String {
        format!(
            "<socket object, fd={}, family={}, type={}, proto={}>",
            // cast because INVALID_SOCKET is unsigned, so would show usize::MAX instead of -1
            sock_fileno(&self.sock()) as i64,
            self.family.load(),
            self.kind.load(),
            self.proto.load(),
        )
    }
}

impl io::Read for PySocketRef {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <&Socket as io::Read>::read(&mut &*self.sock(), buf)
    }
}
impl io::Write for PySocketRef {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <&Socket as io::Write>::write(&mut &*self.sock(), buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        <&Socket as io::Write>::flush(&mut &*self.sock())
    }
}

struct Address {
    host: PyStrRef,
    port: u16,
}

impl ToSocketAddrs for Address {
    type Iter = std::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        (self.host.borrow_value(), self.port).to_socket_addrs()
    }
}

impl TryFromObject for Address {
    fn try_from_object(vm: &VirtualMachine, obj: PyObjectRef) -> PyResult<Self> {
        let tuple = PyTupleRef::try_from_object(vm, obj)?;
        if tuple.borrow_value().len() != 2 {
            Err(vm.new_type_error("Address tuple should have only 2 values".to_owned()))
        } else {
            Self::from_tuple(tuple.borrow_value(), vm)
        }
    }
}

impl Address {
    fn from_tuple(tuple: &[PyObjectRef], vm: &VirtualMachine) -> PyResult<Self> {
        let host = PyStrRef::try_from_object(vm, tuple[0].clone())?;
        let port = i32::try_from_object(vm, tuple[1].clone())?;
        let port = port
            .to_u16()
            .ok_or_else(|| vm.new_overflow_error("port must be 0-65535.".to_owned()))?;
        Ok(Address { host, port })
    }
    fn from_tuple_ipv6(tuple: &[PyObjectRef], vm: &VirtualMachine) -> PyResult<(Self, u32, u32)> {
        let addr = Address::from_tuple(tuple, vm)?;
        let flowinfo = tuple
            .get(2)
            .map(|obj| u32::try_from_object(vm, obj.clone()))
            .transpose()?
            .unwrap_or(0);
        let scopeid = tuple
            .get(3)
            .map(|obj| u32::try_from_object(vm, obj.clone()))
            .transpose()?
            .unwrap_or(0);
        if flowinfo > 0xfffff {
            return Err(vm.new_overflow_error("flowinfo must be 0-1048575.".to_owned()));
        }
        Ok((addr, flowinfo, scopeid))
    }
}

fn get_ip_addr_tuple(addr: &SocketAddr, vm: &VirtualMachine) -> PyObjectRef {
    match addr {
        SocketAddr::V4(addr) => (addr.ip().to_string(), addr.port()).into_pyobject(vm),
        SocketAddr::V6(addr) => (
            addr.ip().to_string(),
            addr.port(),
            addr.flowinfo(),
            addr.scope_id(),
        )
            .into_pyobject(vm),
    }
}

fn get_addr_tuple(addr: &socket2::SockAddr, vm: &VirtualMachine) -> PyObjectRef {
    if let Some(addr) = addr.as_socket() {
        return get_ip_addr_tuple(&addr, vm);
    }
    match addr.family() as i32 {
        #[cfg(unix)]
        libc::AF_UNIX => {
            let unix_addr = unsafe { &*(addr.as_ptr() as *const libc::sockaddr_un) };
            let socket_path = unsafe { std::ffi::CStr::from_ptr(unix_addr.sun_path.as_ptr()) };
            vm.ctx.new_str(socket_path.to_string_lossy().into_owned())
        }
        // TODO: support more address families
        _ => (String::new(), 0).into_pyobject(vm),
    }
}

fn _socket_gethostname(vm: &VirtualMachine) -> PyResult {
    gethostname()
        .into_string()
        .map(|hostname| vm.ctx.new_str(hostname))
        .map_err(|err| vm.new_os_error(err.into_string().unwrap()))
}

#[cfg(all(unix, not(target_os = "redox")))]
fn _socket_sethostname(hostname: PyStrRef, vm: &VirtualMachine) -> PyResult<()> {
    sethostname(hostname.borrow_value()).map_err(|err| err.into_pyexception(vm))
}

fn _socket_inet_aton(ip_string: PyStrRef, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
    ip_string
        .borrow_value()
        .parse::<Ipv4Addr>()
        .map(|ip_addr| Vec::<u8>::from(ip_addr.octets()))
        .map_err(|_| vm.new_os_error("illegal IP address string passed to inet_aton".to_owned()))
}

fn _socket_inet_ntoa(packed_ip: PyBytesLike, vm: &VirtualMachine) -> PyResult {
    let packed_ip = packed_ip.borrow_value();
    let packed_ip = <&[u8; 4]>::try_from(&*packed_ip)
        .map_err(|_| vm.new_os_error("packed IP wrong length for inet_ntoa".to_owned()))?;
    Ok(vm.ctx.new_str(Ipv4Addr::from(*packed_ip).to_string()))
}

fn _socket_getservbyname(
    servicename: PyStrRef,
    protocolname: OptionalArg<PyStrRef>,
    vm: &VirtualMachine,
) -> PyResult {
    use std::ffi::CString;
    let cstr_name = CString::new(servicename.borrow_value())
        .map_err(|_| vm.new_value_error("embedded null character".to_owned()))?;
    let cstr_proto = protocolname
        .as_ref()
        .map(|s| CString::new(s.borrow_value()))
        .transpose()
        .map_err(|_| vm.new_value_error("embedded null character".to_owned()))?;
    let cstr_proto = cstr_proto
        .as_ref()
        .map_or_else(std::ptr::null, |s| s.as_ptr());
    let serv = unsafe { c::getservbyname(cstr_name.as_ptr(), cstr_proto) };
    if serv.is_null() {
        return Err(vm.new_os_error("service/proto not found".to_owned()));
    }
    let port = unsafe { (*serv).s_port };
    Ok(vm.ctx.new_int(u16::from_be(port as u16)))
}

// TODO: use `Vec::spare_capacity_mut` once stable.
fn spare_capacity_mut<T>(v: &mut Vec<T>) -> &mut [MaybeUninit<T>] {
    let (len, cap) = (v.len(), v.capacity());
    unsafe {
        std::slice::from_raw_parts_mut(v.as_mut_ptr().add(len) as *mut MaybeUninit<T>, cap - len)
    }
}
fn slice_as_uninit<T>(v: &mut [T]) -> &mut [MaybeUninit<T>] {
    unsafe { &mut *(v as *mut [T] as *mut [MaybeUninit<T>]) }
}

enum IoOrPyException {
    Timeout,
    Py(PyBaseExceptionRef),
    Io(io::Error),
}
impl From<PyBaseExceptionRef> for IoOrPyException {
    fn from(exc: PyBaseExceptionRef) -> Self {
        Self::Py(exc)
    }
}
impl From<io::Error> for IoOrPyException {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}
impl IoOrPyException {
    fn errno(self) -> PyResult<i32> {
        match self {
            Self::Timeout => Ok(errcode!(EWOULDBLOCK)),
            Self::Io(err) => {
                // TODO: just unwrap()?
                Ok(err.raw_os_error().unwrap_or(1))
            }
            Self::Py(exc) => Err(exc),
        }
    }
}
impl IntoPyException for IoOrPyException {
    fn into_pyexception(self, vm: &VirtualMachine) -> PyBaseExceptionRef {
        match self {
            Self::Timeout => timeout_error(vm),
            Self::Py(exc) => exc,
            Self::Io(err) => err.into_pyexception(vm),
        }
    }
}

#[derive(Copy, Clone)]
pub(super) enum SelectKind {
    Read,
    Write,
    Connect,
}

/// returns true if timed out
pub(super) fn sock_select(
    sock: &Socket,
    kind: SelectKind,
    interval: Option<Duration>,
) -> io::Result<bool> {
    let fd = sock_fileno(sock);
    #[cfg(unix)]
    {
        let mut pollfd = libc::pollfd {
            fd,
            events: match kind {
                SelectKind::Read => libc::POLLIN,
                SelectKind::Write => libc::POLLOUT,
                SelectKind::Connect => libc::POLLOUT | libc::POLLERR,
            },
            revents: 0,
        };
        let timeout = match interval {
            Some(d) => d.as_millis() as _,
            None => -1,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret == 0)
        }
    }
    #[cfg(windows)]
    {
        use crate::stdlib::select;

        let mut reads = select::FdSet::new();
        let mut writes = select::FdSet::new();
        let mut errs = select::FdSet::new();

        let fd = fd as usize;
        match kind {
            SelectKind::Read => reads.insert(fd),
            SelectKind::Write => writes.insert(fd),
            SelectKind::Connect => {
                writes.insert(fd);
                errs.insert(fd);
            }
        }

        let mut interval = interval.map(|dur| select::timeval {
            tv_sec: dur.as_secs() as _,
            tv_usec: dur.subsec_micros() as _,
        });

        select::select(
            fd as i32 + 1,
            &mut reads,
            &mut writes,
            &mut errs,
            interval.as_mut(),
        )
        .map(|ret| ret == 0)
    }
}

#[derive(FromArgs)]
struct GAIOptions {
    #[pyarg(positional)]
    host: Option<PyStrRef>,
    #[pyarg(positional)]
    port: Option<Either<PyStrRef, i32>>,

    #[pyarg(positional, default = "c::AF_UNSPEC")]
    family: i32,
    #[pyarg(positional, default = "0")]
    ty: i32,
    #[pyarg(positional, default = "0")]
    proto: i32,
    #[pyarg(positional, default = "0")]
    flags: i32,
}

fn _socket_getaddrinfo(opts: GAIOptions, vm: &VirtualMachine) -> PyResult {
    let hints = dns_lookup::AddrInfoHints {
        socktype: opts.ty,
        protocol: opts.proto,
        address: opts.family,
        flags: opts.flags,
    };

    let host = opts.host.as_ref().map(|s| s.borrow_value());
    let port = opts.port.as_ref().map(|p| -> std::borrow::Cow<str> {
        match p {
            Either::A(ref s) => s.borrow_value().into(),
            Either::B(i) => i.to_string().into(),
        }
    });
    let port = port.as_ref().map(|p| p.as_ref());

    let addrs = dns_lookup::getaddrinfo(host, port, Some(hints))
        .map_err(|err| convert_gai_error(vm, err))?;

    let list = addrs
        .map(|ai| {
            ai.map(|ai| {
                vm.ctx.new_tuple(vec![
                    vm.ctx.new_int(ai.address),
                    vm.ctx.new_int(ai.socktype),
                    vm.ctx.new_int(ai.protocol),
                    ai.canonname.into_pyobject(vm),
                    get_ip_addr_tuple(&ai.sockaddr, vm),
                ])
            })
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| e.into_pyexception(vm))?;
    Ok(vm.ctx.new_list(list))
}

fn _socket_gethostbyaddr(
    addr: PyStrRef,
    vm: &VirtualMachine,
) -> PyResult<(String, PyObjectRef, PyObjectRef)> {
    // TODO: figure out how to do this properly
    let addr = get_addr(vm, addr.borrow_value(), c::AF_UNSPEC)?;
    let (hostname, _) = dns_lookup::getnameinfo(&addr, 0).map_err(|e| convert_gai_error(vm, e))?;
    Ok((
        hostname,
        vm.ctx.new_list(vec![]),
        vm.ctx.new_list(vec![vm.ctx.new_str(addr.ip().to_string())]),
    ))
}

fn _socket_gethostbyname(name: PyStrRef, vm: &VirtualMachine) -> PyResult<String> {
    // TODO: convert to idna
    let addr = get_addr(vm, name.borrow_value(), c::AF_INET)?;
    match addr {
        SocketAddr::V4(ip) => Ok(ip.ip().to_string()),
        _ => unreachable!(),
    }
}

fn _socket_inet_pton(af_inet: i32, ip_string: PyStrRef, vm: &VirtualMachine) -> PyResult {
    match af_inet {
        c::AF_INET => ip_string
            .borrow_value()
            .parse::<Ipv4Addr>()
            .map(|ip_addr| vm.ctx.new_bytes(ip_addr.octets().to_vec()))
            .map_err(|_| {
                vm.new_os_error("illegal IP address string passed to inet_pton".to_owned())
            }),
        c::AF_INET6 => ip_string
            .borrow_value()
            .parse::<Ipv6Addr>()
            .map(|ip_addr| vm.ctx.new_bytes(ip_addr.octets().to_vec()))
            .map_err(|_| {
                vm.new_os_error("illegal IP address string passed to inet_pton".to_owned())
            }),
        _ => Err(vm.new_os_error("Address family not supported by protocol".to_owned())),
    }
}

fn _socket_inet_ntop(
    af_inet: i32,
    packed_ip: PyBytesLike,
    vm: &VirtualMachine,
) -> PyResult<String> {
    let packed_ip = packed_ip.borrow_value();
    match af_inet {
        c::AF_INET => {
            let packed_ip = <&[u8; 4]>::try_from(&*packed_ip).map_err(|_| {
                vm.new_value_error("invalid length of packed IP address string".to_owned())
            })?;
            Ok(Ipv4Addr::from(*packed_ip).to_string())
        }
        c::AF_INET6 => {
            let packed_ip = <&[u8; 16]>::try_from(&*packed_ip).map_err(|_| {
                vm.new_value_error("invalid length of packed IP address string".to_owned())
            })?;
            Ok(get_ipv6_addr_str(Ipv6Addr::from(*packed_ip)))
        }
        _ => Err(vm.new_value_error(format!("unknown address family {}", af_inet))),
    }
}

fn _socket_getprotobyname(name: PyStrRef, vm: &VirtualMachine) -> PyResult {
    use std::ffi::CString;
    let cstr = CString::new(name.borrow_value())
        .map_err(|_| vm.new_value_error("embedded null character".to_owned()))?;
    let proto = unsafe { c::getprotobyname(cstr.as_ptr()) };
    if proto.is_null() {
        return Err(vm.new_os_error("protocol not found".to_owned()));
    }
    let num = unsafe { (*proto).p_proto };
    Ok(vm.ctx.new_int(num))
}

fn _socket_getnameinfo(
    address: PyTupleRef,
    flags: i32,
    vm: &VirtualMachine,
) -> PyResult<(String, String)> {
    let address = address.borrow_value();
    match address.len() {
        2 | 3 | 4 => {}
        _ => return Err(vm.new_type_error("illegal sockaddr argument".to_owned())),
    }
    let (addr, flowinfo, scopeid) = Address::from_tuple_ipv6(address, vm)?;
    let hints = dns_lookup::AddrInfoHints {
        address: c::AF_UNSPEC,
        socktype: c::SOCK_DGRAM,
        flags: c::AI_NUMERICHOST,
        protocol: 0,
    };
    let service = addr.port.to_string();
    let mut res =
        dns_lookup::getaddrinfo(Some(addr.host.borrow_value()), Some(&service), Some(hints))
            .map_err(|e| convert_gai_error(vm, e))?
            .filter_map(Result::ok);
    let mut ainfo = res.next().unwrap();
    if res.next().is_some() {
        return Err(vm.new_os_error("sockaddr resolved to multiple addresses".to_owned()));
    }
    match &mut ainfo.sockaddr {
        SocketAddr::V4(_) => {
            if address.len() != 2 {
                return Err(vm.new_os_error("IPv4 sockaddr must be 2 tuple".to_owned()));
            }
        }
        SocketAddr::V6(addr) => {
            addr.set_flowinfo(flowinfo);
            addr.set_scope_id(scopeid);
        }
    }
    dns_lookup::getnameinfo(&ainfo.sockaddr, flags).map_err(|e| convert_gai_error(vm, e))
}

#[cfg(unix)]
fn _socket_socketpair(
    family: OptionalArg<i32>,
    socket_kind: OptionalArg<i32>,
    proto: OptionalArg<i32>,
    vm: &VirtualMachine,
) -> PyResult<(PySocket, PySocket)> {
    let family = family.unwrap_or(libc::AF_UNIX);
    let socket_kind = socket_kind.unwrap_or(libc::SOCK_STREAM);
    let proto = proto.unwrap_or(0);
    let (a, b) = Socket::pair(family.into(), socket_kind.into(), Some(proto.into()))
        .map_err(|e| e.into_pyexception(vm))?;
    let py_a = PySocket::default();
    py_a.init_inner(family, socket_kind, proto, a, vm)?;
    let py_b = PySocket::default();
    py_b.init_inner(family, socket_kind, proto, b, vm)?;
    Ok((py_a, py_b))
}

fn get_addr(vm: &VirtualMachine, name: &str, af: i32) -> PyResult<SocketAddr> {
    if name.is_empty() {
        let hints = dns_lookup::AddrInfoHints {
            address: af,
            socktype: c::SOCK_DGRAM,
            flags: c::AI_PASSIVE,
            protocol: 0,
        };
        let mut res = dns_lookup::getaddrinfo(None, Some("0"), Some(hints))
            .map_err(|e| convert_gai_error(vm, e))?;
        let ainfo = res.next().unwrap().map_err(|e| e.into_pyexception(vm))?;
        if res.next().is_some() {
            return Err(vm.new_os_error("wildcard resolved to multiple address".to_owned()));
        }
        return Ok(ainfo.sockaddr);
    }
    if name == "255.255.255.255" || name == "<broadcast>" {
        match af {
            c::AF_INET | c::AF_UNSPEC => {}
            _ => return Err(vm.new_os_error("address family mismatched".to_owned())),
        }
        return Ok(SocketAddr::V4(net::SocketAddrV4::new(
            c::INADDR_BROADCAST.into(),
            0,
        )));
    }
    if let c::AF_INET | c::AF_UNSPEC = af {
        if let Ok(addr) = name.parse::<Ipv4Addr>() {
            return Ok(SocketAddr::V4(net::SocketAddrV4::new(addr, 0)));
        }
    }
    if matches!(af, c::AF_INET | c::AF_UNSPEC) && !name.contains('%') {
        if let Ok(addr) = name.parse::<Ipv6Addr>() {
            return Ok(SocketAddr::V6(net::SocketAddrV6::new(addr, 0, 0, 0)));
        }
    }
    let hints = dns_lookup::AddrInfoHints {
        address: af,
        ..Default::default()
    };
    let mut res = dns_lookup::getaddrinfo(Some(name), None, Some(hints))
        .map_err(|e| convert_gai_error(vm, e))?;
    res.next()
        .unwrap()
        .map(|ainfo| ainfo.sockaddr)
        .map_err(|e| e.into_pyexception(vm))
}

fn sock_from_raw(fileno: RawSocket, vm: &VirtualMachine) -> PyResult<Socket> {
    let invalid = {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                fileno == INVALID_SOCKET
            } else {
                fileno < 0
            }
        }
    };
    if invalid {
        return Err(vm.new_value_error("negative file descriptor".to_owned()));
    }
    Ok(unsafe { sock_from_raw_unchecked(fileno) })
}
/// SAFETY: fileno must not be equal to INVALID_SOCKET
unsafe fn sock_from_raw_unchecked(fileno: RawSocket) -> Socket {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        Socket::from_raw_fd(fileno)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawSocket;
        Socket::from_raw_socket(fileno)
    }
}
pub(super) fn sock_fileno(sock: &Socket) -> RawSocket {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        sock.as_raw_fd()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        sock.as_raw_socket()
    }
}
fn into_sock_fileno(sock: Socket) -> RawSocket {
    #[cfg(unix)]
    {
        use std::os::unix::io::IntoRawFd;
        sock.into_raw_fd()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::IntoRawSocket;
        sock.into_raw_socket()
    }
}

pub(super) const INVALID_SOCKET: RawSocket = {
    #[cfg(unix)]
    {
        -1
    }
    #[cfg(windows)]
    {
        winapi::um::winsock2::INVALID_SOCKET as RawSocket
    }
};
fn invalid_sock() -> Socket {
    // TODO: socket2 might make Socket have a niche at -1, so this may be UB in the future
    unsafe { sock_from_raw_unchecked(INVALID_SOCKET) }
}

fn convert_gai_error(vm: &VirtualMachine, err: dns_lookup::LookupError) -> PyBaseExceptionRef {
    if let dns_lookup::LookupErrorKind::System = err.kind() {
        return io::Error::from(err).into_pyexception(vm);
    }
    let strerr = {
        #[cfg(unix)]
        {
            let s = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(err.error_num())) };
            std::str::from_utf8(s.to_bytes()).unwrap()
        }
        #[cfg(windows)]
        {
            "getaddrinfo failed"
        }
    };
    vm.new_exception(
        GAI_ERROR.get().unwrap().clone(),
        vec![vm.ctx.new_int(err.error_num()), vm.ctx.new_str(strerr)],
    )
}

fn timeout_error(vm: &VirtualMachine) -> PyBaseExceptionRef {
    timeout_error_msg(vm, "timed out".to_owned())
}
pub(super) fn timeout_error_msg(vm: &VirtualMachine, msg: String) -> PyBaseExceptionRef {
    vm.new_exception_msg(TIMEOUT_ERROR.get().unwrap().clone(), msg)
}

fn get_ipv6_addr_str(ipv6: Ipv6Addr) -> String {
    match ipv6.to_ipv4() {
        // instead of "::0.0.ddd.ddd" it's "::xxxx"
        Some(v4) if !ipv6.is_unspecified() && matches!(v4.octets(), [0, 0, _, _]) => {
            format!("::{:x}", u32::from(v4))
        }
        _ => ipv6.to_string(),
    }
}

pub(crate) struct Deadline {
    deadline: Instant,
}

impl Deadline {
    fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }
    fn time_until(&self) -> Result<Duration, IoOrPyException> {
        self.deadline
            .checked_duration_since(Instant::now())
            // past the deadline already
            .ok_or(IoOrPyException::Timeout)
    }
}

static DEFAULT_TIMEOUT: AtomicCell<f64> = AtomicCell::new(-1.0);

fn _socket_getdefaulttimeout() -> Option<f64> {
    let timeout = DEFAULT_TIMEOUT.load();
    if timeout >= 0.0 {
        Some(timeout)
    } else {
        None
    }
}

fn _socket_setdefaulttimeout(timeout: Option<Duration>) {
    DEFAULT_TIMEOUT.store(timeout.map_or(-1.0, |d| d.as_secs_f64()));
}

fn _socket_dup(x: RawSocket, vm: &VirtualMachine) -> PyResult<RawSocket> {
    let sock = std::mem::ManuallyDrop::new(sock_from_raw(x, vm)?);
    sock.try_clone()
        .map(into_sock_fileno)
        .map_err(|e| e.into_pyexception(vm))
}

fn _socket_close(x: RawSocket, vm: &VirtualMachine) -> PyResult<()> {
    #[cfg(unix)]
    use libc::close;
    #[cfg(windows)]
    use winapi::um::winsock2::closesocket as close;
    let ret = unsafe { close(x as _) };
    if ret < 0 {
        let err = super::os::errno();
        if err.raw_os_error() != Some(errcode!(ECONNRESET)) {
            return Err(err.into_pyexception(vm));
        }
    }
    Ok(())
}

rustpython_common::static_cell! {
    static TIMEOUT_ERROR: PyTypeRef;
    static GAI_ERROR: PyTypeRef;
}

pub fn make_module(vm: &VirtualMachine) -> PyObjectRef {
    init_winsock();

    let ctx = &vm.ctx;
    let socket_timeout = TIMEOUT_ERROR
        .get_or_init(|| {
            ctx.new_class(
                "socket.timeout",
                &vm.ctx.exceptions.os_error,
                Default::default(),
            )
        })
        .clone();
    let socket_gaierror = GAI_ERROR
        .get_or_init(|| {
            ctx.new_class(
                "socket.gaierror",
                &vm.ctx.exceptions.os_error,
                Default::default(),
            )
        })
        .clone();

    let socket = PySocket::make_class(ctx);
    let module = py_module!(vm, "_socket", {
        "socket" => socket.clone(),
        "SocketType" => socket,
        "error" => ctx.exceptions.os_error.clone(),
        "timeout" => socket_timeout,
        "gaierror" => socket_gaierror,
        "inet_aton" => named_function!(ctx, _socket, inet_aton),
        "inet_ntoa" => named_function!(ctx, _socket, inet_ntoa),
        "gethostname" => named_function!(ctx, _socket, gethostname),
        "htonl" => ctx.new_function("htonl", u32::to_be),
        "htons" => ctx.new_function("htons", u16::to_be),
        "ntohl" => ctx.new_function("ntohl", u32::from_be),
        "ntohs" => ctx.new_function("ntohs", u16::from_be),
        "getdefaulttimeout" => named_function!(ctx, _socket, getdefaulttimeout),
        "setdefaulttimeout" => named_function!(ctx, _socket, setdefaulttimeout),
        "has_ipv6" => ctx.new_bool(true),
        "inet_pton" => named_function!(ctx, _socket, inet_pton),
        "inet_ntop" => named_function!(ctx, _socket, inet_ntop),
        "getprotobyname" => named_function!(ctx, _socket, getprotobyname),
        "getservbyname" => named_function!(ctx, _socket, getservbyname),
        "dup" => named_function!(ctx, _socket, dup),
        "close" => named_function!(ctx, _socket, close),
        "getaddrinfo" => named_function!(ctx, _socket, getaddrinfo),
        "gethostbyaddr" => named_function!(ctx, _socket, gethostbyaddr),
        "gethostbyname" => named_function!(ctx, _socket, gethostbyname),
        "getnameinfo" => named_function!(ctx, _socket, getnameinfo),
        // constants
        "AF_UNSPEC" => ctx.new_int(0),
        "AF_INET" => ctx.new_int(c::AF_INET),
        "AF_INET6" => ctx.new_int(c::AF_INET6),
        "SOCK_STREAM" => ctx.new_int(c::SOCK_STREAM),
        "SOCK_DGRAM" => ctx.new_int(c::SOCK_DGRAM),
        "SHUT_RD" => ctx.new_int(c::SHUT_RD),
        "SHUT_WR" => ctx.new_int(c::SHUT_WR),
        "SHUT_RDWR" => ctx.new_int(c::SHUT_RDWR),
        "MSG_PEEK" => ctx.new_int(c::MSG_PEEK),
        "MSG_OOB" => ctx.new_int(c::MSG_OOB),
        "MSG_WAITALL" => ctx.new_int(c::MSG_WAITALL),
        "IPPROTO_TCP" => ctx.new_int(c::IPPROTO_TCP),
        "IPPROTO_UDP" => ctx.new_int(c::IPPROTO_UDP),
        "IPPROTO_IP" => ctx.new_int(c::IPPROTO_IP),
        "IPPROTO_IPIP" => ctx.new_int(c::IPPROTO_IP),
        "IPPROTO_IPV6" => ctx.new_int(c::IPPROTO_IPV6),
        "SOL_SOCKET" => ctx.new_int(c::SOL_SOCKET),
        "SOL_TCP" => ctx.new_int(6),
        "SO_REUSEADDR" => ctx.new_int(c::SO_REUSEADDR),
        "SO_TYPE" => ctx.new_int(c::SO_TYPE),
        "SO_BROADCAST" => ctx.new_int(c::SO_BROADCAST),
        "SO_OOBINLINE" => ctx.new_int(c::SO_OOBINLINE),
        "SO_ERROR" => ctx.new_int(c::SO_ERROR),
        "SO_LINGER" => ctx.new_int(c::SO_LINGER),
        "TCP_NODELAY" => ctx.new_int(c::TCP_NODELAY),
        "NI_NAMEREQD" => ctx.new_int(c::NI_NAMEREQD),
        "NI_NOFQDN" => ctx.new_int(c::NI_NOFQDN),
        "NI_NUMERICHOST" => ctx.new_int(c::NI_NUMERICHOST),
        "NI_NUMERICSERV" => ctx.new_int(c::NI_NUMERICSERV),
    });

    #[cfg(not(target_os = "freebsd"))]
    extend_module!(vm, module, {
        "AI_PASSIVE" => ctx.new_int(c::AI_PASSIVE),
        "AI_NUMERICHOST" => ctx.new_int(c::AI_NUMERICHOST),
        "AI_ALL" => ctx.new_int(c::AI_ALL),
        "AI_ADDRCONFIG" => ctx.new_int(c::AI_ADDRCONFIG),
        "AI_NUMERICSERV" => ctx.new_int(c::AI_NUMERICSERV),
    });

    #[cfg(not(target_os = "redox"))]
    extend_module!(vm, module, {
        "SOCK_RAW" => ctx.new_int(c::SOCK_RAW),
        "SOCK_RDM" => ctx.new_int(c::SOCK_RDM),
    });

    extend_module_platform_specific(vm, &module);

    module
}

#[cfg(not(unix))]
fn extend_module_platform_specific(_vm: &VirtualMachine, _module: &PyObjectRef) {}

#[cfg(unix)]
fn extend_module_platform_specific(vm: &VirtualMachine, module: &PyObjectRef) {
    let ctx = &vm.ctx;

    extend_module!(vm, module, {
        "socketpair" => named_function!(ctx, _socket, socketpair),
        "AF_UNIX" => ctx.new_int(c::AF_UNIX),
        "SO_REUSEPORT" => ctx.new_int(c::SO_REUSEPORT),
    });

    #[cfg(not(target_os = "redox"))]
    extend_module!(vm, module, {
        "sethostname" => named_function!(ctx, _socket, sethostname),
        "SOCK_SEQPACKET" => ctx.new_int(c::SOCK_SEQPACKET),
    });
}

pub fn init_winsock() {
    #[cfg(windows)]
    {
        static WSA_INIT: parking_lot::Once = parking_lot::Once::new();
        WSA_INIT.call_once(|| {
            let _ = unsafe { winapi::um::winsock2::WSAStartup(0x0101, &mut std::mem::zeroed()) };
        })
    }
}
