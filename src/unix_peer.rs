extern crate tokio_uds;

#[cfg(any(feature = "workaround1", feature = "seqpacket"))]
extern crate libc;

use futures;
use futures::stream::Stream;
use std;
use std::io::Result as IoResult;
use std::io::{Read, Write};
use tokio_core::reactor::Handle;
use tokio_io::{AsyncRead, AsyncWrite};

use std::cell::RefCell;
use std::rc::Rc;

use std::path::{Path, PathBuf};

use self::tokio_uds::{UnixDatagram, UnixListener, UnixStream};

#[allow(unused)]
use super::simple_err;
use super::{box_up_err, peer_err_s, BoxedNewPeerFuture, BoxedNewPeerStream, Peer};
use super::{multi, once, ConstructParams, Options, PeerConstructor, Specifier};

#[derive(Debug, Clone)]
pub struct UnixConnect(pub PathBuf);
impl Specifier for UnixConnect {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        once(unix_connect_peer(&p.tokio_handle, &self.0))
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec typ=Other);
}
specifier_class!(
    name = UnixConnectClass,
    target = UnixConnect,
    prefixes = [
        "unix:",
        "unix-connect:",
        "connect-unix:",
        "unix-c:",
        "c-unix:"
    ],
    arg_handling = into,
    help = r#"
Connect to UNIX socket. Argument is filesystem path.

Example: forward connections from websockets to a UNIX stream socket

    websocat ws-l:127.0.0.1:8088 unix:the_socket
"#
);

#[derive(Debug, Clone)]
pub struct UnixListen(pub PathBuf);
impl Specifier for UnixListen {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        multi(unix_listen_peer(
            &p.tokio_handle,
            &self.0,
            p.program_options,
        ))
    }
    specifier_boilerplate!(noglobalstate multiconnect no_subspec typ=Other);
}
specifier_class!(
    name = UnixListenClass,
    target = UnixListen,
    prefixes = ["unix-listen:", "listen-unix:", "unix-l:", "l-unix:"],
    arg_handling = into,
    help = r#"
Listen for connections on a specified UNIX socket

Example: forward connections from a UNIX socket to a WebSocket

    websocat --unlink unix-l:the_socket ws://127.0.0.1:8089
    
Example: Accept forwarded WebSocket connections from Nginx

    umask 0000
    websocat --unlink ws-l:unix-l:/tmp/wstest tcp:[::]:22
      
Nginx config:
    
    location /ws {
        proxy_read_timeout 7d;
        proxy_send_timeout 7d;
        #proxy_pass http://localhost:3012;
        proxy_pass http://unix:/tmp/wstest;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection \"upgrade\";
    }

This configuration allows to make Nginx responsible for
SSL and also it can choose which connections to forward
to websocat based on URLs.

Obviously, Nginx can also redirect to TCP-listening
websocat just as well - UNIX sockets are not a requirement for this feature.

TODO: --chmod option?
"#
);

#[derive(Debug, Clone)]
pub struct UnixDgram(pub PathBuf, pub PathBuf);
impl Specifier for UnixDgram {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        once(dgram_peer(
            &p.tokio_handle,
            &self.0,
            &self.1,
            p.program_options,
        ))
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec typ=Other);
}
specifier_class!(
    name = UnixDgramClass,
    target = UnixDgram,
    prefixes = ["unix-dgram:"],
    arg_handling = {
        fn construct(
            self: &UnixDgramClass,
            _full: &str,
            just_arg: &str,
        ) -> super::Result<Rc<Specifier>> {
            let splits: Vec<&str> = just_arg.split(":").collect();
            if splits.len() != 2 {
                Err("Expected two colon-separted paths")?;
            }
            Ok(Rc::new(UnixDgram(splits[0].into(), splits[1].into())))
        }
    },
    help = r#"
Send packets to one path, receive from the other.
A socket for sending must be already openend.

I don't know if this mode has any use, it is here just for completeness.

Example:

    socat unix-recv:./sender -&
    websocat - unix-dgram:./receiver:./sender
"#
);

fn to_abstract(x: &str) -> PathBuf {
    format!("\x00{}", x).into()
}

