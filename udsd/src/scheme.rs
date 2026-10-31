use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use libredox::protocol::SocketCall;
use libredox::Fd;
use rand::rngs::SmallRng;
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, Response, SendFdRequest, SignalBehavior,
    Socket as SchemeSocket,
};
use scheme_utils::FpathWriter;
use syscall::error::*;
use syscall::flag::*;
use syscall::schemev2::NewFdFlags;
use syscall::{Error, Stat};

use crate::common::{create_token_generator, read_num};

#[derive(Clone, Copy, Default)]
pub struct MsgFlags(libc::c_int);

impl MsgFlags {
    pub(super) fn nonblock(&self) -> bool {
        self.0 & libc::MSG_DONTWAIT == libc::MSG_DONTWAIT
    }
}

pub trait Socket: Sized {
    fn handle_unnamed_socket(scheme: &mut UdsScheme<Self>, flags: usize, ctx: &CallerCtx) -> usize;
    fn handle_bind(scheme: &mut UdsScheme<Self>, id: usize, path_buf: &[u8]) -> Result<usize>;

    /// There are three phases of connecting a socket:
    ///
    /// Phase 1: The listener is bound but not yet listening.
    ///          The client is trying to connect.
    ///          If the listener is not listening, the listener will
    ///          refuse to connect until the listener starts listening.
    ///
    /// Phase 2: The listener is now listening.
    ///          The client is still trying to connect.
    ///          The client pushes its ID to the listener's awaiting queue
    ///          and sets its state to `Connecting`.
    ///          The client will be blocked from receiving messages,
    ///          but now allowed to send messages.
    ///
    /// Phase 3: The listener accepts the client, changes its state to `Established`,
    ///          and then changes the client's state to `Accepted`.
    ///          The client detects that its state has changed to `Accepted`
    ///          and changes its own state to `Established`.
    ///
    /// After these three phases, the socket connection is considered established.
    fn handle_connect(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        target_socket: &RefCell<Self>,
    ) -> Result<usize>;

    fn handle_setsockopt(&mut self, option: i32, value_slice: &[u8]) -> Result<usize>;
    fn handle_getsockopt(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        option: i32,
        payload: &mut [u8],
    ) -> Result<usize>;

    fn handle_sendmsg(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        msg_flags: MsgFlags,
        msg_stream: &[u8],
        ctx: &CallerCtx,
    ) -> Result<usize>;
    fn handle_recvmsg(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        msg_flags: MsgFlags,
        msg_stream: &mut [u8],
    ) -> Result<usize>;

    fn handle_unbind(&mut self) -> Result<usize>;

    fn handle_get_peer_name(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        payload: &mut [u8],
    ) -> Result<usize>;
    /// Handle a `dup` call for `b"listen"`.
    /// If the socket is not yet listening, it transitions it to the Listening state.
    /// If it is already listening, it tries to accept a pending connection.
    fn handle_listen(
        scheme: &mut UdsScheme<Self>,
        id: usize,
        ctx: &CallerCtx,
    ) -> Result<OpenResult>;
    fn handle_connect_socketpair(
        scheme: &mut UdsScheme<Self>,
        id: usize,
        ctx: &CallerCtx,
    ) -> Result<OpenResult>;
    fn handle_recvfd(&mut self) -> Result<OpenResult>;

    fn write_inner(
        scheme: &UdsScheme<Self>,
        sender: &RefCell<Self>,
        buf: &[u8],
        ctx: &CallerCtx,
    ) -> Result<usize>;
    fn sendfd_inner(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        sendfd_request: &SendFdRequest,
    ) -> Result<usize>;
    fn recvfd_inner(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        recvfd_request: &RecvFdRequest,
    ) -> Result<OpenResult>;
    fn read_inner(&mut self, buf: &mut [u8], flags: u32) -> Result<usize>;

    fn handle_closure(scheme: &mut UdsScheme<Self>, id: usize, socket_rc: Rc<RefCell<Self>>);

    fn path(&self) -> Option<&str>;
    fn events(&self) -> EventFlags;
    fn get_flags(&self) -> usize;
    fn set_flags(&mut self, flags: usize);
    fn issued_token(&self) -> Option<u64>;
}

enum Handle<S: Socket> {
    Socket(Rc<RefCell<S>>),
    Entrypoint(Rc<RefCell<S>>),
    SchemeRoot,
}

impl<S: Socket> Handle<S> {
    fn as_socket(&self) -> Option<&Rc<RefCell<S>>> {
        if let Self::Socket(socket) = self {
            Some(socket)
        } else {
            None
        }
    }
    fn is_scheme_root(&self) -> bool {
        matches!(self, Self::SchemeRoot)
    }
}

pub struct UdsScheme<'sock, S: Socket> {
    handles: BTreeMap<usize, Handle<S>>,
    pub(super) next_id: usize,
    pub(super) socket_tokens: BTreeMap<u64, Rc<RefCell<S>>>,
    pub(super) socket: &'sock SchemeSocket,
    pub(super) scheme_name: String,
    pub(super) proc_creds_capability: Fd,
    pub(super) rng: SmallRng,
}

