// Boost/Apache2 License

//! Functions for managing the Apple event loop.
//!
//! Systems made by Apple (macOS, iOS, etc.) use a special event loop type to handle incoming system
//! events. For users of the raw Core Foundation, this is the [`CFRunLoop`] type. For users of the
//! Cocoa framework, this is the [`NSRunLoop`] type. However, it should be noted that [`NSRunLoop`] is
//! just a wrapper around [`CFRunLoop`], and the two are interchangeable.
//!
//! This crate provides asynchronous functions for managing the Apple event loop.
//! allow `Future`s to be run on the event loop.
//!
//! [`CFRunLoop`]: https://developer.apple.com/documentation/corefoundation/cfrunloop
//! [`NSRunLoop`]: https://developer.apple.com/documentation/foundation/nsrunloop

#![cfg(target_vendor = "apple")]

use core_foundation::runloop as cfrunloop;
use io_lifetimes::{AsFd, BorrowedFd, OwnedFd};

use std::convert::Infallible;
use std::io;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// The Apple event loop.
pub struct RunLoop {
    /// The file descriptor used to poll the run loop.
    kqueue: BorrowedFd<'static>,
}

impl AsRawFd for RunLoop {
    fn as_raw_fd(&self) -> RawFd {
        self.kqueue.as_raw_fd()
    }
}

impl AsFd for RunLoop {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.kqueue.as_fd()
    }
}

impl RunLoop {
    /// Create a new run loop.
    pub fn new() -> io::Result<Self> {
        let kqueue = gcd_kqueue();
        Ok(Self { kqueue })
    }

    /// Drain a single event from this loop.
    ///
    /// Returns `true` if a event was successfully run.
    pub fn tick(&self) -> bool {
        let result = cfrunloop::CFRunLoop::run_in_mode(
            unsafe { cfrunloop::kCFRunLoopDefaultMode },
            Duration::from_secs(0),
            true,
        );

        matches!(result, cfrunloop::CFRunLoopRunResult::HandledSource)
    }

    /// Drain the queue of events from this loop.
    pub fn drain(&self) {
        while self.tick() {}
    }

    /// Block until an event is available.
    pub fn wait_for_event(&self) -> io::Result<()> {
        let changelist = [];
        let mut eventlist = unsafe { mem::zeroed::<libc::kevent>() };

        let result = unsafe {
            libc::kevent(
                self.as_raw_fd(),
                changelist.as_ptr(),
                changelist.len() as _,
                &mut eventlist,
                1,
                ptr::null(),
            )
        };

        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Block until either an event is available or the timeout is reached.
    pub fn wait_for_event_timeout(&self, timeout: Duration) -> io::Result<bool> {
        let changelist = [];
        let mut eventlist = unsafe { mem::zeroed::<libc::kevent>() };

        let timeout = libc::timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };

        let result = unsafe {
            libc::kevent(
                self.as_raw_fd(),
                changelist.as_ptr(),
                changelist.len() as _,
                &mut eventlist,
                1,
                &timeout,
            )
        };

        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result > 0)
        }
    }

    /// Block until either an event is available or the deadline is reached.
    pub fn wait_for_event_deadline(&self, deadline: Instant) -> io::Result<bool> {
        let now = Instant::now();

        match deadline.checked_duration_since(now) {
            Some(timeout) => self.wait_for_event_timeout(timeout),
            None => Ok(false),
        }
    }

    /// Run a single cycle of the event loop.
    pub fn cycle(&self) -> io::Result<()> {
        self.wait_for_event()?;
        self.drain();
        Ok(())
    }

    /// Run a single cycle of the event loop, or timeout.
    pub fn cycle_timeout(&self, timeout: Duration) -> io::Result<bool> {
        if self.wait_for_event_timeout(timeout)? {
            self.drain();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Run a single cycle of the event loop, or until a deadline.
    pub fn cycle_deadline(&self, deadline: Instant) -> io::Result<bool> {
        if self.wait_for_event_deadline(deadline)? {
            self.drain();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Run the event loop indefinitely.
    pub fn run(&self) -> io::Result<Infallible> {
        loop {
            self.cycle()?;
        }
    }

    /// Run the event loop indefinitely, or until a timeout.
    pub fn run_timeout(&self, timeout: Duration) -> io::Result<()> {
        loop {
            if !self.cycle_timeout(timeout)? {
                return Ok(());
            }
        }
    }

    /// Run the event loop indefinitely, or until a deadline.
    pub fn run_deadline(&self, deadline: Instant) -> io::Result<()> {
        loop {
            if !self.cycle_deadline(deadline)? {
                return Ok(());
            }
        }
    }
}

/// Get a static reference to a file descriptor that can be used to poll GCD.
fn gcd_kqueue() -> BorrowedFd<'static> {
    static GCD_KQUEUE: AtomicI32 = AtomicI32::new(-1);

    let mut kqueue = GCD_KQUEUE.load(Ordering::Relaxed);

    if kqueue == -1 {
        let new_kqueue = initialize_gcd_kqueue().expect("failed to initialize GCD kqueue");
        let old_kqueue = GCD_KQUEUE
            .compare_exchange(-1, new_kqueue, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or_else(|x| x);

        if old_kqueue == -1 {
            kqueue = new_kqueue;
        } else {
            unsafe { libc::close(kqueue) };
        }
    }

    unsafe { BorrowedFd::borrow_raw(kqueue) }
}

/// Get a file descriptor that can be used to poll GCD.
fn initialize_gcd_kqueue() -> io::Result<RawFd> {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }

    // Register the GCD mach port into this system.
    let port = unsafe { _dispatch_get_main_queue_port_4CF() };
    let changelist = [libc::kevent64_s {
        ident: port as _,
        filter: libc::EVFILT_MACHPORT,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: MACH_RECV_MSG,
        data: 0,
        udata: 0,
        ..unsafe { mem::zeroed() }
    }];
    let mut eventlist = changelist;

    let result = unsafe {
        libc::kevent64(
            kq,
            changelist.as_ptr(),
            changelist.len() as _,
            eventlist.as_mut_ptr(),
            eventlist.len() as _,
            0,
            ptr::null(),
        )
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(kq)
    }
}

extern "C" {
    fn _dispatch_get_main_queue_port_4CF() -> mach_port_t;
}

// https://github.com/fitzgen/mach/blob/d5756520e1ef9e0b58216b1302ac4eff30bae825/src/port.rs#L13
#[allow(non_camel_case_types)]
type mach_port_t = libc::c_uint;
const MACH_RECV_MSG: u32 = 0x2;
