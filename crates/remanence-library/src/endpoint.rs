//! Durable endpoint identities shared by discovery, state, and mount callers.

/// Persistent mount identity. Bay zero is an ordinary library bay.
#[derive(
    Clone, Debug, Hash, Ord, PartialOrd, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriveKey {
    /// A changer-owned drive bay.
    Bay {
        /// Stable changer serial.
        library_serial: String,
        /// Element address, including zero.
        bay: u16,
    },
    /// A drive proven or configured to belong to no changer.
    Standalone {
        /// VPD page 0x80 serial, never a device path.
        drive_serial: String,
    },
}
impl DriveKey {
    /// Construct an ordinary library bay.
    pub fn new(library_serial: impl Into<String>, bay: u16) -> Self {
        Self::Bay {
            library_serial: library_serial.into(),
            bay,
        }
    }
    /// Collision-free SQLite encoding (JSON escapes identifiers).
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("DriveKey contains only strings and integers")
    }
    /// Decode a durable endpoint, rejecting malformed or out-of-range bays.
    pub fn decode(encoded: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(encoded)
    }
    /// Library serial, absent for a standalone endpoint.
    pub fn library_serial(&self) -> Option<&str> {
        match self {
            Self::Bay { library_serial, .. } => Some(library_serial),
            Self::Standalone { .. } => None,
        }
    }
    /// Bay address, absent for a standalone endpoint.
    pub fn bay(&self) -> Option<u16> {
        match self {
            Self::Bay { bay, .. } => Some(*bay),
            Self::Standalone { .. } => None,
        }
    }
    /// Operator-facing endpoint description.
    pub fn label(&self) -> String {
        match self {
            Self::Bay {
                library_serial,
                bay,
            } => format!("{library_serial} drive bay 0x{bay:04x}"),
            Self::Standalone { drive_serial } => format!("drive {drive_serial}"),
        }
    }
}

/// A mounted location; an unserialized device has no persistent identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountEndpoint {
    /// Stable identity suitable for durable state.
    Durable(DriveKey),
    /// Explicitly allowed read-only device, never a durable authority.
    Ephemeral {
        /// Device path for this invocation only.
        sg_path: std::path::PathBuf,
    },
}
impl MountEndpoint {
    /// Durable key, absent for a read-only ephemeral endpoint.
    pub fn durable_key(&self) -> Option<&DriveKey> {
        match self {
            Self::Durable(key) => Some(key),
            Self::Ephemeral { .. } => None,
        }
    }
}
