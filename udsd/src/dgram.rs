//! uds scheme for handling Unix Domain Socket datagram communication

use std::cell::RefCell;
use std::cmp;
use std::collections::{BTreeSet, VecDeque};
use std::convert::TryInto;
use std::rc::Rc;

use libc::{AF_UNIX, SO_DOMAIN, SO_PASSCRED};
use libredox::Fd;
use redox_scheme::{CallerCtx, OpenResult, RecvFdRequest, SendFdRequest};
use syscall::error::*;
use syscall::flag::*;
use syscall::schemev2::NewFdFlags;
use syscall::{Error, FobtainFdFlags};

use crate::common::{
    get_uid_gid_from_pid, path_buf_to_str, read_msghdr_info, read_num, AncillaryData, Credential,
    DataPacket, MsgWriter, MAX_DGRAM_MSG_LEN, SOCK_MIN_SNDBUF,
};
use crate::scheme::{MsgFlags, UdsScheme};

pub type UdsDgramScheme<'sock> = UdsScheme<'sock, Socket>;

#[derive(Debug, Default)]
pub struct Socket {
    primary_id: usize,
    path: Option<String>,
    state: State,
    peer: Option<usize>,
    messages: VecDeque<DataPacket>,
    options: BTreeSet<i32>,
    fds: VecDeque<usize>,
    flags: usize,
    snd_buf_size: usize,
}

impl Socket {
    fn events_inner(&self) -> EventFlags {
        let mut ready = EventFlags::empty();
        if !self.messages.is_empty() {
            ready |= EVENT_READ;
        }
        if self.peer.is_some() {
            ready |= EVENT_WRITE;
        }
        ready
    }

