//! Generated API v2 contract. Keep these operations in sync with `router`.
#![allow(dead_code)] // Utoipa reads these schema-only functions and fields at compile time.
use super::{devices, display_control, documents, dto, events, guest_agent, guest_exec, streams};
use utoipa::{Modify, OpenApi, ToSchema, openapi::OpenApi as Document};

#[derive(ToSchema)]
struct StreamTicket {
    ticket: String,
    expires_in_seconds: u32,
    kind: String,
}

// The handlers return several domain records from machineemu-core. Until those
// records implement ToSchema, document their JSON payloads as free-form objects
// rather than claiming a shape that the daemon does not guarantee.
macro_rules! endpoint {
    ($name:ident, $method:ident, $path:literal, $summary:literal, $status:literal) => {
        #[utoipa::path(
            $method,
            path = $path,
            summary = $summary,
            responses((status = $status, description = $summary, body = serde_json::Value),
                      (status = 401, description = "Authentication required", body = dto::ErrorBody)),
            security(("bearerAuth" = []))
        )]
        fn $name() {}
    };
    ($name:ident, $method:ident, $path:literal, $summary:literal, $status:literal, $($params:tt)+) => {
        #[utoipa::path(
            $method,
            path = $path,
            summary = $summary,
            params($($params)+),
            responses((status = $status, description = $summary, body = serde_json::Value),
                      (status = 401, description = "Authentication required", body = dto::ErrorBody)),
            security(("bearerAuth" = []))
        )]
        fn $name() {}
    };
}

macro_rules! body_endpoint {
    ($name:ident, $method:ident, $path:literal, $summary:literal, $status:literal, $body:ty) => {
        #[utoipa::path(
            $method,
            path = $path,
            summary = $summary,
            request_body = $body,
            responses((status = $status, description = $summary, body = serde_json::Value),
                      (status = 401, description = "Authentication required", body = dto::ErrorBody)),
            security(("bearerAuth" = []))
        )]
        fn $name() {}
    };
    ($name:ident, $method:ident, $path:literal, $summary:literal, $status:literal, $body:ty, $($params:tt)+) => {
        #[utoipa::path(
            $method,
            path = $path,
            summary = $summary,
            request_body = $body,
            params($($params)+),
            responses((status = $status, description = $summary, body = serde_json::Value),
                      (status = 401, description = "Authentication required", body = dto::ErrorBody)),
            security(("bearerAuth" = []))
        )]
        fn $name() {}
    };
}

