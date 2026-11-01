//! One process per proxy connection, and the proxy keeps one connection per client websocket, so
//! every image this process sees came from one websocket of one user. systemd starts it for the
//! accepted connection (`Accept=yes`) with the connection on stdin and stdout.
//!
//! Each image is decoded in a fresh child forked from this process, several at a time. This
//! process never reads image data: bytes move between the connection and each child's pipes with
//! splice(2), inside the kernel, so no image or pixel enters memory that a later child would
//! inherit (bytes it must discard go through a buffer it zeroes). A child never holds the
//! connection and cannot create processes, and this process forwards exactly one checked frame
//! per image, so a compromised decoder cannot read, or answer for, the connection's other images.
//!
//! The proxy sends `id (u32) length (u32)` and that many bytes of image `id`; the first chunk
//! starts an image and a zero length ends it, and images may interleave.
//!
//! This process sends `id (u32) kind (u8) length (u32)` and that many bytes:
//!   HEADER  width and height (u32 each), already checked
//!   PIXELS  the next of the frame's width * height * 3 RGB bytes
//!   DONE    the frame is complete and its decoder exited cleanly
//!   FAILED  the image was rejected; nothing more is sent for it
//! All integers are big-endian. A protocol error ends the connection.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::time::{Duration, Instant};

use seccompiler::BpfProgram;

use crate::{MAX_INPUT_BYTES, child, is_valid_output};

const CONNECTION_IN: RawFd = 0;
const CONNECTION_OUT: RawFd = 1;
const IMAGE_DEADLINE: Duration = Duration::from_secs(30);
/// The proxy sends at most four images at once on a connection.
const MAX_IN_FLIGHT: usize = 8;
const MAX_PIXELS_MESSAGE: usize = 1 << 20;
const DISCARD_BUFFER: usize = 64 * 1024;
const SPLICE_CHUNK: usize = 1 << 20;
const CHANNELS: usize = 3;

const HEADER: u8 = 1;
const PIXELS: u8 = 2;
const DONE: u8 = 3;
const FAILED: u8 = 4;

pub fn serve() -> io::Result<()> {
  let filter = child::compile_filter()?;

  // Descriptors 0 and 1 share one socket, so this makes both non-blocking.
  set_nonblocking(CONNECTION_IN)?;

  Connection::new(filter).run()
}

struct Image {
  pid: libc::pid_t,
  /// Closed once the image's last chunk has arrived.
  to_child: Option<OwnedFd>,
  from_child: OwnedFd,
  received: usize,
  output: Output,
  deadline: Instant,
}

enum Output {
  Header { bytes: [u8; 8], filled: usize },
  Pixels { remaining: usize },
  End,
}

enum Input {
  Header { bytes: [u8; 8], filled: usize },
  Chunk { id: u32, remaining: usize },
  Discard { remaining: usize },
}

enum Step {
  Continue,
  Wait,
  Fail,
  Finish,
}

struct Connection {
  filter: BpfProgram,
  images: BTreeMap<u32, Image>,
  /// Images that failed before their last chunk arrived; the rest of their chunks are discarded.
  discarding: BTreeSet<u32>,
  input: Input,
  input_open: bool,
}

impl Connection {
  fn new(filter: BpfProgram) -> Self {
    Connection {
      filter,
      images: BTreeMap::new(),
      discarding: BTreeSet::new(),
      input: Input::Header {
        bytes: [0; 8],
        filled: 0,
      },
      input_open: true,
    }
  }

  fn run(mut self) -> io::Result<()> {
    while self.input_open || !self.images.is_empty() {
      self.expire()?;

      let ready = self.wait_for_progress()?;
      if self.input_open {
        self.advance_input()?;
      }
      for (id, revents) in ready {
        self.advance_output(id, revents)?;
      }
    }

    Ok(())
  }

