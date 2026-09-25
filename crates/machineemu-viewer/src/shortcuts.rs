#[cfg(target_os = "linux")]
pub use wayland_shortcuts::ShortcutInhibitor;

#[cfg(not(target_os = "linux"))]
pub struct ShortcutInhibitor;

#[cfg(not(target_os = "linux"))]
impl ShortcutInhibitor {
    pub fn new(_: &winit::window::Window) -> anyhow::Result<Option<Self>> {
        Ok(None)
    }
}
