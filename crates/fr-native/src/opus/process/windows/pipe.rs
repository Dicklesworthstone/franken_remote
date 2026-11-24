//! A connected local byte pipe. Only the supervisor endpoint uses NOWAIT;
//! the codec child may block on its inherited endpoint and is job-contained.
use super::{handle, os, security::Descriptor};
use crate::opus::process::windows_policy;
use std::{
    io,
    os::windows::io::{FromRawHandle, OwnedHandle},
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        },
        Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE, OPEN_EXISTING,
            PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
            WriteFile,
        },
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        },
    },
    core::PCWSTR,
};

const PIPE_BYTES: u32 = 65_536;
pub(in super::super) struct Pipe(OwnedHandle);
impl Pipe {
    pub(super) fn pair(live: &impl Fn() -> io::Result<()>) -> io::Result<(Self, OwnedHandle)> {
        live()?;
        let descriptor = Descriptor::for_logon()?;
        let mut nonce = [0u8; 16];
        // SAFETY: the system-preferred RNG writes only this fixed-size buffer;
        // no provider or callback handle is supplied or retained.
        let status = unsafe { BCryptGenRandom(None, &mut nonce, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        if status.0 < 0 {
            return Err(io::ErrorKind::Other.into());
        }
        let name: Vec<u16> = format!(
            r"\\.\pipe\fr-opus-{}-{:032x}",
            std::process::id(),
            u128::from_ne_bytes(nonce)
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let server_security = descriptor.attributes(false)?;
        live()?;
        // SAFETY: valid NUL-terminated name and a live descriptor. A first,
        // single-instance, remote-rejecting pipe cannot take over an existing
        // endpoint. This server handle is explicitly non-inheritable.
        let raw = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BYTES,
                PIPE_BYTES,
                0,
                Some(&raw const server_security),
            )
        };
        if raw.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful CreateNamedPipeW transfers a unique handle.
        let server = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let client_security = descriptor.attributes(true)?;
        live()?;
        // SAFETY: this process connects its own endpoint before spawning the
        // child. The inheritable client handle is later explicitly allowlisted.
        // Identification-level SQOS prevents using the client for impersonation.
        let raw = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                Some(&raw const client_security),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        }
        .map_err(os)?;
        // SAFETY: the successful CreateFileW transfers a unique handle.
        let client = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        live()?;
        // SAFETY: NOWAIT makes this synchronous call return immediately. The
        // client already connected, so only PIPE_CONNECTED proves success.
        let connected = unsafe { ConnectNamedPipe(handle(&server), None) };
        if connected
            .as_ref()
            .err()
            .is_none_or(|e| e.code() != ERROR_PIPE_CONNECTED.to_hresult())
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let (mut client_pid, mut server_pid) = (0, 0);
        // SAFETY: owned connected pipe handles and independent live outputs.
        unsafe { GetNamedPipeClientProcessId(handle(&server), &raw mut client_pid) }.map_err(os)?;
        // SAFETY: the matching client endpoint and a live process-id output.
        unsafe { GetNamedPipeServerProcessId(handle(&client), &raw mut server_pid) }.map_err(os)?;
        if client_pid != std::process::id() || server_pid != std::process::id() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        live()?;
        Ok((Self(server), client))
    }

    pub(in super::super) fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(PIPE_BYTES as usize);
        let mut done = 0u32;
        // SAFETY: this is a synchronous NOWAIT handle. Windows retains no
        // buffer or OVERLAPPED pointer after this call, even on backpressure.
        match unsafe {
            ReadFile(
                handle(&self.0),
                Some(&mut bytes[..count]),
                Some(&raw mut done),
                None,
            )
        } {
            Ok(()) => windows_policy::read_count(count, done as usize),
            Err(e) if e.code() == ERROR_NO_DATA.to_hresult() => {
                Err(io::ErrorKind::WouldBlock.into())
            }
            Err(e) => Err(os(e)),
        }
    }
    pub(in super::super) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(PIPE_BYTES as usize);
        let mut done = 0u32;
        // SAFETY: synchronous NOWAIT byte mode, with a borrowed bounded input
        // buffer. A successful prefix is returned exactly once to write_all.
        match unsafe {
            WriteFile(
                handle(&self.0),
                Some(&bytes[..count]),
                Some(&raw mut done),
                None,
            )
        } {
            Ok(()) => windows_policy::write_count(count, done as usize),
            // On WRITE, NO_DATA means a closing pipe, not an empty read queue.
            Err(e)
                if e.code() == ERROR_NO_DATA.to_hresult()
                    || e.code() == ERROR_BROKEN_PIPE.to_hresult() =>
            {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            Err(e) => Err(os(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, io::Read};

    #[test]
    fn real_byte_pipe_empty_full_prefix_and_peer_exit_are_distinct() {
        let (mut server, client) = Pipe::pair(&|| Ok(())).unwrap();
        let mut one = [0u8; 1];
        assert_eq!(
            server.read(&mut one).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let input = vec![0xa5; PIPE_BYTES as usize];
        let mut total = 0usize;
        let mut full = false;
        // A finite upper bound also catches a mistakenly blocking server or
        // an implementation that ignores the configured private queue bound.
        for _ in 0..8 {
            match server.write(&input) {
                Ok(count) => {
                    assert!(count > 0 && count <= input.len());
                    total += count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Err(error) => panic!("unexpected write failure: {error}"),
            }
        }
        assert!(
            full && total > 0,
            "undrained pipe must reach bounded backpressure"
        );
        let mut client = File::from(client);
        let mut received = vec![0; total];
        client.read_exact(&mut received).unwrap();
        assert!(received.iter().all(|&byte| byte == 0xa5));
        assert_eq!(
            server.read(&mut one).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(client);
        assert_ne!(
            server.read(&mut one).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            server.write(&one).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