  /// Waits until the connection or a child's output is ready, or the next deadline passes.
  fn wait_for_progress(&self) -> io::Result<Vec<(u32, libc::c_short)>> {
    let mut polls = Vec::with_capacity(self.images.len() + 1);
    let mut ids = Vec::with_capacity(self.images.len());

    if self.input_open {
      polls.push(libc::pollfd {
        fd: CONNECTION_IN,
        events: libc::POLLIN,
        revents: 0,
      });
    }
    for (&id, image) in &self.images {
      polls.push(libc::pollfd {
        fd: image.from_child.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
      });
      ids.push(id);
    }

    let timeout = match self.images.values().map(|image| image.deadline).min() {
      None => -1,
      Some(deadline) => {
        let remaining = deadline.saturating_duration_since(Instant::now());
        remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int
      }
    };

    // SAFETY: polls holds valid pollfds.
    if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) } < 0 {
      let error = io::Error::last_os_error();
      if error.kind() != io::ErrorKind::Interrupted {
        return Err(error);
      }
    }

    let children = &polls[usize::from(self.input_open)..];
    Ok(
      ids
        .into_iter()
        .zip(children)
        .filter(|(_, poll)| poll.revents != 0)
        .map(|(id, poll)| (id, poll.revents))
        .collect(),
    )
  }

  fn expire(&mut self) -> io::Result<()> {
    let now = Instant::now();
    let expired: Vec<u32> = self
      .images
      .iter()
      .filter(|(_, image)| image.deadline <= now)
      .map(|(&id, _)| id)
      .collect();

    for id in expired {
      self.fail(id)?;
    }
    Ok(())
  }

  /// Reads chunk headers and moves chunk bytes until the connection has nothing more ready.
  fn advance_input(&mut self) -> io::Result<()> {
    loop {
      match &mut self.input {
        Input::Header { bytes, filled } => {
          match read_some(CONNECTION_IN, &mut bytes[*filled..])? {
            None => return Ok(()),
            Some(0) if *filled == 0 => return self.end_input(),
            Some(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Some(read) => *filled += read,
          }
          if *filled < bytes.len() {
            continue;
          }

          let id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
          let length = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
          self.input = Input::Header {
            bytes: [0; 8],
            filled: 0,
          };
          self.begin_chunk(id, length)?;
        }

        Input::Chunk { id, remaining } => {
          let (id, remaining) = (*id, *remaining);
          let Some(to_child) = self
            .images
            .get(&id)
            .and_then(|image| image.to_child.as_ref())
          else {
            self.input = Input::Discard { remaining };
            continue;
          };

          let deadline = Instant::now() + IMAGE_DEADLINE;
          let left = splice_into_child(to_child.as_raw_fd(), remaining, deadline)?;
          if left == 0 {
            self.input = Input::Header {
              bytes: [0; 8],
              filled: 0,
            };
          } else {
            // The child stopped reading because it died; discard the rest of its chunk.
            self.input = Input::Chunk {
              id,
              remaining: left,
            };
            self.fail(id)?;
          }
        }

        Input::Discard { remaining } => {
          if *remaining == 0 {
            self.input = Input::Header {
              bytes: [0; 8],
              filled: 0,
            };
            continue;
          }

          let mut scratch = [0u8; DISCARD_BUFFER];
          let wanted = (*remaining).min(scratch.len());
          let read = read_some(CONNECTION_IN, &mut scratch[..wanted])?;
          for byte in scratch.iter_mut() {
            // SAFETY: writes within the buffer; volatile so the zeroing is not optimized away.
            unsafe { ptr::write_volatile(byte, 0) };
          }

          match read {
            None => return Ok(()),
            Some(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Some(read) => *remaining -= read,
          }
        }
      }
    }
  }

  fn begin_chunk(&mut self, id: u32, length: usize) -> io::Result<()> {
    if self.discarding.contains(&id) {
      if length == 0 {
        self.discarding.remove(&id);
      } else {
        self.input = Input::Discard { remaining: length };
      }
      return Ok(());
    }

    if !self.images.contains_key(&id) {
      if self.images.len() >= MAX_IN_FLIGHT {
        return Err(invalid("too many images in flight"));
      }
      if length == 0 {
        return send(id, FAILED, &[]);
      }
      self.spawn(id)?;
    }

    let image = self.images.get_mut(&id).expect("spawned above");
    if image.to_child.is_none() {
      return Err(invalid("chunk after the end of an image"));
    }

    if length == 0 {
      image.to_child = None;
      return Ok(());
    }

    image.received += length;
    self.input = Input::Chunk {
      id,
      remaining: length,
    };
    if image.received > MAX_INPUT_BYTES {
      return self.fail(id);
    }
    Ok(())
  }

  fn spawn(&mut self, id: u32) -> io::Result<()> {
    let (child_input, to_child) = pipe()?;
    let (from_child, child_output) = pipe()?;
    enlarge_pipe(from_child.as_raw_fd());

    // SAFETY: this process is single-threaded, so the child may run arbitrary code after fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
      return Err(io::Error::last_os_error());
    }
    if pid == 0 {
      child::run(child_input, child_output, &self.filter);
    }

    drop(child_input);
    drop(child_output);
    set_nonblocking(to_child.as_raw_fd())?;
    set_nonblocking(from_child.as_raw_fd())?;

    self.images.insert(
      id,
      Image {
        pid,
        to_child: Some(to_child),
        from_child,
        received: 0,
        output: Output::Header {
          bytes: [0; 8],
          filled: 0,
        },
        deadline: Instant::now() + IMAGE_DEADLINE,
      },
    );
    Ok(())
  }

  fn advance_output(&mut self, id: u32, revents: libc::c_short) -> io::Result<()> {
    let hung_up = revents & (libc::POLLHUP | libc::POLLERR) != 0;

    loop {
      let Some(image) = self.images.get_mut(&id) else {
        return Ok(());
      };

      match step_output(id, image, hung_up)? {
        Step::Continue => {}
        Step::Wait => return Ok(()),
        Step::Fail => return self.fail(id),
        Step::Finish => return self.finish(id),
      }
    }
  }

  /// The frame is complete and the child's output has ended: report how its decoder exited.
  fn finish(&mut self, id: u32) -> io::Result<()> {
    let image = self
      .images
      .remove(&id)
      .expect("finishing an image in flight");
    self.stop_input(id, image.to_child.is_some());

    let kind = if wait_for_success(image.pid)? {
      DONE
    } else {
      FAILED
    };
    send(id, kind, &[])
  }

  fn fail(&mut self, id: u32) -> io::Result<()> {
    let Some(image) = self.images.remove(&id) else {
      return Ok(());
    };
    self.stop_input(id, image.to_child.is_some());

    // SAFETY: the pid is this process's unreaped child, so it cannot have been reused.
    unsafe { libc::kill(image.pid, libc::SIGKILL) };
    wait_for_success(image.pid)?;
    send(id, FAILED, &[])
  }

  /// Discards whatever the proxy still sends for an image that will not be decoded.
  fn stop_input(&mut self, id: u32, more_chunks_expected: bool) {
    if let Input::Chunk {
      id: current,
      remaining,
    } = self.input
      && current == id
    {
      self.input = Input::Discard { remaining };
    }
    if more_chunks_expected {
      self.discarding.insert(id);
    }
  }

  /// The proxy closed its side: images still waiting for chunks can never finish.
  fn end_input(&mut self) -> io::Result<()> {
    self.input_open = false;

    let unfinished: Vec<u32> = self
      .images
      .iter()
      .filter(|(_, image)| image.to_child.is_some())
      .map(|(&id, _)| id)
      .collect();
    for id in unfinished {
      self.fail(id)?;
    }
    Ok(())
  }
}

