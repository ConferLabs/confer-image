//! The decoder child's system call filter, installed in children forked from the test process.
#![cfg(target_os = "linux")]

use confer_image_normalizer::child::compile_filter;

#[derive(Debug, PartialEq)]
enum Ending {
  Exited(i32),
  Signalled(i32),
}

/// Forks a child that installs the filter, makes one raw system call and exits.
fn ending_after(call: libc::c_long, arguments: [libc::c_long; 3]) -> Ending {
  let filter = compile_filter().unwrap();

  // SAFETY: the child makes only raw system calls before _exit.
  let pid = unsafe { libc::fork() };
  assert!(pid >= 0);

  if pid == 0 {
    // SAFETY: as above.
    unsafe {
      if seccompiler::apply_filter(&filter).is_err() {
        libc::_exit(100);
      }
      libc::syscall(call, arguments[0], arguments[1], arguments[2]);
      libc::_exit(0);
    }
  }

  let mut status = 0;
  // SAFETY: waits for the child forked above.
  assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);

  if libc::WIFSIGNALED(status) {
    Ending::Signalled(libc::WTERMSIG(status))
  } else {
    Ending::Exited(libc::WEXITSTATUS(status))
  }
}

#[test]
fn kills_a_child_that_does_anything_but_decode() {
  let forbidden = [
    (
      "clone",
      libc::SYS_clone,
      [libc::SIGCHLD as libc::c_long, 0, 0],
    ),
    ("clone3", libc::SYS_clone3, [0, 0, 0]),
    ("execve", libc::SYS_execve, [0, 0, 0]),
    (
      "openat",
      libc::SYS_openat,
      [libc::AT_FDCWD as libc::c_long, 0, 0],
    ),
    (
      "socket",
      libc::SYS_socket,
      [
        libc::AF_UNIX as libc::c_long,
        libc::SOCK_STREAM as libc::c_long,
        0,
      ],
    ),
    ("kill", libc::SYS_kill, [0, 0, 0]),
    ("tgkill", libc::SYS_tgkill, [0, 0, 0]),
    ("ptrace", libc::SYS_ptrace, [0, 0, 0]),
    ("process_vm_readv", libc::SYS_process_vm_readv, [0, 0, 0]),
    ("close", libc::SYS_close, [0, 0, 0]),
    ("dup3", libc::SYS_dup3, [0, 5, 0]),
    ("nanosleep", libc::SYS_nanosleep, [0, 0, 0]),
    ("clock_nanosleep", libc::SYS_clock_nanosleep, [0, 0, 0]),
    ("futex", libc::SYS_futex, [0, 0, 0]),
    ("prctl", libc::SYS_prctl, [0, 0, 0]),
    ("seccomp", libc::SYS_seccomp, [0, 0, 0]),
    ("mprotect", libc::SYS_mprotect, [0, 0, 0]),
  ];

  for (name, call, arguments) in forbidden {
    assert_eq!(
      ending_after(call, arguments),
      Ending::Signalled(libc::SIGSYS),
      "{name} was allowed"
    );
  }
}

#[test]
fn allows_what_decoding_needs() {
  assert_eq!(ending_after(libc::SYS_brk, [0, 0, 0]), Ending::Exited(0));
  assert_eq!(ending_after(libc::SYS_write, [1, 0, 0]), Ending::Exited(0));
  assert_eq!(ending_after(libc::SYS_read, [0, 0, 0]), Ending::Exited(0));
  assert_eq!(
    ending_after(libc::SYS_getrandom, [0, 0, 0]),
    Ending::Exited(0)
  );
}
