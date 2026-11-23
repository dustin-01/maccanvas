#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{reason} - Connection error: {0}", reason = io_reason(.0))]
    Io(#[from] std::io::Error),
    #[error("{reason} - VNC protocol error: {0}", reason = protocol_reason(.0))]
    Protocol(String),
    #[error("{} - VNC protocol error: HEVC screen sharing: cannot bind UDP port {port}: {source}", io_reason(.source))]
    ScreenSharingPort { port: u16, source: std::io::Error },
    #[error("The connection to the Mac was lost - Connection interrupted: {0}")]
    ConnectionLost(String),
    #[error("Sign-in failed - {0}")]
    Authentication(String),
    #[error("Could not access saved login details - Windows Credential Manager: {0}")]
    Credential(String),
    #[error("Identity verification did not finish - Windows Hello: {0}")]
    Consent(String),
    #[error("Could not use these settings - Settings: {0}")]
    Configuration(String),
    #[error("Could not display the remote desktop - Display: {0}")]
    Display(String),
    #[error("Could not process remote audio - Audio: {message}")]
    Audio { code: Option<i32>, message: String },
    #[error("Could not complete the update operation - Update: {0}")]
    Update(String),
    #[error("The update needs recovery - Update recovery: {0}")]
    UpdateRecovery(String),
    #[error("Could not complete security logging - Security audit: {0}")]
    Audit(String),
    #[error("Could not share the clipboard - Clipboard: {0}")]
    Clipboard(String),
    #[error("Could not complete the file sharing operation - File transfer: {0}")]
    Transfer(String),
    #[error("{} - File transfer: {message}", sftp_reason(*.code))]
    Sftp { code: u32, message: String },
    #[error("Operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_reason(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match error.kind() {
        AddrInUse => "Connection in use",
        AddrNotAvailable => "The network address is unavailable",
        ConnectionRefused => "The remote computer refused the connection",
        ConnectionReset | ConnectionAborted | BrokenPipe => "The connection was interrupted",
        NotConnected => "The connection is closed",
        HostUnreachable | NetworkUnreachable => "The remote computer is unreachable",
        TimedOut => "The operation timed out",
        PermissionDenied => "Access denied",
        NotFound => "The requested resource was not found",
        AlreadyExists => "The item already exists",
        StorageFull => "The drive is full",
        UnexpectedEof => "The data ended earlier than expected",
        InvalidData => "The data could not be read",
        _ if matches!(error.raw_os_error(), Some(11001 | 11004)) => {
            "Could not find the computer by name"
        }
        _ => "Could not complete the operation",
    }
}

fn protocol_reason(detail: &str) -> &'static str {
    if detail.starts_with("HEVC screen sharing: another HEVC session") {
        "Connection in use"
    } else if detail.starts_with("HEVC screen sharing:") {
        "The high-performance screen sharing session failed"
    } else if detail.starts_with("Unsupported") {
        "The server requested an unsupported screen sharing feature"
    } else {
        "Could not process the screen sharing data"
    }
}

fn sftp_reason(code: u32) -> &'static str {
    match code {
        2 => "The remote file or folder was not found",
        3 => "The Mac denied access",
        6 | 7 => "The file sharing connection is closed",
        8 => "The Mac does not support this file operation",
        _ => "The Mac could not complete the file operation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupied_udp_port_keeps_the_windows_diagnostic() {
        let source = std::io::Error::from_raw_os_error(10048);
        let expected = format!(
            "Connection in use - VNC protocol error: HEVC screen sharing: cannot bind UDP port 5900: {source}"
        );
        assert_eq!(
            Error::ScreenSharingPort { port: 5900, source }.to_string(),
            expected
        );
    }
}
