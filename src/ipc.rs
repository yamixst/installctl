use nix::sys::socket::{
    recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub enum InitEvent {
    FuseFd,
    Ready,
    Finished {
        exit_code: i32,
        leftovers: Vec<Leftover>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub pid: i32,
    pub cmdline: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCommand {
    Kill,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: i32,
}

pub fn send_msg<T: Serialize>(
    sock: &UnixStream,
    msg: &T,
    fds: &[BorrowedFd],
) -> io::Result<()> {
    let payload = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = (payload.len() as u32).to_le_bytes();

    let raw_fds: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let cmsgs = if raw_fds.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(&raw_fds)]
    };

    let iov = [IoSlice::new(&len), IoSlice::new(&payload)];
    let sent = sendmsg::<()>(
        sock.as_raw_fd(),
        &iov,
        &cmsgs,
        MsgFlags::empty(),
        None,
    )?;

    let total = 4 + payload.len();
    if sent < total {
        use std::io::Write;
        if sent < 4 {
            let mut remaining = Vec::with_capacity(total - sent);
            remaining.extend_from_slice(&len[sent..]);
            remaining.extend_from_slice(&payload);
            (&*sock).write_all(&remaining)?;
        } else {
            let payload_sent = sent - 4;
            (&*sock).write_all(&payload[payload_sent..])?;
        }
    }

    Ok(())
}

pub fn recv_msg<T: DeserializeOwned>(sock: &UnixStream) -> io::Result<(T, Vec<OwnedFd>)> {
    let mut len_buf = [0u8; 4];
    let mut received_fds = Vec::new();
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 16]);
    let mut read_bytes = 0;

    while read_bytes < 4 {
        let mut iov = [IoSliceMut::new(&mut len_buf[read_bytes..])];
        let msg = recvmsg::<()>(
            sock.as_raw_fd(),
            &mut iov,
            if read_bytes == 0 {
                Some(&mut cmsg_buf)
            } else {
                None
            },
            MsgFlags::empty(),
        )?;
        if msg.bytes == 0 {
            if read_bytes == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"));
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated message length",
                ));
            }
        }
        if read_bytes == 0
            && let Ok(cmsgs) = msg.cmsgs()
        {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(fds) = c {
                    for fd in fds {
                        received_fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                    }
                }
            }
        }
        read_bytes += msg.bytes;
    }

    let payload_len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; payload_len];
    use std::io::Read;
    (&*sock).read_exact(&mut payload)?;

    let val: T = serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok((val, received_fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    #[test]
    fn ipc_roundtrip_with_fd() {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        let (pipe_r, pipe_w) = nix::unistd::pipe().unwrap();

        send_msg(&sock1, &InitEvent::FuseFd, &[pipe_w.as_fd()]).unwrap();
        drop(pipe_w);

        let (event, fds) = recv_msg::<InitEvent>(&sock2).unwrap();
        assert_eq!(event, InitEvent::FuseFd);
        assert_eq!(fds.len(), 1);

        let mut w_file = std::fs::File::from(fds.into_iter().next().unwrap());
        w_file.write_all(b"roundtrip payload").unwrap();
        drop(w_file);

        let mut r_file = std::fs::File::from(pipe_r);
        let mut buf = String::new();
        r_file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "roundtrip payload");
    }
}