/// One step of moving a child's output to the connection.
fn step_output(id: u32, image: &mut Image, hung_up: bool) -> io::Result<Step> {
  let from_child = image.from_child.as_raw_fd();

  match &mut image.output {
    Output::Header { bytes, filled } => {
      match read_some(from_child, &mut bytes[*filled..])? {
        None => return Ok(Step::Wait),
        // The child writes nothing when it rejects an image; its own error is in the journal.
        Some(0) => return Ok(Step::Fail),
        Some(read) => *filled += read,
      }
      if *filled < bytes.len() {
        return Ok(Step::Continue);
      }

      let header = *bytes;
      let width = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
      let height = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
      if !is_valid_output(width, height) {
        return Ok(Step::Fail);
      }

      send(id, HEADER, &header)?;
      image.output = Output::Pixels {
        remaining: width as usize * height as usize * CHANNELS,
      };
      Ok(Step::Continue)
    }

    Output::Pixels { remaining } => {
      let available = bytes_available(from_child)?;
      if available == 0 {
        return Ok(if hung_up { Step::Fail } else { Step::Wait });
      }

      let count = available.min(*remaining).min(MAX_PIXELS_MESSAGE);
      send_header(id, PIXELS, count)?;
      splice_exact(
        from_child,
        CONNECTION_OUT,
        count,
        Instant::now() + IMAGE_DEADLINE,
      )?;

      *remaining -= count;
      if *remaining == 0 {
        image.output = Output::End;
      }
      Ok(Step::Continue)
    }

    // The child's output must end exactly where its frame does.
    Output::End => {
      if bytes_available(from_child)? > 0 {
        Ok(Step::Fail)
      } else if hung_up {
        Ok(Step::Finish)
      } else {
        Ok(Step::Wait)
      }
    }
  }
}

fn send(id: u32, kind: u8, payload: &[u8]) -> io::Result<()> {
  send_header(id, kind, payload.len())?;
  write_exact(CONNECTION_OUT, payload, Instant::now() + IMAGE_DEADLINE)
}

fn send_header(id: u32, kind: u8, length: usize) -> io::Result<()> {
  let mut header = [0u8; 9];
  header[0..4].copy_from_slice(&id.to_be_bytes());
  header[4] = kind;
  header[5..9].copy_from_slice(&(length as u32).to_be_bytes());
  write_exact(CONNECTION_OUT, &header, Instant::now() + IMAGE_DEADLINE)
}

