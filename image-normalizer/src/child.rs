//! The child that decodes one image. It takes over the pipes the parent created, closes every
//! other descriptor, caps its CPU time and installs a system call allowlist before it reads a
//! byte of the image.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

use crate::{MAX_INPUT_BYTES, normalize};

/// Everything the child does once its filter is installed: read the image, allocate while
/// decoding, write the frame (and errors to stderr), exit. Any other call kills it, so it cannot
/// create processes, signal or inspect other processes, open files or sockets, or sleep.
/// System call numbers are i64 on every architecture seccompiler supports.
const ALLOWED_CALLS: &[i64] = &[
  libc::SYS_read,
  libc::SYS_write,
  libc::SYS_mmap,
  libc::SYS_munmap,
  libc::SYS_mremap,
  libc::SYS_madvise,
  libc::SYS_brk,
  // The first hash map a decoder builds seeds its hasher from the kernel (lossy WebP does).
  libc::SYS_getrandom,
  libc::SYS_rt_sigreturn,
  libc::SYS_exit,
  libc::SYS_exit_group,
];

/// The largest image vLLM accepts decodes in a few seconds; a decoder spinning longer is killed.
const CPU_SECONDS: libc::rlim_t = 30;

/// The largest image vLLM accepts peaks near 2 GiB while decoding. Several children share the
/// sandbox's memory, so each is capped on its own and fails alone.
const ADDRESS_SPACE_BYTES: libc::rlim_t = 4 << 30;

/// Compiled once in the parent, before any image arrives, so each child only installs it.
pub fn compile_filter() -> io::Result<BpfProgram> {
  let rules = ALLOWED_CALLS
    .iter()
    .map(|&call| (call, Vec::new()))
    .collect::<BTreeMap<_, _>>();
  let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(io::Error::other)?;
  let filter = SeccompFilter::new(
    rules,
    SeccompAction::KillProcess,
    SeccompAction::Allow,
    arch,
  )
  .map_err(io::Error::other)?;

  filter.try_into().map_err(io::Error::other)
}

pub fn run(input: OwnedFd, output: OwnedFd, filter: &BpfProgram) -> ! {
  let code = match enter_sandbox(input, output, filter).and_then(|()| decode()) {
    Ok(()) => 0,
    Err(error) => {
      // One write, so concurrent children's lines in the journal do not interleave.
      let line = format!("confer-image-normalizer: {error}\n");
      let _ = io::stderr().write_all(line.as_bytes());
      1
    }
  };

  // SAFETY: ends the child without running anything else.
  unsafe { libc::_exit(code) }
}

fn enter_sandbox(input: OwnedFd, output: OwnedFd, filter: &BpfProgram) -> io::Result<()> {
  // SAFETY: plain descriptor and rlimit calls on this process.
  unsafe {
    if libc::dup2(input.as_raw_fd(), 0) < 0 || libc::dup2(output.as_raw_fd(), 1) < 0 {
      return Err(io::Error::last_os_error());
    }

    // Replaces the inherited connection on 0 and 1 above; this closes everything else,
    // including both pipes' originals, the parent's ends and other images' pipes.
    if libc::syscall(libc::SYS_close_range, 3, libc::c_uint::MAX, 0) != 0 {
      return Err(io::Error::last_os_error());
    }

    for (resource, limit) in [
      (libc::RLIMIT_CPU, CPU_SECONDS),
      (libc::RLIMIT_AS, ADDRESS_SPACE_BYTES),
    ] {
      let limit = libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
      };
      if libc::setrlimit(resource, &limit) != 0 {
        return Err(io::Error::last_os_error());
      }
    }
  }

  // Already closed by close_range; dropping them would close unrelated descriptors.
  std::mem::forget(input);
  std::mem::forget(output);

  seccompiler::apply_filter(filter).map_err(io::Error::other)
}

fn decode() -> io::Result<()> {
  let mut input = Vec::new();
  io::stdin()
    .lock()
    .take(MAX_INPUT_BYTES as u64 + 1)
    .read_to_end(&mut input)?;

  let image = normalize(&input)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

  let mut output = io::stdout().lock();
  output.write_all(&image.width().to_be_bytes())?;
  output.write_all(&image.height().to_be_bytes())?;
  output.write_all(image.as_raw())?;
  output.flush()
}