#[derive(Debug, Clone)]
pub struct AbstractConnect(pub String);
impl Specifier for AbstractConnect {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        once(unix_connect_peer(&p.tokio_handle, &to_abstract(&self.0)))
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec typ=Other);
}
specifier_class!(
    name = AbstractConnectClass,
    target = AbstractConnect,
    prefixes = [
        "abstract:",
        "abstract-connect:",
        "connect-abstract:",
        "abstract-c:",
        "c-abstract:"
    ],
    arg_handling = into,
    help = r#"
Connect to UNIX abstract-namespaced socket. Argument is some string used as address.

Too long addresses may be silently chopped off.

Example: forward connections from websockets to an abstract stream socket

    websocat ws-l:127.0.0.1:8088 abstract:the_socket

Note that abstract-namespaced Linux sockets may not be normally supported by Rust,
so non-prebuilt versions may have problems with them.
"#
);

#[derive(Debug, Clone)]
pub struct AbstractListen(pub String);
impl Specifier for AbstractListen {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        multi(unix_listen_peer(
            &p.tokio_handle,
            &to_abstract(&self.0),
            Rc::new(Default::default()),
        ))
    }
    specifier_boilerplate!(noglobalstate multiconnect no_subspec typ=Other);
}
specifier_class!(
    name = AbstractListenClass,
    target = AbstractListen,
    prefixes = [
        "abstract-listen:",
        "listen-abstract:",
        "abstract-l:",
        "l-abstract:"
    ],
    arg_handling = into,
    help = r#"
Listen for connections on a specified abstract UNIX socket

Example: forward connections from an abstract UNIX socket to a WebSocket

    websocat abstract-l:the_socket ws://127.0.0.1:8089

Note that abstract-namespaced Linux sockets may not be normally supported by Rust,
so non-prebuilt versions may have problems with them.
"#
);

#[derive(Debug, Clone)]
pub struct AbstractDgram(pub String, pub String);
impl Specifier for AbstractDgram {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        #[cfg(not(feature = "workaround1"))]
        {
            once(dgram_peer(
                &p.tokio_handle,
                &to_abstract(&self.0),
                &to_abstract(&self.1),
                p.program_options,
            ))
        }
        #[cfg(feature = "workaround1")]
        {
            once(dgram_peer_workaround(
                &p.tokio_handle,
                &to_abstract(&self.0),
                &to_abstract(&self.1),
                p.program_options,
            ))
        }
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec typ=Other);
}
specifier_class!(
    name = AbstractDgramClass,
    target = AbstractDgram,
    prefixes = ["abstract-dgram:"],
    arg_handling = {
        fn construct(
            self: &AbstractDgramClass,
            _full: &str,
            just_arg: &str,
        ) -> super::Result<Rc<Specifier>> {
            let splits: Vec<&str> = just_arg.split(":").collect();
            if splits.len() != 2 {
                Err("Expected two colon-separted addresses")?;
            }
            Ok(Rc::new(UnixDgram(splits[0].into(), splits[1].into())))
        }
    },
    help = r#"
Send packets to one address, receive from the other.
A socket for sending must be already openend.

I don't know if this mode has any use, it is here just for completeness.

Example (untested):

    websocat - abstract-dgram:receiver_addr:sender_addr

Note that abstract-namespaced Linux sockets may not be normally supported by Rust,
so non-prebuilt versions may have problems with them. In particular, this mode
may fail to work without `workaround1` Cargo feature.
"#
);

#[cfg(feature = "seqpacket")]
#[derive(Debug, Clone)]
pub struct SeqpacketConnect(pub PathBuf);
#[cfg(feature = "seqpacket")]
impl Specifier for SeqpacketConnect {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        once(seqpacket_connect_peer(&p.tokio_handle, &self.0))
    }
    specifier_boilerplate!(noglobalstate singleconnect no_subspec typ=Other);
}
#[cfg(feature = "seqpacket")]
specifier_class!(
    name = SeqpacketConnectClass,
    target = SeqpacketConnect,
    prefixes = [
        "seqpacket:",
        "seqpacket-connect:",
        "connect-seqpacket:",
        "seqpacket-c:",
        "c-seqpacket:"
    ],
    arg_handling = into,
    help = r#"
Connect to AF_UNIX SOCK_SEQPACKET socket. Argument is a filesystem path.

Start the path with `@` character to make it connect to abstract-namespaced socket instead.

Too long paths are silently truncated.

Example: forward connections from websockets to a UNIX seqpacket abstract socket

    websocat ws-l:127.0.0.1:1234 seqpacket:@test
"#
);

