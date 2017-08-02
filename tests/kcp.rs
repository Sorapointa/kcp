extern crate kcp;
extern crate bytes;
extern crate rand;
extern crate time;
extern crate env_logger;

use std::cell::RefCell;
use std::collections::LinkedList;
use std::io::{self, Cursor, ErrorKind, Read, Write};
use std::rc::Rc;
use std::thread::sleep;
use std::time::Duration;

use bytes::{Bytes, BytesMut, LittleEndian};
use bytes::buf::{Buf, BufMut, IntoBuf};
use time::Timespec;

use kcp::Kcp;

#[derive(Debug)]
struct DelayPacket {
    buf: Bytes,
    ts: u32,
}

impl DelayPacket {
    fn new(buf: Bytes) -> DelayPacket {
        DelayPacket { buf: buf, ts: 0 }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn ts(&self) -> u32 {
        self.ts
    }

    fn set_ts(&mut self, ts: u32) {
        self.ts = ts;
    }

    fn reader(self) -> Cursor<Bytes> {
        self.buf.into_buf()
    }
}

struct Random {
    seeds: Vec<u32>,
    size: usize,
}

impl Random {
    fn new(size: usize) -> Random {
        Random {
            seeds: vec![0u32; size],
            size: 0,
        }
    }

    fn random(&mut self) -> u32 {
        if self.seeds.is_empty() {
            return 0;
        }

        if self.size == 0 {
            let len = self.seeds.len();
            for i in 0..len {
                self.seeds[i] = i as u32;
            }
            self.size = self.seeds.len();
        }

        let i = rand::random::<usize>() % self.size;
        let x = self.seeds[i];

        self.size -= 1;
        self.seeds[i] = self.seeds[self.size];

        x
    }
}

#[inline]
fn as_millisec(timespec: &Timespec) -> u32 {
    (timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000) as u32
}

#[inline]
fn current() -> u32 {
    let timespec = time::get_time();
    as_millisec(&timespec)
}

struct LatencySimulator {
    lostrate: u32,
    rttmin: u32,
    rttmax: u32,
    nmax: usize,
    tx1: u32,
    tx2: u32,
    current: u32,
    p12: LinkedList<DelayPacket>,
    p21: LinkedList<DelayPacket>,
    r12: Random,
    r21: Random,
}

impl LatencySimulator {
    fn new(lostrate: u32, rttmin: u32, rttmax: u32, nmax: usize) -> LatencySimulator {
        LatencySimulator {
            lostrate: lostrate / 2,
            rttmin: rttmin,
            rttmax: rttmax,
            nmax: nmax,
            tx1: 0,
            tx2: 0,
            current: ::current(),
            p12: LinkedList::new(),
            p21: LinkedList::new(),
            r12: Random::new(100),
            r21: Random::new(100),
        }
    }

    fn send(&mut self, peer: u32, data: &[u8]) -> io::Result<usize> {
        // println!("[VNET] SEND {} {:?}", peer, data);
        if peer == 0 {
            self.tx1 += 1;

            if self.r12.random() < self.lostrate {
                return Ok(data.len());
            }
            if self.p12.len() >= self.nmax {
                return Ok(data.len());
            }
        } else {
            self.tx2 += 1;

            if self.r21.random() < self.lostrate {
                return Ok(data.len());
            }
            if self.p21.len() >= self.nmax {
                return Ok(data.len());
            }
        }

        let mut pkg = DelayPacket::new(Bytes::from(data));
        self.current = ::current();

        let mut delay = self.rttmin;
        if self.rttmax > self.rttmin {
            delay += rand::random::<u32>() % (self.rttmax - self.rttmin);
        }

        pkg.set_ts(self.current + delay);
        // println!("[VNET] ACTUAL SEND {:?}", pkg);

        if peer == 0 {
            self.p12.push_back(pkg);
        } else {
            self.p21.push_back(pkg);
        }

        Ok(data.len())
    }

    fn recv(&mut self, peer: u32, data: &mut [u8]) -> io::Result<usize> {
        {
            let pkg = if peer == 0 {
                match self.p12.front() {
                    None => {
                        return Err(io::Error::new(ErrorKind::WouldBlock, "No packet yet"));
                    }
                    Some(pkg) => pkg,
                }
            } else {
                match self.p21.front() {
                    None => {
                        return Err(io::Error::new(ErrorKind::WouldBlock, "No packet yet"));
                    }
                    Some(pkg) => pkg,
                }
            };

            self.current = ::current();
            if self.current < pkg.ts() {
                return Err(io::Error::new(ErrorKind::WouldBlock, "No packet yet"));
            }

            if data.len() < pkg.len() {
                return Err(io::Error::new(ErrorKind::InvalidInput, "Buffer is too small"));
            }
        }

        let pkg = if peer == 0 {
            self.p12.pop_front().unwrap()
        } else {
            self.p21.pop_front().unwrap()
        };

        pkg.reader().read(data)
    }

