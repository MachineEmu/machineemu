//! Configuration documents used by the profile/instance resolver.
//!
//! These documents deliberately remain separate from the small lifecycle records in
//! [`super::Instance`].  `spec` is represented as JSON so that the schema can grow
//! without making the persistence layer a second source of truth; the resolver
//! validates and normalizes the fields it owns.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentMetadata {
    pub name: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub digest: Option<String>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: DocumentMetadata,
    #[serde(default)]
    pub spec: Value,
}
pub type PartialProfile = ProfileDocument;

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareIdentityDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: DocumentMetadata,
    #[serde(default)]
    pub spec: Value,
}
pub type HardwareIdentity = HardwareIdentityDocument;

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: DocumentMetadata,
    #[serde(default)]
    pub spec: Value,
}
pub type ImageManifestDocument = ImageDocument;

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: DocumentMetadata,
    pub spec: Value,
    #[serde(default)]
    pub source: Option<SourceProvenance>,
    #[serde(default)]
    pub status: Option<Value>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub digest: Option<String>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SourceProvenance {
    #[serde(default)]
    pub profile: Option<SourceRef>,
    #[serde(default)]
    pub hardware_identity: Option<SourceRef>,
    #[serde(default)]
    pub image: Option<SourceRef>,
    #[serde(default)]
    pub overrides: std::collections::BTreeMap<String, Value>,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CreateInstanceOverrides {
    #[serde(default)]
    pub spec: Value,
}

#[cfg_attr(feature = "api-schema", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EngineSnapshot {
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub build_digest: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub patch_revision: Option<String>,
    #[serde(default)]
    pub compatibility: Value,
}
