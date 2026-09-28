use interprocess::local_socket::{ToFsName as _, traits::Stream as _};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_millis(300);

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "SHEPR_BRIDGE_TEST_SOCKET is this test's own re-exec harness probe, not a shepr setting"
)]
fn bridge_child() {
    let Some(path) = std::env::var_os("SHEPR_BRIDGE_TEST_SOCKET") else {
        return;
    };
    let name = PathBuf::from(path)
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .expect("test precondition");
    let stream = interprocess::local_socket::Stream::connect(name).expect("test precondition");
    let outcome = super::forward_remote_bridge_stdio_with_timeout(stream, Some(TIMEOUT))
        .expect("test precondition");
    if outcome == super::RemoteBridgeOutcome::IdleExpired {
        exit_as_expired_bridge();
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "this re-exec child stands in for the bridge process, whose main exits 1 on IdleExpired; \
              returning to libtest would block its result report on the full stdout pipe"
)]
fn exit_as_expired_bridge() -> ! {
    std::process::exit(1)
}

struct Bridge {
    child: Child,
    stream: UnixStream,
    path: PathBuf,
}

impl Bridge {
    fn start() -> Self {
        let path = shepr_test_support::ScratchDir::new("bridge").join("s.sock");
        let listener = UnixListener::bind(&path).expect("test precondition");
        listener.set_nonblocking(true).expect("test precondition");
        let mut command = shepr_test_support::command_in_scratch(
            std::env::current_exe().expect("test precondition"),
            "bridge-child",
        );
        command
            .args([
                "--exact",
                "remote_bridge_tests::bridge_child",
                "--nocapture",
            ])
            .env("SHEPR_BRIDGE_TEST_SOCKET", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().expect("test precondition");
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&path);
                    panic!("bridge did not connect: {error}");
                }
            }
        };
        stream.set_nonblocking(false).expect("test precondition");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("test precondition");
        stream
            .set_write_timeout(Some(Duration::from_millis(100)))
            .expect("test precondition");
        Self {
            child,
            stream,
            path,
        }
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().expect("test precondition") {
                return status;
            }
            assert!(Instant::now() < deadline, "bridge did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) -> String {
        drop(self.child.stdin.take());
        let mut input = Vec::new();
        self.stream
            .read_to_end(&mut input)
            .expect("test precondition");
        self.stream
            .write_all(b"final-output-after-stdin-eof")
            .expect("test precondition");
        self.stream
            .shutdown(std::net::Shutdown::Write)
            .expect("test precondition");
        assert!(self.wait().success());
        let mut output = String::new();
        self.child
            .stdout
            .take()
            .expect("test precondition")
            .read_to_string(&mut output)
            .expect("test precondition");
        output
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn bridge_expires_when_silent_or_stdout_is_blocked() {
    for blocked in [false, true] {
        let mut bridge = Bridge::start();
        if blocked {
            let buffer = vec![b'x'; 64 * 1024];
            let deadline = Instant::now() + Duration::from_secs(2);
            while bridge.stream.write_all(&buffer).is_ok() {
                assert!(Instant::now() < deadline, "failed to fill bridge stdout");
            }
        }
        assert_eq!(bridge.wait().code(), Some(1));
    }
}

#[test]
fn bridge_preserves_one_way_progress_and_drains_after_stdin_eof() {
    for upload in [false, true] {
        let mut bridge = Bridge::start();
        for _ in 0..12 {
            if upload {
                bridge
                    .child
                    .stdin
                    .as_mut()
                    .expect("test precondition")
                    .write_all(b"ping")
                    .expect("test precondition");
                bridge
                    .stream
                    .read_exact(&mut [0; 4])
                    .expect("test precondition");
            } else {
                bridge
                    .stream
                    .write_all(b"output")
                    .expect("test precondition");
            }
            std::thread::sleep(Duration::from_millis(60));
            assert!(
                bridge
                    .child
                    .try_wait()
                    .expect("test precondition")
                    .is_none()
            );
        }
        assert!(bridge.finish().contains("final-output-after-stdin-eof"));
    }
}