#[cfg(feature = "seqpacket")]
#[derive(Debug, Clone)]
pub struct SeqpacketListen(pub PathBuf);
#[cfg(feature = "seqpacket")]
impl Specifier for SeqpacketListen {
    fn construct(&self, p: ConstructParams) -> PeerConstructor {
        multi(seqpacket_listen_peer(&p.tokio_handle, &self.0, p.program_options))
    }
    specifier_boilerplate!(noglobalstate multiconnect no_subspec typ=Other);
}
#[cfg(feature = "seqpacket")]
specifier_class!(
    name = SeqpacketListenClass,
    target = SeqpacketListen,
    prefixes = [
        "seqpacket-listen:",
        "listen-seqpacket:",
        "seqpacket-l:",
        "l-seqpacket:"
    ],
    arg_handling = into,
    help = r#"
Listen for connections on a specified AF_UNIX SOCK_SEQPACKET socket

Start the path with `@` character to make it connect to abstract-namespaced socket instead.

Too long (>=108 bytes) paths are silently truncated.

Example: forward connections from a UNIX seqpacket socket to a WebSocket

    websocat --unlink seqpacket-l:the_socket ws://127.0.0.1:8089
"#
);

// based on https://github.com/tokio-rs/tokio-core/blob/master/examples/proxy.rs
#[derive(Clone)]
struct MyUnixStream(Rc<UnixStream>, bool);

impl Read for MyUnixStream {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        (&*self.0).read(buf)
    }
}

