//! Configuration resolution, capability inspection and ordered launch plans.
mod analysis;
mod capabilities;
mod document;
mod error;
mod plan;
pub use capabilities::{
    LegacyBoardReport, LegacyValidationReport, QemuOptions, inspect_qemu, validate_legacy_config,
    validate_profile_against_qemu,
};
pub use document::load_document;
pub use error::Error;
pub use plan::{
    DiskPreparation, LaunchPlan, PlanInput, Preparation, PreparationFile, build_plan, derive_mac,
};
#[cfg(test)]
mod tests;
