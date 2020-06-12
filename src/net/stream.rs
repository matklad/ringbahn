use std::io;
use std::future::Future;
use std::net::ToSocketAddrs;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::ready;
use futures_io::{AsyncRead, AsyncBufRead, AsyncWrite};

use crate::buf::Buffer;
use crate::drive::{Drive, ProvideBuffer};
use crate::drive::demo::DemoDriver;
use crate::{Ring, Submission};
use crate::event;

use super::socket;

pub struct TcpStream<D: Drive = DemoDriver<'static>> {
    ring: Ring<D>,
    buf: Buffer<D>,
    active: Op,
    fd: RawFd,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Op {
    Read,
    Write,
    Close,
    Nothing,
}

impl TcpStream {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> Connect {
        TcpStream::connect_on_driver(addr, DemoDriver::default())
    }
}

impl<D: Drive> TcpStream<D> {
    pub fn connect_on_driver<A: ToSocketAddrs>(addr: A, driver: D) -> Connect<D> {
        let (fd, addr) = match socket(addr) {
            Ok(fd)  => fd,
            Err(e)  => return Connect(Err(Some(e))),
        };
        Connect(Ok(Submission::new(event::Connect::new(fd, addr), driver)))
    }

    pub(super) fn from_fd(fd: RawFd, ring: Ring<D>) -> TcpStream<D> {
        TcpStream {
            buf: Buffer::new(),
            active: Op::Nothing,
            fd, ring,
        }
    }

    fn guard_op(self: Pin<&mut Self>, op: Op) {
        let this = unsafe { Pin::get_unchecked_mut(self) };
        if this.active != Op::Nothing && this.active != op {
            this.cancel();
        }
        this.active = op;
    }

    fn cancel(&mut self) {
        self.active = Op::Nothing;
        self.ring.cancel(self.buf.cancellation());
    }

    #[inline(always)]
    fn ring(self: Pin<&mut Self>) -> Pin<&mut Ring<D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.ring) }
    }

    #[inline(always)]
    fn buf(self: Pin<&mut Self>) -> Pin<&mut Buffer<D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.buf) }
    }

    #[inline(always)]
    fn split(self: Pin<&mut Self>) -> (Pin<&mut Ring<D>>, &mut Buffer<D>) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (Pin::new_unchecked(&mut this.ring), &mut this.buf)
        }
    }
}

pub struct Connect<D: Drive = DemoDriver<'static>>(
    Result<Submission<event::Connect, D>, Option<io::Error>>
);

impl<D: Drive> Future for Connect<D> {
    type Output = io::Result<TcpStream<D>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match &mut Pin::get_unchecked_mut(self).0 {
                Ok(future)  => {
                    let (connect, driver, result) = ready!(Pin::new_unchecked(future).poll(ctx));
                    result?;
                    Poll::Ready(Ok(TcpStream::from_fd(connect.fd, Ring::new(driver))))
                }
                Err(err)    => {
                    let err = err.take().expect("polled Connect future after completion");
                    Poll::Ready(Err(err))
                }
            }
        }
    }
}

impl<D: Drive> AsyncRead for TcpStream<D> {
    fn poll_read(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, buf: &mut [u8])
        -> Poll<io::Result<usize>>
    {
        let mut inner = ready!(self.as_mut().poll_fill_buf(ctx))?;
        let len = io::Read::read(&mut inner, buf)?;
        self.consume(len);
        Poll::Ready(Ok(len))
    }
}

impl<D: Drive> AsyncBufRead for TcpStream<D> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.as_mut().guard_op(Op::Read);
        let fd = self.fd;
        let (ring, buf) = self.split();
        unsafe {
            buf.fill_read_buf(ctx, ring, |ring, ctx, buf| {
                let n = ready!(ring.poll(ctx, |sqe| {
                    buf.prepare_read(sqe);
                    sqe.raw_mut().fd = fd;
                    sqe.raw_mut().off_addr2.off = 0;
                }))?;
                Poll::Ready(Ok(n as u32))
            })
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.buf().consume(amt);
    }
}

impl<D: Drive> AsyncWrite for TcpStream<D> {
    fn poll_write(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, slice: &[u8]) -> Poll<io::Result<usize>> {
        self.as_mut().guard_op(Op::Write);
        let fd = self.fd;
        let (mut ring, buf) = self.split();
        let data = ready!(buf.fill_write_buf(ctx, ring.as_mut(), slice))?;
        let n = ready!(ring.poll(ctx, |sqe| unsafe {
            data.prepare_write(sqe);
            sqe.raw_mut().fd = fd;
            sqe.raw_mut().off_addr2.off = 0;
        }))?;
        buf.clear();
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_write(ctx, &[]))?;
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().guard_op(Op::Close);
        let fd = self.fd;
        ready!(self.ring().poll(ctx, |sqe| unsafe {
            uring_sys::io_uring_prep_close(sqe.raw_mut(), fd)
        }))?;
        Poll::Ready(Ok(()))
    }
}