impl Write for MyUnixStream {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl AsyncRead for MyUnixStream {}

impl AsyncWrite for MyUnixStream {
    fn shutdown(&mut self) -> futures::Poll<(), std::io::Error> {
        try!(self.0.shutdown(std::net::Shutdown::Write));
        Ok(().into())
    }
}

impl Drop for MyUnixStream {
    fn drop(&mut self) {
        let i_am_read_part = self.1;
        if i_am_read_part {
            let _ = self.0.shutdown(std::net::Shutdown::Read);
        }
    }
}

pub fn unix_connect_peer(handle: &Handle, addr: &Path) -> BoxedNewPeerFuture {
    Box::new(futures::future::result(
        UnixStream::connect(&addr, handle)
            .map(|x| {
                info!("Connected to a unix socket");
                let x = Rc::new(x);
                Peer::new(
                    MyUnixStream(x.clone(), true),
                    MyUnixStream(x.clone(), false),
                )
            })
            .map_err(box_up_err),
    )) as BoxedNewPeerFuture
}

pub fn unix_listen_peer(handle: &Handle, addr: &Path, opts: Rc<Options>) -> BoxedNewPeerStream {
    if opts.unlink_unix_socket {
        let _ = ::std::fs::remove_file(addr);
    };
    let bound = match UnixListener::bind(&addr, handle) {
        Ok(x) => x,
        Err(e) => return peer_err_s(e),
    };
    // TODO: chmod
    Box::new(
        bound
            .incoming()
            .map(|(x, _addr)| {
                info!("Incoming unix socket connection");
                let x = Rc::new(x);
                Peer::new(
                    MyUnixStream(x.clone(), true),
                    MyUnixStream(x.clone(), false),
                )
            })
            .map_err(|e| box_up_err(e)),
    ) as BoxedNewPeerStream
}

struct DgramPeer {
    s: UnixDatagram,
    #[allow(unused)]
    oneshot_mode: bool, // TODO
}

#[derive(Clone)]
struct DgramPeerHandle(Rc<RefCell<DgramPeer>>);

pub fn dgram_peer(
    handle: &Handle,
    bindaddr: &Path,
    connectaddr: &Path,
    opts: Rc<Options>,
) -> BoxedNewPeerFuture {
    Box::new(futures::future::result(
        UnixDatagram::bind(bindaddr, handle)
            .and_then(|x| {
                x.connect(connectaddr)?;

                let h1 = DgramPeerHandle(Rc::new(RefCell::new(DgramPeer {
                    s: x,
                    oneshot_mode: opts.udp_oneshot_mode,
                })));
                let h2 = h1.clone();
                Ok(Peer::new(h1, h2))
            })
            .map_err(box_up_err),
    )) as BoxedNewPeerFuture
}

#[cfg(feature = "workaround1")]
pub fn dgram_peer_workaround(
    handle: &Handle,
    bindaddr: &Path,
    connectaddr: &Path,
    opts: Rc<Options>,
) -> BoxedNewPeerFuture {
    info!("Workaround method for getting abstract datagram socket");
    fn getfd(bindaddr: &Path, connectaddr: &Path) -> Option<i32> {
        use self::libc::{
            bind, c_char, close, connect, sa_family_t, sockaddr_un, socket, socklen_t, AF_UNIX,
            SOCK_DGRAM,
        };
        use std::mem::{size_of, transmute};
        use std::os::unix::ffi::OsStrExt;
        unsafe {
            let s = socket(AF_UNIX, SOCK_DGRAM, 0);
            if s == -1 {
                return None;
            }
            {
                let mut sa = sockaddr_un {
                    sun_family: AF_UNIX as sa_family_t,
                    sun_path: [0; 108],
                };
                let bp: &[c_char] = transmute(bindaddr.as_os_str().as_bytes());
                let l = 108.min(bp.len());
                sa.sun_path[..l].copy_from_slice(&bp[..l]);
                let sa_len = l + size_of::<sa_family_t>();
                let ret = bind(s, transmute(&sa), sa_len as socklen_t);
                if ret == -1 {
                    close(s);
                    return None;
                }
            }
            {
                let mut sa = sockaddr_un {
                    sun_family: AF_UNIX as sa_family_t,
                    sun_path: [0; 108],
                };
                let bp: &[c_char] = transmute(connectaddr.as_os_str().as_bytes());
                let l = 108.min(bp.len());
                sa.sun_path[..l].copy_from_slice(&bp[..l]);
                let sa_len = l + size_of::<sa_family_t>();
                let ret = connect(s, transmute(&sa), sa_len as socklen_t);
                if ret == -1 {
                    close(s);
                    return None;
                }
            }
            Some(s)
        }
    }
    fn getpeer(
        handle: &Handle,
        bindaddr: &Path,
        connectaddr: &Path,
        opts: Rc<Options>,
    ) -> Result<Peer, Box<::std::error::Error>> {
        if let Some(fd) = getfd(bindaddr, connectaddr) {
            let s: ::std::os::unix::net::UnixDatagram =
                unsafe { ::std::os::unix::io::FromRawFd::from_raw_fd(fd) };
            let ss = UnixDatagram::from_datagram(s, handle)?;
            let h1 = DgramPeerHandle(Rc::new(RefCell::new(DgramPeer {
                s: ss,
                oneshot_mode: opts.udp_oneshot_mode,
            })));
            let h2 = h1.clone();
            Ok(Peer::new(h1, h2))
        } else {
            Err("Failed to get, bind or connect socket")?
        }
    }
    Box::new(futures::future::result({
        getpeer(handle, bindaddr, connectaddr, opts)
    })) as BoxedNewPeerFuture
}

impl Read for DgramPeerHandle {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let p = self.0.borrow_mut();
        p.s.recv(buf)
    }
}

