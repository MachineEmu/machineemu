"""Analysis identity, host-profile, and guest-verification primitives."""

from .host_profile import dump_host_profile
from .identity import build_identity
from .profile import validate
from .verify import missing_observation_fields, record_observation
from .clone import create_clone, validate_clone, validate_clone_metadata
from .acpi import dump_acpi_tables
from .kvm_guard import GuardSession, load_command, parse_stats, read_stats, session_from_record, snapshot, status
from .environment import build_environment_report, write_environment_report

__all__ = ["GuardSession", "build_environment_report", "build_identity", "create_clone", "dump_acpi_tables", "dump_host_profile", "load_command", "missing_observation_fields", "parse_stats", "read_stats", "record_observation", "session_from_record", "snapshot", "status", "validate", "validate_clone", "validate_clone_metadata", "write_environment_report"]