    fn tx1(&self) -> u32 {
        self.tx1
    }
}

struct KcpOutput {
    sim: Rc<RefCell<LatencySimulator>>,
    peer: u32,
}

impl Write for KcpOutput {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut sim = self.sim.borrow_mut();
        sim.send(self.peer, data)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
enum TestMode {
    Default,
    Normal,
    Fast,
}

fn run(mode: TestMode, msgcount: u32) {
    // Lost Rate 10%, Rtt 60ms ~ 125ms
    let vnet = LatencySimulator::new(10, 60, 125, 1000);
    let vnet = Rc::new(RefCell::new(vnet));

    let mut kcp1 = Kcp::new(0x11223344,
                            KcpOutput {
                                sim: vnet.clone(),
                                peer: 0,
                            });
    let mut kcp2 = Kcp::new(0x11223344,
                            KcpOutput {
                                sim: vnet.clone(),
                                peer: 1,
                            });

    let mut current = ::current();
    let mut slap = current + 20;
    let mut index = 0;
    let mut next = 0;
    let mut sumrtt = 0;
    let mut count = 0;
    let mut maxrtt = 0;

    // Set wnd size, average latency 200ms, 20ms per packet
    // Set max wnd to 128 considering packet lost and retry
    kcp1.set_wndsize(128, 128);
    kcp2.set_wndsize(128, 128);

    match mode {
        TestMode::Default => {
            kcp1.set_nodelay(0, 10, 0, false);
            kcp2.set_nodelay(0, 10, 0, false);
        }
        TestMode::Normal => {
            kcp1.set_nodelay(0, 10, 0, true);
            kcp2.set_nodelay(0, 10, 0, true);
        }
        TestMode::Fast => {
            kcp1.set_nodelay(1, 10, 2, true);
            kcp2.set_nodelay(1, 10, 2, true);

            kcp1.set_rx_minrto(10);
            kcp2.set_fast_resend(1);
        }
    }

    let mut ts1 = ::current();

    let mut buf = [0u8; 2000];
    while next <= msgcount {
        sleep(Duration::from_millis(1));

        current = ::current();
        kcp1.update(::current()).unwrap();
        kcp2.update(::current()).unwrap();

        // kcp1 send packet every 20ms
        while current >= slap {
            let mut buf = BytesMut::with_capacity(8);
            buf.put_u32::<LittleEndian>(index);
            index += 1;
            buf.put_u32::<LittleEndian>(current);

            kcp1.send(&buf).unwrap();
            // println!("SENT curr: {} {} {:?}", index, current, &buf[..]);

            slap += 20;
        }

        // vnet p1 -> p2
        loop {
            let mut vn = vnet.borrow_mut();
            match vn.recv(1, &mut buf) {
                Err(..) => break,
                Ok(n) => {
                    // println!("RECV kcp2 {:?}", &buf[..n]);
                    kcp2.input(&buf[..n]).unwrap();
                }
            }
        }

        // vnet p2 -> p1
        loop {
            let mut vn = vnet.borrow_mut();
            match vn.recv(0, &mut buf) {
                Err(..) => break,
                Ok(n) => {
                    // println!("RECV kcp1 {:?}", &buf[..n]);
                    kcp1.input(&buf[..n]).unwrap();
                }
            }
        }

        // kcp2 echos back
        loop {
            match kcp2.recv(&mut buf) {
                Err(..) => break,
                Ok(n) => {
                    // println!("ECHO kcp2 {:?}", &buf[..n]);
                    kcp2.send(&buf[..n]).unwrap();
                }
            }
        }

        // kcp1 checks response from kcp2
        loop {
            match kcp1.recv(&mut buf) {
                Err(..) => break,
                Ok(n) => {
                    let mut cur = Cursor::new(&buf[..n]);

                    let sn = cur.get_u32::<LittleEndian>();
                    let ts = cur.get_u32::<LittleEndian>();
                    // println!("[RECV] sn={} ts={}", sn, ts);
                    let rtt = current - ts;

                    if sn != next {
                        panic!("Received not continously packet: sn {} <-> {}", count, next);
                    }

                    next += 1;
                    sumrtt += rtt;
                    count += 1;

                    if rtt > maxrtt {
                        maxrtt = rtt;
                    }

                    println!("[RECV] mode={:?} sn={} rtt={}", mode, sn, rtt);
                }
            }
        }
    }

    ts1 = ::current() - ts1;
    println!("{:?} mode result ({}ms):", mode, ts1);
    println!("avgrtt={} maxrtt={} tx={}", (sumrtt / count), maxrtt, vnet.borrow().tx1());
}

#[test]
fn kcp_default() {
    let _ = env_logger::init();
    run(TestMode::Default, 200);
}

#[test]
fn kcp_normal() {
    let _ = env_logger::init();
    run(TestMode::Normal, 200);
}

#[test]
fn kcp_fast() {
    let _ = env_logger::init();
    run(TestMode::Fast, 200);
}