impl<'sock, S: Socket> UdsScheme<'sock, S> {
    pub fn new(socket: &'sock SchemeSocket, scheme_name: String) -> Result<Self> {
        Ok(Self {
            handles: BTreeMap::new(),
            next_id: 0,
            socket_tokens: BTreeMap::new(),
            socket,
            scheme_name,
            proc_creds_capability: {
                libredox::Fd::open(
                    "/scheme/proc/proc-creds-capability",
                    libredox::flag::O_RDONLY,
                    0,
                )?
            },
            rng: create_token_generator(),
        })
    }

    pub(super) fn post_fevent(&self, id: usize, flags: EventFlags) -> Result<()> {
        /*TODO: filter out unnecessary flags?
        if let Ok(socket_rc) = self.get_socket(id) {
            let socket = socket_rc.borrow();
            let socket_flags = socket.events();
        }
        */
        let fevent_response = Response::post_fevent(id, flags.bits());
        match self
            .socket
            .write_response(fevent_response, SignalBehavior::Restart)
        {
            Ok(true) => Ok(()),                   // Write response success
            Ok(false) => Err(Error::new(EAGAIN)), // Write response failed, retry.
            Err(err) => Err(err),                 // Error writing response
        }
    }

    pub(super) fn get_socket(&self, id: usize) -> Result<&Rc<RefCell<S>>, Error> {
        self.handles
            .get(&id)
            .ok_or(Error::new(EBADF))?
            .as_socket()
            .ok_or(Error::new(EINVAL))
    }

    pub(super) fn insert_socket(&mut self, id: usize, socket: Rc<RefCell<S>>) {
        self.handles.insert(id, Handle::Socket(socket));
    }

    pub(super) fn fpath_inner(&self, path: &str, buf: &mut [u8]) -> Result<usize> {
        FpathWriter::with(buf, &self.scheme_name, |w| {
            w.push_str(path);
            Ok(())
        })
    }
}

impl<'sock, S: Socket> SchemeSync for UdsScheme<'sock, S> {
    fn scheme_root(&mut self) -> Result<usize> {
        let new_id = self.next_id;
        self.handles.insert(new_id, Handle::SchemeRoot);
        self.next_id += 1;
        Ok(new_id)
    }
    fn openat(
        &mut self,
        fd: usize,
        path: &str,
        mut flags: usize,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        {
            let Some(handle) = self.handles.get(&fd) else {
                return Err(Error::new(EBADF));
            };
            if !handle.is_scheme_root() {
                eprintln!(
                    "openat(fd: {}, path: '{}'): fd is not an open capability.",
                    fd, path
                );
                return Err(Error::new(EACCES));
            }
        }

        flags |= fcntl_flags as usize;

        let new_id = if path.is_empty() {
            if flags & O_CREAT == O_CREAT {
                S::handle_unnamed_socket(self, flags, ctx)
            } else {
                if flags & O_STAT != O_STAT {
                    eprintln!(
                        "uds_stream: open({:?}, {:x}): Attempting to open an unnamed socket without O_CREAT.",
                        path, flags
                    );
                }
                return Err(Error::new(EINVAL));
            }
        } else {
            eprintln!(
                "uds_stream: open({:?}): Attempting to open a named socket, which is not supported.",
                path
            );
            return Err(Error::new(EINVAL));
        };
        Ok(OpenResult::ThisScheme {
            number: new_id,
            flags: NewFdFlags::empty(),
        })
    }

