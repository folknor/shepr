//! Exit bytes shared by the client, daemon and SSH status decoders.

/// Every process status emitted by a shepr executable.
// limits-exempt: process exit bytes are an operator and SSH contract.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    Success = 0,
    Failed = 1,
    Usage = 2,
    BootMismatch = 3,
    NoServer = 4,
    AlreadyRunning = 10,
    ConfigRefused = 11,
}

impl ProcessStatus {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            code if code == Self::Success as i32 => Some(Self::Success),
            code if code == Self::Failed as i32 => Some(Self::Failed),
            code if code == Self::Usage as i32 => Some(Self::Usage),
            code if code == Self::BootMismatch as i32 => Some(Self::BootMismatch),
            code if code == Self::NoServer as i32 => Some(Self::NoServer),
            code if code == Self::AlreadyRunning as i32 => Some(Self::AlreadyRunning),
            code if code == Self::ConfigRefused as i32 => Some(Self::ConfigRefused),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessStatus;

    #[test]
    fn all_process_statuses_round_trip_and_unknown_bytes_are_refused() {
        for status in [
            ProcessStatus::Success,
            ProcessStatus::Failed,
            ProcessStatus::Usage,
            ProcessStatus::BootMismatch,
            ProcessStatus::NoServer,
            ProcessStatus::AlreadyRunning,
            ProcessStatus::ConfigRefused,
        ] {
            assert_eq!(
                ProcessStatus::from_code(i32::from(status.code())),
                Some(status)
            );
        }
        assert_eq!(ProcessStatus::from_code(-1), None);
        assert_eq!(ProcessStatus::from_code(255), None);
    }
}