/// Moves a chunk into a child's input; returns how much was left if the child stopped reading.
fn splice_into_child(
  to_child: RawFd,
  mut remaining: usize,
  deadline: Instant,
) -> io::Result<usize> {
  while remaining > 0 {
    match splice_some(CONNECTION_IN, to_child, remaining) {
      Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
      Ok(moved) => remaining -= moved,
      Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(remaining),
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
        wait(to_child, libc::POLLOUT, Some(deadline))?;
        wait(CONNECTION_IN, libc::POLLIN, Some(deadline))?;
      }
      Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
      Err(error) => return Err(error),
    }
  }
  Ok(0)
}

fn splice_exact(from: RawFd, to: RawFd, mut remaining: usize, deadline: Instant) -> io::Result<()> {
  while remaining > 0 {
    match splice_some(from, to, remaining) {
      Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
      Ok(moved) => remaining -= moved,
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
        wait(to, libc::POLLOUT, Some(deadline))?;
        wait(from, libc::POLLIN, Some(deadline))?;
      }
      Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
      Err(error) => return Err(error),
    }
  }
  Ok(())
}

fn splice_some(from: RawFd, to: RawFd, remaining: usize) -> io::Result<usize> {
  // SAFETY: both descriptors are open; null offsets splice from and to the current position.
  let moved = unsafe {
    libc::splice(
      from,
      ptr::null_mut(),
      to,
      ptr::null_mut(),
      remaining.min(SPLICE_CHUNK),
      libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
    )
  };

  if moved < 0 {
    Err(io::Error::last_os_error())
  } else {
    Ok(moved as usize)
  }
}

/// How many bytes are waiting in a pipe, so a PIXELS message can name its length up front.
fn bytes_available(fd: RawFd) -> io::Result<usize> {
  let mut available: libc::c_int = 0;

  // SAFETY: FIONREAD writes one int.
  if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(available.max(0) as usize)
}

fn write_exact(fd: RawFd, buffer: &[u8], deadline: Instant) -> io::Result<()> {
  let mut written = 0;

  while written < buffer.len() {
    // SAFETY: the buffer is valid for the given length.
    let count = unsafe {
      libc::write(
        fd,
        buffer[written..].as_ptr().cast(),
        buffer.len() - written,
      )
    };
    if count >= 0 {
      written += count as usize;
      continue;
    }

    let error = io::Error::last_os_error();
    match error.kind() {
      io::ErrorKind::WouldBlock => wait(fd, libc::POLLOUT, Some(deadline))?,
      io::ErrorKind::Interrupted => {}
      _ => return Err(error),
    }
  }

  Ok(())
}

/// `None` when the descriptor would block.
fn read_some(fd: RawFd, buffer: &mut [u8]) -> io::Result<Option<usize>> {
  loop {
    // SAFETY: the buffer is valid for the given length.
    let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if count >= 0 {
      return Ok(Some(count as usize));
    }

    let error = io::Error::last_os_error();
    match error.kind() {
      io::ErrorKind::WouldBlock => return Ok(None),
      io::ErrorKind::Interrupted => {}
      _ => return Err(error),
    }
  }
}

fn wait(fd: RawFd, events: libc::c_short, deadline: Option<Instant>) -> io::Result<()> {
  loop {
    let timeout = match deadline {
      None => -1,
      Some(deadline) => {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
          return Err(io::ErrorKind::TimedOut.into());
        }
        remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int
      }
    };

    let mut poll = libc::pollfd {
      fd,
      events,
      revents: 0,
    };
    // SAFETY: one valid pollfd.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
    if ready > 0 {
      return Ok(());
    }
    if ready == 0 {
      return Err(io::ErrorKind::TimedOut.into());
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::Interrupted {
      return Err(error);
    }
  }
}

/// Reaps a child that has exited or been killed; true if it exited cleanly.
fn wait_for_success(pid: libc::pid_t) -> io::Result<bool> {
  let mut status = 0;

  loop {
    // SAFETY: waits for this process's own child.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited == pid {
      return Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::Interrupted {
      return Err(error);
    }
  }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut fds = [0; 2];

  // SAFETY: fds has room for both descriptors.
  if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
    return Err(io::Error::last_os_error());
  }

  // SAFETY: pipe2 returned two new descriptors that nothing else owns.
  Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Lets a child write a whole PIXELS message before the parent has to wake and relay it; with the
/// default 64 KiB pipe a frame takes dozens of round trips between them, and waking another CPU is
/// costly in a TDX guest. If the kernel refuses (this user already holds too much pipe memory),
/// the default size still works, in smaller messages.
fn enlarge_pipe(fd: RawFd) {
  // SAFETY: fcntl on an open descriptor.
  unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, MAX_PIXELS_MESSAGE as libc::c_int) };
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  // SAFETY: fcntl on an open descriptor.
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn invalid(message: &str) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidData, message)
}
