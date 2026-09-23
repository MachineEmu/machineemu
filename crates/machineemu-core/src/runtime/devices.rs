use super::RunningInstance;
use crate::{Error, Result, domain::Id};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn qmp(running: &mut RunningInstance, command: &str, arguments: Value) -> Result<Value> {
    running.qmp.execute(command, arguments)
}

fn file_name(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Process("media path is not UTF-8".into()))
}

impl RunningInstance {
    pub fn usb_host_attach(&mut self, id: &Id, hostbus: u16, hostaddr: u16) -> Result<Value> {
        qmp(
            self,
            "device_add",
            json!({"driver":"usb-host","id":id.as_str(),
            "hostbus":hostbus,"hostaddr":hostaddr}),
        )
    }

    pub fn usb_image_attach(&mut self, id: &Id, image: &Path, read_only: bool) -> Result<Value> {
        let node = format!("{}-node", id.as_str());
        qmp(
            self,
            "blockdev-add",
            json!({"node-name":node,"driver":"raw",
            "read-only":read_only,"file":{"driver":"file","filename":file_name(image)?}}),
        )?;
        let result = qmp(
            self,
            "device_add",
            json!({"driver":"usb-storage","id":id.as_str(),"drive":node}),
        );
        if result.is_err() {
            let _ = qmp(self, "blockdev-del", json!({"node-name":node}));
        }
        result
    }

    pub fn iso_attach(&mut self, id: &Id, image: &Path, bus: &str) -> Result<Value> {
        let controller = format!("{}-ctl", id.as_str());
        let node = format!("{}-node", id.as_str());
        qmp(
            self,
            "device_add",
            json!({"driver":"virtio-scsi-pci","id":controller,"bus":bus}),
        )?;
        if let Err(error) = qmp(
            self,
            "blockdev-add",
            json!({"node-name":node,"driver":"raw",
            "read-only":true,"file":{"driver":"file","filename":file_name(image)?}}),
        ) {
            let _ = qmp(self, "device_del", json!({"id":controller}));
            return Err(error);
        }
        let result = qmp(
            self,
            "device_add",
            json!({"driver":"scsi-cd","id":id.as_str(),
            "bus":format!("{controller}.0"),"drive":node}),
        );
        if result.is_err() {
            let _ = qmp(self, "blockdev-del", json!({"node-name":node}));
            let _ = qmp(self, "device_del", json!({"id":controller}));
        }
        result
    }

    pub fn iso_change(&mut self, id: &str, image: &Path) -> Result<Value> {
        qmp(
            self,
            "blockdev-change-medium",
            json!({"id":id,"filename":file_name(image)?,
            "format":"raw","read-only-mode":"read-only"}),
        )
    }

    pub fn iso_eject(&mut self, id: &str, force: bool) -> Result<Value> {
        qmp(self, "blockdev-open-tray", json!({"id":id,"force":force}))?;
        qmp(self, "blockdev-remove-medium", json!({"id":id}))?;
        qmp(self, "blockdev-close-tray", json!({"id":id}))
    }

    pub fn network_attach(
        &mut self,
        id: &Id,
        model: &str,
        mac: Option<&str>,
        bus: &str,
    ) -> Result<Value> {
        let backend = format!("{}-netdev", id.as_str());
        qmp(self, "netdev_add", json!({"type":"user","id":backend}))?;
        let mut device = json!({"driver":model,"id":id.as_str(),"netdev":backend,"bus":bus});
        if let Some(mac) = mac {
            device["mac"] = Value::String(mac.into());
        }
        let result = qmp(self, "device_add", device);
        if result.is_err() {
            let _ = qmp(self, "netdev_del", json!({"id":backend}));
        }
        result
    }

    pub fn usbredir_attach(&mut self, id: &Id, socket: &Path) -> Result<Value> {
        let chardev = format!("{}-char", id.as_str());
        qmp(
            self,
            "chardev-add",
            json!({"id":chardev,"backend":{"type":"socket","data":{
            "addr":{"type":"unix","data":{"path":file_name(socket)?}},"server":true,"wait":false}}}),
        )?;
        let result = qmp(
            self,
            "device_add",
            json!({"driver":"usb-redir","id":id.as_str(),"chardev":chardev}),
        );
        if result.is_err() {
            let _ = qmp(self, "chardev-remove", json!({"id":chardev}));
        }
        result
    }

    /// Returns false when guest-side hot removal has not completed yet.
    pub fn detach_device(&mut self, id: &Id, kind: &str) -> Result<bool> {
        qmp(self, "device_del", json!({"id":id.as_str()}))?;
        if !self
            .qmp
            .wait_device_deleted(id.as_str(), Duration::from_secs(5))?
        {
            return Ok(false);
        }
        let suffix = match kind {
            "usb-image" | "iso" => Some(("blockdev-del", "node-name", "-node")),
            "network" => Some(("netdev_del", "id", "-netdev")),
            "usbredir" => Some(("chardev-remove", "id", "-char")),
            "usb-host" => None,
            _ => return Err(Error::Process("unknown device kind".into())),
        };
        if let Some((command, key, ending)) = suffix {
            let mut arguments = serde_json::Map::new();
            arguments.insert(
                key.into(),
                Value::String(format!("{}{ending}", id.as_str())),
            );
            qmp(self, command, Value::Object(arguments))?;
        }
        if kind == "iso" {
            let controller = format!("{}-ctl", id.as_str());
            qmp(self, "device_del", json!({"id":controller}))?;
            let _ = self
                .qmp
                .wait_device_deleted(&controller, Duration::from_secs(5))?;
        }
        Ok(true)
    }

    pub fn query_usb(&mut self) -> Result<Value> {
        qmp(
            self,
            "human-monitor-command",
            json!({"command-line":"info usb"}),
        )
    }

    pub fn query_block(&mut self) -> Result<Value> {
        qmp(self, "query-block", Value::Null)
    }
    pub fn query_network(&mut self) -> Result<Value> {
        qmp(self, "query-pci", Value::Null)
    }
}