endpoint!(health, get, "/api/v2/health", "Health check", 200);
endpoint!(
    openapi_document,
    get,
    "/api/v2/openapi.json",
    "Get the API v2 OpenAPI document",
    200
);
body_endpoint!(
    register_image,
    post,
    "/api/v2/images",
    "Register an image",
    201,
    dto::RegisterImage
);
body_endpoint!(
    start_vmmanager_base_import,
    post,
    "/api/v2/image-imports/vmmanager-base",
    "Start a vmmanager-sh base image import job",
    202,
    dto::ImportVmmanagerBase
);
endpoint!(
    get_image_import,
    get,
    "/api/v2/image-imports/{id}",
    "Get an image import job",
    200,
    ("id" = String, Path)
);
#[utoipa::path(
    get,
    path = "/api/v2/image-imports/{id}/events",
    summary = "Stream image import progress",
    description = "SSE progress events use event, id, and JSON data fields. Reconnect with Last-Event-ID to replay retained progress.",
    params(("id" = String, Path), ("Last-Event-ID" = Option<String>, Header)),
    responses(
        (status = 200, description = "Image import event stream", content_type = "text/event-stream", body = String),
        (status = 401, description = "Authentication required", body = dto::ErrorBody),
        (status = 404, description = "Image import not found", body = dto::ErrorBody)
    ),
    security(("bearerAuth" = []))
)]
fn stream_image_import_events() {}
endpoint!(
    get_image,
    get,
    "/api/v2/images/{id}",
    "Get an image",
    200,
    ("id" = String, Path)
);
body_endpoint!(
    put_image,
    put,
    "/api/v2/images/{id}",
    "Replace an image manifest (JSON or YAML)",
    200,
    dto::RegisterImage,
    ("id" = String, Path)
);
endpoint!(
    get_profile,
    get,
    "/api/v2/profiles/{id}",
    "Get a shared profile (Accept: application/yaml supported)",
    200,
    ("id" = String, Path)
);
body_endpoint!(
    put_profile,
    put,
    "/api/v2/profiles/{id}",
    "Replace a workspace profile override (JSON or YAML)",
    200,
    serde_json::Value,
    ("id" = String, Path)
);
endpoint!(
    get_instance_tombstone,
    get,
    "/api/v2/instances/{id}/tombstone",
    "Get an auto-removed instance tombstone",
    200,
    ("id" = String, Path)
);
endpoint!(
    list_instances,
    get,
    "/api/v2/instances",
    "List instances",
    200
);
body_endpoint!(
    create_instance,
    post,
    "/api/v2/instances",
    "Create an instance",
    201,
    dto::CreateInstance
);
endpoint!(
    get_instance,
    get,
    "/api/v2/instances/{id}",
    "Get an instance",
    200,
    ("id" = String, Path)
);
endpoint!(
    get_instance_config,
    get,
    "/api/v2/instances/{id}/config",
    "Get saved instance profile and launch plan (Accept: application/yaml supported)",
    200,
    ("id" = String, Path)
);
body_endpoint!(
    put_instance_config,
    put,
    "/api/v2/instances/{id}/config",
    "Replace stopped instance profile and launch plan (JSON or YAML)",
    200,
    documents::InstanceConfig,
    ("id" = String, Path)
);
#[utoipa::path(
    get,
    path = "/api/v2/instances/{id}/events",
    summary = "Stream instance state, operation, and selected QMP events",
    description = "SSE events use event, id, and JSON data fields. Last-Event-ID replays retained events; an expired, future, foreign, or previous-generation cursor receives a snapshot instead. Heartbeats are comments every 15 seconds. See docs/operations/api-v2-live-vm-events.md.",
    params(("id" = String, Path), ("Last-Event-ID" = Option<String>, Header)),
    responses(
        (status = 200, description = "SSE stream", content_type = "text/event-stream", body = String),
        (status = 400, description = "Invalid instance ID or event cursor", body = dto::ErrorBody),
        (status = 401, description = "Authentication required", body = dto::ErrorBody),
        (status = 404, description = "Instance not found", body = dto::ErrorBody),
        (status = 429, description = "Subscriber limit reached", body = dto::ErrorBody)
    ),
    security(("bearerAuth" = []))
)]
fn stream_events() {}
#[utoipa::path(
    get,
    path = "/api/v2/instances/{id}/guest-agent",
    summary = "Get read-only guest agent information when available",
    description = "Returns available=false when the instance is stopped, the agent channel is absent, or the agent does not respond. Optional information fields are null when unsupported or unavailable from this guest.",
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Guest agent availability and information", body = guest_agent::GuestAgentInformation),
        (status = 400, description = "Invalid instance ID", body = dto::ErrorBody),
        (status = 401, description = "Authentication required", body = dto::ErrorBody),
        (status = 404, description = "Instance not found", body = dto::ErrorBody)
    ),
    security(("bearerAuth" = []))
)]
fn guest_agent_information() {}
#[utoipa::path(
    post,
    path = "/api/v2/instances/{id}/guest-executions",
    summary = "Start a command through the QEMU guest agent",
    description = "Runs a shell command through guest-exec. The command may continue in the guest after an API timeout. Captured output is available only after exit.",
    request_body = guest_exec::StartExecution,
    params(("id" = String, Path)),
    responses(
        (status = 202, description = "Execution accepted", body = guest_exec::ExecutionAccepted),
        (status = 400, description = "Invalid request", body = dto::ErrorBody),
        (status = 401, description = "Authentication required", body = dto::ErrorBody),
        (status = 404, description = "Instance not found", body = dto::ErrorBody),
        (status = 409, description = "Instance not running", body = dto::ErrorBody),
        (status = 429, description = "Execution limit reached", body = dto::ErrorBody)
    ),
    security(("bearerAuth" = []))
)]
fn start_guest_execution() {}
#[utoipa::path(
    get,
    path = "/api/v2/guest-executions/{id}/events",
    summary = "Stream guest execution status and captured output",
    description = "SSE status events report queued and running states. When QGA reports process exit, output events contain base64 chunks followed by complete. QGA does not expose live stdout or interactive stdin.",
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Execution event stream", content_type = "text/event-stream", body = String),
        (status = 401, description = "Authentication required", body = dto::ErrorBody),
        (status = 404, description = "Execution not found", body = dto::ErrorBody)
    ),
    security(("bearerAuth" = []))
)]
fn stream_guest_execution() {}
#[utoipa::path(delete, path = "/api/v2/instances/{id}", params(("id" = String, Path)), responses((status = 204, description = "Instance removed"), (status = 401, description = "Authentication required", body = dto::ErrorBody)), security(("bearerAuth" = [])))]
fn remove_instance() {}
body_endpoint!(
    start_instance,
    post,
    "/api/v2/instances/{id}/start",
    "Start an instance",
    200,
    dto::StartInstance,
    ("id" = String, Path)
);
endpoint!(
    stop_instance,
    post,
    "/api/v2/instances/{id}/stop",
    "Stop an instance",
    200,
    ("id" = String, Path)
);
endpoint!(
    restart_instance,
    post,
    "/api/v2/instances/{id}/restart",
    "Restart an instance from its saved configuration",
    200,
    ("id" = String, Path)
);
#[utoipa::path(
    post,
    path = "/api/v2/instances/{id}/send-key",
    summary = "Send a QEMU key chord",
    params(("id" = String, Path)),
    request_body = display_control::SendKey,
    responses((status = 204, description = "Keys sent"), (status = 401, body = dto::ErrorBody)),
    security(("bearerAuth" = []))
)]
fn send_key() {}
#[utoipa::path(
    post,
    path = "/api/v2/instances/{id}/screenshot",
    summary = "Capture the primary QEMU display as PNG",
    params(("id" = String, Path)),
    responses((status = 200, content_type = "image/png", body = Vec<u8>), (status = 401, body = dto::ErrorBody)),
    security(("bearerAuth" = []))
)]
fn screenshot() {}
endpoint!(
    pause_instance,
    post,
    "/api/v2/instances/{id}/pause",
    "Pause an instance",
    200,
    ("id" = String, Path)
);
endpoint!(
    resume_instance,
    post,
    "/api/v2/instances/{id}/resume",
    "Resume an instance",
    200,
    ("id" = String, Path)
);
endpoint!(
    reset_instance,
    post,
    "/api/v2/instances/{id}/reset",
    "Reset an instance",
    200,
    ("id" = String, Path)
);
body_endpoint!(
    create_snapshot,
    post,
    "/api/v2/instances/{id}/snapshots",
    "Create a snapshot",
    201,
    dto::CreateSnapshot,
    ("id" = String, Path)
);
endpoint!(
    get_snapshot,
    get,
    "/api/v2/snapshots/{id}",
    "Get a snapshot",
    200,
    ("id" = String, Path)
);
body_endpoint!(
    clone_snapshot,
    post,
    "/api/v2/snapshots/{id}/clone",
    "Clone a snapshot",
    201,
    dto::CloneSnapshot,
    ("id" = String, Path)
);
endpoint!(
    get_operation,
    get,
    "/api/v2/operations/{id}",
    "Get an operation",
    200,
    ("id" = String, Path)
);
endpoint!(
    reconcile,
    post,
    "/api/v2/reconcile",
    "Reconcile active runs",
    200
);
#[utoipa::path(post, path = "/api/v2/instances/{id}/streams/{kind}/ticket", request_body = streams::TicketRequest, params(("id" = String, Path), ("kind" = String, Path)), responses((status = 200, description = "One-use stream ticket", body = StreamTicket), (status = 401, description = "Authentication required", body = dto::ErrorBody)), security(("bearerAuth" = [])))]
fn issue_stream_ticket() {}
body_endpoint!(
    issue_spice_tickets,
    post,
    "/api/v2/instances/{id}/audio/spice/tickets",
    "Issue SPICE audio tickets",
    200,
    streams::SpiceTicketRequest,
    ("id" = String, Path)
);
endpoint!(
    helper_status,
    get,
    "/api/v2/instances/{id}/helpers/{kind}",
    "Get helper status",
    200,
    ("id" = String, Path),
    ("kind" = String, Path)
);
body_endpoint!(
    helper_action,
    post,
    "/api/v2/instances/{id}/helpers/{kind}",
    "Send a helper action",
    200,
    serde_json::Value,
    ("id" = String, Path),
    ("kind" = String, Path)
);
endpoint!(
    list_devices,
    get,
    "/api/v2/instances/{id}/devices/{kind}",
    "List live devices",
    200,
    ("id" = String, Path),
    ("kind" = String, Path)
);
body_endpoint!(
    attach_device,
    post,
    "/api/v2/instances/{id}/devices/{kind}",
    "Attach a live device",
    200,
    devices::AttachDevice,
    ("id" = String, Path),
    ("kind" = String, Path)
);
endpoint!(
    detach_device,
    delete,
    "/api/v2/instances/{id}/devices/{kind}/{device_id}",
    "Detach a live device",
    200,
    ("id" = String, Path),
    ("kind" = String, Path),
    ("device_id" = String, Path)
);
body_endpoint!(
    change_iso,
    post,
    "/api/v2/instances/{id}/devices/iso/{device_id}/change",
    "Change ISO medium",
    200,
    devices::ChangeMedium,
    ("id" = String, Path),
    ("device_id" = String, Path)
);
body_endpoint!(
    eject_iso,
    post,
    "/api/v2/instances/{id}/devices/iso/{device_id}/eject",
    "Eject ISO medium",
    200,
    devices::EjectMedium,
    ("id" = String, Path),
    ("device_id" = String, Path)
);