    fn call(
        &mut self,
        id: usize,
        payload: &mut [u8],
        metadata: &[u64],
        ctx: &CallerCtx,
    ) -> Result<usize> {
        let Some(verb) =
            SocketCall::try_from_raw(*metadata.get(0).ok_or(Error::new(EINVAL))? as usize)
        else {
            eprintln!("call_inner: Invalid verb in metadata: {:?}", metadata);
            return Err(Error::new(EINVAL));
        };
        match verb {
            SocketCall::GetToken => {
                let handle = self.handles.get(&id).ok_or(Error::new(EBADF))?;
                let socket = match handle {
                    Handle::Socket(socket) | Handle::Entrypoint(socket) => socket,
                    _ => return Err(Error::new(EBADF)),
                };
                let Some(token) = socket.borrow().issued_token() else {
                    return Err(Error::new(EINVAL));
                };
                let token_bytes = token.to_le_bytes();
                let token_bytes_len = token_bytes.len();
                if payload.len() < token_bytes_len {
                    eprintln!("handle_get_token(id: {id}): Payload buffer is too small for token.",);
                    return Err(Error::new(ENOBUFS));
                }
                payload[..token_bytes_len].copy_from_slice(&token_bytes);
                return Ok(token_bytes_len);
            }
            _ => {}
        }
        let socket = self.get_socket(id)?;
        match verb {
            SocketCall::Bind => S::handle_bind(self, id, &payload),
            SocketCall::Connect => {
                let token = read_num::<u64>(payload)?;

                let target_rc = self
                    .socket_tokens
                    .get(&token)
                    .ok_or(Error::new(ECONNREFUSED))?;
                let target_socket_token = target_rc
                    .borrow()
                    .issued_token()
                    .ok_or(Error::new(ECONNREFUSED))?;
                if target_socket_token != token {
                    return Err(Error::new(EACCES));
                }

                S::handle_connect(self, self.get_socket(id)?, target_rc)
            }
            SocketCall::SetSockOpt => socket
                .borrow_mut()
                .handle_setsockopt(*metadata.get(1).ok_or(Error::new(EINVAL))? as i32, &payload),
            SocketCall::GetSockOpt => S::handle_getsockopt(
                self,
                socket,
                *metadata.get(1).ok_or(Error::new(EINVAL))? as i32,
                payload,
            ),
            SocketCall::SendMsg => S::handle_sendmsg(
                self,
                socket,
                metadata
                    .get(1)
                    .map(|x| MsgFlags(*x as _))
                    .unwrap_or_default(),
                payload,
                ctx,
            ),
            SocketCall::RecvMsg => S::handle_recvmsg(
                self,
                socket,
                metadata
                    .get(1)
                    .map(|x| MsgFlags(*x as _))
                    .unwrap_or_default(),
                payload,
            ),
            SocketCall::Unbind => socket.borrow_mut().handle_unbind(),
            SocketCall::GetToken => unreachable!("handled above"),
            SocketCall::GetPeerName => S::handle_get_peer_name(self, socket, payload),
            _ => Err(Error::new(EOPNOTSUPP)),
        }
    }

    fn dup(&mut self, id: usize, buf: &[u8], ctx: &CallerCtx) -> Result<OpenResult> {
        match buf {
            // Connect for socket pair
            b"listen" => S::handle_listen(self, id, ctx),
            b"connect" => S::handle_connect_socketpair(self, id, ctx),
            // listen will generate a id for same socket
            b"recvfd" => self.get_socket(id)?.borrow_mut().handle_recvfd(),
            b"entrypoint" => {
                let socket = self.get_socket(id)?;
                let id = self.next_id;
                self.handles.insert(id, Handle::Entrypoint(socket.clone()));
                self.next_id += 1;
                Ok(OpenResult::ThisScheme {
                    number: id,
                    flags: NewFdFlags::empty(),
                })
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        _offset: u64,
        _flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        S::write_inner(self, self.get_socket(id)?, buf, ctx)
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        match self.handles.get(&id).ok_or(Error::new(EBADF))? {
            Handle::SchemeRoot => Ok(self.fpath_inner(&String::new(), buf)?),
            Handle::Socket(socket_rc) | Handle::Entrypoint(socket_rc) => {
                let socket = socket_rc.borrow();
                let path = socket.path().unwrap_or("");
                Ok(self.fpath_inner(path, buf)?)
            }
        }
    }

    fn fsync(&mut self, id: usize, _ctx: &CallerCtx) -> Result<()> {
        self.get_socket(id).and(Ok(()))
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        _offset: u64,
        flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        self.get_socket(id)?.borrow_mut().read_inner(buf, flags)
    }

    fn on_close(&mut self, id: usize) {
        let Some(Handle::Socket(socket_rc)) = self.handles.remove(&id) else {
            return;
        };

        S::handle_closure(self, id, socket_rc);
    }

    fn on_sendfd(&mut self, sendfd_request: &SendFdRequest) -> Result<usize> {
        S::sendfd_inner(self, self.get_socket(sendfd_request.id())?, sendfd_request)
    }

    fn on_recvfd(&mut self, recvfd_request: &RecvFdRequest) -> Result<OpenResult> {
        S::recvfd_inner(self, self.get_socket(recvfd_request.id())?, recvfd_request)
    }

    fn fcntl(&mut self, id: usize, cmd: usize, arg: usize, _ctx: &CallerCtx) -> Result<usize> {
        let socket_rc = self.get_socket(id)?;
        let mut socket = socket_rc.borrow_mut();
        match cmd {
            F_GETFL => Ok(socket.get_flags()),
            F_SETFL => {
                socket.set_flags(arg);
                Ok(0)
            }
            _ => {
                eprintln!("fcntl(id: {}): Unsupported cmd: {}", id, cmd);
                Err(Error::new(EINVAL))
            }
        }
    }

    fn fevent(&mut self, id: usize, flags: EventFlags, _ctx: &CallerCtx) -> Result<EventFlags> {
        let socket_rc = self.get_socket(id)?;
        let socket = socket_rc.borrow();
        Ok(socket.events() & flags)
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        match self.handles.get(&id).ok_or(Error::new(EBADF))? {
            Handle::Socket(_) | Handle::Entrypoint(_) => {}
            _ => return Err(Error::new(EINVAL)),
        };

        *stat = Stat {
            st_mode: MODE_SOCK,
            ..Default::default()
        };

        Ok(())
    }
}