    fn drop_fds(&mut self, num_fd: usize) -> Result<()> {
        for i in 0..num_fd {
            if self.fds.pop_front().is_none() {
                eprintln!("Socket::drop_fds: Attempted to drop FD #{} of {}, but fd queue is empty. State inconsistency.", i + 1, num_fd);
                return Err(Error::new(EINVAL));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Unbound,
    Bound,
    Closed,
}

impl Default for State {
    fn default() -> Self {
        Self::Unbound
    }
}

impl DataPacket {
    pub fn serialize_to_stream(
        self,
        scheme: &UdsScheme<Socket>,
        stream: &mut [u8],
        socket: &mut Socket,
        name_buf_size: usize,
        iov_size: usize,
    ) -> Result<usize> {
        let mut msg_writer = MsgWriter::new(stream);
        msg_writer.write_name(self.ancillary_data.name, name_buf_size, |path, buf| {
            scheme.fpath_inner(path, buf)
        })?;

        msg_writer.write_payload(&self.payload, self.payload.len(), iov_size)?;

        // Write the ancillary data
        if !msg_writer.write_rights(self.ancillary_data.num_fds) {
            // Buffer was too small, FDs could not be described. Drop the actual FDs.
            eprintln!(
                "serialize_to_stream: Buffer too small for SCM_RIGHTS, dropping {} FDs.",
                self.ancillary_data.num_fds
            );
            socket.drop_fds(self.ancillary_data.num_fds)?;
        }
        // Write other ancillary datas
        for option in &socket.options {
            let result = match *option {
                SO_PASSCRED => msg_writer.write_credentials(&self.ancillary_data.cred),
                _ => {
                    eprintln!(
                        "serialize_to_stream: Unsupported socket option for serialization: {}",
                        option
                    );
                    return Err(Error::new(EOPNOTSUPP));
                }
            };
            if !result {
                eprintln!("serialize_to_stream: Buffer too small for ancillary data, stopping further serialization.");
                break;
            }
        }

        Ok(msg_writer.len())
    }
}

impl Socket {
    fn get_connected_peer(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
    ) -> Result<(usize, Rc<RefCell<Socket>>), Error> {
        let socket = socket.borrow();

        let remote_id = socket.peer.ok_or(Error::new(ENOTCONN))?;

        let remote_rc = scheme.get_socket(remote_id).map_err(|e| {
            eprintln!(
                "get_connected_peer(id: {}): Peer socket (id: {}) has vanished. Original error: {:?}",
                socket.primary_id, remote_id, e
            );
            Error::new(EPIPE)
        })?;

        if remote_rc.borrow().state == State::Closed {
            eprintln!(
                "get_connected_peer(id: {}): Attempted to interact with a closed peer (id: {}).",
                socket.primary_id, remote_id
            );
            return Err(Error::new(ECONNREFUSED));
        }

        Ok((remote_id, remote_rc.clone()))
    }

    fn sendmsg_inner(
        cap_fd: &Fd,
        socket: &mut Socket,
        name: Option<String>,
        msg_stream: &[u8],
        ctx: &CallerCtx,
    ) -> Result<usize> {
        if msg_stream.is_empty() {
            eprintln!("sendmsg_inner: msg_stream is empty.");
            return Err(Error::new(EINVAL));
        }

        let (pid, uid, gid) = get_uid_gid_from_pid(cap_fd, ctx.pid)?;
        let message = DataPacket::from_stream(
            msg_stream,
            name,
            Credential::new(pid as i32, uid as i32, gid as i32),
        )?;
        let payload_len = message.len();

        if payload_len > socket.snd_buf_size {
            eprintln!("sendmsg_inner: msg_stream is longer than SO_SNDBUF.");
        }

        let space_left = socket
            .messages
            .iter()
            .try_fold(socket.snd_buf_size, |acc, msg| acc.checked_sub(msg.len()))
            .unwrap_or_default();

        if space_left == 0 {
            return if (socket.flags as usize) & O_NONBLOCK == O_NONBLOCK {
                Err(Error::new(EAGAIN))
            } else {
                Err(Error::new(EWOULDBLOCK))
            };
        }

        socket.messages.push_back(message);

        Ok(payload_len)
    }

    fn recvmsg_inner(
        scheme: &UdsScheme<Self>,
        socket: &mut Socket,
        message: DataPacket,
        msg_stream: &mut [u8],
    ) -> Result<usize> {
        // Read the name length, whole iov size, and msg controllen from the stream
        let (prepared_name_len, prepared_whole_iov_size, _) = read_msghdr_info(msg_stream)?;

        message.serialize_to_stream(
            scheme,
            msg_stream,
            socket,
            prepared_name_len,
            prepared_whole_iov_size,
        )
    }
}

impl super::scheme::Socket for Socket {
    fn handle_unnamed_socket(
        scheme: &mut UdsScheme<Self>,
        flags: usize,
        _ctx: &CallerCtx,
    ) -> usize {
        let new_id = scheme.next_id;
        let mut new = Socket::default();
        new.flags = flags;
        new.primary_id = new_id;
        // FIXME: Use wmem_default when it's available.
        new.snd_buf_size = usize::MAX;

        scheme.insert_socket(new_id, Rc::new(RefCell::new(new)));
        scheme.next_id += 1;
        new_id
    }

    fn handle_bind(scheme: &mut UdsScheme<Self>, id: usize, path_buf: &[u8]) -> Result<usize> {
        let path = path_buf_to_str(path_buf)?;

        let socket_rc = scheme.get_socket(id)?.clone();
        let path_owned: String;
        {
            let mut socket = socket_rc.borrow_mut();

            if socket.state != State::Unbound {
                eprintln!(
                    "handle_bind(id: {}): Socket is already bound or connected (state: {:?})",
                    id, socket.state
                );
                return Err(Error::new(EINVAL));
            }

            path_owned = path.to_string();
            socket.path = Some(path_owned.clone());
            socket.state = State::Bound;
        }

        Ok(0)
    }

    fn handle_connect(
        _scheme: &UdsScheme<Self>,
        socket_rc: &RefCell<Self>,
        target_rc: &RefCell<Self>,
    ) -> Result<usize> {
        let target_id = target_rc.borrow().primary_id;
        socket_rc.borrow_mut().peer = Some(target_id);
        Ok(0)
    }

    fn handle_setsockopt(&mut self, option: i32, value_slice: &[u8]) -> Result<usize> {
        match option {
            SO_PASSCRED => {
                let value = read_num::<i32>(value_slice)?;
                if value != 0 {
                    self.options.insert(SO_PASSCRED);
                } else {
                    self.options.remove(&SO_PASSCRED);
                }
                Ok(value_slice.len())
            }
            libc::SO_SNDBUF => {
                let value = read_num::<i32>(value_slice)?;
                let value = value.try_into().unwrap_or(usize::MAX);
                // TODO: Select min between the value and wmem_max when it's there.
                let value = cmp::min(value, SOCK_MIN_SNDBUF);
                self.snd_buf_size = value;
                Ok(0)
            }
            _ => {
                eprintln!(
                    "handle_setsockopt(id: {}): Unsupported option: {}",
                    self.primary_id, option
                );
                Err(Error::new(ENOPROTOOPT))
            }
        }
    }

    fn handle_getsockopt(
        _scheme: &UdsScheme<Self>,
        socket_rc: &RefCell<Self>,
        option: i32,
        payload: &mut [u8],
    ) -> Result<usize> {
        match option {
            SO_DOMAIN => {
                payload.fill(0);
                if payload.len() < size_of::<i32>() {
                    eprintln!(
                        "handle_getsockopt(id: {}): SO_DOMAIN payload buffer is too small. len: {}",
                        socket_rc.borrow().primary_id,
                        payload.len()
                    );
                    return Err(Error::new(ENOBUFS));
                }
                let domain = AF_UNIX.to_le_bytes();
                payload[..domain.len()].copy_from_slice(&domain);
                Ok(domain.len())
            }
            libc::SO_SNDBUF => {
                payload.fill(0);
                if payload.len() < size_of::<i32>() {
                    eprintln!(
                        "handle_getsockopt(id: {}): SO_SNDBUF payload buffer is too small. len: {}",
                        socket_rc.borrow().primary_id,
                        payload.len()
                    );
                    return Err(Error::new(ENOBUFS));
                }

                let socket = socket_rc.borrow();
                let sndbuf: i32 = socket.snd_buf_size.try_into().unwrap_or_else(|_| {
                    eprintln!(
                        "handle_getsockopt(id: {}): SO_SNDBUF value overflows the payload buffer.",
                        socket.primary_id,
                    );
                    return i32::MAX;
                });
                let sndbuf = sndbuf.to_le_bytes();
                payload[..sndbuf.len()].copy_from_slice(&sndbuf);
                Ok(0)
            }
            _ => {
                eprintln!(
                    "handle_getsockopt(id: {}): Unsupported option: {}",
                    socket_rc.borrow().primary_id,
                    option
                );
                Err(Error::new(ENOPROTOOPT))
            }
        }
    }

    fn handle_sendmsg(
        scheme: &UdsScheme<Self>,
        socket_rc: &RefCell<Self>,
        _msg_flags: MsgFlags,
        msg_stream: &[u8],
        ctx: &CallerCtx,
    ) -> Result<usize> {
        if msg_stream.is_empty() {
            eprintln!(
                "handle_sendmsg(id: {}): msg_stream is empty.",
                socket_rc.borrow().primary_id
            );
            return Err(Error::new(EINVAL));
        }

        let name = socket_rc.borrow().path.clone();
        let (remote_id, remote_rc) = Self::get_connected_peer(scheme, socket_rc)?;

        let bytes_written = Self::sendmsg_inner(
            &scheme.proc_creds_capability,
            &mut remote_rc.borrow_mut(),
            name,
            msg_stream,
            ctx,
        )?;
        scheme.post_fevent(remote_id, EVENT_READ)?;
        Ok(bytes_written)
    }

    fn handle_recvmsg(
        scheme: &UdsScheme<Self>,
        socket_rc: &RefCell<Self>,
        _msg_flags: MsgFlags,
        msg_stream: &mut [u8],
    ) -> Result<usize> {
        let mut socket = socket_rc.borrow_mut();

        if let Some(message) = socket.messages.pop_front() {
            Ok(Self::recvmsg_inner(
                scheme,
                &mut socket,
                message,
                msg_stream,
            )?)
        } else if (socket.flags as usize) & O_NONBLOCK == O_NONBLOCK {
            Err(Error::new(EAGAIN))
        } else {
            Err(Error::new(EWOULDBLOCK))
        }
    }

    fn handle_unbind(&mut self) -> Result<usize> {
        if self.state != State::Bound {
            return Err(Error::new(EINVAL));
        }

        self.state = State::Unbound;
        self.path = None;

        Ok(0)
    }

    fn handle_get_peer_name(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        payload: &mut [u8],
    ) -> Result<usize> {
        let (_, peer_socket) = Self::get_connected_peer(scheme, socket)?;
        let peer_borrow = peer_socket.borrow();
        match peer_borrow.path.as_ref() {
            Some(path_string) => scheme.fpath_inner(path_string, payload),
            None => {
                let empty_path = "".to_string();
                scheme.fpath_inner(&empty_path, payload)
            }
        }
    }

    fn handle_listen(
        scheme: &mut UdsScheme<Self>,
        id: usize,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        let socket_rc = scheme.get_socket(id)?;

        let new_id = scheme.next_id;

        scheme.insert_socket(new_id, socket_rc.clone());
        scheme.next_id += 1;

        Ok(OpenResult::ThisScheme {
            number: new_id,
            flags: NewFdFlags::empty(),
        })
    }

    fn handle_connect_socketpair(
        scheme: &mut UdsScheme<Self>,
        id: usize,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        let new_id = scheme.next_id;
        let mut new = Socket::default();
        new.primary_id = new_id;

        let socket_rc = scheme.get_socket(id)?;
        if socket_rc.borrow().state == State::Closed {
            eprintln!(
                "handle_connect_socketpair(id: {}): Attempting to connect from a closed socket.",
                id
            );
            return Err(Error::new(ECONNREFUSED));
        }

        {
            let mut socket = socket_rc.borrow_mut();
            socket.peer = Some(new_id);
        }

        new.peer = Some(id);

        // smoltcp sends writeable whenever a listener gets a
        // client, we'll do the same too (but also readable,
        // why not)
        scheme.post_fevent(id, EVENT_READ | EVENT_WRITE)?;

        scheme.insert_socket(new_id, Rc::new(RefCell::new(new)));

        scheme.next_id += 1;

        Ok(OpenResult::ThisScheme {
            number: new_id,
            flags: NewFdFlags::empty(),
        })
    }

    fn handle_recvfd(&mut self) -> Result<OpenResult> {
        let fd = self.fds.pop_front().ok_or(Error::new(EWOULDBLOCK))?;

        Ok(OpenResult::OtherScheme { fd })
    }

    fn write_inner(
        scheme: &UdsScheme<Self>,
        sender_rc: &RefCell<Self>,
        buf: &[u8],
        ctx: &CallerCtx,
    ) -> Result<usize> {
        if buf.len() > MAX_DGRAM_MSG_LEN {
            return Err(Error::new(EMSGSIZE));
        }

        let name = {
            let socket = sender_rc.borrow();
            if matches!(socket.state, State::Closed) {
                return Err(Error::new(EPIPE));
            }

            socket.path.clone()
        };

        // Assume writing to the connected socket if the given id is the primary id
        let (remote_id, remote_rc) = Self::get_connected_peer(scheme, sender_rc)?;
        let mut remote = remote_rc.borrow_mut();
        let message = DataPacket::new(
            buf.to_vec(),
            AncillaryData::new(
                Credential::new(ctx.pid as i32, ctx.uid as i32, ctx.gid as i32),
                name,
            ),
        );
        remote.messages.push_back(message);

        scheme.post_fevent(remote_id, EVENT_READ)?;

        Ok(buf.len())
    }

    fn sendfd_inner(
        scheme: &UdsScheme<Self>,
        socket: &RefCell<Self>,
        sendfd_request: &SendFdRequest,
    ) -> Result<usize> {
        if sendfd_request.num_fds() == 0 {
            return Ok(0);
        }
        let mut new_fds = Vec::new();
        new_fds.resize(sendfd_request.num_fds(), usize::MAX);
        if let Err(e) =
            sendfd_request.obtain_fd(&scheme.socket, FobtainFdFlags::UPPER_TBL, &mut new_fds)
        {
            eprintln!("sendfd_inner: obtain_fd failed with error: {:?}", e);
            return Err(e);
        }
        let (remote_id, remote_rc) = Self::get_connected_peer(scheme, socket)?;
        {
            let mut remote = remote_rc.borrow_mut();
            for new_fd in &new_fds {
                remote.fds.push_back(*new_fd);
            }
        }

        scheme.post_fevent(remote_id, EVENT_READ)?;
        Ok(new_fds.len())
    }

    fn recvfd_inner(
        scheme: &UdsScheme<Self>,
        socket_rc: &RefCell<Self>,
        recvfd_request: &RecvFdRequest,
    ) -> Result<OpenResult> {
        if recvfd_request.num_fds() == 0 {
            return Ok(OpenResult::OtherSchemeMultiple { num_fds: 0 });
        }

        let mut socket = socket_rc.borrow_mut();

        if socket.fds.len() < recvfd_request.num_fds() {
            return if (socket.flags as usize) & O_NONBLOCK == O_NONBLOCK {
                Ok(OpenResult::WouldBlock)
            } else {
                Err(Error::new(EWOULDBLOCK))
            };
        }

        let fds: Vec<usize> = socket.fds.drain(..recvfd_request.num_fds()).collect();
        if let Err(e) = recvfd_request.move_fd(&scheme.socket, FmoveFdFlags::empty(), &fds) {
            eprintln!("recvfd_inner: move_fd failed with error: {:?}", e);
            return Err(Error::new(EPROTO));
        }

        Ok(OpenResult::OtherSchemeMultiple {
            num_fds: recvfd_request.num_fds(),
        })
    }

    fn read_inner(&mut self, buf: &mut [u8], flags: u32) -> Result<usize> {
        if let Some(message) = self.messages.pop_front() {
            let full_len = message.len();
            let copy_len = cmp::min(buf.len(), full_len);
            buf[..copy_len].copy_from_slice(&message.payload[..copy_len]);

            Ok(copy_len)
        } else if (flags as usize) & O_NONBLOCK == O_NONBLOCK {
            Err(Error::new(EAGAIN))
        } else {
            Err(Error::new(EWOULDBLOCK))
        }
    }

    fn handle_closure(_scheme: &mut UdsScheme<Self>, id: usize, socket_rc: Rc<RefCell<Self>>) {
        let mut socket = socket_rc.borrow_mut();
        if socket.primary_id == id {
            socket.state = State::Closed;
            socket.peer = None;
            socket.path = None;
        }
    }

    fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn events(&self) -> EventFlags {
        self.events_inner()
    }

    fn get_flags(&self) -> usize {
        self.flags
    }

    fn set_flags(&mut self, flags: usize) {
        self.flags = flags;
    }
}
