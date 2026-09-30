//! The concrete build backend a cached result was produced by.

use std::{
    fmt,
    hash::{Hash, Hasher},
};

use pixi_record::PixiRecord;
use rattler_conda_types::{PackageName, VersionWithSource};
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

use crate::input_hash::BackendBinaryFingerprint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BackendIdentity {
    Package(Box<PackageBackend>),
    System {
        command: String,
        fingerprint: BackendBinaryFingerprint,
    },
    InMemory {
        identifier: String,
    },
}

impl fmt::Display for BackendIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package(package) => package.fmt(f),
            Self::System {
                command,
                fingerprint,
            } => write!(f, "{command} (system, {:016x})", fingerprint.as_u64()),
            Self::InMemory { identifier } => write!(f, "{identifier} (in-memory)"),
        }
    }
}

/// A backend installed from a package into a solved ephemeral environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageBackend {
    pub command: String,
    pub name: PackageName,
    pub version: VersionWithSource,
    pub build: String,
    pub env_digest: EnvDigest,
}

impl fmt::Display for PackageBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {}",
            self.name.as_source(),
            self.version,
            self.build
        )
    }
}

/// An xxh3 digest over every package in a backend's environment, so a
/// change to any dependency of the backend is observable.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvDigest(u64);

impl EnvDigest {
    pub fn from_records(records: &[PixiRecord]) -> Self {
        let mut entries: Vec<_> = records
            .iter()
            .map(PixiRecord::package_record)
            .map(|record| {
                (
                    record.name.as_normalized(),
                    record.version.as_str(),
                    record.build.as_str(),
                    record.sha256.as_ref().map(|sha| sha.as_slice()),
                )
            })
            .collect();
        entries.sort();
        let mut hasher = Xxh3::new();
        entries.hash(&mut hasher);
        Self(hasher.finish())
    }
}