impl Write for DgramPeerHandle {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let p = self.0.borrow_mut();
        p.s.send(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl AsyncRead for DgramPeerHandle {}

impl AsyncWrite for DgramPeerHandle {
    fn shutdown(&mut self) -> futures::Poll<(), std::io::Error> {
        Ok(().into())
    }
}

#[cfg(feature = "seqpacket")]
pub fn seqpacket_connect_peer(handle: &Handle, addr: &Path) -> BoxedNewPeerFuture {
    fn getfd(addr: &Path) -> Option<i32> {
        use self::libc::{
            c_char, close, connect, sa_family_t, sockaddr_un, socket, socklen_t, AF_UNIX,
            SOCK_SEQPACKET,
        };
        use std::mem::{size_of, transmute};
        use std::os::unix::ffi::OsStrExt;
        unsafe {
            let s = socket(AF_UNIX, SOCK_SEQPACKET, 0);
            if s == -1 {
                return None;
            }
            {
                let mut sa = sockaddr_un {
                    sun_family: AF_UNIX as sa_family_t,
                    sun_path: [0; 108],
                };
                let bp: &[c_char] = transmute(addr.as_os_str().as_bytes());
                let l = 108.min(bp.len());
                sa.sun_path[..l].copy_from_slice(&bp[..l]);
                if sa.sun_path[0] == b'@' as c_char {
                    sa.sun_path[0] = b'\x00' as c_char;
                }
                let sa_len = l + size_of::<sa_family_t>();
                let ret = connect(s, transmute(&sa), sa_len as socklen_t);
                if ret == -1 {
                    close(s);
                    return None;
                }
            }
            Some(s)
        }
    }
    fn getpeer(handle: &Handle, addr: &Path) -> Result<Peer, Box<::std::error::Error>> {
        if let Some(fd) = getfd(addr) {
            let s: ::std::os::unix::net::UnixStream =
                unsafe { ::std::os::unix::io::FromRawFd::from_raw_fd(fd) };
            let ss = UnixStream::from_stream(s, handle)?;
            let x = Rc::new(ss);
            Ok(Peer::new(
                MyUnixStream(x.clone(), true),
                MyUnixStream(x.clone(), false),
            ))
        } else {
            Err("Failed to get or connect socket")?
        }
    }
    Box::new(futures::future::result({ getpeer(handle, addr) })) as BoxedNewPeerFuture
}

#[cfg(feature = "seqpacket")]
pub fn seqpacket_listen_peer(
    handle: &Handle,
    addr: &Path,
    opts: Rc<Options>,
) -> BoxedNewPeerStream {
    fn getfd(addr: &Path, opts: Rc<Options>) -> Option<i32> {
        use self::libc::{
            bind, c_char, close, listen, sa_family_t, sockaddr_un, socket, socklen_t, unlink,
            AF_UNIX, SOCK_SEQPACKET,
        };
        use std::mem::{size_of, transmute};
        use std::os::unix::ffi::OsStrExt;
        unsafe {
            let s = socket(AF_UNIX, SOCK_SEQPACKET, 0);
            if s == -1 {
                return None;
            }
            {
                let mut sa = sockaddr_un {
                    sun_family: AF_UNIX as sa_family_t,
                    sun_path: [0; 108],
                };
                let bp: &[c_char] = transmute(addr.as_os_str().as_bytes());

                let l = 108.min(bp.len());
                sa.sun_path[..l].copy_from_slice(&bp[..l]);
                if sa.sun_path[0] == b'@' as c_char {
                    sa.sun_path[0] = b'\x00' as c_char;
                } else {
                    if opts.unlink_unix_socket {
                        sa.sun_path[107] = 0;
                        unlink(&sa.sun_path as *const c_char);
                    }
                }
                let sa_len = l + size_of::<sa_family_t>();
                let ret = bind(s, transmute(&sa), sa_len as socklen_t);
                if ret == -1 {
                    close(s);
                    return None;
                }
            }
            {
                let ret = listen(s, 50);
                if ret == -1 {
                    close(s);
                    return None;
                }
            }
            Some(s)
        }
    }
    let fd = match getfd(addr, opts) {
        Some(x) => x,
        None => return peer_err_s(simple_err("Failed to get or bind socket".into())),
    };
    let l1: ::std::os::unix::net::UnixListener =
        unsafe { ::std::os::unix::io::FromRawFd::from_raw_fd(fd) };
    let bound = match UnixListener::from_listener(l1, handle) {
        Ok(x) => x,
        Err(e) => return peer_err_s(e),
    };
    Box::new(
        bound
            .incoming()
            .map(|(x, _addr)| {
                info!("Incoming unix socket connection");
                let x = Rc::new(x);
                Peer::new(
                    MyUnixStream(x.clone(), true),
                    MyUnixStream(x.clone(), false),
                )
            })
            .map_err(|e| box_up_err(e)),
    ) as BoxedNewPeerStream
}
