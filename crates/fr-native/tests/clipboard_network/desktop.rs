use std::{
    io::{BufRead, BufReader, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    time::Duration,
};
pub struct Desktop {
    process: Child,
    pub display: String,
}
impl Desktop {
    pub fn new() -> Self {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let process = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::inherit())
            .spawn()
            .expect("real Xvfb required");
        let mut desktop = Self {
            process,
            display: String::new(),
        };
        let mut number = String::new();
        BufReader::new(reader.take(16))
            .read_line(&mut number)
            .unwrap();
        assert!(number.ends_with('\n'));
        let number: u16 = number.trim().parse().unwrap();
        desktop.display = format!(":{number}");
        desktop
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = self.process.kill();
        self.process.wait().expect("reap test X server");
    }
}