#[utoipa::path(
    get,
    path = "/ws/v2/instances/{id}/{kind}",
    summary = "Open a ticketed WebSocket stream",
    description = "Use a one-use ticket issued by the stream or SPICE ticket endpoint. Kinds: vnc, video, audio-dbus, usbredir, lcm, frontpanel, spice-main, spice-playback, spice-record. See docs/operations/api-v2-streams-devices.md for binary framing and input messages.",
    params(("id" = String, Path), ("kind" = String, Path), ("ticket" = String, Query)),
    responses((status = 101, description = "WebSocket upgrade"))
)]
fn connect_stream() {}

#[derive(OpenApi)]
#[openapi(
    paths(
        health, openapi_document, register_image, start_vmmanager_base_import, get_image_import, stream_image_import_events, get_image, put_image, get_profile, put_profile, list_instances, create_instance,
        get_instance, get_instance_config, put_instance_config, stream_events, guest_agent_information, start_guest_execution, stream_guest_execution,
        remove_instance, get_instance_tombstone, start_instance, stop_instance, restart_instance, send_key, screenshot, pause_instance,
        resume_instance, reset_instance, create_snapshot, get_snapshot, clone_snapshot,
        get_operation, reconcile, issue_stream_ticket, issue_spice_tickets,
        helper_status, helper_action, list_devices, attach_device, detach_device,
        change_iso, eject_iso, connect_stream
    ),
    components(schemas(
        dto::RegisterImage, dto::ImportVmmanagerBase, dto::CreateInstance, documents::InstanceConfig, dto::CreateSnapshot,
        dto::CloneSnapshot, dto::ErrorBody, dto::StartInstance,
        dto::LaunchSpec, dto::HelperSpec, dto::PreparationSpec, StreamTicket,
        streams::TicketRequest, streams::SpiceTicketRequest,
        devices::AttachDevice, devices::ChangeMedium, devices::EjectMedium, display_control::SendKey,
        events::EventBase, events::SnapshotEvent, events::OperationSummary,
        events::StateEvent, events::OperationEvent, events::QmpEvent,
        guest_agent::GuestAgentInformation,
        guest_exec::StartExecution, guest_exec::ExecutionAccepted
    )),
    modifiers(&SecurityAddon),
    info(title = "MachineEmu Rust API", version = "0.1.0", description = "API v2 for instances, live devices, and ticketed streams")
)]
struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut Document) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }
    }
}

pub(super) fn document() -> Document {
    ApiDoc::openapi()
}
